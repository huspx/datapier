use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value as Json, json};
use sqlx::{Arguments, Row, mysql::MySqlArguments};
use tokio_util::sync::CancellationToken;

use super::{MysqlSession, quote_ident, stream_query};
use crate::{
    ExecutionSink,
    model::{
        AppError, Column, Resource, ResourcePage, ResourceTarget, Result, TableSchema, Value,
        ensure_not_cancelled, param,
    },
};

async fn database(s: &mut MysqlSession, requested: Option<String>) -> Result<String> {
    if let Some(name) = requested.or_else(|| s.profile.database.clone()) {
        if name.is_empty() {
            return Err(AppError::invalid("数据库名不能为空"));
        }
        return Ok(name);
    }
    let current: Option<String> = sqlx::query_scalar("SELECT DATABASE()")
        .fetch_one(s.connection()?)
        .await?;
    current.ok_or_else(|| AppError::invalid("请选择数据库"))
}

pub(super) async fn list(s: &mut MysqlSession, params: Json) -> Result<Json> {
    let requested = match params.get("database") {
        None | Some(Json::Null) => None,
        Some(v) => Some(serde_json::from_value::<String>(v.clone())?),
    };
    let mut conn = s.new_connection().await?;
    let mut resources = Vec::new();
    if let Some(db) = requested {
        let rows = sqlx::query("SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, ENGINE FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? ORDER BY TABLE_NAME")
            .bind(&db).fetch_all(&mut conn).await?;
        for row in rows {
            let table_type: String = row.try_get("TABLE_TYPE")?;
            let kind = if table_type.eq_ignore_ascii_case("VIEW") {
                "view"
            } else {
                "table"
            };
            let engine: Option<String> = row.try_get("ENGINE")?;
            resources.push(Resource {
                name: row.try_get("TABLE_NAME")?,
                kind: kind.into(),
                database: Some(row.try_get("TABLE_SCHEMA")?),
                key_b64: None,
                data: json!({"engine": engine}),
            });
        }
    } else {
        let rows =
            sqlx::query("SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME")
                .fetch_all(&mut conn)
                .await?;
        for row in rows {
            let name: String = row.try_get("SCHEMA_NAME")?;
            resources.push(Resource {
                name: name.clone(),
                kind: "database".into(),
                database: Some(name),
                key_b64: None,
                data: Json::Null,
            });
        }
    }
    Ok(serde_json::to_value(ResourcePage {
        resources,
        cursor: None,
        complete: true,
    })?)
}

pub(super) async fn describe(s: &mut MysqlSession, params: Json) -> Result<Json> {
    let target = param(&params, "target")?;
    Ok(serde_json::to_value(schema(s, target).await?)?)
}

pub(super) async fn schema(
    s: &mut MysqlSession,
    mut target: ResourceTarget,
) -> Result<TableSchema> {
    let db = database(s, target.database.take()).await?;
    if target.name.is_empty() {
        return Err(AppError::invalid("表名不能为空"));
    }
    let mut conn = s.new_connection().await?;
    let table = sqlx::query("SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE, ENGINE FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?")
        .bind(&db).bind(&target.name).fetch_optional(&mut conn).await?
        .ok_or_else(|| AppError::new("not_found", "表或视图不存在，或没有访问权限"))?;
    let db: String = table.try_get("TABLE_SCHEMA")?;
    let name: String = table.try_get("TABLE_NAME")?;
    let table_type: String = table.try_get("TABLE_TYPE")?;
    let is_view = table_type.eq_ignore_ascii_case("VIEW");
    let engine: Option<String> = table.try_get("ENGINE")?;
    target.database = Some(db.clone());
    target.name = name.clone();
    target.kind = if is_view { "view" } else { "table" }.into();

    let rows = sqlx::query("SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, COLUMN_DEFAULT, EXTRA, GENERATION_EXPRESSION FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION")
        .bind(&db).bind(&name).fetch_all(&mut conn).await?;
    let mut columns = Vec::with_capacity(rows.len());
    for row in rows {
        let extra: String = row.try_get("EXTRA")?;
        let expression: Option<String> = row.try_get("GENERATION_EXPRESSION")?;
        let key: String = row.try_get("COLUMN_KEY")?;
        columns.push(Column {
            name: row.try_get("COLUMN_NAME")?,
            type_name: row.try_get("COLUMN_TYPE")?,
            nullable: row.try_get::<String, _>("IS_NULLABLE")? == "YES",
            generated: extra.to_ascii_lowercase().contains("generated")
                && !extra.to_ascii_lowercase().contains("default_generated")
                || expression.is_some_and(|v| !v.is_empty()),
            primary_key: key == "PRI",
            default_value: row.try_get("COLUMN_DEFAULT")?,
        });
    }
    let rows = sqlx::query("SELECT INDEX_NAME, CAST(NON_UNIQUE AS SIGNED) AS NON_UNIQUE, CAST(SEQ_IN_INDEX AS SIGNED) AS SEQ_IN_INDEX, COLUMN_NAME, CAST(SUB_PART AS SIGNED) AS SUB_PART, EXPRESSION, INDEX_TYPE, COLLATION FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY (INDEX_NAME = 'PRIMARY') DESC, INDEX_NAME, SEQ_IN_INDEX")
        .bind(&db).bind(&name).fetch_all(&mut conn).await?;
    let nullable: HashMap<&str, bool> = columns
        .iter()
        .map(|c| (c.name.as_str(), c.nullable))
        .collect();
    let mut indexes: Vec<Json> = Vec::with_capacity(rows.len());
    let mut candidates: Vec<(String, bool, Vec<String>)> = Vec::new();
    for row in rows {
        let index: String = row.try_get("INDEX_NAME")?;
        let non_unique: i64 = row.try_get("NON_UNIQUE")?;
        let column: Option<String> = row.try_get("COLUMN_NAME")?;
        let prefix: Option<i64> = row.try_get("SUB_PART")?;
        let expression: Option<String> = row.try_get("EXPRESSION")?;
        let seq: i64 = row.try_get("SEQ_IN_INDEX")?;
        let index_type: String = row.try_get("INDEX_TYPE")?;
        let collation: Option<String> = row.try_get("COLLATION")?;
        indexes.push(json!({"name":index, "unique":non_unique == 0, "sequence":seq, "column":column, "prefix_length":prefix, "expression":expression, "type":index_type, "collation":collation}));
        // 唯一索引只有全部列均非 NULL 且不是前缀或表达式时才能可靠定位行。
        // 任一索引列不合格就排除整个复合键，而不是保留部分列。
        if candidates.last().is_none_or(|v| v.0 != index) {
            candidates.push((index.clone(), non_unique == 0, Vec::new()));
        }
        let candidate = candidates.last_mut().expect("index candidate exists");
        if prefix.is_some()
            || expression.is_some()
            || column
                .as_deref()
                .is_none_or(|c| nullable.get(c).copied() != Some(false))
        {
            candidate.1 = false;
        }
        if let Some(column) = column {
            candidate.2.push(column);
        }
    }
    let keys = if is_view {
        Vec::new()
    } else {
        candidates
            .into_iter()
            .filter_map(|(_, valid, cols)| (valid && !cols.is_empty()).then_some(cols))
            .collect()
    };
    let rows = sqlx::query("SELECT tc.CONSTRAINT_NAME, tc.CONSTRAINT_TYPE, kcu.COLUMN_NAME, CAST(kcu.ORDINAL_POSITION AS SIGNED) AS ORDINAL_POSITION, kcu.REFERENCED_TABLE_SCHEMA, kcu.REFERENCED_TABLE_NAME, kcu.REFERENCED_COLUMN_NAME FROM information_schema.TABLE_CONSTRAINTS tc LEFT JOIN information_schema.KEY_COLUMN_USAGE kcu ON tc.CONSTRAINT_SCHEMA = kcu.CONSTRAINT_SCHEMA AND tc.TABLE_SCHEMA = kcu.TABLE_SCHEMA AND tc.TABLE_NAME = kcu.TABLE_NAME AND tc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME WHERE tc.TABLE_SCHEMA = ? AND tc.TABLE_NAME = ? ORDER BY tc.CONSTRAINT_NAME, kcu.ORDINAL_POSITION")
        .bind(&db).bind(&name).fetch_all(&mut conn).await?;
    let mut constraints = Vec::with_capacity(rows.len());
    for row in rows {
        constraints.push(json!({"name":row.try_get::<String, _>("CONSTRAINT_NAME")?, "type":row.try_get::<String, _>("CONSTRAINT_TYPE")?, "column":row.try_get::<Option<String>, _>("COLUMN_NAME")?, "sequence":row.try_get::<Option<i64>, _>("ORDINAL_POSITION")?, "referenced_database":row.try_get::<Option<String>, _>("REFERENCED_TABLE_SCHEMA")?, "referenced_table":row.try_get::<Option<String>, _>("REFERENCED_TABLE_NAME")?, "referenced_column":row.try_get::<Option<String>, _>("REFERENCED_COLUMN_NAME")?}));
    }
    let db_ident = quote_ident(&db)?;
    let table_ident = quote_ident(&name)?;
    let ddl_sql = format!(
        "SHOW CREATE {} {}.{}",
        if is_view { "VIEW" } else { "TABLE" },
        db_ident,
        table_ident
    );
    let ddl_row = sqlx::query(sqlx::AssertSqlSafe(ddl_sql))
        .fetch_one(&mut conn)
        .await?;
    let ddl: String = ddl_row.try_get(1)?;
    Ok(TableSchema {
        target,
        columns,
        keys,
        ddl,
        engine: engine.unwrap_or_default(),
        details: json!({"indexes":indexes,"constraints":constraints}),
    })
}

fn add_filter_arg(args: &mut MySqlArguments, value: &Json) -> Result<()> {
    let value = if value.is_object() && value.get("type").is_some() {
        serde_json::from_value::<Value>(value.clone())?
    } else {
        match value {
            Json::Null => Value::Null,
            Json::Bool(v) => Value::Bool(*v),
            Json::Number(v) => Value::Decimal(v.to_string()),
            Json::String(v) => Value::Text(v.clone()),
            _ => return Err(AppError::invalid("过滤值必须是标量或类型化值")),
        }
    };
    let result = match value {
        Value::Bool(v) => args.add(v),
        Value::Integer(v) => args.add(
            v.parse::<i64>()
                .map_err(|_| AppError::invalid("无效的整数过滤值"))?,
        ),
        Value::Unsigned(v) => args.add(
            v.parse::<u64>()
                .map_err(|_| AppError::invalid("无效的无符号整数过滤值"))?,
        ),
        Value::Decimal(v) => args.add(
            bigdecimal::BigDecimal::from_str(&v)
                .map_err(|_| AppError::invalid("无效的小数过滤值"))?,
        ),
        Value::Float(v) => {
            let n = v
                .parse::<f64>()
                .map_err(|_| AppError::invalid("无效的浮点过滤值"))?;
            if !n.is_finite() {
                return Err(AppError::invalid("浮点过滤值必须有限"));
            }
            args.add(n)
        }
        Value::Bytes(v) => args.add(
            STANDARD
                .decode(v)
                .map_err(|_| AppError::invalid("无效的 base64 过滤值"))?,
        ),
        Value::Text(v) | Value::Date(v) | Value::Time(v) | Value::DateTime(v) => args.add(v),
        Value::Json(v) => args.add(v.to_string()),
        _ => return Err(AppError::invalid("不支持该类型的过滤值")),
    };
    result.map_err(|e| AppError::invalid(format!("无法绑定过滤值: {e}")))
}

pub(super) async fn browse(
    s: &mut MysqlSession,
    params: Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    ensure_not_cancelled(cancel)?;
    let target: ResourceTarget = param(&params, "target")?;
    let table = schema(s, target).await?;
    ensure_not_cancelled(cancel)?;
    let db = table
        .target
        .database
        .as_deref()
        .ok_or_else(|| AppError::invalid("缺少数据库"))?;
    let mut sql = format!(
        "SELECT * FROM {}.{}",
        quote_ident(db)?,
        quote_ident(&table.target.name)?
    );
    let columns: HashSet<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    let mut args = MySqlArguments::default();
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
            return Err(AppError::invalid(format!("未知列: {column}")));
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
        sql.push_str(&quote_ident(column)?);
        sql.push(' ');
        sql.push_str(operator);
        if operator != "IS NULL" && operator != "IS NOT NULL" {
            let value = filter
                .get("value")
                .ok_or_else(|| AppError::invalid("过滤条件缺少值"))?;
            let is_null =
                value.is_null() || value.get("type").and_then(Json::as_str) == Some("null");
            if is_null {
                return Err(AppError::invalid("NULL 过滤请使用 is_null 或 is_not_null"));
            }
            add_filter_arg(&mut args, value)?;
            sql.push_str(" ?");
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
        if !columns.contains(column) {
            return Err(AppError::invalid(format!("未知列: {column}")));
        }
        if !ordered.insert(column.to_owned()) {
            return Err(AppError::invalid("排序列重复"));
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
            return Err(AppError::invalid("无效的排序方向"));
        };
        orders.push(format!("{} {direction}", quote_ident(column)?));
    }
    if let Some(key) = table.keys.first() {
        for column in key {
            if ordered.insert(column.clone()) {
                orders.push(format!("{} ASC", quote_ident(column)?));
            }
        }
    }
    if !orders.is_empty() {
        sql.push_str(" ORDER BY ");
        sql.push_str(&orders.join(", "));
    }
    let mut conn = s.new_connection().await?;
    stream_query(&mut conn, &sql, args, sink, cancel).await
}
