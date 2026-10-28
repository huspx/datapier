//! 内置 MySQL 与 Redis 会话的编译期分派，具体语义由各 provider 实现。

pub mod mysql;
pub mod redis;
use crate::{ExecutionSink, Result, model::*};
use serde_json::Value as Json;
use tokio_util::sync::CancellationToken;

/// 一个数据库会话；枚举分派保留各 provider 的独立连接与操作语义。
pub enum ProviderSession {
    Mysql(mysql::MysqlSession),
    Redis(redis::RedisSession),
}
impl ProviderSession {
    /// 使用配置和可选凭据建立对应的数据库连接，失败时返回 provider 错误。
    pub async fn connect(profile: Profile, secret: Option<String>) -> Result<Self> {
        match profile.provider {
            ProviderKind::Mysql => Ok(Self::Mysql(
                mysql::MysqlSession::connect(profile, secret).await?,
            )),
            ProviderKind::Redis => Ok(Self::Redis(
                redis::RedisSession::connect(profile, secret).await?,
            )),
        }
    }
    pub fn profile(&self) -> &Profile {
        match self {
            Self::Mysql(s) => s.profile(),
            Self::Redis(s) => s.profile(),
        }
    }
    /// 在当前会话上执行 provider 方法；取消信号不保证服务端操作已撤销。
    pub async fn call(
        &mut self,
        method: &str,
        params: Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        match self {
            Self::Mysql(s) => s.call(method, params, sink, cancel).await,
            Self::Redis(s) => s.call(method, params, sink, cancel).await,
        }
    }
    /// 关闭当前会话并释放 provider 管理的连接资源。
    pub async fn close(&mut self) -> Result<()> {
        match self {
            Self::Mysql(s) => s.close().await,
            Self::Redis(s) => s.close().await,
        }
    }
}
