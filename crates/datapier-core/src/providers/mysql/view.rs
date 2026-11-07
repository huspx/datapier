//! 单一基表视图的保守映射证明；所有写入仍发送给原视图。

use std::collections::{BTreeMap, HashSet};

use serde_json::json;
use sha2::{Digest, Sha256};
use sqlparser::{
    ast::{
        Expr, ObjectNamePart, Query, SelectItem, SetExpr, Statement, TableFactor, Visit, Visitor,
    },
    dialect::MySqlDialect,
    parser::Parser,
};
use sqlx::Row;
use tokio_util::sync::CancellationToken;

use super::{MysqlSession, catalog};
use crate::model::{AppError, ResourceTarget, Result, TableSchema};

/// 视图投影、独立操作能力和覆盖整个依赖链的指纹。
#[derive(Clone)]
pub(super) struct ViewMapping {
    pub schema: TableSchema,
    pub update: bool,
    pub fingerprint: String,
}

struct Layer {
    schema: TableSchema,
    sources: Vec<Option<String>>,
    definition: String,
}

fn name_parts(name: &sqlparser::ast::ObjectName) -> Option<Vec<String>> {
    name.0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(id) => Some(id.value.clone()),
            _ => None,
        })
        .collect()
}

// 使用服务端展开的 VIEW_DEFINITION；只接受能直接证明来源的简单 SELECT。
fn projection(definition: &str, database: &str) -> Result<(ResourceTarget, Vec<Option<String>>)> {
    let mut statements = Parser::parse_sql(&MySqlDialect {}, definition)
        .map_err(|e| AppError::invalid(format!("无法解析视图定义: {e}")))?;
    if statements.len() != 1 {
        return Err(AppError::invalid("视图定义不是单条查询"));
    }
    let Statement::Query(query) = statements.remove(0) else {
        return Err(AppError::invalid("视图定义不是 SELECT"));
    };
    struct QueryCount(usize);
    impl Visitor for QueryCount {
        type Break = ();
        fn pre_visit_query(&mut self, _: &Query) -> std::ops::ControlFlow<()> {
            self.0 += 1;
            if self.0 > 1 {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        }
    }
    if query.visit(&mut QueryCount(0)).is_break() {
        return Err(AppError::invalid("视图含无法证明的子查询"));
    }
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || !query.pipe_operators.is_empty()
    {
        return Err(AppError::invalid("视图查询包含无法证明的结构"));
    }
    let SetExpr::Select(select) = *query.body else {
        return Err(AppError::invalid("视图不是单一 SELECT"));
    };
    if select.from.len() != 1
        || !select.from[0].joins.is_empty()
        || select.distinct.is_some()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || !matches!(&select.group_by, sqlparser::ast::GroupByExpr::Expressions(exprs, modifiers) if exprs.is_empty() && modifiers.is_empty())
    {
        return Err(AppError::invalid("视图不是可证明的单表投影"));
    }
    let TableFactor::Table {
        name,
        alias,
        args: None,
        ..
    } = &select.from[0].relation
    else {
        return Err(AppError::invalid("视图来源不是单表"));
    };
    let parts = name_parts(name).ok_or_else(|| AppError::invalid("视图来源名称无法解析"))?;
    let (db, table) = match parts.as_slice() {
        [table] => (database.to_owned(), table.clone()),
        [db, table] => (db.clone(), table.clone()),
        _ => return Err(AppError::invalid("视图来源名称无法解析")),
    };
    let qualifier = alias
        .as_ref()
        .map(|a| a.name.value.as_str())
        .unwrap_or(&table);
    let mut sources = Vec::with_capacity(select.projection.len());
    for item in &select.projection {
        let expr = match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
            _ => return Err(AppError::invalid("视图列包含无法证明的通配符")),
        };
        let source = match expr {
            Expr::Identifier(id) => Some(id.value.clone()),
            Expr::CompoundIdentifier(ids)
                if ids.len() == 2 && ids[0].value.eq_ignore_ascii_case(qualifier) =>
            {
                Some(ids[1].value.clone())
            }
            Expr::CompoundIdentifier(ids)
                if ids.len() == 3
                    && alias.is_none()
                    && ids[0].value.eq_ignore_ascii_case(&db)
                    && ids[1].value.eq_ignore_ascii_case(&table) =>
            {
                Some(ids[2].value.clone())
            }
            _ => None,
        };
        sources.push(source);
    }
    Ok((
        ResourceTarget {
            database: Some(db),
            name: table,
            kind: "table".into(),
            key_b64: None,
        },
        sources,
    ))
}

/// 基于完整可见定义和目录元数据证明映射；任何无法证明的情况返回错误。
/// 调用方应将错误表示为只读原因，而不是推测权限或直接访问基表数据。
pub(super) async fn resolve(
    s: &mut MysqlSession,
    schema: TableSchema,
    cancel: &CancellationToken,
) -> Result<ViewMapping> {
    let original = schema.clone();
    let mut current = schema;
    let mut layers = Vec::new();
    let mut seen = HashSet::new();
    while current.target.kind == "view" {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled());
        }
        let db = current
            .target
            .database
            .as_deref()
            .ok_or_else(|| AppError::invalid("缺少视图库名"))?;
        if !seen.insert((db.to_owned(), current.target.name.clone())) || layers.len() >= 64 {
            return Err(AppError::invalid("视图依赖循环或嵌套过深"));
        }
        let mut conn = s.new_connection().await?;
        let row = sqlx::query("SELECT VIEW_DEFINITION, IS_UPDATABLE FROM information_schema.VIEWS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?")
            .bind(db).bind(&current.target.name).fetch_optional(&mut conn).await?
            .ok_or_else(|| AppError::invalid("视图定义不可见"))?;
        let definition: String = row.try_get("VIEW_DEFINITION")?;
        let updatable: String = row.try_get("IS_UPDATABLE")?;
        if !updatable.eq_ignore_ascii_case("YES") {
            return Err(AppError::invalid("服务端未标记视图可更新"));
        }
        let (target, sources) = projection(&definition, db)?;
        if sources.len() != current.columns.len() {
            return Err(AppError::invalid("视图列与定义不一致"));
        }
        layers.push(Layer {
            schema: current,
            sources,
            definition,
        });
        current = catalog::schema_raw(s, target, cancel).await?;
    }
    if current.target.kind != "table" {
        return Err(AppError::invalid("视图来源不是基表"));
    }
    let mut fingerprint = Sha256::new();
    fingerprint.update(catalog::schema_fingerprint(&current)?.as_bytes());
    let mut mapped: BTreeMap<String, String> = current
        .columns
        .iter()
        .map(|c| (c.name.clone(), c.name.clone()))
        .collect();
    let mut no_expressions = true;
    for layer in layers.iter().rev() {
        fingerprint.update(catalog::schema_fingerprint(&layer.schema)?.as_bytes());
        fingerprint.update(layer.definition.as_bytes());
        let mut next = BTreeMap::new();
        for (col, source) in layer.schema.columns.iter().zip(&layer.sources) {
            if let Some(base) = source.as_ref().and_then(|name| mapped.get(name)) {
                next.insert(col.name.clone(), base.clone());
            } else {
                no_expressions = false;
            }
        }
        mapped = next;
    }
    let mut schema = original;
    schema.engine = current.engine.clone();
    schema.keys = current
        .keys
        .iter()
        .filter_map(|key| {
            key.iter()
                .map(|base| {
                    let mut names = mapped
                        .iter()
                        .filter(|(_, v)| *v == base)
                        .map(|(k, _)| k.clone());
                    let name = names.next()?;
                    if names.next().is_some() {
                        None
                    } else {
                        Some(name)
                    }
                })
                .collect::<Option<Vec<_>>>()
        })
        .collect();
    let mut writable = HashSet::new();
    for column in &mut schema.columns {
        if let Some(base) = mapped
            .get(&column.name)
            .and_then(|name| current.columns.iter().find(|c| &c.name == name))
        {
            column.generated = base.generated;
            if !base.generated {
                writable.insert(column.name.clone());
            }
        } else {
            column.generated = true;
        }
    }
    let mut seen_base = HashSet::new();
    let unique = mapped.values().all(|value| seen_base.insert(value));
    let required_present = current.columns.iter().all(|column| {
        column.nullable
            || column.generated
            || column.auto_increment
            || column.default_value.is_some()
            || mapped.values().any(|name| name == &column.name)
    });
    let required_insert_columns: Vec<String> = current
        .columns
        .iter()
        .filter(|column| {
            !column.nullable
                && !column.generated
                && !column.auto_increment
                && column.default_value.is_none()
        })
        .filter_map(|column| {
            mapped
                .iter()
                .find(|(_, base)| *base == &column.name)
                .map(|(view, _)| view.clone())
        })
        .collect();
    let keyed = !schema.keys.is_empty();
    let insert = keyed && no_expressions && unique && required_present;
    let mut reasons = Vec::new();
    if !keyed {
        reasons.push("视图未直接投影完整可靠键");
    }
    if !insert {
        reasons.push("视图包含表达式、重复映射或缺少必填基表列，不能插入");
    }
    let fingerprint = format!("{:x}", fingerprint.finalize());
    schema.details["view_mapping"] = json!({"columns":mapped,"required_insert_columns":required_insert_columns,"insert":insert,"update":keyed,"delete":keyed,"reasons":reasons,"writable":writable,"fingerprint":fingerprint});
    Ok(ViewMapping {
        schema,
        update: keyed,
        fingerprint,
    })
}

#[cfg(test)]
#[path = "../../../tests/unit/mysql/view.rs"]
mod tests;
