//! 有界数据事件；JSON 外层与 Redis key 的字段顺序均不影响读取。
use super::{
    VerifiedReader,
    csv::{CsvReader, CsvRecord},
    json::JsonScan,
};
use crate::model::{
    AppError, Column, ProviderKind, Result, Value,
    transfer::{InputFingerprint, TransferFormat, TransferInput},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::collections::HashSet;

/// 记录定位使用原始文件字节偏移，Redis 集合逐行交付。
#[derive(Debug)]
pub enum RowEvent {
    MysqlRow {
        record: u64,
        byte_offset: u64,
        values: Vec<Value>,
    },
    RedisKeyHeader {
        record: u64,
        byte_offset: u64,
        key_b64: String,
        kind: String,
        expires_at_ms: serde_json::Value,
    },
    RedisChunk {
        record: u64,
        byte_offset: u64,
        values: Vec<Value>,
    },
    RedisKeyEnd {
        record: u64,
    },
}
struct RedisActive {
    key: String,
    kind: String,
    record: u64,
}
enum Source {
    Csv {
        reader: Box<CsvReader>,
        pending: Option<CsvRecord>,
        active: Option<RedisActive>,
        width: usize,
    },
    Json {
        scan: Box<JsonScan>,
        first: bool,
        active: Option<JsonKey>,
    },
}
struct JsonKey {
    record: u64,
    kind: String,
    end: u64,
    first: bool,
}
/// 同步读取器，调用方应在阻塞线程中运行并使用有界通道传递事件。
pub struct RowReader {
    source: Source,
    input: TransferInput,
    canonical_path: std::path::PathBuf,
    provider: ProviderKind,
    columns: Vec<Column>,
    needs_confirmation: bool,
    record: u64,
    stopped: bool,
}
impl RowReader {
    /// 返回当前事件已经消费的原始文件偏移；不计 JSON 元数据预扫描。
    pub fn bytes_consumed(&self) -> u64 {
        match &self.source {
            Source::Csv {
                reader, pending, ..
            } => pending
                .as_ref()
                .map_or_else(|| reader.position(), |record| record.byte_offset),
            Source::Json { scan, .. } => scan.position(),
        }
    }
    pub fn open(
        input: &TransferInput,
        fingerprint: &InputFingerprint,
        provider: ProviderKind,
    ) -> Result<Self> {
        Self::from_verified(input, VerifiedReader::open(fingerprint)?, provider)
    }
    /// 打开和 JSON 元数据全扫描都受同一取消信号约束。
    pub fn open_with_cancel(
        input: &TransferInput,
        fingerprint: &InputFingerprint,
        provider: ProviderKind,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Self> {
        let mut file = VerifiedReader::open(fingerprint)?;
        file.set_cancel(cancel.clone());
        Self::from_verified(input, file, provider).map_err(|error| {
            if cancel.is_cancelled() {
                AppError::cancelled()
            } else {
                error
            }
        })
    }
    pub fn from_verified_with_cancel(
        input: &TransferInput,
        mut file: VerifiedReader,
        provider: ProviderKind,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<Self> {
        file.set_cancel(cancel.clone());
        Self::from_verified(input, file, provider).map_err(|error| {
            if cancel.is_cancelled() {
                AppError::cancelled()
            } else {
                error
            }
        })
    }
    /// 将准备阶段保留的句柄交给解析器，避免重新按路径打开输入。
    pub fn from_verified(
        input: &TransferInput,
        file: VerifiedReader,
        provider: ProviderKind,
    ) -> Result<Self> {
        Self::from_verified_inner(input, file, provider).map_err(|mut error| {
            if !error.details.is_object() {
                *error.details = serde_json::json!({});
            }
            if let Some(details) = error.details.as_object_mut() {
                details
                    .entry("path")
                    .or_insert_with(|| serde_json::json!(input.path));
            }
            error
        })
    }
    fn from_verified_inner(
        input: &TransferInput,
        file: VerifiedReader,
        provider: ProviderKind,
    ) -> Result<Self> {
        if std::fs::canonicalize(&input.path)? != file.fingerprint.path {
            return Err(AppError::conflict("输入路径与准备时不同"));
        }
        file.verify_path()?;
        let canonical_path = file.fingerprint.path.clone();
        let mut columns = Vec::new();
        let mut needs_confirmation = false;
        let source = match input.format {
            TransferFormat::Csv => {
                let mut reader = CsvReader::open(file, &input.csv)?;
                let names = if input.csv.header {
                    reader
                        .next()?
                        .ok_or_else(|| AppError::invalid("CSV 缺少表头"))?
                        .fields
                } else {
                    input.csv.columns.clone()
                };
                let width = names.len();
                let mut unique = HashSet::new();
                if names
                    .iter()
                    .any(|name| name.is_empty() || !unique.insert(name.clone()))
                {
                    return Err(AppError::invalid("CSV 表头为空或重复"));
                }
                if provider == ProviderKind::Redis {
                    let valid_width = width == 11 && names[10] == "field_expires_at_ms";
                    if !valid_width
                        || names.iter().take(10).map(String::as_str).ne([
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
                        ])
                    {
                        return Err(AppError::invalid("Redis CSV 表头不匹配"));
                    }
                } else {
                    if input
                        .column_mapping
                        .keys()
                        .any(|name| !unique.contains(name))
                    {
                        return Err(AppError::invalid("列映射包含不存在的源列"));
                    }
                    let mut targets = HashSet::new();
                    for name in names {
                        let target = input.column_mapping.get(&name).unwrap_or(&name);
                        if !targets.insert(target.as_str().to_owned()) {
                            return Err(AppError::invalid("多个源列映射到同一目标列"));
                        }
                        let found = input.columns.iter().find(|column| &column.name == target);
                        if found.is_some_and(|column| column.generated)
                            && input.column_mapping.contains_key(&name)
                        {
                            return Err(AppError::invalid(format!("不能映射到生成列: {target}")));
                        }
                        let mut col = match found {
                            Some(c) => c.clone(),
                            None if input.columns.is_empty() => {
                                needs_confirmation = true;
                                Column::new(&name, "TEXT")
                            }
                            None => {
                                return Err(AppError::invalid(format!("目标列不存在: {target}")));
                            }
                        };
                        col.name = name;
                        columns.push(col);
                    }
                }
                Source::Csv {
                    reader: Box::new(reader),
                    pending: None,
                    active: None,
                    width,
                }
            }
            TransferFormat::Json => {
                let mut scan = JsonScan::new(file);
                scan.expect(b'{')?;
                let mut first = true;
                let mut format: Option<String> = None;
                let mut version: Option<u64> = None;
                let mut data = None;
                let mut source_columns: Option<Vec<Column>> = None;
                let data_name = if provider == ProviderKind::Mysql {
                    "rows"
                } else {
                    "records"
                };
                while let Some(name) = scan.object_next(&mut first)? {
                    match name.as_str() {
                        "format" if format.is_none() => format = Some(scan.value()?),
                        "version" if version.is_none() => version = Some(scan.value()?),
                        "columns" if source_columns.is_none() => {
                            source_columns = Some(scan.value()?)
                        }
                        s if s == data_name && data.is_none() => {
                            data = Some(scan.value_start()?);
                            scan.skip()?;
                        }
                        "format" | "version" | "columns" => {
                            return Err(AppError::invalid("JSON 字段重复"));
                        }
                        s if s == data_name => return Err(AppError::invalid("JSON 数据字段重复")),
                        _ => scan.skip()?,
                    }
                }
                scan.finish()?;
                if format.as_deref() != Some("datapier")
                    || version
                        != Some(if provider == ProviderKind::Redis {
                            2
                        } else {
                            1
                        })
                {
                    return Err(AppError::invalid("不是受支持的 Datapier 版本化文件"));
                }
                if provider == ProviderKind::Mysql {
                    columns = source_columns.ok_or_else(|| AppError::invalid("JSON 缺少列定义"))?;
                    if columns.len() > crate::model::transfer::MAX_PLAN_ITEMS {
                        return Err(AppError::new("resource_limit", "JSON 列定义超出限制"));
                    }
                    let mut names = HashSet::new();
                    if columns
                        .iter()
                        .any(|c| c.name.is_empty() || !names.insert(c.name.clone()))
                    {
                        return Err(AppError::invalid("JSON 列名为空或重复"));
                    }
                }
                scan.seek(data.ok_or_else(|| AppError::invalid("JSON 缺少数据数组"))?)?;
                scan.expect(b'[')?;
                Source::Json {
                    scan: Box::new(scan),
                    first: true,
                    active: None,
                }
            }
            TransferFormat::Sql => return Err(AppError::invalid("SQL 不属于数据文件格式")),
        };
        Ok(Self {
            source,
            input: input.clone(),
            canonical_path,
            provider,
            columns,
            needs_confirmation,
            record: 0,
            stopped: false,
        })
    }
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }
    pub fn needs_confirmation(&self) -> bool {
        self.needs_confirmation
    }
    pub fn next_event(&mut self) -> Result<Option<RowEvent>> {
        if self.stopped {
            return Err(AppError::invalid("输入已因不可恢复错误停止"));
        }
        let verified = match std::fs::canonicalize(&self.input.path) {
            Ok(path) if path == self.canonical_path => match &self.source {
                Source::Csv { reader, .. } => reader.verify(),
                Source::Json { scan, .. } => scan.verify(),
            },
            Ok(_) => Err(AppError::conflict("输入原路径已被替换")),
            Err(error) => Err(error.into()),
        };
        let result = verified
            .and_then(|()| self.next_event_inner())
            .map_err(|mut error| {
                let cancelled = match &self.source {
                    Source::Csv { reader, .. } => reader.is_cancelled(),
                    Source::Json { scan, .. } => scan.is_cancelled(),
                };
                if cancelled {
                    return AppError::cancelled();
                }
                if !error.details.is_object() {
                    *error.details = serde_json::json!({});
                }
                if let Some(details) = error.details.as_object_mut() {
                    if self.provider == ProviderKind::Mysql
                        && self.input.format == TransferFormat::Csv
                        && self.input.csv.header
                        && error.code != "conversion"
                        && let Some(record) = details.get_mut("record")
                        && let Some(number) = record.as_u64()
                    {
                        *record = serde_json::json!(number.saturating_sub(1));
                    }
                    details
                        .entry("path")
                        .or_insert_with(|| serde_json::json!(self.input.path));
                }
                error
            });
        if let Err(error) = &result
            && error
                .details
                .get("recoverable")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
        {
            self.stopped = true;
        }
        result
    }
    fn next_event_inner(&mut self) -> Result<Option<RowEvent>> {
        loop {
            let event = match &mut self.source {
                Source::Csv {
                    reader,
                    pending,
                    active,
                    width,
                } => {
                    let record = match pending.take() {
                        Some(r) => Some(r),
                        None => reader.next()?,
                    };
                    match record {
                        None => active
                            .take()
                            .map(|key| RowEvent::RedisKeyEnd { record: key.record }),
                        Some(record) if self.provider == ProviderKind::Mysql => {
                            self.record += 1;
                            if !self.input.record_ranges.is_empty()
                                && !self.input.record_ranges.iter().any(|range| {
                                    range.first <= self.record && self.record <= range.last
                                })
                            {
                                continue;
                            }
                            if record.fields.len() != self.columns.len() {
                                return Err(conversion_error(
                                    AppError::invalid("CSV 记录列数不匹配"),
                                    &record,
                                    self.record,
                                    None,
                                ));
                            }
                            let mut values = Vec::with_capacity(record.fields.len());
                            for (field, column) in record.fields.iter().zip(&self.columns) {
                                let value = if column.generated {
                                    Value::Text(field.clone())
                                } else if field == &self.input.csv.null_marker {
                                    Value::Null
                                } else {
                                    let text = if self.input.csv.backslash_escape {
                                        crate::execution::transfer::unescape(field).map_err(
                                            |e| {
                                                conversion_error(
                                                    e,
                                                    &record,
                                                    self.record,
                                                    Some(&column.name),
                                                )
                                            },
                                        )?
                                    } else {
                                        field.clone()
                                    };
                                    crate::execution::transfer::csv_typed(
                                        text,
                                        column,
                                        self.input.csv.binary_columns.contains(&column.name),
                                    )
                                    .map_err(|e| {
                                        conversion_error(
                                            e,
                                            &record,
                                            self.record,
                                            Some(&column.name),
                                        )
                                    })?
                                };
                                if !column.generated && !self.needs_confirmation {
                                    crate::providers::mysql::validate_import_value(&value, column)
                                        .map_err(|e| {
                                            conversion_error(
                                                e,
                                                &record,
                                                self.record,
                                                Some(&column.name),
                                            )
                                        })?;
                                }
                                values.push(value);
                            }
                            Some(RowEvent::MysqlRow {
                                record: self.record,
                                byte_offset: record.byte_offset,
                                values,
                            })
                        }
                        Some(record) => {
                            if record.fields.len() != *width || record.fields[0] != "2" {
                                return Err(location(
                                    AppError::invalid("Redis CSV 记录格式错误"),
                                    &record,
                                ));
                            }
                            match record.fields[1].as_str() {
                                "key" => {
                                    if let Some(key) = active.take() {
                                        *pending = Some(record);
                                        Some(RowEvent::RedisKeyEnd { record: key.record })
                                    } else {
                                        self.record += 1;
                                        let key = record.fields[2].clone();
                                        let kind = record.fields[3].clone();
                                        valid_key(&key, &kind)?;
                                        let expires = if record.fields[4] == "permanent" {
                                            serde_json::Value::Null
                                        } else {
                                            serde_json::Value::String(record.fields[4].clone())
                                        };
                                        *active = Some(RedisActive {
                                            key: key.clone(),
                                            kind: kind.clone(),
                                            record: self.record,
                                        });
                                        Some(RowEvent::RedisKeyHeader {
                                            record: self.record,
                                            byte_offset: record.byte_offset,
                                            key_b64: key,
                                            kind,
                                            expires_at_ms: expires,
                                        })
                                    }
                                }
                                "item" => {
                                    let key = active.as_ref().ok_or_else(|| {
                                        location(
                                            AppError::invalid("Redis CSV 元素缺少 key"),
                                            &record,
                                        )
                                    })?;
                                    if record.fields[2] != key.key || record.fields[3] != key.kind {
                                        return Err(location(
                                            AppError::invalid("Redis CSV 元素不属于当前 key"),
                                            &record,
                                        ));
                                    }
                                    let bytes = |index: usize| -> Result<Value> {
                                        STANDARD.decode(&record.fields[index]).map_err(|e| {
                                            location(
                                                AppError::invalid(format!("无效 Base64: {e}")),
                                                &record,
                                            )
                                        })?;
                                        Ok(Value::Bytes(record.fields[index].clone()))
                                    };
                                    let values = match key.kind.as_str() {
                                        "string" => vec![bytes(9)?],
                                        "hash" => {
                                            let expiry = &record.fields[10];
                                            validate_field_expiry(expiry)
                                                .map_err(|error| location(error, &record))?;
                                            vec![
                                                bytes(5)?,
                                                bytes(9)?,
                                                Value::Integer(expiry.clone()),
                                            ]
                                        }
                                        "list" => {
                                            record.fields[6].parse::<u64>().map_err(|_| {
                                                location(
                                                    AppError::invalid("无效 List 索引"),
                                                    &record,
                                                )
                                            })?;
                                            vec![
                                                Value::Unsigned(record.fields[6].clone()),
                                                bytes(9)?,
                                            ]
                                        }
                                        "set" => vec![bytes(7)?],
                                        "zset" => {
                                            crate::execution::transfer::validate_redis_score(
                                                &record.fields[8],
                                            )?;
                                            vec![bytes(7)?, Value::Float(record.fields[8].clone())]
                                        }
                                        _ => return Err(AppError::invalid("不支持的 Redis 类型")),
                                    };
                                    Some(RowEvent::RedisChunk {
                                        record: key.record,
                                        byte_offset: record.byte_offset,
                                        values,
                                    })
                                }
                                _ => {
                                    return Err(location(
                                        AppError::invalid("未知 Redis CSV 记录类型"),
                                        &record,
                                    ));
                                }
                            }
                        }
                    }
                }
                Source::Json {
                    scan,
                    first,
                    active,
                } => {
                    if let Some(key) = active {
                        if scan.array_next(&mut key.first)? {
                            let offset = scan.value_start()?;
                            let values: Vec<Value> = scan.value()?;
                            for value in &values {
                                validate_value(value)?;
                            }
                            if key.kind == "hash" {
                                match values.as_slice() {
                                    [Value::Bytes(_), Value::Bytes(_), Value::Integer(expiry)] => {
                                        validate_field_expiry(expiry)?
                                    }
                                    _ => {
                                        return Err(AppError::invalid(
                                            "Hash 行必须包含字段绝对期限",
                                        ));
                                    }
                                }
                            }
                            Some(RowEvent::RedisChunk {
                                record: key.record,
                                byte_offset: offset,
                                values,
                            })
                        } else {
                            let record = key.record;
                            let end = key.end;
                            scan.seek(end)?;
                            *active = None;
                            Some(RowEvent::RedisKeyEnd { record })
                        }
                    } else if !scan.array_next(first)? {
                        None
                    } else if self.provider == ProviderKind::Mysql {
                        self.record += 1;
                        let offset = scan.value_start()?;
                        if !self.input.record_ranges.is_empty()
                            && !self.input.record_ranges.iter().any(|range| {
                                range.first <= self.record && self.record <= range.last
                            })
                        {
                            scan.skip()?;
                            continue;
                        }
                        let values: Vec<Value> = scan.row(self.record, offset)?;
                        if values.len() != self.columns.len() {
                            return Err(json_conversion(
                                "JSON 行列数不匹配",
                                self.record,
                                offset,
                                None,
                            ));
                        }
                        for (value, column) in values.iter().zip(&self.columns) {
                            validate_value(value).map_err(|error| {
                                json_conversion(
                                    &error.message,
                                    self.record,
                                    offset,
                                    Some(&column.name),
                                )
                            })?;
                        }
                        Some(RowEvent::MysqlRow {
                            record: self.record,
                            byte_offset: offset,
                            values,
                        })
                    } else {
                        self.record += 1;
                        let offset = scan.value_start()?;
                        scan.expect(b'{')?;
                        let mut field_first = true;
                        let mut key: Option<String> = None;
                        let mut kind: Option<String> = None;
                        let mut expiry: Option<serde_json::Value> = None;
                        let mut rows = None;
                        while let Some(name) = scan.object_next(&mut field_first)? {
                            match name.as_str() {
                                "key_b64" if key.is_none() => key = Some(scan.value()?),
                                "type" if kind.is_none() => kind = Some(scan.value()?),
                                "expires_at_ms" if expiry.is_none() => expiry = Some(scan.value()?),
                                "rows" if rows.is_none() => {
                                    rows = Some(scan.value_start()?);
                                    scan.skip()?;
                                }
                                "key_b64" | "type" | "expires_at_ms" | "rows" => {
                                    return Err(AppError::invalid("Redis JSON 字段重复"));
                                }
                                _ => scan.skip()?,
                            }
                        }
                        let end = scan.position();
                        let key =
                            key.ok_or_else(|| AppError::invalid("Redis JSON 缺少 key_b64"))?;
                        let kind = kind.ok_or_else(|| AppError::invalid("Redis JSON 缺少 type"))?;
                        valid_key(&key, &kind)?;
                        let expires = expiry
                            .ok_or_else(|| AppError::invalid("Redis JSON 缺少 expires_at_ms"))?;
                        scan.seek(rows.ok_or_else(|| AppError::invalid("Redis JSON 缺少 rows"))?)?;
                        scan.expect(b'[')?;
                        *active = Some(JsonKey {
                            record: self.record,
                            kind: kind.clone(),
                            end,
                            first: true,
                        });
                        Some(RowEvent::RedisKeyHeader {
                            record: self.record,
                            byte_offset: offset,
                            key_b64: key,
                            kind,
                            expires_at_ms: expires,
                        })
                    }
                }
            };
            let record = match &event {
                Some(
                    RowEvent::MysqlRow { record, .. }
                    | RowEvent::RedisKeyHeader { record, .. }
                    | RowEvent::RedisChunk { record, .. }
                    | RowEvent::RedisKeyEnd { record },
                ) => Some(*record),
                None => None,
            };
            if let Some(record) = record
                && !self.input.record_ranges.is_empty()
                && !self
                    .input
                    .record_ranges
                    .iter()
                    .any(|r| r.first <= record && record <= r.last)
            {
                continue;
            }
            return Ok(event);
        }
    }
}
pub(super) fn valid_key(key: &str, kind: &str) -> Result<()> {
    STANDARD
        .decode(key)
        .map_err(|e| AppError::invalid(format!("无效 Base64: {e}")))?;
    if !matches!(kind, "string" | "hash" | "list" | "set" | "zset") {
        return Err(AppError::invalid("不支持的 Redis 类型"));
    }
    Ok(())
}
fn location(error: AppError, record: &CsvRecord) -> AppError {
    error.with_details(serde_json::json!({"record":record.record,"line":record.line,"byte_offset":record.byte_offset}))
}
fn conversion_error(
    mut error: AppError,
    record: &CsvRecord,
    number: u64,
    column: Option<&str>,
) -> AppError {
    error.code = "conversion".into();
    error.with_details(serde_json::json!({"record":number,"line":record.line,"byte_offset":record.byte_offset,"column":column,"recoverable":true}))
}
fn json_conversion(message: &str, record: u64, byte_offset: u64, column: Option<&str>) -> AppError {
    AppError::new("conversion", message).with_details(serde_json::json!({"record":record,"byte_offset":byte_offset,"column":column,"recoverable":true}))
}
pub(super) fn validate_value(value: &Value) -> Result<()> {
    match value {
        Value::Bytes(bytes) => {
            STANDARD
                .decode(bytes)
                .map_err(|e| AppError::invalid(format!("无效 Base64: {e}")))?;
        }
        Value::Array(values) => {
            for value in values {
                validate_value(value)?;
            }
        }
        Value::Map(values) => {
            for (key, value) in values {
                validate_value(key)?;
                validate_value(value)?;
            }
        }
        Value::Missing | Value::Large { .. } => {
            return Err(AppError::invalid("文件不能包含缺失值或大字段引用"));
        }
        _ => (),
    }
    Ok(())
}

/// Redis 字段期限以绝对毫秒表示，-1 表示永久。
pub(super) fn validate_field_expiry(expiry: &str) -> Result<()> {
    if expiry.parse::<i64>().is_ok_and(|value| value >= -1) {
        Ok(())
    } else {
        Err(AppError::invalid("Hash 字段过期时间无效"))
    }
}
