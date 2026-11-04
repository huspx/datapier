//! 认证发起和控制权。

use super::*;

impl Engine {
    pub(super) async fn archery_auth_begin(
        self: &Arc<Self>,
        owner: Uuid,
        params: &Value,
    ) -> Result<Value> {
        let mut server: ArcheryServer = match optional(params, "server")? {
            Some(server) => server,
            None => {
                self.store
                    .archery_server(model::param(params, "server_id")?)
                    .await?
            }
        };
        normalize_server(&mut server)?;
        let expected: u64 = model::param(params, "expected_revision")?;
        if server.revision != expected {
            return Err(AppError::conflict("候选服务修订不一致"));
        }
        let supplied: Option<String> = optional(params, "password")?;
        let totp: Option<String> = optional(params, "totp_uri")?;
        if let Some(uri) = &totp {
            datapier_core::providers::archery::validate_totp_uri(uri)?;
        }
        let gate = self.service_gate(server.id);
        let guard = gate.lock().await;
        self.active(owner)?;
        let existing = {
            let state = self.state.lock().unwrap();
            state
                .archery
                .handles
                .values()
                .filter(|entry| entry.server.lock().unwrap().id == server.id)
                .cloned()
                .collect::<Vec<_>>()
        };
        for entry in existing {
            self.reconcile_challenge(&entry).await;
        }
        let saved = match self.store.archery_server(server.id).await {
            Ok(saved) => {
                if saved.revision != expected || !same_identity(&server, &saved) {
                    return Err(AppError::conflict("服务身份或修订已变化"));
                }
                Some(saved)
            }
            Err(error) if error.code == "not_found" && expected == 0 => None,
            Err(error) => return Err(error),
        };
        let requested = optional::<bool>(params, "candidate")?;
        let shared = {
            self.state
                .lock()
                .unwrap()
                .archery
                .shared
                .get(&server.id)
                .cloned()
        };
        let shared_ready = match shared {
            Some(shared) => shared.auth.status().await.state == "ready",
            None => false,
        };
        let has_secrets = supplied.is_some() || totp.is_some();
        let candidate = saved
            .as_ref()
            .is_none_or(|old| !same_auth_config(old, &server))
            || requested.unwrap_or(has_secrets)
            || (has_secrets && shared_ready);
        let operation_id = model::param(params, "operation_id")?;
        let operation = self.operation(owner, operation_id, server.id, None)?;
        let entry = {
            let mut state = self.state.lock().unwrap();
            let shared = if let Some(saved) = &saved {
                if let Some(shared) = state.archery.shared.get(&server.id) {
                    Some(shared.clone())
                } else {
                    let shared = Arc::new(SharedAuth {
                        auth: Arc::new(AuthSlot::new(saved.clone())?),
                        controller: Arc::new(Mutex::new(None)),
                    });
                    state.archery.shared.insert(server.id, shared.clone());
                    Some(shared)
                }
            } else {
                None
            };
            if candidate
                && shared.as_ref().is_some_and(|s| {
                    s.controller
                        .lock()
                        .unwrap()
                        .as_ref()
                        .is_some_and(|c| c.owner != owner)
                })
            {
                return Err(AppError::new("busy", "其他客户端正在认证"));
            }
            if (candidate || !shared_ready)
                && state
                    .archery
                    .candidates
                    .iter()
                    .any(|((client, id), entry)| {
                        *id == server.id
                            && *client != owner
                            && entry.candidate
                            && entry.controller.lock().unwrap().is_some()
                    })
            {
                return Err(AppError::new("busy", "其他客户端正在认证"));
            }
            if (candidate || has_secrets)
                && let Some(shared) = &shared
            {
                let mut control = shared.controller.lock().unwrap();
                if control.as_ref().is_some_and(|c| c.owner == owner) {
                    let still_running =
                        state.archery.operations.iter().any(|((client, id), op)| {
                            *client == owner
                                && *id != operation_id
                                && op.server_id == Some(server.id)
                        });
                    if still_running {
                        return Err(AppError::new("busy", "认证请求仍在进行"));
                    }
                    *control = None;
                }
            }
            if candidate
                && let Some(previous) = state.archery.candidates.get(&(owner, server.id)).cloned()
                && previous.candidate
                && previous.controller.lock().unwrap().is_some()
            {
                let running = state.archery.operations.iter().any(|((client, id), op)| {
                    *client == owner && *id != operation_id && op.server_id == Some(server.id)
                });
                if running {
                    return Err(AppError::new("busy", "认证请求仍在进行"));
                }
                *previous.controller.lock().unwrap() = None;
                state.archery.handles.remove(&previous.id);
            }
            if !candidate
                && !has_secrets
                && let Some(entry) = state.archery.slots.get(&(owner, server.id))
            {
                entry.clone()
            } else {
                let (auth, controller) = if candidate {
                    (
                        Arc::new(AuthSlot::new(server.clone())?),
                        Arc::new(Mutex::new(None)),
                    )
                } else {
                    let shared = shared.unwrap();
                    (shared.auth.clone(), shared.controller.clone())
                };
                let entry = Arc::new(AuthEntry {
                    id: Uuid::new_v4(),
                    owner,
                    server: Mutex::new(server.clone()),
                    auth,
                    secret: Mutex::new(if candidate { supplied.clone() } else { None }),
                    totp: Mutex::new(if candidate { totp.clone() } else { None }),
                    expected_revision: expected,
                    candidate,
                    controller,
                    context_operation: Mutex::new(None),
                });
                state.archery.handles.insert(entry.id, entry.clone());
                if candidate {
                    state
                        .archery
                        .candidates
                        .insert((owner, server.id), entry.clone());
                } else {
                    state
                        .archery
                        .slots
                        .insert((owner, server.id), entry.clone());
                }
                entry
            }
        };
        let status = entry.auth.status().await;
        if status.state == "ready" && !candidate {
            *entry.context_operation.lock().unwrap() = None;
            drop(guard);
            let response = Self::auth_response(&entry).await?;
            model::ensure_not_cancelled(&operation.cancel)?;
            return Ok(response);
        }
        let credential_token = {
            let mut control = entry.controller.lock().unwrap();
            if control.is_some() {
                if supplied.is_some() || totp.is_some() {
                    return Err(AppError::new("busy", "其他客户端正在认证"));
                }
                None
            } else {
                let token = Uuid::new_v4();
                *control = Some(Controller {
                    owner,
                    auth_id: entry.id,
                    token,
                    generation: status.generation,
                    challenge: false,
                    verify_operation: None,
                });
                Some(token)
            }
        };
        let Some(credential_token) = credential_token else {
            drop(guard);
            let response = Self::auth_response(&entry).await?;
            model::ensure_not_cancelled(&operation.cancel)?;
            return Ok(response);
        };
        *entry.context_operation.lock().unwrap() = Some(operation_id);
        drop(guard);
        let password = match supplied {
            Some(password) => Some(password),
            None => {
                let read = async {
                    #[cfg(test)]
                    if FAIL_CREDENTIAL_READ.lock().unwrap().contains(&server.id) {
                        return Err(AppError::new("credentials", "凭据读取失败"));
                    }
                    self.store.archery_secret(server.id).await
                };
                let result = tokio::select! { biased; _ = operation.cancel.cancelled() => Err(AppError::cancelled()), result = read => result };
                match result {
                    Ok(secret) => secret,
                    Err(error) => {
                        let _guard = gate.lock().await;
                        // 只释放本次读取凭据前取得的控制权，避免迟到结果清除新一轮认证。
                        {
                            let mut context = entry.context_operation.lock().unwrap();
                            let mut control = entry.controller.lock().unwrap();
                            if *context == Some(operation_id)
                                && control.as_ref().is_some_and(|c| {
                                    c.owner == owner
                                        && c.auth_id == entry.id
                                        && c.token == credential_token
                                })
                            {
                                *context = None;
                                *control = None;
                            }
                        }
                        self.emit_auth_changed(&entry).await;
                        return Err(error);
                    }
                }
            }
        };
        let Some(password) = password else {
            return Err(AppError::new("auth_required", "请输入 Archery 密码")
                .with_details(Self::auth_response(&entry).await?));
        };
        let begin_token = {
            let mut controller = entry.controller.lock().unwrap();
            let controller = controller
                .as_mut()
                .filter(|c| {
                    c.owner == owner && c.auth_id == entry.id && c.token == credential_token
                })
                .ok_or_else(AppError::cancelled)?;
            controller.verify_operation = Some(operation_id);
            controller.token
        };
        let _verify_guard = VerifyGuard {
            controller: entry.controller.clone(),
            token: begin_token,
            operation_id,
        };
        if !entry.candidate {
            self.emit_auth_state(server.id, "authenticating", status.generation);
        }
        let result = entry.auth.authenticate(password, &operation.cancel).await;
        if let Err(error) = result {
            let _guard = gate.lock().await;
            let current = {
                let mut controller = entry.controller.lock().unwrap();
                if controller.as_ref().is_some_and(|c| {
                    c.token == begin_token && c.verify_operation == Some(operation_id)
                }) {
                    *controller = None;
                    true
                } else {
                    false
                }
            };
            if !current {
                return Err(AppError::cancelled());
            }
            {
                let mut context = entry.context_operation.lock().unwrap();
                if *context == Some(operation_id) {
                    *context = None;
                }
            }
            self.emit_auth_changed(&entry).await;
            return Err(error);
        }
        if entry.auth.status().await.state == "challenge_required"
            && server.totp_mode == TotpMode::Automatic
        {
            let uri = if totp.is_some() {
                totp
            } else {
                tokio::select! { biased; _ = operation.cancel.cancelled() => return Err(AppError::cancelled()), result = self.store.archery_totp_secret(server.id) => result.ok().flatten() }
            };
            if let Some(uri) = uri
                && let Ok(code) = generate_totp(&uri)
            {
                let _ = entry.auth.verify(code, &operation.cancel).await;
            }
        }
        let _guard = gate.lock().await;
        model::ensure_not_cancelled(&operation.cancel)?;
        if !entry
            .controller
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|c| c.token == begin_token && c.verify_operation == Some(operation_id))
            || *entry.context_operation.lock().unwrap() != Some(operation_id)
        {
            return Err(AppError::cancelled());
        }
        let final_status = entry.auth.status().await;
        let release = {
            let mut controller = entry.controller.lock().unwrap();
            if let Some(current) = controller.as_mut().filter(|c| c.token == begin_token) {
                if final_status.state == "ready" || final_status.state == "auth_required" {
                    *controller = None;
                    true
                } else {
                    current.challenge = final_status.state == "challenge_required";
                    false
                }
            } else {
                false
            }
        };
        if release {
            *entry.context_operation.lock().unwrap() = None;
        }
        drop(_verify_guard);
        model::ensure_not_cancelled(&operation.cancel)?;
        self.emit_auth_changed(&entry).await;
        let response = Self::auth_response(&entry).await?;
        model::ensure_not_cancelled(&operation.cancel)?;
        Ok(response)
    }
}
