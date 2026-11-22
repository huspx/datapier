use super::*;
pub(super) fn words(sql: &str) -> Vec<String> {
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
pub(super) fn is(w: Option<&String>, s: &str) -> bool {
    w.is_some_and(|v| v.eq_ignore_ascii_case(s))
}

/// mysqldump 的临时视图只投影 `1 AS 列名`，且必须在同名最终定义之前。
pub(super) fn constant_view_stub(words: &[String]) -> bool {
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

pub(super) fn name_parts(
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

pub(super) fn ddl_identity(
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

pub(super) fn static_references(
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

pub(super) fn function_calls(
    sql: &str,
    kind: SqlKind,
    budget: &mut IndexBudget,
) -> Result<Vec<String>> {
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

pub(super) fn parse_insert(
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

pub(super) fn create_columns(sql: &str, budget: &mut IndexBudget) -> Result<Option<Vec<String>>> {
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

pub(super) fn classify(
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
