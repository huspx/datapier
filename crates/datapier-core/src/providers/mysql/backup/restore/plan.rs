//! 恢复输入预检和计划生成。
use super::*;
impl MysqlSession {
    /// 扫描所有输入并比较目标结构；对象依赖或定义不可确定时拒绝准备。
    pub async fn prepare_restore(
        &mut self,
        spec: &TransferSpec,
        sink: &crate::ExecutionSink,
        retry_of: Option<uuid::Uuid>,
        cancel: &CancellationToken,
    ) -> Result<Vec<TransferPlanItem>> {
        let sources = inputs(spec, sink, cancel).await?;
        self.restore_plan(spec, &sources, &sink.store, retry_of, cancel)
            .await
    }

    pub(super) async fn restore_plan(
        &mut self,
        spec: &TransferSpec,
        sources: &[RestoreInput],
        store: &crate::Store,
        retry_of: Option<uuid::Uuid>,
        cancel: &CancellationToken,
    ) -> Result<Vec<TransferPlanItem>> {
        let mut conn = self.new_connection().await?;
        let version: String = sqlx::query_scalar("SELECT VERSION()")
            .fetch_one(&mut conn)
            .await?;
        let mut parts = version.split('.').take(3).map(|part| {
            part.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u32>()
        });
        let major = parts.next().and_then(|v| v.ok()).ok_or_else(|| {
            AppError::new("unsupported_version", format!("无法识别目标版本 {version}"))
        })?;
        let minor = parts.next().and_then(|v| v.ok()).ok_or_else(|| {
            AppError::new("unsupported_version", format!("无法识别目标版本 {version}"))
        })?;
        let patch = parts.next().and_then(|v| v.ok()).ok_or_else(|| {
            AppError::new("unsupported_version", format!("无法识别目标版本 {version}"))
        })?;
        let target_version = major
            .saturating_mul(10000)
            .saturating_add(minor.saturating_mul(100))
            .saturating_add(patch);
        let mut checked_contexts = BTreeSet::new();
        for source in sources {
            for statement in &source.index.statements {
                for constraint in &statement.version_constraints {
                    if constraint
                        .minimum
                        .is_some_and(|minimum| minimum > target_version)
                    {
                        return Err(AppError::new(
                            "unsupported_version",
                            format!(
                                "SQL 第 {} 行、字节 {} 的版本注释在目标 MySQL {version} 上被忽略，不能声明完整恢复",
                                statement.line, constraint.start
                            ),
                        ));
                    }
                }
                if spec.options.include_schema
                    && matches!(
                        statement.kind,
                        SqlKind::View
                            | SqlKind::Trigger
                            | SqlKind::Procedure
                            | SqlKind::Function
                            | SqlKind::Event
                    )
                    && checked_contexts.insert(&statement.context.settings)
                {
                    // 在独立空会话验证源创建上下文，不能到数据已导入后才发现目标不接受 SQL_MODE 等设置。
                    let mut probe = self.new_connection().await?;
                    for setting in &statement.context.settings {
                        ensure_not_cancelled(cancel)?;
                        if let Err(error) = sqlx::raw_sql(sqlx::AssertSqlSafe(setting.as_str()))
                            .execute(&mut probe)
                            .await
                        {
                            let database_code = error
                                .as_database_error()
                                .and_then(|error| error.code())
                                .map(|code| code.into_owned());
                            let mut error = AppError::new(
                                "unsupported_context",
                                format!(
                                    "SQL 第 {} 行的对象创建上下文不被目标 MySQL {version} 接受；请修订源 SET 后重新准备",
                                    statement.line
                                ),
                            );
                            error.database_code = database_code;
                            return Err(error);
                        }
                    }
                }
            }
        }
        let mut schema = Vec::new();
        let mut data = Vec::new();
        let mut deferred = Vec::new();
        let mut objects = Vec::new();
        let mut definitions = BTreeSet::new();
        let mut mappings = spec.objects.clone();
        for (input, source) in sources.iter().enumerate() {
            for statement in &source.index.statements {
                if statement.placeholder_view {
                    continue;
                }
                if let Some(kind) = object_kind(&statement.kind)
                    && statement.kind != SqlKind::Insert
                    && statement.is_create
                {
                    let object = mapping(spec, statement, input)?;
                    definitions.insert((
                        object.source.database.clone().unwrap_or_default(),
                        object.source.name.clone(),
                        kind.to_owned(),
                    ));
                    if !mappings.iter().any(|m| {
                        m.source.database == object.source.database
                            && m.source.name == object.source.name
                            && m.source.kind == object.source.kind
                    }) {
                        mappings.push(object);
                    }
                }
            }
        }
        let mut source_tables: BTreeMap<(String, String), super::super::MigrationTable> =
            BTreeMap::new();
        if spec.options.include_schema {
            for (input, source) in sources.iter().enumerate() {
                for (number, statement) in source.index.statements.iter().enumerate() {
                    let drop_index = statement.kind == SqlKind::Drop
                        && statement.drop_kind == Some(SqlKind::Index);
                    if statement.placeholder_view
                        || (!matches!(statement.kind, SqlKind::Table | SqlKind::Index)
                            && !drop_index)
                    {
                        continue;
                    }
                    let mut owner = statement.clone();
                    if statement.kind == SqlKind::Index || drop_index {
                        owner.kind = SqlKind::Table;
                        owner.source_database = statement.index_table_database.clone();
                        owner.source_name = statement.index_table_name.clone();
                    }
                    let object = mapping(spec, &owner, input)?;
                    let key = (
                        object.source.database.clone().unwrap_or_default(),
                        object.source.name.clone(),
                    );
                    let ddl = if drop_index {
                        let index = statement
                            .source_name
                            .as_deref()
                            .ok_or_else(|| AppError::invalid("DROP INDEX 缺少索引名"))?;
                        format!(
                            "ALTER TABLE {}.{} DROP INDEX {}",
                            quote_ident(
                                object
                                    .source
                                    .database
                                    .as_deref()
                                    .ok_or_else(|| AppError::invalid("索引源库缺失"))?
                            )?,
                            quote_ident(&object.source.name)?,
                            quote_ident(index)?
                        )
                    } else {
                        sql_restore::read_ddl(&source.path, &source.index, number)?
                    };
                    if statement.kind == SqlKind::Table && statement.is_create {
                        let table =
                            self.from_restore_ddl(object.source, &ddl, &sql_mode(statement))?;
                        if source_tables.insert(key, table).is_some() {
                            return Err(AppError::new(
                                "schema_incomplete",
                                format!("SQL 第 {} 行重复创建表", statement.line),
                            ));
                        }
                    } else {
                        let table = source_tables.get(&key).ok_or_else(|| {
                            AppError::new(
                                "source_schema_required",
                                format!(
                                    "SQL 第 {} 行的结构变更缺少先前 CREATE TABLE 基线",
                                    statement.line
                                ),
                            )
                        })?;
                        let updated = self.apply_restore_structure(table, &ddl)?;
                        source_tables.insert(key, updated);
                    }
                }
            }
        }
        if spec.options.include_schema {
            super::super::super::schema::validate_restore_dependencies(&source_tables, &mappings)?;
        }
        for (input, source) in sources.iter().enumerate() {
            for (number, statement) in source.index.statements.iter().enumerate() {
                ensure_not_cancelled(cancel)?;
                if statement.placeholder_view {
                    continue;
                }
                if statement.unresolved_references {
                    return Err(AppError::new(
                        "unresolved_reference",
                        format!(
                            "SQL 第 {} 行含动态 SQL 或无法确定的引用，编辑后重新准备",
                            statement.line
                        ),
                    ));
                }
                if statement.alters_database_defaults {
                    continue;
                }
                if statement.kind == SqlKind::Database && statement.is_create {
                    if let Some(state) =
                        prior_state(store, retry_of, input, statement.start, "database:create")
                            .await?
                        && matches!(
                            state,
                            TransferStepState::Sent | TransferStepState::OutcomeUnknown
                        )
                    {
                        return Err(AppError::unknown("数据库创建结果未知，请先人工核对"));
                    }
                    let source_db = statement
                        .source_name
                        .as_deref()
                        .or(statement.object.as_deref())
                        .ok_or_else(|| AppError::invalid("CREATE DATABASE 缺少数据库名称"))?;
                    let target_db = spec
                        .destination
                        .as_ref()
                        .and_then(|endpoint| endpoint.database.as_deref())
                        .unwrap_or(source_db);
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
                    let mut conn = self.new_connection().await?;
                    let exists: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME=?",
                    )
                    .bind(target_db)
                    .fetch_one(&mut conn)
                    .await?;
                    if exists == 0 {
                        let raw = sql_restore::read_ddl(&source.path, &source.index, number)?;
                        let words: Vec<String> = sqlparser::tokenizer::Tokenizer::new(
                            &sqlparser::dialect::MySqlDialect {},
                            &raw,
                        )
                        .tokenize()
                        .map_err(|e| AppError::invalid(format!("CREATE DATABASE 解析失败: {e}")))?
                        .into_iter()
                        .map(|token| token.to_string())
                        .filter(|s| !s.trim().is_empty() && s != "=")
                        .collect();
                        let mut i = 2;
                        if words.get(i).is_some_and(|s| s.eq_ignore_ascii_case("IF")) {
                            i += 3;
                        }
                        if words.get(i).is_none() {
                            return Err(AppError::invalid("CREATE DATABASE 缺少名称"));
                        }
                        i += 1;
                        let mut suffix = String::new();
                        while i < words.len() {
                            if words[i].eq_ignore_ascii_case("DEFAULT") {
                                i += 1;
                            }
                            let word = words
                                .get(i)
                                .ok_or_else(|| AppError::invalid("CREATE DATABASE 子句不完整"))?;
                            let collation = word.eq_ignore_ascii_case("COLLATE");
                            if word.eq_ignore_ascii_case("CHARACTER")
                                && words
                                    .get(i + 1)
                                    .is_some_and(|s| s.eq_ignore_ascii_case("SET"))
                            {
                                i += 2;
                            } else if collation || word.eq_ignore_ascii_case("CHARSET") {
                                i += 1;
                            } else {
                                return Err(AppError::new(
                                    "schema_incomplete",
                                    format!("不支持的 CREATE DATABASE 子句 {word}"),
                                ));
                            }
                            let value = words.get(i).ok_or_else(|| {
                                AppError::invalid("CREATE DATABASE 缺少字符集或排序规则")
                            })?;
                            super::super::token(value)?;
                            suffix.push_str(if collation {
                                " COLLATE "
                            } else {
                                " CHARACTER SET "
                            });
                            suffix.push_str(value);
                            i += 1;
                        }
                        let target = ResourceTarget {
                            database: Some(target_db.into()),
                            name: target_db.into(),
                            key_b64: None,
                            kind: "database".into(),
                        };
                        schema.push(TransferPlanItem {
                            id: statement_id(input, number),
                            phase: "database".into(),
                            action: "create_database".into(),
                            source: None,
                            target: Some(target),
                            sql: Some(format!(
                                "CREATE DATABASE {}{suffix}",
                                quote_ident(target_db)?
                            )),
                            destructive: true,
                            selected: spec.options.create_missing,
                            precondition: Some(source.index.sha256.clone()),
                            notes: vec!["创建数据库需要 CREATE 权限，属于独立隐式提交步骤".into()],
                        });
                    }
                    continue;
                }
                if statement.kind == SqlKind::Index
                    || (statement.kind == SqlKind::Drop
                        && statement.drop_kind == Some(SqlKind::Index))
                    || (statement.kind == SqlKind::Table && !statement.is_create)
                {
                    continue;
                }
                if statement.kind == SqlKind::Drop {
                    let name = statement
                        .source_name
                        .as_deref()
                        .ok_or_else(|| AppError::invalid("DROP 缺少对象名"))?;
                    let db = statement
                        .source_database
                        .as_deref()
                        .or(statement.context.database.as_deref())
                        .or_else(|| spec.source.as_ref().and_then(|e| e.database.as_deref()))
                        .or_else(|| {
                            spec.destination
                                .as_ref()
                                .and_then(|e| e.database.as_deref())
                        })
                        .ok_or_else(|| AppError::invalid("DROP 缺少数据库"))?;
                    let drop_kind = statement
                        .drop_kind
                        .as_ref()
                        .and_then(object_kind)
                        .ok_or_else(|| AppError::invalid("DROP 对象类型不支持"))?;
                    if !definitions.iter().any(|(d, n, kind)| {
                        d == db
                            && n == name
                            && (kind == drop_kind
                                || (drop_kind == "table"
                                    && kind == "view"
                                    && source.index.statements.iter().any(|s| {
                                        s.placeholder_view
                                            && s.source_database
                                                .as_deref()
                                                .or(s.context.database.as_deref())
                                                == Some(db)
                                            && s.source_name.as_deref() == Some(name)
                                    })))
                    }) {
                        return Err(AppError::new(
                            "unsupported_statement",
                            format!(
                                "SQL 第 {} 行的独立 DROP 没有对应创建定义，不能按恢复执行",
                                statement.line
                            ),
                        ));
                    }
                    objects.push(TransferPlanItem {
                        id: statement_id(input, number),
                        phase: "restore".into(),
                        action: "wrapper_conversion".into(),
                        source: None,
                        target: None,
                        sql: None,
                        destructive: false,
                        selected: true,
                        precondition: Some(source.index.sha256.clone()),
                        notes: vec![
                            "输入 DROP 已转换为目标差异和单独确认；已有表不删除重建".into(),
                        ],
                    });
                    continue;
                }
                let Some(kind) = object_kind(&statement.kind) else {
                    if statement.kind == SqlKind::Wrapper {
                        let mut wrapper = TransferPlanItem {
                            id: statement_id(input, number),
                            phase: "restore".into(),
                            action: "wrapper_conversion".into(),
                            source: None,
                            target: None,
                            sql: None,
                            destructive: false,
                            selected: true,
                            precondition: Some(source.index.sha256.clone()),
                            notes: vec![
                                "输入锁表或检查开关不直接执行；恢复保持约束检查并使用独立事务"
                                    .into(),
                            ],
                        };
                        wrapper.selected = true;
                        objects.push(wrapper);
                    }
                    continue;
                };
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
                // 创建上下文随对象一起调度，不能作为普通会话 SET 丢弃。
                for reference in &statement.references {
                    let (db, name) = reference.rsplit_once('.').unwrap_or((
                        object.source.database.as_deref().unwrap_or_default(),
                        reference.as_str(),
                    ));
                    let matches: BTreeSet<_> = definitions
                        .iter()
                        .filter(|(d, n, _)| {
                            d.eq_ignore_ascii_case(db) && n.eq_ignore_ascii_case(name)
                        })
                        .map(|(d, n, _)| (d.as_str(), n.as_str()))
                        .collect();
                    if matches.len() != 1 || !matches.contains(&(db, name)) {
                        return Err(AppError::new(
                            "unresolved_reference",
                            format!(
                                "SQL 第 {} 行外部或缺失引用 {reference}，请修订定义或纳入映射范围",
                                statement.line
                            ),
                        ));
                    }
                }
                if statement.kind == SqlKind::Insert {
                    if !spec.options.include_data {
                        continue;
                    }
                    if statement.source_columns.is_none() && spec.inputs[input].columns.is_empty() {
                        return Err(AppError::new(
                            "source_columns_required",
                            format!(
                                "SQL 第 {} 行无法安全推导源列顺序，请明确提供列序",
                                statement.line
                            ),
                        ));
                    }
                    let mut entry = item(input, number, "insert", &object, None);
                    entry
                        .notes
                        .push("每批按目标冲突策略写入，保留外键检查；目标现有触发器会执行".into());
                    let mut conn = self.new_connection().await?;
                    let db = object
                        .target
                        .database
                        .as_deref()
                        .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                    let triggers: Vec<String> = sqlx::query_scalar("SELECT TRIGGER_NAME FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA=? AND EVENT_OBJECT_TABLE=? ORDER BY TRIGGER_NAME").bind(db).bind(&object.target.name).fetch_all(&mut conn).await?;
                    entry.precondition = Some(format!(
                        "{:x}",
                        base16ct::HexDisplay(&Sha256::digest(serde_json::to_vec(&(
                            &source.index.sha256,
                            &triggers
                        ))?))
                    ));
                    if !triggers.is_empty() {
                        entry.destructive = true;
                        entry.notes.push(format!(
                            "目标表已有触发器 {}，写入将执行其业务逻辑，必须单独确认",
                            triggers.join(", ")
                        ));
                    }
                    data.push(entry);
                    continue;
                }
                if !spec.options.include_schema {
                    continue;
                }
                let ddl = sql_restore::read_ddl(&source.path, &source.index, number)?;
                if kind == "table" {
                    if let Some(state) = prior_state(
                        store,
                        retry_of,
                        input,
                        statement.start,
                        "schema:create_table",
                    )
                    .await?
                        && matches!(
                            state,
                            TransferStepState::Sent | TransferStepState::OutcomeUnknown
                        )
                    {
                        return Err(AppError::unknown(format!(
                            "表 {} 的创建结果未知，请先人工核对后重新准备",
                            object.target.name
                        )));
                    }
                    let key = (
                        object.source.database.clone().unwrap_or_default(),
                        object.source.name.clone(),
                    );
                    let source_table = source_tables
                        .get(&key)
                        .ok_or_else(|| AppError::new("source_schema_required", "缺少表结构基线"))?;
                    let target = target_table(self, &object, cancel).await?;
                    if target.is_none()
                        && !(object.create_if_missing || spec.options.create_missing)
                    {
                        return Err(AppError::new(
                            "target_missing",
                            format!("目标表 {} 不存在，需确认创建", object.target.name),
                        ));
                    }
                    let mut changes = self
                        .prepare_schema(source_table, target.as_ref(), &object, &mappings)
                        .await?;
                    if let Some(incomplete) = changes
                        .iter()
                        .find(|change| change.action == "unknown_attribute")
                    {
                        return Err(AppError::new(
                            "schema_incomplete",
                            incomplete.notes.join("; "),
                        ));
                    }
                    for change in &mut changes {
                        let old_id = change.id.clone();
                        change.id = format!("{}:{old_id}", statement_id(input, number));
                        change.selected = true;
                        change
                            .notes
                            .push(format!("SQL 输入第 {} 行", statement.line));
                    }
                    for change in changes {
                        if change.phase == "after_data" {
                            deferred.push(change);
                        } else {
                            schema.push(change);
                        }
                    }
                } else {
                    if retry_of.is_some() {
                        let prior = prior_state(
                            store,
                            retry_of,
                            input,
                            statement.start,
                            "object:create_object",
                        )
                        .await?;
                        if matches!(
                            prior,
                            Some(
                                TransferStepState::Applied
                                    | TransferStepState::Skipped
                                    | TransferStepState::Sent
                                    | TransferStepState::OutcomeUnknown
                            )
                        ) {
                            let unknown = matches!(
                                prior,
                                Some(TransferStepState::Sent | TransferStepState::OutcomeUnknown)
                            );
                            let mut done = item(
                                input,
                                number,
                                if unknown {
                                    "prior_unknown"
                                } else {
                                    "prior_object"
                                },
                                &object,
                                None,
                            );
                            done.precondition = Some(source.index.sha256.clone());
                            done.notes.push(
                                if matches!(
                                    prior,
                                    Some(
                                        TransferStepState::Sent | TransferStepState::OutcomeUnknown
                                    )
                                ) {
                                    "此前对象创建结果未知，不重复执行；请人工核对目标".into()
                                } else {
                                    "此前对象创建已完成，不重复执行".into()
                                },
                            );
                            objects.push(done);
                            continue;
                        }
                    }
                    let mut resolved_statement = statement.clone();
                    if resolved_statement.source_database.is_none() {
                        resolved_statement.source_database = object.source.database.clone();
                    }
                    let rewritten = sql_restore::rewrite_ddl(
                        &ddl,
                        &resolved_statement,
                        &mappings,
                        &spec.options.definer,
                    )?;
                    let exists = existing_object(self, &object).await?;
                    if exists {
                        let mut drop_item = item(
                            input,
                            number,
                            "drop_object",
                            &object,
                            Some(format!(
                                "DROP {} {}.{}",
                                kind.to_ascii_uppercase(),
                                quote_ident(
                                    object
                                        .target
                                        .database
                                        .as_deref()
                                        .ok_or_else(|| AppError::invalid("目标库缺失"))?
                                )?,
                                quote_ident(&object.target.name)?
                            )),
                        );
                        let (show, index) = super::super::show_command(&object.target)?;
                        let mut conn = self.new_connection().await?;
                        let row = sqlx::query(sqlx::AssertSqlSafe(show))
                            .fetch_one(&mut conn)
                            .await?;
                        let existing = super::super::string_at(&row, index, "目标对象定义")?;
                        drop_item.id.push_str(":drop");
                        drop_item.destructive = true;
                        drop_item.precondition = Some(format!(
                            "{:x}",
                            base16ct::HexDisplay(&Sha256::digest(existing.as_bytes()))
                        ));
                        drop_item
                            .notes
                            .push("DROP 和 CREATE 分别隐式提交，失败不会自动恢复原定义".into());
                        objects.push(drop_item);
                    }
                    let context = database_context(self, source, statement, &object).await?;
                    if let Some((before, desired)) = &context {
                        let db = object
                            .target
                            .database
                            .as_deref()
                            .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                        let mut enter = item(
                            input,
                            number,
                            "database_context_enter",
                            &object,
                            Some(format!(
                                "ALTER DATABASE {} CHARACTER SET {} COLLATE {}",
                                quote_ident(db)?,
                                desired.0,
                                desired.1
                            )),
                        );
                        enter.id.push_str(":context-enter");
                        enter.destructive = true;
                        enter.precondition = Some(serde_json::to_string(&(before, desired))?);
                        enter.notes.push(format!(
                            "库级默认值 {} / {} 临时改为 {} / {}；需 ALTER 权限，建议协调变更窗口",
                            before.0, before.1, desired.0, desired.1
                        ));
                        objects.push(enter);
                    }
                    let mut create = item(input, number, "create_object", &object, Some(rewritten));
                    create.precondition = Some(source.index.sha256.clone());
                    create.notes.push(format!(
                        "原 DEFINER {:?}；SQL SECURITY {:?}；事件默认禁用",
                        statement.definer, statement.sql_security
                    ));
                    objects.push(create);
                    if let Some((before, desired)) = &context {
                        let db = object
                            .target
                            .database
                            .as_deref()
                            .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                        let mut leave = item(
                            input,
                            number,
                            "database_context_leave",
                            &object,
                            Some(format!(
                                "ALTER DATABASE {} CHARACTER SET {} COLLATE {}",
                                quote_ident(db)?,
                                before.0,
                                before.1
                            )),
                        );
                        leave.id.push_str(":context-leave");
                        leave.destructive = true;
                        leave.precondition = Some(serde_json::to_string(&(before, desired))?);
                        leave.notes.push(
                            "仅在默认值仍为本次设置值时恢复；外部变更或结果未知需人工核对".into(),
                        );
                        objects.push(leave);
                    }
                }
            }
        }
        // 对象依赖按源引用拓扑排序；触发器与事件保留数据之后的安装顺序。
        let mut ordered = Vec::new();
        let mut completed = BTreeSet::new();
        for s in &schema {
            if let Some(t) = &s.source {
                completed.insert((t.database.clone().unwrap_or_default(), t.name.clone()));
            }
        }
        for s in &data {
            if let Some(t) = &s.source {
                completed.insert((t.database.clone().unwrap_or_default(), t.name.clone()));
            }
        }
        while !objects.is_empty() {
            let pos = objects.iter().position(|entry| {
                if entry.action == "wrapper_conversion" {
                    return true;
                }
                let Ok((i, n)) = index_for_item(&entry.id) else {
                    return false;
                };
                let stmt = &sources[i].index.statements[n];
                stmt.references.iter().all(|r| {
                    let (db, name) = r
                        .rsplit_once('.')
                        .map(|(d, n)| (d.to_owned(), n))
                        .unwrap_or((
                            entry
                                .source
                                .as_ref()
                                .and_then(|t| t.database.clone())
                                .unwrap_or_default(),
                            r.as_str(),
                        ));
                    !definitions.iter().any(|(d, n, _)| d == &db && n == name)
                        || completed.iter().any(|(d, n)| d == &db && n == name)
                })
            });
            let Some(pos) = pos else {
                return Err(AppError::new(
                    "dependency_cycle",
                    "对象引用存在循环，无法安全排列恢复顺序",
                ));
            };
            let key = index_for_item(&objects[pos].id)?;
            let mut i = 0;
            while i < objects.len() {
                if index_for_item(&objects[i].id)? != key {
                    i += 1;
                    continue;
                }
                let entry = objects.remove(i);
                if (entry.action == "create_object" || entry.action == "prior_object")
                    && let Some(t) = &entry.source
                {
                    completed.insert((t.database.clone().unwrap_or_default(), t.name.clone()));
                }
                ordered.push(entry);
            }
        }
        schema.extend(data);
        schema.extend(deferred);
        schema.extend(ordered);
        let mut scoped = BTreeSet::new();
        for (input, source) in sources.iter().enumerate() {
            for (number, statement) in source.index.statements.iter().enumerate() {
                if statement.placeholder_view
                    || object_kind(&statement.kind).is_none()
                    || (statement.kind != SqlKind::Insert && !statement.is_create)
                    || (statement.kind == SqlKind::Insert && !spec.options.include_data)
                    || (statement.kind != SqlKind::Insert && !spec.options.include_schema)
                {
                    continue;
                }
                let object = mapping(spec, statement, input)?;
                let key = (
                    object.source.database.clone(),
                    object.source.name.clone(),
                    object.source.kind.clone(),
                );
                if scoped.insert(key) {
                    let mut scope = item(input, number, "object_scope", &object, None);
                    scope.id.push_str(":object-scope");
                    scope.precondition = Some(source.index.sha256.clone());
                    scope
                        .notes
                        .push("选择或排除该对象及其所有结构和数据步骤".into());
                    schema.push(scope);
                }
            }
        }
        if !schema
            .iter()
            .any(|entry| entry.source.is_some() && entry.selected)
        {
            for entry in &mut schema {
                if entry.phase == "database" {
                    entry.selected = false;
                }
            }
        }
        Ok(schema)
    }
}
