//! 高级编辑的运行期所有权；用户新值可持久化，数据库原值和计划基线只留在内存。
use super::*;
use datapier_core::model::schema::SchemaPlan;
use std::sync::Arc;

mod changes;
mod coordination;
mod drafts;
mod files;
mod lifecycle;
mod replace;
mod schema;
mod values;

#[derive(Default)]
pub(super) struct EditingState {
    schemas: HashMap<Uuid, SchemaDraft>,
    values: values::ValueState,
    files: files::FileState,
    replacements: HashMap<Uuid, ReplaceBinding>,
    coordination: coordination::Coordination,
}
impl EditingState {
    pub(in crate::server) fn workspace_busy(&self, owner: Uuid) -> bool {
        self.values.has_owner(owner) || self.files.has_session(owner)
    }

    pub(in crate::server) fn invalidate_owner_plans(&mut self, owner: Uuid) {
        let ids: Vec<_> = self
            .schemas
            .iter()
            .filter_map(|(id, plan)| (plan.owner == owner).then_some(*id))
            .chain(
                self.replacements
                    .iter()
                    .filter_map(|(id, plan)| (plan.owner == owner).then_some(*id)),
            )
            .collect();
        self.coordination.stale_plans.extend(ids);
    }
}

struct SchemaDraft {
    owner: Uuid,
    session_id: Uuid,
    profile_id: Uuid,
    profile_revision: u64,
    workspace: String,
    revision: u64,
    source: &'static str,
    writable: bool,
    plan: Arc<SchemaPlan>,
    gate: Arc<AsyncMutex<()>>,
    execution: Option<Uuid>,
    touched: Instant,
}

struct ReplaceBinding {
    owner: Uuid,
    session_id: Uuid,
    profile_id: Uuid,
    profile_revision: u64,
    budget_handle: Uuid,
    revision: u64,
    target: Value,
    gate: Arc<AsyncMutex<()>>,
    execution: Option<Uuid>,
    touched: Instant,
}

/// 外部编辑文件只接收经过基线与所有权检查的单值原字节。
pub(super) struct ValueFileExport {
    pub profile_id: Uuid,
    pub workspace_id: String,
    pub target: Value,
    pub bytes: Vec<u8>,
    pub baseline_digest: String,
}

impl Engine {
    pub(super) fn ensure_editing_client(&self, owner: Uuid) -> Result<()> {
        self.active(owner)?;
        let state = self.state.lock().unwrap();
        if state
            .contexts
            .get(&owner)
            .is_some_and(|context| context.kind == model::ClientKind::Tsp)
        {
            Ok(())
        } else {
            Err(AppError::new(
                "permission_denied",
                "高级编辑只允许原生工作台客户端",
            ))
        }
    }

    pub(super) async fn editing_rpc(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        // helper 只领取一次性交接和报告终态，不能借同一 IPC 通道访问数据库。
        if matches!(method, "value.editor.claim" | "value.editor.finish") {
            self.active(owner)?;
            return self.file_rpc_with_events(owner, method, params).await;
        }
        self.ensure_editing_client(owner)?;
        let owner_gate = self.workspace_owner_gate(owner);
        let _owner_guard = owner_gate.lock().await;
        match method {
            "change.preview" => self.preview_changes(owner, params).await,
            name if name.starts_with("schema.") => self.schema_rpc(owner, method, params).await,
            name if name.starts_with("replace.") => self.replace_rpc(owner, method, params).await,
            name if name.starts_with("value.files.")
                || name.starts_with("value.file.")
                || name.starts_with("value.external.") =>
            {
                self.file_rpc_with_events(owner, method, params).await
            }
            name if name.starts_with("value.") => self.value_rpc(owner, method, params).await,
            _ => Err(AppError::new("method_not_found", "未知高级编辑操作")),
        }
    }

    fn editing_cancel(&self, owner: Uuid) -> Result<CancellationToken> {
        self.state
            .lock()
            .unwrap()
            .client_cancels
            .get(&owner)
            .cloned()
            .ok_or_else(|| AppError::new("disconnected", "客户端会话已关闭"))
    }

    async fn editing_profile(&self, owner: Uuid, session_id: Uuid) -> Result<Arc<Database>> {
        self.ensure_editing_client(owner)?;
        let db = self.database(owner, &json!({"session_id":session_id}))?;
        let current = self.store.profile(db.profile.id).await?;
        if current.revision != db.profile.revision {
            return Err(AppError::new(
                "schema_conflict",
                "连接设置已变化，请重新连接并确认编辑基线",
            ));
        }
        Ok(db)
    }

    pub(super) fn emit_editing(
        &self,
        owner: Uuid,
        kind: &str,
        id: Uuid,
        revision: u64,
        status: &str,
    ) {
        let mut state = self.state.lock().unwrap();
        let scope = state.editing.schemas.get(&id).map(|plan| {
            json!({"profile_id":plan.profile_id,"database":plan.plan.intent.target.database,"name":plan.plan.intent.target.name})
        }).or_else(|| state.editing.replacements.get(&id).map(|plan| {
            let mut scope = json!({"profile_id":plan.profile_id});
            for key in ["database", "name"] {
                if let Some(value) = plan.target.get(key) { scope[key] = value.clone(); }
            }
            scope
        }));
        let scope = if let Some(scope) = scope {
            state
                .editing
                .coordination
                .plan_scopes
                .insert(id, scope.clone());
            Some(scope)
        } else {
            state.editing.coordination.plan_scopes.get(&id).cloned()
        };
        if matches!(status, "discarded" | "expired") {
            state.editing.coordination.plan_scopes.remove(&id);
        }
        state.event_seq += 1;
        let mut payload = json!({"id":id,"revision":revision,"state":status});
        if let Some(scope) = scope {
            payload["scope"] = scope;
        }
        let event = EngineEvent {
            seq: state.event_seq,
            kind: kind.into(),
            task_id: id,
            payload,
        };
        if event.payload.get("scope").is_some() {
            for sender in state.event_senders.values() {
                let _ = sender.send(event.clone());
            }
        } else if let Some(sender) = state.event_senders.get(&owner) {
            let _ = sender.send(event);
        }
    }
}
