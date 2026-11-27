//! Engine 会话、数据库连接与任务的唯一运行状态所有者。
//!
//! `State` 的同步锁只保护注册表与短状态迁移；数据库会话的异步锁用于串行化
//! 同一物理连接上的命令。`transition` 则把取消检查、发送阶段持久化与完成记录
//! 放在同一控制门内，不能用注册表锁替代，也不能把取消等待解释为撤销写入。

use crate::{EngineEvent, platform};
use datapier_core::{
    AppError, ExecutionSink, Result, Store,
    model::{self, Profile},
    providers::ProviderSession,
};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod archery;
mod completion;
mod discovery;
mod editing;
mod local_project;
mod mcp;
mod operations;
mod project;

mod results;
mod rpc;
mod session;
mod tasks;
mod transfer;
mod workspaces;
#[cfg(test)]
use datapier_core::model::AccessMode;
use session::client;
#[cfg(test)]
use session::client_with_lease;

#[cfg(test)]
#[path = "../tests/unit/server.rs"]
mod tests;

struct Database {
    owner: Uuid,
    profile: Profile,
    database: Option<String>,
    completion_context: completion::Context,
    redis_db: Option<u32>,
    capabilities: Mutex<model::Capabilities>,
    session: AsyncMutex<ProviderSession>,
    closing: AtomicBool,
}
struct Running {
    owner: Uuid,
    session_id: Uuid,
    cancel: CancellationToken,
    transition: Arc<AsyncMutex<()>>,
    abort: Option<tokio::task::AbortHandle>,
    started: Option<Instant>,
}
// SQLite 最终更新失败时仍须给存活客户端一个终态，不能继续返回磁盘中的 running。
// 该副本只在本次 Engine 内有效；重启后的未知写入仍按已落盘的 sent 阶段恢复。
struct UnpersistedCompletion {
    state: &'static str,
    summary: Value,
    error: AppError,
}
impl UnpersistedCompletion {
    fn apply(&self, task: &mut model::TaskRecord) {
        task.state.clear();
        task.state.push_str(self.state);
        task.summary = self.summary.clone();
        task.started_at = self
            .summary
            .pointer("/_task/started_at")
            .and_then(Value::as_f64);
        task.elapsed_ms = self
            .summary
            .pointer("/_task/elapsed_ms")
            .and_then(Value::as_u64);
        task.error = Some(self.error.clone());
    }
}
#[derive(Clone)]
struct ClientContext {
    kind: model::ClientKind,
    workspace: String,
    allow_connections: Vec<Uuid>,
}

struct WindowBinding {
    owner: Uuid,
    window_id: Option<String>,
    pane_id: Option<String>,
    seen: Option<Instant>,
    last_active: Instant,
}
struct State {
    accepting: bool,
    clients: HashSet<Uuid>,
    mcp_owners: HashSet<Uuid>,
    contexts: HashMap<Uuid, ClientContext>,
    mcp: mcp::McpState,
    completion: completion::CatalogState,
    windows: HashMap<String, WindowBinding>,
    client_cancels: HashMap<Uuid, CancellationToken>,
    event_senders: HashMap<Uuid, broadcast::Sender<EngineEvent>>,
    event_seq: u64,
    idle: Instant,
    databases: HashMap<Uuid, Arc<Database>>,
    tasks: HashMap<Uuid, Running>,
    discovery: HashMap<Uuid, (u64, Value)>,
    result_sessions: HashMap<Uuid, Uuid>,
    unpersisted: HashMap<Uuid, UnpersistedCompletion>,
    profile_locks: HashMap<Uuid, Arc<AsyncMutex<()>>>,
    archery: archery::ArcheryState,
    editing: editing::EditingState,
    workspace_uploads: workspaces::WorkspaceUploadState,
    projects: project::ProjectState,
    transfers: transfer::TransferState,
    operations: Arc<operations::OperationRegistry>,
}
impl State {
    fn stop_if_idle(&mut self) -> bool {
        if self.clients.is_empty()
            && self.workspace_uploads.is_empty()
            && !self.archery.has_persistent_operations()
            && self.idle.elapsed() >= Duration::from_secs(30)
        {
            self.accepting = false;
            true
        } else {
            false
        }
    }
}
struct Engine {
    root: PathBuf,
    #[cfg(target_os = "macos")]
    instance_lock: Arc<std::fs::File>,
    update_stop: CancellationToken,
    update_done: Mutex<Option<PathBuf>>,
    id: Uuid,
    store: Store,
    state: Mutex<State>,
    send_gate: Arc<AsyncMutex<()>>,
    // 测试清理等待最后一个 Arc 释放；发送端须置于末尾，先析构 Store 和 State。
    #[cfg(test)]
    destroyed: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    #[cfg(test)]
    _destroyed_signal: Option<tokio::sync::oneshot::Sender<()>>,
}
fn serialize<T: serde::Serialize>(value: T) -> Result<Value> {
    Ok(serde_json::to_value(value)?)
}
fn workspace(params: &Value) -> &str {
    params
        .get("workspace_id")
        .and_then(Value::as_str)
        .unwrap_or("default")
}
fn optional<T: serde::de::DeserializeOwned>(params: &Value, key: &str) -> Result<Option<T>> {
    params
        .get(key)
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()).map_err(Into::into))
        .transpose()
}
fn mysql_connect_timeout(profile: &Profile, fallback: u64) -> u64 {
    if profile.provider == model::ProviderKind::Mysql {
        profile
            .direct()
            .ok()
            .and_then(|direct| direct.mysql.connect_timeout_seconds)
            .unwrap_or(fallback)
    } else {
        fallback
    }
}

fn mysql_query_timeout(profile: &Profile, fallback: u64) -> u64 {
    if profile.provider == model::ProviderKind::Mysql {
        profile
            .direct()
            .ok()
            .and_then(|direct| direct.mysql.query_timeout_seconds)
            .unwrap_or(fallback)
    } else {
        fallback
    }
}

fn profile_with_connect_timeout(profile: &Profile, fallback: u64) -> Profile {
    let mut effective = profile.clone();
    if effective.provider == model::ProviderKind::Mysql
        && let Ok(direct) = effective.direct_mut()
    {
        direct.mysql.connect_timeout_seconds.get_or_insert(fallback);
    }
    effective
}
// 执行记录不保留结构化认证字段；SQL 正文属于查询历史，不通过关键词替换改写。
fn redact(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|key, _| {
                let key = key.to_ascii_lowercase();
                !matches!(
                    key.as_str(),
                    "secret" | "credential" | "access_token" | "auth_token"
                ) && key != concat!("pass", "word")
            });
            for value in fields.values_mut() {
                redact(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                redact(value);
            }
        }
        _ => {}
    }
}

pub async fn run(root: PathBuf) -> Result<()> {
    let root = platform::root(&root)?;
    let admission = platform::update_admission(&root, platform::AdmissionMode::Shared)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("engine.lock"))?;
    if let Err(error) = lock.try_lock_exclusive() {
        if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
            return Ok(());
        }
        return Err(error.into());
    }
    // 只有持有 OS 锁的候选才能清理残留端点；端点文件存在与否不能裁决实例存活。
    let endpoint = platform::endpoint(&root)?;
    let mut listener = platform::Listener::bind(&endpoint)?;
    let store = Store::open(&root).await?;
    store.recover_tasks().await?;
    store.transfer_recover().await?;

    store.recover_edit_files().await?;
    drop(admission);
    let engine = Arc::new(Engine {
        root: root.clone(),
        #[cfg(target_os = "macos")]
        instance_lock: Arc::new(lock),
        update_stop: CancellationToken::new(),
        update_done: Mutex::new(None),
        id: Uuid::new_v4(),
        store,
        send_gate: Arc::new(AsyncMutex::new(())),
        state: Mutex::new(State {
            accepting: true,
            clients: HashSet::new(),
            mcp_owners: HashSet::new(),
            contexts: HashMap::new(),
            mcp: Default::default(),
            completion: Default::default(),
            windows: HashMap::new(),
            client_cancels: HashMap::new(),
            event_senders: HashMap::new(),
            event_seq: 0,
            idle: Instant::now(),
            databases: HashMap::new(),
            tasks: HashMap::new(),
            discovery: HashMap::new(),
            result_sessions: HashMap::new(),
            unpersisted: HashMap::new(),
            profile_locks: HashMap::new(),
            archery: archery::ArcheryState::default(),
            editing: editing::EditingState::default(),
            workspace_uploads: Default::default(),
            projects: Default::default(),
            transfers: Default::default(),
            operations: Default::default(),
        }),
        #[cfg(test)]
        destroyed: Mutex::new(None),
        #[cfg(test)]
        _destroyed_signal: None,
    });
    let descriptor = root.join("engine.json");
    std::fs::write(
        &descriptor,
        serde_json::to_vec(
            &json!({"protocol_version":crate::protocol::VERSION,"engine_id":engine.id,"endpoint":endpoint}),
        )?,
    )?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    let mut history_tick = tokio::time::interval(Duration::from_secs(60));
    let mut history_cleanup: Option<tokio::task::JoinHandle<()>> = None;
    let mut editing_cleanup: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            _ = engine.update_stop.cancelled() => break,
            accepted = listener.accept() => {
                let stream = accepted?;
                let owner = Uuid::new_v4();
                tokio::spawn(client(engine.clone(), owner, stream));
            }
            _ = history_tick.tick() => {
                if history_cleanup.as_ref().is_none_or(tokio::task::JoinHandle::is_finished) {
                    let engine = engine.clone();
                    history_cleanup = Some(tokio::spawn(async move {
                        if let Err(error) = engine.store.cleanup_history().await {
                            eprintln!("历史自动清理失败: {error}");
                        }
                    }));
                }
            }
            _ = tick.tick() => {
                // 与 register 共用临界区，先停止接入再收尾，防止最后一次检查后接入新会话。
                let stop = engine.state.lock().unwrap().stop_if_idle();
                if stop { break; }
                if editing_cleanup.as_ref().is_none_or(tokio::task::JoinHandle::is_finished) {
                    let engine = engine.clone();
                    editing_cleanup = Some(tokio::spawn(async move {
                        if let Err(error) = engine.store.expire_results().await { tracing::warn!("结果过期回收失败: {error}"); }
                        // 忙会话稍后再处理，不阻塞事务或接入。
                        let databases: Vec<_> = engine.state.lock().unwrap().databases.values().cloned().collect();
                        for db in databases {
                            if let Ok(mut session) = db.session.try_lock() {
                                session.prune_results();
                            }
                        }
                        if let Err(error) = engine.editing_sweep().await { tracing::warn!("编辑会话回收失败: {error}"); }
                    }));
                }
            }
        }
    }
    if let Some(cleanup) = history_cleanup {
        cleanup.abort();
        let _ = cleanup.await;
    }
    if let Some(cleanup) = editing_cleanup {
        cleanup.abort();
        let _ = cleanup.await;
    }
    drop(listener);
    let aborts: Vec<_> = engine
        .state
        .lock()
        .unwrap()
        .tasks
        .values()
        .filter_map(|task| task.abort.clone())
        .collect();
    for abort in &aborts {
        abort.abort();
    }
    // 等待中止生效再恢复持久化状态，避免尚在运行的任务覆盖恢复结果。
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        while aborts.iter().any(|abort| !abort.is_finished()) {
            tokio::task::yield_now().await;
        }
    })
    .await;
    // Blocking index workers must close their files before recovery and lock release.
    engine.store.wait_input_index_workers().await;
    // 无人计时同时是已发送写入的宽限期；到期中断后按持久化阶段记录待确认结果。
    engine.store.recover_tasks().await?;
    engine.store.transfer_recover().await?;
    let _ = std::fs::remove_file(descriptor);
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(endpoint);
    }
    // 等待所有客户端和后台任务释放 Store，不能先释放实例锁再结束写入。
    while Arc::strong_count(&engine) > 1 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    engine.store.pool.close().await;
    let handoff_done = engine.update_done.lock().unwrap().take();
    drop(engine);
    if let Some(done) = handoff_done {
        std::fs::write(done, b"released")?;
    }
    Ok(())
}

impl Engine {
    /// 仅允许空闲 Engine 把原锁的同一 open file description 交给有限更新进程。
    #[cfg(target_os = "macos")]
    async fn update_handoff(self: &Arc<Self>, owner: Uuid, params: Value) -> Result<Value> {
        let id: Uuid =
            optional(&params, "handoff_id")?.ok_or_else(|| AppError::invalid("缺少交接身份"))?;
        let state_dir: PathBuf =
            optional(&params, "state_dir")?.ok_or_else(|| AppError::invalid("缺少状态目录"))?;
        let plugin_dir: PathBuf = optional(&params, "plugin_dir")?
            .ok_or_else(|| AppError::invalid("缺少 plugin 目录"))?;
        let state_digest: String = optional(&params, "state_digest")?
            .ok_or_else(|| AppError::invalid("缺少更新状态摘要"))?;
        if state_digest.len() != 64 || !state_digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AppError::invalid("更新状态摘要无效"));
        }
        let state_dir = std::fs::canonicalize(state_dir)?;
        let plugin_dir = std::fs::canonicalize(plugin_dir)?;
        if state_dir.starts_with(&self.root)
            || self.root.starts_with(&state_dir)
            || plugin_dir.starts_with(&self.root)
            || self.root.starts_with(&plugin_dir)
            || state_dir.starts_with(&plugin_dir)
            || plugin_dir.starts_with(&state_dir)
        {
            return Err(AppError::invalid("更新目录互相重叠"));
        }
        let admission = platform::update_admission(&self.root, platform::AdmissionMode::Exclusive)?;
        {
            let mut state = self.state.lock().unwrap();
            if !state.accepting
                || state.contexts.len() != 1
                || !state
                    .contexts
                    .get(&owner)
                    .is_some_and(|c| c.kind == model::ClientKind::WindowBridge)
                || !state.clients.is_empty()
                || !state.tasks.is_empty()
                || !state.databases.is_empty()
                || !state.workspace_uploads.is_empty()
                || state.archery.has_persistent_operations()
            {
                return Err(AppError::new(
                    "update_busy",
                    "仍有客户端、会话或任务，不能交接实例锁",
                ));
            }
            state.accepting = false;
        }
        let gate_path = self.root.join("update-gate.json");
        let ready = state_dir.join(format!("handoff-{id}.ready"));
        let done = state_dir.join(format!("handoff-{id}.done"));
        let mut created_gate = false;
        let result = async {
            let mut gate = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&gate_path)?;
            created_gate = true;
            serde_json::to_writer(
                &mut gate,
                &json!({"phase":"handoff","id":id,"state_digest":state_digest,
                    "plugin_dir":plugin_dir,"state_dir":state_dir}),
            )?;
            gate.sync_all()?;
            std::fs::File::open(&self.root)?.sync_all()?;
            let inherited = self.instance_lock.try_clone()?;
            let mut command = tokio::process::Command::new(std::env::current_exe()?);
            command
                .arg("--data-dir")
                .arg(&self.root)
                .arg("plugin-update")
                .arg("--action")
                .arg("apply")
                .arg("--plugin-dir")
                .arg(&plugin_dir)
                .arg("--state-dir")
                .arg(&state_dir)
                .arg("--worker")
                .arg("--handoff-id")
                .arg(id.to_string())
                .stdin(std::process::Stdio::from(inherited))
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            let mut child = command.spawn()?;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
            while !ready.exists() {
                if child.try_wait()?.is_some() || tokio::time::Instant::now() >= deadline {
                    let _ = child.kill().await;
                    return Err(AppError::new(
                        "update_handoff_failed",
                        "更新进程未证明锁所有权",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            if child.try_wait()?.is_some() {
                return Err(AppError::new("update_handoff_failed", "锁交接进程已退出"));
            }
            *self.update_done.lock().unwrap() = Some(done);
            self.update_stop.cancel();
            Ok(json!({"handoff_id":id,"accepted":true}))
        }
        .await;
        if result.is_err() {
            if created_gate {
                let _ = std::fs::remove_file(gate_path);
            }
            self.state.lock().unwrap().accepting = true;
        }
        drop(admission);
        result
    }

    #[cfg(not(target_os = "macos"))]
    async fn update_handoff(self: &Arc<Self>, _owner: Uuid, _params: Value) -> Result<Value> {
        Err(AppError::new(
            "update_unavailable",
            "当前平台未验证连续锁交接",
        ))
    }
    #[cfg(test)]
    fn register(&self, owner: Uuid) -> bool {
        self.register_with(
            owner,
            ClientContext {
                kind: model::ClientKind::Tsp,
                workspace: "default".into(),
                allow_connections: Vec::new(),
            },
        )
    }
    fn register_with(&self, owner: Uuid, context: ClientContext) -> bool {
        let Ok(_admission) =
            platform::update_admission(&self.root, platform::AdmissionMode::Shared)
        else {
            return false;
        };
        let mut state = self.state.lock().unwrap();
        if !state.accepting {
            return false;
        }
        if !matches!(context.kind, model::ClientKind::WindowBridge) {
            state.clients.insert(owner);
        }
        if matches!(context.kind, model::ClientKind::Mcp) {
            state.mcp_owners.insert(owner);
        }
        state.contexts.insert(owner, context);
        state.client_cancels.insert(owner, CancellationToken::new());
        state.event_senders.insert(owner, broadcast::channel(128).0);
        true
    }
    fn emit_state(&self, owner: Uuid, task_id: Uuid, task_state: &str) {
        let mut state = self.state.lock().unwrap();
        if state.event_senders.contains_key(&owner) {
            state.event_seq += 1;
            let event = EngineEvent {
                seq: state.event_seq,
                kind: "execution.state".into(),
                task_id,
                payload: json!({"state":task_state}),
            };
            let _ = state.event_senders[&owner].send(event);
        }
    }
    fn emit_settings_changed(&self) {
        let mut state = self.state.lock().unwrap();
        state.event_seq += 1;
        let event = EngineEvent {
            seq: state.event_seq,
            kind: "settings.changed".into(),
            task_id: Uuid::nil(),
            payload: Value::Null,
        };
        for sender in state.event_senders.values() {
            let _ = sender.send(event.clone());
        }
    }
    fn profile_lock(&self, id: Uuid) -> Arc<AsyncMutex<()>> {
        self.state
            .lock()
            .unwrap()
            .profile_locks
            .entry(id)
            .or_insert_with(|| Arc::new(AsyncMutex::new(())))
            .clone()
    }
    async fn cancel_if_safe(
        &self,
        id: Uuid,
        cancel: CancellationToken,
        transition: Arc<AsyncMutex<()>>,
    ) {
        // 单次写入取消只允许发生在发送前；分批迁移由自身 journal 在批次边界停止。
        // 迁移任务的 sent 单调置真，不能据此拒绝停止后续尚未发送的批次。
        let _guard = transition.lock().await;
        match self.store.task(id).await {
            Ok(task) if task.method == "transfer.submit" || !task.sent || !task.may_write => {
                cancel.cancel()
            }
            Ok(_) => (),
            Err(error) => tracing::error!("读取任务发送阶段失败，保留任务等待最终结果: {error}"),
        }
    }
    fn active(&self, owner: Uuid) -> Result<()> {
        if self.state.lock().unwrap().contexts.contains_key(&owner) {
            Ok(())
        } else {
            Err(AppError::new("disconnected", "客户端会话已关闭"))
        }
    }
    fn database(&self, owner: Uuid, params: &Value) -> Result<Arc<Database>> {
        let id: Uuid = model::param(params, "session_id")?;
        let state = self.state.lock().unwrap();
        let db = state
            .databases
            .get(&id)
            .ok_or_else(|| AppError::new("not_found", "数据库会话不存在"))?;
        if db.owner != owner
            || !state.clients.contains(&owner)
            || db.closing.load(Ordering::Acquire)
        {
            return Err(AppError::new(
                "permission_denied",
                "数据库会话不可用或属于其他客户端",
            ));
        }
        Ok(db.clone())
    }
    async fn disconnect(self: &Arc<Self>, owner: Uuid) {
        // 先在锁内撤销归属、禁止新请求，再在锁外取消任务与关闭各自连接，避免拖住其他客户端。
        let send_guard = self.send_gate.lock().await;
        let (databases, tasks) = {
            let mut state = self.state.lock().unwrap();
            if state.clients.remove(&owner) && state.clients.is_empty() {
                state.idle = Instant::now();
            }
            state.contexts.remove(&owner);
            state.mcp.disconnect(owner);
            state.windows.retain(|_, binding| binding.owner != owner);
            if let Some(cancel) = state.client_cancels.remove(&owner) {
                cancel.cancel();
            }
            state.event_senders.remove(&owner);
            state.transfers.remove_owner(owner);
            state.projects.remove_owner(owner);
            state.operations.remove_owner(owner);
            let dbs = state
                .databases
                .iter()
                .filter(|(_, db)| db.owner == owner && !db.closing.swap(true, Ordering::AcqRel))
                .map(|(id, db)| (*id, db.clone()))
                .collect::<Vec<_>>();
            let tasks = state
                .tasks
                .iter()
                .filter(|(_, task)| task.owner == owner)
                .map(|(id, task)| (*id, task.cancel.clone(), task.transition.clone()))
                .collect::<Vec<_>>();
            (dbs, tasks)
        };
        drop(send_guard);
        if let Err(error) = self.mcp_disconnected(owner).await {
            tracing::warn!("清理客户端审批失败: {error}");
        }
        // 释放展示预算会唤醒背压等待；潜在写入仍由 provider 消费协议终态。
        if let Err(error) = self.store.release_owner_results(owner).await {
            tracing::warn!("释放客户端结果失败: {error}");
        }
        self.disconnect_archery(owner).await;
        for (id, cancel, transition) in tasks {
            self.cancel_if_safe(id, cancel, transition).await;
        }
        if let Err(error) = self.editing_disconnect(owner).await {
            tracing::warn!("清理客户端编辑会话失败: {error}");
        }
        for (id, db) in databases {
            let engine = self.clone();
            tokio::spawn(async move {
                let _ = db.session.lock().await.close().await;
                engine.state.lock().unwrap().databases.remove(&id);
            });
        }
    }
}
