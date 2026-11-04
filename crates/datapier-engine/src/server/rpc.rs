//! Engine RPC 分派。

use super::*;

impl Engine {
    pub(super) async fn dispatch(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let archery_response = method.starts_with("archery.")
            || params
                .pointer("/profile/connection/kind")
                .and_then(Value::as_str)
                == Some("archery")
            || optional::<Uuid>(&params, "session_id")?.is_some_and(|id| {
                self.state
                    .lock()
                    .unwrap()
                    .databases
                    .get(&id)
                    .is_some_and(|db| db.profile.is_archery())
            });
        let result = self.dispatch_inner(owner, method, params).await;
        if archery_response && !matches!(method, "result.page" | "result.value") {
            match &result {
                Ok(value) => archery::budget(value)?,
                Err(error) => archery::budget(error)?,
            }
        }
        result
    }
    async fn dispatch_inner(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.active(owner)?;
        let owner_cancel = self
            .state
            .lock()
            .unwrap()
            .client_cancels
            .get(&owner)
            .cloned()
            .ok_or_else(|| AppError::new("disconnected", "客户端会话已关闭"))?;
        match method {
            method if method.starts_with("archery.") => {
                self.archery_rpc(owner, method, params).await
            }
            "settings.get" => serialize(self.store.settings().await?),
            "settings.set" => {
                let settings: model::AppSettings = model::param(&params, "settings")?;
                self.store.set_settings(&settings).await?;
                self.emit_settings_changed();
                serialize(settings)
            }
            "profile.databases" | "profile.test" => {
                let profile: Profile = model::param(&params, "profile")?;
                profile.validate()?;
                if profile.is_archery() {
                    return self
                        .test_archery_profile(owner, method, profile, &params)
                        .await;
                }
                if method == "profile.databases" && profile.provider != model::ProviderKind::Mysql {
                    return Err(AppError::invalid("仅 MySQL 连接支持加载数据库"));
                }
                let secret = optional(&params, "secret")?;
                let mut provider = tokio::select! {
                    biased;
                    _ = owner_cancel.cancelled() => return Err(AppError::cancelled()),
                    result = async {
                        let secret = match secret {
                            Some(value) => Some(value),
                            None => self.store.secret(profile.id).await?,
                        };
                        ProviderSession::connect(profile.clone(), secret).await
                    } => result?,
                };
                let record = self
                    .store
                    .create_task(owner, workspace(&params), method, json!({"_task_context":{"connection":profile.name,"provider":profile.provider,"source":"direct","database":profile.default_database(),"redis_db":if profile.provider == model::ProviderKind::Redis { Some(profile.redis_db()) } else { None },"targets":[]}}), false)
                    .await?;
                self.emit_state(owner, record.id, "queued");
                let mut started = None;
                let result = async {
                    let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| AppError::new("internal", e.to_string()))?.as_secs_f64();
                    let instant = Instant::now();
                    self.store.update_task(record.id, "running", json!({"_task":{"started_at":wall}}), None).await?;
                    started = Some(instant);
                    self.emit_state(owner, record.id, "running");
                    let sink = ExecutionSink::new(self.store.clone(), record.id);
                    tokio::select! {
                        biased;
                        _ = owner_cancel.cancelled() => Err(AppError::cancelled()),
                        result = provider.call(
                            if method == "profile.test" { "connection.info" } else { "resource.list" },
                            json!({}), &sink, &owner_cancel,
                        ) => result,
                    }
                }.await;
                // 已中断的只读连接直接丢弃，避免关闭握手再次阻塞断线清理。
                if !owner_cancel.is_cancelled() {
                    tokio::select! {
                        _ = owner_cancel.cancelled() => (),
                        _ = provider.close() => (),
                    }
                }
                self.finish_task(owner, record.id, &result, started).await;
                if method == "profile.test" {
                    Ok(json!({"ok":true,"info":result?}))
                } else {
                    result
                }
            }
            "profile.list" => {
                let profiles = self.store.profiles().await?;
                let has_archery = profiles.iter().any(Profile::is_archery);
                let response = json!({"profiles":profiles});
                if has_archery {
                    archery::budget(&response)?;
                }
                Ok(response)
            }
            "profile.save" => {
                let profile: Profile = model::param(&params, "profile")?;
                profile.validate()?;
                if profile.is_archery() {
                    return Err(AppError::invalid(
                        "Archery 连接配置必须通过 archery.profile.apply_batch 保存",
                    ));
                }
                let secret = optional(&params, "secret")?;
                let persist = params
                    .get("save_secret")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let engine = self.clone();
                // 保存任务独占连接配置锁直到凭据交接完成；调用方停止等待不能提前放行打开连接。
                tokio::spawn(async move {
                    let lock = engine.profile_lock(profile.id);
                    let _guard = lock.lock().await;
                    let saved = engine.store.save_profile(&profile, secret, persist).await?;
                    engine.emit_connections_changed();
                    Ok(json!({"profile":profile,"credential_saved":saved}))
                })
                .await
                .map_err(|e| AppError::new("internal", format!("保存连接配置任务异常结束：{e}")))?
            }
            "profile.delete" => {
                let id: Uuid = model::param(&params, "profile_id")?;
                let profile = self
                    .store
                    .profiles()
                    .await?
                    .into_iter()
                    .find(|p| p.id == id)
                    .ok_or_else(|| AppError::new("not_found", "连接不存在"))?;
                if let Some(connection) = profile.archery() {
                    let server_id = connection.server_id;
                    let operation =
                        self.register_operation(owner, Uuid::new_v4(), server_id, None, true)?;
                    let engine = self.clone();
                    return tokio::spawn(async move {
                        let gate = engine.service_gate(server_id);
                        let _service_guard = gate.lock().await;
                        let _operation = operation;
                        let lock = engine.profile_lock(id);
                        let _guard = lock.lock().await;
                        if engine.profile_occupied(id) {
                            return Err(AppError::new("busy", "连接仍在使用，不能删除配置"));
                        }
                        let current = engine
                            .store
                            .profiles()
                            .await?
                            .into_iter()
                            .find(|item| item.id == id)
                            .ok_or_else(|| AppError::new("not_found", "连接不存在"))?;
                        if serialize(&current)? != serialize(&profile)? {
                            return Err(AppError::conflict("连接配置已变化"));
                        }
                        engine.store.delete_profile(id).await?;
                        engine.emit_connections_changed();
                        Ok(json!({"deleted":true}))
                    })
                    .await
                    .map_err(|e| {
                        AppError::new("internal", format!("删除连接配置任务异常结束：{e}"))
                    })?;
                }
                let lock = self.profile_lock(id);
                let _guard = lock.lock().await;
                if self.profile_occupied(id) {
                    return Err(AppError::new("busy", "连接仍在使用，不能删除配置"));
                }
                self.store.delete_profile(id).await?;
                self.emit_connections_changed();
                Ok(json!({"deleted":true}))
            }
            "workspace.load" => {
                self.store
                    .workspace_load(&model::param::<String>(&params, "workspace_id")?)
                    .await
            }
            "workspace.save" => {
                let id: String = model::param(&params, "workspace_id")?;
                let revision = self
                    .store
                    .workspace_save(
                        &id,
                        model::param(&params, "expected_revision")?,
                        model::param(&params, "state")?,
                    )
                    .await?;
                Ok(json!({"revision":revision}))
            }
            "connection.open" => {
                let id: Uuid = model::param(&params, "profile_id")?;
                if self
                    .store
                    .profiles()
                    .await?
                    .iter()
                    .any(|p| p.id == id && p.is_archery())
                {
                    return self.open_archery(owner, &params, id).await;
                }
                let lock = self.profile_lock(id);
                let _guard = tokio::select! {
                    biased;
                    _ = owner_cancel.cancelled() => return Err(AppError::cancelled()),
                    guard = lock.lock() => guard,
                };
                // 只取消只读配置和连接等待；profile.save/delete 的凭据交接必须完成。
                let (profile, session) = tokio::select! {
                    biased;
                    _ = owner_cancel.cancelled() => return Err(AppError::cancelled()),
                    result = async {
                        let mut profile = self.store.profiles().await?.into_iter()
                            .find(|p| p.id == id)
                            .ok_or_else(|| AppError::new("not_found", "连接配置不存在"))?;
                        if let Some(redis_db) = optional::<u32>(&params, "redis_db")? {
                            if profile.provider != model::ProviderKind::Redis {
                                return Err(AppError::invalid("仅 Redis 连接支持指定 DB"));
                            }
                            profile.direct_mut()?.redis_db = redis_db;
                        }
                        let secret = match optional(&params, "secret")? {
                            Some(s) => Some(s),
                            None => self.store.secret(id).await?,
                        };
                        let session = ProviderSession::connect(profile.clone(), secret).await?;
                        Ok::<_, AppError>((profile, session))
                    } => result?,
                };
                let session_id = Uuid::new_v4();
                let db = Arc::new(Database {
                    owner,
                    profile: profile.clone(),
                    database: profile.default_database().map(str::to_owned),
                    redis_db: if profile.provider == model::ProviderKind::Redis {
                        Some(profile.redis_db())
                    } else {
                        None
                    },
                    capabilities: Mutex::new(profile.capabilities()),
                    session: AsyncMutex::new(session),
                    closing: AtomicBool::new(false),
                });
                let inserted = {
                    let mut state = self.state.lock().unwrap();
                    if state.clients.contains(&owner) {
                        state.databases.insert(session_id, db.clone());
                        true
                    } else {
                        false
                    }
                };
                if !inserted {
                    // owner 已离开，直接丢弃新建会话，不能在 profile 锁内等待关闭握手。
                    drop(db);
                    return Err(AppError::new("disconnected", "客户端会话已关闭"));
                }
                Ok(
                    json!({"session_id":session_id,"profile":profile,"source":"direct","database":profile.default_database(),"redis_db":if profile.provider==model::ProviderKind::Redis {Some(profile.redis_db())}else{None},"query_semantics":"session","capabilities":profile.capabilities()}),
                )
            }
            "connection.close" => {
                let db = self.database(owner, &params)?;
                let id: Uuid = model::param(&params, "session_id")?;
                let tasks = {
                    let state = self.state.lock().unwrap();
                    if db.closing.swap(true, Ordering::AcqRel) {
                        return Err(AppError::new("busy", "数据库会话正在关闭"));
                    }
                    state
                        .tasks
                        .iter()
                        .filter(|(_, t)| t.session_id == id)
                        .map(|(id, t)| (*id, t.cancel.clone(), t.transition.clone()))
                        .collect::<Vec<_>>()
                };
                for (task_id, cancel, transition) in tasks {
                    self.cancel_if_safe(task_id, cancel, transition).await;
                }
                let result = db.session.lock().await.close().await;
                self.state.lock().unwrap().databases.remove(&id);
                result?;
                Ok(json!({"closed":true}))
            }
            "connection.info" | "resource.list" | "resource.describe" | "change.open"
            | "change.discard" | "change.preview" | "transfer.preview" => {
                self.start(owner, method, params, false).await
            }
            "resource.browse" | "execution.start" | "change.submit" | "transfer.import"
            | "transfer.export" => self.start(owner, method, params, true).await,
            "execution.get" => {
                let id = model::param(&params, "task_id")?;
                let include_text = optional::<bool>(&params, "include_text")?.unwrap_or(false);
                let mut task = if include_text {
                    self.store.task_details(id).await?
                } else {
                    self.store.task(id).await?
                };
                if let Some(completion) = self.state.lock().unwrap().unpersisted.get(&id) {
                    completion.apply(&mut task);
                }
                self.apply_live_elapsed(&mut task);
                serialize(task)
            }
            "execution.list" => {
                let mut tasks = self
                    .store
                    .tasks(&model::param::<String>(&params, "workspace_id")?)
                    .await?;
                let state = self.state.lock().unwrap();
                if !state.unpersisted.is_empty() {
                    for task in &mut tasks {
                        if let Some(completion) = state.unpersisted.get(&task.id) {
                            completion.apply(task);
                        }
                    }
                }
                drop(state);
                for task in &mut tasks {
                    self.apply_live_elapsed(task);
                }
                Ok(json!({"executions":tasks}))
            }
            "execution.cancel" => {
                let id: Uuid = model::param(&params, "task_id")?;
                let task = self.task(id).await?;
                if task.owner != owner {
                    return Err(AppError::new(
                        "permission_denied",
                        "不能取消其他客户端的任务",
                    ));
                }
                let running = self
                    .state
                    .lock()
                    .unwrap()
                    .tasks
                    .get(&id)
                    .map(|t| (t.cancel.clone(), t.transition.clone()));
                if let Some((cancel, transition)) = running {
                    cancel.cancel();
                    let _guard = transition.lock().await;
                    // 完成与取消共用控制门；取得门后重读，不能把已结束任务改回 cancelling。
                    let now = self.task(id).await?;
                    if matches!(now.state.as_str(), "queued" | "running" | "cancelling") {
                        self.store
                            .update_task(id, "cancelling", now.summary, None)
                            .await?;
                        if now.state != "cancelling" {
                            self.emit_state(owner, id, "cancelling");
                        }
                        Ok(json!({"task_id":id,"state":"cancelling"}))
                    } else {
                        Ok(json!({"task_id":id,"state":now.state}))
                    }
                } else {
                    Ok(json!({"task_id":id,"state":task.state}))
                }
            }
            "history.list" => {
                let state_overrides: HashMap<Uuid, &str> = self
                    .state
                    .lock()
                    .unwrap()
                    .unpersisted
                    .iter()
                    .map(|(&id, completion)| (id, completion.state))
                    .collect();
                let search_states =
                    optional::<Vec<String>>(&params, "search_states")?.unwrap_or_default();
                self.store
                    .history(
                        &model::param::<String>(&params, "workspace_id")?,
                        optional::<u64>(&params, "offset")?.unwrap_or(0),
                        optional::<u64>(&params, "limit")?
                            .unwrap_or(20)
                            .clamp(1, 100),
                        optional::<String>(&params, "search")?
                            .as_deref()
                            .unwrap_or(""),
                        &state_overrides,
                        &search_states,
                    )
                    .await
            }
            "result.pagination" => serialize(
                self.store
                    .result_pagination(
                        model::param(&params, "result_id")?,
                        model::param(&params, "page_size")?,
                        optional::<usize>(&params, "max_bytes")?.unwrap_or(1024 * 1024),
                    )
                    .await?,
            ),
            "result.page" => serialize(
                self.store
                    .result_page(
                        model::param(&params, "result_id")?,
                        optional(&params, "start")?.unwrap_or(0),
                        optional::<usize>(&params, "max_rows")?
                            .unwrap_or(200)
                            .clamp(1, 1000),
                        optional::<usize>(&params, "max_bytes")?
                            .unwrap_or(1024 * 1024)
                            .clamp(1, 4 * 1024 * 1024),
                    )
                    .await?,
            ),
            "result.value" => {
                self.store
                    .large_value(
                        model::param(&params, "value_id")?,
                        optional(&params, "offset")?.unwrap_or(0),
                        optional::<usize>(&params, "max_bytes")?
                            .unwrap_or(1024 * 1024)
                            .clamp(1, 4 * 1024 * 1024),
                    )
                    .await
            }
            "result.release" => {
                let id: Uuid = model::param(&params, "result_id")?;
                match self.store.result_meta(id).await {
                    Ok(meta) => {
                        if self
                            .state
                            .lock()
                            .unwrap()
                            .tasks
                            .contains_key(&meta.execution_id)
                        {
                            return Err(AppError::new(
                                "busy",
                                "任务仍在写入结果，请在任务结束后释放",
                            ));
                        }
                    }
                    Err(error) if error.code == "not_found" => {}
                    Err(error) => return Err(error),
                }
                self.store.release_result(id).await?;
                Ok(json!({"released":true}))
            }
            "file.open" => {
                let path: String = model::param(&params, "path")?;
                if path.trim().is_empty() {
                    return Err(AppError::invalid("文件路径不能为空"));
                }
                let text = tokio::fs::read_to_string(&path).await?;
                Ok(json!({"path":path,"text":text}))
            }
            "file.save" => {
                let path: String = model::param(&params, "path")?;
                if path.trim().is_empty() {
                    return Err(AppError::invalid("文件路径不能为空"));
                }
                let text: String = model::param(&params, "text")?;
                tokio::fs::write(&path, &text).await?;
                Ok(json!({"path":path,"text":text}))
            }
            _ => Err(AppError::new(
                "method_not_found",
                format!("未知方法：{method}"),
            )),
        }
    }
}
