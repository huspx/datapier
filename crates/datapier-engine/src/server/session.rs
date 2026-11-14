//! IPC 会话握手、租约与响应队列。

use super::{Engine, platform};
use crate::protocol;
use datapier_core::{AppError, Result};
use serde_json::{Value, json};
use std::{sync::Arc, time::Duration};
use tokio::sync::{broadcast, mpsc};
use uuid::Uuid;

pub(super) async fn client(engine: Arc<Engine>, owner: Uuid, stream: platform::Stream) {
    client_with_lease(engine, owner, stream, Duration::from_secs(45), 128).await;
}

pub(super) async fn client_with_lease(
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
            != Some(protocol::VERSION)
        {
            Err(AppError::new(
                "protocol_incompatible",
                "Engine 协议版本不兼容",
            ))
        } else {
            let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
            let context = (|| -> Result<super::ClientContext> {
                Ok(super::ClientContext {
                    kind: datapier_core::model::param(&params, "client_kind")?,
                    workspace: datapier_core::model::param(&params, "workspace")?,
                    allow_connections: datapier_core::model::param(&params, "allow_connections")?,
                })
            })();
            match context {
                Err(error) => Err(error),
                Ok(context) => {
                    if !engine.register_with(owner, context) {
                        Err(AppError::new("disconnected", "Engine 正在退出"))
                    } else {
                        opened = true;
                        events =
                            Some(engine.state.lock().unwrap().event_senders[&owner].subscribe());
                        match engine.store.settings().await {
                            Ok(settings) => Ok(
                                json!({"protocol_version":protocol::VERSION,"engine_id":engine.id,"client_session_id":owner,"settings_revision":settings.revision,"heartbeat_seconds":10,"lease_seconds":45,"grace_seconds":30}),
                            ),
                            Err(error) => Err(error),
                        }
                    }
                }
            }
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
