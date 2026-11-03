//! Archery 认证归属、短操作和服务配置；所有远端等待均在运行引用登记之后开始。

use super::*;
use datapier_core::{
    model::{ArcheryServer, CredentialUpdate, ProviderKind, TotpMode},
    providers::archery::{AuthSlot, generate_totp, normalize_server},
};

mod auth;
mod configuration;
mod connections;
mod rpc;

#[cfg(test)]
pub(super) static FAIL_CREDENTIAL_READ: Mutex<Vec<Uuid>> = Mutex::new(Vec::new());

#[derive(Default)]
pub(super) struct ArcheryState {
    slots: HashMap<(Uuid, Uuid), Arc<AuthEntry>>,
    candidates: HashMap<(Uuid, Uuid), Arc<AuthEntry>>,
    handles: HashMap<Uuid, Arc<AuthEntry>>,
    shared: HashMap<Uuid, Arc<SharedAuth>>,
    gates: HashMap<Uuid, Arc<AsyncMutex<()>>>,
    operations: HashMap<(Uuid, Uuid), ShortOperation>,
}
impl ArcheryState {
    pub(super) fn has_persistent_operations(&self) -> bool {
        self.operations
            .values()
            .any(|operation| operation.persistent)
    }
}
struct SharedAuth {
    auth: Arc<AuthSlot>,
    controller: Arc<Mutex<Option<Controller>>>,
}
#[derive(Clone)]
struct Controller {
    owner: Uuid,
    auth_id: Uuid,
    token: Uuid,
    generation: u64,
    challenge: bool,
    verify_operation: Option<Uuid>,
}
struct AuthEntry {
    id: Uuid,
    owner: Uuid,
    server: Mutex<ArcheryServer>,
    auth: Arc<AuthSlot>,
    secret: Mutex<Option<String>>,
    context_operation: Mutex<Option<Uuid>>,
    totp: Mutex<Option<String>>,
    expected_revision: u64,
    candidate: bool,
    controller: Arc<Mutex<Option<Controller>>>,
}
struct VerifyGuard {
    controller: Arc<Mutex<Option<Controller>>>,
    token: Uuid,
    operation_id: Uuid,
}
impl Drop for VerifyGuard {
    fn drop(&mut self) {
        let mut controller = self.controller.lock().unwrap();
        if let Some(controller) = controller.as_mut()
            && controller.token == self.token
            && controller.verify_operation == Some(self.operation_id)
        {
            controller.verify_operation = None;
        }
    }
}
struct ShortOperation {
    server_id: Option<Uuid>,
    profile_id: Option<Uuid>,
    cancel: CancellationToken,
    persistent: bool,
}
// future 被丢弃也释放运行引用；取消占位仍须保留到原 operation_id 开始，避免先取消后发送。
pub(super) struct OperationGuard {
    engine: Arc<Engine>,
    owner: Uuid,
    id: Uuid,
    cancel: CancellationToken,
}
impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.engine
            .state
            .lock()
            .unwrap()
            .archery
            .operations
            .remove(&(self.owner, self.id));
    }
}

pub(super) fn budget<T: serde::Serialize>(value: &T) -> Result<()> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            if self.0 > 4 * 1024 * 1024 - 1024 {
                return Err(std::io::Error::other("响应超限"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // 预留 RPC 信封，计数时不再分配一份最多 4 MiB 的 JSON 副本。
    serde_json::to_writer(Counter(0), value)
        .map_err(|_| AppError::new("response_too_large", "响应超过 4 MiB，请缩小选择或查询范围"))
}
fn same_identity(a: &ArcheryServer, b: &ArcheryServer) -> bool {
    a.id == b.id && a.base_url == b.base_url && a.username == b.username
}
fn same_auth_config(a: &ArcheryServer, b: &ArcheryServer) -> bool {
    same_identity(a, b) && a.http_tls == b.http_tls && a.totp_mode == b.totp_mode
}

impl Engine {
    pub(super) fn service_gate(&self, server_id: Uuid) -> Arc<AsyncMutex<()>> {
        self.state
            .lock()
            .unwrap()
            .archery
            .gates
            .entry(server_id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
    fn auth_entry(&self, owner: Uuid, id: Uuid) -> Result<Arc<AuthEntry>> {
        let state = self.state.lock().unwrap();
        state
            .archery
            .handles
            .get(&id)
            .filter(|entry| entry.owner == owner)
            .cloned()
            .ok_or_else(|| AppError::new("auth_required", "认证上下文不存在或不属于当前客户端"))
    }
    pub(super) fn register_operation(
        self: &Arc<Self>,
        owner: Uuid,
        id: Uuid,
        server_id: Uuid,
        profile_id: Option<Uuid>,
        persistent: bool,
    ) -> Result<OperationGuard> {
        let mut state = self.state.lock().unwrap();
        let parent = state
            .client_cancels
            .get(&owner)
            .ok_or_else(|| AppError::new("disconnected", "客户端已关闭"))?;
        let cancel = if persistent {
            CancellationToken::new()
        } else {
            parent.child_token()
        };
        if let Some(previous) = state.archery.operations.get(&(owner, id)) {
            if previous.server_id.is_none() && previous.cancel.is_cancelled() {
                state.archery.operations.remove(&(owner, id));
                return Err(AppError::cancelled());
            }
            return Err(AppError::conflict("operation_id 已在使用"));
        }
        if state
            .archery
            .operations
            .keys()
            .filter(|(client, _)| *client == owner)
            .count()
            >= 128
        {
            return Err(AppError::new("rate_limited", "待处理操作过多"));
        }
        state.archery.operations.insert(
            (owner, id),
            ShortOperation {
                server_id: Some(server_id),
                profile_id,
                cancel: cancel.clone(),
                persistent,
            },
        );
        Ok(OperationGuard {
            engine: self.clone(),
            owner,
            id,
            cancel,
        })
    }
    fn operation(
        self: &Arc<Self>,
        owner: Uuid,
        id: Uuid,
        server_id: Uuid,
        profile_id: Option<Uuid>,
    ) -> Result<OperationGuard> {
        self.register_operation(owner, id, server_id, profile_id, false)
    }
    pub(super) fn profile_occupied(&self, id: Uuid) -> bool {
        let state = self.state.lock().unwrap();
        state.databases.values().any(|db| db.profile.id == id)
            || state
                .archery
                .operations
                .values()
                .any(|op| op.profile_id == Some(id))
    }
    async fn service_exclusive(
        &self,
        server_id: Uuid,
        allowed_auth: Option<Uuid>,
        operation: (Uuid, Uuid),
    ) -> Result<()> {
        let state = self.state.lock().unwrap();
        let operating = state
            .archery
            .operations
            .iter()
            .any(|(key, op)| *key != operation && op.server_id == Some(server_id));
        let running = state.tasks.values().any(|task| {
            state
                .databases
                .get(&task.session_id)
                .and_then(|db| db.profile.archery())
                .is_some_and(|c| c.server_id == server_id)
        });
        let authenticating = state.archery.handles.values().any(|entry| {
            entry.server.lock().unwrap().id == server_id
                && Some(entry.id) != allowed_auth
                && entry.controller.lock().unwrap().is_some()
        });
        if operating || running || authenticating {
            return Err(AppError::new("busy", "服务仍有活动请求或认证"));
        }
        Ok(())
    }
    async fn auth_response(entry: &AuthEntry) -> Result<Value> {
        let status = entry.auth.status().await;
        let controller = entry
            .controller
            .lock()
            .unwrap()
            .clone()
            .filter(|c| c.owner == entry.owner && c.auth_id == entry.id);
        Ok(
            json!({"auth_id":entry.id,"server_id":entry.server.lock().unwrap().id,
            "state":status.state,"auth_generation":controller.as_ref().map_or(status.generation, |c| c.generation),
            "controller":controller.is_some(),"controller_token":controller.map(|c| c.token)}),
        )
    }
    async fn reconcile_challenge(&self, entry: &AuthEntry) {
        let status = entry.auth.status().await;
        if status.state == "challenge_required" || status.state == "authenticating" {
            return;
        }
        let revoked = {
            let mut control = entry.controller.lock().unwrap();
            if control.as_ref().is_some_and(|c| {
                c.challenge && (status.state != "ready" || c.verify_operation.is_none())
            }) {
                control.take()
            } else {
                None
            }
        };
        if let Some(control) = revoked {
            let owner = {
                let state = self.state.lock().unwrap();
                if let Some(operation_id) = control.verify_operation
                    && let Some(operation) =
                        state.archery.operations.get(&(control.owner, operation_id))
                {
                    operation.cancel.cancel();
                }
                state.archery.handles.get(&control.auth_id).cloned()
            };
            if let Some(owner) = owner {
                *owner.context_operation.lock().unwrap() = None;
            }
            self.emit_auth_changed(entry).await;
        }
    }
    async fn require_ready(entry: &AuthEntry) -> Result<()> {
        let status = serialize(entry.auth.status().await)?;
        if status.get("state").and_then(Value::as_str) != Some("ready") {
            return Err(
                AppError::new("auth_required", "请先登录 Archery 服务").with_details(
                    json!({"auth_id":entry.id,"server_id":entry.server.lock().unwrap().id}),
                ),
            );
        }
        Ok(())
    }
    async fn emit_shared_auth_changed(&self, server_id: Uuid, auth: &AuthSlot) {
        let status = auth.status().await;
        self.emit_auth_state(server_id, &status.state, status.generation);
    }
    fn emit_auth_state(&self, server_id: Uuid, auth_state: &str, generation: u64) {
        let mut state = self.state.lock().unwrap();
        state.event_seq += 1;
        let event = EngineEvent {
            seq: state.event_seq,
            kind: "archery.auth.changed".into(),
            task_id: Uuid::nil(),
            payload: json!({"server_id":server_id,"state":auth_state,"auth_generation":generation}),
        };
        for sender in state.event_senders.values() {
            let _ = sender.send(event.clone());
        }
    }
    async fn emit_auth_changed(&self, entry: &AuthEntry) {
        if !entry.candidate {
            let server_id = entry.server.lock().unwrap().id;
            self.emit_shared_auth_changed(server_id, &entry.auth).await;
        }
    }
    pub(super) fn emit_connections_changed(&self) {
        let mut state = self.state.lock().unwrap();
        state.event_seq += 1;
        let event = EngineEvent {
            seq: state.event_seq,
            kind: "connections.changed".into(),
            task_id: Uuid::nil(),
            payload: json!({}),
        };
        for sender in state.event_senders.values() {
            let _ = sender.send(event.clone());
        }
    }
}
