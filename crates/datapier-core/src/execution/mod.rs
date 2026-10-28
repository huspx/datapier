//! 执行结果落盘、发送阶段控制和数据交换。

pub mod transfer;
use crate::{Result, Store, model::*};
use serde_json::Value as Json;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 将一次执行的结果增量写入缓存，并在发送前记录可能的写入。
/// 克隆共享同一 Store；如需与取消操作同步，须配置相同的控制门和取消信号。
#[derive(Clone)]
pub struct ExecutionSink {
    pub store: Store,
    pub execution_id: Uuid,
    dispatch: Option<Arc<Mutex<()>>>,
    cancel: CancellationToken,
}
impl ExecutionSink {
    pub fn new(store: Store, execution_id: Uuid) -> Self {
        Self {
            store,
            execution_id,
            dispatch: None,
            cancel: CancellationToken::new(),
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
    /// 创建尚未完成的结果并返回其 ID。
    pub async fn start(&self, columns: Vec<Column>, kind: &str) -> Result<Uuid> {
        self.store
            .start_result(self.execution_id, columns, kind)
            .await
    }
    /// 向结果追加类型化行；写盘失败会返回错误。
    pub async fn append(&self, result_id: Uuid, rows: Vec<Vec<Value>>) -> Result<()> {
        self.store.append_rows(result_id, rows).await
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
        self.store.mark_sent(self.execution_id, may_write).await
    }
}
