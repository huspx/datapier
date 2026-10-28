use serde::{Deserialize, Serialize};
use std::ops::Range;

/// 文本编辑状态；Tern 的 cursor 和 anchor 均为 UTF-16 code unit 偏移。
/// undo 仅保存在当前进程，不随工作区序列化。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
    pub anchor: usize,
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

impl Editor {
    pub fn new(text: impl Into<String>) -> Self {
        let text = text.into();
        let cursor = text.encode_utf16().count();
        Self {
            text,
            cursor,
            anchor: cursor,
            undo: Vec::new(),
        }
    }

    pub fn selection(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }

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
        self.cursor = range.start + replacement.encode_utf16().count();
        self.anchor = self.cursor;
        true
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

/// 从 SQL 关键词和调用方缓存的目录名称中补全，按大小写无关方式排序去重。
pub fn completions(prefix: &str, catalog: &[String]) -> Vec<String> {
    let lower = prefix.to_lowercase();
    let mut items: Vec<String> = KEYWORDS
        .iter()
        .copied()
        .map(str::to_owned)
        .chain(catalog.iter().cloned())
        .filter(|item| item.to_lowercase().starts_with(&lower))
        .collect();
    items.sort_by_key(|item| item.to_lowercase());
    items.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_selection_edit_and_undo_preserve_unicode() {
        let mut editor = Editor::new("中😀文");
        assert!(!editor.set_selection(1, 2));
        assert!(editor.set_selection(1, 3));
        assert_eq!(editor.selected_text(), Some("😀"));
        assert!(editor.key(Key::Text("好"), false));
        assert_eq!(editor.text, "中好文");
        assert!(editor.undo());
        assert_eq!(editor.selected_text(), Some("😀"));
        assert!(editor.key(Key::Backspace, false));
        assert_eq!(editor.text, "中文");
    }

    #[test]
    fn shift_selection_tracks_utf16_across_chinese_and_emoji() {
        let mut editor = Editor::new("中😀文");
        assert!(editor.key(Key::Left, true));
        assert_eq!((editor.anchor, editor.cursor), (4, 3));
        assert_eq!(editor.selected_text(), Some("文"));
        assert!(editor.key(Key::Left, true));
        assert_eq!((editor.anchor, editor.cursor), (4, 1));
        assert_eq!(editor.selected_text(), Some("😀文"));
        assert!(editor.key(Key::Left, true));
        assert_eq!((editor.anchor, editor.cursor), (4, 0));
        assert_eq!(editor.selected_text(), Some("中😀文"));
        assert!(editor.key(Key::Right, true));
        assert_eq!((editor.anchor, editor.cursor), (4, 1));
        assert_eq!(editor.selected_text(), Some("😀文"));
        assert!(editor.key(Key::Right, false));
        assert_eq!((editor.anchor, editor.cursor), (4, 4));
        assert_eq!(editor.selected_text(), None);
    }

    #[test]
    fn vertical_navigation_preserves_character_column_and_shift_anchor() {
        let mut editor = Editor::new("甲😀乙\na😀z\n短");
        assert!(editor.set_selection(3, 3));
        assert!(editor.key(Key::Down, true));
        assert_eq!(editor.cursor, 8);
        assert_eq!(editor.anchor, 3);
        assert_eq!(editor.selected_text(), Some("乙\na😀"));
        assert!(editor.key(Key::Down, false));
        assert_eq!(editor.cursor, 11);
        assert_eq!(editor.anchor, 11);
        assert!(editor.key(Key::Up, false));
        assert_eq!(editor.cursor, 6);
    }

    #[test]
    fn mysql_statement_respects_quotes_comments_and_mode() {
        let sql = "SELECT '😀;x' /* ; */; SELECT `中;文`;";
        let cursor = sql[..sql.find("`中").unwrap()].encode_utf16().count();
        let range = current_statement(sql, cursor, MysqlMode::default()).unwrap();
        assert_eq!(
            &sql[utf16_to_byte(sql, range.start).unwrap()..utf16_to_byte(sql, range.end).unwrap()],
            "SELECT `中;文`"
        );
        assert!(current_statement("SELECT 'unterminated;", 0, MysqlMode::default()).is_err());
        assert!(
            current_statement(
                "CREATE DEFINER = root PROCEDURE p() BEGIN SELECT 1; END;",
                0,
                MysqlMode::default()
            )
            .is_err()
        );
        assert!(current_statement("SELECT 1; \\g", 0, MysqlMode::default()).is_err());
    }

    #[test]
    fn mysql_rejects_ambiguous_comments_and_client_directives() {
        let mode = MysqlMode::default();
        for sql in [
            "SELECT 1 /*!80000 ; SELECT 2 */;",
            "SELECT 1 /*M!100000 ; SELECT 2 */;",
            "SELECT 1 /* outer /* inner */ ; SELECT 2 */;",
            "SELECT 1 # comment\r; SELECT 2",
            "DELIMITER $$\nSELECT 1$$",
            "SET @@session.`sql_mode` = ''; SELECT 1;",
        ] {
            assert!(current_statement(sql, 0, mode).is_err(), "{sql}");
        }
        let sql = "SELECT 1 -- comment;\r\n; SELECT 2";
        let cursor = sql.find("SELECT 2").unwrap();
        let range = current_statement(sql, cursor, mode).unwrap();
        assert_eq!(&sql[range], "SELECT 2");
    }

    #[test]
    fn mysql_modes_control_backslash_and_double_quotes() {
        let string_sql = r"SELECT 'a\'; SELECT 2;";
        assert!(current_statement(string_sql, 0, MysqlMode::default()).is_err());
        let no_escape = MysqlMode {
            no_backslash_escapes: true,
            ansi_quotes: false,
        };
        assert_eq!(
            &string_sql[current_statement(string_sql, 0, no_escape).unwrap()],
            r"SELECT 'a\'"
        );

        let quoted_sql = r#"SELECT "a\"; SELECT 2;"#;
        assert!(current_statement(quoted_sql, 0, MysqlMode::default()).is_err());
        let ansi = MysqlMode {
            no_backslash_escapes: false,
            ansi_quotes: true,
        };
        assert_eq!(
            &quoted_sql[current_statement(quoted_sql, 0, ansi).unwrap()],
            r#"SELECT "a\""#
        );
    }
}
