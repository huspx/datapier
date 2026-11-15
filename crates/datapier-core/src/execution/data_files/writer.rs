//! 数据文件增量写出；CSV 编码错误不使用替换字符。
use super::{AtomicOutput, JsonWriter, RowEvent};
use crate::model::{
    AppError, Column, Result, Value,
    transfer::{CsvOptions, MAX_VALUE_BYTES, TransferFormat},
};
use encoding_rs::{Encoder, EncoderResult, GB18030, GBK};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct SharedBuffer(Arc<Mutex<Vec<u8>>>);
impl Write for SharedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("CSV 缓冲区锁失败"))?
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct CsvWriter {
    output: AtomicOutput,
    options: CsvOptions,
    columns: Vec<Column>,
    redis: bool,
    active: Option<(String, String)>,
    sealed: bool,
    csv: ::csv::Writer<SharedBuffer>,
    buffer: SharedBuffer,
    scratch: Vec<u8>,
    encoder: Option<Encoder>,
}
impl CsvWriter {
    fn new(
        output: AtomicOutput,
        options: CsvOptions,
        columns: Vec<Column>,
        redis: bool,
    ) -> Result<Self> {
        options.validate()?;
        let buffer = SharedBuffer(Arc::new(Mutex::new(Vec::new())));
        let csv = ::csv::WriterBuilder::new()
            .delimiter(options.delimiter.as_bytes()[0])
            .from_writer(buffer.clone());
        let encoder = match options.encoding.to_ascii_lowercase().as_str() {
            "gbk" => Some(GBK.new_encoder()),
            "gb18030" => Some(GB18030.new_encoder()),
            _ => None,
        };
        let mut writer = Self {
            output,
            options,
            columns,
            redis,
            active: None,
            sealed: false,
            csv,
            buffer,
            scratch: Vec::with_capacity(8192),
            encoder,
        };
        if writer.options.encoding.eq_ignore_ascii_case("utf-8-bom") {
            writer.output.write_all(&[0xef, 0xbb, 0xbf])?;
        }
        if writer.options.encoding.eq_ignore_ascii_case("utf-16le") {
            writer.output.write_all(&[0xff, 0xfe])?;
        }
        if writer.options.encoding.eq_ignore_ascii_case("utf-16be") {
            writer.output.write_all(&[0xfe, 0xff])?;
        }
        if writer.options.header {
            let names: Vec<String> = if redis {
                [
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
                ]
                .iter()
                .map(|s| (*s).to_owned())
                .collect()
            } else {
                writer.columns.iter().map(|c| c.name.clone()).collect()
            };
            writer.fields(&names)?;
        }
        Ok(writer)
    }
    fn fields(&mut self, fields: &[String]) -> Result<()> {
        let total = fields
            .iter()
            .try_fold(0usize, |n, s| n.checked_add(s.len()))
            .ok_or_else(|| AppError::new("resource_limit", "CSV 单条记录超出限制"))?;
        if total > MAX_VALUE_BYTES {
            return Err(AppError::new("resource_limit", "CSV 单条记录超出限制"));
        }
        if self.sealed {
            return Err(AppError::invalid("CSV 输出已结束"));
        }
        self.csv
            .write_record(fields)
            .map_err(|e| AppError::new("csv", e.to_string()))?;
        self.csv.flush()?;
        let mut bytes = self
            .buffer
            .0
            .lock()
            .map_err(|_| AppError::new("csv", "CSV 缓冲区锁失败"))?;
        let text = std::str::from_utf8(&bytes).map_err(|e| AppError::invalid(e.to_string()))?;
        if self.options.encoding.eq_ignore_ascii_case("utf-8")
            || self.options.encoding.eq_ignore_ascii_case("utf-8-bom")
        {
            self.output.write_all(&bytes)?;
        } else if self.options.encoding.eq_ignore_ascii_case("utf-16le")
            || self.options.encoding.eq_ignore_ascii_case("utf-16be")
        {
            let le = self.options.encoding.eq_ignore_ascii_case("utf-16le");
            for unit in text.encode_utf16() {
                let pair = if le {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                };
                self.scratch.extend_from_slice(&pair);
                if self.scratch.len() == 8192 {
                    self.output.write_all(&self.scratch)?;
                    self.scratch.clear();
                }
            }
            self.output.write_all(&self.scratch)?;
            self.scratch.clear();
        } else {
            let encoder = self
                .encoder
                .as_mut()
                .ok_or_else(|| AppError::invalid("CSV 编码器缺失"))?;
            let mut input = text;
            loop {
                let mut buffer = [0u8; 8192];
                let (result, read, written) =
                    encoder.encode_from_utf8_without_replacement(input, &mut buffer, false);
                self.output.write_all(&buffer[..written])?;
                input = &input[read..];
                match result {
                    EncoderResult::InputEmpty => break,
                    EncoderResult::OutputFull => (),
                    EncoderResult::Unmappable(_) => {
                        return Err(AppError::invalid("字符无法以所选 CSV 编码表示"));
                    }
                }
            }
        }
        bytes.clear();
        Ok(())
    }
    fn seal(&mut self) -> Result<()> {
        if self.sealed {
            return Ok(());
        }
        if self.active.is_some() {
            return Err(AppError::invalid("Redis key 尚未结束"));
        }
        if let Some(encoder) = self.encoder.as_mut() {
            let mut buffer = [0u8; 32];
            let (status, _, written) =
                encoder.encode_from_utf8_without_replacement("", &mut buffer, true);
            if !matches!(status, EncoderResult::InputEmpty) {
                return Err(AppError::invalid("CSV 编码结束失败"));
            }
            self.output.write_all(&buffer[..written])?;
        }
        self.output.flush()?;
        self.sealed = true;
        Ok(())
    }
    fn mysql_row(&mut self, values: &[Value]) -> Result<()> {
        if self.redis || values.len() != self.columns.len() {
            return Err(AppError::invalid("CSV 行列数不匹配"));
        }
        let mut fields = Vec::with_capacity(values.len());
        for value in values {
            super::reader::validate_value(value)?;
            if matches!(value, Value::Null) {
                fields.push(self.options.null_marker.clone());
                continue;
            }
            let escaped = crate::execution::transfer::export::csv_value(value)?;
            let text = if self.options.backslash_escape {
                escaped
            } else {
                crate::execution::transfer::unescape(&escaped)?
            };
            if text == self.options.null_marker {
                return Err(AppError::invalid("CSV 普通值与 NULL 标记冲突"));
            }
            fields.push(text);
        }
        self.fields(&fields)
    }
    fn redis_event(&mut self, event: &RowEvent) -> Result<()> {
        if !self.redis {
            return Err(AppError::invalid("MySQL CSV 不能写入 Redis 事件"));
        }
        match event {
            RowEvent::RedisKeyHeader {
                key_b64,
                kind,
                expires_at_ms,
                ..
            } => {
                if self.active.is_some() {
                    return Err(AppError::invalid("Redis key 尚未结束"));
                }
                super::reader::valid_key(key_b64, kind)?;
                let expiry = match expires_at_ms {
                    serde_json::Value::Null => "permanent".to_owned(),
                    serde_json::Value::String(s) => s.clone(),
                    _ => return Err(AppError::invalid("Redis 过期时间无效")),
                };
                let fields = vec![
                    "2".into(),
                    "key".into(),
                    key_b64.clone(),
                    kind.clone(),
                    expiry,
                    "".into(),
                    "".into(),
                    "".into(),
                    "".into(),
                    "".into(),
                    "".into(),
                ];
                self.fields(&fields)?;
                self.active = Some((key_b64.clone(), kind.clone()));
            }
            RowEvent::RedisChunk { values, .. } => {
                for value in values {
                    super::reader::validate_value(value)?;
                }
                let (key, kind) = self
                    .active
                    .as_ref()
                    .ok_or_else(|| AppError::invalid("Redis 元素缺少 key"))?;
                let mut fields = vec![
                    "2".to_owned(),
                    "item".to_owned(),
                    key.clone(),
                    kind.clone(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                    String::new(),
                ];
                let bytes = |value: &Value| -> Result<String> {
                    if let Value::Bytes(s) = value {
                        Ok(s.clone())
                    } else {
                        Err(AppError::invalid("Redis 二进制值类型错误"))
                    }
                };
                match (kind.as_str(), values.as_slice()) {
                    ("string", [v]) => fields[9] = bytes(v)?,
                    ("hash", [f, v, Value::Integer(expiry)]) => {
                        fields[5] = bytes(f)?;
                        fields[9] = bytes(v)?;
                        super::reader::validate_field_expiry(expiry)?;
                        fields[10] = expiry.clone();
                    }
                    ("list", [Value::Unsigned(i), v]) | ("list", [Value::Integer(i), v]) => {
                        i.parse::<u64>()
                            .map_err(|_| AppError::invalid("无效 List 索引"))?;
                        fields[6] = i.clone();
                        fields[9] = bytes(v)?;
                    }
                    ("set", [v]) => fields[7] = bytes(v)?,
                    ("zset", [v, Value::Float(s)]) => {
                        crate::execution::transfer::validate_redis_score(s)?;
                        fields[7] = bytes(v)?;
                        fields[8] = s.clone();
                    }
                    _ => return Err(AppError::invalid("Redis 行格式错误")),
                }
                self.fields(&fields)?;
            }
            RowEvent::RedisKeyEnd { .. } => {
                if self.active.take().is_none() {
                    return Err(AppError::invalid("Redis key 顺序错误"));
                }
            }
            RowEvent::MysqlRow { .. } => {
                return Err(AppError::invalid("Redis CSV 不能写入 MySQL 行"));
            }
        }
        Ok(())
    }
}

enum Writer {
    Csv(Box<CsvWriter>),
    Json(JsonWriter),
}
/// 支持 CSV 和 Datapier 类型化 JSON 的增量写出。
pub struct CsvJsonWriter {
    inner: Writer,
}
impl CsvJsonWriter {
    pub fn mysql(
        output: AtomicOutput,
        format: TransferFormat,
        csv: CsvOptions,
        columns: Vec<Column>,
    ) -> Result<Self> {
        let inner = match format {
            TransferFormat::Csv => {
                Writer::Csv(Box::new(CsvWriter::new(output, csv, columns, false)?))
            }
            TransferFormat::Json => Writer::Json(JsonWriter::mysql(output, &columns)?),
            TransferFormat::Sql => return Err(AppError::invalid("SQL 不是数据文件格式")),
        };
        Ok(Self { inner })
    }
    pub fn redis(output: AtomicOutput, format: TransferFormat, csv: CsvOptions) -> Result<Self> {
        let inner = match format {
            TransferFormat::Csv => {
                Writer::Csv(Box::new(CsvWriter::new(output, csv, Vec::new(), true)?))
            }
            TransferFormat::Json => Writer::Json(JsonWriter::redis(output)?),
            TransferFormat::Sql => return Err(AppError::invalid("SQL 不是数据文件格式")),
        };
        Ok(Self { inner })
    }
    pub fn partial_path(&self) -> &Path {
        match &self.inner {
            Writer::Csv(w) => w.output.partial_path(),
            Writer::Json(w) => w.partial_path(),
        }
    }
    /// 输出流已写入的文件字节数，不表示文件已发布。
    pub fn bytes_written(&self) -> u64 {
        match &self.inner {
            Writer::Csv(w) => w.output.bytes_written(),
            Writer::Json(w) => w.output.bytes_written(),
        }
    }
    pub fn snapshot(&mut self, with_sha256: bool) -> Result<super::OutputSnapshot> {
        match &mut self.inner {
            Writer::Csv(w) => w.output.snapshot(with_sha256),
            Writer::Json(w) => w.snapshot(with_sha256),
        }
    }
    /// 补齐容器结尾并 flush，之后可对最终字节登记摘要。
    pub fn seal(&mut self) -> Result<()> {
        match &mut self.inner {
            Writer::Csv(w) => w.seal(),
            Writer::Json(w) => {
                w.seal()?;
                w.output.flush()?;
                Ok(())
            }
        }
    }
    pub fn write_row(&mut self, values: &[Value]) -> Result<()> {
        match &mut self.inner {
            Writer::Csv(w) => w.mysql_row(values),
            Writer::Json(w) => w.write_row(values),
        }
    }
    pub fn write_event(&mut self, event: &RowEvent) -> Result<()> {
        match &mut self.inner {
            Writer::Csv(w) => w.redis_event(event),
            Writer::Json(w) => match event {
                RowEvent::RedisKeyHeader {
                    key_b64,
                    kind,
                    expires_at_ms,
                    ..
                } => w.begin_key(key_b64, kind, expires_at_ms),
                RowEvent::RedisChunk { values, .. } => w.write_row(values),
                RowEvent::RedisKeyEnd { .. } => w.end_key(),
                RowEvent::MysqlRow { values, .. } => w.write_row(values),
            },
        }
    }
    pub fn finish(mut self) -> Result<PathBuf> {
        self.seal().map_err(|error| {
            super::publication_error(error, super::PublicationState::NotPublished)
        })?;
        match self.inner {
            Writer::Csv(w) => w.output.publish(),
            Writer::Json(w) => w.finish(),
        }
    }
}
