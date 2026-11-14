//! 执行结果、发送阶段控制和数据交换。

pub mod replace;
pub mod transfer;
use crate::{Result, Store, model::*};
use serde_json::Value as Json;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// 同步归属检查在共享发送门内执行，关闭方不能与实际发送交错。
type SendGate = (Arc<Mutex<()>>, Arc<dyn Fn() -> Result<()> + Send + Sync>);

/// 将一次执行的结果增量写入缓存，并在发送前记录可能的写入。
/// 克隆共享同一 Store；如需与取消操作同步，须配置相同的控制门和取消信号。
#[derive(Clone)]
pub struct ExecutionSink {
    pub store: Store,
    pub execution_id: Uuid,
    dispatch: Option<Arc<Mutex<()>>>,
    cancel: CancellationToken,
    profile: Option<(Uuid, u64, Arc<Mutex<()>>)>,
    profile_dispatched: Arc<AtomicBool>,
    send_gate: Option<SendGate>,
}
impl ExecutionSink {
    pub fn new(store: Store, execution_id: Uuid) -> Self {
        Self {
            store,
            execution_id,
            dispatch: None,
            cancel: CancellationToken::new(),
            profile: None,
            profile_dispatched: Arc::new(AtomicBool::new(false)),
            send_gate: None,
        }
    }
    /// 设置与取消方共享的状态转换锁和取消信号。
    pub fn with_dispatch_control(
        mut self,
        transition: Arc<Mutex<()>>,
        cancel: CancellationToken,
    ) -> Self {
        self.dispatch = Some(transition);
        self.cancel = cancel;
        self
    }
    /// 发送首个请求时与配置更新串行化并验证会话快照。
    pub fn with_profile_revision(mut self, id: Uuid, revision: u64, lock: Arc<Mutex<()>>) -> Self {
        self.profile = Some((id, revision, lock));
        self
    }
    /// 与会话关闭共享发送门，并在门内校验会话归属。
    pub fn with_send_gate(
        mut self,
        gate: Arc<Mutex<()>>,
        check: Arc<dyn Fn() -> Result<()> + Send + Sync>,
    ) -> Self {
        self.send_gate = Some((gate, check));
        self
    }
    /// 创建尚未完成的结果并返回其 ID。
    pub async fn start(&self, columns: Vec<Column>, kind: &str) -> Result<Uuid> {
        self.store
            .start_result(self.execution_id, columns, kind)
            .await
    }
    /// 向结果追加类型化行；达到内存预算时返回错误。
    pub async fn append(&self, result_id: Uuid, rows: Vec<Vec<Value>>) -> Result<()> {
        self.store.append_rows(result_id, rows).await
    }
    /// 等待消费者请求更多行；释放、预算耗尽或取消时返回 false。
    pub async fn wait_demand(
        &self,
        result_id: Uuid,
        produced: u64,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        self.store
            .wait_result_demand(result_id, produced, cancel)
            .await
    }
    /// 标记结果完整并保存摘要。
    pub async fn finish(&self, result_id: Uuid, summary: Json) -> Result<()> {
        self.store.finish_result(result_id, summary).await
    }
    /// 在控制门内检查取消并持久化发送阶段，调用方应在实际发送前等待成功。
    /// `may_write` 一旦为真，后续调用不会将其清除。
    pub async fn mark_sent(&self, may_write: bool) -> Result<()> {
        let _guard = match &self.dispatch {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        ensure_not_cancelled(&self.cancel)?;
        let _profile_guard = match &self.profile {
            Some((id, revision, lock)) if !self.profile_dispatched.load(Ordering::Acquire) => {
                let guard = lock.lock().await;
                if !self.profile_dispatched.load(Ordering::Acquire)
                    && self.store.profile(*id).await?.revision != *revision
                {
                    return Err(AppError::new("invalidated", "连接设置已变化，请重新连接"));
                }
                Some(guard)
            }
            _ => None,
        };
        let _send_guard = match &self.send_gate {
            Some((gate, check)) => {
                let guard = gate.lock().await;
                check()?;
                Some(guard)
            }
            None => None,
        };
        ensure_not_cancelled(&self.cancel)?;
        self.store.mark_sent(self.execution_id, may_write).await?;
        self.profile_dispatched.store(true, Ordering::Release);
        Ok(())
    }
    /// 记录已校验的完整响应；即使取消已触发，也保留已知远端完成事实。
    pub async fn mark_remote_completed(&self) -> Result<()> {
        let _guard = match &self.dispatch {
            Some(lock) => Some(lock.lock().await),
            None => None,
        };
        self.store
            .mark_remote_completed(self.execution_id)
            .await
            .map_err(|error| {
                AppError::new("storage", "平台已返回完整响应，但本地完成阶段保存失败").with_details(
                    serde_json::json!({"remote_completion":"completed","persistence_error":error}),
                )
            })
    }
}
