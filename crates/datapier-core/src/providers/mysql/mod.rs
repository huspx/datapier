//! MySQL 会话和查询执行；结果处理、目录浏览及行变更由子模块负责。

mod catalog;
mod changes;
mod errors;
mod live;
mod results;
mod sql;

pub use changes::validate_import_value;
pub(crate) use results::{columns as export_columns, decode_row as export_decode_row};
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
    thread_id: u64,
    options: MySqlConnectOptions,
    has_index_expression: bool,
    edits: HashMap<Uuid, ResourceTarget>,
    recipes: HashMap<Uuid, live::Recipe>,
    transaction: Option<bool>,
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
        let thread_id = sqlx::query_scalar("SELECT CONNECTION_ID()")
            .fetch_one(&mut connection)
            .await
            .map_err(errors::mysql_error)?;
        Ok(Self {
            profile,
            connection: Some(connection),
            thread_id,
            options,
            has_index_expression: has_index_expression != 0,
            edits: HashMap::new(),
            recipes: HashMap::new(),
            transaction: None,
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
    /// 返回可在独立连接上执行的完整浏览查询；会话查询不可证明独立重放语义。
    pub(crate) async fn export_query(
        &mut self,
        params: &Json,
        cancel: &CancellationToken,
    ) -> Result<(MySqlConnection, String, sqlx::mysql::MySqlArguments)> {
        ensure_not_cancelled(cancel)?;
        let id: Uuid = param(params, "result_id")?;
        let recipe = self
            .recipes
            .get(&id)
            .cloned()
            .ok_or_else(|| AppError::new("not_found", "结果不属于当前会话或已过期"))?;
        if !recipe.independent {
            return Err(AppError::new(
                "unsupported",
                "会话 SQL 无法证明在独立连接上保持事务与临时表语义",
            ));
        }
        Ok((
            self.new_connection().await?,
            recipe.sql.clone(),
            recipe.arguments()?,
        ))
    }
    /// 在原物理连接重查可重放 SQL，调用者必须消费完整流后释放借用。
    pub(crate) async fn export_session_query(
        &mut self,
        params: &Json,
    ) -> Result<(&mut MySqlConnection, String, sqlx::mysql::MySqlArguments)> {
        let id: Uuid = param(params, "result_id")?;
        let recipe = self
            .recipes
            .get(&id)
            .cloned()
            .ok_or_else(|| AppError::new("not_found", "结果不属于当前会话或已过期"))?;
        if recipe.independent {
            return Err(AppError::new("unsupported", "表浏览应使用独立连接导出"));
        }
        live::validate_context(self, &recipe).await?;
        let args = recipe.arguments()?;
        Ok((self.connection()?, recipe.sql, args))
    }
    /// 结果流未消费至协议终态时禁止继续复用物理连接。
    pub(crate) fn invalidate_connection(&mut self) {
        self.connection = None;
        self.edits.clear();
        self.recipes.clear();
    }

    /// 清除编辑令牌，尝试回滚并关闭会话连接；回滚或关闭失败时返回错误。
    pub async fn close(&mut self) -> Result<()> {
        self.edits.clear();
        self.recipes.clear();
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
                let row = sqlx::raw_sql(
                    "SELECT VERSION() AS version, DATABASE() AS db, @@SESSION.sql_mode AS sql_mode, @@lower_case_table_names AS lower_case_table_names",
                )
                .fetch_one(self.connection()?)
                .await
                .map_err(errors::mysql_error)?;
                Ok(
                    json!({"server_version": row.try_get::<String,_>("version")?, "database": row.try_get::<Option<String>,_>("db")?, "sql_mode": row.try_get::<String,_>("sql_mode")?, "lower_case_table_names": row.try_get::<u64,_>("lower_case_table_names")?, "transaction":self.transaction}),
                )
            }
            "database.list" => catalog::databases(self).await,
            "policy.inspect" => catalog::inspect(self, &params).await,
            "result.read" => live::read(self, &params, sink, cancel).await,
            "result.count" => live::count(self, &params, sink).await,
            "transaction.begin" | "transaction.commit" | "transaction.rollback" => {
                let text = match method {
                    "transaction.begin" => "BEGIN",
                    "transaction.commit" => "COMMIT",
                    _ => "ROLLBACK",
                };
                self.run(json!({"text":text}), sink, cancel).await
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
    async fn try_live_query(
        &mut self,
        text: &str,
        params: &Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Option<Json>> {
        let row = sqlx::raw_sql("SELECT DATABASE() AS db, @@SESSION.sql_mode AS sql_mode, @@lower_case_table_names AS lower_case_table_names")
            .fetch_one(self.connection()?).await.map_err(errors::mysql_error)?;
        let database: Option<String> = row.try_get("db")?;
        let mode: String = row.try_get("sql_mode")?;
        let (statement, rest) = sql::next_statement(text, &mode)?;
        let parameters = match params.get("parameters") {
            None | Some(Json::Null) => Vec::new(),
            Some(Json::Array(values)) => values.clone(),
            _ => return Err(AppError::invalid("parameters 必须是数组")),
        };
        if !parameters.is_empty() && !rest.trim().is_empty() {
            return Err(AppError::invalid("多语句不支持绑定参数"));
        }
        let mut checked = sqlx::mysql::MySqlArguments::default();
        for value in &parameters {
            catalog::add_filter_arg(&mut checked, value)?;
        }
        if !rest.trim().is_empty() {
            return Ok(None);
        }
        let modes: Vec<_> = mode.split(',').collect();
        let context = crate::policy::SqlContext {
            database: database.clone(),
            no_backslash_escapes: modes.contains(&"NO_BACKSLASH_ESCAPES"),
            ansi_quotes: modes.contains(&"ANSI_QUOTES"),
            ignore_space: modes.contains(&"IGNORE_SPACE"),
            lower_case_table_names: row.try_get::<u64, _>("lower_case_table_names")? as u8,
        };
        if !crate::policy::analyze_mysql(statement, &context).is_ok_and(|proof| proof.replayable) {
            return Ok(None);
        }
        let recipe = live::Recipe {
            sql: statement.to_owned(),
            bindings: parameters,
            source_columns: None,
            independent: false,
            context: Some((database.clone(), mode.clone())),
        };
        // 派生表不支持某些合法 SELECT（例如重复列名），这些查询仍走一次流。
        let probe = format!(
            "SELECT * FROM (\n{}\n) AS `_datapier_page` LIMIT 1",
            statement.trim().trim_end_matches(';')
        );
        use sqlx::{Executor, SqlSafeStr};
        if self
            .connection()?
            .prepare(sqlx::AssertSqlSafe(probe).into_sql_str())
            .await
            .is_err()
        {
            return Ok(None);
        }
        sink.store
            .record_task_database(sink.execution_id, database.as_deref())
            .await?;
        sink.mark_sent(false).await?;
        let size = params
            .get("page_size")
            .and_then(Json::as_u64)
            .unwrap_or(sink.store.settings().await?.paging.sql_rows as u64)
            .clamp(1, 1000) as usize;
        let result = live::start(self, recipe, size, sink, cancel).await?;
        Ok(Some(result))
    }

    async fn run(
        &mut self,
        params: Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        let text: String = param(&params, "text")?;
        if sql::diagnostic_statement(&text).is_none()
            && let Some(result) = self.try_live_query(&text, &params, sink, cancel).await?
        {
            return Ok(result);
        }
        let parameters = params
            .get("parameters")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        let mut remaining = text.as_str();
        let mut results = Vec::new();
        let mut affected = 0u64;
        let mut statements = Vec::new();
        let mut context_recorded = false;
        while !remaining.trim().is_empty() {
            // 诊断语句前不能查询会话状态，否则会清空上一条语句的诊断区。
            let parsed = if let Some(statement) = sql::diagnostic_statement(remaining) {
                ensure_not_cancelled(cancel).map(|()| statement)
            } else {
                // 前一条语句可能改变 sql_mode，普通语句按当前模式切分。
                async {
                    ensure_not_cancelled(cancel)?;
                    // DATABASE() 必须走当前连接的非缓存查询。
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
                .await
            };
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
            let transaction_control = matches!(
                statement
                    .trim()
                    .trim_end_matches(';')
                    .trim()
                    .to_ascii_uppercase()
                    .as_str(),
                "BEGIN"
                    | "BEGIN WORK"
                    | "START TRANSACTION"
                    | "COMMIT"
                    | "COMMIT WORK"
                    | "ROLLBACK"
                    | "ROLLBACK WORK"
            );
            if self.profile.access_mode == AccessMode::ReadOnly && !readonly && !transaction_control
            {
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
            let may_write = !readonly
                && !(transaction_control && self.profile.access_mode == AccessMode::ReadOnly);
            let mut sent = false;
            match self
                .run_statement(statement, may_write, sink, cancel, &parameters, &mut sent)
                .await
            {
                Ok((ids, count)) => {
                    // SQLx 不暴露服务端事务状态；任意 SQL、隐式提交和 autocommit
                    // 都可能改变状态，只有显式开始成功可证明事务正在进行。
                    self.transaction = match statement
                        .trim()
                        .trim_end_matches(';')
                        .trim()
                        .to_ascii_uppercase()
                        .as_str()
                    {
                        "BEGIN" | "BEGIN WORK" | "START TRANSACTION" => Some(true),
                        _ => None,
                    };
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
        parameters: &[Json],
        sent: &mut bool,
    ) -> Result<(Vec<Uuid>, u64)> {
        let thread_id = self.thread_id;
        ensure_not_cancelled(cancel)?;
        sink.mark_sent(may_write).await?;
        *sent = true;
        let options = self.options.clone();
        let outcome = {
            let batch_size = sink.store.settings().await?.paging.sql_rows as usize;
            let future = execute_statement(
                self.connection()?,
                statement,
                sink,
                cancel,
                may_write,
                batch_size.max(1),
                parameters,
            );
            tokio::pin!(future);
            tokio::select! {
            biased;
            result = &mut future => result,
            _ = cancel.cancelled(), if may_write => {
                // 停止展示不终止可能写入的协议；超时后销毁连接并报告未知结果。
                match tokio::time::timeout(std::time::Duration::from_secs(30), &mut future).await {
                    Ok(result) => result,
                    Err(_) => Err(AppError::unknown("取消后写入未在期限内结束").with_details(json!({"result_stream_incomplete":true}))),
                }
            }
            _ = cancel.cancelled(), if !may_write => {
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
            self.invalidate_connection();
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
