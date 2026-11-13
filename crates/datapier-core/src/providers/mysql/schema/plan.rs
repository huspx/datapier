//! MySQL 结构计划与执行；预览和执行使用同一冻结 SQL。
use super::super::quote_ident;
use crate::model::schema::*;
use crate::model::{AppError, Result};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

fn ident(value: &str) -> Result<String> {
    quote_ident(value)
}
pub(super) fn qualified(db: &str, name: &str) -> Result<String> {
    Ok(format!("{}.{}", ident(db)?, ident(name)?))
}
pub(super) fn literal(value: &str, mode: &str) -> String {
    let s = if mode
        .split(',')
        .any(|s| s.trim().eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"))
    {
        value.to_owned()
    } else {
        value.replace('\\', "\\\\")
    };
    format!("'{}'", s.replace('\'', "''"))
}
// 表达式来自显式 SQL 输入；限制语句边界，不能通过表单拼入第二条语句。
fn expression(value: &str) -> Result<&str> {
    let value = value.trim();
    if value.is_empty()
        || value.contains(';')
        || value.contains("--")
        || value.contains("/*")
        || value.contains('#')
    {
        return Err(AppError::invalid("SQL 表达式为空或包含语句边界／注释"));
    }
    Ok(value)
}
fn keyword(value: &str) -> Result<&str> {
    if value.is_empty()
        || !value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_')
    {
        return Err(AppError::invalid("无效 SQL 关键字"));
    }
    Ok(value)
}
// 类型文本不是 SQL 表达式；禁止在括号外追加字段属性或 ALTER 子句。
fn native_type(value: &str) -> Result<&str> {
    let value = value.trim();
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    let mut outside = String::new();
    for c in value.chars() {
        if quoted {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '\'' {
                quoted = false;
            }
        } else {
            match c {
                '\'' if depth > 0 => quoted = true,
                '(' if depth == 0 => {
                    depth = 1;
                    outside.push(' ');
                }
                ')' if depth == 1 => depth = 0,
                ',' if depth == 1 => (),
                c if depth == 1 && (c.is_ascii_digit() || c.is_ascii_whitespace()) => (),
                c if depth == 0
                    && (c.is_ascii_alphanumeric() || c == '_' || c.is_ascii_whitespace()) =>
                {
                    outside.push(c)
                }
                _ => return Err(AppError::invalid("字段类型包含无效字符")),
            }
        }
    }
    let words: Vec<_> = outside
        .split_whitespace()
        .map(str::to_ascii_uppercase)
        .collect();
    if depth != 0
        || quoted
        || words.is_empty()
        || words.len() > 3
        || words
            .iter()
            .skip(1)
            .any(|w| w != "UNSIGNED" && w != "ZEROFILL" && w != "PRECISION")
    {
        return Err(AppError::invalid("字段类型格式无效"));
    }
    Ok(value)
}
fn column(c: &ColumnDefinition, mode: &str) -> Result<String> {
    let mut s = format!("{} {}", ident(&c.name)?, native_type(&c.native_type)?);
    if let Some(v) = &c.charset {
        s += &format!(" CHARACTER SET {}", ident(v)?);
    }
    if let Some(v) = &c.collation {
        s += &format!(" COLLATE {}", ident(v)?);
    }
    if let Some(g) = &c.generated {
        s += &format!(
            " GENERATED ALWAYS AS ({}) {}",
            expression(&g.expression)?,
            if g.stored { "STORED" } else { "VIRTUAL" }
        );
    }
    s += if c.nullable { " NULL" } else { " NOT NULL" };
    match &c.default {
        ColumnDefault::Unspecified => (),
        ColumnDefault::Null => s += " DEFAULT NULL",
        ColumnDefault::Literal(v) => s += &format!(" DEFAULT {}", literal(v, mode)),
        ColumnDefault::Expression(v) => s += &format!(" DEFAULT {}", expression(v)?),
    }
    if let Some(v) = &c.on_update {
        s += &format!(" ON UPDATE {}", expression(v)?);
    }
    if c.auto_increment {
        s += " AUTO_INCREMENT";
    }
    if !c.visible {
        s += " INVISIBLE";
    }
    if !c.comment.is_empty() {
        s += &format!(" COMMENT {}", literal(&c.comment, mode));
    }
    for extra in &c.extra {
        s += &format!(" {}", expression(extra)?);
    }
    Ok(s)
}
fn names(values: &[String]) -> Result<String> {
    values
        .iter()
        .map(|s| ident(s))
        .collect::<Result<Vec<_>>>()
        .map(|v| v.join(", "))
}
fn index_option<'a>(value: &'a str, mode: &str) -> Result<&'a str> {
    let v = value.trim();
    if let Some(literal) = v.strip_prefix("COMMENT ") {
        let bytes = literal.as_bytes();
        let no_backslash = mode
            .split(',')
            .any(|m| m.trim().eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"));
        if bytes.first() != Some(&b'\'') {
            return Err(AppError::invalid("索引注释必须是 SQL 字符串"));
        }
        let mut i = 1;
        while i < bytes.len() {
            if bytes[i] == b'\\' && !no_backslash {
                i += 2;
                continue;
            }
            if bytes[i] == b'\'' {
                if bytes.get(i + 1) == Some(&b'\'') {
                    i += 2;
                    continue;
                }
                if i + 1 == bytes.len() {
                    return Ok(v);
                }
                break;
            }
            i += 1;
        }
        return Err(AppError::invalid("索引注释包含无效 SQL"));
    }
    expression(v)
}
fn index(i: &IndexDefinition, mode: &str) -> Result<String> {
    if i.parts.is_empty() {
        return Err(AppError::invalid("索引不能为空"));
    }
    let mut parts = Vec::new();
    for part in &i.parts {
        let mut p = match (&part.column, &part.expression) {
            (Some(c), None) => ident(c)?,
            (None, Some(e)) if part.prefix_length.is_none() => format!("({})", expression(e)?),
            _ => return Err(AppError::invalid("索引列或表达式无效")),
        };
        if let Some(n) = part.prefix_length {
            if n == 0 {
                return Err(AppError::invalid("索引前缀必须大于零"));
            }
            p += &format!("({n})");
        }
        if part.descending {
            p += " DESC";
        }
        parts.push(p);
    }
    let prefix = match i.kind {
        IndexKind::Primary => "PRIMARY KEY".to_owned(),
        IndexKind::Unique => format!("UNIQUE KEY {}", ident(&i.name)?),
        IndexKind::Normal => format!("KEY {}", ident(&i.name)?),
        IndexKind::Fulltext => format!("FULLTEXT KEY {}", ident(&i.name)?),
        IndexKind::Spatial => format!("SPATIAL KEY {}", ident(&i.name)?),
    };
    let mut s = format!("{} ({})", prefix, parts.join(", "));
    if let Some(m) = &i.method {
        s += &format!(" USING {}", keyword(m)?);
    }
    if !i.visible {
        s += " INVISIBLE";
    }
    for v in &i.options {
        s += &format!(" {}", index_option(v, mode)?);
    }
    Ok(s)
}
fn constraint(c: &ConstraintDefinition) -> Result<String> {
    match c {
        ConstraintDefinition::ForeignKey {
            name,
            columns,
            referenced_database,
            referenced_table,
            referenced_columns,
            on_update,
            on_delete,
            ..
        } => {
            if columns.is_empty() || columns.len() != referenced_columns.len() {
                return Err(AppError::invalid("外键列不完整"));
            }
            let action = |s: &str| -> Result<String> {
                let v = s.to_ascii_uppercase();
                if [
                    "RESTRICT",
                    "CASCADE",
                    "SET NULL",
                    "NO ACTION",
                    "SET DEFAULT",
                ]
                .contains(&v.as_str())
                {
                    Ok(v)
                } else {
                    Err(AppError::invalid("无效外键动作"))
                }
            };
            Ok(format!(
                "CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) ON UPDATE {} ON DELETE {}",
                ident(name)?,
                names(columns)?,
                qualified(referenced_database, referenced_table)?,
                names(referenced_columns)?,
                action(on_update)?,
                action(on_delete)?
            ))
        }
        ConstraintDefinition::Check {
            name,
            expression: e,
            enforced,
            ..
        } => Ok(format!(
            "CONSTRAINT {} CHECK ({}) {}",
            ident(name)?,
            expression(e)?,
            if *enforced {
                "ENFORCED"
            } else {
                "NOT ENFORCED"
            }
        )),
    }
}
fn constraint_id(c: &ConstraintDefinition) -> &str {
    match c {
        ConstraintDefinition::ForeignKey { id, .. } | ConstraintDefinition::Check { id, .. } => id,
    }
}
fn definer(value: &Option<String>) -> Result<String> {
    match value {
        None => Ok(String::new()),
        Some(v) => {
            let (user, host) = v
                .split_once('@')
                .ok_or_else(|| AppError::invalid("DEFINER 必须包含用户名和主机"))?;
            Ok(format!("DEFINER = {}@{} ", ident(user)?, ident(host)?))
        }
    }
}
fn view_sql(target: &str, v: &ViewDefinition, create: bool, mode: &str) -> Result<String> {
    let algorithm = v.algorithm.to_ascii_uppercase();
    let security = v.security.to_ascii_uppercase();
    if !["UNDEFINED", "MERGE", "TEMPTABLE"].contains(&algorithm.as_str())
        || !["DEFINER", "INVOKER"].contains(&security.as_str())
    {
        return Err(AppError::invalid("视图属性无效"));
    }
    let mut s = format!(
        "{} ALGORITHM = {} {}SQL SECURITY {} VIEW {}",
        if create { "CREATE" } else { "ALTER" },
        algorithm,
        definer(&v.definer)?,
        security,
        target
    );
    if !v.columns.is_empty() {
        s += &format!(" ({})", names(&v.columns)?);
    }
    let (select, rest) = super::super::sql::next_statement(&v.select, mode)?;
    if !rest.trim().is_empty() {
        return Err(AppError::invalid("视图正文必须是一条 SELECT"));
    }
    let statements =
        sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::MySqlDialect {}, select)
            .map_err(|e| AppError::invalid(format!("视图正文无效: {e}")))?;
    if !matches!(statements.as_slice(), [sqlparser::ast::Statement::Query(_)]) {
        return Err(AppError::invalid("视图正文必须是一条 SELECT"));
    }
    s += &format!(" AS {}", select.trim());
    if let Some(v) = &v.check_option {
        let v = v.to_ascii_uppercase();
        if !["LOCAL", "CASCADED"].contains(&v.as_str()) {
            return Err(AppError::invalid("无效 CHECK OPTION"));
        }
        s += &format!("\nWITH {v} CHECK OPTION");
    }
    Ok(s)
}
fn trigger_sql(t: &TriggerDefinition) -> Result<String> {
    let timing = t.timing.to_ascii_uppercase();
    let event = t.event.to_ascii_uppercase();
    if !["BEFORE", "AFTER"].contains(&timing.as_str())
        || !["INSERT", "UPDATE", "DELETE"].contains(&event.as_str())
        || t.body.trim().is_empty()
    {
        return Err(AppError::invalid("触发器属性无效"));
    }
    let mut s = format!(
        "CREATE {}TRIGGER {} {} {} ON {} FOR EACH ROW",
        definer(&t.definer)?,
        qualified(&t.database, &t.name)?,
        timing,
        event,
        qualified(&t.database, &t.table)?
    );
    if let Some(order) = &t.order {
        s += &format!(
            " {} {}",
            if order.follows { "FOLLOWS" } else { "PRECEDES" },
            ident(&order.trigger)?
        );
    }
    s += " ";
    s += &t.body;
    Ok(s)
}
pub(super) fn version(s: &str) -> (u32, u32, u32) {
    let mut v = s.split('.').map(|p| {
        p.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    });
    (
        v.next().unwrap_or(0),
        v.next().unwrap_or(0),
        v.next().unwrap_or(0),
    )
}
fn validate_features(d: &SchemaDefinition, v: &str) -> Result<()> {
    let ver = version(v);
    if let SchemaDefinition::Table(t) = d {
        if ver < (8, 0, 16)
            && t.constraints
                .iter()
                .any(|c| matches!(c, ConstraintDefinition::Check { .. }))
        {
            return Err(AppError::invalid("该版本不支持可执行 CHECK"));
        }
        if ver < (8, 0, 13)
            && (t.columns.iter().any(|c| match &c.default {
                ColumnDefault::Expression(e) => {
                    !e.to_ascii_uppercase().starts_with("CURRENT_TIMESTAMP")
                }
                _ => false,
            }) || t
                .indexes
                .iter()
                .any(|i| i.parts.iter().any(|p| p.expression.is_some())))
        {
            return Err(AppError::invalid("该版本不支持表达式默认值或函数索引"));
        }
        if ver < (8, 0, 0)
            && t.indexes
                .iter()
                .any(|i| !i.visible || i.parts.iter().any(|p| p.descending))
        {
            return Err(AppError::invalid("该版本不支持索引可见性或降序"));
        }
        if ver < (8, 0, 23) && t.columns.iter().any(|c| !c.visible) {
            return Err(AppError::invalid("该版本不支持隐藏列"));
        }
    }
    Ok(())
}
fn unique_ids<'a>(ids: impl Iterator<Item = &'a str>) -> Result<()> {
    let mut seen = HashSet::new();
    for id in ids {
        if id.is_empty() || !seen.insert(id) {
            return Err(AppError::invalid("结构项 ID 为空或重复"));
        }
    }
    Ok(())
}
fn table_alter(
    old: &TableDefinition,
    new: &TableDefinition,
    mode: &str,
) -> Result<(Vec<String>, Vec<String>)> {
    unique_ids(old.columns.iter().map(|x| x.id.as_str()))?;
    unique_ids(new.columns.iter().map(|x| x.id.as_str()))?;
    unique_ids(old.indexes.iter().map(|x| x.id.as_str()))?;
    unique_ids(new.indexes.iter().map(|x| x.id.as_str()))?;
    unique_ids(old.constraints.iter().map(constraint_id))?;
    unique_ids(new.constraints.iter().map(constraint_id))?;
    if old.options != new.options {
        return Err(AppError::invalid("无法安全修改未识别的表选项"));
    }
    let mut ops = Vec::new();
    let mut risks = Vec::new();
    let new_cols: HashMap<_, _> = new.columns.iter().map(|c| (c.id.as_str(), c)).collect();
    for c in &old.columns {
        if !new_cols.contains_key(c.id.as_str()) {
            ops.push(format!("DROP COLUMN {}", ident(&c.name)?));
            risks.push(format!("删除字段 {}", c.name));
        }
    }
    let old_cols: HashMap<_, _> = old.columns.iter().map(|c| (c.id.as_str(), c)).collect();
    for (n, c) in new.columns.iter().enumerate() {
        let position = if n == 0 {
            " FIRST".to_owned()
        } else {
            format!(" AFTER {}", ident(&new.columns[n - 1].name)?)
        };
        match old_cols.get(c.id.as_str()) {
            None => ops.push(format!("ADD COLUMN {}{}", column(c, mode)?, position)),
            Some(previous) => {
                let old_pos = old.columns.iter().position(|x| x.id == c.id).unwrap();
                let moved = old_pos != n
                    || (n > 0
                        && old.columns.get(old_pos.wrapping_sub(1)).map(|x| &x.id)
                            != Some(&new.columns[n - 1].id));
                if *previous != c || moved {
                    if previous.native_type != c.native_type {
                        risks.push(format!("字段 {} 类型变化可能丢失数据", c.name));
                    }
                    ops.push(format!(
                        "CHANGE COLUMN {} {}{}",
                        ident(&previous.name)?,
                        column(c, mode)?,
                        if moved { position } else { String::new() }
                    ));
                }
            }
        }
    }
    let new_indexes: HashMap<_, _> = new.indexes.iter().map(|x| (x.id.as_str(), x)).collect();
    for i in &old.indexes {
        if new_indexes.get(i.id.as_str()).copied() != Some(i) {
            ops.push(if i.kind == IndexKind::Primary {
                "DROP PRIMARY KEY".into()
            } else {
                format!("DROP INDEX {}", ident(&i.name)?)
            });
            risks.push(format!("删除或修改索引 {}", i.name));
        }
    }
    let old_indexes: HashMap<_, _> = old.indexes.iter().map(|x| (x.id.as_str(), x)).collect();
    for i in &new.indexes {
        if old_indexes.get(i.id.as_str()).copied() != Some(i) {
            ops.push(format!("ADD {}", index(i, mode)?));
        }
    }
    let new_c: HashMap<_, _> = new
        .constraints
        .iter()
        .map(|x| (constraint_id(x), x))
        .collect();
    for c in &old.constraints {
        if new_c.get(constraint_id(c)).copied() != Some(c) {
            let (kind, name) = match c {
                ConstraintDefinition::ForeignKey { name, .. } => ("FOREIGN KEY", name),
                ConstraintDefinition::Check { name, .. } => ("CHECK", name),
            };
            ops.push(format!("DROP {} {}", kind, ident(name)?));
            risks.push(format!("删除或修改约束 {}", name));
        }
    }
    let old_c: HashMap<_, _> = old
        .constraints
        .iter()
        .map(|x| (constraint_id(x), x))
        .collect();
    for c in &new.constraints {
        if old_c.get(constraint_id(c)).copied() != Some(c) {
            ops.push(format!("ADD {}", constraint(c)?));
        }
    }
    if old.engine != new.engine {
        ops.push(format!("ENGINE = {}", keyword(&new.engine)?));
        risks.push("更改存储引擎可能重建表".into());
    }
    if old.charset != new.charset || old.collation != new.collation {
        let cs = new
            .charset
            .as_ref()
            .ok_or_else(|| AppError::invalid("字符集不能为空"))?;
        let mut op = format!("DEFAULT CHARACTER SET {}", ident(cs)?);
        if let Some(c) = &new.collation {
            op += &format!(" COLLATE {}", ident(c)?);
        }
        ops.push(op);
    }
    if old.comment != new.comment {
        ops.push(format!("COMMENT = {}", literal(&new.comment, mode)));
    }
    Ok((ops, risks))
}
/// 根据完整基线和稳定 ID 生成冻结的 DDL，不访问数据库。
pub fn build_plan(intent: &SchemaEditIntent) -> Result<SchemaPlan> {
    let is_database = intent.target.kind.eq_ignore_ascii_case("database");
    let db = if is_database {
        if intent.target.name.trim().is_empty()
            || intent.target.database.as_deref() != Some(intent.target.name.as_str())
            || intent.baseline.is_some()
        {
            return Err(AppError::invalid("新建数据库目标无效"));
        }
        ""
    } else {
        intent
            .target
            .database
            .as_deref()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| AppError::invalid("必须指定数据库"))?
    };
    let target = if is_database {
        ident(&intent.target.name)?
    } else {
        qualified(db, &intent.target.name)?
    };
    let definition = intent
        .definition
        .as_ref()
        .or_else(|| intent.baseline.as_ref().map(|b| &b.definition));
    let kind = match definition {
        Some(SchemaDefinition::Database(_)) => "database",
        Some(SchemaDefinition::Table(_)) => "table",
        Some(SchemaDefinition::View(_)) => "view",
        Some(SchemaDefinition::Trigger(_)) => "trigger",
        None => "",
    };
    if !intent.target.kind.eq_ignore_ascii_case(kind) {
        return Err(AppError::invalid("目标对象类型不匹配"));
    }
    if let Some(b) = &intent.baseline {
        if b.completeness.state != CompletenessState::Complete
            || b.ddl.is_empty()
            || b.fingerprint.is_empty()
        {
            return Err(AppError::invalid("结构基线不完整"));
        }
        if b.target.database.as_deref() != Some(db) || b.target.name != intent.target.name {
            return Err(AppError::invalid("结构基线目标不匹配"));
        }
    }
    // 平台来源允许未知版本的本地草拟；版本相关属性仍按未知能力拒绝。
    // 真正执行时 schema_execute 会重新读取并严格比较服务端版本和 SQL 模式。
    if !intent.server_version.is_empty() && version(&intent.server_version).0 == 0 {
        return Err(AppError::invalid("服务端版本上下文无效"));
    }
    if let Some(b) = &intent.baseline
        && (b.server_version != intent.server_version || b.sql_mode != intent.sql_mode)
    {
        return Err(AppError::invalid("结构基线与当前版本或 sql_mode 不一致"));
    }
    let mode = intent.sql_mode.as_str();
    if let Some(d) = &intent.definition {
        validate_features(d, &intent.server_version)?;
    }
    let before = intent
        .baseline
        .as_ref()
        .map(|b| SchemaPrecondition::Fingerprint(b.fingerprint.clone()))
        .unwrap_or(SchemaPrecondition::Absent);
    let mut steps = Vec::new();
    let mut add = |sql, precondition, destructive_reasons, warnings| {
        steps.push(SchemaStep {
            sql,
            precondition,
            destructive_reasons,
            warnings,
        })
    };
    match (&intent.baseline, &intent.definition) {
        (None, None) => return Err(AppError::invalid("缺少结构变更")),
        (Some(b), None) => {
            let kind = match &b.definition {
                SchemaDefinition::Database(_) => {
                    return Err(AppError::invalid("不支持修改或删除数据库结构"));
                }
                SchemaDefinition::Table(_) => "TABLE",
                SchemaDefinition::View(_) => "VIEW",
                SchemaDefinition::Trigger(_) => "TRIGGER",
            };
            add(
                format!("DROP {kind} {target}"),
                before,
                vec!["删除对象可能丢失数据".into()],
                vec![],
            );
        }
        (None, Some(SchemaDefinition::Database(database))) => {
            if database.charset.is_none() && database.collation.is_some() {
                return Err(AppError::invalid("选择排序规则时必须指定字符集"));
            }
            let mut sql = format!("CREATE DATABASE {target}");
            if let Some(charset) = &database.charset {
                sql += &format!(" CHARACTER SET {}", ident(charset)?);
            }
            if let Some(collation) = &database.collation {
                sql += &format!(" COLLATE {}", ident(collation)?);
            }
            add(sql, before, vec![], vec![]);
        }
        (None, Some(SchemaDefinition::Table(t))) => {
            if t.columns.is_empty() {
                return Err(AppError::invalid("表至少需要一个字段"));
            }
            unique_ids(t.columns.iter().map(|x| x.id.as_str()))?;
            unique_ids(t.indexes.iter().map(|x| x.id.as_str()))?;
            unique_ids(t.constraints.iter().map(constraint_id))?;
            let mut defs = t
                .columns
                .iter()
                .map(|c| column(c, mode))
                .collect::<Result<Vec<_>>>()?;
            for i in &t.indexes {
                defs.push(index(i, mode)?);
            }
            for c in &t.constraints {
                defs.push(constraint(c)?);
            }
            let mut sql = format!(
                "CREATE TABLE {target} ({}) ENGINE = {}",
                defs.join(", "),
                keyword(&t.engine)?
            );
            if let Some(v) = &t.charset {
                sql += &format!(" DEFAULT CHARACTER SET {}", ident(v)?);
            }
            if let Some(v) = &t.collation {
                sql += &format!(" COLLATE {}", ident(v)?);
            }
            if !t.comment.is_empty() {
                sql += &format!(" COMMENT = {}", literal(&t.comment, mode));
            }
            for v in &t.options {
                sql += &format!(" {}", expression(v)?);
            }
            add(sql, before, vec![], vec![]);
        }
        (None, Some(SchemaDefinition::View(v))) => {
            add(view_sql(&target, v, true, mode)?, before, vec![], vec![])
        }
        (None, Some(SchemaDefinition::Trigger(t))) => {
            if t.database != db || t.name != intent.target.name {
                return Err(AppError::invalid("触发器目标不匹配"));
            }
            add(trigger_sql(t)?, before, vec![], vec![]);
        }
        (Some(b), Some(d)) => match (&b.definition, d) {
            (SchemaDefinition::Table(old), SchemaDefinition::Table(new)) => {
                let (mut ops, risks) = table_alter(old, new, mode)?;
                if !ops.is_empty() {
                    if let Some(a) = &intent.algorithm {
                        ops.push(format!("ALGORITHM = {}", keyword(a)?));
                    }
                    if let Some(l) = &intent.lock {
                        ops.push(format!("LOCK = {}", keyword(l)?));
                    }
                    add(
                        format!("ALTER TABLE {target} {}", ops.join(", ")),
                        before,
                        risks,
                        vec!["ALTER 可能等待元数据锁、锁表或重建".into()],
                    );
                }
            }
            (SchemaDefinition::View(_), SchemaDefinition::View(v)) => {
                add(view_sql(&target, v, false, mode)?, before, vec![], vec![])
            }
            (SchemaDefinition::Trigger(old), SchemaDefinition::Trigger(t)) => {
                if t.database != db || t.name != intent.target.name {
                    return Err(AppError::invalid("触发器目标不匹配"));
                }
                if old.table != t.table {
                    return Err(AppError::invalid("修改触发器所属表需要重新读取目标表依赖"));
                }
                if old.timing == t.timing && old.event == t.event && old.order != t.order {
                    return Err(AppError::invalid("修改触发器时必须保留原有顺序"));
                }
                add(
                    format!("DROP TRIGGER {target}"),
                    before,
                    vec!["触发器重建期间存在空窗".into()],
                    vec![],
                );
                add(
                    trigger_sql(t)?,
                    SchemaPrecondition::Absent,
                    vec![],
                    vec!["创建失败时原触发器已删除".into()],
                );
            }
            _ => return Err(AppError::invalid("不能更改对象种类")),
        },
    }
    if matches!(
        intent.definition.as_ref(),
        Some(SchemaDefinition::View(_) | SchemaDefinition::Trigger(_))
    ) {
        let previous = intent
            .baseline
            .as_ref()
            .and_then(|baseline| match &baseline.definition {
                SchemaDefinition::View(view) => Some(format!(
                    "原创建字符集：{}，原排序规则：{}",
                    view.character_set_client.as_deref().unwrap_or("未知"),
                    view.collation_connection.as_deref().unwrap_or("未知")
                )),
                SchemaDefinition::Trigger(trigger) => Some(format!(
                    "原 SQL 模式：{}，原创建字符集：{}，原排序规则：{}",
                    trigger.sql_mode,
                    trigger.character_set_client.as_deref().unwrap_or("未知"),
                    trigger.collation_connection.as_deref().unwrap_or("未知")
                )),
                _ => None,
            });
        let (charset, collation) = match intent.definition.as_ref() {
            Some(SchemaDefinition::View(view)) => (
                view.character_set_client.as_deref(),
                view.collation_connection.as_deref(),
            ),
            Some(SchemaDefinition::Trigger(trigger)) => (
                trigger.character_set_client.as_deref(),
                trigger.collation_connection.as_deref(),
            ),
            _ => (None, None),
        };
        for step in &mut steps {
            if step.sql.starts_with("CREATE ") || step.sql.starts_with("ALTER VIEW ") {
                step.warnings.push(format!(
                    "{}；新 SQL 模式：{}，新创建字符集：{}，新排序规则：{}",
                    previous.as_deref().unwrap_or("新建对象"),
                    intent.sql_mode,
                    charset.unwrap_or("未知"),
                    collation.unwrap_or("未知")
                ));
            }
        }
    }
    if intent.server_version.is_empty() {
        for step in &mut steps {
            step.warnings
                .push("服务端版本与 SQL 模式未知；仅供本地草拟，须按目标环境核对".into());
        }
    }
    let mut plan = SchemaPlan {
        intent: intent.clone(),
        steps,
        plan_digest: String::new(),
    };
    plan.plan_digest = format!("{:x}", Sha256::digest(serde_json::to_vec(&plan)?));
    Ok(plan)
}

#[cfg(test)]
#[path = "../../../../tests/unit/mysql/schema_plan.rs"]
mod tests;
