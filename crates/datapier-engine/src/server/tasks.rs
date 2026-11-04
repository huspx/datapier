//! 执行调度与任务终态。

use super::*;

impl Engine {
    async fn provider(
        &self,
        db: Arc<Database>,
        method: &str,
        params: Value,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
        transition: &AsyncMutex<()>,
    ) -> Result<Value> {
        db.profile.capabilities().ensure_method(method, &params)?;
        let mut session = tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), lock = db.session.lock() => lock };
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
        let result = if method == "transfer.preview" {
            datapier_core::execution::transfer::preview(&mut session, params, sink, cancel).await
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
            Ok(value) => ("succeeded", value.clone(), None),
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
        let db = self.database(owner, &params)?;
        let session_id: Uuid = model::param(&params, "session_id")?;
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
        let mut persisted_params = params.clone();
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
        let record = self
            .store
            .create_task(owner, workspace(&params), method, persisted_params, false)
            .await?;
        let id = record.id;
        let cancel = CancellationToken::new();
        // 每任务独占控制门；排队、真正发送与完成都必须和客户端离开时的取消决策串行化。
        let transition = Arc::new(AsyncMutex::new(()));
        {
            let mut state = self.state.lock().unwrap();
            if !state.clients.contains(&owner)
                || state
                    .databases
                    .get(&session_id)
                    .is_none_or(|db| db.closing.load(Ordering::Acquire))
            {
                cancel.cancel();
            }
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
        }
        drop(service_guard);
        self.emit_state(owner, id, "queued");
        let engine = self.clone();
        let method = method.to_owned();
        let future = async move {
            let sink = ExecutionSink::new(engine.store.clone(), id)
                .with_dispatch_control(transition.clone(), cancel.clone());
            let result = if cancel.is_cancelled() {
                Err(AppError::cancelled())
            } else {
                engine
                    .provider(db, &method, params, &sink, &cancel, &transition)
                    .await
            };
            let started = engine
                .state
                .lock()
                .unwrap()
                .tasks
                .get(&id)
                .and_then(|task| task.started);
            engine.finish_task(owner, id, &result, started).await;
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
