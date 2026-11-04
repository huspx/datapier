use super::ArcherySession;
use crate::{
    execution::ExecutionSink,
    model::{self, AppError, Column, ProviderKind, Result, Value},
    providers::{mysql, redis::command},
};
use serde_json::{Value as Json, json};
use tokio_util::sync::CancellationToken;

#[cfg(test)]
#[path = "../../../tests/unit/archery/query.rs"]
mod tests;

const MAX_TEXT: usize = 1024 * 1024;
const MAX_STATEMENTS: usize = 100;
const DEFAULT_LIMIT: u64 = 100;
const MAX_LIMIT: u64 = 10000;

/// 平台查询的有序列和原始行；完整性必须由调用者结合平台计数与脱敏证据判断。
#[derive(Debug)]
pub(super) struct QueryResponse {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Json>>,
    pub affected_rows: Option<u64>,
    pub is_masked: Option<bool>,
    pub mask_rule_hit: Option<bool>,
    types: Vec<String>,
    remote: Json,
    full_text: Option<String>,
}

fn protocol(message: &str) -> AppError {
    AppError::new("remote_protocol", message)
}
fn bounded(value: &impl serde::Serialize, max: usize) -> Result<()> {
    if serde_json::to_vec(value)?.len() > max {
        return Err(AppError::new(
            "response_too_large",
            "平台结果元数据超过大小限制",
        ));
    }
    Ok(())
}
fn target<'a>(session: &'a ArcherySession, database: &str) -> Result<&'a model::ArcheryConnection> {
    let connection = session
        .profile
        .archery()
        .ok_or_else(|| AppError::invalid("需要 Archery 连接"))?;
    if session.profile.access_mode != model::AccessMode::ReadOnly {
        return Err(AppError::new("permission_denied", "Archery 连接必须只读"));
    }
    if !connection.databases.iter().any(|db| db == database)
        || (session.profile.provider == ProviderKind::Redis && database != session.database)
    {
        return Err(AppError::new("target_not_selected", "数据库不在所选范围内"));
    }
    Ok(connection)
}
fn limit(params: &Json) -> Result<u64> {
    match params.get("limit") {
        None | Some(Json::Null) => Ok(DEFAULT_LIMIT),
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=MAX_LIMIT).contains(n))
            .ok_or_else(|| AppError::invalid("limit 必须在 1 到 10000 之间")),
    }
}
fn text(params: &Json) -> Result<String> {
    let text: String = model::param(params, "text")?;
    if text.is_empty() || text.len() > MAX_TEXT {
        return Err(AppError::invalid("文本为空或超过 1 MiB"));
    }
    Ok(text)
}
#[derive(serde::Deserialize)]
struct QueryEnvelope {
    status: i64,
    #[serde(default)]
    msg: Option<String>,
    #[serde(default)]
    data: Option<QueryData>,
}
#[derive(serde::Deserialize)]
struct QueryData {
    #[serde(default)]
    column_list: Option<Vec<String>>,
    #[serde(default)]
    column_type: Option<Vec<String>>,
    #[serde(default)]
    rows: Option<Vec<Vec<Json>>>,
    #[serde(default)]
    affected_rows: Option<u64>,
    #[serde(default)]
    is_masked: Option<bool>,
    #[serde(default)]
    mask_rule_hit: Option<bool>,
    #[serde(default)]
    full_sql: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    warning: Option<Json>,
    #[serde(default)]
    query_time: Option<Json>,
}
fn decode_query(envelope: QueryEnvelope, redis: bool) -> Result<QueryResponse> {
    if envelope.status != 0 {
        return Err(AppError::new(
            "database",
            envelope.msg.unwrap_or_else(|| "平台查询失败".into()),
        ));
    }
    let data = envelope.data.ok_or_else(|| protocol("查询响应缺少数据"))?;
    if let Some(error) = data.error.filter(|v| !v.is_empty()) {
        return Err(AppError::new("database", error));
    }
    let columns = data.column_list.ok_or_else(|| protocol("缺少查询列"))?;
    // 1.8.4 的查询响应不提供 column_type；保留未知类型，不能据值猜数据库类型。
    let types = match data.column_type {
        Some(types) => {
            if (!redis && types.len() != columns.len())
                || (redis && !types.is_empty() && types.len() != columns.len())
            {
                return Err(protocol("查询列类型数量不匹配"));
            }
            types
        }
        None => Vec::new(),
    };
    let rows = data.rows.ok_or_else(|| protocol("缺少查询行"))?;
    if rows.iter().any(|row| row.len() != columns.len()) {
        return Err(protocol("查询行列数量不匹配"));
    }
    let (preview, truncated) = preview(data.full_sql.as_deref().unwrap_or(""), 4096);
    let remote = json!({"query_time":data.query_time,"warning":data.warning,"executed_text_preview":if data.full_sql.is_some(){Some(preview)}else{None},"executed_text_truncated":truncated});
    Ok(QueryResponse {
        columns,
        rows,
        affected_rows: data.affected_rows,
        is_masked: data.is_masked,
        mask_rule_hit: data.mask_rule_hit,
        types,
        remote,
        full_text: data.full_sql,
    })
}
fn preview(text: &str, max: usize) -> (&str, bool) {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (&text[..end], end < text.len())
}
async fn request(
    session: &ArcherySession,
    database: &str,
    sql: &str,
    requested_limit: u64,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<QueryResponse> {
    let connection = target(session, database)?;
    model::ensure_not_cancelled(cancel)?;
    let response: QueryEnvelope = session
        .auth
        .post_execution(
            "query/",
            &[
                ("instance_name", connection.instance_name.clone()),
                ("db_name", database.to_owned()),
                ("sql_content", sql.to_owned()),
                ("limit_num", requested_limit.to_string()),
            ],
            true,
            sink,
            cancel,
        )
        .await?;
    let decoded = decode_query(response, session.profile.provider == ProviderKind::Redis);
    if decoded
        .as_ref()
        .map_or_else(|error| error.code == "database", |_| true)
    {
        sink.mark_remote_completed().await?;
    }
    decoded
}
/// 执行经过共享只读验证的目录 SQL，并保留原始平台行供结构解析。
pub(super) async fn catalog_query(
    session: &ArcherySession,
    database: &str,
    sql: &str,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<QueryResponse> {
    catalog_browse(session, database, sql, MAX_LIMIT, sink, cancel).await
}
/// 按调用方指定的行数上限执行受控表浏览。
pub(super) async fn catalog_browse(
    session: &ArcherySession,
    database: &str,
    sql: &str,
    requested_limit: u64,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<QueryResponse> {
    if session.profile.provider != ProviderKind::Mysql {
        return Err(AppError::new(
            "capability_unavailable",
            "仅 MySQL 支持关系目录",
        ));
    }
    if !(1..=MAX_LIMIT).contains(&requested_limit) {
        return Err(AppError::invalid("limit 必须在 1 到 10000 之间"));
    }
    mysql::validate_readonly(sql)?;
    if mysql::stateless_statements(sql)?.len() != 1 {
        return Err(AppError::invalid("目录查询必须恰好包含一条语句"));
    }
    request(session, database, sql, requested_limit, sink, cancel).await
}
fn cell(value: Json, ty: &str, redis: bool, masked: bool) -> Value {
    match value {
        Json::Null => Value::Null,
        Json::Bool(v) => Value::Bool(v),
        Json::Array(_) | Json::Object(_) => Value::Json(value),
        Json::Number(n) => {
            let s = n.to_string();
            if redis {
                Value::Text(s)
            } else if masked {
                Value::Json(Json::Number(n))
            } else {
                match ty.to_ascii_uppercase().as_str() {
                    "TINY" | "SHORT" | "LONG" | "LONGLONG" | "INT24" | "YEAR" => Value::Integer(s),
                    "DECIMAL" | "NEWDECIMAL" => Value::Decimal(s),
                    "FLOAT" | "DOUBLE" => Value::Float(s),
                    _ => Value::Json(Json::Number(n)),
                }
            }
        }
        Json::String(s) => {
            if redis || masked {
                return Value::Text(s);
            }
            match ty.to_ascii_uppercase().as_str() {
                "TINY" | "SHORT" | "LONG" | "LONGLONG" | "INT24" | "YEAR"
                    if s.parse::<i128>().is_ok() =>
                {
                    Value::Integer(s)
                }
                "DECIMAL" | "NEWDECIMAL" if s.parse::<bigdecimal::BigDecimal>().is_ok() => {
                    Value::Decimal(s)
                }
                "DATE" | "NEWDATE" => Value::Date(s),
                "DATETIME" | "TIMESTAMP" => Value::DateTime(s),
                "TIME" => Value::Time(s),
                _ => Value::Text(s),
            }
        }
    }
}
/// 将完整平台响应写入本地结果缓存，完成状态仅描述该响应。
pub(super) async fn cache_response(
    session: &ArcherySession,
    database: &str,
    response: QueryResponse,
    requested_limit: u64,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    let connection = target(session, database)?;
    let redis = session.profile.provider == ProviderKind::Redis;
    let columns: Vec<Column> = response
        .columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            Column::new(
                name,
                response
                    .types
                    .get(i)
                    .map(String::as_str)
                    .unwrap_or(if redis { "platform_text" } else { "unknown" }),
            )
        })
        .collect();
    let count = response.rows.len();
    let coverage = if !redis && response.affected_rows.is_some_and(|n| n > count as u64) {
        "limited"
    } else {
        "unknown"
    };
    let masking = if redis {
        "not_supported"
    } else if response.is_masked == Some(true) {
        "masked"
    } else {
        "unknown"
    };
    let summary = json!({"origin":{"source":"archery","server_id":connection.server_id,"instance_id":connection.instance_id,"database":database,"provider":session.profile.provider,"readonly_policy":"1"},"coverage":{"scope":"returned_response","requested_limit":requested_limit,"returned_rows":count,"original_query":coverage},"fidelity":{"kind":if redis {"platform_text"}else{"platform_values"},"lossy":["binary","bit","encoding","masking"],"unknown":["original_types","original_bytes"]},"masking":masking,"remote":response.remote});
    bounded(&summary, 32 * 1024)?;
    bounded(&json!({"columns":columns,"summary":summary}), 256 * 1024)?;
    let id = sink.start(columns, "rows").await?;
    let mut batch = Vec::with_capacity(128);
    for row in response.rows {
        model::ensure_not_cancelled(cancel)?;
        batch.push(
            row.into_iter()
                .enumerate()
                .map(|(i, v)| {
                    cell(
                        v,
                        response.types.get(i).map(String::as_str).unwrap_or(""),
                        redis,
                        response.is_masked == Some(true),
                    )
                })
                .collect(),
        );
        if batch.len() == 128 {
            sink.append(id, std::mem::take(&mut batch)).await?;
        }
    }
    if !batch.is_empty() {
        sink.append(id, batch).await?;
    }
    model::ensure_not_cancelled(cancel)?;
    sink.finish(id, summary).await?;
    Ok(json!({"results":[id],"row_count":count}))
}
fn redis_commands(input: &str) -> Result<Vec<String>> {
    let commands = command::parse_commands(input)?;
    if commands.is_empty() || commands.len() > MAX_STATEMENTS {
        return Err(AppError::invalid("命令数量必须为 1 到 100"));
    }
    commands
        .into_iter()
        .map(|args| {
            let name = std::str::from_utf8(&args[0])
                .map_err(|_| AppError::invalid("命令名必须是 UTF-8"))?
                .to_ascii_uppercase();
            if !name.bytes().all(|b| b.is_ascii_uppercase())
                || !command::read_command(&args)
                || !matches!(
                    name.as_str(),
                    "SCAN"
                        | "EXISTS"
                        | "TTL"
                        | "PTTL"
                        | "TYPE"
                        | "GET"
                        | "MGET"
                        | "STRLEN"
                        | "HGETALL"
                        | "HEXISTS"
                        | "HGET"
                        | "HMGET"
                        | "HKEYS"
                        | "HVALS"
                        | "SMEMBERS"
                        | "SCARD"
                        | "SDIFF"
                        | "SUNION"
                        | "SISMEMBER"
                        | "LLEN"
                        | "LRANGE"
                        | "LINDEX"
                        | "ZRANGE"
                        | "ZRANGEBYSCORE"
                        | "ZSCORE"
                        | "ZCARD"
                        | "ZCOUNT"
                        | "ZRANK"
                        | "INFO"
                )
            {
                return Err(AppError::new(
                    "permission_denied",
                    format!("平台不支持只读命令 {name}"),
                ));
            }
            let mut result = name;
            for arg in &args[1..] {
                let arg = std::str::from_utf8(arg)
                    .map_err(|_| AppError::invalid("平台命令仅支持 UTF-8"))?;
                if arg.contains('\0') {
                    return Err(AppError::invalid("平台命令不支持 NUL"));
                }
                result.push(' ');
                result.push('\'');
                result.push_str(&arg.replace('\'', "'\\''"));
                result.push('\'');
            }
            Ok(result)
        })
        .collect()
}
/// 先验证整批，再逐条向平台发送，避免前面已执行后才发现不安全输入。
pub(super) async fn run(
    session: &ArcherySession,
    params: &Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    let database: String = model::param(params, "database")?;
    target(session, &database)?;
    let input = text(params)?;
    let requested_limit = limit(params)?;
    let statements = if session.profile.provider == ProviderKind::Mysql {
        mysql::validate_readonly(&input)?;
        mysql::stateless_statements(&input)?
    } else {
        redis_commands(&input)?
    };
    if statements.is_empty() || statements.len() > MAX_STATEMENTS {
        return Err(AppError::invalid("语句数量必须为 1 到 100"));
    }
    let mut results = Vec::new();
    let mut completed = Vec::new();
    for (index, statement) in statements.iter().enumerate() {
        let outcome = async {
            let response =
                request(session, &database, statement, requested_limit, sink, cancel).await?;
            if let Some(actual) = response.full_text.as_deref() {
                let (actual, truncated) = preview(actual, 65536);
                sink.store
                    .append_execution_text(
                        sink.execution_id,
                        &format!("{}: {}\n", index + 1, actual),
                        truncated,
                    )
                    .await?;
            }
            cache_response(session, &database, response, requested_limit, sink, cancel).await
        }
        .await;
        match outcome {
            Ok(result) => {
                let ids = result["results"].as_array().cloned().unwrap_or_default();
                results.extend(ids.clone());
                completed.push(
                    json!({"index":index,"status":"executed","results":ids,"may_write":false}),
                );
            }
            Err(mut error) => {
                let rest: Vec<_> = (index + 1..statements.len())
                    .map(|i| json!({"index":i,"status":"not_executed"}))
                    .collect();
                error.details = Box::new(
                    json!({"cause":error.details,"results":results,"statements":completed,"failed_index":index,"not_executed":rest}),
                );
                return Err(error);
            }
        }
    }
    Ok(json!({"results":results,"statements":completed,"transaction_state":"unknown"}))
}

#[derive(Debug)]
struct CheckResponse {
    rows: Vec<serde_json::Map<String, Json>>,
    warnings: u64,
    errors: u64,
    verdict: &'static str,
}

#[derive(serde::Deserialize)]
struct CheckDto {
    #[serde(default)]
    checked: Option<Json>,
    #[serde(default)]
    warning: Option<String>,
    #[serde(default)]
    error: Option<String>,
    warning_count: u64,
    error_count: u64,
    rows: Vec<serde_json::Map<String, Json>>,
}
fn decode_check(data: CheckDto) -> Result<CheckResponse> {
    match data.checked.as_ref() {
        Some(Json::String(v)) if v == "True" => {}
        Some(Json::String(v)) if v == "False" => {
            return Err(AppError::new("check_incomplete", "平台未完成 SQL 检查"));
        }
        None | Some(Json::Null) => {
            return Err(AppError::new("check_incomplete", "平台未报告 SQL 检查完成"));
        }
        _ => return Err(protocol("SQL 检查状态无效")),
    }
    let mut max_level = 0u64;
    for row in &data.rows {
        let level = row
            .get("errlevel")
            .and_then(Json::as_u64)
            .ok_or_else(|| protocol("SQL 检查级别无效"))?;
        max_level = max_level.max(level);
    }
    let error = data
        .error
        .as_deref()
        .is_some_and(|v| !v.is_empty() && v != "None");
    let warning = data
        .warning
        .as_deref()
        .is_some_and(|v| !v.is_empty() && v != "None");
    let verdict = if error || data.error_count > 0 || max_level >= 2 {
        "error"
    } else if warning || data.warning_count > 0 || max_level == 1 {
        "warning"
    } else {
        "pass"
    };
    Ok(CheckResponse {
        rows: data.rows,
        warnings: data.warning_count,
        errors: data.error_count,
        verdict,
    })
}

/// SQL 检查是独立 REST 操作，不经过只读查询检查，也不创建工单。
pub(super) async fn sql_check(
    session: &ArcherySession,
    params: &Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    if session.profile.provider != ProviderKind::Mysql {
        return Err(AppError::new(
            "capability_unavailable",
            "仅 MySQL 支持 SQL 检查",
        ));
    }
    let database: String = model::param(params, "database")?;
    let connection = target(session, &database)?;
    let input = text(params)?;
    model::ensure_not_cancelled(cancel)?;
    let data: CheckDto = session
        .auth
        .post_execution(
            "api/v1/workflow/sqlcheck/",
            &[
                ("instance_id", connection.instance_id.to_string()),
                ("db_name", database.clone()),
                ("full_sql", input),
            ],
            true,
            sink,
            cancel,
        )
        .await?;
    let checked = decode_check(data)?;
    sink.mark_remote_completed().await?;
    let names = [
        "id",
        "stage",
        "errlevel",
        "stagestatus",
        "errormessage",
        "sql",
        "affected_rows",
        "sequence",
        "backup_dbname",
        "execute_time",
        "sqlsha1",
        "backup_time",
        "actual_affected_rows",
    ];
    let mut columns: Vec<_> = names
        .iter()
        .map(|name| Column::new(*name, "platform_value"))
        .collect();
    columns.push(Column::new("platform_details", "JSON"));
    let summary = json!({"origin":{"source":"archery","server_id":connection.server_id,"instance_id":connection.instance_id,"database":database,"provider":"mysql","readonly_policy":"1"},"coverage":{"scope":"returned_response","returned_rows":checked.rows.len(),"original_query":"unknown"},"fidelity":{"kind":"platform_values","unknown":["original_types"]},"masking":"unknown","check_state":"completed","verdict":checked.verdict,"warning_count":checked.warnings,"error_count":checked.errors});
    bounded(&summary, 32 * 1024)?;
    bounded(&json!({"columns":columns,"summary":summary}), 256 * 1024)?;
    let id = sink.start(columns, "rows").await?;
    let mut batch = Vec::with_capacity(128);
    for row in checked.rows {
        model::ensure_not_cancelled(cancel)?;
        let mut values: Vec<_> = names
            .iter()
            .map(|name| {
                cell(
                    row.get(*name).cloned().unwrap_or(Json::Null),
                    "",
                    false,
                    false,
                )
            })
            .collect();
        values.push(Value::Json(Json::Object(row)));
        batch.push(values);
        if batch.len() == 128 {
            sink.append(id, std::mem::take(&mut batch)).await?;
        }
    }
    if !batch.is_empty() {
        sink.append(id, batch).await?;
    }
    model::ensure_not_cancelled(cancel)?;
    sink.finish(id, summary).await?;
    Ok(
        json!({"results":[id],"result_id":id,"check_state":"completed","verdict":checked.verdict,"warning_count":checked.warnings,"error_count":checked.errors}),
    )
}
