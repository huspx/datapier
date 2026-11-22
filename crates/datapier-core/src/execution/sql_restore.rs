//! MySQL 逻辑备份的只读预检与有界字面量读取。
//!
//! 索引只包含输入偏移和结构元数据；数据行仅在回调期间存在于内存。

mod classify;
mod literal;
pub(crate) mod persist;
mod rewrite;
mod scan;

use literal::decode_hex;
pub use rewrite::rewrite_ddl;
use scan::{ScanSettings, scan};

use crate::model::{AppError, Result, Value};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use encoding_rs::{
    Decoder, DecoderResult, Encoding, GB18030, GBK, UTF_8, UTF_16BE, UTF_16LE, WINDOWS_1252,
};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{BufReader, Read, Seek, SeekFrom},
    path::Path,
};

const LIMIT: usize = 16 * 1024 * 1024;
// 无外部预留器时仍为单行临时空间设置上界，允许 16 MiB BIT 字面量展开。
const TUPLE_MEMORY_BUDGET: usize = 2 * 1024 * 1024 * 1024;
const HEADER: &str = "-- datapier-sql format=datapier-sql version=1\n";

/// 单条语句创建时的会话状态。原始 SET 语句按顺序保留以供独立会话重建。
#[derive(Debug, Clone, Default)]
pub struct SqlContext {
    pub database: Option<String>,
    pub settings: Vec<String>,
    pub no_backslash_escapes: bool,
    pub ansi_quotes: bool,
    saved_mode: Option<(bool, bool)>,
    saved_object_mode: Option<(bool, bool)>,
    pub input_encoding: String,
    pub character_set_client: Option<String>,
    old_character_set_client: Option<Option<String>>,
    saved_character_set_client: Option<Option<String>>,
}

/// 输入中的语句类别；包装语句必须在计划中显式披露其语义变化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlKind {
    Database,
    Table,
    View,
    Trigger,
    Procedure,
    Function,
    Event,
    Index,
    Drop,
    Insert,
    Context,
    Wrapper,
}

/// 存储对象创建前后的库默认值切换；索引分别指向两条 ALTER DATABASE。
#[derive(Debug, Clone)]
pub struct DatabaseContext {
    pub database: String,
    pub before_statement: usize,
    pub restore_statement: usize,
}

/// 一个输入语句的偏移与逻辑对象信息，不含 INSERT 行值。
#[derive(Debug, Clone)]
pub struct SqlStatement {
    pub start: u64,
    pub end: u64,
    pub line: u64,
    pub kind: SqlKind,
    pub is_create: bool,
    pub drop_kind: Option<SqlKind>,
    pub object: Option<String>,
    pub source_database: Option<String>,
    pub source_name: Option<String>,
    pub index_table_database: Option<String>,
    pub index_table_name: Option<String>,
    /// 原始 DEFINER 语法；映射时应依据完整 DDL 再做语法级改写。
    pub definer: Option<String>,
    pub sql_security: Option<String>,
    pub columns: Option<Vec<String>>,
    /// 输入 CREATE TABLE 的列顺序；纯数据文件可能缺失。
    pub source_columns: Option<Vec<String>>,
    pub row_width: Option<usize>,
    pub context: SqlContext,
    pub version: Option<u32>,
    pub version_constraints: Vec<VersionConstraint>,
    pub unresolved_references: bool,
    pub placeholder_view: bool,
    pub tuple_start: Option<u64>,
    pub references: Vec<String>,
    pub function_calls: Vec<String>,
    pub database_context: Option<DatabaseContext>,
    /// 该数据库语句是创建对象上下文切换，不能作为普通 SET 运行。
    pub alters_database_defaults: bool,
    pub delimiter: String,
}

/// 可执行注释的原始字节范围和最低 MySQL 版本；无版本号代表无条件。
#[derive(Debug, Clone)]
pub struct VersionConstraint {
    pub start: u64,
    pub end: u64,
    pub minimum: Option<u32>,
}

/// 自生成备份头中声明的对象及选中的结构、数据范围。
#[derive(Debug, Clone)]
pub struct DeclaredObject {
    pub kind: SqlKind,
    pub database: String,
    pub name: String,
    pub schema: bool,
    pub data: bool,
}

/// 已完整预检的输入；执行前调用方还须检查每个数据块的文件身份。
#[derive(Debug, Clone)]
pub struct SqlIndex {
    pub statements: Vec<SqlStatement>,
    pub sha256: String,
    pub bytes: u64,
    pub datapier_format: bool,
    pub declared_objects: Vec<DeclaredObject>,
    pub input_encoding: String,
    bom_bytes: usize,
    fixed_encoding: bool,
    block_hashes: Vec<[u8; 32]>,
}

/// 跨输入累计保留索引容量；不计入逐行回调中的临时数据。
pub(crate) struct IndexBudget {
    used: usize,
    limit: usize,
    tuple_reserver: Option<Box<dyn FnMut(usize) -> Result<()> + Send>>,
    tuple_reserved: usize,
}

impl IndexBudget {
    pub(crate) fn new() -> Self {
        Self {
            used: 0,
            limit: crate::model::transfer::MAX_PLAN_BYTES,
            tuple_reserver: None,
            tuple_reserved: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_limit(limit: usize) -> Self {
        Self {
            used: 0,
            limit,
            tuple_reserver: None,
            tuple_reserved: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> usize {
        self.used
    }

    /// 回调接收单行临时内存的完整高水位估算；预留失败时不读取或解码该增长。
    pub(crate) fn set_tuple_reserver(
        &mut self,
        reserver: Box<dyn FnMut(usize) -> Result<()> + Send>,
    ) {
        self.tuple_reserver = Some(reserver);
        self.tuple_reserved = 0;
    }

    fn reserve_tuple(&mut self, estimate: usize) -> Result<()> {
        if estimate <= self.tuple_reserved {
            return Ok(());
        }
        if let Some(reserver) = self.tuple_reserver.as_mut() {
            let reserved = estimate.checked_next_power_of_two().unwrap_or(usize::MAX);
            reserver(reserved)?;
            self.tuple_reserved = reserved;
        } else if estimate > TUPLE_MEMORY_BUDGET {
            return Err(AppError::new("resource_limit", "INSERT 单行超过内存预算"));
        } else {
            self.tuple_reserved = estimate
                .checked_next_power_of_two()
                .unwrap_or(usize::MAX)
                .min(TUPLE_MEMORY_BUDGET);
        }
        Ok(())
    }

    fn check(&self, bytes: usize) -> Result<()> {
        if self.used.saturating_add(bytes) > self.limit {
            return Err(AppError::new("resource_limit", "SQL 索引超过内存预算"));
        }
        Ok(())
    }

    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.check(bytes)?;
        self.used += bytes;
        Ok(())
    }

    fn replace(&mut self, old: usize, new: usize) -> Result<()> {
        self.used = self.used.saturating_sub(old);
        self.charge(new)
    }

    fn push<T>(&mut self, vec: &mut Vec<T>, value: T) -> Result<()> {
        if vec.len() == vec.capacity() {
            let old = vec.capacity();
            let next = old.saturating_mul(2).max(4);
            let extra = next
                .saturating_sub(old)
                .saturating_mul(std::mem::size_of::<T>());
            self.check(extra)?;
            vec.try_reserve_exact(next - vec.len())
                .map_err(|_| AppError::new("resource_limit", "SQL 索引分配失败"))?;
            self.charge(
                vec.capacity()
                    .saturating_sub(old)
                    .saturating_mul(std::mem::size_of::<T>()),
            )?;
        }
        vec.push(value);
        Ok(())
    }
}

fn strings_heap(values: &[String]) -> usize {
    values.iter().map(String::capacity).sum()
}

fn string_vec_heap(values: &Vec<String>) -> usize {
    values
        .capacity()
        .saturating_mul(std::mem::size_of::<String>())
        .saturating_add(strings_heap(values))
}

fn context_heap(context: &SqlContext) -> usize {
    context
        .database
        .as_ref()
        .map_or(0, String::capacity)
        .saturating_add(string_vec_heap(&context.settings))
        .saturating_add(context.input_encoding.capacity())
        .saturating_add(
            context
                .character_set_client
                .as_ref()
                .map_or(0, String::capacity),
        )
        .saturating_add(
            context
                .old_character_set_client
                .as_ref()
                .and_then(Option::as_ref)
                .map_or(0, String::capacity),
        )
        .saturating_add(
            context
                .saved_character_set_client
                .as_ref()
                .and_then(Option::as_ref)
                .map_or(0, String::capacity),
        )
}

fn statement_heap(statement: &SqlStatement) -> usize {
    let strings = [
        &statement.object,
        &statement.source_database,
        &statement.source_name,
        &statement.index_table_database,
        &statement.index_table_name,
        &statement.definer,
        &statement.sql_security,
    ];
    strings
        .into_iter()
        .flatten()
        .map(String::capacity)
        .sum::<usize>()
        .saturating_add(statement.columns.as_ref().map_or(0, string_vec_heap))
        .saturating_add(statement.source_columns.as_ref().map_or(0, string_vec_heap))
        .saturating_add(context_heap(&statement.context))
        .saturating_add(string_vec_heap(&statement.references))
        .saturating_add(string_vec_heap(&statement.function_calls))
        .saturating_add(
            statement
                .database_context
                .as_ref()
                .map_or(0, |v| v.database.capacity()),
        )
        .saturating_add(statement.delimiter.capacity())
}

fn push_statement(
    budget: &mut IndexBudget,
    statements: &mut Vec<SqlStatement>,
    statement: SqlStatement,
) -> Result<()> {
    // version_constraints 的 Vec 容量在扫描注释时已经计入。
    let already = statement
        .columns
        .as_ref()
        .map_or(0, string_vec_heap)
        .saturating_add(statement.source_columns.as_ref().map_or(0, string_vec_heap))
        .saturating_add(string_vec_heap(&statement.references))
        .saturating_add(string_vec_heap(&statement.function_calls))
        .saturating_add(context_heap(&statement.context));
    budget.charge(statement_heap(&statement).saturating_sub(already))?;
    budget.push(statements, statement)
}

fn err(offset: u64, line: u64, message: impl AsRef<str>) -> AppError {
    AppError::invalid(format!(
        "SQL 第 {line} 行、字节 {offset}: {}",
        message.as_ref()
    ))
    .with_details(serde_json::json!({"byte_offset": offset, "line": line}))
}

/// 自生成备份的版本首行。
pub fn backup_header() -> &'static str {
    HEADER
}

/// 根据首行到尾标记之前的全部原始字节计算完整标记。
pub fn backup_footer(digest: &[u8]) -> String {
    let mut hex = String::new();
    for b in digest {
        use std::fmt::Write as _;
        let _ = write!(hex, "{b:02x}");
    }
    format!("-- datapier-sql complete sha256={hex}\n")
}

/// 声明备份对象数量，零对象用于完整空库备份。
pub fn backup_object_count(count: usize) -> String {
    format!("-- datapier-objects count={count}\n")
}

/// 生成与预检器共享的对象清单行；名称按 UTF-8 字节编码。
pub fn backup_object_marker(object: &DeclaredObject) -> Result<String> {
    let kind = match object.kind {
        SqlKind::Table => "table",
        SqlKind::View => "view",
        SqlKind::Trigger => "trigger",
        SqlKind::Procedure => "procedure",
        SqlKind::Function => "function",
        SqlKind::Event => "event",
        _ => return Err(AppError::invalid("不支持的备份对象类型")),
    };
    if object.database.is_empty()
        || object.name.is_empty()
        || (!object.schema && !object.data)
        || (object.data && object.kind != SqlKind::Table)
    {
        return Err(AppError::invalid("非法备份对象清单"));
    }
    Ok(format!(
        "-- datapier-object kind={kind} database={} name={} schema={} data={}\n",
        &hex_literal(object.database.as_bytes())[2..],
        &hex_literal(object.name.as_bytes())[2..],
        u8::from(object.schema),
        u8::from(object.data)
    ))
}

/// 将类型化值编码为不依赖 NO_BACKSLASH_ESCAPES 的 MySQL 字面量。
pub fn encode_literal(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok("NULL".into()),
        Value::Bool(v) => Ok(if *v { "1" } else { "0" }.into()),
        Value::Integer(v) | Value::Unsigned(v) | Value::Decimal(v) | Value::Float(v) => {
            if !numeric(v) {
                return Err(AppError::invalid("非法数字字面量"));
            }
            Ok(v.clone())
        }
        Value::Bytes(v) => {
            let bytes = STANDARD
                .decode(v)
                .map_err(|_| AppError::invalid("非法 Base64"))?;
            Ok(format!("X'{}'", &hex_literal(&bytes)[2..]))
        }
        Value::Text(v) | Value::Date(v) | Value::Time(v) | Value::DateTime(v) => {
            Ok(format!("_utf8mb4 X'{}'", &hex_literal(v.as_bytes())[2..]))
        }
        Value::Json(v) => Ok(format!(
            "_utf8mb4 X'{}'",
            &hex_literal(v.to_string().as_bytes())[2..]
        )),
        _ => Err(AppError::invalid("该值不能编码为 SQL 字面量")),
    }
}
fn hex_literal(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for b in bytes {
        let _ = write!(out, "{b:02X}");
    }
    out
}
fn numeric(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0;
    if matches!(b.first(), Some(b'+' | b'-')) {
        i += 1;
    }
    let mut digits = 0;
    while i < b.len() && b[i].is_ascii_digit() {
        digits += 1;
        i += 1;
    }
    if i < b.len() && b[i] == b'.' {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            digits += 1;
            i += 1;
        }
    }
    if digits == 0 {
        return false;
    }
    if i < b.len() && matches!(b[i], b'e' | b'E') {
        i += 1;
        if i < b.len() && matches!(b[i], b'+' | b'-') {
            i += 1;
        }
        let start = i;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        if start == i {
            return false;
        }
    }
    i == b.len()
}

const BLOCK: usize = 1024 * 1024;
fn encoding_for(name: &str) -> Result<&'static Encoding> {
    match name.to_ascii_lowercase().replace('_', "-").as_str() {
        "utf8" | "utf8mb3" | "utf8mb4" | "utf-8" | "utf-8-bom" | "ascii" => Ok(UTF_8),
        "utf16" | "utf-16" | "utf-16le" => Ok(UTF_16LE),
        "utf-16be" => Ok(UTF_16BE),
        "gbk" => Ok(GBK),
        "gb18030" => Ok(GB18030),
        "latin1" | "windows-1252" | "cp1252" => Ok(WINDOWS_1252),
        _ => Err(AppError::invalid(format!("不支持的 SQL 输入字符集 {name}"))),
    }
}
struct Input<'a> {
    file: BufReader<File>,
    pos: u64,
    byte_start: u64,
    line: u64,
    peeked: Option<(u8, u64, u64)>,
    hashes: &'a [[u8; 32]],
    block: Vec<u8>,
    cursor: usize,
    block_index: usize,
    decoder: Decoder,
    encoding: &'static Encoding,
    raw_pos: u64,
    char_start: u64,
    decoded: std::collections::VecDeque<(u8, u64, u64)>,
    ended: bool,
    cancel: Option<tokio_util::sync::CancellationToken>,
}
impl<'a> Input<'a> {
    fn new(
        file: File,
        hashes: &'a [[u8; 32]],
        encoding: &'static Encoding,
        bom: usize,
        cancel: Option<tokio_util::sync::CancellationToken>,
    ) -> Result<Self> {
        let mut input = Self {
            file: BufReader::new(file),
            pos: 0,
            byte_start: 0,
            line: 1,
            peeked: None,
            hashes,
            block: Vec::new(),
            cursor: 0,
            block_index: 0,
            decoder: encoding.new_decoder_without_bom_handling(),
            encoding,
            raw_pos: 0,
            char_start: 0,
            decoded: std::collections::VecDeque::new(),
            ended: false,
            cancel,
        };
        for _ in 0..bom {
            if input.raw_byte()?.is_none() {
                return Err(AppError::invalid("SQL BOM 不完整"));
            }
            input.raw_pos += 1;
        }
        input.pos = bom as u64;
        input.char_start = input.pos;
        Ok(input)
    }
    /// 从已索引语句的字符边界开始，仍校验其所在完整输入块。
    fn seek_statement(&mut self, start: u64, line: u64) -> Result<()> {
        let block = start / BLOCK as u64;
        self.file.seek(SeekFrom::Start(block * BLOCK as u64))?;
        self.block_index = usize::try_from(block).map_err(|_| AppError::invalid("SQL 偏移超限"))?;
        self.block.clear();
        self.cursor = 0;
        for _ in 0..start % BLOCK as u64 {
            self.raw_byte()?
                .ok_or_else(|| AppError::conflict("SQL 输入已截断"))?;
        }
        self.pos = start;
        self.byte_start = start;
        self.raw_pos = start;
        self.char_start = start;
        self.line = line;
        Ok(())
    }
    fn raw_byte(&mut self) -> Result<Option<u8>> {
        if self.cursor == self.block.len() {
            if self
                .cancel
                .as_ref()
                .is_some_and(|token| token.is_cancelled())
            {
                return Err(AppError::cancelled());
            }
            let mut data = vec![0; BLOCK];
            let mut size = 0;
            while size < BLOCK {
                let n = self.file.read(&mut data[size..])?;
                if n == 0 {
                    break;
                }
                size += n;
            }
            data.truncate(size);
            if size == 0 {
                if self.block_index != self.hashes.len() {
                    return Err(AppError::conflict("SQL 输入已截断"));
                }
                return Ok(None);
            }
            let actual: [u8; 32] = Sha256::digest(&data).into();
            if self.hashes.get(self.block_index) != Some(&actual) {
                return Err(AppError::conflict("SQL 输入内容在预检后发生变化"));
            }
            self.block_index += 1;
            self.block = data;
            self.cursor = 0;
        }
        let b = self.block[self.cursor];
        self.cursor += 1;
        Ok(Some(b))
    }
    fn decoded_byte(&mut self) -> Result<Option<(u8, u64, u64)>> {
        if let Some(value) = self.decoded.pop_front() {
            return Ok(Some(value));
        }
        if self.ended {
            return Ok(None);
        }
        loop {
            let input = self.raw_byte()?;
            let data = [input.unwrap_or(0)];
            let mut output = [0u8; 16];
            if let Some(b) = input
                && b.is_ascii()
                && self.char_start == self.raw_pos
                && self.encoding != UTF_16LE
                && self.encoding != UTF_16BE
            {
                let start = self.raw_pos;
                self.raw_pos += 1;
                self.char_start = self.raw_pos;
                return Ok(Some((b, start, self.raw_pos)));
            }
            let (status, consumed, written) = self.decoder.decode_to_utf8_without_replacement(
                if input.is_some() { &data } else { &[] },
                &mut output,
                input.is_none(),
            );
            self.raw_pos += consumed as u64;
            match status {
                DecoderResult::Malformed(_, _) => {
                    return Err(err(self.char_start, self.line, "SQL 输入字符编码错误"));
                }
                DecoderResult::OutputFull => {
                    return Err(AppError::new("resource_limit", "SQL 解码缓冲区不足"));
                }
                DecoderResult::InputEmpty => {}
            }
            if input.is_some() && consumed != 1 {
                return Err(AppError::invalid("SQL 解码器未消费输入"));
            }
            if written > 0 {
                let start = self.char_start;
                let end = self.raw_pos;
                self.char_start = end;
                for &b in &output[..written] {
                    self.decoded.push_back((b, start, end));
                }
                return Ok(self.decoded.pop_front());
            }
            if input.is_none() {
                self.ended = true;
                return Ok(None);
            }
        }
    }
    fn set_encoding(&mut self, encoding: &'static Encoding) -> Result<()> {
        if self.encoding == encoding {
            return Ok(());
        }
        if self.peeked.is_some() || !self.decoded.is_empty() || self.raw_pos != self.pos {
            return Err(AppError::invalid("SQL 字符集切换发生在字符中间"));
        }
        self.encoding = encoding;
        self.decoder = encoding.new_decoder_without_bom_handling();
        self.char_start = self.raw_pos;
        self.ended = false;
        Ok(())
    }
    fn peek(&mut self) -> Result<Option<u8>> {
        if self.peeked.is_none() {
            self.peeked = self.decoded_byte()?;
        }
        Ok(self.peeked.map(|v| v.0))
    }
    fn next(&mut self) -> Result<Option<u8>> {
        let value = if let Some(v) = self.peeked.take() {
            Some(v)
        } else {
            self.decoded_byte()?
        };
        if let Some((b, start, end)) = value {
            self.byte_start = start;
            self.pos = end;
            if b == b'\n' {
                self.line += 1;
            }
        }
        Ok(value.map(|v| v.0))
    }
}

fn finish_declared(
    objects: Vec<DeclaredObject>,
    expected: Option<usize>,
) -> Result<Vec<DeclaredObject>> {
    if expected != Some(objects.len()) {
        return Err(AppError::invalid("SQL 对象清单数量缺失或不匹配"));
    }
    Ok(objects)
}

fn declared_objects(
    file: File,
    hashes: &[[u8; 32]],
    cancel: Option<tokio_util::sync::CancellationToken>,
    budget: &mut IndexBudget,
) -> Result<Vec<DeclaredObject>> {
    let mut input = Input::new(file, hashes, UTF_8, 0, cancel)?;
    let mut objects: Vec<DeclaredObject> = Vec::new();
    let mut expected = None;
    loop {
        let mut line = Vec::new();
        while let Some(b) = input.next()? {
            if b == b'\n' {
                break;
            }
            line.push(b);
            if line.len() > 8192 {
                return Err(AppError::invalid("SQL 头部注释过长"));
            }
            if let Some(first) = line.iter().position(|c| !c.is_ascii_whitespace()) {
                let trimmed = &line[first..];
                if trimmed[0] != b'-' || (trimmed.len() >= 2 && &trimmed[..2] != b"--") {
                    return finish_declared(objects, expected);
                }
            }
        }
        if line.is_empty() && input.peek()?.is_none() {
            return finish_declared(objects, expected);
        }
        let text = std::str::from_utf8(&line)
            .map_err(|_| AppError::invalid("SQL 头部不是 UTF-8"))?
            .trim();
        if let Some(count) = text.strip_prefix("-- datapier-objects count=") {
            if expected.is_some() || !objects.is_empty() {
                return Err(AppError::invalid("SQL 对象数量声明重复或顺序错误"));
            }
            expected = Some(
                count
                    .parse::<usize>()
                    .map_err(|_| AppError::invalid("SQL 对象数量声明错误"))?,
            );
            continue;
        }
        let Some(fields) = text.strip_prefix("-- datapier-object ") else {
            continue;
        };
        let parts: Vec<&str> = fields.split_ascii_whitespace().collect();
        if parts.len() != 5 {
            return Err(AppError::invalid("SQL 对象清单格式错误"));
        }
        let kind = match parts[0].strip_prefix("kind=") {
            Some("table") => SqlKind::Table,
            Some("view") => SqlKind::View,
            Some("trigger") => SqlKind::Trigger,
            Some("procedure") => SqlKind::Procedure,
            Some("function") => SqlKind::Function,
            Some("event") => SqlKind::Event,
            _ => return Err(AppError::invalid("SQL 对象清单类型错误")),
        };
        let decode = |s: &str, key: &str| -> Result<String> {
            let hex = s
                .strip_prefix(key)
                .ok_or_else(|| AppError::invalid("SQL 对象清单字段错误"))?;
            let value = String::from_utf8(decode_hex(hex, 0, 1)?)
                .map_err(|_| AppError::invalid("SQL 对象名不是 UTF-8"))?;
            if value.is_empty() {
                return Err(AppError::invalid("SQL 对象名为空"));
            }
            Ok(value)
        };
        let database = decode(parts[1], "database=")?;
        let name = decode(parts[2], "name=")?;
        let flag = |s: &str, key: &str| -> Result<bool> {
            match s.strip_prefix(key) {
                Some("0") => Ok(false),
                Some("1") => Ok(true),
                _ => Err(AppError::invalid("SQL 对象清单范围错误")),
            }
        };
        let schema = flag(parts[3], "schema=")?;
        let data = flag(parts[4], "data=")?;
        if (!schema && !data) || (data && kind != SqlKind::Table) {
            return Err(AppError::invalid("SQL 对象清单选择错误"));
        }
        if objects
            .iter()
            .any(|o| o.kind == kind && o.database == database && o.name == name)
        {
            return Err(AppError::invalid("SQL 对象清单重复"));
        }
        budget.charge(database.capacity().saturating_add(name.capacity()))?;
        budget.push(
            &mut objects,
            DeclaredObject {
                kind,
                database,
                name,
                schema,
                data,
            },
        )?;
    }
}

fn validate_declared(objects: &[DeclaredObject], statements: &[SqlStatement]) -> Result<()> {
    // 空清单合法：调用方已通过显式 count=0 声明空范围。
    for object in objects {
        if object.schema
            && !statements.iter().any(|s| {
                s.kind == object.kind
                    && s.is_create
                    && !s.placeholder_view
                    && s.source_database.as_deref() == Some(object.database.as_str())
                    && s.source_name.as_deref() == Some(object.name.as_str())
            })
        {
            return Err(AppError::invalid(format!(
                "SQL 缺少声明的对象 {}.{}",
                object.database, object.name
            )));
        }
    }
    for statement in statements {
        if statement.placeholder_view {
            continue;
        }
        let kind = if statement.kind == SqlKind::Insert {
            SqlKind::Table
        } else {
            statement.kind
        };
        if !matches!(
            kind,
            SqlKind::Table
                | SqlKind::View
                | SqlKind::Trigger
                | SqlKind::Procedure
                | SqlKind::Function
                | SqlKind::Event
        ) {
            continue;
        }
        let found = objects.iter().any(|o| {
            o.kind == kind
                && statement.source_database.as_deref() == Some(o.database.as_str())
                && statement.source_name.as_deref() == Some(o.name.as_str())
                && if statement.kind == SqlKind::Insert {
                    o.data
                } else {
                    o.schema
                }
        });
        if !found {
            return Err(err(
                statement.start,
                statement.line,
                "SQL 包含未声明的对象或数据",
            ));
        }
    }
    Ok(())
}

/// 完整预检 SQL 输入，并登记每 1 MiB 原始字节的 SHA-256；不会保存行值。
pub fn inspect(path: impl AsRef<Path>) -> Result<SqlIndex> {
    inspect_inner(path.as_ref(), None, None, &mut IndexBudget::new())
}

/// 使用指定文件编码预检；BOM 与指定编码冲突时拒绝。
pub fn inspect_with_encoding(path: impl AsRef<Path>, encoding: &str) -> Result<SqlIndex> {
    inspect_inner(path.as_ref(), Some(encoding), None, &mut IndexBudget::new())
}

/// 允许在哈希、解码及完整扫描期间取消预检。
pub fn inspect_with_cancel(
    path: impl AsRef<Path>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<SqlIndex> {
    inspect_inner(path.as_ref(), None, Some(cancel), &mut IndexBudget::new())
}

/// 使用指定编码进行可取消预检。
pub fn inspect_with_encoding_and_cancel(
    path: impl AsRef<Path>,
    encoding: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<SqlIndex> {
    inspect_inner(
        path.as_ref(),
        Some(encoding),
        Some(cancel),
        &mut IndexBudget::new(),
    )
}

pub(crate) fn inspect_with_budget(
    path: &Path,
    encoding: &str,
    cancel: &tokio_util::sync::CancellationToken,
    budget: &mut IndexBudget,
) -> Result<SqlIndex> {
    let requested = (!encoding.eq_ignore_ascii_case("utf-8")).then_some(encoding);
    inspect_inner(path, requested, Some(cancel), budget)
}

fn inspect_inner(
    path: &Path,
    requested: Option<&str>,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    budget: &mut IndexBudget,
) -> Result<SqlIndex> {
    budget.charge(std::mem::size_of::<SqlIndex>() + 64 + 32)?;
    let mut file = File::open(path)?;
    let bytes = file.metadata()?.len();
    let mut first = vec![0; HEADER.len().min(bytes as usize)];
    file.read_exact(&mut first)?;
    let datapier_format = first.starts_with(b"-- datapier-sql");
    if datapier_format && first != HEADER.as_bytes() {
        return Err(AppError::invalid("不支持的 datapier-sql 首行或版本"));
    }
    let detected = if first.starts_with(&[0xef, 0xbb, 0xbf]) {
        Some((UTF_8, 3))
    } else if first.starts_with(&[0xff, 0xfe]) {
        Some((UTF_16LE, 2))
    } else if first.starts_with(&[0xfe, 0xff]) {
        Some((UTF_16BE, 2))
    } else {
        None
    };
    let chosen = requested.map(encoding_for).transpose()?;
    if let (Some(requested), Some((bom, _))) = (chosen, detected)
        && requested != bom
    {
        return Err(AppError::invalid("SQL BOM 与指定编码冲突"));
    }
    let (encoding, bom_bytes) = detected.unwrap_or((chosen.unwrap_or(UTF_8), 0));
    if bom_bytes > 0
        && encoding == UTF_8
        && first
            .get(bom_bytes..)
            .is_some_and(|v| v.starts_with(b"-- datapier-sql"))
    {
        return Err(AppError::invalid("自生成 SQL 首行不能包含 BOM"));
    }
    if datapier_format && encoding != UTF_8 {
        return Err(AppError::invalid("自生成 SQL 必须为 UTF-8"));
    }
    let fixed_encoding = requested.is_some() || detected.is_some() || datapier_format;
    let footer_start = if datapier_format {
        let tail_len = bytes.min(4096) as usize;
        file.seek(SeekFrom::Start(bytes - tail_len as u64))?;
        let mut tail = vec![0; tail_len];
        file.read_exact(&mut tail)?;
        let end = tail
            .iter()
            .rposition(|b| !b.is_ascii_whitespace())
            .ok_or_else(|| AppError::invalid("缺少 SQL 完成标记"))?
            + 1;
        let begin = tail[..end]
            .iter()
            .rposition(|b| *b == b'\n')
            .map_or(0, |i| i + 1);
        if begin == 0 && bytes > tail_len as u64 {
            return Err(AppError::invalid("SQL 完成标记错误"));
        }
        let marker = std::str::from_utf8(&tail[begin..end])
            .map_err(|_| AppError::invalid("SQL 完成标记编码错误"))?;
        let digest = marker
            .strip_prefix("-- datapier-sql complete sha256=")
            .ok_or_else(|| AppError::invalid("缺少 SQL 完成标记"))?;
        if digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(AppError::invalid("SQL 完成标记摘要错误"));
        }
        Some((
            bytes - tail_len as u64 + begin as u64,
            digest.to_ascii_lowercase(),
        ))
    } else {
        None
    };
    file.seek(SeekFrom::Start(0))?;
    let mut hashes = Vec::new();
    let mut full = Sha256::new();
    let mut prefix = Sha256::new();
    let mut position = 0u64;
    loop {
        if cancel.is_some_and(|token| token.is_cancelled()) {
            return Err(AppError::cancelled());
        }
        let mut data = vec![0; BLOCK];
        let mut len = 0;
        while len < BLOCK {
            let n = file.read(&mut data[len..])?;
            if n == 0 {
                break;
            }
            len += n;
        }
        if len == 0 {
            break;
        }
        data.truncate(len);
        full.update(&data);
        budget.push(&mut hashes, Sha256::digest(&data).into())?;
        if let Some((end, _)) = &footer_start {
            let n = end.saturating_sub(position).min(len as u64) as usize;
            prefix.update(&data[..n]);
        }
        position += len as u64;
    }
    if position != bytes {
        return Err(AppError::conflict("SQL 输入在预检期间发生变化"));
    }
    if let Some((_, expected)) = footer_start
        && format!("{:x}", base16ct::HexDisplay(&prefix.finalize())) != expected
    {
        return Err(AppError::invalid("SQL 完成标记摘要不匹配"));
    }
    file.seek(SeekFrom::Start(0))?;
    let declared = if datapier_format {
        declared_objects(file.try_clone()?, &hashes, cancel.cloned(), budget)?
    } else {
        Vec::new()
    };
    file.seek(SeekFrom::Start(0))?;
    let statements = scan(
        file,
        &hashes,
        ScanSettings {
            encoding,
            bom: bom_bytes,
            fixed: fixed_encoding,
            cancel: cancel.cloned(),
            statement: None,
        },
        budget,
        |_, _, _, _, _| Ok(()),
    )?;
    if datapier_format {
        validate_declared(&declared, &statements)?;
    }
    Ok(SqlIndex {
        statements,
        sha256: format!("{:x}", base16ct::HexDisplay(&full.finalize())),
        bytes,
        datapier_format,
        declared_objects: declared,
        input_encoding: encoding.name().to_owned(),
        bom_bytes,
        fixed_encoding,
        block_hashes: hashes,
    })
}

/// 读取已索引的结构语句；读取的每个块均与预检摘要比较。
pub fn read_ddl(path: impl AsRef<Path>, index: &SqlIndex, statement: usize) -> Result<String> {
    let item = index
        .statements
        .get(statement)
        .ok_or_else(|| AppError::invalid("SQL 语句索引越界"))?;
    if item.kind == SqlKind::Insert {
        return Err(AppError::invalid("INSERT 不能作为 DDL 读取"));
    }
    let mut file = File::open(path)?;
    if file.metadata()?.len() != index.bytes {
        return Err(AppError::conflict("SQL 输入大小已变化"));
    }
    let first = item.start as usize / BLOCK;
    let last = (item.end.saturating_sub(1) as usize) / BLOCK;
    let mut out = Vec::new();
    for block in first..=last {
        let offset = block as u64 * BLOCK as u64;
        file.seek(SeekFrom::Start(offset))?;
        let size = (index.bytes - offset).min(BLOCK as u64) as usize;
        let mut data = vec![0; size];
        file.read_exact(&mut data)?;
        let hash: [u8; 32] = Sha256::digest(&data).into();
        if index.block_hashes.get(block) != Some(&hash) {
            return Err(AppError::conflict("SQL 输入内容已变化"));
        }
        let from = item.start.saturating_sub(offset) as usize;
        let to = (item.end - offset).min(size as u64) as usize;
        out.extend_from_slice(&data[from..to]);
    }
    let encoding = encoding_for(&item.context.input_encoding)?;
    let text = encoding
        .decode_without_bom_handling_and_without_replacement(&out)
        .ok_or_else(|| err(item.start, item.line, "SQL 结构语句编码错误"))?;
    let body = text
        .strip_suffix(item.delimiter.as_str())
        .ok_or_else(|| AppError::conflict("SQL 分隔符已变化"))?;
    Ok(body.trim().to_owned())
}

/// 逐行读取指定表的 INSERT；回调收到行值和输入 tuple 字节偏移。
/// 回调结束后 reader 不保留该行。输入块在使用前重新校验。
pub fn read_inserts<F>(
    path: impl AsRef<Path>,
    index: &SqlIndex,
    table: &str,
    mut callback: F,
) -> Result<()>
where
    F: FnMut(Option<&[String]>, Vec<Value>, u64) -> Result<()>,
{
    let matches: Vec<&SqlStatement> = index
        .statements
        .iter()
        .filter(|s| {
            s.kind == SqlKind::Insert
                && s.object.as_deref().is_some_and(|name| {
                    name.eq_ignore_ascii_case(table)
                        || (!name.contains('.')
                            && s.context.database.as_ref().is_some_and(|db| {
                                format!("{db}.{name}").eq_ignore_ascii_case(table)
                            }))
                })
        })
        .collect();
    if !table.contains('.') {
        let mut databases: Vec<&str> = matches
            .iter()
            .filter_map(|s| s.context.database.as_deref())
            .collect();
        databases.sort_unstable();
        databases.dedup();
        if databases.len() > 1 {
            return Err(AppError::invalid("表名跨多个数据库，请指定完整库表名"));
        }
    }
    let starts: Vec<u64> = matches.iter().map(|s| s.start).collect();
    let file = File::open(path)?;
    if file.metadata()?.len() != index.bytes {
        return Err(AppError::conflict("SQL 输入大小已变化"));
    }
    scan(
        file,
        &index.block_hashes,
        ScanSettings {
            encoding: encoding_for(&index.input_encoding)?,
            bom: index.bom_bytes,
            fixed: index.fixed_encoding,
            cancel: None,
            statement: None,
        },
        &mut IndexBudget::new(),
        |_, columns, values, offset, start| {
            if starts.binary_search(&start).is_ok() {
                callback(columns, values, offset)?;
            }
            Ok(())
        },
    )?;
    Ok(())
}

/// 按预检语句序号读取行，供按创建上下文重排的恢复步骤使用。
pub fn read_insert_statement<F>(
    path: impl AsRef<Path>,
    index: &SqlIndex,
    statement: usize,
    callback: F,
) -> Result<()>
where
    F: FnMut(Option<&[String]>, Vec<Value>, u64) -> Result<()>,
{
    read_insert_inner(
        path.as_ref(),
        index,
        statement,
        None,
        &mut IndexBudget::new(),
        callback,
    )
}

/// 与 read_insert_statement 相同，并在读取输入块时检查取消。
pub fn read_insert_statement_with_cancel<F>(
    path: impl AsRef<Path>,
    index: &SqlIndex,
    statement: usize,
    cancel: &tokio_util::sync::CancellationToken,
    callback: F,
) -> Result<()>
where
    F: FnMut(Option<&[String]>, Vec<Value>, u64) -> Result<()>,
{
    read_insert_inner(
        path.as_ref(),
        index,
        statement,
        Some(cancel),
        &mut IndexBudget::new(),
        callback,
    )
}

pub(crate) fn read_insert_statement_with_budget<F>(
    path: impl AsRef<Path>,
    index: &SqlIndex,
    statement: usize,
    cancel: &tokio_util::sync::CancellationToken,
    budget: &mut IndexBudget,
    callback: F,
) -> Result<()>
where
    F: FnMut(Option<&[String]>, Vec<Value>, u64) -> Result<()>,
{
    read_insert_inner(
        path.as_ref(),
        index,
        statement,
        Some(cancel),
        budget,
        callback,
    )
}

fn read_insert_inner<F>(
    path: &Path,
    index: &SqlIndex,
    statement: usize,
    cancel: Option<&tokio_util::sync::CancellationToken>,
    budget: &mut IndexBudget,
    mut callback: F,
) -> Result<()>
where
    F: FnMut(Option<&[String]>, Vec<Value>, u64) -> Result<()>,
{
    let item = index
        .statements
        .get(statement)
        .ok_or_else(|| AppError::invalid("SQL 语句索引越界"))?;
    if item.kind != SqlKind::Insert {
        return Err(AppError::invalid("该语句不是 INSERT"));
    }
    if item.start >= item.end || item.end > index.bytes {
        return Err(AppError::invalid("SQL 语句范围无效"));
    }
    let file = File::open(path)?;
    if file.metadata()?.len() != index.bytes {
        return Err(AppError::conflict("SQL 输入大小已变化"));
    }
    scan(
        file,
        &index.block_hashes,
        ScanSettings {
            encoding: encoding_for(&index.input_encoding)?,
            bom: 0,
            fixed: index.fixed_encoding,
            cancel: cancel.cloned(),
            statement: Some(item),
        },
        budget,
        |_, columns, values, offset, start| {
            if cancel.is_some_and(|token| token.is_cancelled()) {
                return Err(AppError::cancelled());
            }
            if start == item.start {
                callback(columns, values, offset)?;
            }
            Ok(())
        },
    )?;
    Ok(())
}

#[cfg(test)]
mod tuple_reservation_tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn cumulative_tuple_reservation_fails_before_row_decode() {
        let path = std::env::temp_dir().join(format!(
            "datapier-tuple-reserve-{}.sql",
            uuid::Uuid::new_v4()
        ));
        let value = "a".repeat(60);
        std::fs::write(&path, format!("INSERT INTO t VALUES ('{value}');")).unwrap();
        let single_index = inspect(&path).unwrap();
        let single_statement = single_index
            .statements
            .iter()
            .position(|item| item.kind == SqlKind::Insert)
            .unwrap();
        let mut single_budget = IndexBudget::new();
        single_budget.set_tuple_reserver(Box::new(|bytes| {
            if bytes > 1024 {
                return Err(AppError::new("resource_limit", "tuple reservation refused"));
            }
            Ok(())
        }));
        let mut single_rows = 0;
        read_insert_statement_with_budget(
            &path,
            &single_index,
            single_statement,
            &tokio_util::sync::CancellationToken::new(),
            &mut single_budget,
            |_, _, _| {
                single_rows += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(single_rows, 1);
        std::fs::write(
            &path,
            format!("INSERT INTO t VALUES ('{value}','{value}');"),
        )
        .unwrap();
        let index = inspect(&path).unwrap();
        let statement = index
            .statements
            .iter()
            .position(|item| item.kind == SqlKind::Insert)
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let mut budget = IndexBudget::new();
        budget.set_tuple_reserver(Box::new(move |bytes| {
            observed.fetch_add(1, Ordering::Relaxed);
            if bytes > 1024 {
                return Err(AppError::new("resource_limit", "tuple reservation refused"));
            }
            Ok(())
        }));
        let mut rows = 0;
        let error = read_insert_statement_with_budget(
            &path,
            &index,
            statement,
            &tokio_util::sync::CancellationToken::new(),
            &mut budget,
            |_, _, _| {
                rows += 1;
                Ok(())
            },
        )
        .unwrap_err();
        assert_eq!(error.code, "resource_limit");
        assert_eq!(error.message, "tuple reservation refused");
        assert_eq!(rows, 0);
        assert!(calls.load(Ordering::Relaxed) > 1);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reservation_supports_maximum_bit_literal_estimate() {
        let estimate = (LIMIT * 8 + 128 + 2) * 8
            + 4 * (std::mem::size_of::<(u64, u64)>() + std::mem::size_of::<Value>());
        let mut default_budget = IndexBudget::new();
        assert!(default_budget.reserve_tuple(estimate).is_ok());
        let maximum = Arc::new(AtomicUsize::new(0));
        let observed = maximum.clone();
        let mut budget = IndexBudget::new();
        budget.set_tuple_reserver(Box::new(move |bytes| {
            observed.store(bytes, Ordering::Relaxed);
            Ok(())
        }));
        assert!(budget.reserve_tuple(estimate).is_ok());
        assert!(maximum.load(Ordering::Relaxed) >= estimate);
    }
}
