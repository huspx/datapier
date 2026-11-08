//! 结构读回比较；仅规范化可证明不改变含义的服务端变换。
use crate::model::schema::{
    ConstraintDefinition, IndexDefinition, IndexKind, TableDefinition, TriggerDefinition,
    ViewDefinition,
};
use sqlparser::{
    ast::{
        Expr, Ident, ObjectName, ObjectNamePart, Select, SelectItem, SetExpr, Statement,
        TableFactor, Visit, VisitMut, Visitor, VisitorMut,
    },
    dialect::MySqlDialect,
    parser::Parser,
};
use std::ops::ControlFlow;

fn supports_foreign_key(index: &IndexDefinition, columns: &[String]) -> bool {
    !columns.is_empty()
        && index.visible
        && matches!(
            index.kind,
            IndexKind::Primary | IndexKind::Unique | IndexKind::Normal
        )
        && index
            .method
            .as_deref()
            .is_none_or(|method| method.eq_ignore_ascii_case("BTREE"))
        && index.parts.len() >= columns.len()
        && index.parts.iter().zip(columns).all(|(part, column)| {
            part.column.as_ref() == Some(column)
                && part.expression.is_none()
                && part.prefix_length.is_none()
                && !part.descending
        })
}

/// 没有可复用索引时，InnoDB 为具名外键生成同名普通索引；不忽略其他额外索引。
pub(super) fn include_implicit_foreign_indexes(
    expected: &mut TableDefinition,
    actual: &TableDefinition,
) {
    for constraint in &expected.constraints {
        let ConstraintDefinition::ForeignKey { name, columns, .. } = constraint else {
            continue;
        };
        if name.is_empty()
            || expected
                .indexes
                .iter()
                .any(|index| index.name == *name || supports_foreign_key(index, columns))
        {
            continue;
        }
        if let Some(index) = actual.indexes.iter().find(|index| {
            index.name == *name
                && index.kind == IndexKind::Normal
                && index.parts.len() == columns.len()
                && index.options.is_empty()
                && supports_foreign_key(index, columns)
        }) {
            expected.indexes.push(index.clone());
        }
    }
}

// MySQL 5.7 补全整数显示宽度，8.x 删除该宽度；ZEROFILL 会使宽度影响显示，必须保留。
pub(super) fn normalized_type(value: &str) -> String {
    let value = value.trim();
    let split = value
        .find(|c: char| c == '(' || c.is_ascii_whitespace())
        .unwrap_or(value.len());
    let (base, rest) = value.split_at(split);
    let base = base.to_ascii_lowercase();
    let base = if base == "integer" {
        "int"
    } else {
        base.as_str()
    };
    if !matches!(
        base,
        "tinyint" | "smallint" | "mediumint" | "int" | "bigint"
    ) {
        // ENUM/SET 的字符串值区分大小写，不能把整个类型文本转为小写。
        return format!("{base}{rest}");
    }
    let rest = rest.trim();
    if rest.to_ascii_lowercase().contains("zerofill") {
        return format!("{base}{}", rest.to_ascii_lowercase());
    }
    let rest = if let Some(width) = rest.strip_prefix('(') {
        if let Some((number, suffix)) = width.split_once(')') {
            if !number.is_empty() && number.bytes().all(|b| b.is_ascii_digit()) {
                suffix.trim()
            } else {
                rest
            }
        } else {
            rest
        }
    } else {
        rest
    };
    if rest.is_empty() {
        base.to_owned()
    } else if rest.eq_ignore_ascii_case("unsigned") || rest.eq_ignore_ascii_case("signed") {
        format!("{base} {}", rest.to_ascii_lowercase())
    } else {
        format!("{base} {rest}")
    }
}

struct Normalize {
    database: String,
    qualifier: Option<String>,
    qualifier_database: Option<String>,
}
impl VisitorMut for Normalize {
    type Break = ();
    fn pre_visit_select(&mut self, select: &mut Select) -> ControlFlow<()> {
        // 只有一个直接来源时才能安全去掉限定名；嵌套或 JOIN 不做推断。
        self.qualifier = if select.from.len() == 1 && select.from[0].joins.is_empty() {
            match &mut select.from[0].relation {
                TableFactor::Table {
                    name,
                    alias,
                    args: None,
                    ..
                } => {
                    self.qualifier_database = match name.0.as_slice() {
                        [ObjectNamePart::Identifier(db), _] => Some(db.value.clone()),
                        [_] => Some(self.database.clone()),
                        _ => None,
                    };
                    let table = match name.0.last() {
                        Some(ObjectNamePart::Identifier(i)) => Some(i.value.clone()),
                        _ => None,
                    };
                    let q = alias.as_ref().map(|a| a.name.value.clone()).or(table);
                    *alias = None;
                    q
                }
                _ => None,
            }
        } else {
            None
        };
        ControlFlow::Continue(())
    }
    fn pre_visit_relation(&mut self, name: &mut ObjectName) -> ControlFlow<()> {
        if name.0.len() == 1 {
            name.0
                .insert(0, ObjectNamePart::Identifier(Ident::new(&self.database)));
        }
        for part in &mut name.0 {
            if let ObjectNamePart::Identifier(i) = part {
                i.quote_style = None;
            }
        }
        ControlFlow::Continue(())
    }
    fn post_visit_expr(&mut self, expr: &mut Expr) -> ControlFlow<()> {
        while let Expr::Nested(inner) = expr {
            *expr = *inner.clone();
        }
        match expr {
            Expr::Identifier(i) => i.quote_style = None,
            Expr::CompoundIdentifier(parts) => {
                for i in parts.iter_mut() {
                    i.quote_style = None;
                }
                if (parts.len() == 2
                    || (parts.len() == 3
                        && self.qualifier_database.as_deref() == Some(parts[0].value.as_str())))
                    && self
                        .qualifier
                        .as_deref()
                        .is_some_and(|q| parts[parts.len() - 2].value == q)
                {
                    *expr = Expr::Identifier(parts.last().expect("qualified name").clone());
                }
            }
            _ => (),
        }
        ControlFlow::Continue(())
    }
    fn post_visit_select(&mut self, select: &mut Select) -> ControlFlow<()> {
        for item in &mut select.projection {
            if let SelectItem::ExprWithAlias {
                expr: Expr::Identifier(column),
                alias,
            } = item
            {
                if column.value == alias.value {
                    *item = SelectItem::UnnamedExpr(Expr::Identifier(column.clone()));
                } else {
                    alias.quote_style = None;
                }
            } else if let SelectItem::ExprWithAlias { alias, .. } = item {
                alias.quote_style = None;
            }
        }
        ControlFlow::Continue(())
    }
}
struct SelectCount(usize);
impl Visitor for SelectCount {
    type Break = ();
    fn pre_visit_select(&mut self, _: &Select) -> ControlFlow<()> {
        self.0 += 1;
        ControlFlow::Continue(())
    }
}
fn normalized(input: &str, database: &str) -> Option<Statement> {
    let mut statements = Parser::parse_sql(&MySqlDialect {}, input).ok()?;
    let [Statement::Query(query)] = statements.as_mut_slice() else {
        return None;
    };
    // 多层查询中的限定名有作用域，不能复用顶层来源推断。
    if !matches!(query.body.as_ref(), SetExpr::Select(_)) || query.with.is_some() {
        return None;
    }
    let mut count = SelectCount(0);
    let _ = Visit::visit(&statements, &mut count);
    if count.0 != 1 {
        return None;
    }
    let _ = VisitMut::visit(
        &mut statements,
        &mut Normalize {
            database: database.to_owned(),
            qualifier: None,
            qualifier_database: None,
        },
    );
    Some(statements.remove(0))
}
pub(super) fn views_equal(
    expected: &ViewDefinition,
    actual: &ViewDefinition,
    database: &str,
) -> bool {
    let select_equal = match (
        normalized(&expected.select, database),
        normalized(&actual.select, database),
    ) {
        (Some(a), Some(b)) => a == b,
        _ => expected.select == actual.select,
    };
    select_equal
        && (expected.columns.is_empty() || expected.columns == actual.columns)
        && expected.algorithm.eq_ignore_ascii_case(&actual.algorithm)
        && expected.security.eq_ignore_ascii_case(&actual.security)
        && expected
            .check_option
            .as_ref()
            .map(|s| s.to_ascii_uppercase())
            == actual.check_option.as_ref().map(|s| s.to_ascii_uppercase())
        && expected
            .definer
            .as_ref()
            .is_none_or(|d| actual.definer.as_ref() == Some(d))
        && expected
            .character_set_client
            .as_ref()
            .is_none_or(|v| actual.character_set_client.as_ref() == Some(v))
        && expected
            .collation_connection
            .as_ref()
            .is_none_or(|v| actual.collation_connection.as_ref() == Some(v))
}

pub(super) fn triggers_equal(expected: &TriggerDefinition, actual: &TriggerDefinition) -> bool {
    // 未指定的创建上下文由服务端补齐；显式设置的属性和正文仍须逐项验证。
    expected.database == actual.database
        && expected.name == actual.name
        && expected.table == actual.table
        && expected.timing.eq_ignore_ascii_case(&actual.timing)
        && expected.event.eq_ignore_ascii_case(&actual.event)
        && expected.body.trim() == actual.body.trim()
        && expected.sql_mode == actual.sql_mode
        && expected.order == actual.order
        && expected
            .definer
            .as_ref()
            .is_none_or(|v| actual.definer.as_ref() == Some(v))
        && expected
            .character_set_client
            .as_ref()
            .is_none_or(|v| actual.character_set_client.as_ref() == Some(v))
        && expected
            .collation_connection
            .as_ref()
            .is_none_or(|v| actual.collation_connection.as_ref() == Some(v))
}

#[cfg(test)]
#[path = "../../../../tests/unit/mysql/schema_verify.rs"]
mod tests;
