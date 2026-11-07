//! 结果句柄归属、按需读取与实时分页。

use super::*;

impl Engine {
    pub(super) async fn can_observe_task(
        &self,
        owner: Uuid,
        task: &model::TaskRecord,
    ) -> Result<bool> {
        if task.owner == owner {
            return Ok(true);
        }
        let observer = {
            let state = self.state.lock().unwrap();
            state
                .contexts
                .get(&owner)
                .is_some_and(|ctx| matches!(ctx.kind, model::ClientKind::Tsp))
                && state.mcp_owners.contains(&task.owner)
        };
        Ok(observer && self.mcp_observation_allowed(task).await?)
    }

    pub(super) async fn owned_result(&self, owner: Uuid, id: Uuid) -> Result<model::ResultMeta> {
        let meta = self.store.peek_result_meta(id).await?;
        let task = self.store.task(meta.execution_id).await?;
        if !self.can_observe_task(owner, &task).await? {
            return Err(AppError::new(
                "permission_denied",
                "不能读取其他客户端的结果",
            ));
        }
        if task.owner == owner {
            self.store.result_meta(id).await
        } else {
            Ok(meta)
        }
    }

    async fn result_provider(
        &self,
        owner: Uuid,
        meta: &model::ResultMeta,
        method: &str,
        params: Value,
        timeout: u64,
    ) -> Result<Value> {
        let (session_id, db, owner_cancel) = {
            let state = self.state.lock().unwrap();
            let session_id = *state
                .result_sessions
                .get(&meta.execution_id)
                .ok_or_else(|| AppError::new("expired", "结果会话已失效"))?;
            let db = state
                .databases
                .get(&session_id)
                .ok_or_else(|| AppError::new("expired", "结果会话已关闭"))?;
            if db.closing.load(Ordering::Acquire) {
                return Err(AppError::new("expired", "结果会话已关闭"));
            }
            let owner_cancel = state
                .client_cancels
                .get(&owner)
                .cloned()
                .ok_or_else(|| AppError::new("disconnected", "客户端会话已关闭"))?;
            (session_id, db.clone(), owner_cancel)
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout);
        let gate = (db.profile.provider == model::ProviderKind::Mysql && !db.profile.is_archery())
            .then(|| self.editing_gate(db.profile.id));
        let _gate_guard = match gate.as_ref() {
            Some(gate) if method == "result.count" => Some(
                gate.try_lock()
                    .map_err(|_| AppError::new("busy", "原数据库连接正忙"))?,
            ),
            Some(gate) => Some(tokio::select! {
                _ = owner_cancel.cancelled() => return Err(AppError::cancelled()),
                guard = tokio::time::timeout_at(deadline, gate.lock()) =>
                    guard.map_err(|_| AppError::new("timeout", "等待数据库执行门超时"))?,
            }),
            None => None,
        };
        let cancel = CancellationToken::new();
        let sink = ExecutionSink::new(self.store.clone(), meta.execution_id);
        let mut session = if method == "result.count" {
            db.session
                .try_lock()
                .map_err(|_| AppError::new("busy", "原数据库会话正忙"))?
        } else {
            tokio::select! {
                _ = owner_cancel.cancelled() => return Err(AppError::cancelled()),
                session = tokio::time::timeout_at(deadline, db.session.lock()) =>
                    session.map_err(|_| AppError::new("timeout", "等待数据库会话超时"))?,
            }
        };
        self.editing_apply_invalidation(session_id, &mut session);
        self.mcp_revalidate_result(owner, &db, meta.execution_id, &mut session)
            .await?;
        let call = session.call(method, params, &sink, &cancel);
        tokio::pin!(call);
        let disconnected = tokio::select! {
            result = &mut call => return result,
            _ = owner_cancel.cancelled() => true,
            _ = tokio::time::sleep_until(deadline) => false,
        };
        // 不能丢弃持有物理连接的 provider future；取消后等待协议尾部。
        cancel.cancel();
        let _ = call.await;
        Err(if disconnected {
            AppError::cancelled()
        } else {
            AppError::new("timeout", "读取结果超时")
        })
    }

    pub(super) async fn read_result(&self, owner: Uuid, params: &Value) -> Result<Value> {
        let id: Uuid = model::param(params, "result_id")?;
        let meta = self.owned_result(owner, id).await?;
        let settings = self.store.settings().await?;
        let start = optional::<u64>(params, "start")?.unwrap_or(0);
        let rows = optional::<usize>(params, "max_rows")?
            .unwrap_or(settings.paging.sql_rows)
            .clamp(1, 1000);
        let bytes = optional::<usize>(params, "max_bytes")?
            .unwrap_or(settings.limits.response_bytes)
            .clamp(1, settings.limits.response_bytes);
        if self.store.task(meta.execution_id).await?.owner != owner {
            return serialize(self.store.peek_result_page(id, start, rows, bytes).await?);
        }
        if meta.summary.get("retrieval_mode").and_then(Value::as_str) == Some("live_page") {
            return self
                .result_provider(
                    owner,
                    &meta,
                    "result.read",
                    json!({"result_id":id,"start":start,"max_rows":rows,"max_bytes":bytes}),
                    settings.limits.query_timeout_seconds,
                )
                .await;
        }
        let through = start.saturating_add(rows as u64);
        self.store.set_result_demand(id, through).await?;
        tokio::time::timeout(
            Duration::from_secs(settings.limits.query_timeout_seconds),
            self.store.wait_result_rows(id, through),
        )
        .await
        .map_err(|_| AppError::new("timeout", "等待结果超时"))??;
        serialize(self.store.result_page(id, start, rows, bytes).await?)
    }

    pub(super) async fn count_result(&self, owner: Uuid, params: &Value) -> Result<Value> {
        let id: Uuid = model::param(params, "result_id")?;
        let meta = self.owned_result(owner, id).await?;
        if self.store.task(meta.execution_id).await?.owner != owner {
            return Err(AppError::new("permission_denied", "观察者不能统计结果"));
        }
        let timeout = self.store.settings().await?.limits.count_timeout_seconds;
        self.result_provider(
            owner,
            &meta,
            "result.count",
            json!({"result_id":id}),
            timeout,
        )
        .await
    }
}
