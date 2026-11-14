//! 跨会话结构执行门、草稿范围与延迟令牌失效。
use super::*;
use std::sync::Weak;

#[derive(Default)]
pub(super) struct Coordination {
    gates: HashMap<Uuid, Weak<AsyncMutex<()>>>,
    owner_gates: HashMap<Uuid, Weak<AsyncMutex<()>>>,
    stale_sessions: HashSet<Uuid>,
    pub(super) stale_plans: HashSet<Uuid>,
    pub(super) plan_scopes: HashMap<Uuid, Value>,
    scopes: HashMap<(Uuid, Uuid, String, String), Scope>,
}

struct Scope {
    profile: Uuid,
    revision: u64,
    dirty: bool,
}

impl Engine {
    pub(in crate::server) fn validate_submit_target(
        target: &model::ResourceTarget,
        requested: Option<&Value>,
    ) -> Result<()> {
        if let Some(requested) = requested.filter(|value| !value.is_null()) {
            let requested: model::ResourceTarget = serde_json::from_value(requested.clone())?;
            if requested.name != target.name
                || requested
                    .database
                    .as_ref()
                    .is_some_and(|db| Some(db) != target.database.as_ref())
                || (!requested.kind.is_empty() && requested.kind != target.kind)
                || requested.key_b64.is_some()
            {
                return Err(AppError::invalid("请求目标与编辑令牌不匹配"));
            }
        }
        Ok(())
    }

    pub(in crate::server) fn workspace_owner_gate(&self, owner: Uuid) -> Arc<AsyncMutex<()>> {
        let mut state = self.state.lock().unwrap();
        let gates = &mut state.editing.coordination.owner_gates;
        gates.retain(|_, gate| gate.strong_count() != 0);
        if let Some(gate) = gates.get(&owner).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(AsyncMutex::new(()));
        gates.insert(owner, Arc::downgrade(&gate));
        gate
    }
    pub(in crate::server) fn editing_gate(&self, profile: Uuid) -> Arc<AsyncMutex<()>> {
        let mut state = self.state.lock().unwrap();
        let gates = &mut state.editing.coordination.gates;
        gates.retain(|_, gate| gate.strong_count() != 0);
        if let Some(gate) = gates.get(&profile).and_then(Weak::upgrade) {
            return gate;
        }
        let gate = Arc::new(AsyncMutex::new(()));
        gates.insert(profile, Arc::downgrade(&gate));
        gate
    }

    pub(in crate::server) async fn editing_draft_scope(
        &self,
        owner: Uuid,
        params: &Value,
    ) -> Result<Value> {
        self.ensure_editing_client(owner)?;
        let profile: Uuid = model::param(params, "profile_id")?;
        let session: Uuid = model::param(params, "session_id")?;
        let db = self.database(owner, &json!({"session_id":session}))?;
        if db.profile.id != profile {
            return Err(AppError::invalid("草稿连接与会话不匹配"));
        }
        let target: Value = model::param(params, "target")?;
        let database: String = model::param(&target, "database")?;
        let name: String = model::param(&target, "name")?;
        let dirty: bool = model::param(params, "dirty")?;
        let revision: u64 = model::param(params, "revision")?;
        let cancel = self.editing_cancel(owner)?;
        let gate = self.editing_gate(profile);
        let _guard = tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::cancelled()),
            guard = gate.lock() => guard,
        };
        if dirty {
            if self
                .state
                .lock()
                .unwrap()
                .editing
                .coordination
                .stale_sessions
                .contains(&session)
            {
                return Err(AppError::new(
                    "schema_conflict",
                    "编辑基线已失效，请重新读取并确认草稿",
                ));
            }
            let token: Uuid = model::param(params, "edit_token")?;
            let provider = tokio::select! {
                _ = cancel.cancelled() => return Err(AppError::cancelled()),
                provider = db.session.lock() => provider,
            };
            match &*provider {
                ProviderSession::Mysql(mysql) => {
                    mysql.validate_draft_scope(token, &database, &name)?
                }
                _ => return Err(AppError::invalid("草稿范围仅支持 MySQL 表和视图")),
            }
        }
        let mut state = self.state.lock().unwrap();
        let scopes = &mut state.editing.coordination.scopes;
        let key = (owner, session, database, name);
        if scopes.get(&key).is_some_and(|old| {
            revision < old.revision || (revision == old.revision && dirty != old.dirty)
        }) {
            return Err(AppError::new("stale_revision", "草稿范围版本已变化"));
        }
        if !scopes.contains_key(&key) && scopes.len() >= 4096 {
            return Err(AppError::new("resource_limit", "活动草稿范围超过上限"));
        }
        // 保留 clean 的版本墓碑，避免乱序旧消息重新标记 dirty。
        scopes.insert(
            key,
            Scope {
                profile,
                revision,
                dirty,
            },
        );
        Ok(json!({"revision":revision}))
    }

    pub(in crate::server) fn editing_forget_scopes(&self, owner: Uuid) {
        self.state
            .lock()
            .unwrap()
            .editing
            .coordination
            .scopes
            .retain(|(id, _, _, _), _| *id != owner);
    }

    pub(in crate::server) fn editing_check_dirty(
        &self,
        profile: Uuid,
        target: Option<&Value>,
    ) -> Result<()> {
        self.editing_check_dirty_except(profile, target, None)
    }

    pub(in crate::server) fn editing_check_dirty_except(
        &self,
        profile: Uuid,
        target: Option<&Value>,
        except_session: Option<Uuid>,
    ) -> Result<()> {
        let database = target
            .and_then(|t| t.get("database"))
            .and_then(Value::as_str);
        let name = target.and_then(|t| t.get("name")).and_then(Value::as_str);
        let state = self.state.lock().unwrap();
        if state
            .editing
            .coordination
            .scopes
            .iter()
            .any(|((_, session, db, object), scope)| {
                scope.dirty
                    && Some(*session) != except_session
                    && scope.profile == profile
                    && database.is_none_or(|v| v == db)
                    && name.is_none_or(|v| v == object)
            })
        {
            return Err(AppError::new("busy", "目标存在未提交的数据或值草稿"));
        }
        Ok(())
    }

    pub(in crate::server) fn editing_apply_invalidation(
        &self,
        session_id: Uuid,
        session: &mut ProviderSession,
    ) {
        if self
            .state
            .lock()
            .unwrap()
            .editing
            .coordination
            .stale_sessions
            .remove(&session_id)
        {
            session.invalidate_editing_metadata();
        }
    }

    pub(in crate::server) fn editing_prune_coordination(&self) {
        let mut state = self.state.lock().unwrap();
        let sessions: HashSet<_> = state.databases.keys().copied().collect();
        let plans: HashSet<_> = state
            .editing
            .schemas
            .keys()
            .chain(state.editing.replacements.keys())
            .copied()
            .collect();
        let coordination = &mut state.editing.coordination;
        coordination
            .stale_sessions
            .retain(|id| sessions.contains(id));
        coordination.stale_plans.retain(|id| plans.contains(id));
        coordination.plan_scopes.retain(|id, _| plans.contains(id));
        coordination
            .scopes
            .retain(|(_, session, _, _), _| sessions.contains(session));
    }
    pub(in crate::server) async fn editing_invalidate_data(&self, profile: Uuid) {
        let plans = {
            let mut state = self.state.lock().unwrap();
            let sessions: Vec<_> = state
                .databases
                .iter()
                .filter_map(|(id, db)| (db.profile.id == profile).then_some(*id))
                .collect();
            let plans: Vec<_> = state
                .editing
                .replacements
                .iter()
                .filter_map(|(id, p)| {
                    (p.profile_id == profile).then_some((*id, p.owner, p.revision))
                })
                .collect();
            state.editing.coordination.stale_sessions.extend(sessions);
            state
                .editing
                .coordination
                .stale_plans
                .extend(plans.iter().map(|(id, _, _)| *id));
            state.event_seq += 1;
            let id = Uuid::new_v4();
            let event = EngineEvent {
                seq: state.event_seq,
                kind: "schema.changed".into(),
                task_id: id,
                payload: json!({"id":id,"revision":state.event_seq,"state":"data_changed","scope":{"profile_id":profile}}),
            };
            for sender in state.event_senders.values() {
                let _ = sender.send(event.clone());
            }
            plans
        };
        for (id, owner, revision) in plans {
            self.emit_editing(owner, "edit_plan.changed", id, revision, "invalidated");
        }
        self.editing_invalidate_value_baselines(profile).await;
    }

    pub(in crate::server) fn editing_plan_current(&self, id: Uuid) -> Result<()> {
        if self
            .state
            .lock()
            .unwrap()
            .editing
            .coordination
            .stale_plans
            .contains(&id)
        {
            return Err(AppError::new(
                "schema_conflict",
                "结构已变化，请重新生成计划",
            ));
        }
        Ok(())
    }

    pub(in crate::server) async fn editing_invalidate_schema(
        &self,
        profile: Uuid,
        executing: Option<Uuid>,
        _scope: Option<&Value>,
    ) {
        let plans = {
            let mut state = self.state.lock().unwrap();
            let sessions: Vec<_> = state
                .databases
                .iter()
                .filter_map(|(id, db)| (db.profile.id == profile).then_some(*id))
                .collect();
            let plans: Vec<_> = state
                .editing
                .schemas
                .iter()
                .filter_map(|(id, p)| {
                    (p.profile_id == profile && Some(*id) != executing)
                        .then_some((*id, p.owner, p.revision))
                })
                .chain(state.editing.replacements.iter().filter_map(|(id, p)| {
                    (p.profile_id == profile).then_some((*id, p.owner, p.revision))
                }))
                .collect();
            state.editing.coordination.stale_sessions.extend(sessions);
            state
                .editing
                .coordination
                .stale_plans
                .extend(plans.iter().map(|(id, _, _)| *id));
            state.event_seq += 1;
            let id = executing.unwrap_or_else(Uuid::new_v4);
            // 视图依赖可能跨越目标对象；provider 令牌按 profile 全部失效。
            let event_scope = json!({"profile_id":profile});
            let event = EngineEvent {
                seq: state.event_seq,
                kind: "schema.changed".into(),
                task_id: id,
                payload: json!({"id":id,"revision":state.event_seq,"state":"invalidated","scope":event_scope}),
            };
            for sender in state.event_senders.values() {
                let _ = sender.send(event.clone());
            }
            plans
        };
        for (id, owner, revision) in plans {
            self.emit_editing(owner, "edit_plan.changed", id, revision, "invalidated");
        }
        self.editing_invalidate_value_baselines(profile).await;
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/server/coordination.rs"]
mod tests;
