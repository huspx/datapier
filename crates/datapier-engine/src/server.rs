//! Engine 会话、数据库连接与任务的唯一运行状态所有者。
//!
//! `State` 的同步锁只保护注册表与短状态迁移；数据库会话的异步锁用于串行化
//! 同一物理连接上的命令。`transition` 则把取消检查、发送阶段持久化与完成记录
//! 放在同一控制门内，不能用注册表锁替代，也不能把取消等待解释为撤销写入。

use crate::{EngineEvent, platform, protocol};
use datapier_core::{
    AppError, ExecutionSink, Result, Store,
    model::{self, AccessMode, Profile},
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
use tokio::sync::{Mutex as AsyncMutex, broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[cfg(test)]
#[path = "server_tests.rs"]
mod tests;

struct Database {
    owner: Uuid,
    profile: Profile,
    session: AsyncMutex<ProviderSession>,
    closing: AtomicBool,
}
struct Running {
    owner: Uuid,
    session_id: Uuid,
    cancel: CancellationToken,
    transition: Arc<AsyncMutex<()>>,
    abort: Option<tokio::task::AbortHandle>,
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
        task.error = Some(self.error.clone());
    }
}
struct State {
    accepting: bool,
    clients: HashSet<Uuid>,
    event_senders: HashMap<Uuid, broadcast::Sender<EngineEvent>>,
    event_seq: u64,
    idle: Instant,
    databases: HashMap<Uuid, Arc<Database>>,
    tasks: HashMap<Uuid, Running>,
    unpersisted: HashMap<Uuid, UnpersistedCompletion>,
    profile_locks: HashMap<Uuid, Arc<AsyncMutex<()>>>,
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
            event_senders: HashMap::new(),
            event_seq: 0,
            idle: Instant::now(),
            databases: HashMap::new(),
            tasks: HashMap::new(),
            unpersisted: HashMap::new(),
            profile_locks: HashMap::new(),
        }),
    });
    let descriptor = root.join("engine.json");
    std::fs::write(
        &descriptor,
        serde_json::to_vec(
            &json!({"protocol_version":1,"engine_id":engine.id,"endpoint":endpoint}),
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
                let stop = { let mut state = engine.state.lock().unwrap(); if state.clients.is_empty() && state.idle.elapsed() >= Duration::from_secs(30) { state.accepting = false; true } else { false } };
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

async fn client(engine: Arc<Engine>, owner: Uuid, stream: platform::Stream) {
    client_with_lease(engine, owner, stream, Duration::from_secs(45), 128).await;
}

async fn client_with_lease(
    engine: Arc<Engine>,
    owner: Uuid,
    stream: platform::Stream,
    lease_duration: Duration,
    queue_capacity: usize,
) {
    let (mut reader, mut writer) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<Value>(queue_capacity);
    let mut writer_task = tokio::spawn(async move {
        while let Some(value) = rx.recv().await {
            if let Err(error) = protocol::write(&mut writer, &value).await
                && (error.code != "invalid_input"
                    || protocol::write(
                        &mut writer,
                        &response(value.get("id").cloned().unwrap_or(Value::Null), Err(error)),
                    )
                    .await
                    .is_err())
            {
                break;
            }
        }
    });
    let first = tokio::time::timeout(lease_duration, protocol::read(&mut reader)).await;
    let mut opened = false;
    let mut events = None;
    if let Ok(Ok(request)) = first {
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let result = if request.get("method").and_then(Value::as_str) != Some("session.open") {
            Err(AppError::invalid("必须先进行协议握手"))
        } else if request
            .pointer("/params/protocol_version")
            .and_then(Value::as_u64)
            != Some(1)
        {
            Err(AppError::new(
                "protocol_incompatible",
                "Engine 协议版本不兼容",
            ))
        } else if !engine.register(owner) {
            Err(AppError::new("disconnected", "Engine 正在退出"))
        } else {
            opened = true;
            events = Some(engine.state.lock().unwrap().event_senders[&owner].subscribe());
            Ok(
                json!({"protocol_version":1,"engine_id":engine.id,"client_session_id":owner,"heartbeat_seconds":10,"lease_seconds":45,"grace_seconds":30}),
            )
        };
        let _ = tx.send(response(id, result)).await;
    }
    if opened {
        let mut events = events.expect("registered session has event subscription");
        let mut lease = tokio::time::Instant::now() + lease_duration;
        loop {
            // read_exact 消费帧前缀后不能重启；收到事件时保留同一个读取 future 和租约截止时间。
            let read = tokio::time::timeout_at(lease, protocol::read(&mut reader));
            tokio::pin!(read);
            let request = loop {
                tokio::select! {
                    event = events.recv() => match event {
                        // 通知允许丢失并由状态查询补齐，不能等待满队列而阻断租约检查。
                        Ok(event) => match tx.try_send(serde_json::to_value(event).expect("event serialization")) {
                            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => (),
                            Err(mpsc::error::TrySendError::Closed(_)) => break None,
                        },
                        Err(broadcast::error::RecvError::Lagged(_)) => (),
                        Err(broadcast::error::RecvError::Closed) => break None,
                    },
                    request = &mut read => break match request {
                        Ok(Ok(value)) => Some(value),
                        _ => None,
                    },
                }
            };
            let Some(request) = request else { break };
            lease = tokio::time::Instant::now() + lease_duration;
            let id = request.get("id").cloned().unwrap_or(Value::Null);
            let method = match request.get("method").and_then(Value::as_str) {
                Some(v) => v.to_owned(),
                None => {
                    if !send_before(
                        &tx,
                        lease,
                        response(id, Err(AppError::invalid("缺少 method"))),
                    )
                    .await
                    {
                        break;
                    }
                    continue;
                }
            };
            let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
            if method == "session.close" {
                let _ = send_before(&tx, lease, response(id, Ok(json!({"closed":true})))).await;
                break;
            }
            if method == "session.heartbeat" {
                if !send_before(&tx, lease, response(id, Ok(json!({"ok":true})))).await {
                    break;
                }
                continue;
            }
            let engine = engine.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                let result = engine.dispatch(owner, &method, params).await;
                let _ = tx.send(response(id, result)).await;
            });
        }
    }
    engine.disconnect(owner).await;
    drop(tx);
    // close 确认已先入队；后台操作仍可能持有 sender，给 writer 有限时间 flush 后结束连接。
    if tokio::time::timeout(Duration::from_secs(1), &mut writer_task)
        .await
        .is_err()
    {
        writer_task.abort();
    }
}
// 控制响应不可静默丢弃，但慢客户端也不能无限拖延会话释放。
async fn send_before(
    tx: &mpsc::Sender<Value>,
    deadline: tokio::time::Instant,
    value: Value,
) -> bool {
    matches!(
        tokio::time::timeout_at(deadline, tx.send(value)).await,
        Ok(Ok(()))
    )
}
fn response(id: Value, result: Result<Value>) -> Value {
    match result {
        Ok(value) => json!({"id":id,"result":value}),
        Err(error) => json!({"id":id,"error":error}),
    }
}
impl Engine {
    fn register(&self, owner: Uuid) -> bool {
        let mut state = self.state.lock().unwrap();
        if !state.accepting {
            return false;
        }
        state.clients.insert(owner);
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
    async fn provider(
        &self,
        db: Arc<Database>,
        method: &str,
        params: Value,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
        transition: &AsyncMutex<()>,
    ) -> Result<Value> {
        let mut session = tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), lock = db.session.lock() => lock };
        {
            let _guard = transition.lock().await;
            if cancel.is_cancelled() {
                return Err(AppError::cancelled());
            }
            self.store
                .update_task(sink.execution_id, "running", Value::Null, None)
                .await?;
            self.emit_state(db.owner, sink.execution_id, "running");
        }
        if method == "transfer.preview" {
            datapier_core::execution::transfer::preview(&mut session, params, sink, cancel).await
        } else if method.starts_with("transfer.") {
            datapier_core::execution::transfer::run(&mut session, method, params, sink, cancel)
                .await
        } else {
            session
                .call(
                    if method == "execution.start" {
                        "execution.run"
                    } else {
                        method
                    },
                    params,
                    sink,
                    cancel,
                )
                .await
        }
    }
    async fn task(&self, id: Uuid) -> Result<model::TaskRecord> {
        let mut task = self.store.task(id).await?;
        if let Some(completion) = self.state.lock().unwrap().unpersisted.get(&id) {
            completion.apply(&mut task);
        }
        Ok(task)
    }
    async fn finish_task(&self, owner: Uuid, id: Uuid, result: &Result<Value>) {
        let transition = self
            .state
            .lock()
            .unwrap()
            .tasks
            .get(&id)
            .map(|task| task.transition.clone());
        let _guard = match transition.as_ref() {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        // 错误本身不能证明写入未发生；结合已持久化的 sent/may_write 判定是否需要人工核对。
        let record = match result {
            Err(_) => self.store.task(id).await.ok(),
            Ok(_) => None,
        };
        let (state, summary, error) = match result {
            Ok(value) => ("succeeded", value.clone(), None),
            Err(error) => {
                let unknown = record.as_ref().is_some_and(|r| r.sent && r.may_write)
                    && (error.code == "cancelled"
                        || error.code == "disconnected"
                        || error.code == "outcome_unknown");
                (
                    if unknown {
                        "outcome_unknown"
                    } else if error.code == "cancelled" {
                        "cancelled"
                    } else {
                        "failed"
                    },
                    (*error.details).clone(),
                    Some(error.clone()),
                )
            }
        };
        let mut final_state = state;
        if let Err(e) = self.store.update_task(id, state, summary, error).await {
            tracing::error!(task_id = %id, "任务状态持久化失败: {e}");
            let record = match record {
                Some(record) => Some(record),
                None => self.store.task(id).await.ok(),
            };
            // 连发送阶段都无法读回时也保守待确认，不能用本地存储失败推断数据库回滚。
            let unknown = record.as_ref().is_none_or(|r| r.sent && r.may_write);
            let mut message = format!("任务已结束，但最终记录无法保存到本地存储：{e}");
            if unknown {
                message.push_str("；可能已发送的写入需人工核对，请勿直接重试");
            }
            let completion = UnpersistedCompletion {
                state: if unknown { "outcome_unknown" } else { "failed" },
                summary: match result {
                    Ok(value) => value.clone(),
                    Err(error) => (*error.details).clone(),
                },
                error: AppError::new("storage", message).with_details(json!({
                    "observed_state":state,
                    "execution_error":result.as_ref().err(),
                    "persistence_error":e,
                })),
            };
            final_state = completion.state;
            self.state
                .lock()
                .unwrap()
                .unpersisted
                .insert(id, completion);
        }
        self.emit_state(owner, id, final_state);
        self.state.lock().unwrap().tasks.remove(&id);
    }
    async fn start(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
        background: bool,
    ) -> Result<Value> {
        let db = self.database(owner, &params)?;
        let session_id: Uuid = model::param(&params, "session_id")?;
        let mut persisted_params = params.clone();
        redact(&mut persisted_params);
        let record = self
            .store
            .create_task(owner, workspace(&params), method, persisted_params, false)
            .await?;
        let id = record.id;
        let cancel = CancellationToken::new();
        // 每任务独占控制门；排队、真正发送与完成都必须和客户端离开时的取消决策串行化。
        let transition = Arc::new(AsyncMutex::new(()));
        {
            let mut state = self.state.lock().unwrap();
            if !state.clients.contains(&owner)
                || state
                    .databases
                    .get(&session_id)
                    .is_none_or(|db| db.closing.load(Ordering::Acquire))
            {
                cancel.cancel();
            }
            state.tasks.insert(
                id,
                Running {
                    owner,
                    session_id,
                    cancel: cancel.clone(),
                    transition: transition.clone(),
                    abort: None,
                },
            );
        }
        self.emit_state(owner, id, "queued");
        let engine = self.clone();
        let method = method.to_owned();
        let future = async move {
            let sink = ExecutionSink::new(engine.store.clone(), id)
                .with_dispatch_control(transition.clone(), cancel.clone());
            let result = if cancel.is_cancelled() {
                Err(AppError::cancelled())
            } else {
                engine
                    .provider(db, &method, params, &sink, &cancel, &transition)
                    .await
            };
            engine.finish_task(owner, id, &result).await;
            result
        };
        let task = tokio::spawn(future);
        if let Some(running) = self.state.lock().unwrap().tasks.get_mut(&id) {
            running.abort = Some(task.abort_handle());
        }
        // background 只决定 RPC 是否立即返回任务 ID，后台执行仍具有相同的归属和持久化语义。
        if background {
            Ok(json!({"task_id":id}))
        } else {
            task.await
                .map_err(|e| AppError::new("internal", format!("任务异常结束：{e}")))?
        }
    }
    async fn dispatch(self: &Arc<Self>, owner: Uuid, method: &str, params: Value) -> Result<Value> {
        self.active(owner)?;
        match method {
            "profile.list" => Ok(json!({"profiles":self.store.profiles().await?})),
            "profile.save" => {
                let profile: Profile = model::param(&params, "profile")?;
                profile.validate()?;
                let saved = self
                    .store
                    .save_profile(
                        &profile,
                        optional(&params, "secret")?,
                        params
                            .get("save_secret")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )
                    .await?;
                Ok(json!({"profile":profile,"credential_saved":saved}))
            }
            "profile.delete" => {
                let id: Uuid = model::param(&params, "profile_id")?;
                let lock = self.profile_lock(id);
                let _guard = lock.lock().await;
                if self
                    .state
                    .lock()
                    .unwrap()
                    .databases
                    .values()
                    .any(|db| db.profile.id == id)
                {
                    return Err(AppError::new("busy", "连接仍在使用，不能删除配置"));
                }
                self.store.delete_profile(id).await?;
                Ok(json!({"deleted":true}))
            }
            "profile.test" => {
                let profile: Profile = model::param(&params, "profile")?;
                profile.validate()?;
                let secret = match optional(&params, "secret")? {
                    Some(value) => Some(value),
                    None => self.store.secret(profile.id).await?,
                };
                let mut provider = ProviderSession::connect(profile, secret).await?;
                let record = self
                    .store
                    .create_task(owner, workspace(&params), "profile.test", json!({}), false)
                    .await?;
                self.emit_state(owner, record.id, "queued");
                self.store
                    .update_task(record.id, "running", Value::Null, None)
                    .await?;
                self.emit_state(owner, record.id, "running");
                let sink = ExecutionSink::new(self.store.clone(), record.id);
                let result = provider
                    .call(
                        "connection.info",
                        json!({}),
                        &sink,
                        &CancellationToken::new(),
                    )
                    .await;
                let _ = provider.close().await;
                self.finish_task(owner, record.id, &result).await;
                Ok(json!({"ok":true,"info":result?}))
            }
            "workspace.load" => {
                self.store
                    .workspace_load(&model::param::<String>(&params, "workspace_id")?)
                    .await
            }
            "workspace.save" => {
                let id: String = model::param(&params, "workspace_id")?;
                let revision = self
                    .store
                    .workspace_save(
                        &id,
                        model::param(&params, "expected_revision")?,
                        model::param(&params, "state")?,
                    )
                    .await?;
                Ok(json!({"revision":revision}))
            }
            "connection.open" => {
                let id: Uuid = model::param(&params, "profile_id")?;
                let lock = self.profile_lock(id);
                let _guard = lock.lock().await;
                let profile = self
                    .store
                    .profiles()
                    .await?
                    .into_iter()
                    .find(|p| p.id == id)
                    .ok_or_else(|| AppError::new("not_found", "连接配置不存在"))?;
                let secret = match optional(&params, "secret")? {
                    Some(s) => Some(s),
                    None => self.store.secret(id).await?,
                };
                let session = ProviderSession::connect(profile.clone(), secret).await?;
                let session_id = Uuid::new_v4();
                let db = Arc::new(Database {
                    owner,
                    profile: profile.clone(),
                    session: AsyncMutex::new(session),
                    closing: AtomicBool::new(false),
                });
                let inserted = {
                    let mut state = self.state.lock().unwrap();
                    if state.clients.contains(&owner) {
                        state.databases.insert(session_id, db.clone());
                        true
                    } else {
                        false
                    }
                };
                if !inserted {
                    let _ = db.session.lock().await.close().await;
                    return Err(AppError::new("disconnected", "客户端会话已关闭"));
                }
                Ok(
                    json!({"session_id":session_id,"profile":profile,"capabilities":{"browse":true,"query":true,"edit":profile.access_mode == AccessMode::ReadWrite,"import":profile.access_mode == AccessMode::ReadWrite,"export":true}}),
                )
            }
            "connection.close" => {
                let db = self.database(owner, &params)?;
                let id: Uuid = model::param(&params, "session_id")?;
                let tasks = {
                    let state = self.state.lock().unwrap();
                    if db.closing.swap(true, Ordering::AcqRel) {
                        return Err(AppError::new("busy", "数据库会话正在关闭"));
                    }
                    state
                        .tasks
                        .iter()
                        .filter(|(_, t)| t.session_id == id)
                        .map(|(id, t)| (*id, t.cancel.clone(), t.transition.clone()))
                        .collect::<Vec<_>>()
                };
                for (task_id, cancel, transition) in tasks {
                    self.cancel_if_safe(task_id, cancel, transition).await;
                }
                let result = db.session.lock().await.close().await;
                self.state.lock().unwrap().databases.remove(&id);
                result?;
                Ok(json!({"closed":true}))
            }
            "connection.info" | "resource.list" | "resource.describe" | "change.open"
            | "change.discard" | "transfer.preview" => {
                self.start(owner, method, params, false).await
            }
            "resource.browse" | "execution.start" | "change.submit" | "transfer.import"
            | "transfer.export" => self.start(owner, method, params, true).await,
            "execution.get" => serialize(self.task(model::param(&params, "task_id")?).await?),
            "execution.list" => {
                let mut tasks = self
                    .store
                    .tasks(&model::param::<String>(&params, "workspace_id")?)
                    .await?;
                let state = self.state.lock().unwrap();
                if !state.unpersisted.is_empty() {
                    for task in &mut tasks {
                        if let Some(completion) = state.unpersisted.get(&task.id) {
                            completion.apply(task);
                        }
                    }
                }
                drop(state);
                Ok(json!({"executions":tasks}))
            }
            "execution.cancel" => {
                let id: Uuid = model::param(&params, "task_id")?;
                let task = self.task(id).await?;
                if task.owner != owner {
                    return Err(AppError::new(
                        "permission_denied",
                        "不能取消其他客户端的任务",
                    ));
                }
                let running = self
                    .state
                    .lock()
                    .unwrap()
                    .tasks
                    .get(&id)
                    .map(|t| (t.cancel.clone(), t.transition.clone()));
                if let Some((cancel, transition)) = running {
                    cancel.cancel();
                    let _guard = transition.lock().await;
                    // 完成与取消共用控制门；取得门后重读，不能把已结束任务改回 cancelling。
                    let now = self.task(id).await?;
                    if matches!(now.state.as_str(), "queued" | "running" | "cancelling") {
                        self.store
                            .update_task(id, "cancelling", now.summary, None)
                            .await?;
                        if now.state != "cancelling" {
                            self.emit_state(owner, id, "cancelling");
                        }
                        Ok(json!({"task_id":id,"state":"cancelling"}))
                    } else {
                        Ok(json!({"task_id":id,"state":now.state}))
                    }
                } else {
                    Ok(json!({"task_id":id,"state":task.state}))
                }
            }
            "history.list" => {
                let mut history = self
                    .store
                    .history(&model::param::<String>(&params, "workspace_id")?)
                    .await?;
                let state = self.state.lock().unwrap();
                if !state.unpersisted.is_empty()
                    && let Some(items) = history["items"].as_array_mut()
                {
                    for item in items {
                        if let Some(completion) = item["id"]
                            .as_str()
                            .and_then(|id| Uuid::parse_str(id).ok())
                            .and_then(|id| state.unpersisted.get(&id))
                        {
                            item["state"] = completion.state.into();
                        }
                    }
                }
                drop(state);
                Ok(history)
            }
            "result.page" => serialize(
                self.store
                    .result_page(
                        model::param(&params, "result_id")?,
                        optional(&params, "start")?.unwrap_or(0),
                        optional::<usize>(&params, "max_rows")?
                            .unwrap_or(200)
                            .clamp(1, 1000),
                        optional::<usize>(&params, "max_bytes")?
                            .unwrap_or(1024 * 1024)
                            .clamp(1, 4 * 1024 * 1024),
                    )
                    .await?,
            ),
            "result.value" => {
                self.store
                    .large_value(
                        model::param(&params, "value_id")?,
                        optional(&params, "offset")?.unwrap_or(0),
                        optional::<usize>(&params, "max_bytes")?
                            .unwrap_or(1024 * 1024)
                            .clamp(1, 4 * 1024 * 1024),
                    )
                    .await
            }
            "result.release" => {
                let id: Uuid = model::param(&params, "result_id")?;
                let meta = self.store.result_meta(id).await?;
                if self
                    .state
                    .lock()
                    .unwrap()
                    .tasks
                    .contains_key(&meta.execution_id)
                {
                    return Err(AppError::new(
                        "busy",
                        "任务仍在写入结果，请在任务结束后释放",
                    ));
                }
                self.store.release_result(id).await?;
                Ok(json!({"released":true}))
            }
            "file.open" => {
                let path: String = model::param(&params, "path")?;
                let text = tokio::fs::read_to_string(&path).await?;
                Ok(json!({"path":path,"text":text}))
            }
            "file.save" => {
                let path: String = model::param(&params, "path")?;
                let text: String = model::param(&params, "text")?;
                tokio::fs::write(&path, &text).await?;
                Ok(json!({"path":path,"text":text}))
            }
            _ => Err(AppError::new(
                "method_not_found",
                format!("未知方法：{method}"),
            )),
        }
    }
}
