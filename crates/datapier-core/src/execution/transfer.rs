//! CSV／版本化 JSON 的文件导入、导出和有限样本预览。
//! 结果按页读取、导入按批提交；失败报告保留已完成进度，不暗示跨批回滚。

mod export;
mod json;
mod mysql_import;
mod preview;
mod redis_export;
mod redis_import;

use self::json::{JsonReader, JsonSpool, json_document};
#[cfg(test)]
use export::{ExportFile, export_cached, finish_export};
use export::{csv_error, export, flush_csv};
use mysql_import::{MysqlImportInput, import_mysql_csv, import_mysql_json};
pub use preview::preview;
use redis_export::export_redis;
use redis_import::{import_redis_csv, import_redis_json};

#[cfg(test)]
#[path = "../../tests/unit/transfer.rs"]
mod tests;

use crate::{
    ExecutionSink,
    model::{
        AppError, Column, ProviderKind, ResourcePage, ResourceTarget, Result, TableSchema, Value,
        ensure_not_cancelled, param,
    },
    providers::ProviderSession,
};
use serde_json::{Value as Json, json};
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{BufReader, BufWriter, Read, Write},
    path::{Path, PathBuf},
    str::FromStr,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const PAGE: usize = 500;
const BYTES: usize = 1024 * 1024;
const REDIS_HEADER: [&str; 11] = [
    "version",
    "record",
    "key_b64",
    "type",
    "expires_at_ms",
    "field_b64",
    "index",
    "member_b64",
    "score",
    "value_b64",
    "field_expires_at_ms",
];

// 只统计已确认的行和 key；错误中另附剩余工作与可能的部分文件路径。
#[derive(Default)]
struct Progress {
    rows: u64,
    keys: u64,
    failed: u64,
    skipped: u64,
    unknown: u64,
    partial: Option<PathBuf>,
}
impl Progress {
    fn details(&self) -> Json {
        json!({"completed_rows":self.rows,"completed_keys":self.keys,"failed":self.failed,"skipped":self.skipped,"unknown":self.unknown,"partial_path":self.partial,"remaining":if self.unknown > 0 { "after_current_batch_not_executed" } else { "not_executed" }})
    }
}

/// 执行导入或导出；导入要求读写连接，导出不覆盖已有目标文件。
/// 失败时错误详情包含已确认进度及可能保留的部分文件，取消不撤销已生效的批次。
pub async fn run(
    session: &mut ProviderSession,
    method: &str,
    params: Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    validate_options(&params)?;
    let mut progress = Progress::default();
    let result = match method {
        "transfer.export" | "export" => export(session, &params, sink, cancel, &mut progress).await,
        "transfer.import" | "import" => import(session, &params, sink, cancel, &mut progress).await,
        _ => Err(AppError::invalid("未知数据交换方法")),
    };
    with_progress(result, &progress)
}
fn with_progress(result: Result<Json>, progress: &Progress) -> Result<Json> {
    result.map_err(|mut e| {
        let original = e.details;
        e.details = Box::new(json!({"progress":progress.details(),"cause":original}));
        e
    })
}
fn validate_options(params: &Json) -> Result<()> {
    if params
        .get("column_mapping")
        .is_some_and(|v| !v.is_null() && !v.is_object())
    {
        return Err(AppError::invalid("column_mapping 必须是对象"));
    }
    if params.get("binary_columns").is_some_and(|v| {
        !v.is_null()
            && v.as_array()
                .is_none_or(|a| a.iter().any(|v| !v.is_string()))
    }) {
        return Err(AppError::invalid("binary_columns 必须是字符串数组"));
    }
    if params
        .get("backslash_escape")
        .is_some_and(|v| !v.is_null() && !v.is_boolean())
    {
        return Err(AppError::invalid("backslash_escape 必须是布尔值"));
    }
    Ok(())
}

fn format(params: &Json) -> Result<&str> {
    match params.get("format").and_then(Json::as_str) {
        Some("csv") => Ok("csv"),
        Some("json") => Ok("json"),
        _ => Err(AppError::invalid("format 必须为 csv 或 json")),
    }
}
fn path(params: &Json) -> Result<PathBuf> {
    let s: String = param(params, "path")?;
    if s.trim().is_empty() {
        return Err(AppError::invalid("文件路径不能为空"));
    }
    Ok(PathBuf::from(s))
}
fn temp_path(path: &Path, label: &str) -> PathBuf {
    let mut p = path.as_os_str().to_os_string();
    p.push(format!(".datapier-{label}-{}", Uuid::new_v4()));
    PathBuf::from(p)
}
fn create_temp(path: &Path, progress: &mut Progress) -> Result<(PathBuf, File)> {
    let tmp = temp_path(path, "partial");
    let file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    progress.partial = Some(tmp.clone());
    Ok((tmp, file))
}
// 同目录硬链接以不覆盖方式发布；预先检查不能消除目标文件的创建竞态。
fn publish_export(tmp: &Path, dst: &Path) -> Result<()> {
    fs::hard_link(tmp, dst).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            AppError::conflict("导出目标文件已存在")
        } else {
            e.into()
        }
    })?;
    fs::remove_file(tmp)?;
    Ok(())
}
fn first_result(v: &Json) -> Result<Uuid> {
    let x = v
        .get("results")
        .and_then(Json::as_array)
        .and_then(|a| a.first())
        .ok_or_else(|| AppError::invalid("数据库未返回结果集"))?;
    serde_json::from_value(x.clone()).map_err(Into::into)
}
async fn browse(
    session: &mut ProviderSession,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Uuid> {
    first_result(
        &session
            .call("resource.browse", json!({"target":target}), sink, cancel)
            .await?,
    )
}
async fn full_row(sink: &ExecutionSink, mut row: Vec<Value>) -> Result<Vec<Value>> {
    for v in &mut row {
        if let Value::Large { id, .. } = v {
            *v = sink.store.read_value(*id).await?;
        }
    }
    Ok(row)
}

fn redis_bytes(v: &Value) -> Result<String> {
    match v {
        Value::Bytes(s) => Ok(s.clone()),
        _ => Err(AppError::invalid("Redis 内容必须是二进制类型")),
    }
}
pub(crate) fn validate_redis_score(score: &str) -> Result<()> {
    if score
        .strip_prefix('+')
        .or_else(|| score.strip_prefix('-'))
        .unwrap_or(score)
        .eq_ignore_ascii_case("inf")
        || score.parse::<f64>().is_ok_and(f64::is_finite)
    {
        Ok(())
    } else {
        Err(AppError::invalid("无效 ZSet 分数"))
    }
}
fn redis_csv_row(key: &str, ty: &str, row: &[Value]) -> Result<Vec<String>> {
    let mut r = vec![
        "2".into(),
        "item".into(),
        key.into(),
        ty.into(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ];
    match (ty, row) {
        ("string", [v]) => r[9] = redis_bytes(v)?,
        ("hash", [f, v, Value::Integer(expiry)]) => {
            let expiry_num = expiry
                .parse::<i64>()
                .map_err(|_| AppError::invalid("无效 Hash 字段期限"))?;
            if expiry_num < -1 {
                return Err(AppError::invalid("无效 Hash 字段期限"));
            }
            r[5] = redis_bytes(f)?;
            r[9] = redis_bytes(v)?;
            r[10] = expiry.clone();
        }
        ("list", [i, v]) => {
            r[6] = match i {
                Value::Integer(s) | Value::Unsigned(s) => s.clone(),
                _ => return Err(AppError::invalid("List 索引类型错误")),
            };
            r[9] = redis_bytes(v)?;
        }
        ("set", [m]) => r[7] = redis_bytes(m)?,
        ("zset", [m, Value::Float(s)]) => {
            validate_redis_score(s)?;
            r[7] = redis_bytes(m)?;
            r[8] = s.clone();
        }
        _ => return Err(AppError::invalid("Redis 行格式错误")),
    }
    Ok(r)
}

async fn import(
    session: &mut ProviderSession,
    params: &Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
) -> Result<Json> {
    if session.profile().is_archery() {
        return Err(AppError::new(
            "capability_unavailable",
            "Archery 不支持导入",
        ));
    }
    if session.profile().access_mode != crate::model::AccessMode::ReadWrite {
        return Err(AppError::new("permission_denied", "只读连接不能导入"));
    }
    let fmt = format(params)?;
    let source = path(params)?;
    let file = File::open(&source)?;
    let target: ResourceTarget = param(params, "target")?;
    let id = Uuid::new_v4();
    let owner = sink.store.task(sink.execution_id).await?.owner;
    let budget = sink.store.available_memory_for_owner(owner).await?;
    if budget == 0 {
        return Err(AppError::new("resource_limit", "客户端内存预算已用尽"));
    }
    sink.store
        .reserve_auxiliary(sink.execution_id, id, budget)
        .await?;
    let reservation = Some((id, budget));
    let budget = reservation.map(|(_, bytes)| bytes).unwrap_or(0);
    let result = match (session.profile().provider, fmt) {
        (ProviderKind::Mysql, "csv") => {
            import_mysql_csv(
                session,
                params,
                &target,
                sink,
                cancel,
                p,
                MysqlImportInput { file, budget },
            )
            .await
        }
        (ProviderKind::Mysql, _) => {
            import_mysql_json(
                session,
                params,
                &target,
                sink,
                cancel,
                p,
                MysqlImportInput { file, budget },
            )
            .await
        }
        (ProviderKind::Redis, "csv") => {
            import_redis_csv(session, &target, sink, cancel, p, file, budget).await
        }
        (ProviderKind::Redis, _) => {
            import_redis_json(session, &target, sink, cancel, p, file, budget).await
        }
    };
    if let Some((id, _)) = reservation {
        sink.store.release_auxiliary(id).await?;
    }
    result?;
    Ok(json!({"path":source,"progress":p.details()}))
}
struct ColumnProjection {
    columns: Vec<Column>,
    indices: Vec<usize>,
    source_len: usize,
}
fn mapped_columns(
    names: &[String],
    params: &Json,
    schema: &TableSchema,
) -> Result<ColumnProjection> {
    let map = match params.get("column_mapping") {
        None | Some(Json::Null) => None,
        Some(Json::Object(m)) => Some(m),
        _ => return Err(AppError::invalid("column_mapping 必须是对象")),
    };
    let mut used = HashSet::new();
    let mut source_names = HashSet::new();
    let mut columns = Vec::new();
    let mut indices = Vec::new();
    for (index, name) in names.iter().enumerate() {
        if !source_names.insert(name) {
            return Err(AppError::invalid("源文件包含重复列名"));
        }
        let explicit = map.and_then(|m| m.get(name));
        let dest = match explicit {
            Some(Json::Null) => continue,
            Some(Json::String(s)) => s.as_str(),
            Some(_) => return Err(AppError::invalid("列映射必须是目标列名或 null")),
            None => name.as_str(),
        };
        let column = schema
            .columns
            .iter()
            .find(|c| c.name == dest)
            .ok_or_else(|| AppError::invalid(format!("目标列不存在: {dest}")))?;
        if column.generated {
            if explicit.is_none() {
                continue;
            }
            return Err(AppError::invalid(format!("不能映射到生成列: {dest}")));
        }
        if !used.insert(dest.to_owned()) {
            return Err(AppError::invalid("多个源列映射到同一个目标列"));
        }
        columns.push(column.clone());
        indices.push(index);
    }
    if columns.is_empty() {
        return Err(AppError::invalid("没有可导入的目标列"));
    }
    Ok(ColumnProjection {
        columns,
        indices,
        source_len: names.len(),
    })
}
impl ColumnProjection {
    fn project(&self, row: Vec<Value>) -> Result<Vec<Value>> {
        if row.len() != self.source_len {
            return Err(AppError::invalid("JSON 行列数不匹配"));
        }
        let mut indices = self.indices.iter().copied().peekable();
        Ok(row
            .into_iter()
            .enumerate()
            .filter_map(|(i, v)| {
                if indices.peek() == Some(&i) {
                    indices.next();
                    Some(v)
                } else {
                    None
                }
            })
            .collect())
    }
    fn csv_row(
        &self,
        record: &csv::StringRecord,
        names: &[String],
        null: &str,
        escaped: bool,
        binary: &HashSet<&str>,
    ) -> Result<Vec<Value>> {
        if record.len() != self.source_len {
            return Err(AppError::invalid("CSV 记录列数不匹配"));
        }
        self.indices
            .iter()
            .zip(&self.columns)
            .map(|(&i, column)| {
                let field = &record[i];
                if field == null {
                    Ok(Value::Null)
                } else {
                    let s = if escaped {
                        unescape(field)?
                    } else {
                        field.to_owned()
                    };
                    csv_typed(s, column, binary.contains(names[i].as_str()))
                }
            })
            .collect()
    }
}
async fn schema(
    session: &mut ProviderSession,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<TableSchema> {
    Ok(serde_json::from_value(
        session
            .call("resource.describe", json!({"target":target}), sink, cancel)
            .await?,
    )?)
}
// 只计入 provider 明确确认的成功行；未知或部分结果随错误返回，不自动重试。
async fn insert_batch(
    session: &mut ProviderSession,
    target: &ResourceTarget,
    columns: &[Column],
    rows: &mut Vec<Vec<Value>>,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    ensure_not_cancelled(cancel)?;
    let count = rows.len();
    let batch = std::mem::take(rows);
    let response = session.call("import.rows", json!({"target":target,"columns":columns.iter().map(|c| &c.name).collect::<Vec<_>>(),"rows":batch}), sink, cancel).await;
    match response {
        Ok(v) => {
            if let Some(items) = v.get("items").and_then(Json::as_array) {
                let success = items
                    .iter()
                    .filter(|i| {
                        matches!(
                            i.get("status").and_then(Json::as_str),
                            Some("applied" | "succeeded" | "inserted")
                        )
                    })
                    .count();
                p.rows += success as u64;
                p.failed += items
                    .iter()
                    .filter(|i| i.get("status").and_then(Json::as_str) == Some("failed"))
                    .count() as u64;
                p.unknown += items
                    .iter()
                    .filter(|i| i.get("status").and_then(Json::as_str) == Some("unknown"))
                    .count() as u64;
                if success != count {
                    return Err(
                        AppError::new("import_failed", "导入批次包含失败或未确认的记录")
                            .with_details(v),
                    );
                }
            } else if v.get("affected_rows").and_then(Json::as_u64) == Some(count as u64) {
                p.rows += count as u64;
            } else {
                p.unknown += count as u64;
                return Err(AppError::unknown("数据库未返回可确认的导入批次结果").with_details(v));
            }
            Ok(())
        }
        Err(e) => {
            if let Some(items) = e.details.get("items").and_then(Json::as_array) {
                p.rows += items
                    .iter()
                    .filter(|i| {
                        matches!(
                            i.get("status").and_then(Json::as_str),
                            Some("applied" | "succeeded" | "inserted")
                        )
                    })
                    .count() as u64;
                p.failed += items
                    .iter()
                    .filter(|i| i.get("status").and_then(Json::as_str) == Some("failed"))
                    .count() as u64;
                p.unknown += items
                    .iter()
                    .filter(|i| i.get("status").and_then(Json::as_str) == Some("unknown"))
                    .count() as u64;
            } else if e.code == "outcome_unknown" {
                p.unknown += count as u64;
            }
            Err(e)
        }
    }
}
fn unescape(s: &str) -> Result<String> {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' && chars.next() != Some('\\') {
            return Err(AppError::invalid("CSV 反斜线转义不完整"));
        }
        out.push(c);
    }
    Ok(out)
}
fn csv_typed(s: String, col: &Column, binary: bool) -> Result<Value> {
    let ty = col.type_name.to_ascii_uppercase();
    let base = ty
        .split(|c: char| c == '(' || c.is_ascii_whitespace())
        .next()
        .unwrap_or("");
    if binary
        || matches!(
            base,
            "BINARY" | "VARBINARY" | "TINYBLOB" | "BLOB" | "MEDIUMBLOB" | "LONGBLOB" | "BIT"
        )
    {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(&s)
            .map_err(|e| AppError::invalid(format!("无效 Base64: {e}")))?;
        Ok(Value::Bytes(s))
    } else if matches!(
        base,
        "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BIGINT" | "YEAR"
    ) {
        if ty.contains("UNSIGNED") || base == "YEAR" {
            u64::from_str(&s).map_err(|_| AppError::invalid("无效无符号整数"))?;
            Ok(Value::Unsigned(s))
        } else {
            i64::from_str(&s).map_err(|_| AppError::invalid("无效整数"))?;
            Ok(Value::Integer(s))
        }
    } else if matches!(base, "DECIMAL" | "NUMERIC") {
        bigdecimal::BigDecimal::from_str(&s).map_err(|_| AppError::invalid("无效精确小数"))?;
        Ok(Value::Decimal(s))
    } else if matches!(base, "FLOAT" | "DOUBLE" | "REAL") {
        f64::from_str(&s).map_err(|_| AppError::invalid("无效浮点数"))?;
        Ok(Value::Float(s))
    } else if base == "JSON" {
        Ok(Value::Json(serde_json::from_str(&s)?))
    } else if matches!(base, "DATETIME" | "TIMESTAMP") {
        Ok(Value::DateTime(s))
    } else if base == "DATE" {
        Ok(Value::Date(s))
    } else if base == "TIME" {
        Ok(Value::Time(s))
    } else {
        Ok(Value::Text(s))
    }
}

struct RedisKeyHeader {
    key: String,
    kind: String,
    expiry: Json,
}

// 一次仅构造一个临时 key；只有 provider 确认发布后才计入完成进度。
struct KeyImport {
    token: Json,
    key: String,
    kind: String,
    batch: Vec<Vec<Value>>,
    batch_bytes: usize,
    rows: u64,
    next_index: u64,
}
fn valid_b64(s: &str) -> Result<()> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(s)
        .map_err(|e| AppError::invalid(format!("无效 Base64: {e}")))?;
    Ok(())
}
async fn begin_key(
    session: &mut ProviderSession,
    target: &ResourceTarget,
    header: RedisKeyHeader,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &Progress,
) -> Result<KeyImport> {
    let RedisKeyHeader { key, kind, expiry } = header;
    ensure_not_cancelled(cancel)?;
    valid_b64(&key)?;
    if !matches!(kind.as_str(), "string" | "hash" | "list" | "set" | "zset") {
        return Err(AppError::invalid("不支持的 Redis 类型"));
    }
    if !expiry.is_null()
        && expiry
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .is_none()
    {
        return Err(AppError::invalid("无效绝对过期时间"));
    }
    if target.key_b64.is_some() && p.keys + p.skipped > 0 {
        return Err(AppError::invalid("指定目标 key 时文件只能包含一个 key"));
    }
    let mut actual = target.clone();
    actual.key_b64 = Some(target.key_b64.clone().unwrap_or_else(|| key.clone()));
    let response = session
        .call(
            "import.begin",
            json!({"target":actual,"type":kind,"expires_at_ms":expiry}),
            sink,
            cancel,
        )
        .await?;
    let token = response
        .get("token")
        .cloned()
        .ok_or_else(|| AppError::invalid("Redis 导入未返回 token"))?;
    Ok(KeyImport {
        token,
        key,
        kind,
        batch: Vec::new(),
        batch_bytes: 0,
        rows: 0,
        next_index: 0,
    })
}
async fn append_key(
    session: &mut ProviderSession,
    key: &mut KeyImport,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<()> {
    if key.batch.is_empty() {
        return Ok(());
    }
    ensure_not_cancelled(cancel)?;
    let rows = std::mem::take(&mut key.batch);
    key.batch_bytes = 0;
    let count = rows.len() as u64;
    session
        .call(
            "import.append",
            json!({"token":key.token,"rows":rows}),
            sink,
            cancel,
        )
        .await?;
    key.rows += count;
    Ok(())
}
async fn push_key(
    session: &mut ProviderSession,
    key: &mut KeyImport,
    row: Vec<Value>,
    charge: usize,
    budget: usize,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<()> {
    // 写入临时 key 前校验与 provider 浏览相同的行布局和 List 索引。
    validate_key_row(key, &row)?;
    if charge > budget / 2 {
        return Err(AppError::new("resource_limit", "Redis 导入行超过内存预算"));
    }
    if !key.batch.is_empty() && key.batch_bytes.saturating_add(charge) > budget / 2 {
        append_key(session, key, sink, cancel).await?;
    }
    key.batch.push(row);
    key.batch_bytes = key.batch_bytes.saturating_add(charge);
    if key.batch.len() == PAGE {
        append_key(session, key, sink, cancel).await?;
    }
    Ok(())
}
fn validate_key_row(key: &mut KeyImport, row: &[Value]) -> Result<()> {
    redis_csv_row(&key.key, &key.kind, row)?;
    if key.kind == "list" {
        let index = match &row[0] {
            Value::Integer(s) | Value::Unsigned(s) => s.parse::<u64>().ok(),
            _ => None,
        };
        if index != Some(key.next_index) {
            return Err(AppError::invalid("List 索引不连续"));
        }
        key.next_index += 1;
    }
    if key.kind == "string" && key.rows + key.batch.len() as u64 != 0 {
        return Err(AppError::invalid("String 只能包含一行"));
    }
    for value in row {
        if let Value::Bytes(s) = value {
            valid_b64(s)?;
        }
    }
    Ok(())
}
async fn finish_key(
    session: &mut ProviderSession,
    key: &mut KeyImport,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
) -> Result<()> {
    append_key(session, key, sink, cancel).await?;
    ensure_not_cancelled(cancel)?;
    if key.rows == 0 {
        return Err(AppError::invalid("Redis key 内容为空，不能创建空集合"));
    }
    let response = session
        .call("import.finish", json!({"token":key.token}), sink, cancel)
        .await?;
    if response.get("skipped").and_then(Json::as_bool) == Some(true) {
        p.skipped += 1;
    } else if response.get("published").and_then(Json::as_bool) == Some(true)
        || response.get("imported").and_then(Json::as_bool) == Some(true)
    {
        p.keys += 1;
        p.rows += key.rows;
    } else {
        return Err(AppError::unknown("Redis 未确认 key 发布结果").with_details(response));
    }
    Ok(())
}
// 清理使用独立取消信号，避免原任务取消后跳过临时 key 的删除。
async fn discard_key(
    session: &mut ProviderSession,
    key: &KeyImport,
    sink: &ExecutionSink,
) -> Result<()> {
    let mut cleanup_sink = sink.clone();
    cleanup_sink.cancel = CancellationToken::new();
    session
        .call(
            "import.discard",
            json!({"token":key.token}),
            &cleanup_sink,
            &cleanup_sink.cancel,
        )
        .await
        .map(|_| ())
}
async fn cleanup_key(
    session: &mut ProviderSession,
    active: &Option<KeyImport>,
    sink: &ExecutionSink,
    error: AppError,
) -> AppError {
    if let Some(key) = active
        && let Err(cleanup) = discard_key(session, key, sink).await
    {
        return AppError::new(&error.code, error.message.clone()).with_details(
            json!({"error":error,"cleanup_error":cleanup,"temporary_key_cleanup":"failed"}),
        );
    }
    error
}
fn csv_item(record: &csv::StringRecord, key: &KeyImport) -> Result<Vec<Value>> {
    if &record[0] != "2" || &record[1] != "item" || record[2] != key.key || record[3] != key.kind {
        return Err(AppError::invalid("Redis CSV 元素不属于当前 key"));
    }
    let bytes = |n: usize| -> Result<Value> {
        valid_b64(&record[n])?;
        Ok(Value::Bytes(record[n].to_owned()))
    };
    match key.kind.as_str() {
        "string" => Ok(vec![bytes(9)?]),
        "hash" => Ok(vec![
            bytes(5)?,
            bytes(9)?,
            Value::Integer(record[10].to_owned()),
        ]),
        "list" => {
            record[6]
                .parse::<u64>()
                .map_err(|_| AppError::invalid("无效 List 索引"))?;
            Ok(vec![Value::Unsigned(record[6].to_owned()), bytes(9)?])
        }
        "set" => Ok(vec![bytes(7)?]),
        "zset" => {
            validate_redis_score(&record[8])?;
            Ok(vec![bytes(7)?, Value::Float(record[8].to_owned())])
        }
        _ => Err(AppError::invalid("不支持的 Redis 类型")),
    }
}
