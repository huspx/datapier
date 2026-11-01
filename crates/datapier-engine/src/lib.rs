//! 共享 Engine 的 IPC 客户端与运行入口。
//!
//! 一个客户端对应一个带租约的业务会话，clone 共享该会话及请求分发器。
//! 数据库连接与任务留在 Engine；客户端断连只终止本地等待，不重放已经发送的操作。

mod platform;
mod protocol;
mod server;

use datapier_core::{AppError, Result};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 所属客户端的任务状态通知，不承载 SQL、凭据或结果行。
///
/// 通知可能先于启动请求的响应到达，也可能因订阅背压而丢失；消费者应按
/// `task_id` 拉取任务状态，以查询结果为准，不能据通知重发数据库操作。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EngineEvent {
    /// 当前 Engine 实例内递增；其他客户端的事件会造成序号空洞，重启后不延续。
    pub seq: u64,
    pub kind: String,
    pub task_id: Uuid,
    pub payload: Value,
}

/// 返回平台约定的用户数据目录；此处不创建目录或验证其访问权限。
pub fn default_data_dir() -> Result<PathBuf> {
    directories::ProjectDirs::from("", "", "datapier")
        .map(|p| p.data_local_dir().to_owned())
        .ok_or_else(|| AppError::new("io", "无法确定用户数据目录"))
}
/// 以指定数据目录运行共享 Engine，直到无人会话的收尾期限结束。
///
/// 返回目录、存储或 IPC 错误；已有实例持锁时，候选进程正常退出。
/// 因此成功返回不代表新实例已启动，客户端仍须通过连接与握手发现服务。
pub async fn run(root: PathBuf) -> Result<()> {
    server::run(root).await
}

struct Inner {
    send: mpsc::Sender<Value>,
    pending: Mutex<HashMap<String, oneshot::Sender<Result<Value>>>>,
    shutdown: CancellationToken,
    closed: AtomicBool,
    events: Mutex<Option<broadcast::Sender<EngineEvent>>>,
}
// 请求 future 被取消时也要移除关联项，避免永远收不到响应的等待者占用分发器。
// 这里只清理本地等待；已经入队或发送给 Engine 的数据库操作不会因此撤销。
struct PendingRequest {
    inner: Arc<Inner>,
    id: String,
}
impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.inner.pending.lock().unwrap().remove(&self.id);
    }
}
// 由 Arc 在最后一个句柄释放时调用，避免并发 Drop 各自观察到非唯一引用。
struct ClientLifetime(CancellationToken);
impl Drop for ClientLifetime {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
/// 一个可并发请求的 Engine 会话，所有 clone 共享请求 ID 空间、事件流和租约。
///
/// 最后一个客户端句柄释放会停止本地 I/O 与心跳；单独持有事件 receiver 不保活会话。
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
    lifetime: Arc<ClientLifetime>,
}
impl Client {
    /// 连接并握手；必要时启动使用同一可执行文件的 Engine 候选进程。
    ///
    /// 数据目录必须通过当前用户权限检查。启动竞态由 Engine 的 OS 文件锁裁决，
    /// 只探测 socket 或描述文件不能证明实例有效；协议不兼容时直接报错，不强杀旧实例。
    pub async fn connect(root: &Path) -> Result<Self> {
        let root = platform::root(root)?;
        let endpoint = platform::endpoint(&root)?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        let mut next_spawn = tokio::time::Instant::now();
        loop {
            let error = match platform::connect(&endpoint).await {
                Ok(stream) => {
                    match tokio::time::timeout(Duration::from_secs(5), Self::handshake(stream))
                        .await
                    {
                        Ok(Ok(client)) => return Ok(client),
                        Ok(Err(error)) if error.code == "protocol_incompatible" => {
                            return Err(error);
                        }
                        Ok(Err(error)) => error,
                        Err(_) => AppError::new("disconnected", "Engine 握手超时"),
                    }
                }
                Err(error) => error,
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(AppError::new(
                    "connection_failed",
                    format!("无法连接 Engine：{error}"),
                ));
            }
            if tokio::time::Instant::now() >= next_spawn {
                platform::spawn(&root)?;
                next_spawn = tokio::time::Instant::now() + Duration::from_millis(500);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
    async fn handshake(stream: platform::Stream) -> Result<Self> {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::channel::<Value>(128);
        let inner = Arc::new(Inner {
            send: tx,
            pending: Mutex::new(HashMap::new()),
            shutdown: CancellationToken::new(),
            closed: AtomicBool::new(false),
            events: Mutex::new(Some(broadcast::channel(128).0)),
        });
        let writer_inner = inner.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = writer_inner.shutdown.cancelled() => break,
                    message = rx.recv() => match message {
                        Some(message) => {
                            // 写入取消后帧可能不完整，立即关闭通道，不能继续发送下一帧。
                            tokio::select! {
                                biased;
                                _ = writer_inner.shutdown.cancelled() => break,
                                result = protocol::write(&mut writer, &message) => if let Err(error) = result {
                                    if error.code != "invalid_input" {
                                        break;
                                    }
                                    // 编码或超限错误发生在写入帧头前；请求可能已取消。
                                    if let Some(id) = message.get("id").and_then(Value::as_str)
                                        && let Some(tx) = writer_inner.pending.lock().unwrap().remove(id)
                                    {
                                        let _ = tx.send(Err(error));
                                    }
                                },
                            }
                        }
                        None => break,
                    },
                }
            }
            writer_inner.shutdown.cancel();
        });
        let reader_inner = inner.clone();
        tokio::spawn(async move {
            loop {
                let message = tokio::select! { _ = reader_inner.shutdown.cancelled() => break, response = protocol::read(&mut reader) => match response { Ok(v) => v, Err(_) => break } };
                // 事件不占用请求 ID；取消请求后到达的响应也不能完成其他等待者。
                if message.get("id").is_none() {
                    if let Ok(event) = serde_json::from_value::<EngineEvent>(message)
                        && let Some(events) = reader_inner.events.lock().unwrap().as_ref()
                    {
                        let _ = events.send(event);
                    }
                    continue;
                }
                if let Some(id) = message.get("id").and_then(Value::as_str)
                    && let Some(tx) = reader_inner.pending.lock().unwrap().remove(id)
                {
                    let value = if let Some(e) = message.get("error") {
                        serde_json::from_value::<AppError>(e.clone())
                            .map_or_else(|e| Err(AppError::invalid(e.to_string())), Err)
                    } else {
                        Ok(message.get("result").cloned().unwrap_or(Value::Null))
                    };
                    let _ = tx.send(value);
                }
            }
            reader_inner.shutdown.cancel();
            reader_inner.events.lock().unwrap().take();
            for (_, tx) in reader_inner.pending.lock().unwrap().drain() {
                let _ = tx.send(Err(AppError::new("disconnected", "Engine 连接已断开")));
            }
        });
        let client = Self {
            lifetime: Arc::new(ClientLifetime(inner.shutdown.clone())),
            inner,
        };
        let handshake = match client
            .request("session.open", json!({"protocol_version":1}))
            .await
        {
            Ok(value) => value,
            Err(e) => {
                client.inner.shutdown.cancel();
                return Err(e);
            }
        };
        if handshake.get("protocol_version").and_then(Value::as_u64) != Some(1) {
            client.inner.shutdown.cancel();
            return Err(AppError::new(
                "protocol_incompatible",
                "Engine 协议版本不兼容",
            ));
        }
        // I/O worker 会持有 Inner；另用弱 lifetime 判断外部句柄是否仍在，避免心跳自保活。
        let weak = Arc::downgrade(&client.inner);
        let lifetime = Arc::downgrade(&client.lifetime);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                // 不升级 lifetime：心跳不能使最后一个外部句柄的 Drop 错过关闭连接。
                if lifetime.strong_count() == 0 || inner.shutdown.is_cancelled() {
                    break;
                }
                if Client::request_inner(&inner, "session.heartbeat", json!({}))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Ok(client)
    }
    /// 订阅此客户端后续的任务状态通知，不回放此前事件。
    ///
    /// 缓冲有界，`Lagged` 后应查询状态补齐；断连返回关闭的 receiver。
    /// 订阅本身不延长客户端生命周期，也不能代替心跳。
    pub fn subscribe_events(&self) -> broadcast::Receiver<EngineEvent> {
        let events = self.inner.events.lock().unwrap();
        match events.as_ref() {
            Some(events) if !self.inner.shutdown.is_cancelled() => events.subscribe(),
            _ => {
                let (sender, receiver) = broadcast::channel(1);
                drop(sender);
                receiver
            }
        }
    }
    /// 发送一个请求并等待同 ID 的响应；服务端业务错误保留原始错误内容。
    ///
    /// 超过 8 MiB 的序列化请求在发送前以 `invalid_input` 拒绝，不影响共享会话。
    /// 取消此 future 只停止本地等待，不保证请求未发送或数据库未执行。
    /// 连接错误尤其不能作为重试写入的依据；耗时任务应使用返回的任务 ID 查询结果。
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        Self::request_inner(&self.inner, method, params).await
    }
    async fn request_inner(inner: &Arc<Inner>, method: &str, params: Value) -> Result<Value> {
        let id = Uuid::new_v4().to_string();
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = inner.pending.lock().unwrap();
            if inner.shutdown.is_cancelled() || inner.closed.load(Ordering::Acquire) {
                return Err(AppError::new("disconnected", "Engine 连接已关闭"));
            }
            pending.insert(id.clone(), tx);
        }
        let _pending = PendingRequest {
            inner: inner.clone(),
            id: id.clone(),
        };
        tokio::select! {
            biased;
            sent = inner.send.send(json!({"id":id,"method":method,"params":params})) => if sent.is_err() { return Err(AppError::new("disconnected", "Engine 连接已断开")); },
            _ = inner.shutdown.cancelled() => return Err(AppError::new("disconnected", "Engine 连接已断开")),
        }
        // 服务端可能在发送关闭确认后立即断开；两者同时就绪时必须优先交付已收到的响应。
        tokio::select! {
            biased;
            result = rx => result.map_err(|_| AppError::new("disconnected", "Engine 连接已断开"))?,
            _ = inner.shutdown.cancelled() => Err(AppError::new("disconnected", "Engine 连接已断开")),
        }
    }
    /// 请求释放整个共享会话并停止本地 I/O，影响本句柄的全部 clone。
    ///
    /// 确认失败仍关闭本地通道；Engine 按 EOF／租约处理所属资源，已发送写入不因此回滚。
    pub async fn close(&self) -> Result<()> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Ok(());
        }
        let result = self.request("session.close", json!({})).await.map(|_| ());
        self.inner.closed.store(true, Ordering::Release);
        self.inner.shutdown.cancel();
        result
    }
}

#[cfg(test)]
#[path = "../tests/unit/client.rs"]
mod tests;
