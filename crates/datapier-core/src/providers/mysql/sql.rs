use crate::model::{AppError, Result};
use sqlparser::{
    ast::{
        ObjectName, ObjectNamePart, ShowCreateObject, ShowStatementFilter,
        ShowStatementFilterPosition, ShowStatementOptions, Statement,
    },
    dialect::MySqlDialect,
    parser::Parser,
};

#[derive(Clone, Copy)]
enum Token<'a> {
    Word(&'a str),
    Symbol(u8),
    Quoted,
}

// MySQL 可执行注释包含 SQL，普通注释则视为空白。
struct Tokens<'a> {
    input: &'a str,
    pos: usize,
    executable: bool,
    no_escape: bool,
    ansi: bool,
}

impl<'a> Tokens<'a> {
    fn new(input: &'a str, no_escape: bool, ansi: bool) -> Self {
        Self {
            input,
            pos: 0,
            executable: false,
            no_escape,
            ansi,
        }
    }

    fn next(&mut self) -> Option<Token<'a>> {
        let b = self.input.as_bytes();
        loop {
            while b.get(self.pos).is_some_and(u8::is_ascii_whitespace) {
                self.pos += 1;
            }
            if self.executable && b.get(self.pos..self.pos + 2) == Some(b"*/") {
                self.pos += 2;
                self.executable = false;
                continue;
            }
            if b.get(self.pos) == Some(&b'#')
                || (b.get(self.pos..self.pos + 2) == Some(b"--")
                    && b.get(self.pos + 2)
                        .is_some_and(|c| c.is_ascii_whitespace() || c.is_ascii_control()))
            {
                while b.get(self.pos).is_some_and(|c| *c != b'\n' && *c != b'\r') {
                    self.pos += 1;
                }
                continue;
            }
            if b.get(self.pos..self.pos + 2) == Some(b"/*") {
                if !self.executable && b.get(self.pos + 2) == Some(&b'!') {
                    self.pos += 3;
                    while b.get(self.pos).is_some_and(u8::is_ascii_digit) {
                        self.pos += 1;
                    }
                    self.executable = true;
                } else {
                    self.pos += 2;
                    while self.pos + 1 < b.len() && &b[self.pos..self.pos + 2] != b"*/" {
                        self.pos += 1;
                    }
                    self.pos = (self.pos + 2).min(b.len());
                }
                continue;
            }
            break;
        }
        let c = *b.get(self.pos)?;
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || !c.is_ascii() {
            let start = self.pos;
            while b.get(self.pos).is_some_and(|c| {
                c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$' || !c.is_ascii()
            }) {
                self.pos += 1;
            }
            return Some(Token::Word(&self.input[start..self.pos]));
        }
        self.pos += 1;
        if matches!(c, b'\'' | b'"' | b'`') {
            while let Some(&next) = b.get(self.pos) {
                self.pos += 1;
                if next == b'\\' && c != b'`' && !(c == b'"' && self.ansi) && !self.no_escape {
                    self.pos = (self.pos + 1).min(b.len());
                } else if next == c {
                    if b.get(self.pos) == Some(&c) {
                        self.pos += 1;
                    } else {
                        break;
                    }
                }
            }
            Some(Token::Quoted)
        } else {
            Some(Token::Symbol(c))
        }
    }
}

fn word(token: Option<Token<'_>>, expected: &str) -> bool {
    matches!(token, Some(Token::Word(value)) if value.eq_ignore_ascii_case(expected))
}

fn is_program(tokens: &mut Tokens<'_>) -> bool {
    if !word(tokens.next(), "CREATE") {
        return false;
    }
    let mut next = tokens.next();
    if word(next, "DEFINER") {
        if !matches!(tokens.next(), Some(Token::Symbol(b'='))) {
            return false;
        }
        let account = tokens.next();
        if !matches!(account, Some(Token::Word(_) | Token::Quoted)) {
            return false;
        }
        next = tokens.next();
        if word(account, "CURRENT_USER") && matches!(next, Some(Token::Symbol(b'('))) {
            if !matches!(tokens.next(), Some(Token::Symbol(b')'))) {
                return false;
            }
            next = tokens.next();
        } else if matches!(next, Some(Token::Symbol(b'@'))) {
            if !matches!(
                tokens.next(),
                Some(Token::Word(_) | Token::Quoted | Token::Symbol(b'%'))
            ) {
                return false;
            }
            next = tokens.next();
            while matches!(next, Some(Token::Symbol(b'.' | b'-' | b'%'))) {
                next = tokens.next();
                if matches!(next, Some(Token::Word(_))) {
                    next = tokens.next();
                }
            }
        }
    }
    ["PROCEDURE", "FUNCTION", "TRIGGER", "EVENT"]
        .iter()
        .any(|kind| word(next, kind))
}

pub(super) fn first_keyword(input: &str) -> Option<String> {
    match Tokens::new(input, false, false).next() {
        Some(Token::Word(value)) => Some(value.to_ascii_uppercase()),
        _ => None,
    }
}

pub(super) fn is_dynamic(input: &str) -> bool {
    matches!(first_keyword(input).as_deref(), Some("CALL" | "EXECUTE"))
}

// 按会话 sql_mode 识别引号和注释中的分号；存储程序整体交给服务端解析，
// 因其主体可以包含分号，而 DELIMITER 是客户端指令，不属于服务端 SQL。
pub(super) fn next_statement<'a>(input: &'a str, mode: &str) -> Result<(&'a str, &'a str)> {
    let no_escape = mode
        .split(',')
        .any(|m| m.trim().eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"));
    let ansi = mode
        .split(',')
        .any(|m| m.trim().eq_ignore_ascii_case("ANSI_QUOTES"));
    if word(Tokens::new(input, no_escape, ansi).next(), "DELIMITER") {
        return Err(AppError::invalid("不支持 MySQL 客户端 DELIMITER 指令"));
    }
    if is_program(&mut Tokens::new(input, no_escape, ansi)) {
        return Ok((input, ""));
    }
    let b = input.as_bytes();
    let (mut i, mut quote, mut block, mut line) = (0usize, 0u8, false, false);
    while i < b.len() {
        let c = b[i];
        if line {
            if c == b'\n' || c == b'\r' {
                line = false;
            }
            i += 1;
            continue;
        }
        if block {
            if c == b'*' && b.get(i + 1) == Some(&b'/') {
                block = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if quote != 0 {
            if c == b'\\' && quote != b'`' && !(quote == b'"' && ansi) && !no_escape {
                i += 2;
                continue;
            }
            if c == quote {
                if b.get(i + 1) == Some(&quote) {
                    i += 2;
                    continue;
                }
                quote = 0;
            }
            i += 1;
            continue;
        }
        match c {
            b'\'' | b'"' | b'`' => quote = c,
            b'#' => line = true,
            b'-' if b.get(i + 1) == Some(&b'-')
                && b.get(i + 2)
                    .is_some_and(|x| x.is_ascii_whitespace() || x.is_ascii_control()) =>
            {
                line = true;
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                block = true;
                i += 1;
            }
            b';' => return Ok((&input[..i], &input[i + 1..])),
            _ => {}
        }
        i += 1;
    }
    if quote != 0 || block {
        return Err(AppError::invalid("SQL 引号或注释未闭合"));
    }
    Ok((input, ""))
}

// 解析成功不足以证明无副作用：同时限制语法和可调用名称，拒绝效果依赖
// 服务端状态的特性。无法证明安全时返回 false，由调用方按可能写入处理。
pub(super) fn readonly(input: &str) -> bool {
    if input.contains("/*")
        || input.contains("--")
        || input.contains('#')
        || input.contains('@')
        || input.contains(":=")
        || input.contains('\\')
    {
        return false;
    }
    let ast = match Parser::parse_sql(&MySqlDialect {}, input) {
        Ok(ast) if ast.len() == 1 => ast,
        _ => return false,
    };
    match &ast[0] {
        Statement::Query(_) => readonly_query(input),
        Statement::Explain { statement, .. }
            if matches!(statement.as_ref(), Statement::Query(_)) =>
        {
            explain_query(input)
        }
        Statement::ExplainTable {
            hive_format: None,
            has_table_keyword: false,
            table_name,
            ..
        } => safe_name(table_name, 2) && describe_target(input),
        Statement::ShowCreate {
            obj_type: ShowCreateObject::Table | ShowCreateObject::View,
            obj_name,
        } => safe_name(obj_name, 2),
        Statement::ShowTables {
            terse: false,
            history: false,
            external: false,
            show_options,
            ..
        } => safe_show_options(show_options, false, 1),
        Statement::ShowColumns { show_options, .. } => safe_show_options(show_options, true, 2),
        Statement::ShowDatabases {
            terse: false,
            history: false,
            show_options,
        }
        | Statement::ShowSchemas {
            terse: false,
            history: false,
            show_options,
        } => safe_show_options(show_options, false, 0),
        Statement::ShowVariables {
            filter,
            session,
            global,
        }
        | Statement::ShowStatus {
            filter,
            session,
            global,
        } => !(*session && *global) && safe_filter(filter.as_ref()),
        Statement::ShowCollation { filter } => safe_filter(filter.as_ref()),
        Statement::ShowCharset(charset) => safe_filter(charset.filter.as_ref()),
        Statement::ShowProcessList { .. } => true,
        _ => false,
    }
}

fn readonly_query(input: &str) -> bool {
    let b = input.as_bytes();
    let mut words: Vec<String> = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'\'' | b'"' | b'`' => {
                let q = b[i];
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        i += 1;
                        if b.get(i) == Some(&q) {
                            i += 1;
                        } else {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                // Quoted callable identifiers may refer to a stored function.
                let j = skip_space(b, i);
                if q == b'`' && b.get(j) == Some(&b'(') {
                    return false;
                }
            }
            b'#' => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-')
                && b.get(i + 2).is_some_and(|c| c.is_ascii_whitespace()) =>
            {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(b.len());
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let start = i;
                i += 1;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$')
                {
                    i += 1;
                }
                let word = input[start..i].to_ascii_uppercase();
                if matches!(
                    word.as_str(),
                    "INTO"
                        | "OUTFILE"
                        | "DUMPFILE"
                        | "FOR"
                        | "LOCK"
                        | "PROCEDURE"
                        | "FUNCTION"
                        | "SLEEP"
                        | "BENCHMARK"
                        | "GET_LOCK"
                        | "RELEASE_LOCK"
                        | "LOAD_FILE"
                        | "SQL_CALC_FOUND_ROWS"
                        | "UPDATE"
                        | "DELETE"
                        | "INSERT"
                        | "REPLACE"
                        | "SET"
                        | "CALL"
                        | "DO"
                ) {
                    return false;
                }
                let next = skip_space(b, i);
                if b.get(next) == Some(&b'(')
                    && (input[..start].trim_end().ends_with('.')
                        || !matches!(
                            word.as_str(),
                            "IN" | "EXISTS"
                                | "AS"
                                | "OVER"
                                | "PARTITION"
                                | "WITH"
                                | "SELECT"
                                | "FROM"
                                | "WHERE"
                                | "ON"
                                | "AND"
                                | "OR"
                                | "NOT"
                                | "COUNT"
                                | "SUM"
                                | "MIN"
                                | "MAX"
                                | "AVG"
                                | "ABS"
                                | "COALESCE"
                                | "IFNULL"
                                | "NULLIF"
                                | "LOWER"
                                | "UPPER"
                                | "LENGTH"
                                | "CHAR_LENGTH"
                                | "CONCAT"
                                | "SUBSTRING"
                                | "TRIM"
                                | "ROUND"
                                | "FLOOR"
                                | "CEIL"
                                | "CAST"
                                | "DATE"
                                | "YEAR"
                                | "MONTH"
                                | "DAY"
                                | "JSON_EXTRACT"
                                | "ROW_NUMBER"
                                | "RANK"
                                | "DENSE_RANK"
                        ))
                {
                    return false;
                }
                words.push(word);
            }
            _ => i += 1,
        }
    }
    matches!(words.first().map(String::as_str), Some("SELECT" | "WITH"))
}

fn safe_name(name: &ObjectName, max_parts: usize) -> bool {
    !name.0.is_empty() && name.0.len() <= max_parts && name.0.iter().all(|part| {
        matches!(part, ObjectNamePart::Identifier(ident) if !ident.value.is_empty() && matches!(ident.quote_style, None | Some('`')))
    })
}

fn safe_filter(filter: Option<&ShowStatementFilter>) -> bool {
    match filter {
        None | Some(ShowStatementFilter::Like(_)) => true,
        Some(ShowStatementFilter::Where(expr)) => readonly(&format!("SELECT 1 WHERE {expr}")),
        _ => false,
    }
}

fn safe_show_options(options: &ShowStatementOptions, required: bool, max_parts: usize) -> bool {
    if options.starts_with.is_some() || options.limit.is_some() || options.limit_from.is_some() {
        return false;
    }
    match &options.show_in {
        Some(scope)
            if max_parts > 0
                && scope.parent_type.is_none()
                && scope
                    .parent_name
                    .as_ref()
                    .is_some_and(|name| safe_name(name, max_parts)) => {}
        None if !required => {}
        _ => return false,
    }
    match &options.filter_position {
        None => true,
        Some(ShowStatementFilterPosition::Suffix(filter)) => safe_filter(Some(filter)),
        _ => false,
    }
}

// sqlparser 回退到表名目标时可能丢弃 EXPLAIN 选项。
// 同时核对原文和 AST，只允许单纯表名通过，不能把被丢弃的选项误判为只读描述。
fn describe_target(input: &str) -> bool {
    let Some((keyword, rest)) = take_word(input.trim_start()) else {
        return false;
    };
    if !matches!(
        keyword.to_ascii_uppercase().as_str(),
        "DESC" | "DESCRIBE" | "EXPLAIN"
    ) {
        return false;
    }
    let mut rest = rest.trim_start();
    for part in 0..2 {
        if let Some(quoted) = rest.strip_prefix('`') {
            let mut chars = quoted.char_indices().peekable();
            let mut end = None;
            while let Some((i, ch)) = chars.next() {
                if ch == '`' {
                    if chars.peek().is_some_and(|(_, next)| *next == '`') {
                        chars.next();
                    } else {
                        end = Some(i + 1);
                        break;
                    }
                }
            }
            let Some(end) = end else {
                return false;
            };
            if end == 1 {
                return false;
            }
            rest = &quoted[end..];
        } else {
            let len = rest
                .bytes()
                .take_while(|c| {
                    c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$' || !c.is_ascii()
                })
                .count();
            if len == 0 {
                return false;
            }
            rest = &rest[len..];
        }
        rest = rest.trim_start();
        if let Some(next) = rest.strip_prefix('.') {
            if part == 1 {
                return false;
            }
            rest = next.trim_start();
        } else {
            return rest.is_empty() || rest == ";";
        }
    }
    false
}

fn take_word(input: &str) -> Option<(&str, &str)> {
    let len = input
        .bytes()
        .take_while(|c| c.is_ascii_alphabetic() || *c == b'_')
        .count();
    (len > 0).then(|| input.split_at(len))
}

fn explain_query(input: &str) -> bool {
    let Some((keyword, rest)) = take_word(input.trim_start()) else {
        return false;
    };
    if !keyword.eq_ignore_ascii_case("EXPLAIN") {
        return false;
    }
    let mut rest = rest.trim_start();
    if let Some((word, next)) = take_word(rest)
        && word.eq_ignore_ascii_case("ANALYZE")
    {
        rest = next.trim_start();
    }
    if let Some((word, next)) = take_word(rest)
        && word.eq_ignore_ascii_case("FORMAT")
    {
        let Some(next) = next.trim_start().strip_prefix('=') else {
            return false;
        };
        let Some((format, next)) = take_word(next.trim_start()) else {
            return false;
        };
        if !matches!(
            format.to_ascii_uppercase().as_str(),
            "JSON" | "TREE" | "TRADITIONAL"
        ) {
            return false;
        }
        rest = next.trim_start();
    }
    readonly(rest)
}
fn skip_space(b: &[u8], mut i: usize) -> usize {
    while b.get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn quoted_delimiters_and_comments() {
        let (first, rest) = next_statement("SELECT '中;😀', `x;y` /* ; */; SELECT 2", "").unwrap();
        assert_eq!(first, "SELECT '中;😀', `x;y` /* ; */");
        assert_eq!(rest.trim(), "SELECT 2");
        assert!(next_statement("SELECT 'unterminated", "").is_err());
    }
    #[test]
    fn program_definitions_keep_the_full_input() {
        for sql in [
            "/* intro */ CREATE\nPROCEDURE p() BEGIN SELECT 1; SELECT 2; END;",
            "-- intro\nCREATE DEFINER=CURRENT_USER PROCEDURE p() BEGIN SELECT 1; END;",
            "# intro\rCREATE DEFINER = CURRENT_USER() FUNCTION f() RETURNS INT BEGIN RETURN 1; END;",
            "CREATE /*!50017 DEFINER=`root`@`localhost`*/ TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET @x=1; END;",
            "/*!50000 CREATE */ DEFINER='root'@'%' EVENT e ON SCHEDULE EVERY 1 DAY DO BEGIN SELECT 1; END;",
            "CREATE DEFINER = root @ db.example EVENT e ON SCHEDULE EVERY 1 DAY DO BEGIN SELECT 1; END;",
        ] {
            assert_eq!(next_statement(sql, "").unwrap(), (sql, ""), "{sql}");
        }
        let (first, rest) =
            next_statement("/* intro */ CREATE TABLE t (id INT); SELECT 2", "").unwrap();
        assert_eq!(first, "/* intro */ CREATE TABLE t (id INT)");
        assert_eq!(rest.trim(), "SELECT 2");
        assert!(next_statement("/* intro */ DELIMITER $$", "").is_err());
    }
    #[test]
    fn dynamic_detection_skips_comments_but_reads_executable_sql() {
        for sql in [
            "/* report */ CALL p()",
            "-- report\nexecute stmt",
            "# report\rCALL p()",
            "/*!50000 CALL p() */",
            "/* report */ /*!50000 EXECUTE stmt */",
        ] {
            assert!(is_dynamic(sql), "{sql}");
        }
        for sql in [
            "/* CALL p() */ SELECT 1",
            "/*!50000 SELECT 1 */ CALL p()",
            "CALLER p()",
            "'CALL'",
            "/* unfinished",
        ] {
            assert!(!is_dynamic(sql), "{sql}");
        }
        assert_eq!(
            first_keyword("/* report */ begin"),
            Some("BEGIN".to_owned())
        );
        assert_eq!(first_keyword("# only a comment"), None);
    }
    #[test]
    fn readonly_rejects_side_effects() {
        assert!(readonly("SELECT count(*) FROM t WHERE id = 1"));
        assert!(!readonly("SELECT secret_function()"));
        assert!(!readonly("SELECT 1 INTO OUTFILE '/tmp/a'"));
        assert!(!readonly("SELECT /*!50000 SLEEP(1) */ 1"));
        assert!(!readonly("SELECT @x := 1"));
    }
    #[test]
    fn readonly_metadata_whitelist() {
        for sql in [
            "SHOW DATABASES",
            "SHOW SCHEMAS LIKE 'app%'",
            "SHOW FULL TABLES FROM app",
            "SHOW COLUMNS FROM app.users",
            "SHOW CREATE TABLE app.users",
            "SHOW CREATE VIEW `app`.`users`",
            "SHOW SESSION VARIABLES LIKE 'sql_mode'",
            "SHOW STATUS",
            "SHOW COLLATION",
            "SHOW CHARACTER SET",
            "SHOW PROCESSLIST",
            "DESCRIBE app.users",
            "DESC `app`.`users`",
            "EXPLAIN app.users",
            "EXPLAIN SELECT count(*) FROM users",
            "EXPLAIN ANALYZE SELECT 1",
        ] {
            assert!(readonly(sql), "should allow: {sql}");
        }
        for sql in [
            "SHOW CREATE PROCEDURE p",
            "SHOW CREATE FUNCTION f",
            "SHOW CREATE TABLE f()",
            "SHOW TABLES FROM db LIMIT 1",
            "SHOW COLUMNS",
            "SHOW TABLES WHERE SLEEP(1)",
            "SHOW VARIABLES WHERE secret_function()",
            "SHOW VARIABLES WHERE @x := 1",
            "SHOW GRANTS FOR CURRENT_USER",
            "SHOW TABLES; DELETE FROM users",
            "SHOW TABLES /*!50000 INTO OUTFILE '/tmp/a' */",
            "DESCRIBE f()",
            "DESCRIBE app.users extra",
            "EXPLAIN ANALYZE users",
            "EXPLAIN UPDATE users SET x = 1",
            "EXPLAIN ANALYZE DELETE FROM users",
            "EXPLAIN SELECT secret_function()",
            "EXPLAIN ANALYZE SELECT SLEEP(1)",
            "EXPLAIN SELECT /*!50000 SLEEP(1) */ 1",
            "EXPLAIN SELECT 1; DELETE FROM users",
            "EXPLAIN SELECT 1 INTO OUTFILE '/tmp/a'",
        ] {
            assert!(!readonly(sql), "should reject: {sql}");
        }
    }
}
