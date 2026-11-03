//! Archery RPC 分派与配置操作。

use super::*;

impl Engine {
    pub(in crate::server) async fn archery_rpc(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        match method {
            "archery.server.list" => {
                let result = json!({"servers":self.store.archery_servers().await?});
                budget(&result)?;
                Ok(result)
            }
            "archery.auth.begin" => self.archery_auth_begin(owner, &params).await,
            "archery.auth.status" => {
                let entry = self.auth_entry(owner, model::param(&params, "auth_id")?)?;
                let server_id = entry.server.lock().unwrap().id;
                let gate = self.service_gate(server_id);
                let _guard = gate.lock().await;
                self.reconcile_challenge(&entry).await;
                Self::auth_response(&entry).await
            }
            "archery.operation.cancel" => {
                let id = model::param(&params, "operation_id")?;
                let cancelled_entry = {
                    let mut state = self.state.lock().unwrap();
                    if state
                        .archery
                        .operations
                        .get(&(owner, id))
                        .is_some_and(|op| op.persistent)
                    {
                        return Err(AppError::new("busy", "持久配置保存不能中断"));
                    }
                    let mut matched_auth = false;
                    let mut cancelled_entry = None;
                    for entry in state
                        .archery
                        .handles
                        .values()
                        .filter(|entry| entry.owner == owner)
                    {
                        let mut current = entry.context_operation.lock().unwrap();
                        if *current == Some(id) {
                            *current = None;
                            let mut control = entry.controller.lock().unwrap();
                            if control.as_ref().is_some_and(|c| c.owner == owner) {
                                *control = None;
                                cancelled_entry = Some(entry.clone());
                            }
                            matched_auth = true;
                        }
                    }
                    if let Some(operation) = state.archery.operations.get(&(owner, id)) {
                        operation.cancel.cancel();
                    } else if !matched_auth {
                        if state
                            .archery
                            .operations
                            .keys()
                            .filter(|(client, _)| *client == owner)
                            .count()
                            >= 128
                        {
                            return Err(AppError::new("rate_limited", "待处理取消过多"));
                        }
                        let cancel = CancellationToken::new();
                        cancel.cancel();
                        state.archery.operations.insert(
                            (owner, id),
                            ShortOperation {
                                server_id: None,
                                profile_id: None,
                                cancel,
                                persistent: false,
                            },
                        );
                    }
                    cancelled_entry
                };
                if let Some(entry) = cancelled_entry {
                    let server_id = entry.server.lock().unwrap().id;
                    let gate = self.service_gate(server_id);
                    let _guard = gate.lock().await;
                    let idle = entry.controller.lock().unwrap().is_none();
                    if idle {
                        entry.auth.cancel_authentication().await;
                        self.emit_auth_changed(&entry).await;
                    }
                }
                Ok(json!({"cancelled":true}))
            }
            "archery.auth.verify" | "archery.instances" | "archery.databases" => {
                let entry = self.auth_entry(owner, model::param(&params, "auth_id")?)?;
                let server_id = entry.server.lock().unwrap().id;
                let gate = self.service_gate(server_id);
                let guard = gate.lock().await;
                let operation = self.operation(
                    owner,
                    model::param(&params, "operation_id")?,
                    server_id,
                    None,
                )?;
                let verify = if method == "archery.auth.verify" {
                    let token: Uuid = model::param(&params, "controller_token")?;
                    let generation: u64 = model::param(&params, "auth_generation")?;
                    let otp: String = model::param(&params, "otp")?;
                    self.reconcile_challenge(&entry).await;
                    {
                        let mut controller = entry.controller.lock().unwrap();
                        let current = controller
                            .as_mut()
                            .filter(|c| {
                                c.owner == owner
                                    && c.auth_id == entry.id
                                    && c.token == token
                                    && c.generation == generation
                            })
                            .ok_or_else(|| AppError::new("auth_required", "认证控制权已失效"))?;
                        if current.verify_operation.is_some() {
                            return Err(AppError::new("busy", "验证码验证正在进行"));
                        }
                        current.verify_operation = Some(operation.id);
                    }
                    *entry.context_operation.lock().unwrap() = Some(operation.id);
                    Some((
                        token,
                        generation,
                        otp,
                        VerifyGuard {
                            controller: entry.controller.clone(),
                            token,
                            operation_id: operation.id,
                        },
                    ))
                } else {
                    None
                };
                drop(guard);
                if let Some((token, generation, otp, _verify_guard)) = verify {
                    let result = entry.auth.verify(otp, &operation.cancel).await;
                    let status = entry.auth.status().await;
                    let _guard = gate.lock().await;
                    let still_current = {
                        let mut controller = entry.controller.lock().unwrap();
                        if let Some(current) = controller.as_mut().filter(|c| {
                            c.owner == owner
                                && c.auth_id == entry.id
                                && c.token == token
                                && c.generation == generation
                                && c.verify_operation == Some(operation.id)
                        }) {
                            if status.state != "challenge_required" {
                                *controller = None;
                            } else {
                                current.challenge = true;
                            }
                            true
                        } else {
                            false
                        }
                    };
                    let context_matches =
                        *entry.context_operation.lock().unwrap() == Some(operation.id);
                    if !still_current || !context_matches || operation.cancel.is_cancelled() {
                        return Err(AppError::cancelled());
                    }
                    if status.state != "challenge_required" {
                        *entry.context_operation.lock().unwrap() = None;
                    }
                    drop(_verify_guard);
                    self.emit_auth_changed(&entry).await;
                    result?;
                    let response = Self::auth_response(&entry).await?;
                    model::ensure_not_cancelled(&operation.cancel)?;
                    Ok(response)
                } else {
                    Self::require_ready(&entry).await?;
                    let instances = entry.auth.instances(&operation.cancel).await?;
                    if method == "archery.instances" {
                        Ok(json!({"instances":instances}))
                    } else {
                        let id: i64 = model::param(&params, "instance_id")?;
                        let instance = instances
                            .iter()
                            .find(|instance| instance.id == id)
                            .ok_or_else(|| {
                                AppError::new("target_changed", "所选实例已不在当前账号范围")
                            })?;
                        let databases = entry.auth.databases(instance, &operation.cancel).await?;
                        Ok(json!({"databases":databases,"discovery":"platform_reported"}))
                    }
                }
            }
            "archery.auth.close" => {
                let entry = self.auth_entry(owner, model::param(&params, "auth_id")?)?;
                let server_id = entry.server.lock().unwrap().id;
                let gate = self.service_gate(server_id);
                let _guard = gate.lock().await;
                let control = entry.controller.lock().unwrap().clone();
                if let Some(control) = control.filter(|c| c.owner == owner && c.auth_id == entry.id)
                {
                    let token: Uuid = model::param(&params, "controller_token")?;
                    let generation: u64 = model::param(&params, "auth_generation")?;
                    if token != control.token || generation != control.generation {
                        return Err(AppError::new("auth_required", "认证控制权已失效"));
                    }
                    *entry.controller.lock().unwrap() = None;
                    let context_operation = *entry.context_operation.lock().unwrap();
                    if let Some(id) = context_operation
                        && let Some(op) = self
                            .state
                            .lock()
                            .unwrap()
                            .archery
                            .operations
                            .get(&(owner, id))
                    {
                        op.cancel.cancel();
                    }
                    entry.auth.cancel_authentication().await;
                }
                {
                    let mut state = self.state.lock().unwrap();
                    state
                        .archery
                        .slots
                        .retain(|_, handle| handle.id != entry.id);
                    state
                        .archery
                        .candidates
                        .retain(|_, handle| handle.id != entry.id);
                    state.archery.handles.remove(&entry.id);
                }
                self.emit_auth_changed(&entry).await;
                Ok(json!({"closed":true}))
            }
            "archery.server.save" => self.save_archery_server(owner, params).await,
            "archery.profile.apply_batch" => self.apply_archery_profiles(owner, params).await,
            "archery.server.delete" => self.delete_archery_server(owner, params).await,
            "archery.query_history" => {
                let db = self.database(owner, &params)?;
                let connection = db.profile.archery().ok_or_else(|| {
                    AppError::new("capability_unavailable", "仅 Archery 有平台历史")
                })?;
                let entry = self.ready_entry(owner, connection.server_id).await?;
                let gate = self.service_gate(connection.server_id);
                let guard = gate.lock().await;
                let operation = self.operation(
                    owner,
                    model::param(&params, "operation_id")?,
                    connection.server_id,
                    Some(db.profile.id),
                )?;
                drop(guard);
                let offset = optional(&params, "offset")?.unwrap_or(0);
                let limit = optional::<u64>(&params, "limit")?
                    .unwrap_or(20)
                    .clamp(1, 100);
                let result = entry.auth.query_history(offset, limit,
                    &optional::<String>(&params, "search")?.unwrap_or_default(), &operation.cancel).await
                    .map(|value| json!({"rows":value["rows"],"total":value["total"],"offset":offset,"limit":limit}));
                db.capabilities.lock().unwrap().observe(method, &result);
                if let Ok(value) = &result {
                    budget(value)?;
                }
                result
            }
            "archery.sql_check" => self.start(owner, method, params, true).await,
            _ => Err(AppError::new("method_not_found", "未知 Archery 方法")),
        }
    }
}
