//! MySQL 会话和查询执行；结果处理、目录浏览及行变更由子模块负责。

mod catalog;
mod changes;
mod errors;
mod results;
mod sql;

pub use changes::validate_import_value;
pub use sql::{stateless_statement_ranges, stateless_statements, validate_readonly};

use crate::{ExecutionSink, model::*};
use results::execute_statement;
use serde_json::{Value as Json, json};
use sqlx::mysql::{MySqlConnectOptions, MySqlConnection, MySqlSslMode};
use sqlx::{Connection, Row};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 一个查询会话拥有独立物理连接，SQL 和显式事务在该连接上顺序执行。
/// 浏览和变更使用单独连接，编辑令牌只在此会话内有效。
pub struct MysqlSession {
    profile: Profile,
    connection: Option<MySqlConnection>,
    options: MySqlConnectOptions,
    has_index_expression: bool,
    edits: HashMap<Uuid, ResourceTarget>,
}

impl MysqlSession {
    /// 校验连接配置并建立 MySQL 连接；只读模式还会设置服务端只读事务。
    /// TLS 配置、连接或会话初始化失败时返回错误。
    pub async fn connect(profile: Profile, secret: Option<String>) -> Result<Self> {
        profile.validate()?;
        if profile.provider != ProviderKind::Mysql {
            return Err(AppError::invalid("非 MySQL 连接"));
        }
        let direct = profile.direct()?;
        let tls = &direct.tls;
        if tls.client_cert_path.is_some() != tls.client_key_path.is_some() {
            return Err(AppError::invalid("客户端证书和私钥必须同时提供"));
        }
        if !tls.enabled && (tls.ca_path.is_some() || tls.client_cert_path.is_some()) {
            return Err(AppError::invalid("证书需要启用 TLS"));
        }
        let mut options = MySqlConnectOptions::new()
            .host(&direct.host)
            .port(direct.port)
            .username(&direct.username)
            .ssl_mode(if tls.enabled {
                MySqlSslMode::VerifyIdentity
            } else {
                MySqlSslMode::Disabled
            });
        if let Some(secret) = secret.as_deref() {
            options = options.password(secret);
        }
        if let Some(db) = direct.database.as_deref() {
            options = options.database(db);
        }
        if let Some(path) = &tls.ca_path {
            options = options.ssl_ca(path);
        }
        if let Some(path) = &tls.client_cert_path {
            options = options.ssl_client_cert(path);
        }
        if let Some(path) = &tls.client_key_path {
            options = options.ssl_client_key(path);
        }
        let mut connection = connect_initialized(&options, profile.access_mode).await?;
        // 5.7 没有表达式索引列；按实际元数据探测，避免误判兼容服务端的版本号。
        let has_index_expression: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = 'information_schema' AND TABLE_NAME = 'STATISTICS' AND COLUMN_NAME = 'EXPRESSION'")
            .fetch_one(&mut connection)
            .await
            .map_err(errors::mysql_error)?;
        Ok(Self {
            profile,
            connection: Some(connection),
            options,
            has_index_expression: has_index_expression != 0,
            edits: HashMap::new(),
        })
    }
    pub fn profile(&self) -> &Profile {
        &self.profile
    }
    pub(super) async fn new_connection(&self) -> Result<MySqlConnection> {
        if self.connection.is_none() {
            return Err(AppError::new("connection_closed", "连接已关闭"));
        }
        connect_initialized(&self.options, self.profile.access_mode).await
    }
    pub(super) fn connection(&mut self) -> Result<&mut MySqlConnection> {
        self.connection
            .as_mut()
            .ok_or_else(|| AppError::new("connection_closed", "连接已关闭"))
    }
    /// 清除编辑令牌，尝试回滚并关闭会话连接；回滚或关闭失败时返回错误。
    pub async fn close(&mut self) -> Result<()> {
        self.edits.clear();
        if let Some(mut connection) = self.connection.take() {
            let rollback = sqlx::raw_sql("ROLLBACK").execute(&mut connection).await;
            let closed = connection.close().await;
            rollback?;
            closed?;
        }
        Ok(())
    }
    /// 执行指定 provider 方法，查询结果通过 `sink` 持久化。
    /// 参数无效、只读策略拒绝、取消或数据库操作失败时返回相应错误；
    /// 写入发送后的不确定结果会以 `outcome_unknown` 报告。
    pub async fn call(
        &mut self,
        method: &str,
        params: Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        ensure_not_cancelled(cancel)?;
        match method {
            "connection.info" => {
                let row = sqlx::query(
                    "SELECT VERSION() AS version, DATABASE() AS db, @@SESSION.sql_mode AS sql_mode",
                )
                .fetch_one(self.connection()?)
                .await
                .map_err(errors::mysql_error)?;
                Ok(
                    json!({"server_version": row.try_get::<String,_>("version")?, "database": row.try_get::<Option<String>,_>("db")?, "sql_mode": row.try_get::<String,_>("sql_mode")?}),
                )
            }
            "resource.list" => catalog::list(self, params, cancel).await,
            "resource.describe" => catalog::describe(self, params, cancel).await,
            "resource.browse" => catalog::browse(self, params, sink, cancel).await,
            "change.open" => changes::open(self, params, cancel).await,
            "change.discard" => changes::discard(self, params).await,
            "change.preview" => changes::preview(self, params, sink, cancel).await,
            "change.submit" => {
                self.profile.ensure_write()?;
                changes::submit(self, params, sink, cancel).await
            }
            "import.rows" => {
                self.profile.ensure_write()?;
                changes::import(self, params, sink, cancel).await
            }
            "execution.run" => self.run(params, sink, cancel).await,
            _ => Err(AppError::invalid(format!("未知 MySQL 方法 {method}"))),
        }
    }
    async fn run(
        &mut self,
        params: Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        let text: String = param(&params, "text")?;
        let mut remaining = text.as_str();
        let mut results = Vec::new();
        let mut affected = 0u64;
        let mut statements = Vec::new();
        let mut context_recorded = false;
        while !remaining.trim().is_empty() {
            // 前一条语句可能改变 sql_mode，因此每次切分都查询当前连接的模式。
            let parsed = async {
                ensure_not_cancelled(cancel)?;
                // DATABASE() 在预处理语句中绑定 prepare 时的库；元数据必须走当前连接的非缓存查询。
                let state =
                    sqlx::raw_sql("SELECT @@SESSION.sql_mode AS sql_mode, DATABASE() AS db")
                        .fetch_one(self.connection()?)
                        .await
                        .map_err(errors::mysql_error)?;
                if !context_recorded {
                    let database: Option<String> = state.try_get("db")?;
                    sink.store
                        .record_task_database(sink.execution_id, database.as_deref())
                        .await?;
                    context_recorded = true;
                }
                sql::next_statement(remaining, &state.try_get::<String, _>("sql_mode")?)
            }
            .await;
            let (statement, rest) = match parsed {
                Ok(value) => value,
                Err(error) => {
                    return Err(script_error(
                        error,
                        &statements,
                        &results,
                        affected,
                        remaining,
                        None,
                    ));
                }
            };
            remaining = rest;
            if statement.trim().is_empty() {
                continue;
            }
            let readonly = sql::readonly(statement);
            if self.profile.access_mode == AccessMode::ReadOnly && !readonly {
                let error = AppError::new("permission_denied", "只读连接拒绝无法证明安全的 SQL");
                return Err(script_error(
                    error,
                    &statements,
                    &results,
                    affected,
                    remaining,
                    Some((statement, "not_executed")),
                ));
            }
            let may_write = !readonly;
            let mut sent = false;
            match self
                .run_statement(statement, may_write, sink, cancel, &mut sent)
                .await
            {
                Ok((ids, count)) => {
                    statements.push(json!({"index":statements.len(),"text":statement,"status":"executed","may_write":may_write,"affected_rows":count,"results":ids,"transaction_state":"unknown"}));
                    results.extend(ids);
                    affected = affected.saturating_add(count);
                }
                Err(error) => {
                    let status = if !sent {
                        "not_executed"
                    } else if error.code == "outcome_unknown" {
                        "outcome_unknown"
                    } else if error.code == "cancelled" {
                        "cancelled"
                    } else {
                        "failed"
                    };
                    if let Ok(ids) = sink.store.result_ids(sink.execution_id).await {
                        results = ids;
                    }
                    return Err(script_error(
                        error,
                        &statements,
                        &results,
                        affected,
                        remaining,
                        Some((statement, status)),
                    ));
                }
            }
        }
        Ok(
            json!({"results":results,"affected_rows":affected,"statements":statements,"transaction_state":"unknown"}),
        )
    }

    async fn run_statement(
        &mut self,
        statement: &str,
        may_write: bool,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
        sent: &mut bool,
    ) -> Result<(Vec<Uuid>, u64)> {
        let thread_id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(self.connection()?)
            .await
            .map_err(errors::mysql_error)?;
        ensure_not_cancelled(cancel)?;
        sink.mark_sent(may_write).await?;
        *sent = true;
        let options = self.options.clone();
        let outcome = {
            let future = execute_statement(self.connection()?, statement, sink);
            tokio::pin!(future);
            tokio::select! {
            biased;
            result = &mut future => result,
            _ = cancel.cancelled() => {
                // 控制连接上的 KILL 可能与目标语句完成竞态。两者都结束前保持
                // 对目标连接的独占借用，避免迟到的 KILL 命中下一条语句。
                let killed = match MySqlConnection::connect_with(&options).await {
                    Ok(mut control) => {
                        let kill = format!("KILL QUERY {thread_id}");
                        sqlx::raw_sql(sqlx::AssertSqlSafe(kill)).execute(&mut control).await.map(|_| ())
                    }
                    Err(error) => Err(error),
                };
                let final_result = future.await;
                match (killed, final_result) {
                    (Ok(_), Err(mut e)) if e.details.get("mysql_errno").and_then(Json::as_u64) == Some(1317) && !may_write => { e.code = "cancelled".into(); Err(e) },
                    (Ok(_), Err(mut e)) if may_write && e.details.get("mysql_errno").and_then(Json::as_u64) == Some(1317) => { e.code = "outcome_unknown".into(); e.message = format!("写入被中断，可能部分生效: {}", e.message); Err(e) },
                    (_, Err(e)) if may_write && e.database_code.is_none() && e.code != "outcome_unknown" => Err(AppError::unknown("取消后写入结果无法确认")),
                    (Err(_), Ok(result)) => Ok(result),
                    (Err(e), Err(mut result)) => { result.details["cancel_failure"] = json!(e.to_string()); Err(result) },
                    (_, result) => result,
                }
            }
        }.map_err(|mut e| {
            if may_write && e.code == "database" && e.database_code.is_none() {
                e.code = "outcome_unknown".into();
                e.message = format!("写入结果无法确认: {}", e.message);
            }
            e
        })
        };
        if outcome.as_ref().is_err_and(|error| {
            error
                .details
                .get("result_stream_incomplete")
                .and_then(Json::as_bool)
                == Some(true)
        }) {
            // 本地缓存失败会提前结束结果流；SQLx 不能保证该连接已消费服务端终态。
            // 丢弃物理连接及其编辑令牌，避免下一条请求等待不存在的协议边界。
            self.connection = None;
            self.edits.clear();
        }
        outcome
    }
}

fn script_error(
    mut error: AppError,
    completed: &[Json],
    results: &[Uuid],
    affected: u64,
    remaining: &str,
    current: Option<(&str, &str)>,
) -> AppError {
    let mut statements = completed.to_vec();
    if let Some((text, status)) = current {
        statements.push(json!({"index":statements.len(),"text":text,"status":status,"transaction_state":"unknown"}));
    }
    let mut details = match error.details.as_ref() {
        Json::Object(map) => map.clone(),
        _ => serde_json::Map::new(),
    };
    details.insert("statements".into(), json!(statements));
    details.insert("results".into(), json!(results));
    details.insert("affected_rows".into(), json!(affected));
    details.insert("unexecuted_sql".into(), json!(remaining));
    // SQLx 的 raw SQL 不暴露服务端事务状态标志，不能推断提交或回滚状态。
    details.insert("transaction_state".into(), json!("unknown"));
    error.details = Box::new(Json::Object(details));
    error
}

async fn connect_initialized(
    options: &MySqlConnectOptions,
    mode: AccessMode,
) -> Result<MySqlConnection> {
    let mut connection = MySqlConnection::connect_with(options)
        .await
        .map_err(errors::mysql_error)?;
    // 每条新物理连接都设置服务端约束，补充 SQL 只读判定。
    if mode == AccessMode::ReadOnly {
        sqlx::raw_sql("SET SESSION TRANSACTION READ ONLY")
            .execute(&mut connection)
            .await
            .map_err(errors::mysql_error)?;
    }
    Ok(connection)
}

pub(super) fn quote_ident(name: &str) -> Result<String> {
    if name.is_empty() || name.contains('\0') {
        return Err(AppError::invalid("无效标识符"));
    }
    Ok(format!("`{}`", name.replace('`', "``")))
}

#[cfg(test)]
#[path = "../../../tests/unit/mysql/session.rs"]
mod tests;
