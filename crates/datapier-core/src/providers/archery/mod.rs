//! Archery 无状态只读会话；认证 Cookie 只属于客户端的共享认证槽。
mod catalog;
mod client;
mod query;
mod totp;

use crate::{ExecutionSink, Result, model::*};
pub use client::{AuthSlot, AuthStatus, normalize_server};
use serde_json::{Value as Json, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
pub use totp::{generate_totp, validate_totp_uri};

/// 绑定一个已经选择的远端实例和数据库，不代表专属数据库连接。
pub struct ArcherySession {
    pub(super) profile: Profile,
    pub(super) auth: Arc<AuthSlot>,
    pub(super) database: String,
    capabilities: Capabilities,
    closed: bool,
}
impl ArcherySession {
    pub async fn connect(
        profile: Profile,
        auth: Arc<AuthSlot>,
        database: Option<String>,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        profile.validate()?;
        let binding = profile
            .archery()
            .ok_or_else(|| AppError::invalid("连接不是 Archery 来源"))?;
        if binding.server_id != auth.server_id() {
            return Err(AppError::new("target_changed", "认证服务与连接不一致"));
        }
        if auth.status().await.state != "ready" {
            return Err(AppError::new("auth_required", "请先登录 Archery"));
        }
        let database = database.unwrap_or_else(|| binding.default_database.clone());
        if !binding.databases.contains(&database) {
            return Err(AppError::new("target_not_selected", "数据库不在选定范围内"));
        }
        let instance = ArcheryInstance {
            id: binding.instance_id,
            name: binding.instance_name.clone(),
            provider: profile.provider,
        };
        let discovered = auth.databases(&instance, cancel).await?;
        if !discovered.contains(&database) {
            return Err(AppError::new(
                "target_changed",
                "所选数据库已不在平台资源中",
            ));
        }
        let capabilities = profile.capabilities();
        Ok(Self {
            profile,
            auth,
            database,
            capabilities,
            closed: false,
        })
    }
    pub fn profile(&self) -> &Profile {
        &self.profile
    }
    pub async fn close(&mut self) -> Result<()> {
        self.closed = true;
        Ok(())
    }
    pub async fn call(
        &mut self,
        method: &str,
        params: Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        ensure_not_cancelled(cancel)?;
        if self.closed {
            return Err(AppError::new("connection_closed", "连接已关闭"));
        }
        self.capabilities.ensure_method(method, &params)?;
        let value = match method {
            "connection.info" => Ok(
                json!({"source":"archery","database":self.database,"capabilities":self.capabilities,"query_semantics":"stateless"}),
            ),
            "resource.list" => catalog::list(self, params, sink, cancel).await,
            "resource.describe" => catalog::describe(self, params, sink, cancel).await,
            "resource.browse" => catalog::browse(self, params, sink, cancel).await,
            "execution.run" => query::run(self, &params, sink, cancel).await,
            "archery.sql_check" => query::sql_check(self, &params, sink, cancel).await,
            "archery.query_history" => {
                let offset = params.get("offset").and_then(Json::as_u64).unwrap_or(0);
                let limit = params.get("limit").and_then(Json::as_u64).unwrap_or(20);
                let search = params.get("search").and_then(Json::as_str).unwrap_or("");
                self.auth.query_history(offset, limit, search, cancel).await
            }
            _ => Err(AppError::new(
                "capability_unavailable",
                format!("Archery 不支持方法 {method}"),
            )),
        };
        self.capabilities.observe(method, &value);
        let value = value?;
        client::budget(&value)?;
        Ok(value)
    }
}
