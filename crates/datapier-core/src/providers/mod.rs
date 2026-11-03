//! 内置 MySQL 与 Redis 会话的编译期分派，具体语义由各 provider 实现。

pub mod archery;
pub mod mysql;
pub mod redis;
use crate::{ExecutionSink, Result, model::*};
pub use redis::redis_contains_auth;
use serde_json::Value as Json;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 一个数据库会话；枚举分派保留各 provider 的独立连接与操作语义。
pub enum ProviderSession {
    Mysql(mysql::MysqlSession),
    // Redis 独立连接及事务状态较大，避免其体积传播到所有 provider 会话。
    Redis(Box<redis::RedisSession>),
    Archery(archery::ArcherySession),
}
impl ProviderSession {
    /// 使用配置和可选凭据建立对应的数据库连接，失败时返回 provider 错误。
    pub async fn connect(profile: Profile, secret: Option<String>) -> Result<Self> {
        if profile.is_archery() {
            return Err(AppError::new("auth_required", "Archery 连接需要认证上下文"));
        }
        match profile.provider {
            ProviderKind::Mysql => Ok(Self::Mysql(
                mysql::MysqlSession::connect(profile, secret).await?,
            )),
            ProviderKind::Redis => Ok(Self::Redis(Box::new(
                redis::RedisSession::connect(profile, secret).await?,
            ))),
        }
    }
    /// 使用调用方所属的认证槽建立 Archery 逻辑会话。
    pub async fn connect_archery(
        profile: Profile,
        auth: Arc<archery::AuthSlot>,
        database: Option<String>,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        Ok(Self::Archery(
            archery::ArcherySession::connect(profile, auth, database, cancel).await?,
        ))
    }
    pub fn profile(&self) -> &Profile {
        match self {
            Self::Mysql(s) => s.profile(),
            Self::Redis(s) => s.profile(),
            Self::Archery(s) => s.profile(),
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
            Self::Archery(s) => s.call(method, params, sink, cancel).await,
        }
    }
    /// 关闭当前会话并释放 provider 管理的连接资源。
    pub async fn close(&mut self) -> Result<()> {
        match self {
            Self::Mysql(s) => s.close().await,
            Self::Redis(s) => s.close().await,
            Self::Archery(s) => s.close().await,
        }
    }
}
