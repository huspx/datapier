use serde::{Deserialize, Serialize};
use std::ops::Range;

/// 文本编辑状态；Tern 的 cursor 和 anchor 均为 UTF-16 code unit 偏移。
/// undo 仅保存在当前进程，不随工作区序列化。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
    pub anchor: usize,
    #[serde(skip, default = "uuid::Uuid::new_v4")]
    identity: uuid::Uuid,
    #[serde(skip)]
    revision: u64,
    #[serde(skip)]
    undo: Vec<Snapshot>,
}

#[derive(Clone, Debug)]
struct Snapshot {
    text: String,
    cursor: usize,
    anchor: usize,
}

/// native editing 关闭时由 UI 处理的文本按键。
#[derive(Clone, Copy, Debug)]
pub enum Key<'a> {
    Text(&'a str),
    Backspace,
    Delete,
    Left,
    Right,
    Up,
    Down,
    Home,
    End,
    SelectAll,
    Undo,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new("")
    }
}

impl Editor {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.encode_utf16().count();
        Self {
            text,
            cursor,
            anchor: cursor,
            identity: uuid::Uuid::new_v4(),
            revision: 0,
            undo: Vec::new(),
        }
    }

    pub(crate) fn identity(&self) -> (uuid::Uuid, u64) {
        (self.identity, self.revision)
    }

    pub fn selection(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }

    #[cfg(test)]
    pub fn selected_text(&self) -> Option<&str> {
        let range = self.selection();
        if range.is_empty() {
            return None;
        }
        Some(
            &self.text
                [utf16_to_byte(&self.text, range.start)?..utf16_to_byte(&self.text, range.end)?],
        )
    }

    /// 更新选区；拒绝超出文本或落在代理对中间的位置，并保持原选区。
    pub fn set_selection(&mut self, anchor: usize, cursor: usize) -> bool {
        if utf16_to_byte(&self.text, anchor).is_none()
            || utf16_to_byte(&self.text, cursor).is_none()
        {
            return false;
        }
        self.anchor = anchor;
        self.cursor = cursor;
        true
    }

    fn save_undo(&mut self) {
        self.undo.push(Snapshot {
            text: self.text.clone(),
            cursor: self.cursor,
            anchor: self.anchor,
        });
    }

    pub fn undo(&mut self) -> bool {
        let Some(old) = self.undo.pop() else {
            return false;
        };
        self.text = old.text;
        self.revision = self.revision.wrapping_add(1);
        self.cursor = old.cursor;
        self.anchor = old.anchor;
        true
    }

    /// 按 UTF-16 范围应用原生编辑或 IME 提交；无效范围返回 false 且不修改文本。
    pub fn edit(&mut self, range: Range<usize>, replacement: &str) -> bool {
        if range.start > range.end {
            return false;
        }
        let Some(start) = utf16_to_byte(&self.text, range.start) else {
            return false;
        };
        let Some(end) = utf16_to_byte(&self.text, range.end) else {
            return false;
        };
        if start == end && replacement.is_empty() {
            return true;
        }
        self.save_undo();
        self.text.replace_range(start..end, replacement);
        self.revision = self.revision.wrapping_add(1);
        self.cursor = range.start + replacement.encode_utf16().count();
        self.anchor = self.cursor;
        true
    }

    /// 校验完整原生编辑后再修改，避免非法光标使正文与宿主状态分离。
    pub fn native_edit(&mut self, range: Range<usize>, replacement: &str, cursor: usize) -> bool {
        if range.start > range.end {
            return false;
        }
        let Some(start) = utf16_to_byte(&self.text, range.start) else {
            return false;
        };
        let Some(end) = utf16_to_byte(&self.text, range.end) else {
            return false;
        };
        let replacement_len = replacement.encode_utf16().count();
        let Some(replacement_end) = range.start.checked_add(replacement_len) else {
            return false;
        };
        let valid_cursor = if cursor < range.start {
            utf16_to_byte(&self.text[..start], cursor).is_some()
        } else if cursor <= replacement_end {
            utf16_to_byte(replacement, cursor - range.start).is_some()
        } else {
            utf16_to_byte(&self.text[end..], cursor - replacement_end).is_some()
        };
        if !valid_cursor || !self.edit(range, replacement) {
            return false;
        }
        self.set_selection(cursor, cursor)
    }

    /// 在当前选区处理按键；shift 保留移动前的 anchor，非法位置返回 false。
    pub fn key(&mut self, key: Key<'_>, shift: bool) -> bool {
        let Some(byte) = utf16_to_byte(&self.text, self.cursor) else {
            return false;
        };
        if utf16_to_byte(&self.text, self.anchor).is_none() {
            return false;
        }
        let selection = self.selection();
        match key {
            Key::Text(text) => self.edit(selection, text),
            Key::Undo => self.undo(),
            Key::SelectAll => {
                self.anchor = 0;
                self.cursor = self.text.encode_utf16().count();
                true
            }
            Key::Backspace | Key::Delete => {
                let range = if !selection.is_empty() {
                    selection
                } else if matches!(key, Key::Backspace) {
                    let Some(ch) = self.text[..byte].chars().next_back() else {
                        return true;
                    };
                    self.cursor - ch.len_utf16()..self.cursor
                } else {
                    let Some(ch) = self.text[byte..].chars().next() else {
                        return true;
                    };
                    self.cursor..self.cursor + ch.len_utf16()
                };
                self.edit(range, "")
            }
            Key::Left | Key::Right | Key::Up | Key::Down | Key::Home | Key::End => {
                let next = match key {
                    Key::Left if !shift && !selection.is_empty() => selection.start,
                    Key::Right if !shift && !selection.is_empty() => selection.end,
                    Key::Left => {
                        self.cursor
                            - self.text[..byte]
                                .chars()
                                .next_back()
                                .map_or(0, char::len_utf16)
                    }
                    Key::Right => {
                        self.cursor + self.text[byte..].chars().next().map_or(0, char::len_utf16)
                    }
                    Key::Up | Key::Down => {
                        let line_start = self.text[..byte].rfind('\n').map_or(0, |pos| pos + 1);
                        let column = self.text[line_start..byte].chars().count();
                        let target = if matches!(key, Key::Up) {
                            if line_start == 0 {
                                None
                            } else {
                                let end = line_start - 1;
                                let start = self.text[..end].rfind('\n').map_or(0, |pos| pos + 1);
                                Some(start..end)
                            }
                        } else {
                            self.text[byte..].find('\n').map(|offset| {
                                let start = byte + offset + 1;
                                let end = self.text[start..]
                                    .find('\n')
                                    .map_or(self.text.len(), |n| start + n);
                                start..end
                            })
                        };
                        target.map_or(self.cursor, |line| {
                            let text = &self.text[line.clone()];
                            let offset = text
                                .char_indices()
                                .nth(column)
                                .map_or(text.len(), |(pos, _)| pos);
                            byte_to_utf16(&self.text, line.start + offset).unwrap()
                        })
                    }
                    Key::Home => self.text[..byte]
                        .rsplit_once('\n')
                        .map_or(0, |(prefix, _)| prefix.encode_utf16().count() + 1),
                    Key::End => {
                        self.cursor
                            + self.text[byte..]
                                .split('\n')
                                .next()
                                .unwrap_or("")
                                .encode_utf16()
                                .count()
                    }
                    _ => unreachable!(),
                };
                self.cursor = next;
                if !shift {
                    self.anchor = next;
                }
                true
            }
        }
    }
}

/// 将 UTF-16 code unit 偏移转换为 UTF-8 字节边界；越界或位于代理对内部返回 None。
pub fn utf16_to_byte(text: &str, offset: usize) -> Option<usize> {
    let mut units = 0;
    for (byte, ch) in text.char_indices() {
        if units == offset {
            return Some(byte);
        }
        units += ch.len_utf16();
        if units > offset {
            return None;
        }
    }
    (units == offset).then_some(text.len())
}

/// 将 UTF-8 字节边界转换为 UTF-16 code unit 偏移；非法字节边界返回 None。
pub fn byte_to_utf16(text: &str, byte: usize) -> Option<usize> {
    text.get(..byte).map(|prefix| prefix.encode_utf16().count())
}

/// MySQL 引号和转义规则；必须取自实际执行查询的同一连接的 sql_mode。
#[derive(Clone, Copy, Debug, Default)]
pub struct MysqlMode {
    pub no_backslash_escapes: bool,
    pub ansi_quotes: bool,
}

/// 无状态查询只接受语句边界不依赖 sql_mode 的共同子集。
#[derive(Clone, Copy, Debug)]
pub enum SqlContext {
    Session(MysqlMode),
    Stateless,
}

/// SQL tokenizer 识别出的高亮类别。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HighlightKind {
    Keyword,
    String,
    Identifier,
    Comment,
    Number,
}

const KEYWORDS: &[&str] = &[
    "SELECT", "FROM", "WHERE", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "ON", "AS", "AND", "OR",
    "NOT", "NULL", "IS", "IN", "LIKE", "ORDER", "BY", "GROUP", "HAVING", "LIMIT", "OFFSET",
    "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE", "ALTER", "DROP", "TABLE",
    "DATABASE", "INDEX", "VIEW", "SHOW", "DESCRIBE", "EXPLAIN", "WITH", "UNION", "ALL", "DISTINCT",
    "CASE", "WHEN", "THEN", "ELSE", "END", "BEGIN", "COMMIT", "ROLLBACK",
];

#[derive(Clone, Debug)]
struct Token {
    start: usize,
    end: usize,
    kind: Option<HighlightKind>,
}

// 用字节定位 SQL 分隔符，但字符串和注释内部的分号不是语句边界。
// 对可执行注释、嵌套注释等无法可靠判断的情况直接报错，避免执行错误片段。
fn tokenize(sql: &str, mode: MysqlMode) -> Result<Vec<Token>, String> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }

        let kind = if b == b'#'
            || (b == b'-'
                && bytes.get(i + 1) == Some(&b'-')
                && bytes
                    .get(i + 2)
                    .is_some_and(|c| c.is_ascii_whitespace() || c.is_ascii_control()))
        {
            i = sql[i..].find('\n').map_or(bytes.len(), |n| i + n);
            let comment = &bytes[start..i];
            if comment.iter().enumerate().any(|(index, byte)| {
                *byte == b'\r' && (index + 1 != comment.len() || i == bytes.len())
            }) {
                return Err("SQL 行注释包含无法安全识别的回车".into());
            }
            Some(HighlightKind::Comment)
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            if matches!(bytes.get(i + 2), Some(b'!' | b'+'))
                || (matches!(bytes.get(i + 2), Some(b'M' | b'm'))
                    && bytes.get(i + 3) == Some(&b'!'))
            {
                return Err("无法安全定位含可执行或提示注释的 SQL".into());
            }
            let Some(end) = sql[i + 2..].find("*/") else {
                return Err("SQL 块注释未结束".into());
            };
            if sql[i + 2..i + 2 + end].contains("/*") {
                return Err("嵌套 SQL 注释无法安全定位".into());
            }
            i += end + 4;
            Some(HighlightKind::Comment)
        } else if matches!(b, b'\'' | b'"' | b'`') {
            let identifier = b == b'`' || (b == b'"' && mode.ansi_quotes);
            i += 1;
            let mut closed = false;
            while i < bytes.len() {
                if bytes[i] == b {
                    if bytes.get(i + 1) == Some(&b) {
                        i += 2;
                    } else {
                        i += 1;
                        closed = true;
                        break;
                    }
                } else if bytes[i] == b'\\' && !identifier && !mode.no_backslash_escapes {
                    i += 1;
                    if i < bytes.len() {
                        i += 1;
                    }
                } else {
                    i += 1;
                }
            }
            if !closed {
                return Err("SQL 引号未结束".into());
            }
            Some(if identifier {
                HighlightKind::Identifier
            } else {
                HighlightKind::String
            })
        } else if b.is_ascii_alphabetic() || b == b'_' || b == b'$' || b >= 128 {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || matches!(bytes[i], b'_' | b'$')
                    || bytes[i] >= 128)
            {
                i += 1;
            }
            let word = &sql[start..i];
            KEYWORDS
                .iter()
                .any(|keyword| word.eq_ignore_ascii_case(keyword))
                .then_some(HighlightKind::Keyword)
        } else if b.is_ascii_digit() {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric() || matches!(bytes[i], b'.' | b'_'))
            {
                i += 1;
            }
            Some(HighlightKind::Number)
        } else {
            i += 1;
            None
        };
        tokens.push(Token {
            start,
            end: i,
            kind,
        });
    }
    Ok(tokens)
}

/// 返回 UTF-16 光标所在 SQL 语句的 UTF-16 范围，不包含分隔符。
/// 不支持安全判断边界的脚本、模式变更或未闭合文本，返回错误。
pub fn current_statement(
    sql: &str,
    cursor: usize,
    mode: MysqlMode,
) -> Result<Range<usize>, String> {
    let byte = utf16_to_byte(sql, cursor).ok_or("光标不在有效的 UTF-16 字符边界")?;
    let tokens = tokenize(sql, mode)?;
    let words: Vec<&str> = tokens
        .iter()
        .filter(|t| {
            !matches!(
                t.kind,
                Some(HighlightKind::Comment | HighlightKind::String | HighlightKind::Identifier)
            ) && sql.as_bytes()[t.start].is_ascii_alphabetic()
        })
        .map(|t| &sql[t.start..t.end])
        .collect();
    if words
        .iter()
        .any(|w| w.eq_ignore_ascii_case("DELIMITER") || w.eq_ignore_ascii_case("SOURCE"))
        || sql.lines().any(|line| line.trim_start().starts_with("\\."))
    {
        return Err("不支持 MySQL 客户端脚本指令".into());
    }
    if tokens
        .iter()
        .any(|token| token.kind.is_none() && &sql[token.start..token.end] == "\\")
    {
        return Err("不支持 MySQL 客户端转义指令".into());
    }
    if (words.iter().any(|w| w.eq_ignore_ascii_case("CREATE"))
        && words.iter().any(|w| {
            ["PROCEDURE", "FUNCTION", "TRIGGER", "EVENT"]
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k))
        }))
        || words.iter().any(|w| w.eq_ignore_ascii_case("SQL_MODE"))
        || tokens.iter().any(|t| {
            t.kind == Some(HighlightKind::Identifier)
                && sql[t.start + 1..t.end - 1].eq_ignore_ascii_case("SQL_MODE")
        })
    {
        return Err("SQL 模式变化或存储程序体的语句边界无法安全定位".into());
    }
    let mut start = 0;
    let mut end = sql.len();
    for token in &tokens {
        if token.kind.is_none() && &sql[token.start..token.end] == ";" {
            if token.start < byte {
                start = token.end;
            } else {
                end = token.start;
                break;
            }
        }
    }
    let fragment = &sql[start..end];
    let left = fragment.len() - fragment.trim_start().len();
    let right = fragment.trim_end().len();
    if left >= right
        || !tokens.iter().any(|t| {
            t.start >= start + left
                && t.end <= start + right
                && t.kind != Some(HighlightKind::Comment)
        })
    {
        return Err("光标处没有可执行的 SQL 语句".into());
    }
    Ok(byte_to_utf16(sql, start + left).unwrap()..byte_to_utf16(sql, start + right).unwrap())
}

/// 一个可安全定位的语句，范围使用 UTF-16，行号从一开始。
#[derive(Clone, Debug)]
pub struct StatementRange {
    pub range: Range<usize>,
    pub first_line: usize,
    pub last_line: usize,
}

/// 列出每条语句；任何无法可靠分割的脚本都整体拒绝。
pub fn statements(sql: &str, mode: MysqlMode) -> Result<Vec<StatementRange>, String> {
    let tokens = tokenize(sql, mode)?;
    let Some(first) = tokens
        .iter()
        .find(|t| t.kind != Some(HighlightKind::Comment) && &sql[t.start..t.end] != ";")
    else {
        return Ok(Vec::new());
    };
    // current_statement 包含客户端命令、存储程序和模式变更的统一安全检查。
    current_statement(sql, byte_to_utf16(sql, first.start).unwrap(), mode)?;
    let mut result = Vec::new();
    let mut start = 0;
    for end in tokens
        .iter()
        .filter(|t| t.kind.is_none() && &sql[t.start..t.end] == ";")
        .map(|t| t.start)
        .chain(std::iter::once(sql.len()))
    {
        if let Some(first) = tokens
            .iter()
            .find(|t| t.start >= start && t.end <= end && t.kind != Some(HighlightKind::Comment))
        {
            let fragment = &sql[start..end];
            let left = start + fragment.len() - fragment.trim_start().len();
            let right = start + fragment.trim_end().len();
            result.push(StatementRange {
                range: byte_to_utf16(sql, left).unwrap()..byte_to_utf16(sql, right).unwrap(),
                first_line: sql[..first.start].bytes().filter(|b| *b == b'\n').count() + 1,
                last_line: sql[..right].bytes().filter(|b| *b == b'\n').count() + 1,
            });
        }
        start = end.saturating_add(1);
    }
    Ok(result)
}

/// Map Core's UTF-8 statement ranges to the editor's UTF-16 coordinates.
pub fn statements_in(sql: &str, context: SqlContext) -> Result<Vec<StatementRange>, String> {
    match context {
        SqlContext::Session(mode) => statements(sql, mode),
        SqlContext::Stateless => {
            let ranges = datapier_core::providers::mysql::stateless_statement_ranges(sql)
                .map_err(|error| error.message)?;
            ranges
                .into_iter()
                .map(|(start, end)| {
                    let fragment = sql.get(start..end).ok_or("无效的语句范围")?;
                    let tokens = tokenize(fragment, MysqlMode::default())?;
                    let first = tokens
                        .iter()
                        .find(|token| token.kind != Some(HighlightKind::Comment))
                        .ok_or("无效的语句范围")?;
                    Ok(StatementRange {
                        range: byte_to_utf16(sql, start).ok_or("无效的语句范围")?
                            ..byte_to_utf16(sql, end).ok_or("无效的语句范围")?,
                        first_line: sql[..start + first.start]
                            .bytes()
                            .filter(|b| *b == b'\n')
                            .count()
                            + 1,
                        last_line: sql[..end].bytes().filter(|b| *b == b'\n').count() + 1,
                    })
                })
                .collect()
        }
    }
}

pub fn current_statement_in(
    sql: &str,
    cursor: usize,
    context: SqlContext,
) -> Result<Range<usize>, String> {
    if let SqlContext::Session(mode) = context {
        return current_statement(sql, cursor, mode);
    }
    let byte = utf16_to_byte(sql, cursor).ok_or("光标不在有效的 UTF-16 字符边界")?;
    let ranges = datapier_core::providers::mysql::stateless_statement_ranges(sql)
        .map_err(|error| error.message)?;
    let tokens = tokenize(sql, MysqlMode::default())?;
    let mut start = 0;
    let mut end = sql.len();
    for token in tokens
        .iter()
        .filter(|token| token.kind.is_none() && &sql[token.start..token.end] == ";")
    {
        if token.start < byte {
            start = token.end;
        } else {
            end = token.start;
            break;
        }
    }
    let (left, right) = ranges
        .into_iter()
        .find(|(left, right)| *left >= start && *right <= end)
        .ok_or("光标处没有可执行的 SQL 语句")?;
    Ok(byte_to_utf16(sql, left).ok_or("无效的语句范围")?
        ..byte_to_utf16(sql, right).ok_or("无效的语句范围")?)
}

pub fn validate_explain_selection_in(sql: &str, context: SqlContext) -> Result<(), String> {
    match context {
        SqlContext::Session(mode) => validate_explain_selection(sql, mode),
        SqlContext::Stateless => {
            let statements = datapier_core::providers::mysql::stateless_statements(sql)
                .map_err(|error| error.message)?;
            if statements.len() != 1 {
                return Err("执行计划只支持单条 SQL".into());
            }
            validate_explain_ast(&statements[0])
        }
    }
}

/// 验证单条可解释语句；用于控件状态时不构造执行文本。
pub fn validate_explain_selection(sql: &str, mode: MysqlMode) -> Result<(), String> {
    if mode.ansi_quotes || mode.no_backslash_escapes {
        return Err("当前 SQL 模式无法安全解析执行计划".into());
    }
    if statements(sql, mode)?.len() != 1 {
        return Err("执行计划只支持单条 SQL".into());
    }
    validate_explain_ast(sql)
}

fn validate_explain_ast(sql: &str) -> Result<(), String> {
    let parsed = sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::MySqlDialect {}, sql)
        .map_err(|_| "无法安全解析选中的 SQL")?;
    if parsed.len() != 1
        || !matches!(
            &parsed[0],
            sqlparser::ast::Statement::Query(_)
                | sqlparser::ast::Statement::Insert(_)
                | sqlparser::ast::Statement::Update(_)
                | sqlparser::ast::Statement::Delete(_)
        )
    {
        return Err("该语句不支持执行计划".into());
    }
    Ok(())
}

#[cfg(test)]
#[path = "../tests/unit/editor.rs"]
mod tests;
