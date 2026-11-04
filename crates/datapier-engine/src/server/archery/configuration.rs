//! 服务与连接配置的持久化操作。

use super::*;

impl Engine {
    pub(super) async fn save_archery_server(
        self: &Arc<Self>,
        owner: Uuid,
        params: Value,
    ) -> Result<Value> {
        let mut server: ArcheryServer = model::param(&params, "server")?;
        normalize_server(&mut server)?;
        let expected: u64 = model::param(&params, "expected_revision")?;
        if server.revision != expected {
            return Err(AppError::conflict("服务修订不一致"));
        }
        let password: CredentialUpdate =
            optional(&params, "password_update")?.unwrap_or(CredentialUpdate::Keep);
        let totp: CredentialUpdate =
            optional(&params, "totp_update")?.unwrap_or(CredentialUpdate::Keep);
        let entry = optional::<Uuid>(&params, "auth_id")?
            .map(|id| self.auth_entry(owner, id))
            .transpose()?;
        let engine = self.clone();
        let operation_id = Uuid::new_v4();
        let operation = self.register_operation(owner, operation_id, server.id, None, true)?;
        tokio::spawn(async move {
            let gate = engine.service_gate(server.id);
            let _guard = gate.lock().await;
            let _operation = operation;
            let old = match engine.store.archery_server(server.id).await {
                Ok(old) => Some(old),
                Err(e) if e.code == "not_found" && expected == 0 => None,
                Err(e) => return Err(e),
            };
            if let Some(old) = &old
                && (old.revision != expected || !same_identity(old, &server))
            {
                return Err(AppError::conflict("服务身份或修订已变化"));
            }
            let mut preview_server = server.clone();
            preview_server.revision = expected
                .checked_add(1)
                .ok_or_else(|| AppError::invalid("服务修订溢出"))?;
            let connection_count = engine
                .store
                .profiles()
                .await?
                .iter()
                .filter(|profile| {
                    profile
                        .archery()
                        .is_some_and(|connection| connection.server_id == server.id)
                })
                .count() as u64;
            budget(&datapier_core::model::ArcheryServerInfo {
                server: preview_server,
                credential_mode: datapier_core::model::CredentialMode::Unknown,
                totp_credential_mode: datapier_core::model::CredentialMode::Unknown,
                totp_configured: false,
                connection_count,
            })?;
            let changed = old
                .as_ref()
                .is_none_or(|old| !same_auth_config(old, &server))
                || !matches!(password, CredentialUpdate::Keep)
                || !matches!(totp, CredentialUpdate::Keep);
            if changed {
                let entry = entry
                    .as_ref()
                    .ok_or_else(|| AppError::new("auth_required", "请先验证候选认证配置"))?;
                if !entry.candidate
                    || entry.expected_revision != expected
                    || !same_auth_config(&entry.server.lock().unwrap(), &server)
                {
                    return Err(AppError::conflict("候选认证配置不一致"));
                }
                if let CredentialUpdate::Replace { value, .. } = &password
                    && entry.secret.lock().unwrap().as_ref() != Some(value)
                {
                    return Err(AppError::conflict("候选密码不一致"));
                }
                if let CredentialUpdate::Replace { value, .. } = &totp
                    && entry.totp.lock().unwrap().as_ref() != Some(value)
                {
                    return Err(AppError::conflict("候选 TOTP 配置不一致"));
                }
                if matches!(password, CredentialUpdate::Keep)
                    && entry.secret.lock().unwrap().is_some()
                {
                    return Err(AppError::conflict("候选密码更新意图不一致"));
                }
                if matches!(totp, CredentialUpdate::Keep) && entry.totp.lock().unwrap().is_some() {
                    return Err(AppError::conflict("候选 TOTP 更新意图不一致"));
                }
                Self::require_ready(entry).await?;
                engine
                    .service_exclusive(server.id, Some(entry.id), (owner, operation_id))
                    .await?;
            }
            let saved = engine
                .store
                .save_archery_server(&server, expected, password, totp)
                .await?;
            let shared = {
                let mut state = engine.state.lock().unwrap();
                if let Some(shared) = state.archery.shared.get(&server.id) {
                    shared.clone()
                } else {
                    let shared = Arc::new(SharedAuth {
                        auth: Arc::new(AuthSlot::new(saved.server.clone())?),
                        controller: Arc::new(Mutex::new(None)),
                    });
                    state.archery.shared.insert(server.id, shared.clone());
                    shared
                }
            };
            if changed {
                let entry = entry.as_ref().unwrap();
                shared
                    .auth
                    .publish_candidate(&entry.auth, &saved.server)
                    .await?;
                *entry.secret.lock().unwrap() = None;
                *entry.totp.lock().unwrap() = None;
            } else {
                shared.auth.rebind_revision(saved.server.revision).await?;
            }
            {
                let state = engine.state.lock().unwrap();
                for handle in state.archery.handles.values().filter(|entry| {
                    entry.server.lock().unwrap().id == server.id && !entry.candidate
                }) {
                    *handle.server.lock().unwrap() = saved.server.clone();
                }
            }
            if changed
                && let Some(entry) = &entry
                && entry.candidate
            {
                let replacement = Arc::new(AuthEntry {
                    id: entry.id,
                    owner,
                    server: Mutex::new(saved.server.clone()),
                    auth: shared.auth.clone(),
                    secret: Mutex::new(None),
                    totp: Mutex::new(None),
                    expected_revision: saved.server.revision,
                    candidate: false,
                    controller: shared.controller.clone(),
                    context_operation: Mutex::new(None),
                });
                engine
                    .state
                    .lock()
                    .unwrap()
                    .archery
                    .handles
                    .insert(entry.id, replacement.clone());
                engine
                    .state
                    .lock()
                    .unwrap()
                    .archery
                    .candidates
                    .insert((owner, server.id), replacement);
            }
            engine.emit_connections_changed();
            if changed {
                engine
                    .emit_shared_auth_changed(server.id, &shared.auth)
                    .await;
            }
            serialize(saved)
        })
        .await
        .map_err(|e| AppError::new("internal", e.to_string()))?
    }
    pub(super) async fn apply_archery_profiles(
        self: &Arc<Self>,
        owner: Uuid,
        params: Value,
    ) -> Result<Value> {
        let server_id: Uuid = model::param(&params, "server_id")?;
        let expected_revision: u64 = model::param(&params, "expected_server_revision")?;
        let expected: Vec<Profile> = model::param(&params, "expected_profiles")?;
        let desired: Vec<Profile> = model::param(&params, "desired_profiles")?;
        let entry = optional::<Uuid>(&params, "auth_id")?
            .map(|id| self.auth_entry(owner, id))
            .transpose()?;
        let mut added = Vec::new();
        let mut modified = Vec::new();
        let mut deleted = Vec::new();
        let mut remote = Vec::new();
        for profile in &desired {
            profile.validate()?;
            let connection = profile
                .archery()
                .ok_or_else(|| AppError::invalid("批量保存只接受 Archery 连接"))?;
            if connection.server_id != server_id {
                return Err(AppError::conflict("连接所属服务不一致"));
            }
            match expected.iter().find(|old| old.id == profile.id) {
                None => {
                    added.push(profile.id);
                    remote.push(profile.clone());
                }
                Some(old) => {
                    if serialize(old)? != serialize(profile)? {
                        modified.push(profile.id);
                    }
                    let old_connection = old
                        .archery()
                        .ok_or_else(|| AppError::invalid("连接来源不一致"))?;
                    if old_connection.databases != connection.databases
                        || old_connection.default_database != connection.default_database
                    {
                        remote.push(profile.clone());
                    }
                }
            }
        }
        for old in &expected {
            if !desired.iter().any(|profile| profile.id == old.id) {
                deleted.push(old.id);
            }
        }
        budget(&json!({"added":added,"modified":modified,"deleted":deleted,"profiles":desired}))?;
        let engine = self.clone();
        let operation = self.register_operation(owner, Uuid::new_v4(), server_id, None, true)?;
        tokio::spawn(async move {
            let gate = engine.service_gate(server_id);
            let guard = gate.lock().await;
            if engine.store.archery_server(server_id).await?.revision != expected_revision {
                return Err(AppError::conflict("服务修订已变化"));
            }
            if !remote.is_empty() {
                let entry = entry
                    .as_ref()
                    .ok_or_else(|| AppError::new("auth_required", "请先登录服务"))?;
                if entry.server.lock().unwrap().id != server_id || entry.candidate {
                    return Err(AppError::conflict("认证服务不一致"));
                }
                Self::require_ready(entry).await?;
            }
            drop(guard);
            if !remote.is_empty() {
                let entry = entry.as_ref().unwrap();
                let instances = entry.auth.instances(&operation.cancel).await?;
                for profile in &remote {
                    let connection = profile.archery().unwrap();
                    let instance = instances
                        .iter()
                        .find(|i| {
                            i.id == connection.instance_id
                                && i.name == connection.instance_name
                                && i.provider == profile.provider
                        })
                        .ok_or_else(|| {
                            AppError::new("target_changed", "实例名称、类型或 ID 已变化")
                        })?;
                    let databases = entry.auth.databases(instance, &operation.cancel).await?;
                    if connection
                        .databases
                        .iter()
                        .any(|db| !databases.contains(db))
                    {
                        return Err(AppError::new(
                            "target_not_selected",
                            "所选 DB 不在平台当前返回范围",
                        ));
                    }
                }
            }
            let _guard = gate.lock().await;
            let mut ids = added
                .iter()
                .chain(&modified)
                .chain(&deleted)
                .copied()
                .collect::<Vec<_>>();
            ids.sort_unstable();
            ids.dedup();
            let locks = ids
                .iter()
                .map(|id| engine.profile_lock(*id))
                .collect::<Vec<_>>();
            let mut guards = Vec::with_capacity(locks.len());
            for lock in &locks {
                guards.push(lock.lock().await);
            }
            if ids.iter().any(|id| engine.profile_occupied(*id)) {
                return Err(AppError::new("busy", "连接仍被使用"));
            }
            let result = engine
                .store
                .apply_archery_profiles(server_id, expected_revision, &expected, &desired)
                .await?;
            engine.emit_connections_changed();
            Ok(result)
        })
        .await
        .map_err(|e| AppError::new("internal", e.to_string()))?
    }
    pub(super) async fn delete_archery_server(
        self: &Arc<Self>,
        owner: Uuid,
        params: Value,
    ) -> Result<Value> {
        let server_id: Uuid = model::param(&params, "server_id")?;
        let expected: u64 = model::param(&params, "expected_revision")?;
        let mut ids: Vec<Uuid> = model::param(&params, "expected_profile_ids")?;
        ids.sort_unstable();
        if ids.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(AppError::invalid("连接 ID 重复"));
        }
        let engine = self.clone();
        let operation_id = Uuid::new_v4();
        let operation = self.register_operation(owner, operation_id, server_id, None, true)?;
        tokio::spawn(async move {
            let gate = engine.service_gate(server_id);
            let _guard = gate.lock().await;
            let _operation = operation;
            engine
                .service_exclusive(server_id, None, (owner, operation_id))
                .await?;
            if engine.store.archery_server(server_id).await?.revision != expected {
                return Err(AppError::conflict("服务修订已变化"));
            }
            let mut current_ids = engine
                .store
                .profiles()
                .await?
                .into_iter()
                .filter(|profile| {
                    profile
                        .archery()
                        .is_some_and(|connection| connection.server_id == server_id)
                })
                .map(|profile| profile.id)
                .collect::<Vec<_>>();
            current_ids.sort_unstable();
            if current_ids != ids {
                return Err(AppError::conflict("服务连接确认集合已变化"));
            }
            budget(&json!({"deleted":ids}))?;
            let locks = ids
                .iter()
                .map(|id| engine.profile_lock(*id))
                .collect::<Vec<_>>();
            let mut guards = Vec::with_capacity(locks.len());
            for lock in &locks {
                guards.push(lock.lock().await);
            }
            if ids.iter().any(|id| engine.profile_occupied(*id)) {
                return Err(AppError::new("busy", "连接仍被使用"));
            }
            let deleted = engine
                .store
                .delete_archery_server(server_id, expected, &ids)
                .await?;
            let shared = {
                let mut state = engine.state.lock().unwrap();
                state.archery.slots.retain(|(_, id), _| *id != server_id);
                state
                    .archery
                    .candidates
                    .retain(|(_, id), _| *id != server_id);
                state
                    .archery
                    .handles
                    .retain(|_, entry| entry.server.lock().unwrap().id != server_id);
                state.archery.shared.remove(&server_id)
            };
            if let Some(shared) = shared {
                shared.auth.close().await;
            }
            engine.emit_connections_changed();
            Ok(json!({"deleted":deleted}))
        })
        .await
        .map_err(|e| AppError::new("internal", e.to_string()))?
    }
}
