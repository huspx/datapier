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
        let result = self.dispatch_inner(owner, method, params, false).await;
        if archery_response && !matches!(method, "result.page" | "result.value") {
            match &result {
                Ok(value) => archery::budget(value)?,
                Err(error) => archery::budget(error)?,
            }
        }
        result
    }
    pub(super) async fn dispatch_internal(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        // MCP 等内部调用会重新进入大型 RPC 分派器；把递归层放到堆上，避免在工作线程栈内嵌套完整 future。
        Box::pin(self.dispatch_inner(owner, method, params, true)).await
    }
    async fn dispatch_inner(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
        internal: bool,
    ) -> Result<Value> {
        self.active(owner)?;
        let context = self
            .state
            .lock()
            .unwrap()
            .contexts
            .get(&owner)
            .cloned()
            .ok_or_else(|| AppError::new("disconnected", "客户端会话已关闭"))?;
        if !internal {
            match context.kind {
                model::ClientKind::ValueEditor
                    if !matches!(method, "value.editor.claim" | "value.editor.finish") =>
                {
                    return Err(AppError::new(
                        "permission_denied",
                        "编辑器 helper 只能领取和完成自身交接",
                    ));
                }
                model::ClientKind::WindowBridge if method != "ui.window.sync" => {
                    return Err(AppError::new("permission_denied", "窗口桥接不能调用此方法"));
                }
                model::ClientKind::Mcp if !method.starts_with("mcp.") => {
                    return Err(AppError::new(
                        "permission_denied",
                        "MCP 客户端不能直接调用内部方法",
                    ));
                }
                model::ClientKind::Tui if method == "approval.decide" => {
                    return Err(AppError::new("permission_denied", "配置界面不能决定审批"));
                }
                _ => (),
            }
        }
        let owner_cancel = self
            .state
            .lock()
            .unwrap()
            .client_cancels
            .get(&owner)
            .cloned()
            .ok_or_else(|| AppError::new("disconnected", "客户端会话已关闭"))?;
        match method {
            "workspace.activate" => {
                self.ensure_editing_client(owner)?;
                let gate = self.workspace_owner_gate(owner);
                let _guard = gate.lock().await;
                let workspace_id: String = model::param(&params, "workspace_id")?;
                let expected: String = model::param(&params, "expected_workspace_id")?;
                if self.store.workspace_revision(&workspace_id).await? == 0 {
                    return Err(AppError::new("not_found", "目标工作区尚未保存"));
                }
                let mut state = self.state.lock().unwrap();
                if state.tasks.values().any(|task| task.owner == owner) {
                    return Err(AppError::new("busy", "工作区仍有运行中的任务"));
                }
                if state.editing.workspace_busy(owner) {
                    return Err(AppError::new(
                        "busy",
                        "请先关闭当前工作区的值编辑器和外部编辑会话",
                    ));
                }
                let context = state
                    .contexts
                    .get_mut(&owner)
                    .ok_or_else(|| AppError::new("disconnected", "客户端会话已关闭"))?;
                if context.workspace != expected {
                    return Err(AppError::new("stale_revision", "当前工作区已变化"));
                }
                context.workspace = workspace_id.clone();
                state.editing.invalidate_owner_plans(owner);
                Ok(json!({"workspace_id":workspace_id}))
            }
            "workspace.draft_scope" => self.editing_draft_scope(owner, &params).await,
            "workspace.save.begin"
            | "workspace.save.append"
            | "workspace.save.commit"
            | "workspace.save.read"
            | "workspace.save.close"
            | "workspace.save.abort" => self.workspace_save_rpc(owner, method, params).await,
            "change.preview" => self.editing_rpc(owner, method, params).await,
            method
                if method.starts_with("schema.")
                    || method.starts_with("replace.")
                    || method.starts_with("value.") =>
            {
                self.editing_rpc(owner, method, params).await
            }
            method if method.starts_with("mcp.") => {
                Box::pin(self.mcp_rpc(owner, method, params)).await
            }
            "ui.window.bind" => {
                if !matches!(context.kind, model::ClientKind::Tsp) {
                    return Err(AppError::new("permission_denied", "仅 TSP 可绑定窗口"));
                }
                let nonce: String = model::param(&params, "nonce")?;
                Uuid::parse_str(&nonce).map_err(|_| AppError::invalid("无效窗口绑定 nonce"))?;
                let active = optional::<bool>(&params, "active")?.unwrap_or(false);
                let mut state = self.state.lock().unwrap();
                if state
                    .windows
                    .get(&nonce)
                    .is_some_and(|binding| binding.owner != owner)
                {
                    return Err(AppError::conflict("窗口绑定 nonce 已使用"));
                }
                if active && !state.windows.contains_key(&nonce) {
                    return Err(AppError::new("permission_denied", "窗口尚未绑定"));
                }
                let binding = state.windows.entry(nonce).or_insert(WindowBinding {
                    owner,
                    window_id: None,
                    pane_id: None,
                    seen: None,
                    last_active: Instant::now(),
                });
                if active {
                    binding.last_active = Instant::now();
                }
                Ok(json!({"bound":true}))
            }
            "ui.window.sync" => {
                if !matches!(context.kind, model::ClientKind::WindowBridge) {
                    return Err(AppError::new("permission_denied", "仅窗口桥接可同步窗口"));
                }
                let nonce: String = model::param(&params, "nonce")?;
                let window_id: String = model::param(&params, "window_id")?;
                let pane_id: String = model::param(&params, "pane_id")?;
                let target = {
                    let mut state = self.state.lock().unwrap();
                    if let Some(binding) = state.windows.get_mut(&nonce) {
                        if binding
                            .window_id
                            .as_ref()
                            .is_some_and(|id| id != &window_id)
                            || binding.pane_id.as_ref().is_some_and(|id| id != &pane_id)
                        {
                            return Err(AppError::new("permission_denied", "窗口绑定不匹配"));
                        }
                        binding.window_id = Some(window_id);
                        binding.pane_id = Some(pane_id.clone());
                        binding.seen = Some(Instant::now());
                        Some(binding.owner)
                    } else {
                        None
                    }
                };
                let notifications = match target {
                    Some(target) => self.mcp_notifications(target).await?,
                    None => json!([]),
                };
                Ok(
                    json!({"bound":target.is_some(),"client_session_id":target,"pane_id":pane_id,"notifications":notifications}),
                )
            }
            "approval.list" | "approval.get" | "approval.decide" => {
                Box::pin(self.mcp_ui(owner, method, params)).await
            }
            "profile.approval.set" => {
                let id = model::param(&params, "profile_id")?;
                let revision = model::param(&params, "expected_revision")?;
                let policy = model::param(&params, "approval_policy")?;
                let lock = self.profile_lock(id);
                let _guard = lock.lock().await;
                let profile = self
                    .store
                    .set_profile_approval(id, revision, policy)
                    .await?;
                self.emit_connections_changed();
                Ok(json!({"profile":profile}))
            }
            "database.list" => self.discover_databases(owner, &params).await,
            "profile.rules.get" => {
                let id: Uuid = model::param(&params, "profile_id")?;
                let profile = self.store.profile(id).await?;
                Ok(
                    json!({"profile_id":id,"revision":profile.revision,"rules":self.store.mcp_rules(id).await?}),
                )
            }
            "profile.rules.set" => {
                let id: Uuid = model::param(&params, "profile_id")?;
                let expected = model::param(&params, "expected_revision")?;
                let rules: Vec<model::McpAccessRule> = model::param(&params, "rules")?;
                let lock = self.profile_lock(id);
                let _guard = lock.lock().await;
                let revision = self.store.save_mcp_rules(id, expected, &rules).await?;
                self.emit_connections_changed();
                Ok(json!({"revision":revision}))
            }
            "cache.retired.preview" => self.store.retired_cache_preview().await,
            "cache.retired.clear" => {
                let expected: Value = model::param(&params, "expected_preview")?;
                self.store.clear_retired_cache(&expected).await
            }
            method if method.starts_with("archery.") => {
                self.archery_rpc(owner, method, params).await
            }
            "settings.get" => serialize(self.store.settings().await?),
            "settings.set" => {
                let settings: model::AppSettings = model::param(&params, "settings")?;
                self.store.set_settings(&settings).await?;
                self.emit_settings_changed();
                serialize(self.store.settings().await?)
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
                        if profile.provider == model::ProviderKind::Mysql {
                            let fallback = self.store.settings().await?.limits.connect_timeout_seconds;
                            let timeout = mysql_connect_timeout(&profile, fallback);
                            tokio::time::timeout(
                                Duration::from_secs(timeout),
                                ProviderSession::connect(profile_with_connect_timeout(&profile, timeout), secret),
                            ).await.map_err(|_| AppError::new("timeout", "连接超时"))?
                        } else {
                            ProviderSession::connect(profile.clone(), secret).await
                        }
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
                    if profile.provider == model::ProviderKind::Mysql {
                        let fallback = self.store.settings().await?.limits.query_timeout_seconds;
                        let timeout = mysql_query_timeout(&profile, fallback);
                        let cancel = owner_cancel.child_token();
                        let call = provider.call(
                            if method == "profile.test" { "connection.info" } else { "resource.list" },
                            json!({}), &sink, &cancel,
                        );
                        tokio::pin!(call);
                        tokio::select! {
                            biased;
                            _ = owner_cancel.cancelled() => Err(AppError::cancelled()),
                            result = &mut call => result,
                            _ = tokio::time::sleep(Duration::from_secs(timeout)) => {
                                cancel.cancel();
                                let _ = call.await;
                                Err(AppError::new("timeout", "查询超时"))
                            }
                        }
                    } else {
                        tokio::select! {
                            biased;
                            _ = owner_cancel.cancelled() => Err(AppError::cancelled()),
                            result = provider.call(
                                if method == "profile.test" { "connection.info" } else { "resource.list" },
                                json!({}), &sink, &owner_cancel,
                            ) => result,
                        }
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
                let update = params
                    .pointer("/secret_update/kind")
                    .and_then(Value::as_str);
                let (secret, clear_secret) = match update {
                    None => (optional(&params, "secret")?, false),
                    Some("keep") => (None, false),
                    Some("replace") => (
                        Some(model::param(&params["secret_update"], "value")?),
                        false,
                    ),
                    Some("clear") => (None, true),
                    Some(_) => return Err(AppError::invalid("无效的凭据更新方式")),
                };
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
                    if clear_secret {
                        engine.store.clear_profile_secret(profile.id).await?;
                    }
                    engine.emit_connections_changed();
                    Ok(json!({"profile":engine.store.profile(profile.id).await?,"credential_saved":saved && !clear_secret}))
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
            "workspace.load" => self.workspace_load(owner, params).await,
            "workspace.save" => self.workspace_save_small(owner, params).await,
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
                        if let Some(database) = optional::<String>(&params, "database")? {
                            if profile.provider != model::ProviderKind::Mysql {
                                return Err(AppError::invalid("仅 MySQL 连接支持指定数据库"));
                            }
                            if profile.direct()?.visible_databases.as_ref().is_some_and(|visible| !visible.contains(&database)) {
                                return Err(AppError::new("permission_denied", "数据库不在可见范围"));
                            }
                            profile.direct_mut()?.database = Some(database);
                        }
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
                        let fallback = self.store.settings().await?.limits.connect_timeout_seconds;
                        let timeout = mysql_connect_timeout(&profile, fallback);
                        let session = tokio::time::timeout(Duration::from_secs(timeout), ProviderSession::connect(profile_with_connect_timeout(&profile, timeout), secret))
                            .await.map_err(|_| AppError::new("timeout", "连接超时"))??;
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
                        state.discovery.remove(&profile.id);
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
                let send_guard = self.send_gate.lock().await;
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
                drop(send_guard);
                for (task_id, cancel, transition) in tasks {
                    self.cancel_if_safe(task_id, cancel.clone(), transition.clone())
                        .await;
                    if db.profile.provider == model::ProviderKind::Mysql && !db.profile.is_archery()
                    {
                        // 写入已发送时仅停止展示；MySQL provider 会继续消费协议终态。
                        let _guard = transition.lock().await;
                        cancel.cancel();
                    }
                }
                let executions = {
                    let state = self.state.lock().unwrap();
                    let executions: Vec<_> = state
                        .result_sessions
                        .iter()
                        .filter_map(|(execution, session)| (*session == id).then_some(*execution))
                        .collect();
                    executions
                };
                let mut results = HashSet::new();
                for &execution in &executions {
                    let task = match self.store.task(execution).await {
                        Ok(task) => task,
                        Err(error) if error.code == "not_found" => continue,
                        Err(error) => return Err(error),
                    };
                    if task.owner == owner {
                        for result_id in task.results {
                            self.store.release_result(result_id).await?;
                            results.insert(result_id);
                        }
                    }
                }
                let result = db.session.lock().await.close().await;
                for execution in executions {
                    let task = match self.store.task(execution).await {
                        Ok(task) => task,
                        Err(error) if error.code == "not_found" => continue,
                        Err(error) => return Err(error),
                    };
                    if task.owner == owner {
                        for result_id in task.results {
                            if results.insert(result_id) {
                                self.store.release_result(result_id).await?;
                            }
                        }
                    }
                }
                self.state
                    .lock()
                    .unwrap()
                    .mcp
                    .release_session_handles(owner, id, &results);
                {
                    let mut state = self.state.lock().unwrap();
                    state.databases.remove(&id);
                    state.result_sessions.retain(|_, session| *session != id);
                }
                result?;
                Ok(json!({"closed":true}))
            }
            "resource.scan.discard" => {
                let db = self.database(owner, &params)?;
                let mut session = db.session.lock().await;
                self.database(owner, &params)?;
                let cancel = {
                    let state = self.state.lock().unwrap();
                    state
                        .client_cancels
                        .get(&owner)
                        .cloned()
                        .ok_or_else(|| AppError::new("disconnected", "客户端会话已关闭"))?
                };
                session
                    .call(
                        method,
                        params,
                        &ExecutionSink::new(self.store.clone(), Uuid::nil()),
                        &cancel,
                    )
                    .await
            }
            "connection.info" | "resource.list" | "resource.describe" | "change.open"
            | "change.discard" | "transfer.preview" => {
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
                if !self.can_observe_task(owner, &task).await? {
                    return Err(AppError::new(
                        "permission_denied",
                        "不能读取其他客户端的任务",
                    ));
                }
                if let Some(completion) = self.state.lock().unwrap().unpersisted.get(&id) {
                    completion.apply(&mut task);
                }
                self.apply_live_elapsed(&mut task);
                serialize(task)
            }
            "execution.observe.list" => {
                if !matches!(context.kind, model::ClientKind::Tsp) {
                    return Err(AppError::new(
                        "permission_denied",
                        "仅 TSP 可观察 Agent 任务",
                    ));
                }
                let owners = self.state.lock().unwrap().mcp_owners.clone();
                let mut observed = Vec::new();
                for task in self.store.tasks("default").await? {
                    if owners.contains(&task.owner) && self.mcp_observation_allowed(&task).await? {
                        observed.push(json!({"id":task.id,"state":task.state,"created_at":task.created_at,"results":task.results}));
                    }
                }
                Ok(json!({"executions":observed}))
            }
            "execution.list" => {
                let tasks = self
                    .store
                    .tasks(&model::param::<String>(&params, "workspace_id")?)
                    .await?;
                let mut visible = Vec::with_capacity(tasks.len());
                for task in tasks {
                    if self.can_observe_task(owner, &task).await? {
                        visible.push(task);
                    }
                }
                let mut tasks = visible;
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
                    self.cancel_if_safe(id, cancel, transition.clone()).await;
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
            "result.read" => self.read_result(owner, &params).await,
            "result.count" => self.count_result(owner, &params).await,
            "result.value" => {
                let value_id: Uuid = model::param(&params, "value_id")?;
                match self.store.edit_file(value_id).await {
                    Ok(record) => {
                        self.ensure_editing_client(owner)?;
                        let workspace = context.workspace.as_str();
                        if record.workspace_id != workspace
                            || (!self.editing_owns_file(owner, value_id)
                                && !self
                                    .store
                                    .edit_file_referenced_in_workspace(value_id, workspace)
                                    .await?)
                        {
                            return Err(AppError::new(
                                "permission_denied",
                                "草稿文件不属于当前工作区",
                            ));
                        }
                        return self
                            .store
                            .draft_value_chunk(
                                value_id,
                                optional(&params, "offset")?.unwrap_or(0),
                                optional::<usize>(&params, "max_bytes")?
                                    .unwrap_or(262144)
                                    .clamp(1, 262144),
                            )
                            .await?
                            .ok_or_else(|| AppError::new("not_found", "用户草稿不存在"));
                    }
                    Err(error) if error.code == "not_found" => (),
                    Err(error) => return Err(error),
                }
                let result_id = self.store.value_result_id(value_id).await?;
                let meta = self.owned_result(owner, result_id).await?;
                let offset = optional(&params, "offset")?.unwrap_or(0);
                let max_bytes = optional::<usize>(&params, "max_bytes")?
                    .unwrap_or(262144)
                    .clamp(1, 262144);
                if self.store.task(meta.execution_id).await?.owner == owner {
                    self.store.large_value(value_id, offset, max_bytes).await
                } else {
                    self.store
                        .peek_large_value(value_id, offset, max_bytes)
                        .await
                }
            }
            "result.release" => {
                let id: Uuid = model::param(&params, "result_id")?;
                let meta = self.owned_result(owner, id).await?;
                if self.store.task(meta.execution_id).await?.owner != owner {
                    return Err(AppError::new("permission_denied", "观察者不能释放结果"));
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
