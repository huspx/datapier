use super::classify::{
    classify, constant_view_stub, create_columns, ddl_identity, function_calls, is, name_parts,
    parse_insert, static_references, words,
};
use super::literal::tuple_values;
use super::*;

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

pub(super) struct ScanSettings<'a> {
    pub(super) encoding: &'static Encoding,
    pub(super) bom: usize,
    pub(super) fixed: bool,
    pub(super) cancel: Option<tokio_util::sync::CancellationToken>,
    pub(super) statement: Option<&'a SqlStatement>,
}

pub(super) fn scan<F>(
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
