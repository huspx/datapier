use std::collections::{BTreeMap, HashSet};
use std::str::FromStr;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value as Json, json};
use tokio_util::sync::CancellationToken;

use super::{
    ArcherySession,
    query::{self, QueryResponse},
};
use crate::{
    ExecutionSink,
    model::{
        AppError, Column, Resource, ResourcePage, ResourceTarget, Result, TableSchema, Value, param,
    },
};

fn protocol(message: &str) -> AppError {
    AppError::new("remote_protocol", message)
}

fn web_data(value: Json) -> Result<Json> {
    match value.get("status").and_then(Json::as_i64) {
        Some(0) => value
            .get("data")
            .cloned()
            .ok_or_else(|| protocol("Archery 响应缺少 data")),
        Some(_) => Err(AppError::new(
            "database",
            value
                .get("msg")
                .and_then(Json::as_str)
                .unwrap_or("Archery 请求失败"),
        )),
        None => Err(protocol("Archery 响应缺少 status")),
    }
}

fn identifier(name: &str) -> Result<String> {
    if name.is_empty() || name.contains('\0') || name.contains('\\') {
        return Err(AppError::invalid("标识符为空或包含不支持的字符"));
    }
    Ok(format!("`{}`", name.replace('`', "``")))
}

fn selected_database<'a>(
    session: &'a ArcherySession,
    requested: Option<&'a str>,
) -> Result<&'a str> {
    let database = requested.unwrap_or(&session.database);
    let connection = session
        .profile
        .archery()
        .ok_or_else(|| AppError::invalid("不是 Archery 连接"))?;
    if !connection.databases.iter().any(|name| name == database) {
        return Err(AppError::new("target_not_selected", "数据库不在已选范围内"));
    }
    identifier(database)?;
    Ok(database)
}

async fn names(
    session: &ArcherySession,
    database: &str,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Vec<String>> {
    let connection = session
        .profile
        .archery()
        .ok_or_else(|| AppError::invalid("不是 Archery 连接"))?;
    sink.mark_sent(false).await?;
    let response = session
        .auth
        .get(
            "instance/instance_resource/",
            &[
                ("instance_id", connection.instance_id.to_string()),
                ("resource_type", "table".into()),
                ("db_name", database.to_owned()),
            ],
            cancel,
        )
        .await?;
    let data = web_data(response)?;
    let items = data
        .as_array()
        .ok_or_else(|| protocol("Archery 资源列表不是数组"))?;
    let mut result = Vec::with_capacity(items.len());
    let mut seen = HashSet::new();
    for item in items {
        let name = item
            .as_str()
            .ok_or_else(|| protocol("Archery 资源名称不是字符串"))?;
        identifier(name)?;
        if !seen.insert(name) {
            return Err(protocol("Archery 资源名称重复"));
        }
        result.push(name.to_owned());
    }
    Ok(result)
}

pub(super) async fn list(
    session: &ArcherySession,
    params: Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    let requested = match params.get("database") {
        None | Some(Json::Null) => None,
        Some(value) => Some(
            value
                .as_str()
                .ok_or_else(|| AppError::invalid("database 必须是字符串"))?,
        ),
    };
    let resources = if let Some(database) = requested {
        let database = selected_database(session, Some(database))?;
        names(session, database, sink, cancel)
            .await?
            .into_iter()
            .map(|name| Resource {
                name,
                kind: "unknown".into(),
                database: Some(database.into()),
                key_b64: None,
                data: json!({"relation_kind":"unknown"}),
            })
            .collect()
    } else {
        let connection = session
            .profile
            .archery()
            .ok_or_else(|| AppError::invalid("不是 Archery 连接"))?;
        connection
            .databases
            .iter()
            .map(|name| Resource {
                name: name.clone(),
                kind: "database".into(),
                database: Some(name.clone()),
                key_b64: None,
                data: Json::Null,
            })
            .collect()
    };
    // 平台会过滤资源（v1.12.0 MySQL 排除 test），不能声称是完整实例目录。
    Ok(serde_json::to_value(ResourcePage {
        resources,
        cursor: None,
        complete: requested.is_none(),
        resource_limited: false,
    })?)
}

fn completeness(result: &QueryResponse) -> &'static str {
    metadata_completeness(
        result.affected_rows,
        result.rows.len(),
        result.is_masked,
        result.mask_rule_hit,
    )
}

fn metadata_completeness(
    affected: Option<u64>,
    rows: usize,
    is_masked: Option<bool>,
    mask_rule_hit: Option<bool>,
) -> &'static str {
    // v1.12.0 MySQL 的 cursor.execute 返回完整行数，随后 fetchmany 才限制返回；
    // 脱敏、缺失计数或触及请求上限时不能推断结构完整。
    match affected {
        Some(n) if n > rows as u64 => "incomplete",
        Some(n)
            if n == rows as u64
                && n < 10_000
                && is_masked == Some(false)
                && mask_rule_hit == Some(false) =>
        {
            "complete"
        }
        _ => "unknown",
    }
}

fn positions(result: &QueryResponse, required: &[&str]) -> Result<Vec<usize>> {
    required
        .iter()
        .map(|name| {
            let mut matches = result
                .columns
                .iter()
                .enumerate()
                .filter(|(_, c)| c.eq_ignore_ascii_case(name));
            let index = matches
                .next()
                .map(|(i, _)| i)
                .ok_or_else(|| protocol("Archery 元数据缺少必需列"))?;
            if matches.next().is_some() {
                return Err(protocol("Archery 元数据列名重复"));
            }
            Ok(index)
        })
        .collect()
}

fn text(row: &[Json], index: usize) -> Result<&str> {
    row.get(index)
        .and_then(Json::as_str)
        .ok_or_else(|| protocol("Archery 元数据字段类型错误"))
}
fn optional_text(row: &[Json], index: usize) -> Result<Option<String>> {
    match row.get(index) {
        Some(Json::Null) => Ok(None),
        Some(Json::String(value)) => Ok(Some(value.clone())),
        _ => Err(protocol("Archery 元数据字段类型错误")),
    }
}
fn number(value: &Json) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
}

fn parse_columns(result: &QueryResponse) -> Result<Vec<Column>> {
    let p = positions(
        result,
        &[
            "Field", "Type", "Null", "Key", "Default", "Extra", "Comment",
        ],
    )?;
    let mut columns = Vec::with_capacity(result.rows.len());
    let mut seen = HashSet::new();
    for row in &result.rows {
        let name = text(row, p[0])?;
        identifier(name)?;
        if !seen.insert(name) {
            return Err(protocol("Archery 字段名称重复"));
        }
        let nullable = match text(row, p[2])? {
            "YES" => true,
            "NO" => false,
            _ => return Err(protocol("Archery 字段 NULL 标志无效")),
        };
        let extra = text(row, p[5])?;
        let generated = extra
            .split_ascii_whitespace()
            .any(|v| v.eq_ignore_ascii_case("generated"));
        let auto_increment = extra
            .split_ascii_whitespace()
            .any(|v| v.eq_ignore_ascii_case("auto_increment"));
        columns.push(Column {
            name: name.into(),
            type_name: text(row, p[1])?.into(),
            nullable,
            generated,
            primary_key: text(row, p[3])? == "PRI",
            auto_increment,
            default_value: optional_text(row, p[4])?,
            comment: Some(text(row, p[6])?.into()),
        });
    }
    Ok(columns)
}

fn parse_indexes(
    result: &QueryResponse,
    columns: &[Column],
    complete: bool,
) -> Result<(Vec<Json>, Vec<Vec<String>>)> {
    let p = positions(
        result,
        &[
            "Key_name",
            "Non_unique",
            "Seq_in_index",
            "Column_name",
            "Sub_part",
            "Index_type",
            "Collation",
        ],
    )?;
    let expression = result
        .columns
        .iter()
        .position(|c| c.eq_ignore_ascii_case("Expression"));
    let mut indexes = Vec::new();
    let mut groups: BTreeMap<String, Vec<(u64, bool, Option<String>)>> = BTreeMap::new();
    for row in &result.rows {
        let name = text(row, p[0])?;
        let non_unique = row
            .get(p[1])
            .and_then(number)
            .filter(|n| *n <= 1)
            .ok_or_else(|| protocol("索引唯一性无效"))?;
        let sequence = row
            .get(p[2])
            .and_then(number)
            .filter(|n| *n > 0)
            .ok_or_else(|| protocol("索引序号无效"))?;
        let column = optional_text(row, p[3])?;
        let prefix = row.get(p[4]).ok_or_else(|| protocol("索引前缀缺失"))?;
        let expr = match expression {
            Some(index) => optional_text(row, index)?,
            None => None,
        };
        let eligible = non_unique == 0
            && prefix.is_null()
            && expr.is_none()
            && column
                .as_ref()
                .is_some_and(|name| columns.iter().any(|c| c.name == *name && !c.nullable));
        groups
            .entry(name.into())
            .or_default()
            .push((sequence, eligible, column.clone()));
        indexes.push(json!({"name":name,"unique":non_unique == 0,"sequence":sequence,"column":column,
            "prefix_length":prefix,"expression":expr,"type":text(row,p[5])?,"collation":optional_text(row,p[6])?}));
    }
    let mut keys = Vec::new();
    if complete {
        for (_, mut entries) in groups {
            entries.sort_by_key(|e| e.0);
            if entries
                .iter()
                .enumerate()
                .all(|(i, e)| e.0 == (i + 1) as u64 && e.1)
            {
                keys.push(entries.into_iter().filter_map(|e| e.2).collect());
            }
        }
    }
    Ok((indexes, keys))
}

async fn ddl(
    session: &ArcherySession,
    database: &str,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<(String, String)> {
    // 上游 describe_table 自行拼接反引号，无法安全承载这些字符；改用本地引用的受控 SHOW。
    let result = if database.contains(['`', '\'', '\\']) || target.name.contains(['`', '\'', '\\'])
    {
        let sql = format!(
            "SHOW CREATE TABLE {}.{}",
            identifier(database)?,
            identifier(&target.name)?
        );
        let response = query::catalog_query(session, database, &sql, sink, cancel).await?;
        json!({"column_list": response.columns, "rows": response.rows})
    } else {
        let connection = session
            .profile
            .archery()
            .ok_or_else(|| AppError::invalid("不是 Archery 连接"))?;
        web_data(
            session
                .auth
                .post_execution(
                    "instance/describetable/",
                    &[
                        ("instance_name", connection.instance_name.clone()),
                        ("db_name", database.into()),
                        ("schema_name", String::new()),
                        ("tb_name", target.name.clone()),
                    ],
                    false,
                    sink,
                    cancel,
                )
                .await?,
        )?
    };
    let cols = result
        .get("column_list")
        .and_then(Json::as_array)
        .ok_or_else(|| protocol("DDL 缺少列定义"))?;
    let rows = result
        .get("rows")
        .and_then(Json::as_array)
        .ok_or_else(|| protocol("DDL 缺少结果"))?;
    if rows.len() != 1 {
        return Err(protocol("DDL 结果行数无效"));
    }
    let row = rows[0].as_array().ok_or_else(|| protocol("DDL 行无效"))?;
    let mut found = None;
    for (i, col) in cols.iter().enumerate() {
        let kind = match col.as_str() {
            Some("Create Table") => Some("table"),
            Some("Create View") => Some("view"),
            _ => None,
        };
        if let Some(kind) = kind {
            if found.is_some() {
                return Err(protocol("DDL 结果列重复"));
            }
            found = Some((
                row.get(i)
                    .and_then(Json::as_str)
                    .ok_or_else(|| protocol("DDL 不是字符串"))?
                    .to_owned(),
                kind.to_owned(),
            ));
        }
    }
    let (ddl, kind) = found.ok_or_else(|| protocol("DDL 缺少 CREATE 列"))?;
    if kind != "view" {
        return Ok((ddl, kind));
    }
    let sql = format!(
        "SHOW CREATE VIEW {}.{}",
        identifier(database)?,
        identifier(&target.name)?
    );
    let response = query::catalog_query(session, database, &sql, sink, cancel).await?;
    let column = positions(&response, &["Create View"])?[0];
    if response.rows.len() != 1 {
        return Err(protocol("视图 DDL 结果行数无效"));
    }
    Ok((text(&response.rows[0], column)?.to_owned(), kind))
}

async fn schema(
    session: &ArcherySession,
    mut target: ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<TableSchema> {
    let database = selected_database(session, target.database.as_deref())?.to_owned();
    identifier(&target.name)?;
    if !names(session, &database, sink, cancel)
        .await?
        .iter()
        .any(|name| name == &target.name)
    {
        return Err(AppError::new(
            "not_found",
            "表或视图不在平台返回的资源列表中",
        ));
    }
    let qualified = format!("{}.{}", identifier(&database)?, identifier(&target.name)?);
    let col_response = query::catalog_query(
        session,
        &database,
        &format!("SHOW FULL COLUMNS FROM {qualified}"),
        sink,
        cancel,
    )
    .await?;
    let col_complete = completeness(&col_response);
    let columns = parse_columns(&col_response)?;
    let (ddl, kind) = ddl(session, &database, &target, sink, cancel).await?;
    let (index_complete, indexes, keys) = if kind == "view" {
        ("not_applicable", Vec::new(), Vec::new())
    } else {
        let response = query::catalog_query(
            session,
            &database,
            &format!("SHOW INDEX FROM {qualified}"),
            sink,
            cancel,
        )
        .await?;
        let complete = completeness(&response);
        let (indexes, keys) = parse_indexes(
            &response,
            &columns,
            col_complete == "complete" && complete == "complete",
        )?;
        (complete, indexes, keys)
    };
    target.database = Some(database);
    target.kind = kind.clone();
    Ok(TableSchema {
        target,
        columns,
        keys,
        ddl,
        engine: String::new(),
        details: json!({
            "relation_kind":kind,"columns_completeness":col_complete,"indexes_completeness":index_complete,
            "metadata_status":if col_complete == "incomplete" || index_complete == "incomplete" { "metadata_incomplete" } else if col_complete == "unknown" || index_complete == "unknown" { "unknown" } else { "complete" },
            "constraints_completeness":"unknown","engine":"unknown","unknown_fields":["engine","constraints","generation_expression","default_provenance"],"indexes":indexes,"constraints":[]
        }),
    })
}

pub(super) async fn describe(
    session: &ArcherySession,
    params: Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    Ok(serde_json::to_value(
        schema(session, param(&params, "target")?, sink, cancel).await?,
    )?)
}

fn literal(input: &Json) -> Result<String> {
    let value = if input.is_object() && input.get("type").is_some() {
        serde_json::from_value::<Value>(input.clone())?
    } else {
        match input {
            Json::Null => Value::Null,
            Json::Bool(v) => Value::Bool(*v),
            Json::Number(v) => Value::Decimal(v.to_string()),
            Json::String(v) => Value::Text(v.clone()),
            _ => return Err(AppError::invalid("过滤值必须是标量或类型化值")),
        }
    };
    match value {
        Value::Bool(v) => Ok(if v { "TRUE" } else { "FALSE" }.into()),
        Value::Integer(v) => {
            v.parse::<i64>()
                .map_err(|_| AppError::invalid("无效整数"))?;
            Ok(v)
        }
        Value::Unsigned(v) => {
            v.parse::<u64>()
                .map_err(|_| AppError::invalid("无效无符号整数"))?;
            Ok(v)
        }
        Value::Decimal(v) => {
            bigdecimal::BigDecimal::from_str(&v).map_err(|_| AppError::invalid("无效小数"))?;
            Ok(v)
        }
        Value::Float(v) => {
            if !v.parse::<f64>().ok().is_some_and(f64::is_finite) {
                return Err(AppError::invalid("无效浮点数"));
            }
            Ok(v)
        }
        Value::Bytes(v) => {
            let bytes = STANDARD
                .decode(v)
                .map_err(|_| AppError::invalid("无效 base64"))?;
            Ok(format!(
                "X'{}'",
                bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
            ))
        }
        Value::Text(v) | Value::Date(v) | Value::Time(v) | Value::DateTime(v) => quoted_literal(&v),
        Value::Json(v) => quoted_literal(&v.to_string()),
        Value::Null => Err(AppError::invalid("NULL 过滤请使用 is_null 或 is_not_null")),
        _ => Err(AppError::invalid("不支持该类型的过滤值")),
    }
}
fn quoted_literal(value: &str) -> Result<String> {
    if value.contains(['\\', '\0']) {
        return Err(AppError::invalid("过滤值包含不支持的字符"));
    }
    Ok(format!("'{}'", value.replace('\'', "''")))
}

fn browse_sql(table: &TableSchema, params: &Json) -> Result<String> {
    if table
        .details
        .get("columns_completeness")
        .and_then(Json::as_str)
        != Some("complete")
        || table.columns.is_empty()
    {
        return Err(AppError::new(
            "metadata_incomplete",
            "字段结构不完整或完整性未知，无法安全浏览",
        ));
    }
    let database = table
        .target
        .database
        .as_deref()
        .ok_or_else(|| AppError::invalid("缺少数据库"))?;
    let projection = table
        .columns
        .iter()
        .map(|c| identifier(&c.name))
        .collect::<Result<Vec<_>>>()?
        .join(",");
    let mut sql = format!(
        "SELECT {projection} FROM {}.{}",
        identifier(database)?,
        identifier(&table.target.name)?
    );
    let columns: HashSet<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    let filters = match params.get("filters") {
        None | Some(Json::Null) => &[][..],
        Some(v) => v
            .as_array()
            .ok_or_else(|| AppError::invalid("filters 必须是数组"))?
            .as_slice(),
    };
    for (i, filter) in filters.iter().enumerate() {
        let column = filter
            .get("column")
            .and_then(Json::as_str)
            .ok_or_else(|| AppError::invalid("过滤条件缺少列名"))?;
        if !columns.contains(column) {
            return Err(AppError::invalid("未知过滤列"));
        }
        let op = filter
            .get("op")
            .and_then(Json::as_str)
            .unwrap_or("eq")
            .to_ascii_lowercase();
        let operator = match op.as_str() {
            "eq" | "=" => "=",
            "ne" | "!=" | "<>" => "<>",
            "lt" | "<" => "<",
            "lte" | "<=" => "<=",
            "gt" | ">" => ">",
            "gte" | ">=" => ">=",
            "like" => "LIKE",
            "not_like" => "NOT LIKE",
            "is_null" => "IS NULL",
            "is_not_null" => "IS NOT NULL",
            _ => return Err(AppError::invalid("不支持的过滤运算符")),
        };
        sql.push_str(if i == 0 { " WHERE " } else { " AND " });
        sql.push_str(&identifier(column)?);
        sql.push(' ');
        sql.push_str(operator);
        if operator != "IS NULL" && operator != "IS NOT NULL" {
            sql.push(' ');
            sql.push_str(&literal(
                filter
                    .get("value")
                    .ok_or_else(|| AppError::invalid("过滤条件缺少值"))?,
            )?);
        }
    }
    let sort = match params.get("sort") {
        None | Some(Json::Null) => &[][..],
        Some(v) => v
            .as_array()
            .ok_or_else(|| AppError::invalid("sort 必须是数组"))?
            .as_slice(),
    };
    let mut orders = Vec::new();
    let mut ordered = HashSet::new();
    for item in sort {
        let column = item
            .get("column")
            .and_then(Json::as_str)
            .ok_or_else(|| AppError::invalid("排序条件缺少列名"))?;
        if !columns.contains(column) || !ordered.insert(column.to_owned()) {
            return Err(AppError::invalid("排序列未知或重复"));
        }
        let direction = item
            .get("direction")
            .and_then(Json::as_str)
            .unwrap_or("asc");
        let direction = if direction.eq_ignore_ascii_case("asc") {
            "ASC"
        } else if direction.eq_ignore_ascii_case("desc") {
            "DESC"
        } else {
            return Err(AppError::invalid("无效排序方向"));
        };
        orders.push(format!("{} {direction}", identifier(column)?));
    }
    if let Some(key) = table.keys.first() {
        for column in key {
            if ordered.insert(column.clone()) {
                orders.push(format!("{} ASC", identifier(column)?));
            }
        }
    }
    if !orders.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(&orders.join(", "));
    }
    Ok(sql)
}

pub(super) async fn browse(
    session: &ArcherySession,
    params: Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    let limit = match params.get("limit") {
        None | Some(Json::Null) => sink.store.settings().await?.archery.request_rows as u64,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=10_000).contains(n))
            .ok_or_else(|| AppError::invalid("limit 必须在 1 到 10000 之间"))?,
    };
    let table = schema(session, param(&params, "target")?, sink, cancel).await?;
    let database = table
        .target
        .database
        .as_deref()
        .ok_or_else(|| AppError::invalid("缺少数据库"))?;
    let sql = browse_sql(&table, &params)?;
    let end = sql.floor_char_boundary(sql.len().min(65536));
    sink.store
        .append_execution_text(
            sink.execution_id,
            &format!("构造的 SQL:\n{}\n", &sql[..end]),
            end < sql.len(),
        )
        .await?;
    let response = query::catalog_browse(session, database, &sql, limit, sink, cancel).await?;
    query::cache_response(session, database, response, limit, sink, cancel).await
}

#[cfg(test)]
#[path = "../../../tests/unit/archery/catalog.rs"]
mod tests;
