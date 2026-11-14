//! MySQL 直接对象范围证明。视图、触发器与外键的间接依赖必须由调用方另行检查。
use std::{collections::BTreeSet, ops::ControlFlow};

use crate::model::{AppError, Result};
use serde::{Deserialize, Serialize};
use sqlparser::{
    ast::{
        AlterTableOperation, AssignmentTarget, ColumnOption, Expr, FromTable, ObjectName,
        ObjectNamePart, ObjectType, Query, Select, SetExpr, Statement, TableConstraint,
        TableFactor, TableObject, Use, Visit, Visitor,
    },
    dialect::MySqlDialect,
    parser::Parser,
};

/// 执行连接的当前数据库及实际 SQL 模式。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SqlContext {
    pub database: Option<String>,
    pub no_backslash_escapes: bool,
    pub ansi_quotes: bool,
    pub ignore_space: bool,
    pub lower_case_table_names: u8,
}

/// 一个必须进一步检查间接依赖的直接对象。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ObjectAccess {
    pub database: String,
    pub name: String,
    pub write: bool,
}

/// 整个批次的直接访问范围；这不是间接对象依赖的完整证明。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SqlAccess {
    pub read_databases: BTreeSet<String>,
    pub write_databases: BTreeSet<String>,
    pub objects: Vec<ObjectAccess>,
    pub replayable: bool,
    pub dangerous: bool,
    pub change_database: Option<String>,
    /// 语句顺序中的事务开始、结束和隐式提交边界。
    pub transaction_actions: Vec<String>,
}

fn denied() -> AppError {
    AppError::new("permission_denied", "无法证明 SQL 的访问范围")
}

// sqlparser 的 MySQL dialect 固定使用反斜线转义且不识别 ANSI 双引号。
// 拒绝这些存在模式分歧的输入，也拒绝可执行注释及变量，避免解析器遗漏服务端代码。
fn lexical_safe(text: &str, view_ddl: bool) -> bool {
    let b = text.as_bytes();
    let (mut i, mut quote, mut view_seen) = (0, 0, false);
    while i < b.len() {
        let c = b[i];
        if c == b'\\' || c == b'"' {
            return false;
        }
        if quote != 0 {
            if c == quote {
                if b.get(i + 1) == Some(&quote) {
                    i += 2;
                    continue;
                }
                quote = 0;
            }
        } else {
            if c.is_ascii_alphabetic() || c == b'_' {
                let start = i;
                while b
                    .get(i)
                    .is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'$')
                {
                    i += 1;
                }
                if b[start..i].eq_ignore_ascii_case(b"VIEW") {
                    view_seen = true;
                }
                continue;
            }
            match c {
                b'\'' | b'`' => quote = c,
                b'@' if !view_ddl || view_seen => return false,
                b'#' => return false,
                b'-' if b.get(i + 1) == Some(&b'-') => return false,
                b'/' if b.get(i + 1) == Some(&b'*') => return false,
                b':' if b.get(i + 1) == Some(&b'=') => return false,
                _ => {}
            }
        }
        i += 1;
    }
    quote == 0
}

fn ident(part: &ObjectNamePart) -> Result<&str> {
    match part {
        ObjectNamePart::Identifier(i)
            if !i.value.is_empty()
                && matches!(i.quote_style, None | Some('`'))
                && !i.value.starts_with('@') =>
        {
            Ok(&i.value)
        }
        _ => Err(denied()),
    }
}
fn db_name(value: &str, mode: u8) -> String {
    if mode == 0 {
        value.to_owned()
    } else {
        value.to_lowercase()
    }
}
fn system_db(db: &str) -> bool {
    ["information_schema", "performance_schema", "mysql", "sys"]
        .iter()
        .any(|s| db.eq_ignore_ascii_case(s))
}

struct Scope {
    visible: BTreeSet<String>,
    definitions: Vec<(usize, BTreeSet<String>)>,
}
struct Analyzer<'a> {
    context: &'a SqlContext,
    database: Option<String>,
    access: SqlAccess,
    scopes: Vec<Scope>,
}
impl Analyzer<'_> {
    fn object(&mut self, name: &ObjectName, write: bool) -> Result<()> {
        let (db, table) = match name.0.as_slice() {
            [table] => (self.database.as_deref().ok_or_else(denied)?, ident(table)?),
            [db, table] => (ident(db)?, ident(table)?),
            _ => return Err(denied()),
        };
        if system_db(db) {
            return Err(denied());
        }
        let database = db_name(db, self.context.lower_case_table_names);
        let name = db_name(table, self.context.lower_case_table_names);
        if write {
            self.access.write_databases.insert(database.clone());
        } else {
            self.access.read_databases.insert(database.clone());
        }
        let object = ObjectAccess {
            database,
            name,
            write,
        };
        if !self.access.objects.contains(&object) {
            self.access.objects.push(object);
        }
        Ok(())
    }
    fn visit<T: Visit>(&mut self, node: &T) -> Result<()> {
        match node.visit(self) {
            ControlFlow::Continue(()) => Ok(()),
            ControlFlow::Break(()) => Err(denied()),
        }
    }
    fn statement(&mut self, stmt: &Statement) -> Result<()> {
        if matches!(
            stmt,
            Statement::CreateTable(_)
                | Statement::AlterTable(_)
                | Statement::Truncate(_)
                | Statement::CreateIndex(_)
                | Statement::Drop { .. }
        ) {
            self.access
                .transaction_actions
                .push("implicit_commit".into());
        }
        match stmt {
            Statement::Query(q) => self.visit(q),
            Statement::StartTransaction {
                statements,
                exception: None,
                has_end_keyword: false,
                modifier: None,
                ..
            } if statements.is_empty() => {
                self.access.transaction_actions.push("begin".into());
                Ok(())
            }
            Statement::Commit {
                chain: false,
                end: false,
                modifier: None,
            } => {
                self.access.transaction_actions.push("commit".into());
                Ok(())
            }
            Statement::Rollback {
                chain: false,
                savepoint: None,
            } => {
                self.access.transaction_actions.push("rollback".into());
                Ok(())
            }
            Statement::Use(Use::Object(name)) => {
                let [part] = name.0.as_slice() else {
                    return Err(denied());
                };
                let db = db_name(ident(part)?, self.context.lower_case_table_names);
                if system_db(&db) {
                    return Err(denied());
                }
                self.access.read_databases.insert(db.clone());
                self.database = Some(db.clone());
                self.access.change_database = Some(db);
                Ok(())
            }
            Statement::Insert(insert) => {
                let TableObject::TableName(target) = &insert.table else {
                    return Err(denied());
                };
                if !insert.optimizer_hints.is_empty()
                    || insert.partitioned.is_some()
                    || insert.settings.is_some()
                    || insert.format_clause.is_some()
                    || insert.multi_table_insert_type.is_some()
                {
                    return Err(denied());
                }
                self.object(target, true)?;
                self.access.dangerous |=
                    insert.replace_into || insert.overwrite || insert.on.is_some();
                self.visit(insert)
            }
            Statement::Update(update) => {
                if update.from.is_some() || !update.optimizer_hints.is_empty() {
                    return Err(denied());
                }
                let mut factors = vec![&update.table.relation];
                factors.extend(update.table.joins.iter().map(|j| &j.relation));
                let mut targets = Vec::new();
                for factor in factors {
                    let TableFactor::Table {
                        name,
                        alias,
                        args: None,
                        ..
                    } = factor
                    else {
                        return Err(denied());
                    };
                    let alias = match alias {
                        Some(a) => a.name.value.clone(),
                        None => ident(name.0.last().ok_or_else(denied)?)?.to_owned(),
                    };
                    targets.push((alias, name));
                }
                for assignment in &update.assignments {
                    let AssignmentTarget::ColumnName(column) = &assignment.target else {
                        return Err(denied());
                    };
                    let target = match column.0.as_slice() {
                        [_] if targets.len() == 1 => targets[0].1,
                        [alias, _] => {
                            let alias = ident(alias)?;
                            let matches: Vec<_> = targets
                                .iter()
                                .filter(|(a, _)| a.eq_ignore_ascii_case(alias))
                                .collect();
                            if matches.len() != 1 {
                                return Err(denied());
                            }
                            matches[0].1
                        }
                        _ => return Err(denied()),
                    };
                    self.object(target, true)?;
                }
                if update.assignments.is_empty() {
                    return Err(denied());
                }
                // 无法可靠证明条件不是恒真，写入均进入危险审批。
                self.access.dangerous = true;
                self.visit(update)
            }
            Statement::Delete(delete) => {
                if !delete.optimizer_hints.is_empty() {
                    return Err(denied());
                }
                let from = match &delete.from {
                    FromTable::WithFromKeyword(t) => t,
                    _ => return Err(denied()),
                };
                let sources = delete.using.as_ref().unwrap_or(from);
                let mut targets = Vec::new();
                for group in sources {
                    for factor in std::iter::once(&group.relation)
                        .chain(group.joins.iter().map(|j| &j.relation))
                    {
                        let TableFactor::Table {
                            name,
                            alias,
                            args: None,
                            ..
                        } = factor
                        else {
                            return Err(denied());
                        };
                        let alias = match alias {
                            Some(a) => a.name.value.clone(),
                            None => ident(name.0.last().ok_or_else(denied)?)?.to_owned(),
                        };
                        targets.push((alias, name));
                    }
                }
                if delete.tables.is_empty() {
                    if targets.len() != 1 {
                        return Err(denied());
                    }
                    self.object(targets[0].1, true)?;
                } else {
                    for target in &delete.tables {
                        let [part] = target.0.as_slice() else {
                            return Err(denied());
                        };
                        let name = ident(part)?;
                        let found: Vec<_> = targets
                            .iter()
                            .filter(|(a, _)| a.eq_ignore_ascii_case(name))
                            .collect();
                        if found.len() != 1 {
                            return Err(denied());
                        }
                        self.object(found[0].1, true)?;
                    }
                }
                self.access.dangerous = true;
                self.visit(delete)
            }
            Statement::CreateTable(table) => {
                if table.like.is_some()
                    || table.clone.is_some()
                    || table.temporary
                    || table.external
                    || table.dynamic
                    || table.or_replace
                    || table
                        .constraints
                        .iter()
                        .any(|c| matches!(c, TableConstraint::ForeignKey(_)))
                    || table
                        .columns
                        .iter()
                        .flat_map(|c| &c.options)
                        .any(|o| matches!(o.option, ColumnOption::ForeignKey(_)))
                {
                    return Err(denied());
                }
                self.object(&table.name, true)?;
                self.access.dangerous = true;
                self.visit(table)
            }
            Statement::AlterTable(table) => {
                for operation in &table.operations {
                    match operation {
                        AlterTableOperation::AddColumn { column_def, .. }
                            if !column_def
                                .options
                                .iter()
                                .any(|o| matches!(o.option, ColumnOption::ForeignKey(_))) => {}
                        AlterTableOperation::DropColumn { .. } => (),
                        _ => return Err(denied()),
                    }
                }
                self.object(&table.name, true)?;
                self.access.dangerous = true;
                self.visit(table)
            }
            Statement::Truncate(truncate) => {
                for table in &truncate.table_names {
                    self.object(&table.name, true)?;
                }
                self.access.dangerous = true;
                self.visit(truncate)
            }
            Statement::CreateIndex(index) => {
                self.object(&index.table_name, true)?;
                self.access.dangerous = true;
                self.visit(index)
            }
            Statement::Drop {
                object_type: ObjectType::Table | ObjectType::View,
                names,
                table: None,
                ..
            } => {
                for name in names {
                    self.object(name, true)?;
                }
                self.access.dangerous = true;
                Ok(())
            }
            Statement::Explain {
                statement,
                analyze: false,
                options: None,
                ..
            } => {
                if !matches!(
                    statement.as_ref(),
                    Statement::Query(_)
                        | Statement::Insert(_)
                        | Statement::Update(_)
                        | Statement::Delete(_)
                ) {
                    return Err(denied());
                }
                self.statement(statement)
            }
            _ => Err(denied()),
        }
    }
}
fn safe_set(set: &SetExpr) -> bool {
    match set {
        SetExpr::Select(_) | SetExpr::Query(_) | SetExpr::Values(_) => true,
        SetExpr::SetOperation { left, right, .. } => safe_set(left) && safe_set(right),
        _ => false,
    }
}
impl Visitor for Analyzer<'_> {
    type Break = ();
    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<()> {
        if !safe_set(&query.body)
            || query.for_clause.is_some()
            || query.settings.is_some()
            || query.format_clause.is_some()
            || !query.pipe_operators.is_empty()
        {
            return ControlFlow::Break(());
        }
        if !query.locks.is_empty() {
            self.access.replayable = false;
        }
        let ptr = query as *const Query as usize;
        let mut visible = self
            .scopes
            .last()
            .map(|s| {
                s.definitions
                    .iter()
                    .find(|(p, _)| *p == ptr)
                    .map(|(_, v)| v.clone())
                    .unwrap_or_else(|| s.visible.clone())
            })
            .unwrap_or_default();
        let mut definitions = Vec::new();
        if let Some(with) = &query.with {
            for cte in &with.cte_tables {
                if cte.from.is_some() || cte.materialized.is_some() {
                    return ControlFlow::Break(());
                }
                let name = cte.alias.name.value.to_lowercase();
                let mut in_definition = visible.clone();
                if with.recursive {
                    in_definition.insert(name.clone());
                }
                definitions.push((cte.query.as_ref() as *const Query as usize, in_definition));
                visible.insert(name);
            }
        }
        self.scopes.push(Scope {
            visible,
            definitions,
        });
        ControlFlow::Continue(())
    }
    fn post_visit_query(&mut self, _: &Query) -> ControlFlow<()> {
        self.scopes.pop();
        ControlFlow::Continue(())
    }
    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<()> {
        if select.into.is_some()
            || !select.optimizer_hints.is_empty()
            || select.select_modifiers.is_some()
        {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    }
    fn pre_visit_table_factor(&mut self, factor: &TableFactor) -> ControlFlow<()> {
        match factor {
            TableFactor::Table {
                name,
                args: None,
                with_hints,
                version: None,
                with_ordinality: false,
                json_path: None,
                sample: None,
                ..
            } if with_hints.is_empty() => {
                if let [ObjectNamePart::Identifier(i)] = name.0.as_slice()
                    && self
                        .scopes
                        .last()
                        .is_some_and(|s| s.visible.contains(&i.value.to_lowercase()))
                {
                    return ControlFlow::Continue(());
                }
                if self.object(name, false).is_err() {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            }
            TableFactor::Derived { sample: None, .. } | TableFactor::NestedJoin { .. } => {
                ControlFlow::Continue(())
            }
            _ => ControlFlow::Break(()),
        }
    }
    fn pre_visit_expr(&mut self, expr: &Expr) -> ControlFlow<()> {
        let safe = match expr {
            Expr::Function(f) => match f.name.0.as_slice() {
                [ObjectNamePart::Identifier(name)] if name.quote_style.is_none() => {
                    let name = name.value.to_ascii_uppercase();
                    if [
                        "RAND",
                        "UUID",
                        "NOW",
                        "CURRENT_TIMESTAMP",
                        "SYSDATE",
                        "CURRENT_DATE",
                        "CURRENT_TIME",
                    ]
                    .contains(&name.as_str())
                    {
                        self.access.replayable = false;
                        true
                    } else {
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
                        .contains(&name.as_str())
                    }
                }
                _ => false,
            },
            Expr::Identifier(i) => {
                if i.quote_style.is_none()
                    && [
                        "CURRENT_TIMESTAMP",
                        "CURRENT_DATE",
                        "CURRENT_TIME",
                        "LOCALTIME",
                        "LOCALTIMESTAMP",
                    ]
                    .iter()
                    .any(|name| i.value.eq_ignore_ascii_case(name))
                {
                    self.access.replayable = false;
                }
                !i.value.starts_with('@')
            }
            Expr::CompoundIdentifier(ids) => ids.iter().all(|i| !i.value.starts_with('@')),
            Expr::Value(_)
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

fn validate_text(text: &str, context: &SqlContext, view_ddl: bool) -> Result<()> {
    if context.lower_case_table_names > 2 || !lexical_safe(text, view_ddl) {
        return Err(denied());
    }
    // 函数名与括号之间有空格时，IGNORE_SPACE 关闭的服务器可能解析为存储函数。
    if !context.ignore_space {
        let bytes = text.as_bytes();
        for (i, c) in bytes.iter().enumerate() {
            if *c == b'(' && i > 0 && bytes[i - 1].is_ascii_whitespace() {
                let before = text[..i].trim_end();
                let name = before
                    .rsplit(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .next()
                    .unwrap_or("");
                if [
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
                    "RAND",
                    "UUID",
                    "NOW",
                    "SYSDATE",
                ]
                .iter()
                .any(|x| name.eq_ignore_ascii_case(x))
                {
                    return Err(denied());
                }
            }
        }
    }
    Ok(())
}

/// 在发送前分析一批 MySQL 语句；无法证明范围时拒绝整批。
pub fn analyze_mysql(text: &str, context: &SqlContext) -> Result<SqlAccess> {
    validate_text(text, context, false)?;
    let statements = Parser::parse_sql(&MySqlDialect {}, text).map_err(|_| denied())?;
    if statements.is_empty() {
        return Err(denied());
    }
    let mut analyzer = Analyzer {
        context,
        database: context.database.clone(),
        access: SqlAccess {
            read_databases: BTreeSet::new(),
            write_databases: BTreeSet::new(),
            objects: Vec::new(),
            replayable: statements.len() == 1 && matches!(&statements[0], Statement::Query(_)),
            dangerous: false,
            change_database: None,
            transaction_actions: Vec::new(),
        },
        scopes: Vec::new(),
    };
    for statement in &statements {
        analyzer.statement(statement)?;
    }
    Ok(analyzer.access)
}

/// 分析 SHOW CREATE VIEW 返回的单条定义，只返回查询本身的直接读取依赖。
/// 视图的间接依赖仍须由调用方递归检查，无法证明的定义会被拒绝。
pub fn analyze_view_ddl(text: &str, context: &SqlContext) -> Result<SqlAccess> {
    // `@` 仅允许出现在 CREATE VIEW 的头部，供 MySQL DEFINER 的账号使用；
    // VIEW 之后仍禁止用户变量，整个输入必须被解析成唯一的 CreateView。
    validate_text(text, context, true)?;
    let statements = Parser::parse_sql(&MySqlDialect {}, text).map_err(|_| denied())?;
    let [Statement::CreateView(view)] = statements.as_slice() else {
        return Err(denied());
    };
    if view.materialized || view.temporary || view.to.is_some() || view.or_alter {
        return Err(denied());
    }
    let mut analyzer = Analyzer {
        context,
        database: context.database.clone(),
        access: SqlAccess {
            read_databases: BTreeSet::new(),
            write_databases: BTreeSet::new(),
            objects: Vec::new(),
            replayable: true,
            dangerous: false,
            change_database: None,
            transaction_actions: Vec::new(),
        },
        scopes: Vec::new(),
    };
    analyzer.visit(view.query.as_ref())?;
    Ok(analyzer.access)
}
