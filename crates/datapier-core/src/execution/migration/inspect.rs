//! 只读取文件中的候选元数据；正式准备时仍会重新校验输入。
use crate::{
    execution::{
        data_files::{self, RowReader, csv::CsvReader},
        sql_restore::{self, SqlKind},
    },
    model::{
        AppError, Column, ProviderKind, Result,
        transfer::{
            MAX_BATCH_BYTES, MAX_PLAN_BYTES, MAX_PLAN_ITEMS, TransferFormat, TransferInput,
        },
    },
};
use std::collections::{BTreeMap, HashSet};
use tokio_util::sync::CancellationToken;

#[derive(serde::Serialize)]
struct Object {
    database: Option<String>,
    name: String,
    kind: &'static str,
    columns: Vec<Column>,
}

fn safe_column(mut column: Column) -> Column {
    column.default_value = None;
    column.comment = None;
    column
}

/// 为文件导入界面发现源列及 SQL 对象。不会生成或缓存正式导入计划。
pub fn inspect_input(
    input: &TransferInput,
    provider: ProviderKind,
    cancel: &CancellationToken,
) -> Result<serde_json::Value> {
    crate::model::ensure_not_cancelled(cancel)?;
    let mut columns = Vec::new();
    let mut objects = Vec::new();
    match input.format {
        TransferFormat::Sql => {
            let index = sql_restore::inspect_with_encoding_and_cancel(
                &input.path,
                &input.csv.encoding,
                cancel,
            )?;
            let mut positions = BTreeMap::new();
            for statement in index.statements {
                crate::model::ensure_not_cancelled(cancel)?;
                let kind = match statement.kind {
                    SqlKind::Insert => SqlKind::Table,
                    SqlKind::Drop => match statement.drop_kind {
                        Some(kind) => kind,
                        None => continue,
                    },
                    kind => kind,
                };
                // 索引的选择范围是所属表；索引名称在不同表中可以重复。
                let (kind, name, database) = if kind == SqlKind::Index {
                    let Some(name) = statement.index_table_name else {
                        continue;
                    };
                    (
                        "table",
                        name,
                        statement
                            .index_table_database
                            .or(statement.context.database),
                    )
                } else {
                    let kind = match kind {
                        SqlKind::Database => "database",
                        SqlKind::Table => "table",
                        SqlKind::View => "view",
                        SqlKind::Trigger => "trigger",
                        SqlKind::Procedure => "procedure",
                        SqlKind::Function => "function",
                        SqlKind::Event => "event",
                        _ => continue,
                    };
                    let Some(name) = statement.source_name else {
                        continue;
                    };
                    let database = if kind == "database" {
                        statement.source_database
                    } else {
                        statement.source_database.or(statement.context.database)
                    };
                    (kind, name, database)
                };
                let key = (database, name, kind);
                let position = match positions.get(&key) {
                    Some(&position) => position,
                    None => {
                        if objects.len() >= MAX_PLAN_ITEMS {
                            return Err(AppError::new("resource_limit", "SQL 对象数超出限制"));
                        }
                        let position = objects.len();
                        objects.push(Object {
                            database: key.0.clone(),
                            name: key.1.clone(),
                            kind,
                            columns: Vec::new(),
                        });
                        positions.insert(key, position);
                        position
                    }
                };
                let object: &mut Object = &mut objects[position];
                for name in statement
                    .source_columns
                    .into_iter()
                    .flatten()
                    .chain(statement.columns.into_iter().flatten())
                {
                    if !object.columns.iter().any(|column| column.name == name) {
                        if object.columns.len() >= MAX_PLAN_ITEMS {
                            return Err(AppError::new("resource_limit", "SQL 列数超出限制"));
                        }
                        object.columns.push(Column::new(name, ""));
                    }
                }
            }
        }
        TransferFormat::Csv | TransferFormat::Json => {
            let (_, file) = data_files::fingerprint_open_with_cancel(
                &input.path,
                MAX_BATCH_BYTES,
                MAX_PLAN_BYTES / 2,
                cancel,
            )?;
            if input.format == TransferFormat::Csv {
                // 读取器本身不跳过表头；仅探测宽度时放宽无表头必须指定列名的验证。
                let probe = (!input.csv.header && input.csv.columns.is_empty()).then(|| {
                    let mut options = input.csv.clone();
                    options.header = true;
                    options
                });
                let mut reader = CsvReader::open(file, probe.as_ref().unwrap_or(&input.csv))?;
                let names = if input.csv.header {
                    reader
                        .next()?
                        .ok_or_else(|| AppError::invalid("CSV 缺少表头"))?
                        .fields
                } else if !input.csv.columns.is_empty() {
                    input.csv.columns.clone()
                } else {
                    let width = reader.next()?.map_or(0, |record| record.fields.len());
                    (1..=width)
                        .map(|position| format!("column_{position}"))
                        .collect()
                };
                let mut unique = HashSet::new();
                if names.len() > MAX_PLAN_ITEMS {
                    return Err(AppError::new("resource_limit", "CSV 列数超出限制"));
                }
                if names
                    .iter()
                    .any(|name| name.is_empty() || !unique.insert(name))
                {
                    return Err(AppError::invalid("CSV 表头为空或重复"));
                }
                if provider != ProviderKind::Redis {
                    columns = names
                        .into_iter()
                        .map(|name| Column::new(name, "TEXT"))
                        .collect();
                }
                reader.verify()?;
            } else {
                let reader = RowReader::from_verified_with_cancel(input, file, provider, cancel)?;
                columns = reader.columns().iter().cloned().map(safe_column).collect();
            }
        }
    }
    crate::model::ensure_not_cancelled(cancel)?;
    let result = serde_json::json!({"columns": columns, "objects": objects});
    if serde_json::to_vec(&result)?.len() > MAX_PLAN_BYTES {
        return Err(AppError::new("resource_limit", "输入元数据超出限制"));
    }
    Ok(result)
}
