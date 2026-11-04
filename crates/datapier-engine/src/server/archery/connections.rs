//! Archery 连接注册、断开与错误上下文。

use super::*;

impl Engine {
    pub(super) async fn ready_entry(&self, owner: Uuid, server_id: Uuid) -> Result<Arc<AuthEntry>> {
        let server = self.store.archery_server(server_id).await?;
        let entry = {
            let mut state = self.state.lock().unwrap();
            if !state.clients.contains(&owner) {
                return Err(AppError::new("disconnected", "客户端已关闭"));
            }
            if let Some(entry) = state
                .archery
                .slots
                .get(&(owner, server_id))
                .filter(|entry| !entry.candidate)
            {
                entry.clone()
            } else {
                let shared = if let Some(shared) = state.archery.shared.get(&server_id) {
                    shared.clone()
                } else {
                    let shared = Arc::new(SharedAuth {
                        auth: Arc::new(AuthSlot::new(server.clone())?),
                        controller: Arc::new(Mutex::new(None)),
                    });
                    state.archery.shared.insert(server_id, shared.clone());
                    shared
                };
                let entry = Arc::new(AuthEntry {
                    id: Uuid::new_v4(),
                    owner,
                    server: Mutex::new(server.clone()),
                    auth: shared.auth.clone(),
                    secret: Mutex::new(None),
                    totp: Mutex::new(None),
                    expected_revision: server.revision,
                    candidate: false,
                    controller: shared.controller.clone(),
                    context_operation: Mutex::new(None),
                });
                state.archery.handles.insert(entry.id, entry.clone());
                state
                    .archery
                    .slots
                    .insert((owner, server_id), entry.clone());
                entry
            }
        };
        Self::require_ready(&entry).await?;
        Ok(entry)
    }
    pub(in crate::server) async fn open_archery(
        self: &Arc<Self>,
        owner: Uuid,
        params: &Value,
        id: Uuid,
    ) -> Result<Value> {
        let candidate = self
            .store
            .profiles()
            .await?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| AppError::new("not_found", "连接不存在"))?;
        let server_id = candidate
            .archery()
            .ok_or_else(|| AppError::invalid("连接来源已变化"))?
            .server_id;
        let gate = self.service_gate(server_id);
        let guard = gate.lock().await;
        let lock = self.profile_lock(id);
        let profile_guard = lock.lock().await;
        let profile = self
            .store
            .profiles()
            .await?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| AppError::new("not_found", "连接不存在"))?;
        let connection = profile
            .archery()
            .ok_or_else(|| AppError::new("target_changed", "连接来源已变化"))?;
        let entry = self.ready_entry(owner, server_id).await?;
        let revision = self.store.archery_server(server_id).await?.revision;
        if entry.server.lock().unwrap().revision != revision {
            return Err(AppError::new("auth_required", "服务修订已变化"));
        }
        let database = if profile.provider == ProviderKind::Redis {
            optional::<u32>(params, "redis_db")?
                .map(|db| db.to_string())
                .unwrap_or_else(|| connection.default_database.clone())
        } else {
            optional::<String>(params, "database")?
                .unwrap_or_else(|| connection.default_database.clone())
        };
        if !connection.databases.contains(&database) {
            return Err(AppError::new("target_not_selected", "目标 DB 不在所选范围"));
        }
        let operation = self.operation(owner, Uuid::new_v4(), server_id, Some(id))?;
        drop(profile_guard);
        drop(guard);
        let session = ProviderSession::connect_archery(
            profile.clone(),
            entry.auth.clone(),
            Some(database.clone()),
            &operation.cancel,
        )
        .await?;
        let _guard = gate.lock().await;
        let _profile_guard = lock.lock().await;
        self.active(owner)?;
        let redis_db = if profile.provider == ProviderKind::Redis {
            Some(
                database
                    .parse::<u32>()
                    .map_err(|_| AppError::invalid("Redis DB 无效"))?,
            )
        } else {
            None
        };
        let session_id = Uuid::new_v4();
        let db = Arc::new(Database {
            owner,
            profile: profile.clone(),
            database: Some(database.clone()),
            redis_db,
            capabilities: Mutex::new(profile.capabilities()),
            session: AsyncMutex::new(session),
            closing: AtomicBool::new(false),
        });
        {
            let mut state = self.state.lock().unwrap();
            if !state.clients.contains(&owner) {
                return Err(AppError::new("disconnected", "客户端已离开"));
            }
            state.databases.insert(session_id, db);
        }
        Ok(
            json!({"session_id":session_id,"profile":profile,"source":"archery","database":database,"redis_db":redis_db,"query_semantics":"stateless","capabilities":profile.capabilities()}),
        )
    }
    pub(in crate::server) async fn disconnect_archery(&self, owner: Uuid) {
        let entries = {
            let mut state = self.state.lock().unwrap();
            state.archery.operations.retain(|(client, _), operation| {
                if *client == owner && !operation.persistent {
                    operation.cancel.cancel();
                    false
                } else {
                    true
                }
            });
            state
                .archery
                .slots
                .retain(|(client, _), _| *client != owner);
            state
                .archery
                .candidates
                .retain(|(client, _), _| *client != owner);
            let ids = state
                .archery
                .handles
                .iter()
                .filter(|(_, entry)| entry.owner == owner)
                .map(|(id, _)| *id)
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| state.archery.handles.remove(&id))
                .collect::<Vec<_>>()
        };
        for entry in entries {
            let was_controller = {
                let mut controller = entry.controller.lock().unwrap();
                if controller
                    .as_ref()
                    .is_some_and(|control| control.owner == owner)
                {
                    *controller = None;
                    true
                } else {
                    false
                }
            };
            if was_controller {
                let server_id = entry.server.lock().unwrap().id;
                let gate = self.service_gate(server_id);
                let _guard = gate.lock().await;
                let idle = entry.controller.lock().unwrap().is_none();
                if idle {
                    entry.auth.cancel_authentication().await;
                    self.emit_auth_changed(&entry).await;
                }
            }
        }
    }
    pub(in crate::server) async fn archery_error(
        &self,
        db: &Database,
        mut error: AppError,
    ) -> AppError {
        if error.code == "auth_required"
            && let Some(connection) = db.profile.archery()
        {
            let entry = {
                self.state
                    .lock()
                    .unwrap()
                    .archery
                    .slots
                    .get(&(db.owner, connection.server_id))
                    .cloned()
            };
            if let Some(entry) = entry {
                if !error.details.is_object() {
                    error.details = Box::new(json!({}));
                }
                error.details["auth_id"] = json!(entry.id);
                error.details["server_id"] = json!(connection.server_id);
                self.emit_auth_changed(&entry).await;
            }
        }
        error
    }
    pub(in crate::server) async fn test_archery_profile(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        profile: Profile,
        params: &Value,
    ) -> Result<Value> {
        let connection = profile
            .archery()
            .ok_or_else(|| AppError::invalid("需要 Archery 来源"))?;
        let server_id = connection.server_id;
        let gate = self.service_gate(server_id);
        let guard = gate.lock().await;
        let entry = self.ready_entry(owner, server_id).await?;
        let operation = self.operation(owner, Uuid::new_v4(), server_id, Some(profile.id))?;
        drop(guard);
        let mut session = ProviderSession::connect_archery(
            profile.clone(),
            entry.auth.clone(),
            None,
            &operation.cancel,
        )
        .await?;
        let task = self.store.create_task(owner,workspace(params),method,json!({"_task_context":{
            "connection":profile.name,"provider":profile.provider,"source":"archery","server_id":server_id,
            "instance_id":connection.instance_id,"database":profile.default_database(),"redis_db":if profile.provider==ProviderKind::Redis {Some(profile.redis_db())}else{None},"targets":[]
        }}),false).await?;
        let sink = ExecutionSink::new(self.store.clone(), task.id);
        let started = Instant::now();
        let result = session
            .call(
                if method == "profile.test" {
                    "connection.info"
                } else {
                    "resource.list"
                },
                json!({}),
                &sink,
                &operation.cancel,
            )
            .await;
        let _ = session.close().await;
        self.finish_task(owner, task.id, &result, Some(started))
            .await;
        if method == "profile.test" {
            Ok(json!({"ok":true,"info":result?}))
        } else {
            result
        }
    }
}
