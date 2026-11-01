use super::*;

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
    validate_options(&params)?;
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
    target: &ResourceTarget,
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
    let mut key_count = 0u64;
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
            } else if active.as_ref().is_some_and(|key| key.rows == 0) {
                preview.failure(record_no, AppError::invalid("Redis key 内容为空"));
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
                if active.as_ref().is_some_and(|key| key.rows == 0) {
                    return Err(AppError::invalid("Redis key 内容为空"));
                }
                if target.key_b64.is_some() && key_count > 0 {
                    return Err(AppError::invalid("指定目标 key 时文件只能包含一个 key"));
                }
                key_count += 1;
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
                    .as_mut()
                    .ok_or_else(|| AppError::invalid("Redis CSV 元素缺少 key 记录"))?;
                let values = csv_item(&record, key)?;
                validate_key_row(key, &values)?;
                key.rows += 1;
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
    target: &ResourceTarget,
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
        if target.key_b64.is_some() && record_no > 1 {
            preview.failure(
                record_no,
                AppError::invalid("指定目标 key 时文件只能包含一个 key"),
            );
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
#[path = "../../../tests/unit/transfer_preview.rs"]
mod tests;
