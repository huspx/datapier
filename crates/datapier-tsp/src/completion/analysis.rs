use std::ops::Range;

use crate::editor::{self, MysqlMode, SqlContext};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Relation {
    pub database: Option<String>,
    pub name: String,
    pub alias: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    Expression,
    Relation,
    QualifiedRelation,
    QualifiedColumn,
    InsertColumn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Analysis {
    pub range: Range<usize>,
    pub prefix: String,
    pub qualifier: Option<String>,
    pub position: Position,
    pub relations: Vec<Relation>,
    pub excluded_columns: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Ident,
    String,
    Comment,
    Symbol,
}

struct Token<'a> {
    text: &'a str,
    start: usize,
    end: usize,
    kind: Kind,
    closed: bool,
}

impl Token<'_> {
    fn is(&self, word: &str) -> bool {
        self.kind == Kind::Word && self.text.eq_ignore_ascii_case(word)
    }

    fn symbol(&self, symbol: &str) -> bool {
        self.kind == Kind::Symbol && self.text == symbol
    }

    fn identifier(&self) -> bool {
        matches!(self.kind, Kind::Word | Kind::Ident)
    }

    fn name(&self) -> String {
        if self.kind == Kind::Ident {
            let quote = &self.text[..1];
            let end = self.text.len() - usize::from(self.closed);
            self.text[1..end].replace(&format!("{quote}{quote}"), quote)
        } else {
            self.text.to_owned()
        }
    }
}

// 编辑中的引号和注释可以尚未闭合，不能使用要求完整语句的执行 tokenizer。
fn tokens(sql: &str, mode: MysqlMode) -> Vec<Token<'_>> {
    let bytes = sql.as_bytes();
    let mut result = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let b = bytes[i];
        if b.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let mut closed = true;
        let kind = if b == b'#'
            || (b == b'-'
                && bytes.get(i + 1) == Some(&b'-')
                && bytes
                    .get(i + 2)
                    .is_some_and(|c| c.is_ascii_whitespace() || c.is_ascii_control()))
        {
            i = sql[i..].find('\n').map_or(bytes.len(), |n| i + n);
            closed = i < bytes.len();
            Kind::Comment
        } else if b == b'/' && bytes.get(i + 1) == Some(&b'*') {
            if let Some(end) = sql[i + 2..].find("*/") {
                i += end + 4;
            } else {
                i = bytes.len();
                closed = false;
            }
            Kind::Comment
        } else if matches!(b, b'\'' | b'"' | b'`') {
            let ident = b == b'`' || (b == b'"' && mode.ansi_quotes);
            i += 1;
            closed = false;
            while i < bytes.len() {
                if bytes[i] == b {
                    if bytes.get(i + 1) == Some(&b) {
                        i += 2;
                    } else {
                        i += 1;
                        closed = true;
                        break;
                    }
                } else if bytes[i] == b'\\' && !ident && !mode.no_backslash_escapes {
                    i = (i + 2).min(bytes.len());
                } else {
                    i += 1;
                }
            }
            if ident { Kind::Ident } else { Kind::String }
        } else if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$') || b >= 128 {
            i += 1;
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || matches!(bytes[i], b'_' | b'$')
                    || bytes[i] >= 128)
            {
                i += 1;
            }
            Kind::Word
        } else {
            i += 1;
            Kind::Symbol
        };
        result.push(Token {
            text: &sql[start..i],
            start,
            end: i,
            kind,
            closed,
        });
    }
    result
}

fn reserved(token: &Token<'_>) -> bool {
    [
        "SELECT",
        "FROM",
        "WHERE",
        "JOIN",
        "LEFT",
        "RIGHT",
        "INNER",
        "OUTER",
        "CROSS",
        "NATURAL",
        "ON",
        "USING",
        "AS",
        "GROUP",
        "ORDER",
        "HAVING",
        "LIMIT",
        "UNION",
        "EXCEPT",
        "INTERSECT",
        "SET",
        "VALUES",
        "VALUE",
        "RETURNING",
        "UPDATE",
        "DELETE",
        "INSERT",
        "INTO",
        "WITH",
        "FOR",
        "WINDOW",
        "JOIN",
        "STRAIGHT_JOIN",
        "PARTITION",
        "USE",
        "FORCE",
        "IGNORE",
    ]
    .iter()
    .any(|word| token.is(word))
}

fn matching(tokens: &[Token<'_>]) -> Vec<Option<usize>> {
    let mut pairs = vec![None; tokens.len()];
    let mut stack = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        if token.symbol("(") {
            stack.push(i);
        } else if token.symbol(")")
            && let Some(open) = stack.pop()
        {
            pairs[open] = Some(i);
            pairs[i] = Some(open);
        }
    }
    pairs
}

fn alias_at(tokens: &[Token<'_>], mut index: usize, end: usize) -> Option<(String, usize)> {
    if index < end && tokens[index].is("AS") {
        index += 1;
    }
    if index < end && tokens[index].identifier() && !reserved(&tokens[index]) {
        Some((tokens[index].name(), index + 1))
    } else {
        None
    }
}

fn is_cte(names: &[String], name: &str) -> bool {
    names.iter().any(|item| item.eq_ignore_ascii_case(name))
}

fn analyze_mode(
    sql: &str,
    cursor: usize,
    mode: MysqlMode,
    database: Option<&str>,
) -> Option<Analysis> {
    let byte = editor::utf16_to_byte(sql, cursor)?;
    let all = tokens(sql, mode);
    if all.iter().any(|t| {
        matches!(t.kind, Kind::String | Kind::Comment)
            && byte >= t.start
            && (byte < t.end || (!t.closed && byte == t.end))
    }) {
        return None;
    }
    let start = all
        .iter()
        .filter(|t| t.symbol(";") && t.end <= byte)
        .map(|t| t.end)
        .next_back()
        .unwrap_or(0);
    let end = all
        .iter()
        .find(|t| t.symbol(";") && t.start >= byte)
        .map_or(sql.len(), |t| t.start);
    let tokens: Vec<_> = all
        .into_iter()
        .filter(|t| t.start >= start && t.end <= end && t.kind != Kind::Comment)
        .collect();
    let pairs = matching(&tokens);

    // 只分析光标所在查询块；子查询中的表不能泄漏到外层查询。
    let mut scope_start = 0;
    let mut scope_end = tokens.len();
    for (i, token) in tokens.iter().enumerate() {
        if token.symbol("(")
            && token.end <= byte
            && pairs[i].is_none_or(|j| tokens[j].start >= byte)
            && tokens
                .get(i + 1)
                .is_some_and(|t| t.is("SELECT") || t.is("WITH"))
        {
            scope_start = i + 1;
            scope_end = pairs[i].unwrap_or(tokens.len());
        }
    }

    // 收集所有 WITH 声明；保守屏蔽同名对象，避免把 CTE 当成实体表。
    let mut ctes = Vec::new();
    for i in 0..tokens.len() {
        if !tokens[i].is("WITH") {
            continue;
        }
        let mut j = i + 1;
        if tokens.get(j).is_some_and(|t| t.is("RECURSIVE")) {
            j += 1;
        }
        while j < tokens.len() && tokens[j].identifier() {
            let name = tokens[j].name();
            j += 1;
            if tokens.get(j).is_some_and(|t| t.symbol("(")) {
                let Some(close) = pairs[j] else {
                    break;
                };
                j = close + 1;
            }
            if !tokens.get(j).is_some_and(|t| t.is("AS")) {
                break;
            }
            j += 1;
            if !tokens.get(j).is_some_and(|t| t.symbol("(")) {
                break;
            }
            ctes.push(name);
            let Some(close) = pairs[j] else {
                break;
            };
            j = close + 1;
            if !tokens.get(j).is_some_and(|t| t.symbol(",")) {
                break;
            }
            j += 1;
        }
    }

    let current = (scope_start..scope_end)
        .find(|&i| tokens[i].identifier() && tokens[i].start <= byte && byte <= tokens[i].end);
    let (range, prefix, before) = if let Some(i) = current {
        let token = &tokens[i];
        let prefix = if token.kind == Kind::Ident {
            let from = (token.start + 1).min(byte);
            let to = byte.min(token.end - usize::from(token.closed));
            if from <= to {
                sql[from..to].replace(&format!("{0}{0}", &token.text[..1]), &token.text[..1])
            } else {
                String::new()
            }
        } else {
            sql[token.start..byte].to_owned()
        };
        (
            editor::byte_to_utf16(sql, token.start)?..editor::byte_to_utf16(sql, token.end)?,
            prefix,
            i,
        )
    } else {
        (
            cursor..cursor,
            String::new(),
            (scope_start..scope_end)
                .find(|&i| tokens[i].start >= byte)
                .unwrap_or(scope_end),
        )
    };
    let dot = before
        .checked_sub(1)
        .filter(|&i| i >= scope_start && tokens[i].symbol("."));
    let qualifier = dot
        .and_then(|i| i.checked_sub(1))
        .filter(|&i| i >= scope_start && tokens[i].identifier())
        .map(|i| tokens[i].name());
    let preceding = dot.and_then(|i| i.checked_sub(1)).unwrap_or(before);

    let mut relations = Vec::new();
    let mut derived_aliases = Vec::new();
    let mut relation_positions = Vec::new();
    let mut insert_columns = None;
    let mut i = scope_start;
    let mut from_list = false;
    while i < scope_end {
        let t = &tokens[i];
        if t.symbol("(") {
            i = pairs[i].map_or(scope_end, |j| j + 1);
            continue;
        }
        if [
            "WHERE",
            "GROUP",
            "ORDER",
            "HAVING",
            "LIMIT",
            "UNION",
            "SET",
            "VALUES",
            "RETURNING",
            "ON",
            "USING",
        ]
        .iter()
        .any(|w| t.is(w))
        {
            from_list = false;
        }
        let trigger = t.is("FROM")
            || t.is("JOIN")
            || t.is("UPDATE")
            || t.is("INTO")
            || (from_list && t.symbol(","));
        if !trigger {
            i += 1;
            continue;
        }
        if t.is("FROM") {
            from_list = true;
        }
        let mut j = i + 1;
        while j < scope_end
            && ["LOW_PRIORITY", "IGNORE", "ONLY"]
                .iter()
                .any(|w| tokens[j].is(w))
        {
            j += 1;
        }
        relation_positions.push(j);
        if j >= scope_end {
            break;
        }
        if tokens[j].symbol("(") {
            let Some(close) = pairs[j] else {
                break;
            };
            if let Some((alias, next)) = alias_at(&tokens, close + 1, scope_end) {
                derived_aliases.push(alias);
                i = next;
            } else {
                i = close + 1;
            }
            continue;
        }
        if !tokens[j].identifier() || reserved(&tokens[j]) {
            i = j;
            continue;
        }
        let first = tokens[j].name();
        j += 1;
        let (db, name) = if j + 1 < scope_end && tokens[j].symbol(".") && tokens[j + 1].identifier()
        {
            j += 2;
            (Some(first), tokens[j - 1].name())
        } else {
            (database.map(str::to_owned), first)
        };
        let table_end = j;
        let (alias, next) = alias_at(&tokens, j, scope_end).unwrap_or_else(|| (name.clone(), j));
        j = next;
        if !is_cte(&ctes, &name) || (table_end >= 3 && tokens[table_end - 2].symbol(".")) {
            relations.push(Relation {
                database: db,
                name,
                alias,
            });
        } else {
            derived_aliases.push(alias);
        }
        if t.is("INTO")
            && tokens[scope_start..i].iter().any(|x| x.is("INSERT"))
            && j < scope_end
            && tokens[j].symbol("(")
        {
            insert_columns = Some((j, pairs[j].unwrap_or(scope_end)));
        }
        i = j;
    }
    // 同名别名或集合查询的作用域无法唯一确定时不推测实体表。
    for a in 0..relations.len() {
        for b in a + 1..relations.len() {
            if relations[a].alias.eq_ignore_ascii_case(&relations[b].alias) {
                derived_aliases.push(relations[a].alias.clone());
            }
        }
    }
    relations.retain(|r| !is_cte(&derived_aliases, &r.alias));
    if tokens[scope_start..scope_end]
        .iter()
        .any(|t| t.is("UNION") || t.is("INTERSECT") || t.is("EXCEPT"))
    {
        relations.clear();
    }
    let mut excluded_columns = Vec::new();
    let in_insert = insert_columns.is_some_and(|(open, close)| {
        tokens[open].end <= byte && (close == scope_end || byte <= tokens[close].start)
    });
    if in_insert {
        let (open, close) = insert_columns.unwrap();
        for (k, token) in tokens.iter().enumerate().take(close).skip(open + 1) {
            if Some(k) != current && token.identifier() && !reserved(token) {
                excluded_columns.push(token.name());
            }
        }
    }
    let relation_position = relation_positions
        .iter()
        .any(|&p| p == preceding || p == before);
    let position = if in_insert {
        Position::InsertColumn
    } else if relation_position && qualifier.is_some() {
        Position::QualifiedRelation
    } else if relation_position {
        Position::Relation
    } else if qualifier.is_some() {
        Position::QualifiedColumn
    } else {
        Position::Expression
    };
    Some(Analysis {
        range,
        prefix,
        qualifier,
        position,
        relations,
        excluded_columns,
    })
}

/// 分析光标所在语句；光标与替换范围均使用 UTF-16 code unit。
pub fn analyze(
    sql: &str,
    cursor: usize,
    context: SqlContext,
    database: Option<&str>,
) -> Option<Analysis> {
    match context {
        SqlContext::Session(mode) => analyze_mode(sql, cursor, mode, database),
        SqlContext::Stateless => {
            // 无连接模式时，仅在所有引号和转义规则得出相同结果时补全。
            let result = analyze_mode(sql, cursor, MysqlMode::default(), database)?;
            for ansi_quotes in [false, true] {
                for no_backslash_escapes in [false, true] {
                    let mode = MysqlMode {
                        ansi_quotes,
                        no_backslash_escapes,
                    };
                    if analyze_mode(sql, cursor, mode, database).as_ref() != Some(&result) {
                        return None;
                    }
                }
            }
            Some(result)
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/completion_analysis.rs"]
mod tests;
