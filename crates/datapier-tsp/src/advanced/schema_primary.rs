use super::SchemaOperation;
use serde_json::{Value, json};
use uuid::Uuid;

fn array<'a>(root: &'a Value, key: &str) -> Result<&'a Vec<Value>, String> {
    root.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("结构中的 {key} 列表无效"))
}

fn column(root: &Value, index: usize) -> Result<&Value, String> {
    array(root, "columns")?
        .get(index)
        .ok_or_else(|| "字段不存在".into())
}

fn column_name(column: &Value) -> Result<&str, String> {
    column
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| "字段名称无效".into())
}

fn self_reference(constraint: &Value, database: Option<&str>, table: &str) -> Result<bool, String> {
    let referenced_table = constraint
        .get("referenced_table")
        .and_then(Value::as_str)
        .ok_or("外键引用表无效")?;
    if referenced_table != table || table.is_empty() {
        return Ok(false);
    }
    let referenced_database = constraint
        .get("referenced_database")
        .and_then(Value::as_str)
        .ok_or("外键引用数据库无效")?;
    if referenced_database.is_empty() {
        return Ok(true);
    }
    match database.filter(|db| !db.is_empty()) {
        Some(db) => Ok(referenced_database == db),
        None => Err("无法确定外键是否引用当前表，请先指定数据库".into()),
    }
}

// 仅跟踪明确的列名引用；空名称无法唯一标识字段。
fn references(
    root: &Value,
    name: &str,
    database: Option<&str>,
    table: &str,
) -> Result<Vec<String>, String> {
    let mut paths = Vec::new();
    if name.is_empty() {
        return Ok(paths);
    }
    for (i, index) in array(root, "indexes")?.iter().enumerate() {
        for (j, part) in array(index, "parts")?.iter().enumerate() {
            if part.get("column").and_then(Value::as_str) == Some(name) {
                paths.push(format!("/indexes/{i}/parts/{j}/column"));
            }
        }
    }
    for (i, constraint) in array(root, "constraints")?.iter().enumerate() {
        if constraint.get("kind").and_then(Value::as_str) != Some("foreign_key") {
            continue;
        }
        for (j, value) in array(constraint, "columns")?.iter().enumerate() {
            if value.as_str() == Some(name) {
                paths.push(format!("/constraints/{i}/columns/{j}"));
            }
        }
        let referenced = array(constraint, "referenced_columns")?;
        if referenced.iter().any(|value| value.as_str() == Some(name))
            && self_reference(constraint, database, table)?
        {
            for (j, value) in referenced.iter().enumerate() {
                if value.as_str() == Some(name) {
                    paths.push(format!("/constraints/{i}/referenced_columns/{j}"));
                }
            }
        }
    }
    Ok(paths)
}

/// 主键列同时移除与 NOT NULL 冲突的 NULL 默认值，保留其他默认表达式。
pub(super) fn primary_column_changes(
    root: &Value,
    index: usize,
) -> Result<[Option<SchemaOperation>; 2], String> {
    let selected = column(root, index)?;
    let nullable = selected
        .get("nullable")
        .and_then(Value::as_bool)
        .ok_or("字段的可空属性无效")?;
    Ok([
        nullable.then(|| SchemaOperation::Set {
            path: format!("/columns/{index}/nullable"),
            value: Value::Bool(false),
        }),
        (selected.pointer("/default/kind").and_then(Value::as_str) == Some("null")).then(|| {
            SchemaOperation::Set {
                path: format!("/columns/{index}/default"),
                value: json!({"kind":"unspecified"}),
            }
        }),
    ])
}

pub(super) fn toggle_primary(
    root: &Value,
    column_id: &str,
) -> Result<Vec<SchemaOperation>, String> {
    let columns = array(root, "columns")?;
    let mut matches = columns
        .iter()
        .enumerate()
        .filter(|(_, column)| column.get("id").and_then(Value::as_str) == Some(column_id));
    let (column_index, selected) = matches.next().ok_or("字段不存在")?;
    if column_id.is_empty() || matches.next().is_some() {
        return Err("字段标识为空或不唯一".into());
    }
    let name = column_name(selected)?;
    if name.is_empty()
        || columns.iter().enumerate().any(|(i, column)| {
            i != column_index && column.get("name").and_then(Value::as_str) == Some(name)
        })
    {
        return Err("字段名称为空或不唯一".into());
    }
    let indexes = array(root, "indexes")?;
    let mut primaries = indexes
        .iter()
        .enumerate()
        .filter(|(_, index)| index.get("kind").and_then(Value::as_str) == Some("primary"));
    let primary = primaries.next();
    if primaries.next().is_some() {
        return Err("存在多个主键索引，请先修正".into());
    }
    let mut operations = Vec::new();
    if let Some((index, primary)) = primary {
        let parts = array(primary, "parts")?;
        let mut matching_parts = parts
            .iter()
            .enumerate()
            .filter(|(_, part)| part.get("column").and_then(Value::as_str) == Some(name));
        if let Some((part, _)) = matching_parts.next() {
            if matching_parts.next().is_some() {
                return Err("主键索引中重复包含该字段".into());
            }
            if parts.len() == 1 {
                operations.push(SchemaOperation::Remove {
                    path: "/indexes".into(),
                    index,
                });
            } else {
                operations.push(SchemaOperation::Remove {
                    path: format!("/indexes/{index}/parts"),
                    index: part,
                });
            }
            return Ok(operations);
        }
        operations.push(SchemaOperation::Insert {
            path: format!("/indexes/{index}/parts"),
            value: json!({"column":name,"expression":null,"prefix_length":null,"descending":false}),
        });
    } else {
        operations.push(SchemaOperation::Insert { path: "/indexes".into(), value: json!({"id":Uuid::new_v4().to_string(),"name":"PRIMARY","kind":"primary","parts":[{"column":name,"expression":null,"prefix_length":null,"descending":false}],"visible":true,"method":null,"options":[]}) });
    }
    operations.extend(
        primary_column_changes(root, column_index)?
            .into_iter()
            .flatten(),
    );
    Ok(operations)
}

pub(super) fn rename_column(
    root: &Value,
    column_index: usize,
    new_name: &str,
    database: Option<&str>,
    table: &str,
) -> Result<Vec<SchemaOperation>, String> {
    let name = column_name(column(root, column_index)?)?;
    if name == new_name {
        return Ok(Vec::new());
    }
    let paths = references(root, name, database, table)?;
    if !paths.is_empty()
        && array(root, "columns")?
            .iter()
            .enumerate()
            .any(|(i, column)| {
                i != column_index && column.get("name").and_then(Value::as_str) == Some(name)
            })
    {
        return Err("字段名称重复，无法安全更新索引或外键引用".into());
    }
    if new_name.is_empty() && !paths.is_empty() {
        return Err("字段被索引或外键引用，不能清空名称".into());
    }
    let mut operations = Vec::with_capacity(paths.len() + 1);
    operations.push(SchemaOperation::Set {
        path: format!("/columns/{column_index}/name"),
        value: json!(new_name),
    });
    operations.extend(paths.into_iter().map(|path| SchemaOperation::Set {
        path,
        value: json!(new_name),
    }));
    Ok(operations)
}

pub(super) fn remove_column(
    root: &Value,
    column_index: usize,
    database: Option<&str>,
    table: &str,
) -> Result<Vec<SchemaOperation>, String> {
    let name = column_name(column(root, column_index)?)?;
    if !references(root, name, database, table)?.is_empty() {
        return Err("字段被索引或外键引用，不能删除".into());
    }
    // 空占位符无法可靠地关联到某个字段。
    for index in array(root, "indexes")? {
        for part in array(index, "parts")? {
            if name.is_empty() && part.get("column").and_then(Value::as_str) == Some("") {
                return Err("无法安全判断索引字段的依赖关系".into());
            }
        }
    }
    if name.is_empty() {
        for constraint in array(root, "constraints")? {
            if constraint.get("kind").and_then(Value::as_str) == Some("foreign_key")
                && (array(constraint, "columns")?
                    .iter()
                    .any(|value| value.as_str() == Some(""))
                    || (array(constraint, "referenced_columns")?
                        .iter()
                        .any(|value| value.as_str() == Some(""))
                        && self_reference(constraint, database, table)?))
            {
                return Err("无法安全判断外键字段的依赖关系".into());
            }
        }
    }
    Ok(vec![SchemaOperation::Remove {
        path: "/columns".into(),
        index: column_index,
    }])
}

#[cfg(test)]
#[path = "../../tests/unit/advanced/schema_primary.rs"]
mod tests;
