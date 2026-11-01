//! Engine 会话、数据库连接与任务的唯一运行状态所有者。
//!
//! `State` 的同步锁只保护注册表与短状态迁移；数据库会话的异步锁用于串行化
//! 同一物理连接上的命令。`transition` 则把取消检查、发送阶段持久化与完成记录
//! 放在同一控制门内，不能用注册表锁替代，也不能把取消等待解释为撤销写入。

use crate::{EngineEvent, platform};
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
use tokio::sync::{Mutex as AsyncMutex, broadcast};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod session;
use session::client;
#[cfg(test)]
use session::client_with_lease;

#[cfg(test)]
#[path = "../tests/unit/server.rs"]
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
            let wall = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| AppError::new("internal", e.to_string()))?
                .as_secs_f64();
            let instant = Instant::now();
            self.store
                .update_task(
                    sink.execution_id,
                    "running",
                    json!({"_task":{"started_at":wall}}),
                    None,
                )
                .await?;
            if let Some(running) = self.state.lock().unwrap().tasks.get_mut(&sink.execution_id) {
                running.started = Some(instant);
            }
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
    fn apply_live_elapsed(&self, task: &mut model::TaskRecord) {
        if let Some(started) = self
            .state
            .lock()
            .unwrap()
            .tasks
            .get(&task.id)
            .and_then(|r| r.started)
        {
            task.elapsed_ms = Some(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        }
    }
    async fn task(&self, id: Uuid) -> Result<model::TaskRecord> {
        let mut task = self.store.task(id).await?;
        if let Some(completion) = self.state.lock().unwrap().unpersisted.get(&id) {
            completion.apply(&mut task);
        }
        self.apply_live_elapsed(&mut task);
        Ok(task)
    }
    async fn finish_task(
        &self,
        owner: Uuid,
        id: Uuid,
        result: &Result<Value>,
        started: Option<Instant>,
    ) {
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
        let record = self.store.task(id).await.ok();
        let (state, summary, error) = match result {
            Ok(value) => ("succeeded", value.clone(), None),
            Err(error) => {
                // change.submit 的 rolled_back 仅在事务 ROLLBACK 得到确认后产生。
                let rolled_back = error.code == "cancelled"
                    && record.as_ref().is_some_and(|r| r.method == "change.submit")
                    && error
                        .details
                        .get("items")
                        .and_then(Value::as_array)
                        .is_some_and(|items| {
                            !items.is_empty()
                                && items.iter().enumerate().all(|(index, item)| {
                                    item.get("index").and_then(Value::as_u64) == Some(index as u64)
                                        && item.get("status").and_then(Value::as_str)
                                            == Some("rolled_back")
                                })
                        });
                let unknown = record.as_ref().is_some_and(|r| r.sent && r.may_write)
                    && (error.code == "cancelled"
                        || error.code == "disconnected"
                        || error.code == "outcome_unknown")
                    && !rolled_back;
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
        let mut summary = match summary {
            Value::Object(map) => Value::Object(map),
            Value::Null => json!({}),
            other => json!({"value":other}),
        };
        let mut metadata = record
            .as_ref()
            .and_then(|r| r.summary.get("_task"))
            .cloned()
            .unwrap_or_else(|| json!({}));
        if let Some(started) = started {
            metadata["elapsed_ms"] =
                json!(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        }
        summary["_task"] = metadata;
        let completion_summary = summary.clone();
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
                summary: completion_summary,
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
        let context = json!({"connection":db.profile.name,"provider":db.profile.provider,
            "database":if matches!(method, "execution.start" | "execution.run") { None } else { params.pointer("/target/database").and_then(Value::as_str).or_else(|| params.get("database").and_then(Value::as_str)).or(db.profile.database.as_deref()) },
            "redis_db":if db.profile.provider == model::ProviderKind::Redis { Some(db.profile.redis_db) } else { None },
            "targets":[]});
        if let Some(object) = persisted_params.as_object_mut() {
            object.insert("_task_context".into(), context);
        }
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
                    started: None,
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
            let started = engine
                .state
                .lock()
                .unwrap()
                .tasks
                .get(&id)
                .and_then(|task| task.started);
            engine.finish_task(owner, id, &result, started).await;
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
        let owner_cancel = self
            .state
            .lock()
            .unwrap()
            .client_cancels
            .get(&owner)
            .cloned()
            .ok_or_else(|| AppError::new("disconnected", "客户端会话已关闭"))?;
        match method {
            "settings.get" => serialize(self.store.settings().await?),
            "settings.set" => {
                let settings: model::AppSettings = model::param(&params, "settings")?;
                self.store.set_settings(&settings).await?;
                self.emit_settings_changed();
                serialize(settings)
            }
            "profile.databases" | "profile.test" => {
                let profile: Profile = model::param(&params, "profile")?;
                profile.validate()?;
                if method == "profile.databases" && profile.provider != model::ProviderKind::Mysql {
                    return Err(AppError::invalid("仅 MySQL 连接支持加载数据库"));
                }
                let secret = optional(&params, "secret")?;
                let mut provider = tokio::select! {
                    biased;
                    _ = owner_cancel.cancelled() => return Err(AppError::cancelled()),
                    result = async {
                        let secret = match secret {
                            Some(value) => Some(value),
                            None => self.store.secret(profile.id).await?,
                        };
                        ProviderSession::connect(profile.clone(), secret).await
                    } => result?,
                };
                let record = self
                    .store
                    .create_task(owner, workspace(&params), method, json!({"_task_context":{"connection":profile.name,"provider":profile.provider,"database":profile.database,"redis_db":if profile.provider == model::ProviderKind::Redis { Some(profile.redis_db) } else { None },"targets":[]}}), false)
                    .await?;
                self.emit_state(owner, record.id, "queued");
                let mut started = None;
                let result = async {
                    let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_err(|e| AppError::new("internal", e.to_string()))?.as_secs_f64();
                    let instant = Instant::now();
                    self.store.update_task(record.id, "running", json!({"_task":{"started_at":wall}}), None).await?;
                    started = Some(instant);
                    self.emit_state(owner, record.id, "running");
                    let sink = ExecutionSink::new(self.store.clone(), record.id);
                    tokio::select! {
                        biased;
                        _ = owner_cancel.cancelled() => Err(AppError::cancelled()),
                        result = provider.call(
                            if method == "profile.test" { "connection.info" } else { "resource.list" },
                            json!({}), &sink, &owner_cancel,
                        ) => result,
                    }
                }.await;
                // 已中断的只读连接直接丢弃，避免关闭握手再次阻塞断线清理。
                if !owner_cancel.is_cancelled() {
                    tokio::select! {
                        _ = owner_cancel.cancelled() => (),
                        _ = provider.close() => (),
                    }
                }
                self.finish_task(owner, record.id, &result, started).await;
                if method == "profile.test" {
                    Ok(json!({"ok":true,"info":result?}))
                } else {
                    result
                }
            }
            "profile.list" => Ok(json!({"profiles":self.store.profiles().await?})),
            "profile.save" => {
                let profile: Profile = model::param(&params, "profile")?;
                profile.validate()?;
                let secret = optional(&params, "secret")?;
                let persist = params
                    .get("save_secret")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let engine = self.clone();
                // 保存任务独占连接配置锁直到凭据交接完成；调用方停止等待不能提前放行打开连接。
                tokio::spawn(async move {
                    let lock = engine.profile_lock(profile.id);
                    let _guard = lock.lock().await;
                    let saved = engine.store.save_profile(&profile, secret, persist).await?;
                    Ok(json!({"profile":profile,"credential_saved":saved}))
                })
                .await
                .map_err(|e| AppError::new("internal", format!("保存连接配置任务异常结束：{e}")))?
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
                let _guard = tokio::select! {
                    biased;
                    _ = owner_cancel.cancelled() => return Err(AppError::cancelled()),
                    guard = lock.lock() => guard,
                };
                // 只取消只读配置和连接等待；profile.save/delete 的凭据交接必须完成。
                let (profile, session) = tokio::select! {
                    biased;
                    _ = owner_cancel.cancelled() => return Err(AppError::cancelled()),
                    result = async {
                        let mut profile = self.store.profiles().await?.into_iter()
                            .find(|p| p.id == id)
                            .ok_or_else(|| AppError::new("not_found", "连接配置不存在"))?;
                        if let Some(redis_db) = optional::<u32>(&params, "redis_db")? {
                            if profile.provider != model::ProviderKind::Redis {
                                return Err(AppError::invalid("仅 Redis 连接支持指定 DB"));
                            }
                            profile.redis_db = redis_db;
                        }
                        let secret = match optional(&params, "secret")? {
                            Some(s) => Some(s),
                            None => self.store.secret(id).await?,
                        };
                        let session = ProviderSession::connect(profile.clone(), secret).await?;
                        Ok::<_, AppError>((profile, session))
                    } => result?,
                };
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
                    // owner 已离开，直接丢弃新建会话，不能在 profile 锁内等待关闭握手。
                    drop(db);
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
            | "change.discard" | "change.preview" | "transfer.preview" => {
                self.start(owner, method, params, false).await
            }
            "resource.browse" | "execution.start" | "change.submit" | "transfer.import"
            | "transfer.export" => self.start(owner, method, params, true).await,
            "execution.get" => {
                let id = model::param(&params, "task_id")?;
                let include_text = optional::<bool>(&params, "include_text")?.unwrap_or(false);
                let mut task = if include_text {
                    self.store.task_details(id).await?
                } else {
                    self.store.task(id).await?
                };
                if let Some(completion) = self.state.lock().unwrap().unpersisted.get(&id) {
                    completion.apply(&mut task);
                }
                self.apply_live_elapsed(&mut task);
                serialize(task)
            }
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
                for task in &mut tasks {
                    self.apply_live_elapsed(task);
                }
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
                match self.store.result_meta(id).await {
                    Ok(meta) => {
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
                    }
                    Err(error) if error.code == "not_found" => {}
                    Err(error) => return Err(error),
                }
                self.store.release_result(id).await?;
                Ok(json!({"released":true}))
            }
            "file.open" => {
                let path: String = model::param(&params, "path")?;
                if path.trim().is_empty() {
                    return Err(AppError::invalid("文件路径不能为空"));
                }
                let text = tokio::fs::read_to_string(&path).await?;
                Ok(json!({"path":path,"text":text}))
            }
            "file.save" => {
                let path: String = model::param(&params, "path")?;
                if path.trim().is_empty() {
                    return Err(AppError::invalid("文件路径不能为空"));
                }
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
