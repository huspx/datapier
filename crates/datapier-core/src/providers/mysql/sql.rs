use crate::model::{AppError, Result};
use sqlparser::{
    ast::{
        Expr, ObjectName, ObjectNamePart, Query, Select, SetExpr, ShowCreateObject,
        ShowStatementFilter, ShowStatementFilterPosition, ShowStatementOptions, Statement,
        TableFactor, Visit, Visitor,
    },
    dialect::MySqlDialect,
    parser::Parser,
};
use std::ops::ControlFlow;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransactionControl {
    Begin,
    Commit,
    Rollback,
    AutocommitOn,
}

/// 仅识别完整的单条事务控制语句，批次和注释均不能走快速通道。
pub fn transaction_control(text: &str) -> Option<TransactionControl> {
    let text = text.trim();
    let text = text.strip_suffix(';').unwrap_or(text).trim();
    if let Some((left, right)) = text.split_once('=') {
        let mut words = left.split_ascii_whitespace();
        let set = words
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("SET"));
        let variable = words.next();
        let session = variable.is_some_and(|word| {
            word.eq_ignore_ascii_case("SESSION") || word.eq_ignore_ascii_case("LOCAL")
        });
        let variable = if session { words.next() } else { variable };
        if set
            && words.next().is_none()
            && variable.is_some_and(|word| {
                word.eq_ignore_ascii_case("autocommit")
                    || (!session
                        && (word.eq_ignore_ascii_case("@@autocommit")
                            || word.eq_ignore_ascii_case("@@session.autocommit")
                            || word.eq_ignore_ascii_case("@@local.autocommit")))
            })
            && (right.trim().eq_ignore_ascii_case("ON") || right.trim() == "1")
        {
            return Some(TransactionControl::AutocommitOn);
        }
        return None;
    }
    let mut words = text.split_ascii_whitespace();
    let first = words.next()?;
    let second = words.next();
    if words.next().is_some() {
        return None;
    }
    let work = second.is_none_or(|word| word.eq_ignore_ascii_case("WORK"));
    if (first.eq_ignore_ascii_case("BEGIN") && work)
        || (first.eq_ignore_ascii_case("START")
            && second.is_some_and(|word| word.eq_ignore_ascii_case("TRANSACTION")))
    {
        Some(TransactionControl::Begin)
    } else if first.eq_ignore_ascii_case("COMMIT") && work {
        Some(TransactionControl::Commit)
    } else if first.eq_ignore_ascii_case("ROLLBACK") && work {
        Some(TransactionControl::Rollback)
    } else {
        None
    }
}

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

// 诊断语句的预准备可能改写当前会话的诊断区，必须直接使用文本协议。
pub(super) fn is_diagnostic(input: &str) -> bool {
    let mut tokens = Tokens::new(input, false, false);
    if !word(tokens.next(), "SHOW") {
        return false;
    }
    let mut next = tokens.next();
    if word(next, "COUNT") {
        if !matches!(tokens.next(), Some(Token::Symbol(b'(')))
            || !matches!(tokens.next(), Some(Token::Symbol(b'*')))
            || !matches!(tokens.next(), Some(Token::Symbol(b')')))
        {
            return false;
        }
        next = tokens.next();
    }
    word(next, "WARNINGS") || word(next, "ERRORS")
}

// 诊断语句无需依赖 sql_mode 切分；含引号或反斜线时退回普通的会话模式解析。
pub(super) fn diagnostic_statement(input: &str) -> Option<(&str, &str)> {
    if !is_diagnostic(input) {
        return None;
    }
    let (statement, rest) = next_statement(input, "").ok()?;
    let mut tokens = Tokens::new(statement, false, false);
    while let Some(token) = tokens.next() {
        if matches!(token, Token::Quoted | Token::Symbol(b'\\')) {
            return None;
        }
    }
    Some((statement, rest))
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

// 只在字符串或反引号之外识别注释、变量和模式敏感的双引号。
// 反斜线始终拒绝，避免 NO_BACKSLASH_ESCAPES 对分词的影响。
fn stateless_lexically_safe(text: &str) -> bool {
    let bytes = text.as_bytes();
    let (mut i, mut quote) = (0, 0);
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' {
            return false;
        }
        if quote != 0 {
            if c == quote {
                if bytes.get(i + 1) == Some(&quote) {
                    i += 2;
                    continue;
                }
                quote = 0;
            }
        } else {
            match c {
                b'\'' | b'`' => quote = c,
                b'"' | b'#' | b'@' => return false,
                b'-' if bytes.get(i + 1) == Some(&b'-') => return false,
                b'/' if bytes.get(i + 1) == Some(&b'*') => return false,
                b':' if bytes.get(i + 1) == Some(&b'=') => return false,
                _ => {}
            }
        }
        i += 1;
    }
    quote == 0
}

/// 按与 MySQL sql_mode 无关的共同子集返回语句的 UTF-8 字节范围。
pub fn stateless_statement_ranges(text: &str) -> Result<Vec<(usize, usize)>> {
    if !stateless_lexically_safe(text) {
        return Err(AppError::invalid(
            "无状态 SQL 不支持模式敏感的引号、转义或注释",
        ));
    }
    let mut ranges = Vec::new();
    let mut offset = 0;
    while offset < text.len() {
        if is_program(&mut Tokens::new(&text[offset..], true, false)) {
            return Err(AppError::invalid("无状态 SQL 不支持存储程序定义"));
        }
        let (part, rest) = next_statement(&text[offset..], "")?;
        let trimmed = part.trim();
        if !trimmed.is_empty() {
            let start = offset + part.len() - part.trim_start().len();
            ranges.push((start, start + trimmed.len()));
        }
        if rest.is_empty() {
            break;
        }
        offset = text.len() - rest.len();
    }
    Ok(ranges)
}

/// 返回无状态语句原文，拒绝依赖会话模式的输入。
pub fn stateless_statements(text: &str) -> Result<Vec<String>> {
    Ok(stateless_statement_ranges(text)?
        .into_iter()
        .map(|(start, end)| text[start..end].to_owned())
        .collect())
}

/// 验证整批语句均属于可证明只读的 SQL 子集。
pub fn validate_readonly(text: &str) -> Result<()> {
    let statements = stateless_statements(text)?;
    if statements.is_empty() || statements.iter().any(|statement| !readonly(statement)) {
        return Err(AppError::new(
            "permission_denied",
            "SQL 不属于允许的只读语句",
        ));
    }
    Ok(())
}

fn readonly_function(name: &str) -> bool {
    [
        "COUNT",
        "SUM",
        "MIN",
        "MAX",
        "AVG",
        "ABS",
        "COALESCE",
        "IFNULL",
        "NULLIF",
        "LOWER",
        "UPPER",
        "LENGTH",
        "CHAR_LENGTH",
        "CONCAT",
        "SUBSTRING",
        "TRIM",
        "ROUND",
        "FLOOR",
        "CEIL",
        "DATE",
        "YEAR",
        "MONTH",
        "DAY",
        "JSON_EXTRACT",
        "ROW_NUMBER",
        "RANK",
        "DENSE_RANK",
    ]
    .iter()
    .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

// 未知 IGNORE_SPACE 时，COUNT (1)、TRIM (1) 等可能绑定到同名存储函数。
// 保留原 SQL，只接受紧邻左括号的内置调用；字符串与反引号由现有词法器跳过。
fn unambiguous_calls(input: &str) -> bool {
    let mut tokens = Tokens::new(input, true, false);
    while let Some(token) = tokens.next() {
        let Token::Word(name) = token else { continue };
        let rest = &input[tokens.pos..];
        if rest.as_bytes().first().is_some_and(u8::is_ascii_whitespace)
            && rest.trim_start().starts_with('(')
            && (readonly_function(name)
                || ["CAST", "CONVERT", "EXTRACT"]
                    .iter()
                    .any(|candidate| name.eq_ignore_ascii_case(candidate)))
        {
            return false;
        }
    }
    true
}

// 解析成功不足以证明无副作用：同时限制语法和可调用名称，拒绝效果依赖
// 服务端状态的特性。无法证明安全时返回 false，由调用方按可能写入处理。
pub(super) fn readonly(input: &str) -> bool {
    if !stateless_lexically_safe(input) || !unambiguous_calls(input) {
        return false;
    }
    // sqlparser 0.62 尚不解析 MySQL SHOW INDEX；仅允许受控表名形式。
    if let Some((show, rest)) = take_word(input.trim_start())
        && show.eq_ignore_ascii_case("SHOW")
        && let Some((index, rest)) = take_word(rest.trim_start())
        && matches!(
            index.to_ascii_uppercase().as_str(),
            "INDEX" | "INDEXES" | "KEYS"
        )
        && let Some((from, rest)) = take_word(rest.trim_start())
        && (from.eq_ignore_ascii_case("FROM") || from.eq_ignore_ascii_case("IN"))
    {
        return describe_target(&format!("DESCRIBE {}", rest.trim_start()));
    }
    let ast = match Parser::parse_sql(&MySqlDialect {}, input) {
        Ok(ast) if ast.len() == 1 => ast,
        _ => return false,
    };
    match &ast[0] {
        Statement::Query(query) => readonly_query(query),
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

fn readonly_query(query: &Query) -> bool {
    query.visit(&mut ReadonlyVisitor).is_continue()
}

fn readonly_set(set: &SetExpr) -> bool {
    match set {
        SetExpr::Select(_) | SetExpr::Query(_) => true,
        SetExpr::SetOperation { left, right, .. } => readonly_set(left) && readonly_set(right),
        _ => false,
    }
}

struct ReadonlyVisitor;
impl Visitor for ReadonlyVisitor {
    type Break = ();

    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
        if readonly_set(&query.body)
            && query.locks.is_empty()
            && query.for_clause.is_none()
            && query.settings.is_none()
            && query.format_clause.is_none()
            && query.pipe_operators.is_empty()
        {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        }
    }

    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<()> {
        if select.into.is_none()
            && select.optimizer_hints.is_empty()
            && select.select_modifiers.is_none()
        {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        }
    }

    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<()> {
        match factor {
            TableFactor::Table { args: None, .. }
            | TableFactor::Derived { .. }
            | TableFactor::NestedJoin { .. } => ControlFlow::Continue(()),
            _ => ControlFlow::Break(()),
        }
    }

    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        let safe = match expr {
            Expr::Function(function) => matches!(function.name.0.as_slice(),
                [ObjectNamePart::Identifier(name)] if name.quote_style.is_none() && readonly_function(&name.value)),
            Expr::Identifier(_)
            | Expr::CompoundIdentifier(_)
            | Expr::Value(_)
            | Expr::Nested(_)
            | Expr::BinaryOp { .. }
            | Expr::UnaryOp { .. }
            | Expr::IsFalse(_)
            | Expr::IsNotFalse(_)
            | Expr::IsTrue(_)
            | Expr::IsNotTrue(_)
            | Expr::IsNull(_)
            | Expr::IsNotNull(_)
            | Expr::IsUnknown(_)
            | Expr::IsNotUnknown(_)
            | Expr::IsDistinctFrom(_, _)
            | Expr::IsNotDistinctFrom(_, _)
            | Expr::InList { .. }
            | Expr::InSubquery { .. }
            | Expr::Between { .. }
            | Expr::Like { .. }
            | Expr::RLike { .. }
            | Expr::Cast { .. }
            | Expr::Convert { .. }
            | Expr::Extract { .. }
            | Expr::Ceil { .. }
            | Expr::Floor { .. }
            | Expr::Substring { .. }
            | Expr::Trim { .. }
            | Expr::Collate { .. }
            | Expr::Case { .. }
            | Expr::Exists { .. }
            | Expr::Subquery(_)
            | Expr::Tuple(_)
            | Expr::Wildcard(_)
            | Expr::QualifiedWildcard(_, _) => true,
            _ => false,
        };
        if safe {
            ControlFlow::Continue(())
        } else {
            ControlFlow::Break(())
        }
    }
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
    if let Some((word, _)) = take_word(rest)
        && word.eq_ignore_ascii_case("ANALYZE")
    {
        return false;
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

#[cfg(test)]
#[path = "../../../tests/unit/mysql/sql.rs"]
mod tests;
