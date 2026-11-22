//! 执行结果、发送阶段控制和数据交换。

pub mod data_files;
pub mod migration;
pub mod replace;
pub mod sql_restore;
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
    transfer_profiles: Vec<(Uuid, u64, Arc<Mutex<()>>)>,
    send_gate: Option<SendGate>,
    transfer_step: Option<(Uuid, u64)>,
    owner: Option<Uuid>,
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
            transfer_profiles: Vec::new(),
            send_gate: None,
            transfer_step: None,
            owner: None,
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
    /// 迁移在每个发送边界检查全部冻结配置，避免长事务占用配置门。
    pub fn with_transfer_profiles(mut self, profiles: Vec<(Uuid, u64, Arc<Mutex<()>>)>) -> Self {
        self.transfer_profiles = profiles;
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
    /// 绑定迁移步骤，使发送事实和逐项日志在同一持久化事务中提交。
    pub fn with_transfer_step(mut self, run_id: Uuid, ordinal: u64) -> Self {
        self.transfer_step = Some((run_id, ordinal));
        self
    }
    pub fn with_owner(mut self, owner: Uuid) -> Self {
        self.owner = Some(owner);
        self
    }
    async fn transfer_owner(&self) -> Result<Uuid> {
        match self.owner {
            Some(owner) => Ok(owner),
            None => Ok(self.store.task(self.execution_id).await?.owner),
        }
    }
    /// 临时资源在首次远端创建之前登记；登记失败时调用方不能发送创建命令。
    pub async fn register_transfer_artifact(&self, kind: &str, identity: Json) -> Result<Uuid> {
        self.store
            .transfer_register_artifact(
                self.transfer_owner().await?,
                self.transfer_step.map(|step| step.0),
                kind,
                &identity,
            )
            .await
    }
    /// 标记轮换先更新候选身份，确保响应丢失后仍可验证临时对象归属。
    pub async fn update_transfer_artifact(&self, id: Uuid, identity: Json) -> Result<()> {
        let current = self.store.transfer_artifact(id).await?;
        self.store
            .transfer_update_artifact(
                id,
                self.transfer_owner().await?,
                Some(current.revision),
                "ready",
                Some(&identity),
            )
            .await?;
        Ok(())
    }
    pub async fn finish_transfer_artifact(&self, id: Uuid, state: &str) -> Result<()> {
        let current = self.store.transfer_artifact(id).await?;
        self.store
            .transfer_update_artifact(
                id,
                self.transfer_owner().await?,
                Some(current.revision),
                state,
                None,
            )
            .await?;
        Ok(())
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
    pub(crate) fn check_cancelled(&self) -> Result<()> {
        ensure_not_cancelled(&self.cancel)
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
        let mut _transfer_guards = Vec::with_capacity(self.transfer_profiles.len());
        for (id, revision, lock) in &self.transfer_profiles {
            let guard = lock.lock().await;
            if self.store.profile(*id).await?.revision != *revision {
                return Err(AppError::new(
                    "invalidated",
                    "连接设置已变化，请重新准备迁移",
                ));
            }
            _transfer_guards.push(guard);
        }
        let _send_guard = match &self.send_gate {
            Some((gate, check)) => {
                let guard = gate.lock().await;
                check()?;
                Some(guard)
            }
            None => None,
        };
        ensure_not_cancelled(&self.cancel)?;
        if let Some((run_id, ordinal)) = self.transfer_step {
            self.store
                .transfer_mark_sent(self.execution_id, run_id, ordinal, may_write)
                .await?;
        } else {
            self.store.mark_sent(self.execution_id, may_write).await?;
        }
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
