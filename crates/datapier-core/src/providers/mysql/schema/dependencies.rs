//! 收集视图依赖并将实际定义纳入基线；不从当前结果推测结构。
use super::{fingerprint, qualified};
use crate::model::{AppError, Result};
use sqlparser::{
    ast::{ObjectName, ObjectNamePart, Visit, Visitor},
    dialect::MySqlDialect,
    parser::Parser,
};
use sqlx::{MySqlConnection, Row};
use std::{collections::HashSet, ops::ControlFlow};

struct Relations(Vec<Vec<String>>);
impl Visitor for Relations {
    type Break = ();
    fn pre_visit_relation(&mut self, name: &ObjectName) -> ControlFlow<()> {
        let mut parts = Vec::new();
        for part in &name.0 {
            match part {
                ObjectNamePart::Identifier(i) => parts.push(i.value.clone()),
                _ => return ControlFlow::Break(()),
            }
        }
        self.0.push(parts);
        ControlFlow::Continue(())
    }
}

pub(super) async fn view_dependencies(
    conn: &mut MySqlConnection,
    db: &str,
    name: &str,
    select: &str,
    mode: &str,
) -> Result<Vec<String>> {
    let mut queue = vec![(db.to_owned(), name.to_owned(), select.to_owned())];
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    while let Some((current_db, current_name, query)) = queue.pop() {
        if !seen.insert((current_db.clone(), current_name.clone())) {
            continue;
        }
        if seen.len() > 256 {
            return Err(AppError::invalid("视图依赖过多"));
        }
        let statements = Parser::parse_sql(&MySqlDialect {}, &query)
            .map_err(|e| AppError::invalid(format!("视图依赖无法解析: {e}")))?;
        let mut relations = Relations(Vec::new());
        if statements.visit(&mut relations).is_break() {
            return Err(AppError::invalid("视图依赖名称无法解析"));
        }
        for parts in relations.0 {
            let (database, table) = match parts.as_slice() {
                [table] => (current_db.clone(), table.clone()),
                [database, table] => (database.clone(), table.clone()),
                _ => return Err(AppError::invalid("视图依赖名称无法解析")),
            };
            if seen.contains(&(database.clone(), table.clone())) {
                continue;
            }
            let row = sqlx::query("SELECT TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA=? AND TABLE_NAME=?").bind(&database).bind(&table).fetch_optional(&mut *conn).await?.ok_or_else(||AppError::invalid("视图依赖不存在或不可见"))?;
            let kind: String = row.try_get("TABLE_TYPE")?;
            let is_view = kind.eq_ignore_ascii_case("VIEW");
            let object = qualified(&database, &table)?;
            let row = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SHOW CREATE {} {object}",
                if is_view { "VIEW" } else { "TABLE" }
            )))
            .fetch_one(&mut *conn)
            .await?;
            let ddl: String = row.try_get(1)?;
            result.push(format!("{}:{}", object, fingerprint(&ddl, mode)));
            if is_view {
                let definition:String = sqlx::query_scalar("SELECT VIEW_DEFINITION FROM information_schema.VIEWS WHERE TABLE_SCHEMA=? AND TABLE_NAME=?").bind(&database).bind(&table).fetch_one(&mut *conn).await?;
                queue.push((database, table, definition));
            } else {
                seen.insert((database, table));
            }
        }
    }
    result.sort();
    result.dedup();
    Ok(result)
}
