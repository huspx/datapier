//! 交换表单的只读候选发现；候选不替代正式准备时的权限和输入校验。
use super::*;
use datapier_core::model::transfer::{
    TRANSFER_VERSION, TransferEndpoint, TransferInput, TransferMode,
};

pub(super) struct CatalogLease {
    pub(super) owner: Uuid,
    key: String,
}

impl Engine {
    fn transfer_metadata_cancel(&self, owner: Uuid) -> Result<CancellationToken> {
        self.state
            .lock()
            .unwrap()
            .client_cancels
            .get(&owner)
            .cloned()
            .ok_or_else(|| AppError::new("disconnected", "客户端会话已关闭"))
    }

    async fn transfer_metadata_spec(
        &self,
        owner: Uuid,
        params: &Value,
        inputs: Vec<TransferInput>,
    ) -> Result<TransferSpec> {
        let mut spec = TransferSpec {
            version: TRANSFER_VERSION,
            mode: TransferMode::Export,
            provider: model::param(params, "provider")?,
            source: optional(params, "endpoint")?,
            destination: None,
            inputs,
            output: None,
            objects: Vec::new(),
            redis: Default::default(),
            options: Default::default(),
        };
        if optional::<Uuid>(params, "project_id")?.is_some() {
            if spec
                .source
                .as_ref()
                .and_then(|endpoint| endpoint.connection_alias.as_ref())
                .is_none()
            {
                return Err(AppError::new(
                    "binding_required",
                    "项目候选需要选择已绑定的逻辑连接",
                ));
            }
            let _binding_guard = self.transfer_binding_guard(params).await?;
            self.resolve_transfer_spec(owner, params, &mut spec).await?;
        } else if spec.source.is_some() {
            self.resolve_transfer_spec(owner, params, &mut spec).await?;
        }
        Ok(spec)
    }

    pub(super) async fn transfer_input_inspect(
        &self,
        owner: Uuid,
        params: &Value,
    ) -> Result<Value> {
        let input: TransferInput = model::param(params, "input")?;
        let mut spec = self
            .transfer_metadata_spec(owner, params, vec![input])
            .await?;
        let input = spec
            .inputs
            .pop()
            .ok_or_else(|| AppError::invalid("缺少输入文件"))?;
        let cancel = self.transfer_metadata_cancel(owner)?;
        let id = Uuid::new_v4();
        self.store
            .reserve_auxiliary_for_owner(owner, id, MAX_PLAN_BYTES)
            .await?;
        let reservation = MemoryReservation {
            store: self.store.clone(),
            id,
        };
        // 阻塞扫描拥有预算直至真正退出；客户端断连后，parser 检查同一取消信号。
        tokio::task::spawn_blocking(move || {
            let _reservation = reservation;
            migration::inspect_input(&input, spec.provider, &cancel)
        })
        .await
        .map_err(|error| AppError::new("io", format!("读取输入元数据失败：{error}")))?
    }

    async fn transfer_catalog_close(&self, owner: Uuid, id: Uuid) -> Result<()> {
        let db = {
            let mut state = self.state.lock().unwrap();
            if !state
                .transfers
                .catalogs
                .get(&id)
                .is_some_and(|lease| lease.owner == owner)
            {
                return Ok(());
            }
            state.transfers.catalogs.remove(&id);
            state.databases.remove(&id)
        };
        if let Some(db) = db {
            db.closing.store(true, Ordering::Release);
            db.session.lock().await.close().await?;
        }
        Ok(())
    }

    async fn transfer_catalog_call(
        &self,
        owner: Uuid,
        params: &Value,
        provider: &mut ProviderSession,
        method: &str,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let record = self
            .store
            .create_task(
                owner,
                workspace(params),
                "transfer.catalog",
                json!({"method":method}),
                false,
            )
            .await?;
        let sink = ExecutionSink::new(self.store.clone(), record.id);
        let limits = self.store.settings().await?.limits;
        let timeout = mysql_query_timeout(provider.profile(), limits.query_timeout_seconds);
        let started = Instant::now();
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(AppError::cancelled()),
            result = tokio::time::timeout(Duration::from_secs(timeout), provider.call(method, arguments, &sink, cancel)) => {
                result.unwrap_or_else(|_| Err(AppError::new("timeout", "加载候选超时")))
            }
        };
        self.finish_task(owner, record.id, &result, Some(started))
            .await;
        result
    }

    pub(super) async fn transfer_catalog(
        self: &Arc<Self>,
        owner: Uuid,
        params: &Value,
    ) -> Result<Value> {
        let scope = params
            .get("scope")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("请选择候选目录范围"))?;
        if !matches!(scope, "databases" | "objects" | "columns") {
            return Err(AppError::invalid("未知候选目录范围"));
        }
        let mut spec = self
            .transfer_metadata_spec(owner, params, Vec::new())
            .await?;
        let mut endpoint: TransferEndpoint = spec
            .source
            .take()
            .ok_or_else(|| AppError::invalid("请选择连接"))?;
        let (profile, fingerprint) =
            migration::resolve_endpoint(&self.store, &endpoint, spec.provider, false).await?;
        let cancel = self.transfer_metadata_cancel(owner)?;
        if endpoint.database.is_none() && spec.provider == model::ProviderKind::Mysql {
            endpoint.database = profile.default_database().map(str::to_owned);
        }
        if endpoint.redis_db.is_none() && spec.provider == model::ProviderKind::Redis {
            endpoint.redis_db = Some(profile.redis_db());
        }
        let method = match scope {
            "databases" => "database.list",
            "objects" => "resource.list",
            _ => "resource.describe",
        };
        let mut arguments = json!({"database":endpoint.database,"cursor":params.get("cursor"),"limit":500,"kind":params.get("kind")});
        if scope == "columns" {
            let mut target: model::ResourceTarget = model::param(params, "target")?;
            let database = if spec.provider == model::ProviderKind::Redis {
                endpoint.redis_db.map(|database| database.to_string())
            } else {
                endpoint.database.clone()
            };
            if target
                .database
                .as_ref()
                .is_some_and(|selected| Some(selected) != database.as_ref())
            {
                return Err(AppError::invalid("对象不属于选中的数据库"));
            }
            target.database = database;
            arguments = json!({"target":target});
        } else if scope == "objects"
            && spec.provider == model::ProviderKind::Mysql
            && endpoint.database.is_none()
        {
            return Err(AppError::invalid("请先选择数据库"));
        }
        let retained = scope == "objects" && spec.provider == model::ProviderKind::Redis;
        if !retained
            && (params.get("cursor").is_some_and(|value| !value.is_null())
                || optional::<Uuid>(params, "session_id")?.is_some())
        {
            return Err(AppError::invalid("此候选目录不接受扫描会话"));
        }
        let limits = self.store.settings().await?.limits;
        let timeout = mysql_connect_timeout(&profile, limits.connect_timeout_seconds);
        if retained {
            let key = migration::digest(&(&endpoint, &fingerprint))?;
            return self
                .transfer_catalog_scan(owner, params, endpoint, key, arguments, &cancel)
                .await;
        }
        let secret = self.store.secret(profile.id).await?;
        let mut provider = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AppError::cancelled()),
            result = tokio::time::timeout(Duration::from_secs(timeout), ProviderSession::connect(profile_with_connect_timeout(&profile, timeout), secret)) => result.map_err(|_| AppError::new("timeout", "连接超时"))??,
        };
        let result = self
            .transfer_catalog_call(owner, params, &mut provider, method, arguments, &cancel)
            .await;
        if !cancel.is_cancelled() {
            let _ = tokio::time::timeout(Duration::from_secs(timeout), provider.close()).await;
        }
        let mut catalog = result?;
        if scope == "databases"
            && spec.provider == model::ProviderKind::Mysql
            && let Some(databases) = catalog["databases"].as_array_mut()
        {
            let visible = profile.direct()?.visible_databases.as_ref();
            databases.retain(|database| {
                database.as_str().is_some_and(|database| {
                    visible.is_none_or(|allowed| allowed.iter().any(|allowed| allowed == database))
                        && (endpoint.connection_alias.is_none()
                            || endpoint.database.as_deref() == Some(database))
                })
            });
        }
        Ok(json!({"endpoint":endpoint,"catalog":catalog,"session_id":null}))
    }

    async fn transfer_catalog_scan(
        self: &Arc<Self>,
        owner: Uuid,
        params: &Value,
        endpoint: TransferEndpoint,
        key: String,
        arguments: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let existing = optional::<Uuid>(params, "session_id")?;
        if params.get("cursor").is_some_and(|value| !value.is_null()) && existing.is_none() {
            return Err(AppError::invalid("继续扫描必须提供候选会话"));
        }
        let id = if let Some(id) = existing {
            let state = self.state.lock().unwrap();
            let lease = state
                .transfers
                .catalogs
                .get(&id)
                .ok_or_else(|| AppError::new("handle_expired", "候选目录已过期，请重新加载"))?;
            if lease.owner != owner || lease.key != key {
                return Err(AppError::new("conflict", "候选目录的连接或数据库已改变"));
            }
            id
        } else {
            let opened = self
                .dispatch_internal(
                    owner,
                    "connection.open",
                    json!({"profile_id":endpoint.profile_id,"redis_db":endpoint.redis_db}),
                )
                .await?;
            let id: Uuid = model::param(&opened, "session_id")?;
            let accepted = {
                let mut state = self.state.lock().unwrap();
                if state
                    .transfers
                    .catalogs
                    .values()
                    .filter(|lease| lease.owner == owner)
                    .count()
                    >= 8
                    || !state.clients.contains(&owner)
                {
                    false
                } else {
                    state
                        .transfers
                        .catalogs
                        .insert(id, CatalogLease { owner, key });
                    true
                }
            };
            if !accepted {
                self.dispatch_internal(owner, "connection.close", json!({"session_id":id}))
                    .await?;
                return Err(AppError::new(
                    "resource_limit",
                    "候选目录过多，请关闭旧选择",
                ));
            }
            let engine = Arc::downgrade(self);
            tokio::spawn(async move {
                tokio::time::sleep(TTL).await;
                if let Some(engine) = engine.upgrade() {
                    let _ = engine.transfer_catalog_close(owner, id).await;
                }
            });
            id
        };
        let db = self.database(owner, &json!({"session_id":id}))?;
        let result = {
            let mut provider = db.session.lock().await;
            self.transfer_catalog_call(
                owner,
                params,
                &mut provider,
                "resource.list",
                arguments,
                cancel,
            )
            .await
        };
        let continued = result.as_ref().is_ok_and(|catalog| {
            catalog
                .get("cursor")
                .is_some_and(|cursor| !cursor.is_null())
        });
        if !continued {
            self.transfer_catalog_close(owner, id).await?;
        }
        Ok(json!({"endpoint":endpoint,"catalog":result?,"session_id":continued.then_some(id)}))
    }
}
