//! 冻结请求、唯一审批派发与真实执行状态。
use super::*;
use sha2::{Digest, Sha256};

fn task_terminal(state: &str) -> bool {
    !matches!(
        state,
        "queued" | "running" | "cancelling" | "approval_required"
    )
}
impl Engine {
    fn mcp_gate(&self, session_id: Uuid) -> Arc<AsyncMutex<()>> {
        self.state
            .lock()
            .unwrap()
            .mcp
            .gates
            .entry(session_id)
            .or_default()
            .clone()
    }
    fn mcp_emit_approval(&self, approval: &ApprovalRecord) {
        let mut state = self.state.lock().unwrap();
        state.event_seq += 1;
        let event = EngineEvent {
            seq: state.event_seq,
            kind: "approval.changed".into(),
            task_id: approval.execution_id,
            payload: json!({"approval_id":approval.id,"state":approval.state}),
        };
        for (client, sender) in &state.event_senders {
            if *client == approval.owner
                || state
                    .contexts
                    .get(client)
                    .is_some_and(|context| context.kind == ClientKind::Tsp)
            {
                let _ = sender.send(event.clone());
            }
        }
    }
    fn mcp_controller(&self, approval: &ApprovalRecord, claim: bool) -> Option<Uuid> {
        let online = self.mcp_online();
        let preferred = self.mcp_preferred_window(approval.profile_id);
        let mut state = self.state.lock().unwrap();
        let previous = state
            .mcp
            .approvals
            .get(&approval.id)
            .and_then(|route| route.controller);
        let controller = if previous == Some(approval.owner)
            || previous.is_some_and(|id| online.contains(&id))
        {
            previous
        } else {
            preferred.or(if claim { Some(approval.owner) } else { None })
        };
        state.mcp.approvals.insert(
            approval.id,
            ApprovalRoute {
                owner: approval.owner,
                execution_id: approval.execution_id,
                controller,
            },
        );
        controller
    }
    async fn mcp_approval_view(
        &self,
        viewer: Uuid,
        approval: &ApprovalRecord,
        include_text: bool,
        claim: bool,
    ) -> Result<Value> {
        let controller = self.mcp_controller(approval, claim);
        let tsp = controller.is_some_and(|id| id != approval.owner);
        let mut value = json!({"approval_id":approval.id,"execution_id":approval.execution_id,"session_id":approval.session_id,
            "profile_id":approval.profile_id,"state":approval.state,"expires_at":approval.expires_at,"digest":approval.digest,
            "route":if tsp {"tsp"} else {"mcp"},"tsp_interactive":tsp,"can_decide":controller == Some(viewer) && approval.state == "pending",
            "dangerous_reason":approval.request.get("dangerous_reason")});
        if include_text {
            value["request"] = approval.request.clone();
        }
        Ok(value)
    }
    pub(super) async fn mcp_execute(
        self: &Arc<Self>,
        owner: Uuid,
        tool: &str,
        mut params: Value,
    ) -> Result<Value> {
        let session_id: Uuid = model::param(&params, "session_id")?;
        let db = self.database(owner, &params)?;
        let gate = self.mcp_gate(session_id);
        let _guard = gate.lock().await;
        self.mcp_refresh_session(owner, session_id).await?;
        let settings = self.store.settings().await?;
        let (_, page_size) = paging(
            &params,
            if tool == "mcp.browse_resource" {
                settings.paging.mysql_table_rows
            } else {
                settings.paging.sql_rows
            },
            settings.limits.mcp_response_rows,
        )?;
        // 小响应预算仍须允许短命令；封装预留不能耗尽整个预算。
        let command_budget =
            settings.limits.response_bytes - (settings.limits.response_bytes / 4).min(2048);
        if serde_json::to_vec(&params)?.len() > command_budget {
            return Err(AppError::new(
                "response_limit",
                "命令及参数超过限制，未发送",
            ));
        }
        if let Some(parameters) = params.get("parameters").and_then(Value::as_array)
            && !parameters.is_empty()
            && (db.profile.provider != ProviderKind::Mysql || db.profile.is_archery())
        {
            return Err(AppError::new(
                "capability_unavailable",
                "此来源没有参数绑定能力，请使用原生命令文本",
            ));
        }
        let method = match tool {
            "mcp.browse_resource" => "resource.browse",
            "mcp.check_sql" => {
                if !db.profile.is_archery() || db.profile.provider != ProviderKind::Mysql {
                    return Err(AppError::new(
                        "capability_unavailable",
                        "仅 Archery MySQL 支持 SQL 检查",
                    ));
                }
                "archery.sql_check"
            }
            "mcp.execute_command" => "execution.start",
            "mcp.begin_transaction" | "mcp.commit_transaction" | "mcp.rollback_transaction" => {
                if db.profile.is_archery() {
                    return Err(AppError::new(
                        "capability_unavailable",
                        "平台查询没有专属事务会话",
                    ));
                }
                params["text"] = json!(match (db.profile.provider, tool) {
                    (ProviderKind::Mysql, "mcp.begin_transaction") => "BEGIN",
                    (ProviderKind::Mysql, "mcp.commit_transaction") => "COMMIT",
                    (ProviderKind::Mysql, _) => "ROLLBACK",
                    (ProviderKind::Redis, "mcp.begin_transaction") => "MULTI",
                    (ProviderKind::Redis, "mcp.commit_transaction") => "EXEC",
                    (ProviderKind::Redis, _) => "DISCARD",
                });
                "execution.start"
            }
            _ => return Err(AppError::invalid("未知执行工具")),
        };
        params["page_size"] = json!(page_size);
        params["workspace_id"] = json!(self.mcp_context(owner)?.workspace);
        if db.profile.is_archery() && params.get("database").is_none() {
            params["database"] = json!(db.database);
        }
        let proof = self
            .mcp_make_proof(owner, &db, session_id, method, &params)
            .await?;
        if method == "resource.browse" && db.profile.provider == ProviderKind::Mysql {
            Self::mcp_freeze_target_database(&db, &mut params, &proof)?;
        }
        params["_access_proof"] = serialize(&proof)?;
        let profile = self.mcp_validate_proof(owner, &proof, true).await?;
        let required = match profile.approval_policy {
            model::ApprovalPolicy::Off => false,
            model::ApprovalPolicy::Dangerous => proof.dangerous,
            model::ApprovalPolicy::AllWrites => proof.may_write,
        };
        if required {
            let created_at = now()?;
            let request = json!({"method":method,"params":params,"proof":proof,"dangerous_reason":proof.reason});
            if serde_json::to_vec(&request)?.len() > command_budget {
                return Err(AppError::new(
                    "response_limit",
                    "完整审批请求超过限制，未发送",
                ));
            }
            let record = self
                .store
                .create_task(
                    owner,
                    workspace(&params),
                    method,
                    params.clone(),
                    proof.may_write,
                )
                .await?;
            let approval = ApprovalRecord {
                id: Uuid::new_v4(),
                execution_id: record.id,
                owner,
                session_id,
                profile_id: profile.id,
                digest: format!("{:x}", Sha256::digest(serde_json::to_vec(&request)?)),
                request,
                profile_revision: profile.revision,
                policy_revision: settings.revision,
                created_at,
                expires_at: created_at
                    .checked_add(
                        i64::try_from(settings.approval.ttl_seconds)
                            .map_err(|_| AppError::invalid("审批期限过大"))?,
                    )
                    .ok_or_else(|| AppError::invalid("审批期限溢出"))?,
                state: "pending".into(),
                decided_at: None,
                decision_source: None,
            };
            if let Err(error) = self.store.create_approval(&approval).await {
                self.store
                    .update_task(record.id, "failed", Value::Null, Some(error.clone()))
                    .await?;
                return Err(error);
            }
            self.store
                .update_task(
                    record.id,
                    "approval_required",
                    json!({"approval_id":approval.id}),
                    None,
                )
                .await?;
            self.state
                .lock()
                .unwrap()
                .mcp
                .sessions
                .get_mut(&session_id)
                .ok_or_else(|| denied("会话已关闭"))?
                .pending = Some(record.id);
            let view = self
                .mcp_approval_view(owner, &approval, false, false)
                .await?;
            self.mcp_emit_approval(&approval);
            return Ok(
                json!({"execution_id":record.id,"session_id":session_id,"state":"approval_required","approval_id":approval.id,"approval":view}),
            );
        }
        let started = self.start(owner, method, params, true).await?;
        let execution_id: Uuid = model::param(&started, "task_id")?;
        if let Some(entry) = self.state.lock().unwrap().mcp.sessions.get_mut(&session_id) {
            entry.pending = Some(execution_id);
        }
        drop(_guard);
        self.mcp_execution(
            owner,
            &json!({"execution_id":execution_id,"_include_first_page":true}),
        )
        .await
    }
    async fn mcp_expire(&self, approval: &ApprovalRecord) -> Result<ApprovalRecord> {
        if approval.state == "pending" && approval.expires_at <= now()? {
            let expired = self
                .store
                .decide_approval(approval.id, approval.owner, "expire", "engine")
                .await?;
            if expired.state == "expired" {
                self.store
                    .update_task(
                        approval.execution_id,
                        "expired",
                        json!({"approval_id":approval.id}),
                        None,
                    )
                    .await?;
                self.mcp_emit_approval(&expired);
            }
            Ok(expired)
        } else {
            Ok(approval.clone())
        }
    }
    async fn mcp_validate_approval(&self, approval: &ApprovalRecord) -> Result<AccessProof> {
        let result = match serde_json::from_value::<AccessProof>(approval.request["proof"].clone())
        {
            Ok(proof) => self
                .mcp_validate_proof(approval.owner, &proof, approval.state == "pending")
                .await
                .map(|_| proof),
            Err(_) => Err(denied("审批没有可靠访问范围证明")),
        };
        match result {
            Ok(proof) => Ok(proof),
            Err(error) => {
                if approval.state == "pending" {
                    let invalidated = self
                        .store
                        .decide_approval(approval.id, approval.owner, "invalidate", "engine")
                        .await?;
                    if matches!(invalidated.state.as_str(), "invalidated" | "expired") {
                        self.store
                            .update_task(
                                approval.execution_id,
                                &invalidated.state,
                                json!({"approval_id":approval.id}),
                                Some(error.clone()),
                            )
                            .await?;
                        self.mcp_emit_approval(&invalidated);
                    }
                }
                Err(error)
            }
        }
    }
    pub(super) async fn mcp_expire_task(&self, task: &model::TaskRecord) -> Result<bool> {
        let Some(id) = task
            .summary
            .get("approval_id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
        else {
            return Ok(false);
        };
        let approval = self.store.approval(id).await?;
        let approval = self.mcp_expire(&approval).await?;
        Ok(approval.state != "pending")
    }
    pub(super) async fn mcp_approve(
        self: &Arc<Self>,
        owner: Uuid,
        params: &Value,
    ) -> Result<Value> {
        let id: Uuid = model::param(params, "approval_id")?;
        let decision: String = model::param(params, "decision")?;
        if !matches!(decision.as_str(), "approve" | "reject") {
            return Err(AppError::invalid("审批决定无效"));
        }
        if params.as_object().is_some_and(|map| {
            map.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "approval_id" | "decision" | "interaction_source"
                )
            })
        }) {
            return Err(AppError::invalid("审批不能附带或替换命令"));
        }
        let approval = self.store.approval(id).await?;
        if approval.owner != owner {
            return Err(denied("审批属于其他客户端"));
        }
        let source: String = model::param(params, "interaction_source")?;
        if !matches!(source.as_str(), "native_elicitation" | "trusted_host") {
            return Err(AppError::invalid("审批交互来源无效"));
        }
        self.mcp_decide(owner, approval, &decision, &source, false)
            .await
    }
    async fn mcp_decide(
        self: &Arc<Self>,
        actor: Uuid,
        approval: ApprovalRecord,
        decision: &str,
        source: &str,
        tsp: bool,
    ) -> Result<Value> {
        let gate = self.mcp_gate(approval.session_id);
        let _guard = gate.lock().await;
        let approval = self
            .mcp_expire(&self.store.approval(approval.id).await?)
            .await?;
        if approval.state != "pending" {
            return self
                .mcp_execution(
                    approval.owner,
                    &json!({"execution_id":approval.execution_id}),
                )
                .await;
        }
        let controller = self.mcp_controller(&approval, !tsp);
        if controller != Some(actor) || (tsp && !self.mcp_online().contains(&actor)) {
            return Err(AppError::new(
                "approval_required",
                "请在拥有当前交互控制权的界面决定审批",
            ));
        }
        let proof = self.mcp_validate_approval(&approval).await?;
        self.database(approval.owner, &json!({"session_id":approval.session_id}))?;
        let current_generation = self
            .state
            .lock()
            .unwrap()
            .mcp
            .sessions
            .get(&approval.session_id)
            .map(|session| session.generation);
        if current_generation != Some(proof.generation) {
            let invalidated = self
                .store
                .decide_approval(approval.id, approval.owner, "invalidate", "engine")
                .await?;
            let error = AppError::new("invalidated", "审批等待期间会话代次已变化");
            self.store
                .update_task(
                    approval.execution_id,
                    &invalidated.state,
                    json!({"approval_id":approval.id}),
                    Some(error.clone()),
                )
                .await?;
            self.mcp_emit_approval(&invalidated);
            return Err(error);
        }
        let decided = self
            .store
            .decide_approval(approval.id, approval.owner, decision, source)
            .await?;
        self.mcp_emit_approval(&decided);
        if decided.state == "dispatching" {
            let method = approval.request["method"]
                .as_str()
                .ok_or_else(|| AppError::invalid("冻结请求缺少方法"))?;
            let request = approval.request["params"].clone();
            if let Err(error) = self
                .start_reserved(approval.owner, method, request, true, approval.execution_id)
                .await
            {
                self.store
                    .update_task(
                        approval.execution_id,
                        "failed",
                        json!({"approval_id":approval.id}),
                        Some(error.clone()),
                    )
                    .await?;
                return Err(error);
            }
        } else {
            self.store
                .update_task(
                    approval.execution_id,
                    &decided.state,
                    json!({"approval_id":approval.id}),
                    None,
                )
                .await?;
        }
        drop(_guard);
        self.mcp_execution(
            approval.owner,
            &json!({"execution_id":approval.execution_id}),
        )
        .await
    }
    pub(super) async fn mcp_execution(
        self: &Arc<Self>,
        owner: Uuid,
        params: &Value,
    ) -> Result<Value> {
        let id: Uuid = model::param(params, "execution_id")?;
        let include_text = optional::<bool>(params, "include_text")?.unwrap_or(false);
        let mut task = if include_text {
            self.store.task_details(id).await?
        } else {
            self.task(id).await?
        };
        if task.owner != owner {
            return Err(denied("执行属于其他客户端"));
        }
        let proof: AccessProof = serde_json::from_value(
            self.store
                .execution_proof(id)
                .await?
                .ok_or_else(|| denied("执行没有可靠访问范围证明"))?,
        )?;
        self.mcp_validate_proof(owner, &proof, false).await?;
        if task.state == "approval_required" && self.mcp_expire_task(&task).await? {
            task = self.task(id).await?;
        }
        let state_name = if task.state == "approval_required" {
            "approval_required"
        } else {
            task.state.as_str()
        };
        let mut value = json!({"execution_id":id,"session_id":proof.session_id,"state":state_name,"sent":task.sent,"may_write":task.may_write,
            "target":{"connection_id":proof.profile_id,"read_databases":proof.read_databases,"write_databases":proof.write_databases},
            "results":task.results,"result_summaries":task.result_summaries,"summary":task.summary,"error":task.error,"elapsed_ms":task.elapsed_ms});
        if include_text {
            value["text"] = json!(task.text);
        }
        let approval_id = task
            .summary
            .get("approval_id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
            .or_else(|| {
                self.state
                    .lock()
                    .unwrap()
                    .mcp
                    .approvals
                    .iter()
                    .find(|(_, route)| route.execution_id == id)
                    .map(|(id, _)| *id)
            });
        if let Some(approval_id) = approval_id {
            let approval = self.store.approval(approval_id).await?;
            value["approval_id"] = json!(approval_id);
            value["approval"] = self
                .mcp_approval_view(owner, &approval, include_text, include_text)
                .await?;
        }
        if params.get("_include_first_page") == Some(&json!(true))
            && let Some(first) = task.results.first()
            && self
                .store
                .result_meta(*first)
                .await
                .is_ok_and(|meta| meta.complete || meta.row_count >= 100)
        {
            match self
                .mcp_read(owner, &json!({"result_id":first,"page":1}))
                .await
            {
                Ok(page) => {
                    let limit = self.store.settings().await?.limits.response_bytes;
                    if serde_json::to_vec(&value)?
                        .len()
                        .saturating_add(serde_json::to_vec(&page)?.len())
                        .saturating_add(32)
                        <= limit
                    {
                        value["first_page"] = page;
                    }
                }
                Err(error)
                    if matches!(
                        error.code.as_str(),
                        "busy" | "result_not_ready" | "not_found" | "expired"
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(value)
    }
    pub(super) async fn mcp_cancel(self: &Arc<Self>, owner: Uuid, params: &Value) -> Result<Value> {
        let id: Uuid = model::param(params, "execution_id")?;
        let task = self.task(id).await?;
        if task.owner != owner {
            return Err(denied("不能取消其他客户端的执行"));
        }
        if task.state == "approval_required" {
            let approval_id: Uuid = model::param(&task.summary, "approval_id")?;
            let approval = self.store.approval(approval_id).await?;
            let gate = self.mcp_gate(approval.session_id);
            let _guard = gate.lock().await;
            let cancelled = self
                .store
                .decide_approval(approval_id, owner, "cancel", "mcp")
                .await?;
            if matches!(cancelled.state.as_str(), "cancelled" | "expired") {
                self.store
                    .update_task(
                        id,
                        &cancelled.state,
                        json!({"approval_id":approval_id}),
                        None,
                    )
                    .await?;
                self.mcp_emit_approval(&cancelled);
            }
        } else if !task_terminal(&task.state) {
            self.dispatch_internal(owner, "execution.cancel", json!({"task_id":id}))
                .await?;
        }
        self.mcp_execution(owner, &json!({"execution_id":id})).await
    }
    pub(in crate::server) async fn mcp_notifications(&self, target: Uuid) -> Result<Value> {
        let ids: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .mcp
            .approvals
            .keys()
            .copied()
            .collect();
        let mut notifications = Vec::new();
        let settings = self.store.settings().await?;
        if settings.notifications.approval {
            for id in ids {
                let approval = self.mcp_expire(&self.store.approval(id).await?).await?;
                if approval.state == "pending"
                    && self.mcp_controller(&approval, false) == Some(target)
                {
                    notifications.push(json!({"kind":"approval","approval_id":id,"execution_id":approval.execution_id}));
                }
            }
        }
        if settings.notifications.authentication {
            for request in self.mcp_authentication_views(target).await? {
                if request["can_authenticate"] == true {
                    notifications.push(json!({"kind":"authentication","request_id":request["request_id"],"connection_id":request["connection_id"],"server_id":request["server_id"]}));
                }
            }
        }
        Ok(json!(notifications))
    }
    pub(in crate::server) async fn mcp_ui(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        if self.mcp_context(owner)?.kind != ClientKind::Tsp {
            return Err(denied("仅 TSP 工作台可查看审批"));
        }
        match method {
            "approval.list" => {
                let ids: Vec<_> = self
                    .state
                    .lock()
                    .unwrap()
                    .mcp
                    .approvals
                    .keys()
                    .copied()
                    .collect();
                let mut approvals = Vec::new();
                for id in ids {
                    let approval = self.mcp_expire(&self.store.approval(id).await?).await?;
                    if approval.state == "pending" {
                        approvals.push(
                            self.mcp_approval_view(owner, &approval, false, false)
                                .await?,
                        );
                    }
                }
                let authentication = self.mcp_authentication_views(owner).await?;
                Ok(json!({"approvals":approvals,"authentication":authentication}))
            }
            "approval.get" | "approval.decide" => {
                let approval: ApprovalRecord = self
                    .store
                    .approval(model::param(&params, "approval_id")?)
                    .await?;
                if method == "approval.get" {
                    let gate = self.mcp_gate(approval.session_id);
                    let _guard = gate.lock().await;
                    let approval = self
                        .mcp_expire(&self.store.approval(approval.id).await?)
                        .await?;
                    self.mcp_validate_approval(&approval).await?;
                    return self.mcp_approval_view(owner, &approval, true, false).await;
                }
                let decision: String = model::param(&params, "decision")?;
                if !matches!(decision.as_str(), "approve" | "reject") {
                    return Err(AppError::invalid("审批决定无效"));
                }
                self.mcp_decide(owner, approval, &decision, "tsp", true)
                    .await
            }
            _ => Err(AppError::new("method_not_found", "未知审批方法")),
        }
    }
    pub(in crate::server) async fn mcp_disconnected(&self, owner: Uuid) -> Result<()> {
        for approval in self.store.approvals_for_owner(owner).await? {
            if approval.state != "pending" {
                continue;
            }
            let cancelled = self
                .store
                .decide_approval(approval.id, owner, "cancel", "disconnect")
                .await?;
            if cancelled.state == "cancelled" {
                self.store
                    .update_task(
                        approval.execution_id,
                        "cancelled",
                        json!({"approval_id":approval.id}),
                        None,
                    )
                    .await?;
                self.mcp_emit_approval(&cancelled);
            }
        }
        Ok(())
    }
}
