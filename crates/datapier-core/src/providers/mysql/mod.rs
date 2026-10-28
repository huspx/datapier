//! MySQL 会话、查询执行和结果解码；目录浏览及行变更由子模块负责。

mod catalog;
mod changes;
mod errors;
mod sql;

pub(crate) use changes::validate_import_value;

use crate::{ExecutionSink, model::*};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::TryStreamExt;
use serde_json::{Value as Json, json};
use sqlx::mysql::{MySqlArguments, MySqlConnectOptions, MySqlConnection, MySqlRow, MySqlSslMode};
use sqlx::{Column as _, Connection, Executor, Row, SqlSafeStr, Statement, TypeInfo, ValueRef};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 一个查询会话拥有独立物理连接，SQL 和显式事务在该连接上顺序执行。
/// 浏览和变更使用单独连接，编辑令牌只在此会话内有效。
pub struct MysqlSession {
    profile: Profile,
    connection: Option<MySqlConnection>,
    options: MySqlConnectOptions,
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
        let tls = &profile.tls;
        if tls.client_cert_path.is_some() != tls.client_key_path.is_some() {
            return Err(AppError::invalid("客户端证书和私钥必须同时提供"));
        }
        if !tls.enabled && (tls.ca_path.is_some() || tls.client_cert_path.is_some()) {
            return Err(AppError::invalid("证书需要启用 TLS"));
        }
        let mut options = MySqlConnectOptions::new()
            .host(&profile.host)
            .port(profile.port)
            .username(&profile.username)
            .ssl_mode(if tls.enabled {
                if tls.ca_path.is_some() {
                    MySqlSslMode::VerifyIdentity
                } else {
                    MySqlSslMode::Required
                }
            } else {
                MySqlSslMode::Disabled
            });
        if let Some(secret) = secret.as_deref() {
            options = options.password(secret);
        }
        if let Some(db) = profile.database.as_deref() {
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
        let connection = connect_initialized(&options, profile.access_mode).await?;
        Ok(Self {
            profile,
            connection: Some(connection),
            options,
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
            "resource.list" => catalog::list(self, params).await,
            "resource.describe" => catalog::describe(self, params).await,
            "resource.browse" => catalog::browse(self, params, sink, cancel).await,
            "change.open" => changes::open(self, params).await,
            "change.discard" => changes::discard(self, params).await,
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
        while !remaining.trim().is_empty() {
            // 前一条语句可能改变 sql_mode，因此每次切分都查询当前连接的模式。
            let parsed = async {
                ensure_not_cancelled(cancel)?;
                let mode: String = sqlx::query_scalar("SELECT @@SESSION.sql_mode")
                    .fetch_one(self.connection()?)
                    .await
                    .map_err(errors::mysql_error)?;
                sql::next_statement(remaining, &mode)
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
                    (_, Err(e)) if may_write && e.database_code.is_none() => Err(AppError::unknown("取消后写入结果无法确认")),
                    (Err(_), Ok(result)) => Ok(result),
                    (Err(e), Err(mut result)) => { result.details["cancel_failure"] = json!(e.to_string()); Err(result) },
                    (_, result) => result,
                }
            }
        }.map_err(|e| if may_write && e.code == "database" && e.database_code.is_none() { AppError::unknown(format!("写入结果无法确认: {}", e.message)) } else { e })
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

fn columns(cols: &[sqlx::mysql::MySqlColumn]) -> Vec<Column> {
    cols.iter()
        .map(|c| Column::new(c.name(), c.type_info().name()))
        .collect()
}

// 按原生类型解码：大整数和 DECIMAL 用字符串保留精度，二进制用 Base64
// 可逆传递；未知类型显式报错，不能退回可能有损的显示文本。
pub(super) fn decode_row(row: &MySqlRow) -> Result<Vec<Value>> {
    row.columns()
        .iter()
        .enumerate()
        .map(|(i, col)| {
            if row.try_get_raw(i)?.is_null() {
                return Ok(Value::Null);
            }
            let ty = col.type_info().name();
            let value = match ty {
                "BOOLEAN" | "TINYINT" => {
                    Value::Integer(row.try_get_unchecked::<i8, _>(i)?.to_string())
                }
                "SMALLINT" => Value::Integer(row.try_get_unchecked::<i16, _>(i)?.to_string()),
                "INT" | "MEDIUMINT" => {
                    Value::Integer(row.try_get_unchecked::<i32, _>(i)?.to_string())
                }
                "BIGINT" => Value::Integer(row.try_get_unchecked::<i64, _>(i)?.to_string()),
                "TINYINT UNSIGNED" => {
                    Value::Unsigned(row.try_get_unchecked::<u8, _>(i)?.to_string())
                }
                "SMALLINT UNSIGNED" | "YEAR" => {
                    Value::Unsigned(row.try_get_unchecked::<u16, _>(i)?.to_string())
                }
                "INT UNSIGNED" | "MEDIUMINT UNSIGNED" => {
                    Value::Unsigned(row.try_get_unchecked::<u32, _>(i)?.to_string())
                }
                "BIGINT UNSIGNED" => {
                    Value::Unsigned(row.try_get_unchecked::<u64, _>(i)?.to_string())
                }
                "DECIMAL" => Value::Decimal(
                    row.try_get_unchecked::<bigdecimal::BigDecimal, _>(i)?
                        .to_string(),
                ),
                "FLOAT" => Value::Float(row.try_get_unchecked::<f32, _>(i)?.to_string()),
                "DOUBLE" => Value::Float(row.try_get_unchecked::<f64, _>(i)?.to_string()),
                "DATE" => Value::Date(
                    row.try_get_unchecked::<chrono::NaiveDate, _>(i)?
                        .to_string(),
                ),
                "DATETIME" | "TIMESTAMP" => Value::DateTime(
                    row.try_get_unchecked::<chrono::NaiveDateTime, _>(i)?
                        .to_string(),
                ),
                "TIME" => Value::Time(
                    row.try_get_unchecked::<sqlx::mysql::types::MySqlTime, _>(i)?
                        .to_string(),
                ),
                "JSON" => Value::Json(row.try_get_unchecked::<Json, _>(i)?),
                "BINARY" | "VARBINARY" | "TINYBLOB" | "BLOB" | "MEDIUMBLOB" | "LONGBLOB"
                | "BIT" | "GEOMETRY" => {
                    Value::Bytes(STANDARD.encode(row.try_get_unchecked::<Vec<u8>, _>(i)?))
                }
                "CHAR" | "VARCHAR" | "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM"
                | "SET" => Value::Text(row.try_get_unchecked::<String, _>(i)?),
                _ => {
                    return Err(AppError::new(
                        "unsupported_type",
                        format!("不支持无损读取 MySQL 类型 {ty}"),
                    ));
                }
            };
            Ok(value)
        })
        .collect()
}

pub(super) async fn stream_query(
    conn: &mut MySqlConnection,
    sql: &str,
    args: MySqlArguments,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    ensure_not_cancelled(cancel)?;
    // 先 prepare 取得列定义，保证零行结果也能建立带列信息的结果集。
    let stmt = conn
        .prepare(sqlx::AssertSqlSafe(sql).into_sql_str())
        .await?;
    let metadata = columns(stmt.columns());
    let id = sink.start(metadata, "rows").await?;
    let mut stream = stmt.query_with(args).fetch(&mut *conn);
    let mut rows = Vec::with_capacity(128);
    let mut count = 0u64;
    loop {
        let row = tokio::select! { row = stream.try_next() => row?, _ = cancel.cancelled() => return Err(AppError::cancelled()) };
        let Some(row) = row else { break };
        rows.push(decode_row(&row)?);
        count += 1;
        if rows.len() == 128 {
            sink.append(id, std::mem::take(&mut rows)).await?;
        }
    }
    if !rows.is_empty() {
        sink.append(id, rows).await?;
    }
    sink.finish(id, json!({"row_count":count})).await?;
    Ok(json!({"results":[id],"row_count":count}))
}

async fn execute_statement(
    conn: &mut MySqlConnection,
    text: &str,
    sink: &ExecutionSink,
) -> Result<(Vec<Uuid>, u64)> {
    // 普通语句的 prepare 元数据用于保留空结果的列；prepare 失败时仍执行。
    // CALL／EXECUTE 的结果由运行时决定，SQLx 不暴露空动态结果元数据，
    // 且边界事件也可能是命令 OK 包，不能据此伪造列或断言存在结果集。
    let dynamic = sql::is_dynamic(text);
    let initial = conn
        .prepare(sqlx::AssertSqlSafe(text).into_sql_str())
        .await
        .ok()
        .map(|stmt| columns(stmt.columns()));
    let mut stream = sqlx::raw_sql(sqlx::AssertSqlSafe(text.to_owned())).fetch_many(&mut *conn);
    let mut results = Vec::new();
    let mut id = None;
    let mut pending = if dynamic { None } else { initial };
    let mut rows = Vec::with_capacity(128);
    let mut count = 0u64;
    let mut affected = 0u64;
    loop {
        let item = match stream.try_next().await {
            Ok(Some(item)) => item,
            Ok(None) => break,
            Err(error) => {
                let mut error = errors::mysql_error(error);
                let mut details = match error.details.as_ref() {
                    Json::Object(map) => map.clone(),
                    _ => serde_json::Map::new(),
                };
                details.insert("current_affected_rows".into(), json!(affected));
                details.insert("current_results".into(), json!(results));
                error.details = Box::new(Json::Object(details));
                return Err(error);
            }
        };
        match item {
            sqlx::Either::Right(row) => {
                if id.is_none() {
                    let meta = pending
                        .take()
                        .filter(|m| !m.is_empty())
                        .unwrap_or_else(|| columns(row.columns()));
                    let result = sink.start(meta, "rows").await?;
                    results.push(result);
                    id = Some(result);
                }
                rows.push(decode_row(&row)?);
                count += 1;
                if rows.len() == 128 {
                    sink.append(id.unwrap(), std::mem::take(&mut rows)).await?;
                }
            }
            sqlx::Either::Left(done) => {
                affected = affected.saturating_add(done.rows_affected());
                if id.is_none() && pending.as_ref().is_some_and(|c| !c.is_empty()) {
                    let result = sink.start(pending.take().unwrap(), "rows").await?;
                    results.push(result);
                    id = Some(result);
                }
                if id.is_none() && dynamic {
                    let result = sink.start(Vec::new(), "columns_unknown").await?;
                    results.push(result);
                    sink.finish(result, json!({"columns_known":false,"result_set_presence":"unknown","reason":"driver_does_not_expose_empty_dynamic_result_metadata"})).await?;
                }
                if let Some(result) = id.take() {
                    if !rows.is_empty() {
                        sink.append(result, std::mem::take(&mut rows)).await?;
                    }
                    sink.finish(result, json!({"row_count":count})).await?;
                    count = 0;
                }
                pending = None;
            }
        }
    }
    if let Some(result) = id {
        if !rows.is_empty() {
            sink.append(result, rows).await?;
        }
        sink.finish(result, json!({"row_count":count})).await?;
    }
    Ok((results, affected))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_errors_preserve_completed_and_unexecuted_work() {
        let id = Uuid::new_v4();
        let done = vec![
            json!({"index":0,"status":"executed","affected_rows":1,"results":[id],"transaction_state":"unknown"}),
        ];
        let error =
            AppError::new("database", "table missing").with_details(json!({"mysql_errno":1146}));
        let error = script_error(
            error,
            &done,
            &[id],
            1,
            "INSERT INTO t VALUES (2)",
            Some(("SELECT * FROM missing", "failed")),
        );
        assert_eq!(error.details["mysql_errno"], 1146);
        assert_eq!(error.details["statements"][0]["status"], "executed");
        assert_eq!(error.details["statements"][1]["status"], "failed");
        assert_eq!(error.details["affected_rows"], 1);
        assert_eq!(error.details["results"][0], id.to_string());
        assert_eq!(error.details["unexecuted_sql"], "INSERT INTO t VALUES (2)");
    }

    fn fixture_profile(mode: AccessMode) -> (Profile, Option<String>) {
        (
            Profile {
                id: Uuid::new_v4(),
                name: "mysql-fixture".into(),
                provider: ProviderKind::Mysql,
                host: "127.0.0.1".into(),
                port: std::env::var("DATAPIER_TEST_MYSQL_PORT")
                    .unwrap()
                    .parse()
                    .unwrap(),
                username: "datapier".into(),
                database: Some("datapier_fixture".into()),
                redis_db: 0,
                access_mode: mode,
                tls: TlsConfig::default(),
            },
            Some(std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap()),
        )
    }

    #[tokio::test]
    #[ignore = "需要隔离 MySQL 8.0 或 8.4 实例"]
    async fn every_readonly_physical_connection_uses_server_constraint() {
        let (profile, secret) = fixture_profile(AccessMode::ReadOnly);
        let mut session = MysqlSession::connect(profile, secret).await.unwrap();
        let mut browse = session.new_connection().await.unwrap();
        let readonly: i64 = sqlx::query_scalar("SELECT @@SESSION.transaction_read_only")
            .fetch_one(&mut browse)
            .await
            .unwrap();
        assert_eq!(readonly, 1);
        browse.close().await.unwrap();
        session.close().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "需要隔离 MySQL 8.0 或 8.4 实例"]
    async fn failed_script_preserves_completed_work_and_remaining_sql() {
        let root = std::env::temp_dir().join(format!("datapier-mysql-script-{}", Uuid::new_v4()));
        let store = crate::Store::open(&root).await.unwrap();
        let task = store
            .create_task(Uuid::new_v4(), "test", "execution.run", json!({}), true)
            .await
            .unwrap();
        let sink = ExecutionSink::new(store.clone(), task.id);
        let (profile, secret) = fixture_profile(AccessMode::ReadWrite);
        let mut session = MysqlSession::connect(profile, secret).await.unwrap();
        let text = "CREATE TEMPORARY TABLE dp_script_progress(id INT); INSERT INTO dp_script_progress VALUES (7); SELECT * FROM dp_table_does_not_exist; INSERT INTO dp_script_progress VALUES (8)";
        let error = session
            .call(
                "execution.run",
                json!({"text":text}),
                &sink,
                &CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.details["statements"][1]["status"], "executed");
        assert_eq!(error.details["statements"][2]["status"], "failed");
        assert_eq!(error.details["affected_rows"], 1);
        assert!(
            error.details["unexecuted_sql"]
                .as_str()
                .unwrap()
                .contains("VALUES (8)")
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dp_script_progress")
            .fetch_one(session.connection().unwrap())
            .await
            .unwrap();
        assert_eq!(count, 1);
        session.close().await.unwrap();
        store.pool.close().await;
        store.cache.close().await;
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
