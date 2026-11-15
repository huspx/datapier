//! MySQL 逻辑备份的只读预检与有界字面量读取。
//!
//! 索引只包含输入偏移和结构元数据；数据行仅在回调期间存在于内存。

pub(crate) mod persist;
mod rewrite;
pub use rewrite::rewrite_ddl;

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

fn words(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if quote {
            if c == '`' {
                if chars.peek() == Some(&'`') {
                    word.push('`');
                    chars.next();
                } else {
                    quote = false;
                    out.push(std::mem::take(&mut word));
                }
            } else {
                word.push(c);
            }
        } else if c == '`' {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            quote = true;
        } else if c == '\'' || c == '"' {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            while let Some(next) = chars.next() {
                if next == '\\' {
                    chars.next();
                } else if next == c {
                    if chars.peek() == Some(&c) {
                        chars.next();
                    } else {
                        break;
                    }
                }
            }
        } else if c.is_alphanumeric() || c == '_' || c == '$' || c == '@' {
            word.push(c);
        } else {
            if !word.is_empty() {
                out.push(std::mem::take(&mut word));
            }
            if matches!(c, '.' | '(' | ')' | ',' | '=') {
                out.push(c.to_string());
            }
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}
fn is(w: Option<&String>, s: &str) -> bool {
    w.is_some_and(|v| v.eq_ignore_ascii_case(s))
}

/// mysqldump 的临时视图只投影 `1 AS 列名`，且必须在同名最终定义之前。
fn constant_view_stub(words: &[String]) -> bool {
    if !is(words.first(), "CREATE") || !is(words.get(1), "VIEW") {
        return false;
    }
    let mut i = if is(words.get(3), ".") { 5 } else { 3 };
    if !is(words.get(i), "AS") || !is(words.get(i + 1), "SELECT") {
        return false;
    }
    i += 2;
    loop {
        if !is(words.get(i), "1") || !is(words.get(i + 1), "AS") || words.get(i + 2).is_none() {
            return false;
        }
        i += 3;
        if i == words.len() {
            return true;
        }
        if !is(words.get(i), ",") {
            return false;
        }
        i += 1;
    }
}
fn object_name(w: &[String], mut i: usize) -> Option<(String, usize)> {
    if is(w.get(i), "IF") {
        i += 1;
        if is(w.get(i), "NOT") {
            i += 1;
        }
        if is(w.get(i), "EXISTS") {
            i += 1;
        }
    }
    let first = w.get(i)?.clone();
    if matches!(first.as_str(), "(" | ")" | "," | "=") {
        return None;
    }
    i += 1;
    if w.get(i).is_some_and(|v| v == ".") {
        let second = w.get(i + 1)?;
        return Some((format!("{first}.{second}"), i + 2));
    }
    Some((first, i))
}

fn name_parts(
    tokens: &[String],
    mut i: usize,
    default_db: Option<&str>,
) -> (Option<String>, Option<String>) {
    if is(tokens.get(i), "IF") {
        i += 1;
        if is(tokens.get(i), "NOT") {
            i += 1;
        }
        if is(tokens.get(i), "EXISTS") {
            i += 1;
        }
    }
    let Some(first) = tokens.get(i) else {
        return (None, None);
    };
    if tokens.get(i + 1).is_some_and(|x| x == ".") {
        (Some(first.clone()), tokens.get(i + 2).cloned())
    } else {
        (default_db.map(str::to_owned), Some(first.clone()))
    }
}

fn ddl_identity(
    sql: &str,
    kind: &SqlKind,
    default_db: Option<&str>,
) -> (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let tokens = words(sql);
    let keyword = match kind {
        SqlKind::Database => "DATABASE",
        SqlKind::Table => "TABLE",
        SqlKind::View => "VIEW",
        SqlKind::Trigger => "TRIGGER",
        SqlKind::Procedure => "PROCEDURE",
        SqlKind::Function => "FUNCTION",
        SqlKind::Event => "EVENT",
        SqlKind::Index => "INDEX",
        _ => "",
    };
    let index = tokens.iter().position(|x| x.eq_ignore_ascii_case(keyword));
    let (db, name) = index.map_or((None, None), |i| name_parts(&tokens, i + 1, default_db));
    let upper = sql.to_ascii_uppercase();
    let object_at = if keyword.is_empty() {
        None
    } else {
        upper.find(keyword)
    };
    let definer = object_at.and_then(|end| {
        let prefix = &sql[..end];
        let up = &upper[..end];
        let pos = up.find("DEFINER")?;
        let after = &prefix[pos + 7..];
        let equals = after.find('=')?;
        let mut value = after[equals + 1..].trim();
        if let Some(i) = value.to_ascii_uppercase().find(" SQL SECURITY") {
            value = value[..i].trim();
        }
        if value.is_empty() {
            None
        } else {
            Some(value.to_owned())
        }
    });
    let security = tokens
        .windows(3)
        .find(|w| w[0].eq_ignore_ascii_case("SQL") && w[1].eq_ignore_ascii_case("SECURITY"))
        .map(|w| w[2].to_ascii_uppercase());
    (db, name, definer, security)
}

fn static_references(
    sql: &str,
    kind: &SqlKind,
    context: &SqlContext,
    budget: &mut IndexBudget,
) -> Result<(Vec<String>, bool)> {
    let tokens = rewrite::rewrite_tokens(sql, context)?;
    let commas = rewrite::from_commas(sql, &tokens);
    let keyword = |index: usize, word: &str| rewrite::keyword_is(sql, tokens.get(index), word);
    let mut refs: Vec<String> = Vec::new();
    let mut unresolved = false;
    let has_table_references = matches!(
        kind,
        SqlKind::View | SqlKind::Trigger | SqlKind::Procedure | SqlKind::Function | SqlKind::Event
    );
    let trigger_on = if *kind == SqlKind::Trigger {
        (0..tokens.len()).find(|&i| keyword(i, "ON"))
    } else {
        None
    };
    if has_table_references
        && commas.iter().any(|comma| *comma)
        && (0..tokens.len()).any(|i| keyword(i, "DELETE"))
    {
        unresolved = true;
    }
    for (i, &comma) in commas.iter().enumerate() {
        if keyword(i, "WITH")
            && tokens.get(i + 1).is_some_and(|next| next.identifier)
            && !["CHECK", "LOCAL", "CASCADED", "ROLLUP"]
                .iter()
                .any(|word| keyword(i + 1, word))
        {
            unresolved = true;
        }
        let reference = keyword(i, "REFERENCES")
            || (has_table_references
                && (comma
                    || ["FROM", "JOIN", "UPDATE", "INTO", "CALL"]
                        .iter()
                        .any(|word| keyword(i, word)))
                && !(keyword(i, "UPDATE")
                    && i > 0
                    && (keyword(i - 1, "BEFORE") || keyword(i - 1, "AFTER"))))
            || trigger_on == Some(i);
        if !reference {
            continue;
        }
        let mut j = i + 1;
        if keyword(j, "IF") {
            j += 1;
            if keyword(j, "NOT") {
                j += 1;
            }
            if keyword(j, "EXISTS") {
                j += 1;
            }
        }
        let Some(first) = tokens.get(j) else {
            unresolved = true;
            continue;
        };
        if !first.identifier {
            if first.text != "(" || !keyword(j + 1, "SELECT") {
                unresolved = true;
            }
            continue;
        }
        let qualified = tokens
            .get(j + 1)
            .is_some_and(|token| token.text == "." && !token.identifier);
        if !qualified && keyword(j, "DUAL") {
            continue;
        }
        let name = if qualified {
            match tokens.get(j + 2).filter(|token| token.identifier) {
                Some(second) => format!("{}.{}", first.text, second.text),
                None => {
                    unresolved = true;
                    continue;
                }
            }
        } else {
            first.text.clone()
        };
        if !refs.iter().any(|known| known == &name) {
            budget.charge(name.capacity())?;
            budget.push(&mut refs, name)?;
        }
    }
    Ok((refs, unresolved))
}

// 参数、返回类型和局部声明中的 INT(11) 等是类型，不是待映射函数。
fn routine_type_positions(words: &[String], kind: SqlKind) -> Vec<usize> {
    fn type_at(words: &[String], mut position: usize) -> Option<usize> {
        if is(words.get(position), "NATIONAL") {
            position += 1;
        }
        if (is(words.get(position), "DOUBLE") && is(words.get(position + 1), "PRECISION"))
            || (is(words.get(position), "CHARACTER") && is(words.get(position + 1), "VARYING"))
        {
            position += 1;
        }
        const TYPES: &[&str] = &[
            "BIT",
            "TINYINT",
            "SMALLINT",
            "MEDIUMINT",
            "INT",
            "INTEGER",
            "BIGINT",
            "DECIMAL",
            "NUMERIC",
            "FLOAT",
            "DOUBLE",
            "PRECISION",
            "REAL",
            "CHAR",
            "CHARACTER",
            "VARYING",
            "VARCHAR",
            "BINARY",
            "VARBINARY",
            "TEXT",
            "BLOB",
            "ENUM",
            "SET",
            "TIME",
            "DATE",
            "DATETIME",
            "TIMESTAMP",
            "YEAR",
        ];
        words
            .get(position)
            .filter(|name| TYPES.iter().any(|kind| name.eq_ignore_ascii_case(kind)))
            .map(|_| position)
    }
    let mut positions = Vec::new();
    let mut body = 0;
    if matches!(kind, SqlKind::Function | SqlKind::Procedure)
        && let Some(keyword) = words.iter().position(|word| {
            word.eq_ignore_ascii_case(if kind == SqlKind::Function {
                "FUNCTION"
            } else {
                "PROCEDURE"
            })
        })
        && let Some(open) = (keyword + 1..words.len()).find(|&index| words[index] == "(")
    {
        let mut depth = 0usize;
        let mut parameter = true;
        for index in open + 1..words.len() {
            if depth == 0 && words[index] == ")" {
                body = index + 1;
                break;
            }
            if depth == 0 && parameter {
                let mut name = index;
                if ["IN", "OUT", "INOUT"]
                    .iter()
                    .any(|mode| is(words.get(name), mode))
                {
                    name += 1;
                }
                if let Some(position) = type_at(words, name + 1) {
                    positions.push(position);
                }
                parameter = false;
            }
            match words[index].as_str() {
                "(" => depth += 1,
                ")" => depth = depth.saturating_sub(1),
                "," if depth == 0 => parameter = true,
                _ => {}
            }
        }
    }
    for index in body..words.len() {
        if is(words.get(index), "RETURNS") {
            if let Some(position) = type_at(words, index + 1) {
                positions.push(position);
            }
        } else if is(words.get(index), "DECLARE")
            && words
                .get(index + 1)
                .is_some_and(|word| !matches!(word.as_str(), "(" | ")" | "," | "." | "="))
        {
            let mut name = index + 1;
            while words.get(name + 1).is_some_and(|word| word == ",") {
                name += 2;
            }
            if let Some(position) = type_at(words, name + 1) {
                positions.push(position);
            }
        }
    }
    positions.sort_unstable();
    positions.dedup();
    positions
}

fn function_calls(sql: &str, kind: SqlKind, budget: &mut IndexBudget) -> Result<Vec<String>> {
    if !matches!(
        kind,
        SqlKind::View | SqlKind::Trigger | SqlKind::Procedure | SqlKind::Function | SqlKind::Event
    ) {
        return Ok(Vec::new());
    }
    let w = words(sql);
    let mut result = Vec::new();
    let type_positions = routine_type_positions(&w, kind);
    const OBJECT_ROLES: &[&str] = &[
        "FUNCTION",
        "PROCEDURE",
        "TRIGGER",
        "VIEW",
        "INTO",
        "REFERENCES",
        "CALL",
    ];
    const BUILTINS: &[&str] = &[
        "ABS",
        "ACOS",
        "ADDDATE",
        "ADDTIME",
        "ASCII",
        "ASIN",
        "ATAN",
        "AVG",
        "BENCHMARK",
        "BIN",
        "BIT_COUNT",
        "BIT_LENGTH",
        "CAST",
        "CEIL",
        "CEILING",
        "CHAR",
        "CHAR_LENGTH",
        "CHARACTER_LENGTH",
        "COALESCE",
        "CONCAT",
        "CONCAT_WS",
        "CONNECTION_ID",
        "CONVERT",
        "COS",
        "COUNT",
        "CRC32",
        "CURDATE",
        "CURRENT_DATE",
        "CURRENT_TIME",
        "CURRENT_TIMESTAMP",
        "CURRENT_USER",
        "CURTIME",
        "DATE",
        "DATE_ADD",
        "DATE_FORMAT",
        "DATE_SUB",
        "DATEDIFF",
        "DAY",
        "DAYNAME",
        "DAYOFMONTH",
        "DAYOFWEEK",
        "DAYOFYEAR",
        "DECIMAL",
        "DEGREES",
        "ELT",
        "EXISTS",
        "EXP",
        "EXTRACT",
        "FIELD",
        "FIND_IN_SET",
        "FLOOR",
        "FORMAT",
        "FROM_BASE64",
        "FROM_UNIXTIME",
        "GREATEST",
        "GROUP_CONCAT",
        "HEX",
        "HOUR",
        "IF",
        "IFNULL",
        "IN",
        "INSERT",
        "INSTR",
        "ISNULL",
        "JSON_ARRAY",
        "JSON_EXTRACT",
        "JSON_OBJECT",
        "JSON_UNQUOTE",
        "LCASE",
        "LEAST",
        "LEFT",
        "LENGTH",
        "LN",
        "LOCATE",
        "LOG",
        "LOWER",
        "LPAD",
        "LTRIM",
        "MAKEDATE",
        "MAX",
        "MD5",
        "MICROSECOND",
        "MID",
        "MIN",
        "MINUTE",
        "MOD",
        "MONTH",
        "MONTHNAME",
        "NOW",
        "NULLIF",
        "OCTET_LENGTH",
        "ORD",
        "PI",
        "POSITION",
        "POW",
        "POWER",
        "QUARTER",
        "QUOTE",
        "RAND",
        "REPLACE",
        "REPEAT",
        "REVERSE",
        "RIGHT",
        "ROUND",
        "RPAD",
        "RTRIM",
        "SECOND",
        "SHA1",
        "SHA2",
        "SIGN",
        "SIN",
        "SPACE",
        "SQRT",
        "STR_TO_DATE",
        "STRCMP",
        "SUBDATE",
        "SUBSTR",
        "SUBSTRING",
        "SUBSTRING_INDEX",
        "SUM",
        "SYSDATE",
        "TAN",
        "TIME",
        "TIMEDIFF",
        "TIMESTAMP",
        "TIMESTAMPADD",
        "TIMESTAMPDIFF",
        "TO_BASE64",
        "TRIM",
        "TRUNCATE",
        "UCASE",
        "UNHEX",
        "UNIX_TIMESTAMP",
        "UPPER",
        "UTC_DATE",
        "UTC_TIME",
        "UTC_TIMESTAMP",
        "UUID",
        "VALUES",
        "WEEK",
        "YEAR",
    ];
    for i in 0..w.len().saturating_sub(1) {
        if w[i + 1] != "(" || matches!(w[i].as_str(), ")" | "(" | "," | "." | "=") {
            continue;
        }
        if type_positions.binary_search(&i).is_ok() {
            continue;
        }
        if i > 0 && OBJECT_ROLES.iter().any(|k| is(w.get(i - 1), k)) {
            continue;
        }
        let qualified = i >= 2 && w[i - 1] == ".";
        if qualified && i >= 3 && OBJECT_ROLES.iter().any(|k| is(w.get(i - 3), k)) {
            continue;
        }
        if !qualified && BUILTINS.iter().any(|name| w[i].eq_ignore_ascii_case(name)) {
            continue;
        }
        let name = if qualified {
            format!("{}.{}", w[i - 2], w[i])
        } else {
            w[i].clone()
        };
        if !result.contains(&name) {
            budget.charge(name.capacity())?;
            budget.push(&mut result, name)?;
        }
    }
    Ok(result)
}

fn parse_insert(
    header: &str,
    pos: u64,
    line: u64,
    budget: &mut IndexBudget,
) -> Result<(String, Option<Vec<String>>)> {
    let w = words(header);
    if !is(w.first(), "INSERT") || !is(w.get(1), "INTO") {
        return Err(err(pos, line, "只支持 INSERT INTO ... VALUES"));
    }
    let (name, mut i) = object_name(&w, 2).ok_or_else(|| err(pos, line, "缺少 INSERT 表名"))?;
    let mut columns = None;
    if w.get(i).is_some_and(|x| x == "(") {
        i += 1;
        let mut cols = Vec::new();
        loop {
            let col = w.get(i).ok_or_else(|| err(pos, line, "列清单未闭合"))?;
            if matches!(col.as_str(), ")" | "(" | "," | "." | "=") {
                return Err(err(pos, line, "非法 INSERT 列名"));
            }
            budget.charge(col.len())?;
            budget.push(&mut cols, col.clone())?;
            i += 1;
            match w.get(i).map(String::as_str) {
                Some(",") => i += 1,
                Some(")") => {
                    i += 1;
                    break;
                }
                _ => return Err(err(pos, line, "非法 INSERT 列清单")),
            }
        }
        columns = Some(cols);
    }
    if !is(w.get(i), "VALUES") || i + 1 != w.len() {
        return Err(err(pos, line, "只支持 INSERT INTO ... VALUES"));
    }
    Ok((name, columns))
}

fn create_columns(sql: &str, budget: &mut IndexBudget) -> Result<Option<Vec<String>>> {
    let before = budget.used;
    let result = create_columns_inner(sql, budget)?;
    if result.is_none() {
        budget.used = before;
    }
    Ok(result)
}

fn create_columns_inner(sql: &str, budget: &mut IndexBudget) -> Result<Option<Vec<String>>> {
    let w = words(sql);
    let Some(table) = w.iter().position(|x| x.eq_ignore_ascii_case("TABLE")) else {
        return Ok(None);
    };
    let Some((_, mut i)) = object_name(&w, table + 1) else {
        return Ok(None);
    };
    if w.get(i).map(String::as_str) != Some("(") {
        return Ok(None);
    }
    i += 1;
    let mut depth = 1usize;
    let mut start = i;
    let mut columns = Vec::new();
    while i < w.len() {
        match w[i].as_str() {
            "(" => depth += 1,
            ")" => {
                let Some(next_depth) = depth.checked_sub(1) else {
                    return Ok(None);
                };
                depth = next_depth;
                if depth == 0 {
                    if start < i && !is_constraint(&w[start..i]) {
                        budget.charge(w[start].len())?;
                        budget.push(&mut columns, w[start].clone())?;
                    }
                    return Ok(if columns.is_empty() {
                        None
                    } else {
                        Some(columns)
                    });
                }
            }
            "," if depth == 1 => {
                if start == i {
                    return Ok(None);
                }
                if !is_constraint(&w[start..i]) {
                    budget.charge(w[start].len())?;
                    budget.push(&mut columns, w[start].clone())?;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    Ok(None)
}
fn is_constraint(w: &[String]) -> bool {
    is(w.first(), "CONSTRAINT")
        || is(w.first(), "FOREIGN")
        || is(w.first(), "CHECK")
        || (is(w.first(), "PRIMARY") && is(w.get(1), "KEY"))
        || (is(w.first(), "UNIQUE")
            && (is(w.get(1), "KEY") || is(w.get(1), "INDEX") || w.get(1).is_some_and(|v| v == "(")))
        || ((is(w.first(), "KEY") || is(w.first(), "INDEX"))
            && (w.get(1).is_some_and(|v| v == "(") || w.get(2).is_some_and(|v| v == "(")))
        || ((is(w.first(), "FULLTEXT") || is(w.first(), "SPATIAL"))
            && (is(w.get(1), "KEY") || is(w.get(1), "INDEX")))
}

fn classify(
    sql: &str,
    context: &mut SqlContext,
    start: u64,
    line: u64,
) -> Result<(SqlKind, Option<String>, bool)> {
    sqlparser::tokenizer::Tokenizer::new(&sqlparser::dialect::MySqlDialect {}, sql)
        .tokenize()
        .map_err(|e| err(start, line, format!("SQL 词法错误: {e}")))?;
    let w = words(sql);
    let first = w
        .first()
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_default();
    match first.as_str() {
        "USE" => {
            let (name, end) = object_name(&w, 1).ok_or_else(|| err(start, line, "缺少数据库名"))?;
            if end != w.len() {
                return Err(err(start, line, "非法 USE"));
            }
            context.database = Some(name.clone());
            Ok((SqlKind::Context, Some(name), false))
        }
        "SET" => {
            let upper = sql.to_ascii_uppercase();
            if upper.contains("GTID")
                || upper.contains("SQL_LOG_BIN")
                || upper.contains("GLOBAL")
                || upper.contains("PERSIST")
            {
                return Err(err(start, line, "禁止修改实例或复制状态"));
            }
            if upper.contains('(') || upper.contains(')') || upper.contains(":=") {
                return Err(err(start, line, "SET 不允许表达式或函数"));
            }
            let allowed = [
                "CHARACTER_SET_CLIENT",
                "CHARACTER_SET_RESULTS",
                "CHARACTER_SET_CONNECTION",
                "COLLATION_CONNECTION",
                "SQL_MODE",
                "TIME_ZONE",
                "UNIQUE_CHECKS",
                "FOREIGN_KEY_CHECKS",
                "SQL_NOTES",
                "AUTOCOMMIT",
            ];
            for (i, token) in w.iter().enumerate().skip(1) {
                if w.get(i + 1).is_some_and(|x| x == "=") {
                    let name = token.trim_start_matches('@').to_ascii_uppercase();
                    let saved = name
                        .strip_prefix("OLD_")
                        .is_some_and(|s| allowed.contains(&s))
                        || matches!(
                            name.as_str(),
                            "SAVED_CS_CLIENT"
                                | "SAVED_CS_RESULTS"
                                | "SAVED_COL_CONNECTION"
                                | "SAVED_SQL_MODE"
                                | "SAVED_TIME_ZONE"
                                | "SAVE_TIME_ZONE"
                        );
                    if !allowed.contains(&name.as_str()) && !(token.starts_with('@') && saved) {
                        return Err(err(start, line, format!("不支持的 SET 变量 {token}")));
                    }
                }
            }
            if !upper.starts_with("SET NAMES") && !w.iter().any(|x| x == "=") {
                return Err(err(start, line, "不支持的 SET 语句"));
            }
            let compact: String = upper.chars().filter(|c| !c.is_ascii_whitespace()).collect();
            if compact.contains("@SAVED_CS_CLIENT=@@CHARACTER_SET_CLIENT") {
                context.saved_character_set_client = Some(context.character_set_client.clone());
            }
            if compact.contains("@OLD_CHARACTER_SET_CLIENT=@@CHARACTER_SET_CLIENT") {
                context.old_character_set_client = Some(context.character_set_client.clone());
            }
            for (i, token) in w.iter().enumerate() {
                if token.eq_ignore_ascii_case("CHARACTER_SET_CLIENT")
                    && w.get(i + 1).is_some_and(|v| v == "=")
                    && let Some(value) = w.get(i + 2)
                {
                    if value.eq_ignore_ascii_case("@SAVED_CS_CLIENT")
                        || value.eq_ignore_ascii_case("@OLD_CHARACTER_SET_CLIENT")
                    {
                        let saved = if value.eq_ignore_ascii_case("@OLD_CHARACTER_SET_CLIENT") {
                            &context.old_character_set_client
                        } else {
                            &context.saved_character_set_client
                        };
                        context.character_set_client = saved
                            .clone()
                            .ok_or_else(|| err(start, line, "未保存 CHARACTER_SET_CLIENT"))?;
                    } else if !value.starts_with('@') {
                        encoding_for(value)?;
                        context.character_set_client = Some(value.clone());
                    }
                }
            }
            if compact.contains("@OLD_SQL_MODE=@@SQL_MODE") {
                context.saved_mode = Some((context.no_backslash_escapes, context.ansi_quotes));
            }
            if compact.contains("@SAVED_SQL_MODE=@@SQL_MODE") {
                context.saved_object_mode =
                    Some((context.no_backslash_escapes, context.ansi_quotes));
            }
            if compact.contains("SQL_MODE=@OLD_SQL_MODE")
                || compact.contains("SQL_MODE=@SAVED_SQL_MODE")
            {
                // dump 全局保存与逐对象保存使用不同槽位，不能互相覆盖词法模式。
                let saved = if compact.contains("SQL_MODE=@OLD_SQL_MODE") {
                    context.saved_mode
                } else {
                    context.saved_object_mode
                };
                let (no_backslash, ansi) =
                    saved.ok_or_else(|| err(start, line, "未保存 SQL_MODE"))?;
                context.no_backslash_escapes = no_backslash;
                context.ansi_quotes = ansi;
            } else if compact.contains("SQL_MODE='") || compact.contains("SQL_MODE=\"") {
                context.no_backslash_escapes = upper.contains("NO_BACKSLASH_ESCAPES");
                context.ansi_quotes = upper.contains("ANSI_QUOTES");
            }
            if upper.contains("FOREIGN_KEY_CHECKS") || upper.contains("UNIQUE_CHECKS") {
                return Ok((SqlKind::Wrapper, None, false));
            }
            context.settings.push(sql.to_owned());
            Ok((SqlKind::Context, None, false))
        }
        "LOCK" if is(w.get(1), "TABLES") => Ok((SqlKind::Wrapper, None, false)),
        "UNLOCK" if is(w.get(1), "TABLES") => Ok((SqlKind::Wrapper, None, false)),
        "CREATE" | "ALTER" | "DROP" => {
            let mut found = None;
            for (i, token) in w.iter().enumerate().skip(1) {
                let kind = match token.to_ascii_uppercase().as_str() {
                    "DATABASE" | "SCHEMA" => Some(SqlKind::Database),
                    "TABLE" => Some(SqlKind::Table),
                    "VIEW" => Some(SqlKind::View),
                    "TRIGGER" => Some(SqlKind::Trigger),
                    "PROCEDURE" => Some(SqlKind::Procedure),
                    "FUNCTION" => Some(SqlKind::Function),
                    "EVENT" => Some(SqlKind::Event),
                    "INDEX" => Some(SqlKind::Index),
                    _ => None,
                };
                if let Some(kind) = kind {
                    found = Some((i, kind));
                    break;
                }
            }
            let (i, kind) = found.ok_or_else(|| err(start, line, "不支持的 DDL 对象"))?;
            if w.get(1).is_some_and(|s| {
                ["USER", "ROLE", "SERVER", "TABLESPACE", "LOGFILE"]
                    .iter()
                    .any(|x| s.eq_ignore_ascii_case(x))
            }) {
                return Err(err(start, line, "禁止实例或账号 DDL"));
            }
            if (first == "ALTER" || first == "DROP") && i != 1 {
                return Err(err(start, line, "不支持的 DDL 前缀"));
            }
            if first == "CREATE" && i > 1 {
                let allowed = [
                    "OR",
                    "ALGORITHM",
                    "DEFINER",
                    "SQL",
                    "TEMPORARY",
                    "UNIQUE",
                    "FULLTEXT",
                    "SPATIAL",
                ];
                if !allowed.iter().any(|v| is(w.get(1), v)) {
                    return Err(err(start, line, "不支持的 CREATE 前缀"));
                }
                if is(w.get(1), "OR")
                    && (!is(w.get(2), "REPLACE")
                        || (i > 3 && !allowed.iter().any(|v| is(w.get(3), v))))
                {
                    return Err(err(start, line, "不支持的 CREATE OR REPLACE"));
                }
            }
            let (name, end) =
                object_name(&w, i + 1).ok_or_else(|| err(start, line, "缺少 DDL 对象名"))?;
            if matches!(kind, SqlKind::Table | SqlKind::Database | SqlKind::Index)
                && w[end..].iter().any(|token| {
                    [
                        "OUTFILE",
                        "DUMPFILE",
                        "TABLESPACE",
                        "DISCARD",
                        "IMPORT",
                        "EXCHANGE",
                    ]
                    .iter()
                    .any(|x| token.eq_ignore_ascii_case(x))
                })
            {
                return Err(err(start, line, "不支持具有外部副作用的结构语句"));
            }
            if first == "CREATE"
                && kind == SqlKind::Table
                && w[end..]
                    .iter()
                    .any(|token| token.eq_ignore_ascii_case("SELECT"))
            {
                return Err(err(start, line, "不支持 CREATE TABLE AS SELECT"));
            }
            if kind == SqlKind::Table
                && w[end..]
                    .windows(2)
                    .any(|pair| is(pair.first(), "DATA") && is(pair.get(1), "DIRECTORY"))
            {
                return Err(err(start, line, "不支持表数据目录"));
            }
            if first == "ALTER" && kind == SqlKind::Database {
                let mut j = end;
                let mut clauses = 0;
                while j < w.len() {
                    if is(w.get(j), "DEFAULT") {
                        j += 1;
                    }
                    if is(w.get(j), "CHARACTER") && is(w.get(j + 1), "SET") {
                        j += 2;
                    } else if is(w.get(j), "CHARSET") || is(w.get(j), "COLLATE") {
                        j += 1;
                    } else {
                        return Err(err(start, line, "不支持的 ALTER DATABASE 子句"));
                    }
                    if w.get(j).is_some_and(|x| x == "=") {
                        j += 1;
                    }
                    let value = w
                        .get(j)
                        .ok_or_else(|| err(start, line, "缺少字符集或排序规则"))?;
                    if !value
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_')
                    {
                        return Err(err(start, line, "非法字符集或排序规则"));
                    }
                    j += 1;
                    clauses += 1;
                }
                if clauses == 0 {
                    return Err(err(start, line, "不支持的 ALTER DATABASE"));
                }
            }
            if first == "ALTER"
                && kind == SqlKind::Table
                && (is(w.get(end), "DISABLE") || is(w.get(end), "ENABLE"))
            {
                if !is(w.get(end + 1), "KEYS") || end + 2 != w.len() {
                    return Err(err(start, line, "非法 KEYS 包装"));
                }
                return Ok((SqlKind::Wrapper, Some(name), false));
            }
            let unresolved = w.iter().any(|token| {
                ["PREPARE", "EXECUTE", "DEALLOCATE", "IMMEDIATE"]
                    .iter()
                    .any(|x| token.eq_ignore_ascii_case(x))
            });
            Ok((
                if first == "DROP" { SqlKind::Drop } else { kind },
                Some(name),
                unresolved,
            ))
        }
        _ => Err(err(start, line, format!("不支持的 SQL 语句 {first}"))),
    }
}

fn decode_hex(s: &str, offset: u64, line: u64) -> Result<Vec<u8>> {
    if s.len() > LIMIT * 2 {
        return Err(err(offset, line, "单值超过 16 MiB"));
    }
    if !s.len().is_multiple_of(2) || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(err(offset, line, "非法十六进制字面量"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| err(offset, line, "非法十六进制字面量"))
        })
        .collect()
}

fn decode_hex_number(s: &str, offset: u64, line: u64) -> Result<Vec<u8>> {
    if s.is_empty() {
        return Err(err(offset, line, "空十六进制字面量"));
    }
    if s.len().is_multiple_of(2) {
        decode_hex(s, offset, line)
    } else {
        decode_hex(&format!("0{s}"), offset, line)
    }
}

fn decode_bits(bits: &str, offset: u64, line: u64) -> Result<Value> {
    if bits.is_empty() || bits.len() > LIMIT * 8 || !bits.bytes().all(|b| b == b'0' || b == b'1') {
        return Err(err(offset, line, "非法或超限 bit 字面量"));
    }
    let mut bytes = vec![0u8; bits.len().div_ceil(8)];
    let padding = bytes.len() * 8 - bits.len();
    for (i, b) in bits.bytes().enumerate() {
        if b == b'1' {
            bytes[(i + padding) / 8] |= 1 << (7 - (i + padding) % 8);
        }
    }
    Ok(Value::Bytes(STANDARD.encode(bytes)))
}

fn character_bytes(out: Vec<u8>, charset: Option<&str>, offset: u64, line: u64) -> Result<Value> {
    if out.len() > LIMIT {
        return Err(err(offset, line, "单值超过 16 MiB"));
    }
    if charset.is_some_and(|c| c.eq_ignore_ascii_case("binary")) {
        return Ok(Value::Bytes(STANDARD.encode(out)));
    }
    if charset.is_some_and(|c| c.eq_ignore_ascii_case("ascii")) && !out.is_ascii() {
        return Err(err(offset, line, "ASCII 字面量包含非 ASCII 字节"));
    }
    let encoding = match charset {
        Some(name) => encoding_for(name)?,
        None => UTF_8,
    };
    let text = encoding
        .decode_without_bom_handling_and_without_replacement(&out)
        .ok_or_else(|| err(offset, line, "SQL 字面量字符编码错误"))?;
    Ok(Value::Text(text.into_owned()))
}

fn literal(s: &str, context: &SqlContext, offset: u64, line: u64) -> Result<Value> {
    let s = s.trim();
    if s.len() > LIMIT * 8 + 128 {
        return Err(err(offset, line, "单值超过 16 MiB"));
    }
    if s.eq_ignore_ascii_case("NULL") {
        return Ok(Value::Null);
    }
    if numeric(s) {
        if s.len() > LIMIT {
            return Err(err(offset, line, "单值超过 16 MiB"));
        }
        return Ok(match s.parse::<i128>() {
            Ok(number) if i64::try_from(number).is_ok() => Value::Integer(s.into()),
            Ok(number) if u64::try_from(number).is_ok() => Value::Unsigned(s.into()),
            _ => Value::Decimal(s.into()),
        });
    }
    if s.as_bytes()
        .get(..2)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"0x"))
    {
        return Ok(Value::Bytes(STANDARD.encode(decode_hex_number(
            &s[2..],
            offset,
            line,
        )?)));
    }
    if s.as_bytes()
        .get(..2)
        .is_some_and(|p| p.eq_ignore_ascii_case(b"0b"))
    {
        return decode_bits(&s[2..], offset, line);
    }
    if s.as_bytes()
        .first()
        .is_some_and(|b| b.eq_ignore_ascii_case(&b'n'))
        && s.as_bytes().get(1) == Some(&b'\'')
    {
        return literal(&s[1..], context, offset, line);
    }
    if s.len() >= 3
        && s.as_bytes()[1] == b'\''
        && s.ends_with('\'')
        && s.as_bytes()[0].eq_ignore_ascii_case(&b'x')
    {
        return Ok(Value::Bytes(STANDARD.encode(decode_hex(
            &s[2..s.len() - 1],
            offset,
            line,
        )?)));
    }
    if s.len() >= 3
        && s.as_bytes()[1] == b'\''
        && s.ends_with('\'')
        && s.as_bytes()[0].eq_ignore_ascii_case(&b'b')
    {
        return decode_bits(&s[2..s.len() - 1], offset, line);
    }
    let (charset, quoted) = if s.starts_with('_') {
        let end = s
            .as_bytes()
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, c)| !c.is_ascii_alphanumeric() && **c != b'_')
            .map(|(i, _)| i)
            .ok_or_else(|| err(offset, line, "非法字符集 introducer"))?;
        (Some(&s[1..end]), s[end..].trim_start())
    } else {
        (None, s)
    };
    if charset.is_some()
        && quoted.len() >= 3
        && quoted.as_bytes()[0].eq_ignore_ascii_case(&b'x')
        && quoted.as_bytes()[1] == b'\''
        && quoted.ends_with('\'')
    {
        return character_bytes(
            decode_hex(&quoted[2..quoted.len() - 1], offset, line)?,
            charset,
            offset,
            line,
        );
    }
    if charset.is_some()
        && quoted
            .as_bytes()
            .get(..2)
            .is_some_and(|p| p.eq_ignore_ascii_case(b"0x"))
    {
        return character_bytes(
            decode_hex_number(&quoted[2..], offset, line)?,
            charset,
            offset,
            line,
        );
    }
    let b = quoted.as_bytes();
    if b.len() < 2
        || !matches!(b[0], b'\'' | b'"')
        || b[b.len() - 1] != b[0]
        || (b[0] == b'"' && context.ansi_quotes)
    {
        return Err(err(offset, line, "INSERT 仅支持字面量，不支持表达式"));
    }
    let mut out = Vec::with_capacity(b.len() - 2);
    let mut i = 1;
    while i < b.len() - 1 {
        let c = b[i];
        i += 1;
        if c == b[0] {
            if i >= b.len() - 1 || b[i] != c {
                return Err(err(offset, line, "字符串引号错误"));
            }
            i += 1;
            out.push(c);
        } else if c == b'\\' && !context.no_backslash_escapes {
            if i >= b.len() - 1 {
                return Err(err(offset, line, "字符串转义不完整"));
            }
            let escaped = b[i];
            i += 1;
            out.push(match escaped {
                b'0' => 0,
                b'b' => 8,
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'Z' => 26,
                other => other,
            });
        } else {
            out.push(c);
        }
    }
    if charset.is_some() {
        let text =
            std::str::from_utf8(&out).map_err(|_| err(offset, line, "SQL 字符串解码错误"))?;
        let source = encoding_for(&context.input_encoding)?;
        let source = if source == UTF_16LE || source == UTF_16BE {
            if let Some(name) = context.character_set_client.as_deref() {
                encoding_for(name)?
            } else {
                match charset {
                    Some(name) if name.eq_ignore_ascii_case("binary") => UTF_8,
                    Some(name) => encoding_for(name)?,
                    None => UTF_8,
                }
            }
        } else {
            source
        };
        let (bytes, _, malformed) = source.encode(text);
        if malformed {
            return Err(err(offset, line, "SQL 字符串不能还原为输入字符集"));
        }
        return character_bytes(bytes.into_owned(), charset, offset, line);
    }
    character_bytes(out, None, offset, line)
}

fn tuple_values(
    tuple: &[u8],
    context: &SqlContext,
    offset: u64,
    positions: &[(u64, u64)],
    line: u64,
) -> Result<Vec<Value>> {
    let s = std::str::from_utf8(tuple).map_err(|_| err(offset, line, "输入不是有效 UTF-8"))?;
    if !s.starts_with('(') || !s.ends_with(')') {
        return Err(err(offset, line, "tuple 未闭合"));
    }
    let bytes = s.as_bytes();
    let mut start = 1;
    let mut i = 1;
    let mut quote = 0;
    let mut values = Vec::new();
    while i < bytes.len() - 1 {
        let b = bytes[i];
        if quote != 0 {
            if b == b'\\' && !context.no_backslash_escapes {
                i += 2;
                continue;
            }
            if b == quote {
                if i + 1 < bytes.len() - 1 && bytes[i + 1] == quote {
                    i += 2;
                    continue;
                }
                quote = 0;
            }
        } else if matches!(b, b'\'' | b'"') {
            quote = b;
        } else if b == b',' {
            let (pos, line) = positions
                .get(values.len())
                .copied()
                .unwrap_or((offset, line));
            values.push(literal(&s[start..i], context, pos, line)?);
            start = i + 1;
        } else if b == b'(' || b == b')' {
            let (pos, line) = positions
                .get(values.len())
                .copied()
                .unwrap_or((offset, line));
            return Err(err(pos, line, "INSERT 不允许表达式"));
        }
        i += 1;
    }
    if quote != 0 {
        return Err(err(offset, line, "字符串未闭合"));
    }
    let (pos, line) = positions
        .get(values.len())
        .copied()
        .unwrap_or((offset, line));
    values.push(literal(&s[start..bytes.len() - 1], context, pos, line)?);
    Ok(values)
}

struct InsertScan {
    name: String,
    source_database: Option<String>,
    source_name: Option<String>,
    columns: Option<Vec<String>>,
    first_tuple: u64,
    tuple_start: u64,
    tuple: Vec<u8>,
    value_positions: Vec<(u64, u64)>,
    value_pending: bool,
    value_start: usize,
    depth: usize,
    rows: usize,
    row_width: Option<usize>,
    need_tuple: bool,
}

struct ScanSettings<'a> {
    encoding: &'static Encoding,
    bom: usize,
    fixed: bool,
    cancel: Option<tokio_util::sync::CancellationToken>,
    statement: Option<&'a SqlStatement>,
}

fn scan<F>(
    file: File,
    hashes: &[[u8; 32]],
    settings: ScanSettings<'_>,
    budget: &mut IndexBudget,
    mut on_row: F,
) -> Result<Vec<SqlStatement>>
where
    F: FnMut(&str, Option<&[String]>, Vec<Value>, u64, u64) -> Result<()>,
{
    let ScanSettings {
        encoding,
        bom,
        fixed,
        cancel,
        statement,
    } = settings;
    let encoding = statement
        .map(|item| encoding_for(&item.context.input_encoding))
        .transpose()?
        .unwrap_or(encoding);
    let mut input = Input::new(
        file,
        hashes,
        encoding,
        if statement.is_some() { 0 } else { bom },
        cancel,
    )?;
    if let Some(item) = statement {
        input.seek_statement(item.start, item.line)?;
    }
    let end = statement.map_or(u64::MAX, |item| item.end);
    let mut statements = Vec::new();
    let mut context = statement
        .map(|item| item.context.clone())
        .unwrap_or_else(|| SqlContext {
            input_encoding: encoding.name().to_owned(),
            ..Default::default()
        });
    let mut context_bytes = context_heap(&context);
    budget.charge(context_bytes)?;
    let mut buf = Vec::<u8>::new();
    let mut delimiter = statement
        .map(|item| item.delimiter.as_bytes().to_vec())
        .unwrap_or_else(|| b";".to_vec());
    let mut start = 0u64;
    let mut line = 1u64;
    let mut active = false;
    let mut quote = 0u8;
    let mut exec_comment = false;
    let mut version = None;
    let mut exec_start: Option<(u64, Option<u32>)> = None;
    let mut version_constraints = Vec::new();
    let mut insert: Option<InsertScan> = None;
    while input.pos < end
        && let Some(b) = input.next()?
    {
        let at = input.byte_start;
        if !active && !b.is_ascii_whitespace() {
            active = true;
            start = at;
            line = input.line;
        }
        if let Some(ins) = &insert {
            // 预留两字节用于转义读取，并覆盖 Vec 扩容及字符集/值解码的临时副本。
            let estimate = ins
                .tuple
                .len()
                .saturating_add(2)
                .saturating_mul(8)
                .saturating_add(ins.value_positions.len().saturating_add(1).saturating_mul(
                    2 * (std::mem::size_of::<(u64, u64)>() + std::mem::size_of::<Value>()),
                ));
            budget.reserve_tuple(estimate)?;
        }
        if quote == 0
            && !b.is_ascii_whitespace()
            && let Some(ins) = insert.as_mut()
            && ins.depth > 0
            && ins.value_pending
        {
            if let Some(pos) = ins.value_positions.last_mut() {
                *pos = (at, input.line);
            }
            ins.value_pending = false;
        }
        if quote != 0 {
            if let Some(ins) = insert.as_mut() {
                ins.tuple.push(b);
            } else {
                buf.push(b);
            }
            if b == b'\\' && quote != b'`' && !context.no_backslash_escapes {
                let next = input
                    .next()?
                    .ok_or_else(|| err(at, input.line, "转义未完成"))?;
                if let Some(ins) = insert.as_mut() {
                    ins.tuple.push(next);
                } else {
                    buf.push(next);
                }
            } else if b == quote {
                if input.peek()? == Some(quote) {
                    let next = input.next()?.unwrap();
                    if let Some(ins) = insert.as_mut() {
                        ins.tuple.push(next);
                    } else {
                        buf.push(next);
                    }
                } else {
                    quote = 0;
                }
            }
            if let Some(ins) = &insert {
                if ins.tuple.len() - ins.value_start > LIMIT * 8 + 128 {
                    return Err(err(at, line, "单值超过 16 MiB"));
                }
            } else if buf.len() > LIMIT {
                return Err(err(start, line, "原生 SQL 语句超过 16 MiB"));
            }
            continue;
        }
        if exec_comment && b == b'*' && input.peek()? == Some(b'/') {
            input.next()?;
            exec_comment = false;
            if let Some((start, minimum)) = exec_start.take() {
                budget.push(
                    &mut version_constraints,
                    VersionConstraint {
                        start,
                        end: input.pos,
                        minimum,
                    },
                )?;
            }
            continue;
        }
        if b == b'/' && input.peek()? == Some(b'*') {
            input.next()?;
            if input.peek()? == Some(b'!') {
                input.next()?;
                exec_comment = true;
                let mut digits = String::new();
                while input.peek()?.is_some_and(|c| c.is_ascii_digit()) {
                    digits.push(input.next()?.unwrap() as char);
                }
                let minimum = if digits.is_empty() {
                    None
                } else {
                    Some(digits.parse().map_err(|_| err(at, line, "版本注释错误"))?)
                };
                if minimum.is_some() {
                    version = minimum;
                }
                exec_start = Some((at, minimum));
            } else {
                let mut prev = 0;
                let mut closed = false;
                while let Some(c) = input.next()? {
                    if prev == b'*' && c == b'/' {
                        closed = true;
                        break;
                    }
                    prev = c;
                }
                if !closed {
                    return Err(err(at, line, "注释未闭合"));
                }
            }
            if insert.is_some() {
                return Err(err(at, line, "INSERT tuple 中不支持注释"));
            }
            if !exec_comment && buf.iter().all(u8::is_ascii_whitespace) {
                active = false;
                buf.clear();
            } else {
                buf.push(b' ');
            }
            continue;
        }
        if b == b'#' || (b == b'-' && input.peek()? == Some(b'-')) {
            if b == b'-' {
                input.next()?;
                if !input
                    .peek()?
                    .is_some_and(|x| x.is_ascii_whitespace() || x < 32)
                {
                    return Err(err(at, line, "非法减号或注释"));
                }
            }
            while let Some(c) = input.next()? {
                if c == b'\n' {
                    break;
                }
            }
            if insert.is_some() {
                return Err(err(at, line, "INSERT tuple 中不支持注释"));
            }
            if buf.iter().all(u8::is_ascii_whitespace) {
                active = false;
                buf.clear();
            } else {
                buf.push(b' ');
            }
            continue;
        }
        if matches!(b, b'\'' | b'"' | b'`') {
            if insert.as_ref().is_some_and(|ins| ins.depth == 0) {
                return Err(err(at, line, "INSERT tuple 后包含额外内容"));
            }
            quote = b;
            if let Some(ins) = insert.as_mut() {
                ins.tuple.push(b);
            } else {
                buf.push(b);
            }
            continue;
        }
        if insert.is_none()
            && b.is_ascii_whitespace()
            && buf.iter().all(u8::is_ascii)
            && String::from_utf8_lossy(&buf)
                .trim()
                .eq_ignore_ascii_case("DELIMITER")
        {
            let mut value = Vec::new();
            while let Some(c) = input.next()? {
                if c == b'\n' {
                    break;
                }
                value.push(c);
            }
            let text = std::str::from_utf8(&value)
                .map_err(|_| err(at, line, "非法 DELIMITER"))?
                .trim();
            if text.is_empty()
                || text.len() > 32
                || text.bytes().any(|c| c.is_ascii_whitespace() || c == b'\\')
            {
                return Err(err(at, line, "非法 DELIMITER"));
            }
            delimiter = text.as_bytes().to_vec();
            buf.clear();
            active = false;
            continue;
        }
        if insert.is_none() && b == b'(' {
            let head = std::str::from_utf8(&buf).map_err(|_| err(start, line, "输入不是 UTF-8"))?;
            let trimmed = head.trim_start();
            if trimmed
                .as_bytes()
                .get(..6)
                .is_some_and(|v| v.eq_ignore_ascii_case(b"INSERT"))
                && trimmed.trim_end().to_ascii_uppercase().ends_with("VALUES")
            {
                let (name, columns) = parse_insert(trimmed, start, line, budget)?;
                let (source_database, source_name) =
                    name_parts(&words(trimmed), 2, context.database.as_deref());
                budget.reserve_tuple(24usize.saturating_add(
                    4 * (std::mem::size_of::<(u64, u64)>() + std::mem::size_of::<Value>()),
                ))?;
                insert = Some(InsertScan {
                    name,
                    source_database,
                    source_name,
                    columns,
                    first_tuple: at,
                    tuple_start: at,
                    tuple: vec![b'('],
                    value_positions: vec![(input.pos, input.line)],
                    value_pending: true,
                    value_start: 1,
                    depth: 1,
                    rows: 0,
                    row_width: None,
                    need_tuple: false,
                });
                buf.clear();
                continue;
            }
        }
        if let Some(ins) = insert.as_mut() {
            if ins.depth > 0 {
                ins.tuple.push(b);
                if b == b',' && ins.depth == 1 {
                    ins.value_start = ins.tuple.len();
                    ins.value_positions.push((input.pos, input.line));
                    ins.value_pending = true;
                } else if ins.tuple.len() - ins.value_start > LIMIT * 8 + 128 {
                    return Err(err(at, line, "单值超过 16 MiB"));
                }
                if b == b'(' {
                    return Err(err(at, input.line, "INSERT 不允许表达式"));
                }
                if b == b')' {
                    ins.depth -= 1;
                    if ins.depth == 0 {
                        let row = tuple_values(
                            &ins.tuple,
                            &context,
                            ins.tuple_start,
                            &ins.value_positions,
                            line,
                        )?;
                        if ins.columns.as_ref().is_some_and(|c| c.len() != row.len()) {
                            return Err(err(at, line, "INSERT 列数与值数量不一致"));
                        }
                        if ins.row_width.is_some_and(|width| width != row.len()) {
                            return Err(err(at, line, "INSERT 各行值数量不一致"));
                        }
                        ins.row_width = Some(row.len());
                        on_row(
                            &ins.name,
                            ins.columns.as_deref(),
                            row,
                            ins.tuple_start,
                            start,
                        )?;
                        ins.rows += 1;
                        ins.tuple.clear();
                        ins.value_positions.clear();
                    }
                }
                continue;
            }
            if b == b'(' {
                if !ins.need_tuple {
                    return Err(err(at, line, "tuple 之间缺少逗号"));
                }
                ins.need_tuple = false;
                ins.depth = 1;
                ins.tuple_start = at;
                ins.tuple.push(b);
                ins.value_start = 1;
                ins.value_positions.push((input.pos, input.line));
                ins.value_pending = true;
                continue;
            }
            if b == b',' {
                if ins.need_tuple {
                    return Err(err(at, line, "重复逗号"));
                }
                ins.need_tuple = true;
                continue;
            }
        }
        buf.push(b);
        if buf.len() > LIMIT {
            return Err(err(start, line, "原生 SQL 语句超过 16 MiB"));
        }
        if !exec_comment && buf.ends_with(&delimiter) {
            buf.truncate(buf.len() - delimiter.len());
            if let Some(ins) = insert.take() {
                if ins.depth != 0
                    || ins.need_tuple
                    || ins.rows == 0
                    || !buf.iter().all(u8::is_ascii_whitespace)
                {
                    return Err(err(at, line, "INSERT 后包含不支持的内容或不完整 tuple"));
                }
                budget.charge(context_heap(&context))?;
                push_statement(
                    budget,
                    &mut statements,
                    SqlStatement {
                        start,
                        end: input.pos,
                        line,
                        kind: SqlKind::Insert,
                        is_create: false,
                        drop_kind: None,
                        object: Some(ins.name),
                        source_database: ins.source_database,
                        source_name: ins.source_name,
                        index_table_database: None,
                        index_table_name: None,
                        definer: None,
                        sql_security: None,
                        columns: ins.columns,
                        source_columns: None,
                        row_width: ins.row_width,
                        context: context.clone(),
                        version,
                        version_constraints: std::mem::take(&mut version_constraints),
                        unresolved_references: false,
                        placeholder_view: false,
                        tuple_start: Some(ins.first_tuple),
                        references: Vec::new(),
                        function_calls: Vec::new(),
                        database_context: None,
                        alters_database_defaults: false,
                        delimiter: String::from_utf8_lossy(&delimiter).into_owned(),
                    },
                )?;
            } else {
                let sql = std::str::from_utf8(&buf)
                    .map_err(|_| err(start, line, "输入不是有效 UTF-8"))?
                    .trim();
                if !sql.is_empty() {
                    budget.charge(context_heap(&context))?;
                    if sql
                        .get(..3)
                        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("SET"))
                    {
                        let settings_growth =
                            if context.settings.len() == context.settings.capacity() {
                                context
                                    .settings
                                    .capacity()
                                    .max(4)
                                    .saturating_mul(std::mem::size_of::<String>())
                            } else {
                                0
                            };
                        budget
                            .check(sql.len().saturating_mul(2).saturating_add(settings_growth))?;
                    }
                    let old = context.clone();
                    let (kind, object, mut unresolved_references) =
                        classify(sql, &mut context, start, line)?;
                    let new_context_bytes = context_heap(&context);
                    budget.replace(context_bytes, new_context_bytes)?;
                    context_bytes = new_context_bytes;
                    let alters_database_defaults = kind == SqlKind::Database
                        && sql
                            .trim_start()
                            .get(..5)
                            .is_some_and(|s| s.eq_ignore_ascii_case("ALTER"));
                    let (references, unresolved_static) =
                        static_references(sql, &kind, &old, budget)?;
                    unresolved_references |= unresolved_static;
                    let is_create = sql
                        .get(..6)
                        .is_some_and(|s| s.eq_ignore_ascii_case("CREATE"));
                    let drop_kind = if kind == SqlKind::Drop {
                        words(sql).iter().skip(1).find_map(|w| {
                            match w.to_ascii_uppercase().as_str() {
                                "TABLE" => Some(SqlKind::Table),
                                "VIEW" => Some(SqlKind::View),
                                "TRIGGER" => Some(SqlKind::Trigger),
                                "PROCEDURE" => Some(SqlKind::Procedure),
                                "FUNCTION" => Some(SqlKind::Function),
                                "EVENT" => Some(SqlKind::Event),
                                "DATABASE" | "SCHEMA" => Some(SqlKind::Database),
                                "INDEX" => Some(SqlKind::Index),
                                _ => None,
                            }
                        })
                    } else {
                        None
                    };
                    let identity_kind = drop_kind.unwrap_or(kind);
                    let (source_database, source_name, definer, sql_security) =
                        ddl_identity(sql, &identity_kind, old.database.as_deref());
                    let (index_table_database, index_table_name) =
                        if identity_kind == SqlKind::Index {
                            let tokens = words(sql);
                            let on = tokens
                                .iter()
                                .position(|t| t.eq_ignore_ascii_case("ON"))
                                .ok_or_else(|| err(start, line, "索引缺少 ON 表"))?;
                            let (db, name) = name_parts(&tokens, on + 1, old.database.as_deref());
                            if name.is_none() {
                                return Err(err(start, line, "索引缺少目标表"));
                            }
                            (db, name)
                        } else {
                            (None, None)
                        };
                    let source_columns = if is_create && kind == SqlKind::Table {
                        create_columns(sql, budget)?
                    } else {
                        None
                    };
                    let tokens = words(sql);
                    let function_calls = function_calls(sql, kind, budget)?;
                    push_statement(
                        budget,
                        &mut statements,
                        SqlStatement {
                            start,
                            end: input.pos,
                            line,
                            kind,
                            is_create,
                            drop_kind,
                            object,
                            source_database,
                            source_name,
                            index_table_database,
                            index_table_name,
                            definer,
                            sql_security,
                            columns: None,
                            source_columns,
                            row_width: None,
                            context: old,
                            version,
                            version_constraints: std::mem::take(&mut version_constraints),
                            unresolved_references,
                            placeholder_view: kind == SqlKind::View && constant_view_stub(&tokens),
                            tuple_start: None,
                            references,
                            function_calls,
                            database_context: None,
                            alters_database_defaults,
                            delimiter: String::from_utf8_lossy(&delimiter).into_owned(),
                        },
                    )?;
                    if is(tokens.first(), "SET") && is(tokens.get(1), "NAMES") {
                        let after_names = sql
                            .trim_start()
                            .get(3..)
                            .unwrap_or("")
                            .trim_start()
                            .get(5..)
                            .unwrap_or("")
                            .trim_start();
                        let name = after_names
                            .split(|c: char| c.is_ascii_whitespace() || c == ',')
                            .next()
                            .unwrap_or("")
                            .trim_matches(['\'', '"']);
                        if name.is_empty() {
                            return Err(err(start, line, "SET NAMES 缺少字符集"));
                        }
                        let next = encoding_for(name)?;
                        context.character_set_client = Some(name.to_owned());
                        if !fixed {
                            input.set_encoding(next)?;
                            context.input_encoding = next.name().to_owned();
                        }
                        let new_context_bytes = context_heap(&context);
                        budget.replace(context_bytes, new_context_bytes)?;
                        context_bytes = new_context_bytes;
                    }
                }
            }
            buf.clear();
            active = false;
            version = None;
            version_constraints.clear();
        }
    }
    if quote != 0 || exec_comment || insert.is_some() || !buf.iter().all(u8::is_ascii_whitespace) {
        return Err(err(input.pos, input.line, "SQL 末尾不完整或缺少分隔符"));
    }
    budget.used -= context_bytes;
    let before_views = budget.used;
    let mut views = Vec::new();
    for s in statements
        .iter()
        .filter(|s| s.kind == SqlKind::View && s.is_create)
    {
        let database = s.source_database.as_ref().or(s.context.database.as_ref());
        budget.charge(
            database
                .map_or(0, String::capacity)
                .saturating_add(s.source_name.as_ref().map_or(0, String::capacity)),
        )?;
        budget.push(
            &mut views,
            (database.cloned(), s.source_name.clone(), s.start),
        )?;
    }
    for s in &mut statements {
        let candidate = (s.kind == SqlKind::Table && s.is_create)
            || (s.kind == SqlKind::View && s.placeholder_view);
        let database = s.source_database.as_ref().or(s.context.database.as_ref());
        s.placeholder_view = candidate
            && views.iter().any(|(db, name, start)| {
                db.as_ref() == database && name == &s.source_name && *start > s.start
            });
    }
    drop(views);
    budget.used = before_views;
    let before_columns = budget.used;
    let mut table_columns = Vec::new();
    for s in statements
        .iter()
        .filter(|s| s.kind == SqlKind::Table && s.is_create && !s.placeholder_view)
    {
        let heap = s
            .source_database
            .as_ref()
            .map_or(0, String::capacity)
            .saturating_add(s.source_name.as_ref().map_or(0, String::capacity))
            .saturating_add(s.source_columns.as_ref().map_or(0, string_vec_heap));
        budget.charge(heap)?;
        budget.push(
            &mut table_columns,
            (
                s.source_database.clone(),
                s.source_name.clone(),
                s.source_columns.clone(),
            ),
        )?;
    }
    let mut temporary_columns = budget.used - before_columns;
    for s in &mut statements {
        if s.kind == SqlKind::Table {
            if let Some((_, _, cols)) = table_columns
                .iter_mut()
                .find(|(db, name, _)| db == &s.source_database && name == &s.source_name)
            {
                let added = if s.is_create {
                    s.source_columns.as_ref().map_or(0, string_vec_heap)
                } else {
                    0
                };
                budget.charge(added)?;
                temporary_columns += added;
                *cols = if s.is_create {
                    s.source_columns.clone()
                } else {
                    None
                };
            }
            continue;
        }
        if s.kind != SqlKind::Insert {
            continue;
        }
        let columns = s.columns.as_ref().or_else(|| {
            table_columns
                .iter()
                .find(|(db, name, _)| db == &s.source_database && name == &s.source_name)
                .and_then(|(_, _, cols)| cols.as_ref())
        });
        if let Some(columns) = columns {
            budget.charge(string_vec_heap(columns))?;
            s.source_columns = Some(columns.clone());
        }
        if s.source_columns
            .as_ref()
            .zip(s.row_width)
            .is_some_and(|(cols, width)| cols.len() != width)
        {
            return Err(err(s.start, s.line, "INSERT 值数量与输入列定义不一致"));
        }
    }
    drop(table_columns);
    budget.used -= temporary_columns;
    let before_functions = budget.used;
    let mut functions = Vec::new();
    for s in statements
        .iter()
        .filter(|s| s.kind == SqlKind::Function && s.is_create)
    {
        budget.charge(
            s.source_database
                .as_ref()
                .map_or(0, String::capacity)
                .saturating_add(s.source_name.as_ref().map_or(0, String::capacity)),
        )?;
        budget.push(
            &mut functions,
            (s.source_database.clone(), s.source_name.clone()),
        )?;
    }
    let temporary_functions = budget.used - before_functions;
    for s in &mut statements {
        for call in &s.function_calls {
            let (db, name) = match call.rsplit_once('.') {
                Some((db, name)) => (Some(db), name),
                None => (
                    s.source_database
                        .as_deref()
                        .or(s.context.database.as_deref()),
                    call.as_str(),
                ),
            };
            if functions.iter().any(|(source_db, source_name)| {
                source_db.as_deref() == db && source_name.as_deref() == Some(name)
            }) {
                if !s.references.iter().any(|r| r == call) {
                    budget.charge(call.len())?;
                    budget.push(&mut s.references, call.clone())?;
                }
            } else {
                s.unresolved_references = true;
            }
        }
    }
    drop(functions);
    budget.used -= temporary_functions;
    // 配对库默认值变更，防止依赖重排将存储对象与其创建上下文分开。
    let mut pending: Option<(usize, String, Vec<usize>)> = None;
    for i in 0..statements.len() {
        if statements[i].alters_database_defaults {
            budget.check(statements[i].object.as_ref().map_or(0, String::capacity))?;
            let database = statements[i].object.clone().ok_or_else(|| {
                err(
                    statements[i].start,
                    statements[i].line,
                    "ALTER DATABASE 缺少库名",
                )
            })?;
            budget.charge(database.capacity())?;
            if let Some((before, previous, objects)) = pending.take() {
                if !previous.eq_ignore_ascii_case(&database) || objects.is_empty() {
                    return Err(err(
                        statements[i].start,
                        statements[i].line,
                        "ALTER DATABASE 上下文无法配对",
                    ));
                }
                let temporary = previous.capacity().saturating_add(
                    objects
                        .capacity()
                        .saturating_mul(std::mem::size_of::<usize>()),
                );
                for object in objects {
                    budget.charge(database.len())?;
                    statements[object].database_context = Some(DatabaseContext {
                        database: database.clone(),
                        before_statement: before,
                        restore_statement: i,
                    });
                }
                budget.used -= temporary + database.capacity();
            } else {
                pending = Some((i, database, Vec::new()));
            }
        } else if matches!(
            statements[i].kind,
            SqlKind::Procedure | SqlKind::Function | SqlKind::Trigger | SqlKind::Event
        ) && let Some((_, database, objects)) = pending.as_mut()
        {
            let own_database = statements[i]
                .source_database
                .as_deref()
                .or(statements[i].context.database.as_deref());
            if own_database.is_some_and(|db| !db.eq_ignore_ascii_case(database)) {
                return Err(err(
                    statements[i].start,
                    statements[i].line,
                    "存储对象与数据库上下文不一致",
                ));
            }
            budget.push(objects, i)?;
        }
    }
    if let Some((i, _, _)) = pending {
        return Err(err(
            statements[i].start,
            statements[i].line,
            "ALTER DATABASE 缺少恢复配对",
        ));
    }
    Ok(statements)
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
        && format!("{:x}", prefix.finalize()) != expected
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
        sha256: format!("{:x}", full.finalize()),
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
