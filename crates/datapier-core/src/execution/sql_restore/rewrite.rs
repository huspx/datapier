use super::*;

#[derive(Debug)]
pub(super) struct RewriteToken {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) text: String,
    pub(super) identifier: bool,
}

// 保留原文偏移；普通注释和字符串永不作为标识符改写。
pub(super) fn rewrite_tokens(sql: &str, context: &SqlContext) -> Result<Vec<RewriteToken>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b[i..].starts_with(b"/*!") {
            i += 3;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
            continue;
        }
        if b[i..].starts_with(b"*/") {
            i += 2;
            continue;
        }
        if b[i..].starts_with(b"/*") {
            let end = sql[i + 2..]
                .find("*/")
                .ok_or_else(|| AppError::invalid("SQL 注释未闭合"))?;
            i += end + 4;
            continue;
        }
        if b[i] == b'#'
            || (b[i..].starts_with(b"--") && b.get(i + 2).is_some_and(u8::is_ascii_whitespace))
        {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        let start = i;
        if b[i] == b'`' || b[i] == b'\'' || b[i] == b'"' {
            let quote = b[i];
            let identifier = quote == b'`' || (quote == b'"' && context.ansi_quotes);
            i += 1;
            let mut text = Vec::new();
            let mut closed = false;
            while i < b.len() {
                if b[i] == quote {
                    if b.get(i + 1) == Some(&quote) {
                        text.push(quote);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    closed = true;
                    break;
                }
                if b[i] == b'\\' && !identifier && !context.no_backslash_escapes && i + 1 < b.len()
                {
                    text.push(b[i]);
                    i += 1;
                }
                text.push(b[i]);
                i += 1;
            }
            if !closed {
                return Err(AppError::invalid("SQL 引号未闭合"));
            }
            out.push(RewriteToken {
                start,
                end: i,
                text: String::from_utf8(text).map_err(|_| AppError::invalid("SQL 不是 UTF-8"))?,
                identifier,
            });
        } else if b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 128 {
            i += 1;
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$' || b[i] >= 128)
            {
                i += 1;
            }
            out.push(RewriteToken {
                start,
                end: i,
                text: sql[start..i].to_owned(),
                identifier: true,
            });
        } else {
            i += 1;
            out.push(RewriteToken {
                start,
                end: i,
                text: sql[start..i].to_owned(),
                identifier: false,
            });
        }
    }
    Ok(out)
}
fn quote_ident(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}
fn token_is(token: Option<&RewriteToken>, s: &str) -> bool {
    token.is_some_and(|t| t.text.eq_ignore_ascii_case(s))
}

fn quoted_identifier(sql: &str, token: &RewriteToken) -> bool {
    token.identifier && matches!(sql.as_bytes().get(token.start), Some(b'`' | b'"'))
}
pub(super) fn keyword_is(sql: &str, token: Option<&RewriteToken>, keyword: &str) -> bool {
    token.is_some_and(|token| {
        token.identifier
            && !quoted_identifier(sql, token)
            && token.text.eq_ignore_ascii_case(keyword)
    })
}
fn table_alias<'a>(sql: &str, tokens: &'a [RewriteToken], after: usize) -> Option<&'a str> {
    let position = if keyword_is(sql, tokens.get(after), "AS") {
        after + 1
    } else {
        after
    };
    let alias = tokens.get(position).filter(|token| token.identifier)?;
    const FOLLOWING: &[&str] = &[
        "WHERE",
        "GROUP",
        "HAVING",
        "ORDER",
        "LIMIT",
        "UNION",
        "INTERSECT",
        "EXCEPT",
        "JOIN",
        "INNER",
        "OUTER",
        "LEFT",
        "RIGHT",
        "CROSS",
        "NATURAL",
        "STRAIGHT_JOIN",
        "ON",
        "USING",
        "SET",
        "VALUES",
        "VALUE",
        "SELECT",
        "FOR",
        "LOCK",
        "IGNORE",
        "USE",
        "FORCE",
        "PARTITION",
        "INDEX",
        "WHEN",
        "THEN",
        "ELSE",
        "END",
        "WITH",
    ];
    if position == after
        && !quoted_identifier(sql, alias)
        && FOLLOWING
            .iter()
            .any(|keyword| alias.text.eq_ignore_ascii_case(keyword))
    {
        return None;
    }
    Some(alias.text.as_str())
}

// The dump does not record the source server's lower_case_table_names. An exact
// spelling is safe only when no other mapping could name the same object under
// case-insensitive server rules.
fn resolve_mapping<'a>(
    mappings: &'a [crate::model::transfer::TransferObject],
    database: &str,
    name: &str,
    kind: Option<&str>,
) -> Option<&'a crate::model::transfer::TransferObject> {
    let mut candidates = mappings.iter().filter(|mapping| {
        mapping
            .source
            .database
            .as_deref()
            .is_some_and(|db| db.eq_ignore_ascii_case(database))
            && mapping.source.name.eq_ignore_ascii_case(name)
            && kind.is_none_or(|kind| mapping.source.kind.eq_ignore_ascii_case(kind))
    });
    let mapping = candidates.next()?;
    if candidates.next().is_some()
        || mapping.source.database.as_deref() != Some(database)
        || mapping.source.name != name
    {
        return None;
    }
    Some(mapping)
}

// Commas separate table factors only inside a FROM list at the same nesting
// level. Nested expressions have their own state and cannot introduce factors
// into the containing list.
pub(super) fn from_commas(sql: &str, tokens: &[RewriteToken]) -> Vec<bool> {
    let mut result = vec![false; tokens.len()];
    let mut scopes = vec![false];
    for (i, token) in tokens.iter().enumerate() {
        match sql.as_bytes().get(token.start).copied() {
            Some(b'(') if token.end == token.start + 1 => scopes.push(false),
            Some(b')') if token.end == token.start + 1 && scopes.len() > 1 => {
                scopes.pop();
            }
            Some(b',') if token.end == token.start + 1 && *scopes.last().unwrap() => {
                result[i] = true
            }
            Some(b';') if token.end == token.start + 1 => *scopes.last_mut().unwrap() = false,
            _ if keyword_is(sql, Some(token), "FROM") => *scopes.last_mut().unwrap() = true,
            _ if [
                "WHERE",
                "GROUP",
                "HAVING",
                "ORDER",
                "LIMIT",
                "UNION",
                "INTERSECT",
                "EXCEPT",
                "FOR",
                "LOCK",
                "PROCEDURE",
                "INTO",
                "SET",
                "VALUES",
                "RETURNING",
                "WINDOW",
                "QUALIFY",
            ]
            .iter()
            .any(|word| keyword_is(sql, Some(token), word)) =>
            {
                *scopes.last_mut().unwrap() = false
            }
            _ => {}
        }
    }
    result
}

/// 对已预检 DDL 的明确对象引用进行语法位置映射，保留正文、注释及 SQL SECURITY。
/// 无法证明引用或列映射归属时返回错误，调用方应要求用户修订定义。
pub fn rewrite_ddl(
    sql: &str,
    stmt: &SqlStatement,
    mappings: &[crate::model::transfer::TransferObject],
    definer: &crate::model::transfer::DefinerMapping,
) -> Result<String> {
    use crate::model::transfer::DefinerMapping;
    if stmt.unresolved_references {
        return Err(err(stmt.start, stmt.line, "动态 SQL 的引用无法安全映射"));
    }
    let tokens = rewrite_tokens(sql, &stmt.context)?;
    let comma_factors = from_commas(sql, &tokens);
    if comma_factors.iter().any(|is_comma| *is_comma)
        && tokens
            .iter()
            .any(|token| keyword_is(sql, Some(token), "DELETE"))
    {
        return Err(err(
            stmt.start,
            stmt.line,
            "无法安全确定多表 DELETE 的表绑定",
        ));
    }
    let multiple_selects = tokens
        .iter()
        .filter(|token| keyword_is(sql, Some(token), "SELECT"))
        .count()
        > 1;
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut implicit_tables: Vec<&crate::model::transfer::TransferObject> = Vec::new();
    let mut aliases: Vec<&str> = Vec::new();
    let has_cte = tokens.iter().enumerate().any(|(index, token)| {
        keyword_is(sql, Some(token), "WITH")
            && tokens.get(index + 1).is_some_and(|next| {
                next.identifier
                    && !["CHECK", "LOCAL", "CASCADED", "ROLLUP"]
                        .iter()
                        .any(|keyword| next.text.eq_ignore_ascii_case(keyword))
            })
    });
    let mut object_found = false;
    let object_keyword = match stmt.kind {
        SqlKind::Table => "TABLE",
        SqlKind::View => "VIEW",
        SqlKind::Trigger => "TRIGGER",
        SqlKind::Procedure => "PROCEDURE",
        SqlKind::Function => "FUNCTION",
        SqlKind::Event => "EVENT",
        SqlKind::Index => "INDEX",
        _ => return Err(AppError::invalid("该语句不能作为对象定义改写")),
    };
    let object_pos = tokens
        .iter()
        .position(|token| keyword_is(sql, Some(token), object_keyword))
        .ok_or_else(|| err(stmt.start, stmt.line, "无法定位 DDL 对象"))?;
    let trigger_on = if stmt.kind == SqlKind::Trigger {
        tokens
            .iter()
            .enumerate()
            .skip(object_pos + 1)
            .find(|(_, token)| keyword_is(sql, Some(token), "ON"))
            .map(|(i, _)| i)
    } else {
        None
    };
    for (i, token) in tokens.iter().enumerate() {
        let comma_factor = comma_factors[i];
        if !comma_factor && (!token.identifier || quoted_identifier(sql, token)) {
            continue;
        }
        let own = i == object_pos;
        let dependency = comma_factor
            || token.text.eq_ignore_ascii_case("REFERENCES")
            || (matches!(
                stmt.kind,
                SqlKind::View
                    | SqlKind::Trigger
                    | SqlKind::Procedure
                    | SqlKind::Function
                    | SqlKind::Event
            ) && ["FROM", "JOIN", "UPDATE", "INTO", "CALL"]
                .iter()
                .any(|v| token.text.eq_ignore_ascii_case(v))
                && !(token.text.eq_ignore_ascii_case("UPDATE")
                    && i > 0
                    && (token_is(tokens.get(i - 1), "BEFORE")
                        || token_is(tokens.get(i - 1), "AFTER"))))
            || trigger_on == Some(i);
        if !own && !dependency {
            continue;
        }
        let mut j = i + 1;
        if token_is(tokens.get(j), "IF") {
            j += 1;
            if token_is(tokens.get(j), "NOT") {
                j += 1;
            }
            if token_is(tokens.get(j), "EXISTS") {
                j += 1;
            }
        }
        let Some(first) = tokens.get(j) else {
            return Err(err(stmt.start, stmt.line, "缺少静态对象名"));
        };
        if !first.identifier {
            if first.text == "(" && !own {
                if !keyword_is(sql, tokens.get(j + 1), "SELECT") {
                    return Err(err(
                        stmt.start + first.start as u64,
                        stmt.line,
                        "无法确定括号内的表引用",
                    ));
                }
                let mut depth = 0usize;
                for end in j..tokens.len() {
                    match tokens[end].text.as_str() {
                        "(" => depth += 1,
                        ")" => {
                            depth = depth.saturating_sub(1);
                            if depth == 0 {
                                if let Some(alias) = table_alias(sql, &tokens, end + 1) {
                                    aliases.push(alias);
                                }
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                continue;
            }
            return Err(err(stmt.start, stmt.line, "无法确定对象引用"));
        }
        if !own && keyword_is(sql, Some(first), "DUAL") && !token_is(tokens.get(j + 1), ".") {
            continue;
        }
        let qualified =
            token_is(tokens.get(j + 1), ".") && tokens.get(j + 2).is_some_and(|t| t.identifier);
        let (database, name, end) = if qualified {
            (
                Some(first.text.as_str()),
                tokens[j + 2].text.as_str(),
                tokens[j + 2].end,
            )
        } else {
            (
                stmt.source_database
                    .as_deref()
                    .or(stmt.context.database.as_deref()),
                first.text.as_str(),
                first.end,
            )
        };
        let source_db =
            database.ok_or_else(|| err(stmt.start, stmt.line, "无法确定引用所属数据库"))?;
        let mapping = resolve_mapping(mappings, source_db, name, None).ok_or_else(|| {
            err(
                stmt.start + first.start as u64,
                stmt.line,
                format!("未映射、外部引用或大小写不确定 {source_db}.{name}"),
            )
        })?;
        if !own
            && (matches!(mapping.source.kind.as_str(), "table" | "view")
                || (mapping.source.kind.is_empty()
                    && (comma_factor
                        || keyword_is(sql, Some(token), "FROM")
                        || keyword_is(sql, Some(token), "JOIN"))))
        {
            if has_cte {
                return Err(err(
                    stmt.start,
                    stmt.line,
                    "CTE 的局部绑定需要人工确认后再改名",
                ));
            }
            let after = j + if qualified { 3 } else { 1 };
            if let Some(alias) = table_alias(sql, &tokens, after) {
                aliases.push(alias);
            } else if !implicit_tables
                .iter()
                .any(|&known| std::ptr::eq(known, mapping))
            {
                implicit_tables.push(mapping);
            }
        }
        if mapping.columns.iter().any(|c| c.source != c.target) {
            return Err(err(
                stmt.start + first.start as u64,
                stmt.line,
                "对象存在列重命名，无法证明正文中各列的绑定",
            ));
        }
        let target_db = mapping
            .target
            .database
            .as_deref()
            .ok_or_else(|| AppError::invalid("目标对象缺少数据库"))?;
        edits.push((
            first.start,
            end,
            format!(
                "{}.{}",
                quote_ident(target_db),
                quote_ident(&mapping.target.name)
            ),
        ));
        if own {
            object_found = true;
        }
    }
    if !object_found {
        return Err(err(stmt.start, stmt.line, "未定位可映射对象"));
    }
    for i in 0..tokens.len().saturating_sub(1) {
        if !tokens[i].identifier || !token_is(tokens.get(i + 1), "(") {
            continue;
        }
        let qualified = i >= 2 && token_is(tokens.get(i - 1), ".") && tokens[i - 2].identifier;
        let first = if qualified {
            &tokens[i - 2]
        } else {
            &tokens[i]
        };
        let full_name = if qualified {
            format!("{}.{}", first.text, tokens[i].text)
        } else {
            tokens[i].text.clone()
        };
        if !stmt
            .function_calls
            .iter()
            .any(|v| v.eq_ignore_ascii_case(&full_name))
        {
            continue;
        }
        if edits
            .iter()
            .any(|(start, end, _)| *start <= first.start && *end >= tokens[i].end)
        {
            continue;
        }
        let source_db = if qualified {
            Some(first.text.as_str())
        } else {
            stmt.source_database
                .as_deref()
                .or(stmt.context.database.as_deref())
        };
        let mapping = source_db
            .and_then(|db| resolve_mapping(mappings, db, &tokens[i].text, Some("function")))
            .ok_or_else(|| {
                err(
                    stmt.start + first.start as u64,
                    stmt.line,
                    "存储函数引用没有唯一的大小写确定映射",
                )
            })?;
        let target_db = mapping
            .target
            .database
            .as_deref()
            .ok_or_else(|| AppError::invalid("目标函数缺少数据库"))?;
        edits.push((
            first.start,
            tokens[i].end,
            format!(
                "{}.{}",
                quote_ident(target_db),
                quote_ident(&mapping.target.name)
            ),
        ));
    }
    // 三段限定名中的库和表也必须跟随映射；不能只改 FROM 后的对象。
    for i in 0..tokens.len().saturating_sub(4) {
        let first = &tokens[i];
        if !first.identifier
            || !token_is(tokens.get(i + 1), ".")
            || !tokens[i + 2].identifier
            || !token_is(tokens.get(i + 3), ".")
            || !tokens[i + 4].identifier
        {
            continue;
        }
        let end = tokens[i + 2].end;
        if edits
            .iter()
            .any(|(start, stop, _)| *start <= first.start && *stop >= end)
        {
            continue;
        }
        let mapping = resolve_mapping(mappings, &first.text, &tokens[i + 2].text, None)
            .filter(|mapping| {
                implicit_tables
                    .iter()
                    .any(|known| std::ptr::eq(*known, *mapping))
            })
            .filter(|_| !multiple_selects)
            .ok_or_else(|| {
                err(
                    stmt.start + first.start as u64,
                    stmt.line,
                    "无法确定三段限定名的对象归属或作用域",
                )
            })?;
        if mapping.columns.iter().any(|c| c.source != c.target) {
            return Err(err(
                stmt.start + first.start as u64,
                stmt.line,
                "无法证明列重命名的绑定",
            ));
        }
        let db = mapping
            .target
            .database
            .as_deref()
            .ok_or_else(|| AppError::invalid("目标对象缺少数据库"))?;
        edits.push((
            first.start,
            end,
            format!("{}.{}", quote_ident(db), quote_ident(&mapping.target.name)),
        ));
    }
    // 显式别名保持原样；无别名且归属唯一的表限定符随 FROM 等静态引用一并改名。
    for i in 0..tokens.len().saturating_sub(2) {
        let first = &tokens[i];
        if !first.identifier
            || !token_is(tokens.get(i + 1), ".")
            || !(tokens[i + 2].identifier || tokens[i + 2].text == "*")
        {
            continue;
        }
        if edits
            .iter()
            .any(|(start, stop, _)| *start <= first.start && *stop >= tokens[i + 2].end)
        {
            continue;
        }
        if i > 0 && token_is(tokens.get(i - 1), ".") {
            continue;
        }
        if stmt.kind == SqlKind::Trigger
            && ["NEW", "OLD"]
                .iter()
                .any(|name| first.text.eq_ignore_ascii_case(name))
        {
            continue;
        }
        let mut candidates = implicit_tables.iter().copied().filter(|mapping| {
            mapping.source.name.eq_ignore_ascii_case(&first.text)
                && (mapping.source.name != mapping.target.name
                    || mapping.source.database != mapping.target.database)
        });
        let Some(mapping) = candidates.next() else {
            continue;
        };
        if candidates.next().is_some()
            || multiple_selects
            || mapping.source.name != first.text
            || aliases
                .iter()
                .any(|alias| alias.eq_ignore_ascii_case(&first.text))
        {
            return Err(err(
                stmt.start + first.start as u64,
                stmt.line,
                "表限定符跨作用域或别名存在歧义，无法安全改名",
            ));
        }
        let database = mapping
            .target
            .database
            .as_deref()
            .ok_or_else(|| AppError::invalid("目标对象缺少数据库"))?;
        edits.push((
            first.start,
            first.end,
            format!(
                "{}.{}",
                quote_ident(database),
                quote_ident(&mapping.target.name)
            ),
        ));
    }
    if let Some(i) = tokens[..object_pos]
        .iter()
        .position(|token| keyword_is(sql, Some(token), "DEFINER"))
        && token_is(tokens.get(i + 1), "=")
    {
        let mut end = object_pos;
        if let Some(pos) = tokens[i + 2..object_pos]
            .iter()
            .position(|token| keyword_is(sql, Some(token), "SQL"))
        {
            end = i + 2 + pos;
        }
        if end <= i + 2 {
            return Err(err(stmt.start, stmt.line, "无法定位 DEFINER"));
        }
        let new_definer = match definer {
            DefinerMapping::Preserve => None,
            DefinerMapping::CurrentUser => Some("CURRENT_USER".to_string()),
            DefinerMapping::Map(accounts) => {
                let source = stmt
                    .definer
                    .as_ref()
                    .ok_or_else(|| err(stmt.start, stmt.line, "缺少源 DEFINER"))?;
                let target = accounts
                    .get(source)
                    .ok_or_else(|| err(stmt.start, stmt.line, "缺少 DEFINER 映射"))?;
                let (user, host) = target
                    .split_once('@')
                    .ok_or_else(|| AppError::invalid("目标 DEFINER 必须为 user@host"))?;
                Some(format!("{}@{}", quote_ident(user), quote_ident(host)))
            }
        };
        if let Some(value) = new_definer {
            edits.push((tokens[i + 2].start, tokens[end - 1].end, value));
        }
    }
    if stmt.kind == SqlKind::Event {
        let do_pos = tokens
            .iter()
            .enumerate()
            .skip(object_pos + 1)
            .find(|(_, t)| t.identifier && t.text.eq_ignore_ascii_case("DO"))
            .map(|(i, _)| i)
            .ok_or_else(|| err(stmt.start, stmt.line, "无法确定 EVENT 正文"))?;
        let state = tokens[object_pos + 1..do_pos].iter().find(|t| {
            t.identifier
                && (t.text.eq_ignore_ascii_case("ENABLE") || t.text.eq_ignore_ascii_case("DISABLE"))
        });
        if let Some(state) = state {
            if state.text.eq_ignore_ascii_case("ENABLE") {
                edits.push((state.start, state.end, "DISABLE".into()));
            }
        } else {
            edits.push((
                tokens[do_pos].start,
                tokens[do_pos].start,
                "DISABLE ".into(),
            ));
        }
    }
    edits.sort_by_key(|(start, _, _)| *start);
    if edits.windows(2).any(|w| w[0].1 > w[1].0) {
        return Err(err(stmt.start, stmt.line, "SQL 映射位置冲突"));
    }
    let mut result = sql.to_owned();
    for (start, end, value) in edits.into_iter().rev() {
        result.replace_range(start..end, &value);
    }
    Ok(result)
}
