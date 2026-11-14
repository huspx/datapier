//! 替换计划的客户端归属、预算和一次提交绑定，行内容仍由 provider 持有。
use super::*;

// 4 MiB 有效请求在操作、ARGV、编码和传输中的副本，另留封装与结果余量。
const REDIS_WORK_BYTES: usize = 20 * 1024 * 1024;

impl Engine {
    pub(super) async fn replace_rpc(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        mut params: Value,
    ) -> Result<Value> {
        if method == "replace.prepare" {
            let session_id: Uuid = model::param(&params, "session_id")?;
            let db = self.editing_profile(owner, session_id).await?;
            db.profile.ensure_write()?;
            let settings = self.store.settings().await?;
            let available = self.store.available_memory_for_owner(owner).await?;
            let headroom = if db.profile.provider == model::ProviderKind::Redis {
                REDIS_WORK_BYTES
            } else {
                settings.limits.response_bytes
            };
            let budget = settings
                .editing
                .max_plan_bytes
                .min(available.saturating_sub(headroom));
            if budget == 0 {
                return Err(AppError::new(
                    "resource_limit",
                    "客户端没有准备替换计划的可用内存",
                ));
            }
            let handle = Uuid::new_v4();
            self.store
                .reserve_auxiliary_for_owner(owner, handle, budget + headroom)
                .await?;
            params["_max_plan_bytes"] = json!(budget);
            params["_max_plan_items"] = json!(settings.editing.max_plan_items);
            params["_editing_plan_budget"] = json!(handle);
            params["_editing_profile_revision"] = json!(db.profile.revision);
            match self.start(owner, "replace.prepare", params, true).await {
                Ok(response) => return Ok(response),
                Err(error) => {
                    self.store.release_auxiliary(handle).await?;
                    return Err(error);
                }
            }
        }
        let plan_id: Uuid = model::param(&params, "plan_id")?;
        let gate = {
            let state = self.state.lock().unwrap();
            let plan = state
                .editing
                .replacements
                .get(&plan_id)
                .ok_or_else(|| AppError::new("handle_expired", "替换计划已失效"))?;
            if plan.owner != owner {
                return Err(AppError::new("permission_denied", "替换计划属于其他客户端"));
            }
            plan.gate.clone()
        };
        let _guard = gate.lock().await;
        if method != "replace.discard" {
            self.editing_plan_current(plan_id)?;
        }
        let settings = self.store.settings().await?;
        let (session_id, profile_id, profile_revision, execution, budget_handle, expired) = {
            let mut state = self.state.lock().unwrap();
            let plan = state
                .editing
                .replacements
                .get_mut(&plan_id)
                .ok_or_else(|| AppError::new("handle_expired", "替换计划已失效"))?;
            let expired = plan.touched.elapsed().as_secs() >= settings.limits.result_idle_seconds;
            plan.touched = Instant::now();
            (
                plan.session_id,
                plan.profile_id,
                plan.profile_revision,
                plan.execution,
                plan.budget_handle,
                expired,
            )
        };
        if let Some(execution) = execution {
            if method == "replace.submit" {
                return Ok(json!({"task_id":execution,"execution_id":execution}));
            }
            if method != "replace.discard" {
                return Err(AppError::new("handle_expired", "该替换计划已经提交"));
            }
        }
        let db = self.editing_profile(owner, session_id).await?;
        if db.profile.id != profile_id || db.profile.revision != profile_revision {
            return Err(AppError::new("schema_conflict", "替换计划所属连接已变化"));
        }
        if params.get("session_id").is_some()
            && optional::<Uuid>(&params, "session_id")? != Some(session_id)
        {
            return Err(AppError::new(
                "permission_denied",
                "替换计划与数据库会话不匹配",
            ));
        }
        params["target"] = self
            .state
            .lock()
            .unwrap()
            .editing
            .replacements
            .get(&plan_id)
            .ok_or_else(|| AppError::new("handle_expired", "替换计划已失效"))?
            .target
            .clone();
        params["session_id"] = json!(session_id);
        if expired && execution.is_none() {
            let mut provider = db.session.lock().await;
            let sink = ExecutionSink::new(self.store.clone(), Uuid::nil());
            let cancel = self.editing_cancel(owner)?;
            let _ = provider
                .call(
                    "replace.discard",
                    json!({"plan_id":plan_id}),
                    &sink,
                    &cancel,
                )
                .await;
            self.state
                .lock()
                .unwrap()
                .editing
                .replacements
                .remove(&plan_id);
            self.store.release_auxiliary(budget_handle).await?;
            return Err(AppError::new(
                "handle_expired",
                "替换计划已过期，请重新准备",
            ));
        }
        if method == "replace.submit" {
            db.profile.ensure_write()?;
            let work_handle = Uuid::new_v4();
            let work_bytes = if db.profile.provider == model::ProviderKind::Redis {
                REDIS_WORK_BYTES
            } else {
                settings.limits.response_bytes
            };
            self.store
                .reserve_auxiliary_for_owner(owner, work_handle, work_bytes)
                .await?;
            params["_editing_work_budget"] = json!(work_handle);
            params["_editing_plan_id"] = json!(plan_id);
            let response = match self.start(owner, "replace.submit", params, true).await {
                Ok(response) => response,
                Err(error) => {
                    self.store.release_auxiliary(work_handle).await?;
                    return Err(error);
                }
            };
            let execution: Uuid = model::param(&response, "task_id")?;
            if let Some(plan) = self
                .state
                .lock()
                .unwrap()
                .editing
                .replacements
                .get_mut(&plan_id)
            {
                plan.execution = Some(execution);
            }
            self.emit_editing(owner, "edit_plan.changed", plan_id, 0, "submitted");
            return Ok(json!({"task_id":execution,"execution_id":execution}));
        }
        if !matches!(
            method,
            "replace.read" | "replace.update" | "replace.discard"
        ) {
            return Err(AppError::new("method_not_found", "未知替换计划操作"));
        }
        if method == "replace.discard"
            && execution.is_some_and(|id| self.state.lock().unwrap().tasks.contains_key(&id))
        {
            return Err(AppError::new("busy", "替换任务尚未结束，请先等待或取消"));
        }
        if method == "replace.discard" && execution.is_some() {
            self.state
                .lock()
                .unwrap()
                .editing
                .replacements
                .remove(&plan_id);
            self.store.release_auxiliary(budget_handle).await?;
            return Ok(json!({"discarded":true}));
        }
        let work_handle = Uuid::new_v4();
        let work_bytes = if db.profile.provider == model::ProviderKind::Redis {
            REDIS_WORK_BYTES
        } else {
            settings.limits.response_bytes
        };
        self.store
            .reserve_auxiliary_for_owner(owner, work_handle, work_bytes)
            .await?;
        let result = {
            let profile_gate = (db.profile.provider == model::ProviderKind::Mysql
                && !db.profile.is_archery())
            .then(|| self.editing_gate(db.profile.id));
            let _profile_guard = match profile_gate.as_ref() {
                Some(gate) => Some(gate.lock().await),
                None => None,
            };
            let mut provider = db.session.lock().await;
            self.editing_apply_invalidation(session_id, &mut provider);
            let sink = ExecutionSink::new(self.store.clone(), Uuid::nil());
            let cancel = self.editing_cancel(owner)?;
            provider.call(method, params, &sink, &cancel).await
        };
        self.store.release_auxiliary(work_handle).await?;
        if method == "replace.discard" && result.is_ok() {
            self.state
                .lock()
                .unwrap()
                .editing
                .replacements
                .remove(&plan_id);
            self.store.release_auxiliary(budget_handle).await?;
            self.emit_editing(owner, "edit_plan.changed", plan_id, 0, "discarded");
        }
        let value = result?;
        if let Some(revision) = value
            .get("revision")
            .or_else(|| value.pointer("/summary/revision"))
            .and_then(Value::as_u64)
            && let Some(plan) = self
                .state
                .lock()
                .unwrap()
                .editing
                .replacements
                .get_mut(&plan_id)
        {
            plan.revision = revision;
        }
        if serde_json::to_vec(&value)?.len() > settings.limits.response_bytes {
            return Err(AppError::new(
                "resource_limit",
                "替换预览超过响应字节预算，请减少预览条目",
            ));
        }
        Ok(value)
    }

    /// provider 准备完成后才发布可访问的计划；准备失败的前缀不可提交。
    pub(in crate::server) async fn bind_prepared_replacement(
        &self,
        db: &Database,
        session_id: Uuid,
        budget_handle: Uuid,
        target: Value,
        value: &Value,
    ) -> Result<()> {
        let id: Uuid = model::param(value, "plan_id")?;
        let bytes = value
            .get("retained_bytes")
            .or_else(|| value.pointer("/summary/retained_bytes"))
            .and_then(Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| AppError::new("internal", "替换计划缺少内存记账"))?;
        let owner = db.owner;
        let profile_id = db.profile.id;
        let profile_revision = db.profile.revision;
        let revision = value
            .get("revision")
            .or_else(|| value.pointer("/summary/revision"))
            .and_then(Value::as_u64)
            .filter(|revision| *revision > 0)
            .ok_or_else(|| AppError::new("internal", "替换计划缺少版本"))?;
        self.store
            .reserve_auxiliary_for_owner(owner, budget_handle, bytes)
            .await?;
        self.active(owner)?;
        self.state.lock().unwrap().editing.replacements.insert(
            id,
            ReplaceBinding {
                owner,
                session_id,
                profile_id,
                profile_revision,
                budget_handle,
                revision,
                target,
                gate: Arc::new(AsyncMutex::new(())),
                execution: None,
                touched: Instant::now(),
            },
        );
        self.emit_editing(owner, "edit_plan.changed", id, revision, "ready");
        Ok(())
    }

    pub(in crate::server) async fn release_submitted_replacement(
        &self,
        owner: Uuid,
        id: Uuid,
    ) -> Result<()> {
        let handle = {
            let state = self.state.lock().unwrap();
            state
                .editing
                .replacements
                .get(&id)
                .filter(|plan| plan.owner == owner)
                .map(|plan| plan.budget_handle)
        };
        if let Some(handle) = handle {
            self.store.release_auxiliary(handle).await?;
        }
        Ok(())
    }
}
