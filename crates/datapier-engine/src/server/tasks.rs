//! 执行调度与任务终态。

use super::*;

impl Engine {
    async fn provider(
        &self,
        db: Arc<Database>,
        method: &str,
        mut params: Value,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
        transition: &AsyncMutex<()>,
    ) -> Result<Value> {
        let budgets = if method == "change.submit" {
            self.prepare_change_value_sources(&db, &mut params).await?
        } else {
            Vec::new()
        };
        let result = self
            .provider_inner(db, method, params, sink, cancel, transition)
            .await;
        for handle in budgets {
            self.store.release_auxiliary(handle).await?;
        }
        result
    }
    async fn provider_inner(
        &self,
        db: Arc<Database>,
        method: &str,
        params: Value,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
        transition: &AsyncMutex<()>,
    ) -> Result<Value> {
        db.profile.capabilities().ensure_method(method, &params)?;
        let gate = (db.profile.provider == model::ProviderKind::Mysql && !db.profile.is_archery())
            .then(|| self.editing_gate(db.profile.id));
        let control = if matches!(method, "execution.start" | "execution.run") {
            params
                .get("text")
                .and_then(Value::as_str)
                .and_then(datapier_core::providers::mysql::transaction_control)
        } else {
            None
        };
        let ending = matches!(
            method,
            "transaction.begin" | "transaction.commit" | "transaction.rollback"
        ) || control.is_some();
        // 结束原查询连接上的事务可释放正在等待的 DDL 所需元数据锁。
        let _gate_guard = match gate.as_ref().filter(|_| !ending) {
            Some(gate) => Some(
                tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), guard = gate.lock() => guard },
            ),
            None => None,
        };
        let session_id: Uuid = model::param(&params, "session_id")?;
        let transaction_control = control.is_some();
        let raw_write = gate.is_some()
            && matches!(method, "execution.start" | "execution.run")
            && !transaction_control
            && params
                .get("text")
                .and_then(Value::as_str)
                .is_none_or(|text| {
                    datapier_core::providers::mysql::validate_readonly(text).is_err()
                });
        let schema_write = method == "schema.execute";
        let data_write = gate.is_some()
            && (matches!(
                method,
                "change.submit"
                    | "replace.submit"
                    | "import.rows"
                    | "transfer.import"
                    | "transaction.begin"
                    | "transaction.commit"
            ) || matches!(
                control,
                Some(
                    datapier_core::providers::mysql::TransactionControl::Begin
                        | datapier_core::providers::mysql::TransactionControl::Commit
                        | datapier_core::providers::mysql::TransactionControl::AutocommitOn
                )
            ));
        if data_write
            && !matches!(
                method,
                "transaction.begin" | "transaction.commit" | "change.submit"
            )
            && !transaction_control
        {
            self.editing_check_dirty_except(db.profile.id, params.get("target"), None)?;
        }
        if schema_write || raw_write {
            self.editing_check_dirty(
                db.profile.id,
                if schema_write {
                    params.get("target")
                } else {
                    None
                },
            )?;
            if schema_write {
                for path in [
                    "/plan/intent/definition",
                    "/plan/intent/baseline/definition",
                ] {
                    if let Some(trigger) = params
                        .pointer(path)
                        .filter(|v| v.get("kind").and_then(Value::as_str) == Some("trigger"))
                    {
                        let target =
                            json!({"database":trigger.get("database"),"name":trigger.get("table")});
                        self.editing_check_dirty(db.profile.id, Some(&target))?;
                    }
                }
                for path in [
                    "/plan/intent/definition/constraints",
                    "/plan/intent/baseline/definition/constraints",
                ] {
                    if let Some(constraints) = params.pointer(path).and_then(Value::as_array) {
                        for constraint in constraints {
                            if constraint.get("kind").and_then(Value::as_str) == Some("foreign_key")
                            {
                                let target = json!({"database":constraint.get("referenced_database"),"name":constraint.get("referenced_table")});
                                self.editing_check_dirty(db.profile.id, Some(&target))?;
                            }
                        }
                    }
                }
            }
        }
        if matches!(method, "schema.execute" | "replace.submit")
            && let Some(id) = optional::<Uuid>(&params, "plan_id")?
        {
            self.editing_plan_current(id)?;
        }
        let mut session = tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), lock = db.session.lock() => lock };
        let is_mcp = {
            let state = self.state.lock().unwrap();
            let context = state
                .contexts
                .get(&db.owner)
                .ok_or_else(|| AppError::new("disconnected", "客户端会话已关闭"))?;
            matches!(context.kind, model::ClientKind::Mcp)
        };
        if !is_mcp && self.store.profile(db.profile.id).await?.revision != db.profile.revision {
            return Err(AppError::new("invalidated", "连接设置已变化，请重新连接"));
        }
        self.editing_apply_invalidation(session_id, &mut session);
        let mut params = params;
        if method == "change.submit" && gate.is_some() {
            let token: Uuid = model::param(&params, "token")?;
            let target = match &*session {
                ProviderSession::Mysql(mysql) => mysql.draft_target(token)?,
                _ => return Err(AppError::invalid("编辑会话不是 MySQL")),
            };
            Self::validate_submit_target(target, params.get("target"))?;
            let target = serialize(target)?;
            self.editing_check_dirty_except(db.profile.id, Some(&target), Some(session_id))?;
            params["target"] = target;
        }
        let invalidation_scope = params.get("target").cloned();
        let executing_plan = optional::<Uuid>(&params, "plan_id")?;
        if is_mcp {
            self.mcp_before_send(db.owner, &db, method, &params, &mut session)
                .await?;
        }
        {
            let _guard = transition.lock().await;
            if cancel.is_cancelled() {
                return Err(AppError::cancelled());
            }
            if matches!(method, "execution.start" | "execution.run")
                && let ProviderSession::Redis(redis) = &*session
            {
                // 排队期间 SELECT 可能改变命令连接，必须取得会话锁后记录实际 DB。
                self.store
                    .record_task_redis_db(sink.execution_id, redis.command_db())
                    .await?;
            }
            let wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| AppError::new("internal", e.to_string()))?
                .as_secs_f64();
            let instant = Instant::now();
            self.store
                .update_task(
                    sink.execution_id,
                    "running",
                    json!({"_task":{"started_at":wall}}),
                    None,
                )
                .await?;
            if let Some(running) = self.state.lock().unwrap().tasks.get_mut(&sink.execution_id) {
                running.started = Some(instant);
            }
            self.emit_state(db.owner, sink.execution_id, "running");
        }
        let draft_budget = if matches!(method, "change.preview" | "change.submit") {
            self.prepare_change_files(&db, &mut params).await?
        } else {
            None
        };
        let (result, mut session) = if schema_write {
            let executor = match &*session {
                ProviderSession::Mysql(mysql) => mysql.schema_executor()?,
                _ => return Err(AppError::invalid("结构执行仅支持 MySQL")),
            };
            drop(session);
            (executor.execute(params, sink, cancel).await, None)
        } else {
            let result = if method == "transfer.preview" {
                datapier_core::execution::transfer::preview(&mut session, params, sink, cancel)
                    .await
            } else if method.starts_with("transfer.") {
                datapier_core::execution::transfer::run(&mut session, method, params, sink, cancel)
                    .await
            } else {
                session
                    .call(
                        if method == "execution.start" {
                            "execution.run"
                        } else {
                            method
                        },
                        params,
                        sink,
                        cancel,
                    )
                    .await
            };
            (result, Some(session))
        };
        if is_mcp
            && result.is_ok()
            && matches!(
                control,
                Some(
                    datapier_core::providers::mysql::TransactionControl::Commit
                        | datapier_core::providers::mysql::TransactionControl::Rollback
                        | datapier_core::providers::mysql::TransactionControl::AutocommitOn
                )
            )
        {
            self.mcp_transaction_finished(session_id);
        }
        if schema_write || raw_write || data_write {
            // sent 持久化失败也不能证明未发送；保守失效并保留查询事务。
            let may_have_sent = self
                .store
                .task(sink.execution_id)
                .await
                .map(|task| task.sent)
                .unwrap_or(true);
            if result.is_ok() || may_have_sent {
                if schema_write || raw_write {
                    self.editing_invalidate_schema(
                        db.profile.id,
                        executing_plan,
                        invalidation_scope.as_ref(),
                    )
                    .await;
                } else {
                    self.editing_invalidate_data(db.profile.id).await;
                }
                if let Some(session) = session.as_mut() {
                    self.editing_apply_invalidation(session_id, session);
                }
            }
        }
        drop(session);
        if let Some(handle) = draft_budget {
            self.store.release_auxiliary(handle).await?;
        }
        if db.profile.is_archery() {
            db.capabilities.lock().unwrap().observe(method, &result);
        }
        let mut value = match result {
            Ok(value) => value,
            Err(error) => return Err(self.archery_error(&db, error).await),
        };
        if method == "connection.info" {
            value["source"] = json!(if db.profile.is_archery() {
                "archery"
            } else {
                "direct"
            });
            value["capabilities"] = serialize(&*db.capabilities.lock().unwrap())?;
            value["query_semantics"] = json!(if db.profile.is_archery() {
                "stateless"
            } else {
                "session"
            });
            if db.profile.is_archery() {
                value["database"] = json!(db.database);
                value["redis_db"] = json!(db.redis_db);
            }
        }
        if db.profile.is_archery() {
            archery::budget(&value)?;
        }
        Ok(value)
    }
    pub(super) fn apply_live_elapsed(&self, task: &mut model::TaskRecord) {
        if let Some(started) = self
            .state
            .lock()
            .unwrap()
            .tasks
            .get(&task.id)
            .and_then(|r| r.started)
        {
            task.elapsed_ms = Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        }
    }
    pub(super) async fn task(&self, id: Uuid) -> Result<model::TaskRecord> {
        let mut task = self.store.task(id).await?;
        if let Some(completion) = self.state.lock().unwrap().unpersisted.get(&id) {
            completion.apply(&mut task);
        }
        self.apply_live_elapsed(&mut task);
        Ok(task)
    }
    pub(super) async fn task_details(&self, id: Uuid) -> Result<model::TaskRecord> {
        let mut task = self.store.task_details(id).await?;
        if let Some(completion) = self.state.lock().unwrap().unpersisted.get(&id) {
            completion.apply(&mut task);
        }
        self.apply_live_elapsed(&mut task);
        Ok(task)
    }
    pub(super) async fn finish_task(
        &self,
        owner: Uuid,
        id: Uuid,
        result: &Result<Value>,
        started: Option<Instant>,
    ) {
        let transition = self
            .state
            .lock()
            .unwrap()
            .tasks
            .get(&id)
            .map(|task| task.transition.clone());
        let _guard = match transition.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        // 错误本身不能证明写入未发生；结合已持久化的 sent/may_write 判定是否需要人工核对。
        let record = self.store.task(id).await.ok();
        let (state, summary, error) = match result {
            Ok(value) => {
                let business = value
                    .get("state")
                    .or_else(|| value.get("status"))
                    .and_then(Value::as_str);
                match business {
                    Some("cancelled") => ("cancelled", value.clone(), Some(AppError::cancelled())),
                    Some("outcome_unknown") => (
                        "outcome_unknown",
                        value.clone(),
                        Some(AppError::unknown("执行结果需人工核对")),
                    ),
                    Some("failed" | "partial" | "verification_mismatch" | "conflict") => (
                        "failed",
                        value.clone(),
                        Some(AppError::new(
                            business.unwrap(),
                            "执行未全部完成，请查看逐项结果",
                        )),
                    ),
                    _ => ("succeeded", value.clone(), None),
                }
            }
            Err(error) => {
                // change.submit 的 rolled_back 仅在事务 ROLLBACK 得到确认后产生。
                let rolled_back = error.code == "cancelled"
                    && record.as_ref().is_some_and(|r| r.method == "change.submit")
                    && error
                        .details
                        .get("items")
                        .and_then(Value::as_array)
                        .is_some_and(|items| {
                            !items.is_empty()
                                && items.iter().enumerate().all(|(index, item)| {
                                    item.get("index").and_then(Value::as_u64) == Some(index as u64)
                                        && item.get("status").and_then(Value::as_str)
                                            == Some("rolled_back")
                                })
                        });
                let unknown = record.as_ref().is_some_and(|r| r.sent && r.may_write)
                    && (error.code == "cancelled"
                        || error.code == "disconnected"
                        || error.code == "outcome_unknown")
                    && !rolled_back;
                (
                    if unknown {
                        "outcome_unknown"
                    } else if error.code == "wait_stopped"
                        || (error.code == "cancelled"
                            && record.as_ref().is_some_and(|r| {
                                r.sent
                                    && r.context
                                        .as_ref()
                                        .is_some_and(|c| c.source.as_deref() == Some("archery"))
                            }))
                    {
                        "wait_stopped"
                    } else if error.code == "cancelled" {
                        "cancelled"
                    } else {
                        "failed"
                    },
                    (*error.details).clone(),
                    Some(error.clone()),
                )
            }
        };
        let mut summary = match summary {
            Value::Object(map) => Value::Object(map),
            Value::Null => json!({}),
            other => json!({"value":other}),
        };
        if record.as_ref().is_some_and(|r| {
            r.sent
                && r.context
                    .as_ref()
                    .is_some_and(|c| c.source.as_deref() == Some("archery"))
        }) && summary.get("remote_completion").is_none()
        {
            summary["remote_completion"] = if result.is_ok() {
                json!("completed")
            } else {
                summary
                    .pointer("/cause/remote_completion")
                    .cloned()
                    .or_else(|| {
                        record
                            .as_ref()
                            .and_then(|r| r.summary.get("remote_completion"))
                            .cloned()
                    })
                    .unwrap_or_else(|| json!("unknown"))
            };
        }
        let mut metadata = record
            .as_ref()
            .and_then(|r| r.summary.get("_task"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        if let Some(started) = started {
            metadata["elapsed_ms"] =
                json!(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        }
        summary["_task"] = metadata;
        let completion_summary = summary.clone();
        let mut final_state = state;
        if let Err(e) = self.store.update_task(id, state, summary, error).await {
            tracing::error!(task_id = %id, "任务状态持久化失败: {e}");
            let record = match record {
                Some(record) => Some(record),
                None => self.store.task(id).await.ok(),
            };
            // 连发送阶段都无法读回时也保守待确认，不能用本地存储失败推断数据库回滚。
            let unknown = record.as_ref().is_none_or(|r| r.sent && r.may_write);
            let mut message = format!("任务已结束，但最终记录无法保存到本地存储：{e}");
            if unknown {
                message.push_str("；可能已发送的写入需人工核对，请勿直接重试");
            }
            let completion = UnpersistedCompletion {
                state: if unknown { "outcome_unknown" } else { "failed" },
                summary: completion_summary,
                error: AppError::new("storage", message).with_details(json!({
                    "observed_state":state,
                    "execution_error":result.as_ref().err(),
                    "persistence_error":e,
                })),
            };
            final_state = completion.state;
            self.state
                .lock()
                .unwrap()
                .unpersisted
                .insert(id, completion);
        }
        self.emit_state(owner, id, final_state);
        self.state.lock().unwrap().tasks.remove(&id);
    }
    pub(super) async fn start(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
        background: bool,
    ) -> Result<Value> {
        self.start_inner(owner, method, params, background, None)
            .await
    }

    pub(super) async fn start_reserved(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
        background: bool,
        execution_id: Uuid,
    ) -> Result<Value> {
        self.start_inner(owner, method, params, background, Some(execution_id))
            .await
    }

    async fn start_inner(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
        background: bool,
        reserved: Option<Uuid>,
    ) -> Result<Value> {
        let db = self.database(owner, &params)?;
        let session_id: Uuid = model::param(&params, "session_id")?;
        let (initial_workspace, is_mcp) = self
            .state
            .lock()
            .unwrap()
            .contexts
            .get(&owner)
            .map(|context| {
                (
                    context.workspace.clone(),
                    matches!(context.kind, model::ClientKind::Mcp),
                )
            })
            .ok_or_else(|| AppError::new("disconnected", "客户端会话已关闭"))?;
        let dispatch_revision = if is_mcp {
            match params
                .pointer("/_access_proof/profile_revision")
                .and_then(Value::as_u64)
            {
                Some(revision) => revision,
                None => self.store.profile(db.profile.id).await?.revision,
            }
        } else {
            db.profile.revision
        };
        let prepare_budget = if method == "replace.prepare" {
            optional::<Uuid>(&params, "_editing_plan_budget")?
        } else {
            None
        };
        let work_budget = if matches!(method, "replace.submit" | "schema.execute") {
            optional::<Uuid>(&params, "_editing_work_budget")?
        } else {
            None
        };
        let submitted_plan = if method == "replace.submit" {
            Some(model::param::<Uuid>(&params, "plan_id")?)
        } else {
            None
        };
        let replacement_target = params.get("target").cloned().unwrap_or(Value::Null);
        if db.profile.provider == model::ProviderKind::Redis
            && matches!(method, "execution.start" | "execution.run")
        {
            let text: String = model::param(&params, "text")?;
            if datapier_core::providers::redis_contains_auth(&text)? {
                return Err(AppError::invalid("请通过连接设置进行 Redis 认证"));
            }
        }
        db.profile.capabilities().ensure_method(method, &params)?;
        if let Some(connection) = db.profile.archery()
            && matches!(
                method,
                "execution.start" | "execution.run" | "archery.sql_check"
            )
        {
            let database: String = model::param(&params, "database")?;
            if !connection.databases.contains(&database)
                || (db.profile.provider == model::ProviderKind::Redis
                    && db.database.as_deref() != Some(database.as_str()))
            {
                return Err(AppError::new(
                    "target_not_selected",
                    "目标 DB 不属于当前连接选择",
                ));
            }
        }
        let mut persisted_params = if matches!(
            method,
            "schema.execute" | "replace.prepare" | "replace.submit" | "change.submit"
        ) {
            let mut metadata = serde_json::Map::new();
            for key in ["target", "targets", "plan_id", "revision", "digest"] {
                if let Some(value) = params.get(key) {
                    metadata.insert(key.into(), value.clone());
                }
            }
            Value::Object(metadata)
        } else {
            params.clone()
        };
        redact(&mut persisted_params);
        let context = json!({"connection":db.profile.name,"provider":db.profile.provider,
            "source":if db.profile.is_archery() { "archery" } else { "direct" },
            "server_id":db.profile.archery().map(|a|a.server_id),"instance_id":db.profile.archery().map(|a|a.instance_id),
            "database":if !db.profile.is_archery() && matches!(method, "execution.start" | "execution.run") { None } else { params.pointer("/target/database").and_then(Value::as_str).or_else(|| params.get("database").and_then(Value::as_str)).or(db.database.as_deref()) },
            "redis_db":if !db.profile.is_archery() && matches!(method, "execution.start" | "execution.run") { None } else { db.redis_db },"targets":[]});
        if let Some(object) = persisted_params.as_object_mut() {
            object.insert("_task_context".into(), context);
        }
        // 与服务配置变更共用门，确保占用检查和任务登记串行化。
        let service_gate = db.profile.archery().map(|a| self.service_gate(a.server_id));
        let service_guard = match service_gate.as_ref() {
            Some(gate) => Some(gate.lock().await),
            None => None,
        };
        let limits = self.store.settings().await?.limits;
        let id = if let Some(id) = reserved {
            let task = self.store.task(id).await?;
            if task.owner != owner || task.sent || task.state != "approval_required" {
                return Err(AppError::conflict("审批任务不再可派发"));
            }
            self.store
                .update_task(id, "queued", task.summary, None)
                .await?;
            id
        } else {
            self.store
                .create_task(owner, workspace(&params), method, persisted_params, false)
                .await?
                .id
        };
        let cancel = CancellationToken::new();
        // 每任务独占控制门；排队、真正发送与完成都必须和客户端离开时的取消决策串行化。
        let transition = Arc::new(AsyncMutex::new(()));
        let accepted = {
            let mut state = self.state.lock().unwrap();
            let at_limit = state
                .contexts
                .get(&owner)
                .is_some_and(|ctx| matches!(ctx.kind, model::ClientKind::Mcp))
                && state
                    .tasks
                    .values()
                    .filter(|task| task.owner == owner)
                    .count()
                    >= limits.running_tasks_per_mcp_client;
            if at_limit {
                false
            } else {
                if !state.clients.contains(&owner)
                    || state
                        .contexts
                        .get(&owner)
                        .is_none_or(|context| context.workspace != initial_workspace)
                    || state
                        .databases
                        .get(&session_id)
                        .is_none_or(|db| db.closing.load(Ordering::Acquire))
                {
                    cancel.cancel();
                }
                state.result_sessions.insert(id, session_id);
                state.tasks.insert(
                    id,
                    Running {
                        owner,
                        session_id,
                        cancel: cancel.clone(),
                        transition: transition.clone(),
                        started: None,
                        abort: None,
                    },
                );
                true
            }
        };
        if !accepted {
            let error = AppError::new("resource_limit", "客户端并发任务数已达上限");
            self.store
                .update_task(id, "failed", json!({}), Some(error.clone()))
                .await?;
            return Err(error);
        }
        drop(service_guard);
        self.emit_state(owner, id, "queued");
        let timeout_engine = Arc::downgrade(self);
        let timeout_duration = Duration::from_secs(mysql_query_timeout(
            &db.profile,
            limits.query_timeout_seconds,
        ));
        tokio::spawn(async move {
            if let Some(engine) = timeout_engine.upgrade() {
                let mut activity = engine.store.subscribe_result_activity().await;
                let mut deadline = tokio::time::Instant::now() + timeout_duration;
                let mut previous_waiting = false;
                let mut completed_waits = 0;
                loop {
                    // 先确认通知版本，再读取状态，避免查询与睡眠之间漏掉状态变化。
                    activity.borrow_and_update();
                    let (waiting, completed) = engine.store.execution_wait_state(id).await;
                    if !waiting && (previous_waiting || completed != completed_waits) {
                        deadline = tokio::time::Instant::now() + timeout_duration;
                    }
                    previous_waiting = waiting;
                    completed_waits = completed;
                    if !engine.state.lock().unwrap().tasks.contains_key(&id) {
                        break;
                    }
                    tokio::select! {
                        changed = activity.changed() => { if changed.is_err() { break; } }
                        _ = tokio::time::sleep_until(deadline), if !waiting => {
                            let (now_waiting, now_completed) = engine.store.execution_wait_state(id).await;
                            if now_waiting || now_completed != completed_waits { continue; }
                            let running = engine.state.lock().unwrap().tasks.get(&id)
                                .map(|task| (task.cancel.clone(), task.transition.clone()));
                            if let Some((cancel, transition)) = running {
                                let _guard = transition.lock().await;
                                let (now_waiting, now_completed) = engine.store.execution_wait_state(id).await;
                                if now_waiting || now_completed != completed_waits { continue; }
                                if engine.state.lock().unwrap().tasks.contains_key(&id) {
                                    // 已发送写入只发取消信号；provider 继续等待协议尾部并判定终态。
                                    cancel.cancel();
                                }
                            }
                            break;
                        }
                    }
                }
            }
        });
        let engine = self.clone();
        let method = method.to_owned();
        let future = async move {
            let send_engine = Arc::downgrade(&engine);
            let send_db = db.clone();
            let sink = ExecutionSink::new(engine.store.clone(), id)
                .with_dispatch_control(transition.clone(), cancel.clone())
                .with_send_gate(
                    engine.send_gate.clone(),
                    Arc::new(move || {
                        let engine = send_engine
                            .upgrade()
                            .ok_or_else(|| AppError::new("disconnected", "引擎已关闭"))?;
                        let state = engine.state.lock().unwrap();
                        if !state.clients.contains(&owner)
                            || !state.contexts.contains_key(&owner)
                            || send_db.closing.load(Ordering::Acquire)
                            || state
                                .databases
                                .get(&session_id)
                                .is_none_or(|current| !Arc::ptr_eq(current, &send_db))
                        {
                            return Err(AppError::new("disconnected", "客户端或数据库会话已关闭"));
                        }
                        Ok(())
                    }),
                )
                .with_profile_revision(
                    db.profile.id,
                    dispatch_revision,
                    engine.profile_lock(db.profile.id),
                );
            let mut result = if cancel.is_cancelled() {
                Err(AppError::cancelled())
            } else {
                engine
                    .provider(db.clone(), &method, params, &sink, &cancel, &transition)
                    .await
            };
            if let Some(handle) = prepare_budget {
                let prepared_id = result
                    .as_ref()
                    .ok()
                    .and_then(|value| optional::<Uuid>(value, "plan_id").ok().flatten());
                if let Ok(value) = &result
                    && let Err(error) = engine
                        .bind_prepared_replacement(
                            &db,
                            session_id,
                            handle,
                            replacement_target,
                            value,
                        )
                        .await
                {
                    result = Err(error);
                }
                if result.is_err() {
                    if let Some(plan_id) = prepared_id {
                        let mut provider = db.session.lock().await;
                        let cleanup = provider
                            .call(
                                "replace.discard",
                                json!({"plan_id":plan_id}),
                                &sink,
                                &CancellationToken::new(),
                            )
                            .await;
                        if let Err(error) = cleanup {
                            tracing::warn!(%plan_id, "释放未发布替换计划失败: {error}");
                        }
                    }
                    if let Err(error) = engine.store.release_auxiliary(handle).await {
                        tracing::warn!("释放替换准备预算失败: {error}");
                    }
                }
            }
            if let Some(plan_id) = submitted_plan {
                let cleanup = {
                    let mut provider = db.session.lock().await;
                    provider
                        .call(
                            "replace.discard",
                            json!({"plan_id":plan_id}),
                            &sink,
                            &CancellationToken::new(),
                        )
                        .await
                };
                if let Err(error) = cleanup {
                    tracing::warn!(%plan_id, "释放已提交替换计划失败: {error}");
                } else if let Err(error) =
                    engine.release_submitted_replacement(owner, plan_id).await
                {
                    tracing::warn!(%plan_id, "释放替换计划预算失败: {error}");
                }
            }
            let started = engine
                .state
                .lock()
                .unwrap()
                .tasks
                .get(&id)
                .and_then(|task| task.started);
            engine.finish_task(owner, id, &result, started).await;
            if let Some(handle) = work_budget
                && let Err(error) = engine.store.release_auxiliary(handle).await
            {
                tracing::warn!("释放编辑执行预算失败: {error}");
            }
            result
        };
        let task = tokio::spawn(future);
        if let Some(running) = self.state.lock().unwrap().tasks.get_mut(&id) {
            running.abort = Some(task.abort_handle());
        }
        // background 只决定 RPC 是否立即返回任务 ID，后台执行仍具有相同的归属和持久化语义。
        if background {
            Ok(json!({"task_id":id}))
        } else {
            task.await
                .map_err(|e| AppError::new("internal", format!("任务异常结束：{e}")))?
        }
    }
}
