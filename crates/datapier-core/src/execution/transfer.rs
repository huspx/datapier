//! CSV／版本化 JSON 的文件导入、导出和有限样本预览。
//! 结果按页读取、导入按批提交；失败报告保留已完成进度，不暗示跨批回滚。

use crate::{
    ExecutionSink,
    model::{
        AppError, Column, ProviderKind, ResourcePage, ResourceTarget, Result, TableSchema, Value,
        ensure_not_cancelled, param,
    },
    providers::ProviderSession,
};
use serde_json::{Value as Json, json};
use sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
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
const REDIS_HEADER: [&str; 10] = [
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
];

// 只统计已确认的行和 key；错误中另附剩余工作与可能的部分文件路径。
#[derive(Default)]
struct Progress {
    rows: u64,
    keys: u64,
    failed: u64,
    skipped: u64,
    partial: Option<PathBuf>,
}
impl Progress {
    fn details(&self) -> Json {
        json!({"completed_rows":self.rows,"completed_keys":self.keys,"failed":self.failed,"skipped":self.skipped,"partial_path":self.partial,"remaining":"not_executed"})
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
    let mut progress = Progress::default();
    let result = match method {
        "transfer.export" | "export" => export(session, &params, sink, cancel, &mut progress).await,
        "transfer.import" | "import" => import(session, &params, sink, cancel, &mut progress).await,
        _ => Err(AppError::invalid("未知数据交换方法")),
    };
    result.map_err(|mut e| {
        let original = e.details;
        e.details = Box::new(json!({"progress":progress.details(),"cause":original}));
        e
    })
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
    if s.is_empty() {
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

async fn export(
    session: &mut ProviderSession,
    params: &Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    progress: &mut Progress,
) -> Result<Json> {
    let fmt = format(params)?;
    let dst = path(params)?;
    match fs::symlink_metadata(&dst) {
        Ok(_) => return Err(AppError::conflict("导出目标文件已存在")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    let (tmp, file) = create_temp(&dst, progress)?;
    let result = match session.profile().provider {
        ProviderKind::Mysql => {
            export_mysql(session, params, sink, cancel, progress, fmt, file).await
        }
        ProviderKind::Redis => {
            export_redis(session, params, sink, cancel, progress, fmt, file).await
        }
    };
    result?;
    ensure_not_cancelled(cancel)?;
    publish_export(&tmp, &dst)?;
    progress.partial = None;
    Ok(json!({"path":dst,"format":fmt,"progress":progress.details()}))
}

async fn export_mysql(
    session: &mut ProviderSession,
    params: &Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    fmt: &str,
    file: File,
) -> Result<()> {
    // 指定已有结果时只读缓存，不重新执行原查询；只有完整结果可全量导出。
    let id = if params.get("result_id").is_some() {
        param(params, "result_id")?
    } else {
        let t: ResourceTarget = param(params, "target")?;
        browse(session, &t, sink, cancel).await?
    };
    let meta = sink.store.result_meta(id).await?;
    if !meta.complete {
        return Err(AppError::new(
            "incomplete_result",
            "结果尚未完整，不能作为全量导出",
        ));
    }
    let mut writer = BufWriter::new(file);
    if fmt == "json" {
        write!(
            writer,
            "{{\"format\":\"datapier\",\"version\":1,\"columns\":{},\"rows\":[",
            serde_json::to_string(&meta.columns)?
        )?;
    }
    let mut csv = if fmt == "csv" {
        let mut c = csv::Writer::from_writer(Vec::new());
        c.write_record(meta.columns.iter().map(|c| c.name.as_str()))
            .map_err(csv_error)?;
        Some(c)
    } else {
        None
    };
    let mut start = 0;
    let mut first = true;
    loop {
        ensure_not_cancelled(cancel)?;
        let page = sink.store.result_page(id, start, PAGE, BYTES).await?;
        for row in page.rows {
            let row = full_row(sink, row).await?;
            if let Some(c) = csv.as_mut() {
                c.write_record(row.iter().map(csv_value).collect::<Result<Vec<_>>>()?)
                    .map_err(csv_error)?;
                flush_csv(c, &mut writer, false)?;
            } else {
                if !first {
                    writer.write_all(b",")?;
                }
                serde_json::to_writer(&mut writer, &row)?;
                first = false;
            }
            p.rows += 1;
        }
        match page.next {
            Some(next) if next > start => start = next,
            Some(_) => return Err(AppError::new("result_page", "结果分页没有推进")),
            None => break,
        }
    }
    if let Some(c) = csv.as_mut() {
        flush_csv(c, &mut writer, true)?;
    } else {
        writer.write_all(b"]}")?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}
fn csv_error(e: csv::Error) -> AppError {
    AppError::new("csv", e.to_string())
}
fn flush_csv(c: &mut csv::Writer<Vec<u8>>, out: &mut impl Write, force: bool) -> Result<()> {
    c.flush()?;
    if force || c.get_ref().len() >= 64 * 1024 {
        let old = std::mem::replace(c, csv::Writer::from_writer(Vec::new()));
        let mut data = old
            .into_inner()
            .map_err(|e| AppError::new("csv", e.to_string()))?;
        out.write_all(&data)?;
        data.clear();
        *c = csv::Writer::from_writer(data);
    }
    Ok(())
}
// NULL 使用独立标记；普通值的反斜线加倍，使字面量 `\N` 可以往返。
fn csv_value(v: &Value) -> Result<String> {
    let s = match v {
        Value::Null => return Ok("\\N".into()),
        Value::Missing | Value::Large { .. } => {
            return Err(AppError::invalid("无法导出缺失值或未展开的大字段"));
        }
        Value::Bool(b) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
        Value::Bytes(s)
        | Value::Text(s)
        | Value::Integer(s)
        | Value::Unsigned(s)
        | Value::Decimal(s)
        | Value::Float(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s) => s.clone(),
        Value::Json(v) => serde_json::to_string(v)?,
        Value::Array(_) | Value::Map(_) => return Err(AppError::invalid("CSV 不支持复合值")),
    };
    Ok(s.replace('\\', "\\\\"))
}

struct RedisExportWriter {
    out: BufWriter<File>,
    csv: Option<csv::Writer<Vec<u8>>>,
    first: bool,
}

async fn export_redis(
    session: &mut ProviderSession,
    params: &Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    fmt: &str,
    file: File,
) -> Result<()> {
    let mut out = BufWriter::new(file);
    let csv = if fmt == "csv" {
        let mut c = csv::Writer::from_writer(Vec::new());
        c.write_record(REDIS_HEADER).map_err(csv_error)?;
        Some(c)
    } else {
        out.write_all(b"{\"format\":\"datapier\",\"version\":1,\"records\":[")?;
        None
    };
    let mut writer = RedisExportWriter {
        out,
        csv,
        first: true,
    };
    let requested_target: Option<ResourceTarget> = params
        .get("target")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()?;
    if let Some(db) = requested_target
        .as_ref()
        .and_then(|t| t.database.as_deref())
        && db.parse::<u32>().ok() != Some(session.profile().redis_db)
    {
        return Err(AppError::invalid("目标数据库与连接不一致"));
    }
    if let Some(target) = requested_target.as_ref().filter(|t| t.key_b64.is_some()) {
        export_key(session, target, sink, cancel, p, &mut writer).await?;
    } else {
        // SCAN 可能重复返回 key；以磁盘索引去重，避免将全库 key 保存在内存。
        let seen_path = temp_path(&sink.store.root, "scan");
        let opts = SqliteConnectOptions::new()
            .filename(&seen_path)
            .create_if_missing(true);
        let mut seen = SqliteConnection::connect_with(&opts).await?;
        sqlx::query("CREATE TABLE seen (key TEXT PRIMARY KEY)")
            .execute(&mut seen)
            .await?;
        let scan_result: Result<()> = async {
            let mut cursor: Option<String> = None;
            loop { ensure_not_cancelled(cancel)?;
                let page: ResourcePage = serde_json::from_value(session.call("resource.list", json!({"cursor":cursor,"pattern":params.get("pattern"),"type":params.get("type"),"limit":500}), sink, cancel).await?)?;
                for resource in page.resources { let key = resource.key_b64.ok_or_else(|| AppError::invalid("Redis 资源缺少 key_b64"))?;
                    let inserted = sqlx::query("INSERT OR IGNORE INTO seen(key) VALUES (?)").bind(&key).execute(&mut seen).await?.rows_affected();
                    if inserted != 0 { let target = ResourceTarget { database: resource.database, name: resource.name, key_b64: Some(key), kind: resource.kind }; export_key(session, &target, sink, cancel, p, &mut writer).await?; }
                }
                if page.complete || page.cursor.is_none() { break; } cursor = page.cursor;
            } Ok(())
        }.await;
        seen.close().await?;
        let _ = fs::remove_file(seen_path);
        scan_result?;
    }
    if let Some(c) = writer.csv.as_mut() {
        flush_csv(c, &mut writer.out, true)?;
    } else {
        writer.out.write_all(b"]}")?;
    }
    writer.out.flush()?;
    writer.out.get_ref().sync_all()?;
    Ok(())
}
async fn export_key(
    session: &mut ProviderSession,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    writer: &mut RedisExportWriter,
) -> Result<()> {
    let RedisExportWriter { out, csv, first } = writer;
    ensure_not_cancelled(cancel)?;
    let key = target
        .key_b64
        .as_deref()
        .ok_or_else(|| AppError::invalid("Redis target 缺少 key_b64"))?;
    let meta = session
        .call("export.meta", json!({"target":target}), sink, cancel)
        .await?;
    let ty = meta
        .get("type")
        .and_then(Json::as_str)
        .ok_or_else(|| AppError::invalid("Redis key 缺少类型"))?;
    if !matches!(ty, "string" | "hash" | "list" | "set" | "zset") {
        return Err(AppError::invalid(format!("不支持 Redis 类型 {ty}")));
    }
    // 导出绝对过期时间，导入时不会因传输耗时延长 key 生命周期。
    let expiry = meta.get("expires_at_ms").cloned().unwrap_or(Json::Null);
    let exp = expiry.as_str().unwrap_or("permanent");
    let browsed = session
        .call("resource.browse", json!({"target":target}), sink, cancel)
        .await?;
    validate_redis_browse_type(ty, &browsed)?;
    let id = first_result(&browsed)?;
    let result_meta = sink.store.result_meta(id).await?;
    if !result_meta.complete {
        return Err(AppError::new("incomplete_result", "Redis 浏览结果不完整"));
    }
    if let Some(c) = csv.as_mut() {
        c.write_record(["1", "key", key, ty, exp, "", "", "", "", ""])
            .map_err(csv_error)?;
        flush_csv(c, out, false)?;
    } else {
        if !*first {
            out.write_all(b",")?;
        }
        *first = false;
        write!(
            out,
            "{{\"key_b64\":{},\"type\":{},\"expires_at_ms\":{},\"rows\":[",
            serde_json::to_string(key)?,
            serde_json::to_string(ty)?,
            expiry
        )?;
    }
    let mut row_first = true;
    let mut start = 0;
    let mut row_count = 0u64;
    loop {
        ensure_not_cancelled(cancel)?;
        let page = sink.store.result_page(id, start, PAGE, BYTES).await?;
        for row in page.rows {
            let row = full_row(sink, row).await?;
            let fields = redis_csv_row(key, ty, &row)?;
            if ty == "list" && fields[6].parse::<u64>().ok() != Some(row_count) {
                return Err(AppError::conflict("Redis List 索引不连续"));
            }
            if ty == "zset" && fields[8].parse::<f64>().is_err() {
                return Err(AppError::invalid("Redis ZSet 分数无效"));
            }
            for value in &row {
                if let Value::Bytes(s) = value {
                    valid_b64(s)?;
                }
            }
            if ty == "string" && row_count != 0 {
                return Err(AppError::conflict("Redis String 包含多行"));
            }
            if let Some(c) = csv.as_mut() {
                c.write_record(fields).map_err(csv_error)?;
                flush_csv(c, out, false)?;
            } else {
                if !row_first {
                    out.write_all(b",")?;
                }
                serde_json::to_writer(&mut *out, &row)?;
                row_first = false;
            }
            p.rows += 1;
            row_count += 1;
        }
        match page.next {
            Some(n) if n > start => start = n,
            Some(_) => return Err(AppError::new("result_page", "结果分页没有推进")),
            None => break,
        }
    }
    if row_count == 0 {
        return Err(AppError::conflict("Redis key 在导出期间消失或内容为空"));
    }
    if csv.is_none() {
        out.write_all(b"]}")?;
    }
    p.keys += 1;
    Ok(())
}
fn validate_redis_browse_type(expected: &str, browsed: &Json) -> Result<()> {
    if browsed.get("type").and_then(Json::as_str) != Some(expected) {
        return Err(AppError::conflict(
            "Redis key 类型在导出期间发生变化或 key 已不存在",
        ));
    }
    Ok(())
}
fn redis_bytes(v: &Value) -> Result<String> {
    match v {
        Value::Bytes(s) => Ok(s.clone()),
        _ => Err(AppError::invalid("Redis 内容必须是二进制类型")),
    }
}
fn redis_csv_row(key: &str, ty: &str, row: &[Value]) -> Result<Vec<String>> {
    let mut r = vec![
        "1".into(),
        "item".into(),
        key.into(),
        ty.into(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    ];
    match (ty, row) {
        ("string", [v]) => r[9] = redis_bytes(v)?,
        ("hash", [f, v]) => {
            r[5] = redis_bytes(f)?;
            r[9] = redis_bytes(v)?;
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
    if session.profile().access_mode != crate::model::AccessMode::ReadWrite {
        return Err(AppError::new("permission_denied", "只读连接不能导入"));
    }
    let fmt = format(params)?;
    let source = path(params)?;
    let file = File::open(&source)?;
    let target: ResourceTarget = param(params, "target")?;
    match (session.profile().provider, fmt) {
        (ProviderKind::Mysql, "csv") => {
            import_mysql_csv(session, params, &target, sink, cancel, p, file).await?
        }
        (ProviderKind::Mysql, _) => {
            import_mysql_json(session, params, &target, sink, cancel, p, file).await?
        }
        (ProviderKind::Redis, "csv") => {
            import_redis_csv(session, &target, sink, cancel, p, file).await?
        }
        (ProviderKind::Redis, _) => {
            import_redis_json(session, &target, sink, cancel, p, file).await?
        }
    }
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
                if success != count {
                    return Err(
                        AppError::new("import_failed", "导入批次包含失败或未确认的记录")
                            .with_details(v),
                    );
                }
            } else if v.get("affected_rows").and_then(Json::as_u64) == Some(count as u64) {
                p.rows += count as u64;
            } else {
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
            }
            Err(e)
        }
    }
}
async fn import_mysql_csv(
    session: &mut ProviderSession,
    params: &Json,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    file: File,
) -> Result<()> {
    let mut reader = csv::Reader::from_reader(BufReader::new(file));
    let names: Vec<String> = reader
        .headers()
        .map_err(csv_error)?
        .iter()
        .map(str::to_owned)
        .collect();
    let projection = mapped_columns(
        &names,
        params,
        &schema(session, target, sink, cancel).await?,
    )?;
    let null = params
        .get("null_marker")
        .and_then(Json::as_str)
        .unwrap_or("\\N");
    let escaped = params
        .get("backslash_escape")
        .and_then(Json::as_bool)
        .unwrap_or(true);
    let binary: HashSet<&str> = params
        .get("binary_columns")
        .and_then(Json::as_array)
        .map(|a| a.iter().filter_map(Json::as_str).collect())
        .unwrap_or_default();
    let mut batch = Vec::with_capacity(PAGE);
    for record in reader.records() {
        ensure_not_cancelled(cancel)?;
        let record = record.map_err(csv_error)?;
        let row = projection.csv_row(&record,&names,null,escaped,&binary).map_err(|e| { p.failed += 1; e.with_details(json!({"record":p.rows + batch.len() as u64 + 1,"buffered_not_executed":batch.len()})) })?;
        batch.push(row);
        if batch.len() == PAGE {
            insert_batch(
                session,
                target,
                &projection.columns,
                &mut batch,
                sink,
                cancel,
                p,
            )
            .await?;
        }
    }
    insert_batch(
        session,
        target,
        &projection.columns,
        &mut batch,
        sink,
        cancel,
        p,
    )
    .await
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
    if binary || ty.contains("BINARY") || ty.contains("BLOB") || ty.contains("BIT") {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .decode(&s)
            .map_err(|e| AppError::invalid(format!("无效 Base64: {e}")))?;
        Ok(Value::Bytes(s))
    } else if ty.contains("INT") || ty.starts_with("YEAR") {
        if ty.contains("UNSIGNED") || ty.starts_with("YEAR") {
            u64::from_str(&s).map_err(|_| AppError::invalid("无效无符号整数"))?;
            Ok(Value::Unsigned(s))
        } else {
            i64::from_str(&s).map_err(|_| AppError::invalid("无效整数"))?;
            Ok(Value::Integer(s))
        }
    } else if ty.contains("DECIMAL") || ty.contains("NUMERIC") {
        bigdecimal::BigDecimal::from_str(&s).map_err(|_| AppError::invalid("无效精确小数"))?;
        Ok(Value::Decimal(s))
    } else if ty.contains("FLOAT") || ty.contains("DOUBLE") || ty.contains("REAL") {
        f64::from_str(&s).map_err(|_| AppError::invalid("无效浮点数"))?;
        Ok(Value::Float(s))
    } else if ty.contains("JSON") {
        Ok(Value::Json(serde_json::from_str(&s)?))
    } else if ty.contains("DATETIME") || ty.contains("TIMESTAMP") {
        Ok(Value::DateTime(s))
    } else if ty.starts_with("DATE") {
        Ok(Value::Date(s))
    } else if ty.starts_with("TIME") {
        Ok(Value::Time(s))
    } else {
        Ok(Value::Text(s))
    }
}

// 每次只解析一个 JSON 值，避免将完整行数组载入内存。
struct JsonReader {
    input: BufReader<File>,
    pending: Option<u8>,
}
impl JsonReader {
    fn new(file: File) -> Self {
        Self {
            input: BufReader::new(file),
            pending: None,
        }
    }
    fn byte(&mut self) -> Result<Option<u8>> {
        if let Some(c) = self.pending.take() {
            return Ok(Some(c));
        }
        let mut b = [0];
        Ok(if self.input.read(&mut b)? == 0 {
            None
        } else {
            Some(b[0])
        })
    }
    fn nonspace(&mut self) -> Result<Option<u8>> {
        loop {
            match self.byte()? {
                Some(b) if b.is_ascii_whitespace() => (),
                c => return Ok(c),
            }
        }
    }
    fn expect(&mut self, expected: u8) -> Result<()> {
        if self.nonspace()? != Some(expected) {
            return Err(AppError::invalid(format!(
                "JSON 格式错误，期望 {}",
                expected as char
            )));
        }
        Ok(())
    }
    fn value<T: serde::de::DeserializeOwned>(&mut self) -> Result<T> {
        let first = self
            .nonspace()?
            .ok_or_else(|| AppError::invalid("JSON 提前结束"))?;
        let mut data = vec![first];
        let mut in_string = first == b'"';
        let mut escape = false;
        let mut depth: i64 = if first == b'{' || first == b'[' { 1 } else { 0 };
        let scalar = depth == 0 && !in_string;
        if in_string || depth > 0 || scalar {
            loop {
                let b = match self.byte()? {
                    Some(b) => b,
                    None if scalar => break,
                    None => return Err(AppError::invalid("JSON 提前结束")),
                };
                if scalar && (b == b',' || b == b']' || b == b'}' || b.is_ascii_whitespace()) {
                    self.pending = Some(b);
                    break;
                }
                data.push(b);
                if in_string {
                    if escape {
                        escape = false;
                    } else if b == b'\\' {
                        escape = true;
                    } else if b == b'"' {
                        in_string = false;
                        if depth == 0 {
                            break;
                        }
                    }
                } else if b == b'"' {
                    in_string = true;
                } else if b == b'{' || b == b'[' {
                    depth += 1;
                } else if b == b'}' || b == b']' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
        }
        serde_json::from_slice(&data).map_err(Into::into)
    }
    fn array_next(&mut self, first: &mut bool) -> Result<bool> {
        let c = self
            .nonspace()?
            .ok_or_else(|| AppError::invalid("JSON 数组提前结束"))?;
        if c == b']' {
            return Ok(false);
        }
        if !*first {
            if c != b',' {
                return Err(AppError::invalid("JSON 数组缺少逗号"));
            }
        } else {
            self.pending = Some(c);
            *first = false;
        }
        Ok(true)
    }
    fn object_next(&mut self, first: &mut bool) -> Result<Option<String>> {
        let c = self
            .nonspace()?
            .ok_or_else(|| AppError::invalid("JSON 对象提前结束"))?;
        if c == b'}' {
            return Ok(None);
        }
        if *first {
            self.pending = Some(c);
            *first = false;
        } else if c != b',' {
            return Err(AppError::invalid("JSON 对象缺少逗号"));
        }
        let name = self.value::<String>()?;
        self.expect(b':')?;
        Ok(Some(name))
    }
    fn copy_value(&mut self, out: &mut impl Write, cancel: &CancellationToken) -> Result<()> {
        let first = self
            .nonspace()?
            .ok_or_else(|| AppError::invalid("JSON 提前结束"))?;
        out.write_all(&[first])?;
        let mut string = first == b'"';
        let mut escaped = false;
        let mut depth: i64 = if first == b'{' || first == b'[' { 1 } else { 0 };
        let scalar = depth == 0 && !string;
        let mut count = 0usize;
        loop {
            if count.is_multiple_of(65536) {
                ensure_not_cancelled(cancel)?;
            }
            count += 1;
            let b = match self.byte()? {
                Some(b) => b,
                None if scalar => break,
                None => return Err(AppError::invalid("JSON 提前结束")),
            };
            if scalar && (b == b',' || b == b']' || b == b'}' || b.is_ascii_whitespace()) {
                self.pending = Some(b);
                break;
            }
            out.write_all(&[b])?;
            if string {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    string = false;
                    if depth == 0 {
                        break;
                    }
                }
            } else if b == b'"' {
                string = true;
            } else if b == b'{' || b == b'[' {
                depth += 1;
            } else if b == b'}' || b == b']' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
        }
        Ok(())
    }
    fn end_document(&mut self) -> Result<()> {
        if self.nonspace()?.is_some() {
            return Err(AppError::invalid("JSON 文件有多余内容"));
        }
        Ok(())
    }
}
// 数据字段可能先于元数据出现：暂存其原始 JSON，验证外层后再流式读取。
struct JsonSpool {
    path: PathBuf,
}
impl JsonSpool {
    fn copy(reader: &mut JsonReader, root: &Path, cancel: &CancellationToken) -> Result<Self> {
        let path = temp_path(&root.join("transfer-json"), "temporary");
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        let spool = Self { path };
        let mut writer = BufWriter::new(file);
        reader.copy_value(&mut writer, cancel)?;
        writer.flush()?;
        Ok(spool)
    }
    fn reader(&self) -> Result<JsonReader> {
        Ok(JsonReader::new(File::open(&self.path)?))
    }
}
impl Drop for JsonSpool {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

struct JsonDocument {
    columns: Option<Vec<Column>>,
    data: JsonSpool,
}
fn json_document(
    file: File,
    root: &Path,
    array: &str,
    cancel: &CancellationToken,
) -> Result<JsonDocument> {
    let mut reader = JsonReader::new(file);
    reader.expect(b'{')?;
    let mut first = true;
    let mut format: Option<String> = None;
    let mut version: Option<u64> = None;
    let mut columns: Option<Vec<Column>> = None;
    let mut data: Option<JsonSpool> = None;
    while let Some(name) = reader.object_next(&mut first)? {
        ensure_not_cancelled(cancel)?;
        match name.as_str() {
            "format" if format.is_none() => format = Some(reader.value()?),
            "version" if version.is_none() => version = Some(reader.value()?),
            "columns" if array == "rows" && columns.is_none() => columns = Some(reader.value()?),
            key if key == array && data.is_none() => {
                data = Some(JsonSpool::copy(&mut reader, root, cancel)?)
            }
            "format" | "version" => return Err(AppError::invalid("JSON 字段重复")),
            "columns" if array == "rows" => return Err(AppError::invalid("JSON 列字段重复")),
            key if key == array => return Err(AppError::invalid("JSON 数据字段重复")),
            _ => reader.copy_value(&mut std::io::sink(), cancel)?,
        }
    }
    reader.end_document()?;
    if format.as_deref() != Some("datapier") || version != Some(1) {
        return Err(AppError::invalid("不是受支持的 Datapier 版本化文件"));
    }
    Ok(JsonDocument {
        columns,
        data: data.ok_or_else(|| AppError::invalid("JSON 缺少数据数组"))?,
    })
}
async fn import_mysql_json(
    session: &mut ProviderSession,
    params: &Json,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    file: File,
) -> Result<()> {
    let doc = json_document(file, &sink.store.root, "rows", cancel)?;
    let source = doc
        .columns
        .ok_or_else(|| AppError::invalid("JSON 缺少列定义"))?;
    let mut r = doc.data.reader()?;
    r.expect(b'[')?;
    let names: Vec<String> = source.iter().map(|c| c.name.clone()).collect();
    let projection = mapped_columns(
        &names,
        params,
        &schema(session, target, sink, cancel).await?,
    )?;
    let mut first = true;
    let mut batch = Vec::with_capacity(PAGE);
    while r.array_next(&mut first)? {
        ensure_not_cancelled(cancel)?;
        let row: Vec<Value> = r.value()?;
        batch.push(projection.project(row)?);
        if batch.len() == PAGE {
            insert_batch(
                session,
                target,
                &projection.columns,
                &mut batch,
                sink,
                cancel,
                p,
            )
            .await?;
        }
    }
    r.end_document()?;
    insert_batch(
        session,
        target,
        &projection.columns,
        &mut batch,
        sink,
        cancel,
        p,
    )
    .await
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
        batch: Vec::with_capacity(PAGE),
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
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<()> {
    // 写入临时 key 前校验与 provider 浏览相同的行布局和 List 索引。
    redis_csv_row(&key.key, &key.kind, &row)?;
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
    key.batch.push(row);
    if key.batch.len() == PAGE {
        append_key(session, key, sink, cancel).await?;
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
    session
        .call(
            "import.discard",
            json!({"token":key.token}),
            sink,
            &CancellationToken::new(),
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
    if &record[0] != "1" || &record[1] != "item" || record[2] != key.key || record[3] != key.kind {
        return Err(AppError::invalid("Redis CSV 元素不属于当前 key"));
    }
    let bytes = |n: usize| -> Result<Value> {
        valid_b64(&record[n])?;
        Ok(Value::Bytes(record[n].to_owned()))
    };
    match key.kind.as_str() {
        "string" => Ok(vec![bytes(9)?]),
        "hash" => Ok(vec![bytes(5)?, bytes(9)?]),
        "list" => {
            record[6]
                .parse::<u64>()
                .map_err(|_| AppError::invalid("无效 List 索引"))?;
            Ok(vec![Value::Unsigned(record[6].to_owned()), bytes(9)?])
        }
        "set" => Ok(vec![bytes(7)?]),
        "zset" => {
            record[8]
                .parse::<f64>()
                .map_err(|_| AppError::invalid("无效 ZSet 分数"))?;
            Ok(vec![bytes(7)?, Value::Float(record[8].to_owned())])
        }
        _ => Err(AppError::invalid("不支持的 Redis 类型")),
    }
}
async fn import_redis_csv(
    session: &mut ProviderSession,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    file: File,
) -> Result<()> {
    let mut reader = csv::Reader::from_reader(BufReader::new(file));
    if reader.headers().map_err(csv_error)?.iter().ne(REDIS_HEADER) {
        return Err(AppError::invalid("Redis CSV 表头不匹配"));
    }
    let mut active: Option<KeyImport> = None;
    let result: Result<()> = async {
        for record in reader.records() {
            ensure_not_cancelled(cancel)?;
            let record = record.map_err(csv_error)?;
            if record.len() != REDIS_HEADER.len() {
                return Err(AppError::invalid("Redis CSV 列数不匹配"));
            }
            match &record[1] {
                "key" => {
                    if &record[0] != "1" {
                        return Err(AppError::invalid("不支持的 Redis CSV 版本"));
                    }
                    if let Some(key) = active.as_mut() {
                        finish_key(session, key, sink, cancel, p).await?;
                    }
                    active = None;
                    let expiry = if &record[4] == "permanent" {
                        Json::Null
                    } else {
                        Json::String(record[4].to_owned())
                    };
                    active = Some(
                        begin_key(
                            session,
                            target,
                            RedisKeyHeader {
                                key: record[2].to_owned(),
                                kind: record[3].to_owned(),
                                expiry,
                            },
                            sink,
                            cancel,
                            p,
                        )
                        .await?,
                    );
                }
                "item" => {
                    let key = active
                        .as_mut()
                        .ok_or_else(|| AppError::invalid("Redis CSV 元素缺少 key 记录"))?;
                    let row = csv_item(&record, key)?;
                    push_key(session, key, row, sink, cancel).await?;
                }
                _ => return Err(AppError::invalid("未知 Redis CSV 记录类型")),
            }
        }
        if let Some(key) = active.as_mut() {
            finish_key(session, key, sink, cancel, p).await?;
        }
        active = None;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            p.failed += 1;
            Err(cleanup_key(session, &active, sink, e).await)
        }
    }
}
async fn import_redis_json(
    session: &mut ProviderSession,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    file: File,
) -> Result<()> {
    let doc = json_document(file, &sink.store.root, "records", cancel)?;
    let mut r = doc.data.reader()?;
    r.expect(b'[')?;
    let mut active: Option<KeyImport> = None;
    let result: Result<()> = async {
        let mut first = true;
        while r.array_next(&mut first)? {
            ensure_not_cancelled(cancel)?;
            r.expect(b'{')?;
            let mut fields_first = true;
            let mut key: Option<String> = None;
            let mut kind: Option<String> = None;
            let mut expiry: Option<Json> = None;
            let mut rows: Option<JsonSpool> = None;
            while let Some(field) = r.object_next(&mut fields_first)? {
                ensure_not_cancelled(cancel)?;
                match field.as_str() {
                    "key_b64" if key.is_none() => key = Some(r.value()?),
                    "type" if kind.is_none() => kind = Some(r.value()?),
                    "expires_at_ms" if expiry.is_none() => expiry = Some(r.value()?),
                    "rows" if rows.is_none() => {
                        rows = Some(JsonSpool::copy(&mut r, &sink.store.root, cancel)?)
                    }
                    "key_b64" | "type" | "expires_at_ms" | "rows" => {
                        return Err(AppError::invalid("Redis JSON 字段重复"));
                    }
                    _ => r.copy_value(&mut std::io::sink(), cancel)?,
                }
            }
            let key = key.ok_or_else(|| AppError::invalid("Redis JSON 缺少 key_b64"))?;
            let kind = kind.ok_or_else(|| AppError::invalid("Redis JSON 缺少 type"))?;
            let expiry =
                expiry.ok_or_else(|| AppError::invalid("Redis JSON 缺少 expires_at_ms"))?;
            let rows = rows.ok_or_else(|| AppError::invalid("Redis JSON 缺少 rows"))?;
            let mut row_reader = rows.reader()?;
            row_reader.expect(b'[')?;
            active = Some(
                begin_key(
                    session,
                    target,
                    RedisKeyHeader { key, kind, expiry },
                    sink,
                    cancel,
                    p,
                )
                .await?,
            );
            let mut row_first = true;
            while row_reader.array_next(&mut row_first)? {
                ensure_not_cancelled(cancel)?;
                let row: Vec<Value> = row_reader.value()?;
                push_key(session, active.as_mut().unwrap(), row, sink, cancel).await?;
            }
            row_reader.end_document()?;
            finish_key(session, active.as_mut().unwrap(), sink, cancel, p).await?;
            active = None;
        }
        r.end_document()
    }
    .await;
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            p.failed += 1;
            Err(cleanup_key(session, &active, sink, e).await)
        }
    }
}

const PREVIEW_ROWS: usize = 50;
const PREVIEW_BYTES: usize = 256 * 1024;
struct Preview {
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
    errors: Vec<Json>,
    bytes: usize,
    complete: bool,
}
impl Preview {
    fn new(columns: Vec<Column>) -> Self {
        Self {
            columns,
            rows: Vec::new(),
            errors: Vec::new(),
            bytes: 0,
            complete: true,
        }
    }
    fn push(&mut self, row: Vec<Value>) -> Result<bool> {
        let bytes = serde_json::to_vec(&row)?.len();
        if self.rows.len() == PREVIEW_ROWS || self.bytes.saturating_add(bytes) > PREVIEW_BYTES {
            self.complete = false;
            return Ok(false);
        }
        self.bytes += bytes;
        self.rows.push(row);
        Ok(true)
    }
    fn failure(&mut self, record: u64, error: AppError) {
        self.errors.push(json!({"record":record,"error":error}));
        self.complete = false;
    }
}
struct LimitedJson {
    bytes: Vec<u8>,
    overflow: bool,
}
impl Write for LimitedJson {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let room = PREVIEW_BYTES.saturating_sub(self.bytes.len());
        let n = room.min(data.len());
        self.bytes.extend_from_slice(&data[..n]);
        if n < data.len() {
            self.overflow = true;
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn preview_json_value<T: serde::de::DeserializeOwned>(
    reader: &mut JsonReader,
    cancel: &CancellationToken,
) -> Result<T> {
    let mut buffer = LimitedJson {
        bytes: Vec::new(),
        overflow: false,
    };
    reader.copy_value(&mut buffer, cancel)?;
    if buffer.overflow {
        return Err(AppError::new(
            "preview_too_large",
            "单条预览记录超过 256 KiB",
        ));
    }
    Ok(serde_json::from_slice(&buffer.bytes)?)
}

/// 读取有限样本并校验文件和目标列映射，不提交导入数据。
/// 返回样本是否完整及逐记录错误；MySQL 预览会查询目标表结构。
pub async fn preview(
    session: &mut ProviderSession,
    params: Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    let fmt = format(&params)?;
    let target: ResourceTarget = param(&params, "target")?;
    let file = File::open(path(&params)?)?;
    let provider = session.profile().provider;
    if provider == ProviderKind::Redis
        && let Some(db) = target.database.as_deref()
        && db.parse::<u32>().ok() != Some(session.profile().redis_db)
    {
        return Err(AppError::invalid("目标数据库与连接不一致"));
    }
    let result = match (provider, fmt) {
        (ProviderKind::Mysql, "csv") => {
            preview_mysql_csv(session, &params, &target, sink, cancel, file).await?
        }
        (ProviderKind::Mysql, _) => {
            preview_mysql_json(session, &params, &target, sink, cancel, file).await?
        }
        (ProviderKind::Redis, "csv") => preview_redis_csv(&target, cancel, file)?,
        (ProviderKind::Redis, _) => preview_redis_json(&target, sink, cancel, file)?,
    };
    Ok(
        json!({"columns":result.columns,"rows":result.rows,"target":target,"format":fmt,"provider":provider,"sample_complete":result.complete,"errors":result.errors}),
    )
}

async fn preview_mysql_csv(
    session: &mut ProviderSession,
    params: &Json,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    file: File,
) -> Result<Preview> {
    let length = file.metadata()?.len();
    let limit = (PREVIEW_BYTES + 1) as u64;
    let mut reader = csv::Reader::from_reader(BufReader::new(file.take(limit)));
    let names: Vec<String> = reader
        .headers()
        .map_err(csv_error)?
        .iter()
        .map(str::to_owned)
        .collect();
    let projection = mapped_columns(
        &names,
        params,
        &schema(session, target, sink, cancel).await?,
    )?;
    let null = params
        .get("null_marker")
        .and_then(Json::as_str)
        .unwrap_or("\\N");
    let escaped = params
        .get("backslash_escape")
        .and_then(Json::as_bool)
        .unwrap_or(true);
    let binary: HashSet<&str> = params
        .get("binary_columns")
        .and_then(Json::as_array)
        .map(|a| a.iter().filter_map(Json::as_str).collect())
        .unwrap_or_default();
    let mut preview = Preview::new(projection.columns.clone());
    let mut record_no = 0u64;
    loop {
        ensure_not_cancelled(cancel)?;
        let mut record = csv::StringRecord::new();
        let read = match reader.read_record(&mut record) {
            Ok(v) => v,
            Err(e) => {
                preview.failure(record_no + 1, csv_error(e));
                break;
            }
        };
        if !read {
            if length > limit {
                preview.complete = false;
            }
            break;
        }
        record_no += 1;
        if length > limit && reader.position().byte() >= limit {
            preview.complete = false;
            break;
        }
        if preview.rows.len() == PREVIEW_ROWS {
            preview.complete = false;
            break;
        }
        let converted = projection.csv_row(&record, &names, null, escaped, &binary);
        match converted.and_then(|row| {
            validate_preview_mysql(&row, &preview.columns)?;
            Ok(row)
        }) {
            Ok(row) => {
                if !preview.push(row)? {
                    break;
                }
            }
            Err(e) => {
                preview.failure(record_no, e);
                break;
            }
        }
    }
    Ok(preview)
}
async fn preview_mysql_json(
    session: &mut ProviderSession,
    params: &Json,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    file: File,
) -> Result<Preview> {
    let doc = json_document(file, &sink.store.root, "rows", cancel)?;
    let source = doc
        .columns
        .ok_or_else(|| AppError::invalid("JSON 缺少列定义"))?;
    let names: Vec<String> = source.iter().map(|c| c.name.clone()).collect();
    let projection = mapped_columns(
        &names,
        params,
        &schema(session, target, sink, cancel).await?,
    )?;
    let mut preview = Preview::new(projection.columns.clone());
    let mut r = doc.data.reader()?;
    r.expect(b'[')?;
    let mut first = true;
    let mut record_no = 0;
    while r.array_next(&mut first)? {
        ensure_not_cancelled(cancel)?;
        record_no += 1;
        if preview.rows.len() == PREVIEW_ROWS {
            preview.complete = false;
            break;
        }
        match preview_json_value::<Vec<Value>>(&mut r, cancel)
            .and_then(|row| projection.project(row))
        {
            Ok(row) => {
                if let Err(e) = validate_preview_mysql(&row, &preview.columns) {
                    preview.failure(record_no, e);
                    break;
                }
                if !preview.push(row)? {
                    break;
                }
            }
            Err(e) if e.code == "cancelled" => return Err(e),
            Err(e) => {
                preview.failure(record_no, e);
                break;
            }
        }
    }
    Ok(preview)
}
fn validate_preview_mysql(row: &[Value], columns: &[Column]) -> Result<()> {
    for (value, column) in row.iter().zip(columns) {
        crate::providers::mysql::validate_import_value(value, column)?;
    }
    Ok(())
}
fn redis_preview_columns() -> Vec<Column> {
    ["key_b64", "type", "expires_at_ms", "record", "values"]
        .into_iter()
        .map(|name| Column::new(name, "redis"))
        .collect()
}
fn redis_preview_row(
    key: &str,
    kind: &str,
    expiry: &Json,
    record: &str,
    values: Vec<Value>,
) -> Vec<Value> {
    vec![
        Value::Text(key.to_owned()),
        Value::Text(kind.to_owned()),
        if expiry.is_null() {
            Value::Null
        } else {
            Value::Text(expiry.as_str().unwrap_or_default().to_owned())
        },
        Value::Text(record.to_owned()),
        Value::Array(values),
    ]
}
fn preview_redis_csv(
    _target: &ResourceTarget,
    cancel: &CancellationToken,
    file: File,
) -> Result<Preview> {
    let length = file.metadata()?.len();
    let limit = (PREVIEW_BYTES + 1) as u64;
    let mut reader = csv::Reader::from_reader(BufReader::new(file.take(limit)));
    if reader.headers().map_err(csv_error)?.iter().ne(REDIS_HEADER) {
        return Err(AppError::invalid("Redis CSV 表头不匹配"));
    }
    let mut preview = Preview::new(redis_preview_columns());
    let mut active: Option<KeyImport> = None;
    let mut expiry = Json::Null;
    let mut record_no = 0;
    loop {
        ensure_not_cancelled(cancel)?;
        let mut record = csv::StringRecord::new();
        let read = match reader.read_record(&mut record) {
            Ok(v) => v,
            Err(e) => {
                preview.failure(record_no + 1, csv_error(e));
                break;
            }
        };
        if !read {
            if length > limit {
                preview.complete = false;
            }
            break;
        }
        record_no += 1;
        if length > limit && reader.position().byte() >= limit {
            preview.complete = false;
            break;
        }
        if preview.rows.len() == PREVIEW_ROWS {
            preview.complete = false;
            break;
        }
        let converted: Result<Vec<Value>> = (|| {
            if record.len() != REDIS_HEADER.len() || &record[0] != "1" {
                return Err(AppError::invalid("Redis CSV 记录格式错误"));
            }
            if &record[1] == "key" {
                valid_b64(&record[2])?;
                if !matches!(&record[3], "string" | "hash" | "list" | "set" | "zset") {
                    return Err(AppError::invalid("不支持的 Redis 类型"));
                }
                expiry = if &record[4] == "permanent" {
                    Json::Null
                } else {
                    record[4]
                        .parse::<i64>()
                        .map_err(|_| AppError::invalid("无效绝对过期时间"))?;
                    Json::String(record[4].to_owned())
                };
                active = Some(KeyImport {
                    token: Json::Null,
                    key: record[2].to_owned(),
                    kind: record[3].to_owned(),
                    batch: Vec::new(),
                    rows: 0,
                    next_index: 0,
                });
                Ok(redis_preview_row(
                    &record[2],
                    &record[3],
                    &expiry,
                    "key",
                    Vec::new(),
                ))
            } else {
                let key = active
                    .as_ref()
                    .ok_or_else(|| AppError::invalid("Redis CSV 元素缺少 key 记录"))?;
                let values = csv_item(&record, key)?;
                Ok(redis_preview_row(
                    &key.key, &key.kind, &expiry, "item", values,
                ))
            }
        })();
        match converted {
            Ok(row) => {
                if !preview.push(row)? {
                    break;
                }
            }
            Err(e) => {
                preview.failure(record_no, e);
                break;
            }
        }
    }
    Ok(preview)
}
fn preview_redis_json(
    _target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    file: File,
) -> Result<Preview> {
    let doc = json_document(file, &sink.store.root, "records", cancel)?;
    let mut r = doc.data.reader()?;
    r.expect(b'[')?;
    let mut preview = Preview::new(redis_preview_columns());
    let mut first = true;
    let mut record_no = 0u64;
    while r.array_next(&mut first)? {
        ensure_not_cancelled(cancel)?;
        record_no += 1;
        if preview.rows.len() == PREVIEW_ROWS {
            preview.complete = false;
            break;
        }
        let parsed: Result<(String, String, Json, JsonSpool)> = (|| {
            r.expect(b'{')?;
            let mut fields_first = true;
            let mut key: Option<String> = None;
            let mut kind: Option<String> = None;
            let mut expiry: Option<Json> = None;
            let mut rows: Option<JsonSpool> = None;
            while let Some(field) = r.object_next(&mut fields_first)? {
                ensure_not_cancelled(cancel)?;
                match field.as_str() {
                    "key_b64" if key.is_none() => key = Some(preview_json_value(&mut r, cancel)?),
                    "type" if kind.is_none() => kind = Some(preview_json_value(&mut r, cancel)?),
                    "expires_at_ms" if expiry.is_none() => {
                        expiry = Some(preview_json_value(&mut r, cancel)?)
                    }
                    "rows" if rows.is_none() => {
                        rows = Some(JsonSpool::copy(&mut r, &sink.store.root, cancel)?)
                    }
                    "key_b64" | "type" | "expires_at_ms" | "rows" => {
                        return Err(AppError::invalid("Redis JSON 字段重复"));
                    }
                    _ => r.copy_value(&mut std::io::sink(), cancel)?,
                }
            }
            let key = key.ok_or_else(|| AppError::invalid("Redis JSON 缺少 key_b64"))?;
            valid_b64(&key)?;
            let kind = kind.ok_or_else(|| AppError::invalid("Redis JSON 缺少 type"))?;
            if !matches!(kind.as_str(), "string" | "hash" | "list" | "set" | "zset") {
                return Err(AppError::invalid("不支持的 Redis 类型"));
            }
            let expiry =
                expiry.ok_or_else(|| AppError::invalid("Redis JSON 缺少 expires_at_ms"))?;
            if !expiry.is_null()
                && expiry
                    .as_str()
                    .and_then(|s| s.parse::<i64>().ok())
                    .is_none()
            {
                return Err(AppError::invalid("无效绝对过期时间"));
            }
            Ok((
                key,
                kind,
                expiry,
                rows.ok_or_else(|| AppError::invalid("Redis JSON 缺少 rows"))?,
            ))
        })();
        let (key, kind, expiry, rows) = match parsed {
            Ok(v) => v,
            Err(e) if e.code == "cancelled" => return Err(e),
            Err(e) => {
                preview.failure(record_no, e);
                break;
            }
        };
        if !preview.push(redis_preview_row(&key, &kind, &expiry, "key", Vec::new()))? {
            break;
        }
        let mut row_reader = rows.reader()?;
        if let Err(e) = row_reader.expect(b'[') {
            preview.failure(record_no, e);
            break;
        }
        let mut row_first = true;
        let mut next_index = 0u64;
        let mut values_count = 0u64;
        loop {
            let has_row = match row_reader.array_next(&mut row_first) {
                Ok(v) => v,
                Err(e) => {
                    preview.failure(record_no, e);
                    break;
                }
            };
            if !has_row {
                if values_count == 0 {
                    preview.failure(record_no, AppError::invalid("Redis key 内容为空"));
                }
                break;
            }
            if preview.rows.len() == PREVIEW_ROWS {
                preview.complete = false;
                break;
            }
            let values: Vec<Value> = match preview_json_value(&mut row_reader, cancel) {
                Ok(v) => v,
                Err(e) if e.code == "cancelled" => return Err(e),
                Err(e) => {
                    preview.failure(record_no, e);
                    break;
                }
            };
            let validated = redis_csv_row(&key, &kind, &values).and_then(|_| {
                if kind == "list" {
                    let index = match &values[0] {
                        Value::Integer(s) | Value::Unsigned(s) => s.parse::<u64>().ok(),
                        _ => None,
                    };
                    if index != Some(next_index) {
                        return Err(AppError::invalid("List 索引不连续"));
                    }
                    next_index += 1;
                }
                if kind == "string" && values_count != 0 {
                    return Err(AppError::invalid("String 只能包含一行"));
                }
                for v in &values {
                    if let Value::Bytes(s) = v {
                        valid_b64(s)?;
                    }
                }
                Ok(())
            });
            if let Err(e) = validated {
                preview.failure(record_no, e);
                break;
            }
            values_count += 1;
            if !preview.push(redis_preview_row(&key, &kind, &expiry, "item", values))? {
                break;
            }
        }
        if !preview.complete {
            break;
        }
        if let Err(e) = row_reader.end_document() {
            preview.failure(record_no, e);
            break;
        }
    }
    Ok(preview)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> TableSchema {
        let mut generated = Column::new("doubled", "int");
        generated.generated = true;
        TableSchema {
            target: ResourceTarget {
                database: None,
                name: "t".into(),
                key_b64: None,
                kind: "table".into(),
            },
            columns: vec![
                Column::new("id", "int"),
                generated,
                Column::new("year", "year"),
                Column::new("text", "varchar"),
            ],
            keys: Vec::new(),
            ddl: String::new(),
            engine: "InnoDB".into(),
            details: Json::Null,
        }
    }
    fn names() -> Vec<String> {
        ["id", "doubled", "year", "text"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn generated_and_explicitly_excluded_columns_are_projected_consistently() {
        let p =
            mapped_columns(&names(), &json!({"column_mapping":{"text":null}}), &table()).unwrap();
        assert_eq!(p.indices, vec![0, 2]);
        assert_eq!(
            p.columns
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            vec!["id", "year"]
        );
        let csv = csv::StringRecord::from(vec!["7", "14", "2026", "ignored"]);
        let expected = vec![Value::Integer("7".into()), Value::Unsigned("2026".into())];
        assert_eq!(
            p.csv_row(&csv, &names(), "\\N", true, &HashSet::new())
                .unwrap(),
            expected
        );
        assert_eq!(
            p.project(vec![
                Value::Integer("7".into()),
                Value::Integer("14".into()),
                Value::Unsigned("2026".into()),
                Value::Text("ignored".into())
            ])
            .unwrap(),
            expected
        );
        assert!(
            mapped_columns(
                &names(),
                &json!({"column_mapping":{"id":"doubled"}}),
                &table()
            )
            .is_err()
        );
        crate::providers::mysql::validate_import_value(&expected[1], &p.columns[1]).unwrap();
    }

    #[test]
    fn csv_distinguishes_null_and_escaped_literal() {
        let p = mapped_columns(&names(), &json!({}), &table()).unwrap();
        let row = csv::StringRecord::from(vec!["1", "2", "2026", "\\N"]);
        assert_eq!(
            p.csv_row(&row, &names(), "\\N", true, &HashSet::new())
                .unwrap()[2],
            Value::Null
        );
        let row = csv::StringRecord::from(vec!["1", "2", "2026", "\\\\N"]);
        assert_eq!(
            p.csv_row(&row, &names(), "\\N", true, &HashSet::new())
                .unwrap()[2],
            Value::Text("\\N".into())
        );
    }

    #[test]
    fn unordered_json_fields_preserve_typed_rows() {
        let root = std::env::temp_dir().join(format!("datapier-transfer-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let input = root.join("input.json");
        fs::write(&input, r#"{"rows":[[{"type":"unsigned","value":"2026"}]],"ignored":{"nested":[1,2]},"columns":[{"name":"year","type_name":"year"}],"version":1,"format":"datapier"}"#).unwrap();
        let doc = json_document(
            File::open(input).unwrap(),
            &root,
            "rows",
            &CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(doc.columns.as_ref().unwrap()[0].name, "year");
        let mut reader = doc.data.reader().unwrap();
        reader.expect(b'[').unwrap();
        let mut first = true;
        assert!(reader.array_next(&mut first).unwrap());
        assert_eq!(
            reader.value::<Vec<Value>>().unwrap(),
            vec![Value::Unsigned("2026".into())]
        );
        assert!(!reader.array_next(&mut first).unwrap());
        reader.end_document().unwrap();
        drop(reader);
        drop(doc);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn export_never_replaces_an_existing_file() {
        let root = std::env::temp_dir().join(format!("datapier-export-test-{}", Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        let tmp = root.join("partial");
        let dst = root.join("output");
        fs::write(&tmp, b"new").unwrap();
        fs::write(&dst, b"existing").unwrap();
        assert_eq!(publish_export(&tmp, &dst).unwrap_err().code, "conflict");
        assert_eq!(fs::read(&dst).unwrap(), b"existing");
        assert_eq!(fs::read(&tmp).unwrap(), b"new");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn redis_export_rejects_changed_type_and_incompatible_rows() {
        assert!(validate_redis_browse_type("string", &json!({"type":"hash"})).is_err());
        assert!(validate_redis_browse_type("string", &json!({})).is_err());
        assert!(
            redis_csv_row(
                "aw==",
                "string",
                &[Value::Bytes("YQ==".into()), Value::Bytes("Yg==".into())]
            )
            .is_err()
        );
        assert!(redis_csv_row("aw==", "string", &[Value::Bytes("YQ==".into())]).is_ok());
    }
}
