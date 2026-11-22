//! 已确认恢复计划的执行。
use super::*;
impl MysqlSession {
    /// 重新校验完整输入及目标计划，逐步骤登记发送和提交结果。
    pub async fn restore_sql(
        &mut self,
        prepared: &PreparedTransfer,
        journal: &mut TransferJournal,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.profile.ensure_write()?;
        let spec = &prepared.spec;
        let index_sink = journal
            .sink
            .clone()
            .with_transfer_step(journal.run_id, journal.next_ordinal());
        let sources = inputs(spec, &index_sink, cancel).await?;
        let mut current = self
            .restore_plan(
                spec,
                &sources,
                &journal.sink.store,
                prepared.retry_of,
                cancel,
            )
            .await?;
        if current.len() != prepared.items.len() {
            return Err(AppError::conflict("恢复计划已变化，请重新准备"));
        }
        let mut confirmed_by_id = BTreeMap::new();
        for confirmed in &prepared.items {
            if confirmed_by_id
                .insert(confirmed.id.as_str(), confirmed)
                .is_some()
            {
                return Err(AppError::invalid("恢复计划含重复步骤 ID"));
            }
        }
        for now in &mut current {
            let confirmed = confirmed_by_id
                .get(now.id.as_str())
                .ok_or_else(|| AppError::conflict("恢复计划步骤已变化"))?;
            if now.sql != confirmed.sql
                || now.precondition != confirmed.precondition
                || now.action != confirmed.action
                || serde_json::to_value(&now.source)? != serde_json::to_value(&confirmed.source)?
                || serde_json::to_value(&now.target)? != serde_json::to_value(&confirmed.target)?
                || now.phase != confirmed.phase
                || now.destructive != confirmed.destructive
            {
                return Err(AppError::conflict(
                    "恢复输入、目标结构或确认选择已变化，请重新准备",
                ));
            }
            now.selected = confirmed.selected;
            if (now.phase == "schema" || now.phase == "after_data")
                && now.selected
                && now.sql.is_none()
            {
                return Err(AppError::new(
                    "schema_incomplete",
                    format!("结构项 {} 尚不能安全执行", now.id),
                ));
            }
        }
        validate_restore_selection(&current, &sources, spec)?;
        let mut fingerprints: BTreeMap<(String, String), String> = BTreeMap::new();
        let mut active_context: Option<TransferPlanItem> = None;
        let result: Result<()> = async {
            for entry in &current {
                ensure_not_cancelled(cancel)?;
                if !entry.selected
                    || entry.action == "wrapper_conversion"
                    || entry.action == "object_scope"
                {
                    continue;
                }
                if entry.action == "prior_unknown" {
                    journal.progress.outcome_unknown += 1;
                    journal.checkpoint().await?;
                    continue;
                }
                if entry.action == "prior_object" {
                    journal.progress.skipped += 1;
                    journal.checkpoint().await?;
                    continue;
                }
                let (input, number) = index_for_item(&entry.id)?;
                let source = sources
                    .get(input)
                    .ok_or_else(|| AppError::invalid("恢复输入序号越界"))?;
                let statement = source
                    .index
                    .statements
                    .get(number)
                    .ok_or_else(|| AppError::invalid("恢复语句序号越界"))?;
                if entry.action == "create_database" {
                    let db = entry
                        .target
                        .as_ref()
                        .and_then(|target| target.database.as_deref())
                        .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                    if self
                        .profile
                        .direct()?
                        .visible_databases
                        .as_ref()
                        .is_some_and(|allowed| !allowed.iter().any(|name| name == db))
                    {
                        return Err(AppError::new(
                            "permission_denied",
                            format!("目标数据库 {db} 不在连接可见范围内"),
                        ));
                    }
                    if prior_skip(journal, input, statement.start, "database:create").await? {
                        continue;
                    }
                    let mut conn = self.new_connection().await?;
                    let exists: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME=?",
                    )
                    .bind(db)
                    .fetch_one(&mut conn)
                    .await?;
                    if exists != 0 {
                        return Err(AppError::conflict(format!(
                            "目标数据库 {db} 已出现，请重新准备"
                        )));
                    }
                    let sql = entry
                        .sql
                        .as_deref()
                        .ok_or_else(|| AppError::invalid("缺少建库 SQL"))?;
                    let ticket = journal
                        .begin(
                            "database:create",
                            TransferLocator {
                                object: Some(db.into()),
                                input: Some(input),
                                byte_start: Some(statement.start),
                                byte_end: Some(statement.end),
                                ..Default::default()
                            },
                            format!(
                                "{:x}",
                                base16ct::HexDisplay(&Sha256::digest(sql.as_bytes()))
                            ),
                        )
                        .await?;
                    if let Err(error) = ticket.sink.mark_sent(true).await {
                        journal
                            .finish(
                                ticket,
                                TransferStepState::NotSent,
                                0,
                                Vec::new(),
                                Some(&error),
                                None,
                            )
                            .await?;
                        return Err(error);
                    }
                    match sqlx::query(sqlx::AssertSqlSafe(sql))
                        .execute(&mut conn)
                        .await
                    {
                        Ok(_) if database_defaults(self, db).await.is_ok() => {
                            journal
                                .finish(
                                    ticket,
                                    TransferStepState::Applied,
                                    1,
                                    Vec::new(),
                                    None,
                                    None,
                                )
                                .await?
                        }
                        result => {
                            let error = match result {
                                Err(error) if error.as_database_error().is_some() => {
                                    AppError::from(error)
                                }
                                Err(error) => {
                                    AppError::unknown(format!("创建数据库结果无法确认: {error}"))
                                }
                                Ok(_) => AppError::unknown("数据库已创建但读回失败"),
                            };
                            let state = if error.code == "outcome_unknown" {
                                TransferStepState::OutcomeUnknown
                            } else {
                                TransferStepState::Failed
                            };
                            journal
                                .finish(ticket, state, 0, Vec::new(), Some(&error), None)
                                .await?;
                            return Err(error);
                        }
                    }
                    journal.checkpoint().await?;
                    continue;
                }
                let object = mapping(spec, statement, input)?;
                let target_db = object
                    .target
                    .database
                    .as_deref()
                    .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                if self
                    .profile
                    .direct()?
                    .visible_databases
                    .as_ref()
                    .is_some_and(|allowed| !allowed.iter().any(|db| db == target_db))
                {
                    return Err(AppError::new(
                        "permission_denied",
                        format!("目标数据库 {target_db} 不在连接可见范围内"),
                    ));
                }
                if entry.action == "database_context_enter"
                    || entry.action == "database_context_leave"
                {
                    self.apply_database_context(entry, input, statement, journal)
                        .await?;
                    if entry.action == "database_context_enter" {
                        let leave_id = format!("{}:context-leave", statement_id(input, number));
                        active_context = Some(
                            prepared
                                .items
                                .iter()
                                .find(|item| item.id == leave_id && item.selected)
                                .ok_or_else(|| {
                                    AppError::invalid("数据库上下文缺少已确认的恢复步骤")
                                })?
                                .clone(),
                        );
                    } else {
                        active_context = None;
                    }
                } else if entry.phase == "schema" || entry.phase == "after_data" {
                    let db = object
                        .target
                        .database
                        .clone()
                        .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                    let key = (db, object.target.name.clone());
                    let expected = fingerprints.get(&key).cloned().or_else(|| {
                        entry
                            .precondition
                            .as_ref()
                            .filter(|value| value.as_str() != "absent")
                            .cloned()
                    });
                    let phase = if entry.action == "create_table" {
                        "schema:create_table".to_owned()
                    } else {
                        format!(
                            "schema:{:x}",
                            base16ct::HexDisplay(&Sha256::digest(entry.id.as_bytes()))
                        )
                    };
                    if prior_skip(journal, input, statement.start, &phase).await? {
                        continue;
                    }
                    let locator = TransferLocator {
                        object: Some(object.target.name.clone()),
                        input: Some(input),
                        byte_start: Some(statement.start),
                        byte_end: Some(statement.end),
                        ..Default::default()
                    };
                    let ticket = journal
                        .begin(
                            &phase,
                            locator,
                            format!(
                                "{:x}",
                                base16ct::HexDisplay(&Sha256::digest(entry.id.as_bytes()))
                            ),
                        )
                        .await?;
                    let result = self
                        .apply_schema(&object.target, entry, expected.as_deref(), &ticket.sink)
                        .await;
                    match result {
                        Ok(fingerprint) => {
                            fingerprints.insert(key, fingerprint);
                            journal
                                .finish(
                                    ticket,
                                    TransferStepState::Applied,
                                    1,
                                    Vec::new(),
                                    None,
                                    None,
                                )
                                .await?;
                        }
                        Err(error) => {
                            let state = if error.code == "outcome_unknown" {
                                TransferStepState::OutcomeUnknown
                            } else {
                                TransferStepState::Failed
                            };
                            journal
                                .finish(ticket, state, 0, Vec::new(), Some(&error), None)
                                .await?;
                            return Err(error);
                        }
                    }
                } else if entry.action == "insert" {
                    self.restore_rows(
                        RestoreRowsContext {
                            spec,
                            input,
                            number,
                            source,
                            object: &object,
                            entry,
                        },
                        journal,
                        cancel,
                    )
                    .await?;
                } else if entry.action == "create_object" || entry.action == "drop_object" {
                    if entry.action == "drop_object" {
                        let (show, index) = super::super::show_command(&object.target)?;
                        let mut conn = self.new_connection().await?;
                        let row = sqlx::query(sqlx::AssertSqlSafe(show))
                            .fetch_one(&mut conn)
                            .await?;
                        let ddl = super::super::string_at(&row, index, "目标对象定义")?;
                        let current = format!(
                            "{:x}",
                            base16ct::HexDisplay(&Sha256::digest(ddl.as_bytes()))
                        );
                        if entry.precondition.as_deref() != Some(current.as_str()) {
                            return Err(AppError::conflict("目标对象定义已变化，请重新准备"));
                        }
                    }
                    let sql = entry
                        .sql
                        .as_deref()
                        .ok_or_else(|| AppError::invalid("对象步骤缺少 SQL"))?;
                    let mut conn = self.new_connection().await?;
                    let db = object
                        .target
                        .database
                        .as_deref()
                        .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("USE {}", quote_ident(db)?)))
                        .execute(&mut conn)
                        .await?;
                    if entry.action == "create_object" {
                        for setting in &statement.context.settings {
                            sqlx::raw_sql(sqlx::AssertSqlSafe(setting.as_str()))
                                .execute(&mut conn)
                                .await?;
                        }
                    }
                    let locator = TransferLocator {
                        object: Some(object.target.name.clone()),
                        input: Some(input),
                        byte_start: Some(statement.start),
                        byte_end: Some(statement.end),
                        ..Default::default()
                    };
                    let phase = format!("object:{}", entry.action);
                    if prior_skip(journal, input, statement.start, &phase).await? {
                        continue;
                    }
                    let ticket = journal
                        .begin(
                            &phase,
                            locator,
                            format!(
                                "{:x}",
                                base16ct::HexDisplay(&Sha256::digest(sql.as_bytes()))
                            ),
                        )
                        .await?;
                    ticket.sink.mark_sent(true).await?;
                    // CREATE PROCEDURE/FUNCTION/TRIGGER 不受 MySQL prepared protocol 支持；正文已由受控 reader 限定为单个 DDL。
                    let result = sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                        .execute(&mut conn)
                        .await;
                    match result {
                        Ok(_) => {
                            let exists = existing_object(self, &object).await;
                            match exists {
                                Ok(exists) if exists == (entry.action == "create_object") => {
                                    journal
                                        .finish(
                                            ticket,
                                            TransferStepState::Applied,
                                            1,
                                            Vec::new(),
                                            None,
                                            None,
                                        )
                                        .await?
                                }
                                _ => {
                                    let error = AppError::unknown(format!(
                                        "对象 {} 的 DDL 已发送，但读回无法确认",
                                        object.target.name
                                    ));
                                    journal
                                        .finish(
                                            ticket,
                                            TransferStepState::OutcomeUnknown,
                                            0,
                                            Vec::new(),
                                            Some(&error),
                                            None,
                                        )
                                        .await?;
                                    return Err(error);
                                }
                            }
                        }
                        Err(e) => {
                            let error = if e.as_database_error().is_some() {
                                AppError::from(e)
                            } else {
                                AppError::unknown(format!("对象 DDL 结果无法确认: {e}"))
                            };
                            let state = if error.code == "outcome_unknown" {
                                TransferStepState::OutcomeUnknown
                            } else {
                                TransferStepState::Failed
                            };
                            journal
                                .finish(ticket, state, 0, Vec::new(), Some(&error), None)
                                .await?;
                            return Err(error);
                        }
                    }
                }
                journal.checkpoint().await?;
            }
            Ok(())
        }
        .await;
        if let (Err(error), Some(leave)) = (&result, active_context) {
            let (input, number) = index_for_item(&leave.id)?;
            let cleanup = self
                .apply_database_context(
                    &leave,
                    input,
                    &sources[input].index.statements[number],
                    journal,
                )
                .await;
            if let Err(cleanup_error) = cleanup {
                return Err(AppError::unknown(format!(
                    "恢复失败: {error}；数据库默认值收尾未确认: {cleanup_error}"
                )));
            }
        }
        result
    }

    async fn apply_database_context(
        &self,
        entry: &TransferPlanItem,
        input: usize,
        statement: &SqlStatement,
        journal: &mut TransferJournal,
    ) -> Result<()> {
        let target = entry
            .target
            .as_ref()
            .ok_or_else(|| AppError::invalid("数据库上下文缺少目标"))?;
        let db = target
            .database
            .as_deref()
            .ok_or_else(|| AppError::invalid("目标库缺失"))?;
        let (before, desired): ((String, String), (String, String)) = serde_json::from_str(
            entry
                .precondition
                .as_deref()
                .ok_or_else(|| AppError::invalid("数据库上下文缺少前置条件"))?,
        )?;
        let enter = entry.action == "database_context_enter";
        let expected = if enter { &before } else { &desired };
        let result = if enter { &desired } else { &before };
        if &database_defaults(self, db).await? != expected {
            return Err(AppError::conflict(format!(
                "数据库 {db} 默认值已被外部修改，未执行上下文变更"
            )));
        }
        let sql = entry
            .sql
            .as_deref()
            .ok_or_else(|| AppError::invalid("数据库上下文缺少 SQL"))?;
        let mut conn = self.new_connection().await?;
        let ticket = journal
            .begin(
                "database_context",
                TransferLocator {
                    object: Some(db.to_owned()),
                    input: Some(input),
                    byte_start: Some(statement.start),
                    byte_end: Some(statement.end),
                    ..Default::default()
                },
                format!(
                    "{:x}",
                    base16ct::HexDisplay(&Sha256::digest(sql.as_bytes()))
                ),
            )
            .await?;
        if let Err(error) = ticket.sink.mark_sent(true).await {
            journal
                .finish(
                    ticket,
                    TransferStepState::NotSent,
                    0,
                    Vec::new(),
                    Some(&error),
                    None,
                )
                .await?;
            return Err(error);
        }
        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&mut conn)
            .await
        {
            let error = if e.as_database_error().is_some() {
                AppError::from(e)
            } else {
                AppError::unknown(format!("数据库默认值变更结果无法确认: {e}"))
            };
            let state = if error.code == "outcome_unknown" {
                TransferStepState::OutcomeUnknown
            } else {
                TransferStepState::Failed
            };
            journal
                .finish(ticket, state, 0, Vec::new(), Some(&error), None)
                .await?;
            return Err(error);
        }
        if database_defaults(self, db).await.ok().as_ref() != Some(result) {
            let error = AppError::unknown(format!("数据库 {db} 默认值变更已发送，读回无法确认"));
            journal
                .finish(
                    ticket,
                    TransferStepState::OutcomeUnknown,
                    0,
                    Vec::new(),
                    Some(&error),
                    None,
                )
                .await?;
            return Err(error);
        }
        journal
            .finish(
                ticket,
                TransferStepState::Applied,
                1,
                Vec::new(),
                None,
                None,
            )
            .await
    }

    async fn restore_rows(
        &mut self,
        context: RestoreRowsContext<'_>,
        journal: &mut TransferJournal,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let RestoreRowsContext {
            spec,
            input,
            number,
            source,
            object,
            entry,
        } = context;
        let statement = &source.index.statements[number];
        let db = object
            .target
            .database
            .as_deref()
            .ok_or_else(|| AppError::invalid("目标库缺失"))?;
        let mut conn = self.new_connection().await?;
        let triggers: Vec<String> = sqlx::query_scalar("SELECT TRIGGER_NAME FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA=? AND EVENT_OBJECT_TABLE=? ORDER BY TRIGGER_NAME").bind(db).bind(&object.target.name).fetch_all(&mut conn).await?;
        let fingerprint = format!(
            "{:x}",
            base16ct::HexDisplay(&Sha256::digest(serde_json::to_vec(&(
                &source.index.sha256,
                &triggers
            ))?))
        );
        if entry.precondition.as_deref() != Some(fingerprint.as_str()) {
            return Err(AppError::conflict("目标触发器清单已变化，请重新准备"));
        }
        let target = target_table(self, object, cancel).await?.ok_or_else(|| {
            AppError::new(
                "target_missing",
                format!("目标表 {} 不存在", object.target.name),
            )
        })?;
        let columns = if let Some(columns) = &statement.source_columns {
            columns.clone()
        } else {
            let columns: Vec<String> = spec.inputs[input]
                .columns
                .iter()
                .filter(|c| !c.generated)
                .map(|c| c.name.clone())
                .collect();
            if columns.is_empty() {
                return Err(AppError::new(
                    "source_columns_required",
                    "INSERT 源列顺序未知，请明确提供",
                ));
            }
            columns
        };
        let mut mapped = object.clone();
        mapped.columns = columns
            .iter()
            .map(|name| ColumnMapping {
                source: name.clone(),
                target: object
                    .columns
                    .iter()
                    .find(|m| m.source == *name)
                    .map(|m| m.target.clone())
                    .or_else(|| spec.inputs[input].column_mapping.get(name).cloned())
                    .unwrap_or_else(|| name.clone()),
            })
            .collect();
        let target_columns = mapped
            .columns
            .iter()
            .map(|mapping| {
                target
                    .schema
                    .columns
                    .iter()
                    .find(|column| column.name == mapping.target)
                    .ok_or_else(|| AppError::invalid(format!("目标字段不存在: {}", mapping.target)))
            })
            .collect::<Result<Vec<_>>>()?;
        let path = source.path.clone();
        let index = source.index.clone();
        // 单槽队列与 reader、当前行和写入批次共同受执行阶段的四份行预算约束。
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<(Vec<Value>, u64)>>(1);
        let reader_stop = cancel.child_token();
        let reader_cancel = reader_stop.clone();
        let reservation = uuid::Uuid::new_v4();
        let store = journal.sink.store.clone();
        let execution_id = journal.sink.execution_id;
        let runtime = tokio::runtime::Handle::current();
        let mut reserved = 0usize;
        let mut scan_budget = sql_restore::IndexBudget::new();
        scan_budget.set_tuple_reserver(Box::new(move |bytes| {
            // reader、单槽队列、当前行与写入批次可能同时持有不同的行。
            let required = bytes.saturating_mul(4);
            if required > reserved {
                let next = required.saturating_add(1024 * 1024 - 1) / (1024 * 1024) * (1024 * 1024);
                runtime.block_on(store.reserve_auxiliary(execution_id, reservation, next))?;
                reserved = next;
            }
            Ok(())
        }));
        let task = tokio::task::spawn_blocking(move || {
            let result = sql_restore::read_insert_statement_with_budget(
                path,
                &index,
                number,
                &reader_cancel,
                &mut scan_budget,
                |_, row, offset| {
                    sender
                        .blocking_send(Ok((row, offset)))
                        .map_err(|_| AppError::cancelled())
                },
            );
            if let Err(error) = result {
                let _ = sender.blocking_send(Err(error));
            }
        });
        let batch_context = RestoreBatchContext {
            spec,
            input,
            number,
            source,
            target: &target,
            object: &mapped,
        };
        let mut batch = Vec::new();
        let mut offsets = Vec::new();
        let mut bytes = 0usize;
        // 绝对 offset 只在当前语句内求差，避免按对象重排或多文件执行时重复累计。
        // 当前记录的尾部在下一条边界（或语句完成）后计入；写入确认仍独立统计。
        let mut consumed_to = statement.start;
        let result: Result<()> = async {
        while let Some(row) = tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), row = receiver.recv() => row }
        {
            ensure_not_cancelled(cancel)?;
            let (mut row, offset) = match row {
                Ok(row) => row,
                Err(error) => {
                    self.flush_restore_batch(&batch_context, &mut batch, &mut offsets, journal)
                        .await?;
                    return Err(error);
                }
            };
            journal.progress.processed_bytes = journal
                .progress
                .processed_bytes
                .saturating_add(offset.saturating_sub(consumed_to));
            consumed_to = offset;
            let previous =
                prior_state(&journal.sink.store, journal.retry_of, input, offset, "data").await?;
            if skip_prior_state(journal, previous) {
                self.flush_restore_batch(&batch_context, &mut batch, &mut offsets, journal)
                    .await?;
                bytes = 0;
                continue;
            }
            if row.len() != columns.len() {
                self.flush_restore_batch(&batch_context, &mut batch, &mut offsets, journal)
                    .await?;
                return Err(AppError::invalid(format!(
                    "字节 {offset} 的 INSERT 列数不符"
                )));
            }
            if let Err(cause) = row
                .iter_mut()
                .zip(&target_columns)
                .try_for_each(|(value, column)| coerce_sql_literal(value, column))
            {
                self.flush_restore_batch(&batch_context, &mut batch, &mut offsets, journal)
                    .await?;
                bytes = 0;
                let error = AppError::new("conversion", "SQL 记录无法按目标列类型无损转换")
                    .with_details(serde_json::json!({"input":input,"byte_offset":offset,"recoverable":true,"cause_code":cause.code}));
                let ticket = journal
                    .begin(
                        "data",
                        TransferLocator {
                            object: Some(mapped.target.name.clone()),
                            input: Some(input),
                            byte_start: Some(offset),
                            byte_end: Some(offset),
                            record_offsets: vec![offset],
                            ..Default::default()
                        },
                        crate::execution::migration::digest(&(
                            &source.index.sha256,
                            number,
                            offset,
                            "conversion",
                        ))?,
                    )
                    .await?;
                let skip = spec.options.conversion_errors
                    == crate::model::transfer::ConversionPolicy::Skip;
                let state = if skip {
                    journal.progress.skipped += 1;
                    TransferStepState::Skipped
                } else {
                    journal.progress.failed += 1;
                    TransferStepState::Failed
                };
                journal.progress.read_rows += 1;
                journal
                    .finish(ticket, state, 0, vec![state], Some(&error), None)
                    .await?;
                journal.checkpoint().await?;
                if !skip {
                    return Err(error);
                }
                continue;
            }
            let size = crate::execution::migration::encoded_size(&row)?;
            if !batch.is_empty()
                && (batch.len() >= crate::model::transfer::MAX_BATCH_ROWS
                    || bytes.saturating_add(size) > crate::model::transfer::MAX_BATCH_BYTES
                    || !target.schema.engine.eq_ignore_ascii_case("innodb"))
            {
                self.flush_restore_batch(&batch_context, &mut batch, &mut offsets, journal)
                    .await?;
                bytes = 0;
            }
            bytes = bytes.saturating_add(size);
            batch.push(row);
            offsets.push(offset);
        }
        self.flush_restore_batch(&batch_context, &mut batch, &mut offsets, journal)
            .await?;
        Ok(())
        }.await;
        if result.is_err() {
            reader_stop.cancel();
        }
        drop(receiver);
        let joined = task
            .await
            .map_err(|e| AppError::new("internal", format!("SQL 数据读取任务失败: {e}")));
        journal.sink.store.release_auxiliary(reservation).await?;
        joined?;
        result?;
        journal.progress.processed_bytes = journal
            .progress
            .processed_bytes
            .saturating_add(statement.end.saturating_sub(consumed_to));
        journal.checkpoint().await?;
        Ok(())
    }

    async fn flush_restore_batch(
        &self,
        context: &RestoreBatchContext<'_>,
        rows: &mut Vec<Vec<Value>>,
        offsets: &mut Vec<u64>,
        journal: &mut TransferJournal,
    ) -> Result<()> {
        let RestoreBatchContext {
            spec,
            input,
            number,
            source,
            target,
            object,
        } = *context;
        if rows.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(rows);
        let offsets = std::mem::take(offsets);
        let count = offsets.len() as u64;
        let digest =
            crate::execution::migration::digest(&(&source.index.sha256, number, &offsets))?;
        let locator = TransferLocator {
            object: Some(object.target.name.clone()),
            input: Some(input),
            byte_start: offsets.first().copied(),
            byte_end: offsets.last().copied(),
            record_offsets: offsets,
            ..Default::default()
        };
        let ticket = journal.begin("data", locator, digest).await?;
        match self
            .write_batch_with_context(
                target,
                object,
                spec.options.conflict,
                rows,
                &ticket.sink,
                MigrationBatchSource::SqlRestore(&source.index.statements[number].context.settings),
            )
            .await
        {
            Ok(outcome) => {
                let state = if outcome.affected == 0 {
                    TransferStepState::Skipped
                } else {
                    TransferStepState::Applied
                };
                journal.progress.read_rows += count;
                journal.progress.written_rows += outcome.affected;
                journal.progress.skipped += outcome
                    .states
                    .iter()
                    .filter(|state| **state == TransferStepState::Skipped)
                    .count() as u64;
                journal
                    .finish(ticket, state, outcome.affected, outcome.states, None, None)
                    .await?;
                journal.checkpoint().await
            }
            Err(mut error) => {
                let states: Vec<TransferStepState> = error
                    .details
                    .get("states")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();
                let failed = error
                    .details
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|index| ticket.locator().record_offsets.get(index as usize))
                    .copied();
                if let (Some(offset), Some(details)) = (failed, error.details.as_object_mut()) {
                    details.insert("byte_offset".into(), serde_json::json!(offset));
                    details.insert("input".into(), serde_json::json!(input));
                }
                let state = if error.code == "outcome_unknown" {
                    TransferStepState::OutcomeUnknown
                } else {
                    TransferStepState::Failed
                };
                journal
                    .finish(ticket, state, 0, states, Some(&error), failed)
                    .await?;
                Err(error)
            }
        }
    }
}
