//! 同一 Engine 内迁移与编辑/写入的资源预约。
use super::*;
use datapier_core::model::transfer::PreparedTransfer;
use std::sync::Mutex;

#[derive(Clone)]
struct Scope {
    provider: model::ProviderKind,
    identity: Option<String>,
    database: Option<String>,
}
impl Scope {
    fn overlaps(&self, other: &Self) -> bool {
        self.provider == other.provider
            && (self.identity.is_none()
                || other.identity.is_none()
                || self.identity == other.identity)
            && (self.database.is_none()
                || other.database.is_none()
                || self.database == other.database)
    }
}
struct Reservation {
    transfer: bool,
    scopes: Vec<Scope>,
}
#[derive(Default)]
pub(super) struct OperationRegistry {
    entries: Mutex<HashMap<Uuid, Reservation>>,
    workspaces: Mutex<HashMap<(Uuid, String), OperationGuard>>,
    pub(super) workspace_gate: Arc<AsyncMutex<()>>,
}
pub(super) struct OperationGuard {
    registry: std::sync::Weak<OperationRegistry>,
    id: Uuid,
}
impl Drop for OperationGuard {
    fn drop(&mut self) {
        if let Some(registry) = self.registry.upgrade() {
            registry.entries.lock().unwrap().remove(&self.id);
        }
    }
}
impl OperationRegistry {
    fn reserve(self: &Arc<Self>, scopes: Vec<Scope>, transfer: bool) -> Result<OperationGuard> {
        let mut entries = self.entries.lock().unwrap();
        if entries.values().any(|entry| {
            (transfer || entry.transfer)
                && entry
                    .scopes
                    .iter()
                    .any(|a| scopes.iter().any(|b| a.overlaps(b)))
        }) {
            return Err(AppError::new(
                "busy",
                "目标数据库存在正在执行的迁移或编辑草稿",
            ));
        }
        let id = Uuid::new_v4();
        entries.insert(id, Reservation { transfer, scopes });
        Ok(OperationGuard {
            registry: Arc::downgrade(self),
            id,
        })
    }
    pub(super) fn remove_owner(&self, owner: Uuid) {
        self.workspaces
            .lock()
            .unwrap()
            .retain(|(id, _), _| *id != owner);
    }
    fn set_workspace(&self, owner: Uuid, workspace: String, guard: OperationGuard) {
        self.workspaces
            .lock()
            .unwrap()
            .insert((owner, workspace), guard);
    }
}
fn normalized(value: Option<&str>) -> Option<String> {
    value.filter(|s| !s.is_empty()).map(|s| s.to_lowercase())
}
fn target_database(
    provider: model::ProviderKind,
    target: Option<&Value>,
    db: &Database,
) -> Option<String> {
    if provider == model::ProviderKind::Redis {
        target
            .and_then(|v| v.get("database"))
            .and_then(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .or_else(|| v.as_u64().map(|n| n.to_string()))
            })
            .or_else(|| db.redis_db.map(|n| n.to_string()))
    } else {
        normalized(
            target
                .and_then(|v| v.get("database"))
                .and_then(Value::as_str),
        )
    }
}
impl Engine {
    pub(super) fn reserve_transfer_resources(
        &self,
        _owner: Uuid,
        plan: &PreparedTransfer,
        _reservation_id: Uuid,
    ) -> Result<OperationGuard> {
        let mut scopes = Vec::new();
        for (source, endpoint, identity) in [
            (true, &plan.spec.source, &plan.source_identity),
            (false, &plan.spec.destination, &plan.destination_identity),
        ] {
            if let Some(endpoint) = endpoint {
                let identity = identity
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| AppError::new("identity_unknown", "无法确定数据库实例身份"))?;
                let database = if plan.spec.provider == model::ProviderKind::Redis {
                    endpoint.redis_db.map(|n| n.to_string())
                } else {
                    normalized(endpoint.database.as_deref())
                };
                scopes.push(Scope {
                    provider: plan.spec.provider,
                    identity: Some(identity.clone()),
                    database,
                });
                if plan.spec.provider == model::ProviderKind::Mysql {
                    for item in &plan.items {
                        let target = if source { &item.source } else { &item.target };
                        if let Some(target) = target {
                            scopes.push(Scope {
                                provider: plan.spec.provider,
                                identity: Some(identity.clone()),
                                database: normalized(target.database.as_deref())
                                    .or_else(|| normalized(endpoint.database.as_deref())),
                            });
                        }
                    }
                    for object in &plan.spec.objects {
                        let target = if source {
                            &object.source
                        } else {
                            &object.target
                        };
                        scopes.push(Scope {
                            provider: plan.spec.provider,
                            identity: Some(identity.clone()),
                            database: normalized(target.database.as_deref())
                                .or_else(|| normalized(endpoint.database.as_deref())),
                        });
                    }
                    if !source {
                        for input in &plan.spec.inputs {
                            if let Some(target) = &input.target {
                                scopes.push(Scope {
                                    provider: plan.spec.provider,
                                    identity: Some(identity.clone()),
                                    database: normalized(target.database.as_deref())
                                        .or_else(|| normalized(endpoint.database.as_deref())),
                                });
                            }
                        }
                    }
                }
            }
        }
        self.state
            .lock()
            .unwrap()
            .operations
            .clone()
            .reserve(scopes, true)
    }
    pub(super) async fn operation_scope(
        &self,
        db: &Database,
        target: Option<&Value>,
    ) -> Result<OperationGuard> {
        self.operation_scope_inner(db, target, false).await
    }
    pub(super) async fn operation_scope_instance(&self, db: &Database) -> Result<OperationGuard> {
        self.operation_scope_inner(db, None, true).await
    }
    async fn operation_scope_inner(
        &self,
        db: &Database,
        target: Option<&Value>,
        instance: bool,
    ) -> Result<OperationGuard> {
        if db.profile.is_archery() {
            return self
                .state
                .lock()
                .unwrap()
                .operations
                .clone()
                .reserve(Vec::new(), false);
        }
        let session = db.session.lock().await;
        let identity = match &*session {
            ProviderSession::Mysql(mysql) => mysql.server_identity().await?,
            ProviderSession::Redis(redis) => redis.server_identity().await?,
            ProviderSession::Archery(_) => unreachable!(),
        };
        if identity.is_empty() {
            return Err(AppError::new("identity_unknown", "无法确定数据库实例身份"));
        }
        let database = if instance || db.profile.provider == model::ProviderKind::Redis {
            None
        } else {
            target_database(db.profile.provider, target, db)
        };
        let scope = Scope {
            provider: db.profile.provider,
            identity: Some(identity),
            database,
        };
        self.state
            .lock()
            .unwrap()
            .operations
            .clone()
            .reserve(vec![scope], false)
    }
    pub(super) async fn reserve_workspace_drafts(
        &self,
        owner: Uuid,
        snapshot: &Value,
    ) -> Result<OperationGuard> {
        let mut scopes = Vec::new();
        if let Some(tabs) = snapshot.get("tabs").and_then(Value::as_array) {
            for tab in tabs {
                let pending = tab.get("pending").unwrap_or(&Value::Null);
                let has_rows = ["rows", "redis", "edits"].iter().any(|name| {
                    pending
                        .get(*name)
                        .and_then(Value::as_array)
                        .is_some_and(|a| !a.is_empty())
                });
                let schema = tab.get("schema_draft").filter(|value| !value.is_null());
                let replacement = tab.get("replace_draft").filter(|value| !value.is_null());
                if !has_rows && schema.is_none() && replacement.is_none() {
                    continue;
                }
                let Some(profile) = optional::<Uuid>(tab, "profile_id")? else {
                    for provider in [model::ProviderKind::Mysql, model::ProviderKind::Redis] {
                        scopes.push(Scope {
                            provider,
                            identity: None,
                            database: None,
                        });
                    }
                    continue;
                };
                let db = {
                    self.state
                        .lock()
                        .unwrap()
                        .databases
                        .values()
                        .find(|db| db.owner == owner && db.profile.id == profile)
                        .cloned()
                };
                if let Some(db) = db {
                    if db.profile.is_archery() {
                        continue;
                    }
                    let session = db.session.lock().await;
                    let identity = match &*session {
                        ProviderSession::Mysql(mysql) => mysql.server_identity().await.ok(),
                        ProviderSession::Redis(redis) => redis.server_identity().await.ok(),
                        ProviderSession::Archery(_) => continue,
                    }
                    .filter(|id| !id.is_empty());
                    let Some(identity) = identity else {
                        scopes.push(Scope {
                            provider: db.profile.provider,
                            identity: None,
                            database: None,
                        });
                        continue;
                    };
                    let targets = [
                        has_rows.then(|| pending.get("target")).flatten(),
                        schema.and_then(|draft| draft.get("target")),
                        replacement.and_then(|draft| draft.pointer("/intent/target")),
                    ];
                    for target in targets.into_iter().flatten() {
                        let database = if db.profile.provider == model::ProviderKind::Redis {
                            None
                        } else {
                            target_database(db.profile.provider, Some(target), &db)
                        };
                        scopes.push(Scope {
                            provider: db.profile.provider,
                            identity: Some(identity.clone()),
                            database,
                        });
                    }
                    if replacement.is_some()
                        && replacement
                            .and_then(|draft| draft.pointer("/intent/target"))
                            .is_none()
                    {
                        scopes.push(Scope {
                            provider: db.profile.provider,
                            identity: Some(identity.clone()),
                            database: None,
                        });
                    }
                    if (has_rows && pending.get("target").is_none_or(Value::is_null))
                        || schema
                            .is_some_and(|draft| draft.get("target").is_none_or(Value::is_null))
                    {
                        scopes.push(Scope {
                            provider: db.profile.provider,
                            identity: Some(identity.clone()),
                            database: None,
                        });
                    }
                } else {
                    match self.store.profile(profile).await {
                        Ok(profile) if !profile.is_archery() => scopes.push(Scope {
                            provider: profile.provider,
                            identity: None,
                            database: None,
                        }),
                        Ok(_) => (),
                        Err(_) => {
                            for provider in [model::ProviderKind::Mysql, model::ProviderKind::Redis]
                            {
                                scopes.push(Scope {
                                    provider,
                                    identity: None,
                                    database: None,
                                });
                            }
                        }
                    }
                }
            }
        }
        self.state
            .lock()
            .unwrap()
            .operations
            .clone()
            .reserve(scopes, false)
    }
    pub(super) fn commit_workspace_drafts(&self, owner: Uuid, id: String, guard: OperationGuard) {
        let state = self.state.lock().unwrap();
        if state.clients.contains(&owner) {
            state.operations.set_workspace(owner, id, guard);
        }
    }
}
