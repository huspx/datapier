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
mod rpc;
mod session;
mod tasks;
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
struct State {
    accepting: bool,
    clients: HashSet<Uuid>,
    client_cancels: HashMap<Uuid, CancellationToken>,
    event_senders: HashMap<Uuid, broadcast::Sender<EngineEvent>>,
    event_seq: u64,
    idle: Instant,
    databases: HashMap<Uuid, Arc<Database>>,
    tasks: HashMap<Uuid, Running>,
    unpersisted: HashMap<Uuid, UnpersistedCompletion>,
    profile_locks: HashMap<Uuid, Arc<AsyncMutex<()>>>,
    archery: archery::ArcheryState,
}
impl State {
    fn stop_if_idle(&mut self) -> bool {
        if self.clients.is_empty()
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
    id: Uuid,
    store: Store,
    state: Mutex<State>,
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
    let engine = Arc::new(Engine {
        id: Uuid::new_v4(),
        store,
        state: Mutex::new(State {
            accepting: true,
            clients: HashSet::new(),
            client_cancels: HashMap::new(),
            event_senders: HashMap::new(),
            event_seq: 0,
            idle: Instant::now(),
            databases: HashMap::new(),
            tasks: HashMap::new(),
            unpersisted: HashMap::new(),
            profile_locks: HashMap::new(),
            archery: archery::ArcheryState::default(),
        }),
    });
    let descriptor = root.join("engine.json");
    std::fs::write(
        &descriptor,
        serde_json::to_vec(
            &json!({"protocol_version":2,"engine_id":engine.id,"endpoint":endpoint}),
        )?,
    )?;
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let stream = accepted?;
                let owner = Uuid::new_v4();
                tokio::spawn(client(engine.clone(), owner, stream));
            }
            _ = tick.tick() => {
                // 与 register 共用临界区，先停止接入再收尾，防止最后一次检查后接入新会话。
                let stop = engine.state.lock().unwrap().stop_if_idle();
                if stop { break; }
            }
        }
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
    for abort in aborts {
        abort.abort();
    }
    tokio::task::yield_now().await;
    // 无人计时同时是已发送写入的宽限期；到期中断后按持久化阶段记录待确认结果。
    engine.store.recover_tasks().await?;
    let _ = std::fs::remove_file(descriptor);
    #[cfg(unix)]
    {
        let _ = std::fs::remove_file(endpoint);
    }
    drop(lock);
    Ok(())
}

impl Engine {
    fn register(&self, owner: Uuid) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.accepting {
            return false;
        }
        state.clients.insert(owner);
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
        // 与 ExecutionSink::mark_sent 共用同一门：取消必须发生在发送前，或明确属于读操作。
        let _guard = transition.lock().await;
        match self.store.task(id).await {
            Ok(task) if !task.sent || !task.may_write => cancel.cancel(),
            Ok(_) => (),
            Err(error) => tracing::error!("读取任务发送阶段失败，保留任务等待最终结果: {error}"),
        }
    }
    fn active(&self, owner: Uuid) -> Result<()> {
        if self.state.lock().unwrap().clients.contains(&owner) {
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
        let (databases, tasks) = {
            let mut state = self.state.lock().unwrap();
            if state.clients.remove(&owner) && state.clients.is_empty() {
                state.idle = Instant::now();
            }
            if let Some(cancel) = state.client_cancels.remove(&owner) {
                cancel.cancel();
            }
            state.event_senders.remove(&owner);
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
        self.disconnect_archery(owner).await;
        for (id, cancel, transition) in tasks {
            self.cancel_if_safe(id, cancel, transition).await;
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
