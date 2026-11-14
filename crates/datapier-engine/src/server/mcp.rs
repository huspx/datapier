//! MCP 的固定业务入口；权限证明、审批与句柄归属均由 Engine 执行。
use super::*;
use datapier_core::model::{ApprovalRecord, ClientKind, McpAccessLevel, ProviderKind};
use datapier_core::policy::{ObjectAccess, SqlAccess, SqlContext, analyze_mysql};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

mod access;
mod authentication;
mod catalog;
mod execution;
mod results;

#[derive(Default)]
pub(super) struct McpState {
    sessions: HashMap<Uuid, SessionState>,
    gates: HashMap<Uuid, Arc<AsyncMutex<()>>>,
    approvals: HashMap<Uuid, ApprovalRoute>,
    cursors: HashMap<Uuid, Cursor>,
    values: HashMap<Uuid, (Uuid, Uuid)>,
    pages: HashMap<(Uuid, Uuid, usize), Vec<u64>>,
    auth_attempts: HashSet<(Uuid, Uuid, u64, u64)>,
    authentication: HashMap<(Uuid, Uuid), AuthenticationRequest>,
}
impl McpState {
    pub(super) fn disconnect(&mut self, owner: Uuid) {
        self.sessions.retain(|_, session| session.owner != owner);
        self.gates.retain(|id, _| self.sessions.contains_key(id));
        self.approvals.retain(|_, route| route.owner != owner);
        self.cursors.retain(|_, cursor| cursor.owner != owner);
        self.values.retain(|_, (client, _)| *client != owner);
        self.pages.retain(|(client, _, _), _| *client != owner);
        self.auth_attempts
            .retain(|(client, _, _, _)| *client != owner);
        self.authentication
            .retain(|(client, _), _| *client != owner);
    }
    pub(super) fn release_session_handles(
        &mut self,
        owner: Uuid,
        session: Uuid,
        results: &HashSet<Uuid>,
    ) {
        self.cursors.retain(|_, cursor| {
            cursor.owner != owner
                || (cursor.resource != session && !results.contains(&cursor.resource))
        });
        self.pages
            .retain(|(client, result, _), _| *client != owner || !results.contains(result));
        self.values
            .retain(|_, (client, result)| *client != owner || !results.contains(result));
    }
}
#[derive(Clone)]
struct AuthenticationRequest {
    id: Uuid,
    owner: Uuid,
    connection_id: Uuid,
    server_id: Uuid,
    instance_id: i64,
    profile_revision: u64,
    controller: Option<Uuid>,
}
struct SessionState {
    owner: Uuid,
    generation: u64,
    pending: Option<Uuid>,
    queued: Option<AccessProof>,
}
struct ApprovalRoute {
    owner: Uuid,
    execution_id: Uuid,
    controller: Option<Uuid>,
}
#[derive(Clone)]
struct Cursor {
    owner: Uuid,
    operation: String,
    resource: Uuid,
    start: u64,
    page_size: usize,
    params: Value,
    seen: Instant,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct AccessProof {
    version: u32,
    profile_id: Uuid,
    session_id: Uuid,
    identity: Value,
    profile_revision: u64,
    policy_revision: u64,
    generation: u64,
    context: Value,
    read_databases: BTreeSet<String>,
    write_databases: BTreeSet<String>,
    objects: Vec<ObjectAccess>,
    definitions: Vec<Value>,
    dangerous: bool,
    transaction_actions: Vec<String>,
    may_write: bool,
    reason: String,
}
fn denied(message: &str) -> AppError {
    AppError::new("permission_denied", message)
}
fn now() -> Result<i64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| AppError::new("internal", error.to_string()))?
        .as_secs() as i64)
}
fn identity(profile: &Profile) -> Value {
    match &profile.connection {
        model::ConnectionConfig::Direct(direct) => {
            json!({"provider":profile.provider,"source":"direct","host":direct.host,"port":direct.port,"username":direct.username,"tls":direct.tls})
        }
        model::ConnectionConfig::Archery(connection) => {
            json!({"provider":profile.provider,"source":"archery","server_id":connection.server_id,"instance_id":connection.instance_id})
        }
    }
}
fn number(params: &Value, key: &str, fallback: u64) -> Result<u64> {
    let value = optional::<u64>(params, key)?.unwrap_or(fallback);
    if value == 0 {
        return Err(AppError::invalid(format!("{key} 必须大于零")));
    }
    Ok(value)
}
fn paging(params: &Value, default: usize, maximum: usize) -> Result<(u64, usize)> {
    let page = number(params, "page", 1)?;
    let size = usize::try_from(number(params, "page_size", default as u64)?)
        .map_err(|_| AppError::invalid("每页行数过大"))?;
    if size > maximum {
        return Err(AppError::invalid("每页行数超过有效限制"));
    }
    Ok((page, size))
}
fn encoded_len<T: Serialize>(value: &T) -> Result<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}
impl Engine {
    pub(super) async fn mcp_rpc(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let context = self
            .state
            .lock()
            .unwrap()
            .contexts
            .get(&owner)
            .cloned()
            .ok_or_else(|| AppError::new("disconnected", "客户端已关闭"))?;
        if context.kind != ClientKind::Mcp {
            return Err(denied("仅 MCP 客户端可调用此入口"));
        }
        // 外部参数不能注入内部证明、归属或持久化上下文。
        if params
            .as_object()
            .is_some_and(|object| object.keys().any(|key| key.starts_with('_')))
        {
            return Err(AppError::invalid("不接受内部参数"));
        }
        let result = match method {
            "mcp.get_server_info" => {
                let settings = self.store.settings().await?;
                Ok(
                    json!({"version":env!("CARGO_PKG_VERSION"),"engine_id":self.id,"protocol_version":crate::protocol::VERSION,
                    "capabilities":{"tools":true,"transport":"stdio","transactions":true},
                    "limits":settings.limits,"fine_grained_access":settings.mcp.fine_grained_access}),
                )
            }
            "mcp.list_connections" => self.mcp_connections(owner, &params).await,
            "mcp.list_databases" => self.mcp_databases(owner, &params).await,
            "mcp.open_session" => self.mcp_open(owner, &params).await,
            "mcp.get_session" => self.mcp_session(owner, &params).await,
            "mcp.close_session" => self.mcp_close(owner, &params).await,
            "mcp.get_auth_status" => self.mcp_auth(owner, &params).await,
            "mcp.list_resources" | "mcp.describe_resource" => {
                self.mcp_resources(owner, method, params).await
            }
            "mcp.browse_resource"
            | "mcp.execute_command"
            | "mcp.begin_transaction"
            | "mcp.commit_transaction"
            | "mcp.rollback_transaction"
            | "mcp.check_sql" => self.mcp_execute(owner, method, params).await,
            "mcp.approve_command" => self.mcp_approve(owner, &params).await,
            "mcp.get_execution" => self.mcp_execution(owner, &params).await,
            "mcp.cancel_execution" => self.mcp_cancel(owner, &params).await,
            "mcp.read_result" => self.mcp_read(owner, &params).await,
            "mcp.count_rows" => self.mcp_count(owner, &params).await,
            "mcp.read_value" => self.mcp_value(owner, &params).await,
            "mcp.release_result" => self.mcp_release(owner, &params).await,
            "mcp.list_history" => self.mcp_history(owner, &params).await,
            _ => Err(AppError::new("method_not_found", "未知 MCP 工具")),
        }?;
        let settings = self.store.settings().await?;
        if encoded_len(&result)? > settings.limits.response_bytes {
            return Err(AppError::new(
                "response_limit",
                "响应超过字节限制，请使用分页或缩小目标",
            ));
        }
        Ok(result)
    }
    fn mcp_online(&self) -> Vec<Uuid> {
        Self::mcp_online_state(&self.state.lock().unwrap())
    }
    fn mcp_online_state(state: &State) -> Vec<Uuid> {
        let mut windows: Vec<_> = state
            .windows
            .values()
            .filter_map(|binding| {
                binding
                    .seen
                    .filter(|seen| seen.elapsed() < Duration::from_secs(15))
                    .filter(|_| {
                        binding.window_id.is_some()
                            && binding.pane_id.is_some()
                            && state.clients.contains(&binding.owner)
                    })
                    .map(|_| (binding.last_active, binding.owner))
            })
            .collect();
        windows.sort_by_key(|(active, owner)| (std::cmp::Reverse(*active), *owner));
        windows.into_iter().map(|(_, owner)| owner).collect()
    }
    fn mcp_preferred_window(&self, profile_id: Uuid) -> Option<Uuid> {
        let state = self.state.lock().unwrap();
        let online = Self::mcp_online_state(&state);
        Self::mcp_preferred_window_state(&state, &online, profile_id)
    }
    fn mcp_preferred_window_state(
        state: &State,
        online: &[Uuid],
        profile_id: Uuid,
    ) -> Option<Uuid> {
        online
            .iter()
            .copied()
            .find(|owner| {
                state
                    .databases
                    .values()
                    .any(|db| db.owner == *owner && db.profile.id == profile_id)
            })
            .or_else(|| online.first().copied())
    }
    fn mcp_context(&self, owner: Uuid) -> Result<ClientContext> {
        self.state
            .lock()
            .unwrap()
            .contexts
            .get(&owner)
            .cloned()
            .ok_or_else(|| AppError::new("disconnected", "客户端已关闭"))
    }
    fn mcp_cursor(
        &self,
        owner: Uuid,
        operation: &str,
        resource: Uuid,
        params: &Value,
    ) -> Result<Option<Cursor>> {
        let Some(token) = optional::<Uuid>(params, "cursor")? else {
            return Ok(None);
        };
        let mut state = self.state.lock().unwrap();
        let cursor = state
            .mcp
            .cursors
            .get(&token)
            .filter(|cursor| {
                cursor.owner == owner
                    && cursor.operation == operation
                    && cursor.resource == resource
            })
            .ok_or_else(|| denied("游标无效或不属于当前请求"))?;
        if cursor.seen.elapsed() > Duration::from_secs(300) {
            state.mcp.cursors.remove(&token);
            return Err(AppError::new("expired", "游标已过期"));
        }
        Ok(Some(cursor.clone()))
    }
    fn mcp_next_cursor(
        &self,
        owner: Uuid,
        operation: &str,
        resource: Uuid,
        start: u64,
        page_size: usize,
        params: Value,
    ) -> Uuid {
        let mut state = self.state.lock().unwrap();
        state
            .mcp
            .cursors
            .retain(|_, cursor| cursor.seen.elapsed() < Duration::from_secs(300));
        // 同一读取位置只保留一个游标，重复读取不线性增长。
        if let Some((id, _)) = state.mcp.cursors.iter_mut().find(|(_, cursor)| {
            cursor.owner == owner
                && cursor.operation == operation
                && cursor.resource == resource
                && cursor.start == start
                && cursor.page_size == page_size
                && cursor.params == params
        }) {
            return *id;
        }
        let id = Uuid::new_v4();
        state.mcp.cursors.insert(
            id,
            Cursor {
                owner,
                operation: operation.into(),
                resource,
                start,
                page_size,
                params,
                seen: Instant::now(),
            },
        );
        id
    }
}
