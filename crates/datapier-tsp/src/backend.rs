use datapier_core::{Result, model::AppError};
use datapier_engine::{Client, EngineEvent};
use serde_json::Value;
use std::{path::PathBuf, thread};
use tern_sdk::term::Waker;
use tokio::sync::{broadcast, mpsc};

/// 发往独立 IPC 线程的请求；tag 用于将异步响应路由回发起方。
pub struct Request {
    pub tag: String,
    pub method: String,
    pub params: Value,
}
/// IPC 请求的结果或 Engine 事件，使用 tag 区分来源。
pub struct Response {
    pub tag: String,
    pub result: Result<Value>,
}

/// UI 线程与 Tokio IPC 线程之间的有界队列桥接，不持有 TSP Session。
/// 请求和响应都可能因队列关闭或容量限制失败；waker 只通知 UI 检查队列。
pub struct Backend {
    commands: mpsc::Sender<Request>,
    responses: mpsc::Receiver<Response>,
    thread: Option<thread::JoinHandle<()>>,
}

async fn consume(
    incoming: &mut mpsc::Receiver<Request>,
    events: &mut broadcast::Receiver<EngineEvent>,
    responses: &mpsc::Sender<Response>,
    mut dispatch: impl FnMut(Request),
    mut wake: impl FnMut(),
) {
    loop {
        tokio::select! {
            request = incoming.recv() => {
                let Some(request) = request else { break };
                dispatch(request);
            }
            event = events.recv() => match event {
                Ok(event) => {
                    if let Ok(value) = serde_json::to_value(event) {
                        if responses.send(Response {
                            tag: "execution.event".into(),
                            result: Ok(value),
                        }).await.is_err() {
                            break;
                        }
                        wake();
                    }
                }
                // 事件可能被丢弃；任务状态以 execution.get 和周期轮询为准。
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    }
}

impl Backend {
    /// 启动后台 runtime 和 Engine 连接；线程创建失败时返回错误。
    /// 连接和 runtime 的异步失败通过响应队列报告。
    pub fn start(root: PathBuf, waker: Waker) -> Result<Self> {
        let (commands, mut incoming) = mpsc::channel::<Request>(64);
        let (responses, receiver) = mpsc::channel::<Response>(64);
        let worker = thread::Builder::new()
            .name("datapier-ipc".into())
            .spawn(move || {
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(error) => {
                        let _ = responses.blocking_send(Response {
                            tag: "connect".into(),
                            result: Err(AppError::new("runtime", error.to_string())),
                        });
                        waker.wake();
                        return;
                    }
                };
                runtime.block_on(async move {
                    let client = match Client::connect(&root).await {
                        Ok(client) => client,
                        Err(error) => {
                            let _ = responses
                                .send(Response {
                                    tag: "connect".into(),
                                    result: Err(error),
                                })
                                .await;
                            waker.wake();
                            return;
                        }
                    };
                    let mut events = client.subscribe_events();

                    if responses
                        .send(Response {
                            tag: "connect".into(),
                            result: Ok(Value::Null),
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                    waker.wake();
                    consume(
                        &mut incoming,
                        &mut events,
                        &responses,
                        |request| {
                            let client = client.clone();
                            let responses = responses.clone();
                            let waker = waker.clone();
                            tokio::spawn(async move {
                                let result = client.request(&request.method, request.params).await;
                                if responses
                                    .send(Response {
                                        tag: request.tag,
                                        result,
                                    })
                                    .await
                                    .is_ok()
                                {
                                    waker.wake();
                                }
                            });
                        },
                        || waker.wake(),
                    )
                    .await;
                    let _ = client.close().await;
                    drop(responses);
                    waker.wake();
                });
            })
            .map_err(|e| AppError::new("runtime", e.to_string()))?;
        Ok(Self {
            commands,
            responses: receiver,
            thread: Some(worker),
        })
    }
    #[cfg(test)]
    pub(crate) fn scripted() -> (Self, mpsc::Receiver<Request>, mpsc::Sender<Response>) {
        let (commands, incoming) = mpsc::channel(64);
        let (responses, receiver) = mpsc::channel(64);
        (
            Self {
                commands,
                responses: receiver,
                thread: None,
            },
            incoming,
            responses,
        )
    }
    /// 非阻塞入队，队列已满或关闭时返回 ipc_queue 错误。
    pub fn request(&self, tag: impl Into<String>, method: &str, params: Value) -> Result<()> {
        self.commands
            .try_send(Request {
                tag: tag.into(),
                method: method.into(),
                params,
            })
            .map_err(|e| AppError::new("ipc_queue", e.to_string()))
    }
    /// 非阻塞读取；None 可能表示暂时为空，须另用 is_closed 判断关闭。
    pub fn receive(&mut self) -> Option<Response> {
        self.responses.try_recv().ok()
    }
    /// 等待下一个响应；发送端全部关闭且队列耗尽时返回 None。
    pub fn receive_blocking(&mut self) -> Option<Response> {
        self.responses.blocking_recv()
    }
    pub fn is_closed(&self) -> bool {
        self.responses.is_closed()
    }
}

impl Drop for Backend {
    fn drop(&mut self) {
        // 先关闭接收端，使等待写入已满响应队列的任务能够退出。
        self.responses.close();
        let (replacement, _) = mpsc::channel(1);
        drop(std::mem::replace(&mut self.commands, replacement));
        // 数据库请求可能仍在运行，不能在 UI 线程等待后台线程结束。
        let _ = self.thread.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn closed_events_end_consumer_and_allow_unsaved_workspace_to_fail() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (commands, mut incoming) = mpsc::channel(1);
        let (responses, receiver) = mpsc::channel(1);
        let (event_sender, mut events) = broadcast::channel(1);
        let mut backend = Backend {
            commands,
            responses: receiver,
            thread: None,
        };
        drop(event_sender);
        runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(1),
                consume(&mut incoming, &mut events, &responses, |_| {}, || {}),
            )
            .await
            .expect("event closure must stop the consumer while commands remain open");
        });
        drop(responses);

        let mut app = crate::App::new("test".into());
        app.dirty = true;
        let error = crate::finish_after_input_closed(&mut app, &mut backend).unwrap_err();
        assert_eq!(error.code, "workspace_save");
    }
}
