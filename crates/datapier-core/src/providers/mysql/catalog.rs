use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::{Arguments, Row, mysql::MySqlArguments};
use tokio_util::sync::CancellationToken;

use super::{
    MysqlSession,
    live::{self, Recipe},
    quote_ident,
};
use crate::{
    ExecutionSink,
    model::{
        AppError, Column, Resource, ResourcePage, ResourceTarget, Result, TableSchema, Value,
        ensure_not_cancelled, param,
    },
};

/// 对完整结构计算指纹，忽略 SHOW CREATE TABLE 中会随插入变化的计数。
pub(super) fn schema_fingerprint(schema: &TableSchema) -> Result<String> {
    let ddl = super::schema::fingerprint(&schema.ddl, "");
    let fields = (
        &schema.target,
        &schema.columns,
        &schema.keys,
        &ddl,
        &schema.engine,
        &schema.details,
    );
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&fields)?)
    ))
}

// EXTRA 中 DEFAULT_GENERATED 仅描述默认值，不能当作生成列。
fn column_flags(extra: &str, expression: Option<&str>) -> (bool, bool) {
    let mut auto_increment = false;
    let mut generated = expression.is_some_and(|value| !value.is_empty());
    for part in extra.split_ascii_whitespace() {
        auto_increment |= part.eq_ignore_ascii_case("auto_increment");
        generated |= part.eq_ignore_ascii_case("generated");
    }
    (auto_increment, generated)
}

async fn database(
    s: &mut MysqlSession,
    requested: Option<String>,
    cancel: &CancellationToken,
) -> Result<String> {
    ensure_not_cancelled(cancel)?;
    if let Some(name) = requested {
        if name.is_empty() {
            return Err(AppError::invalid("数据库名不能为空"));
        }
        return Ok(name);
    }
    // 中断共享连接上的查询后不能继续复用其协议状态；丢弃该连接而非污染后续命令。
    let current = tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            s.connection.take();
            return Err(AppError::cancelled());
        }
        result = sqlx::raw_sql("SELECT DATABASE()").fetch_one(s.connection()?) => result?,
    };
    let current: Option<String> = current.try_get(0)?;
    current.ok_or_else(|| AppError::invalid("请选择数据库"))
}

pub(super) async fn databases(s: &mut MysqlSession, cancel: &CancellationToken) -> Result<Json> {
    // 发现使用独立只读连接；取消时丢弃连接及其未完成的协议状态，保留原会话。
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(AppError::cancelled()),
        result = databases_inner(s) => result,
    }
}

async fn databases_inner(s: &mut MysqlSession) -> Result<Json> {
    let mut conn = s.new_connection().await?;
    let databases: Vec<String> = sqlx::query_scalar(
        "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA ORDER BY SCHEMA_NAME",
    )
    .fetch_all(&mut conn)
    .await
    .map_err(super::errors::mysql_error)?;
    Ok(
        json!({"state":"known","source":"information_schema","databases":databases,"configured_count":null,"reason":null}),
    )
}

#[derive(Clone, Copy)]
enum GrantPatternToken {
    Literal(char),
    One,
    Many,
}

// SHOW GRANTS 的库级 scope 使用 LIKE 模式；反斜线转义和反引号加倍
// 都必须在匹配时保留其含义，不能先去掉反斜线再误认成通配符。
fn grant_pattern_token(
    pattern: &str,
    pos: usize,
) -> std::result::Result<Option<(GrantPatternToken, usize)>, ()> {
    let Some(first) = pattern[pos..].chars().next() else {
        return Ok(None);
    };
    let next = pos + first.len_utf8();
    match first {
        '%' => Ok(Some((GrantPatternToken::Many, next))),
        '_' => Ok(Some((GrantPatternToken::One, next))),
        '\\' | '`' => {
            let escaped = pattern[next..].chars().next().ok_or(())?;
            if first == '`' && escaped != '`' {
                return Err(());
            }
            Ok(Some((
                GrantPatternToken::Literal(escaped),
                next + escaped.len_utf8(),
            )))
        }
        c => Ok(Some((GrantPatternToken::Literal(c), next))),
    }
}

fn grant_schema_matches(scope: &str, database: &str, case_insensitive: bool) -> bool {
    let Some(pattern) = scope.strip_prefix('`').and_then(|s| s.strip_suffix("`.*")) else {
        return false;
    };
    let (mut p, mut d) = (0, 0);
    let mut wildcard = None;
    loop {
        let token = match grant_pattern_token(pattern, p) {
            Ok(token) => token,
            Err(()) => return false,
        };
        if let Some((GrantPatternToken::Many, next)) = token {
            wildcard = Some((next, d));
            p = next;
            continue;
        }
        let current = database[d..].chars().next();
        match (token, current) {
            (None, None) => return true,
            (Some((GrantPatternToken::One, next)), Some(c)) => {
                p = next;
                d += c.len_utf8();
            }
            (Some((GrantPatternToken::Literal(expected), next)), Some(c))
                if expected == c
                    || (case_insensitive
                        && expected.is_ascii()
                        && c.is_ascii()
                        && expected.eq_ignore_ascii_case(&c)) =>
            {
                p = next;
                d += c.len_utf8();
            }
            _ => match wildcard {
                Some((after, previous)) => {
                    let Some(c) = database[previous..].chars().next() else {
                        return false;
                    };
                    d = previous + c.len_utf8();
                    p = after;
                    wildcard = Some((after, d));
                }
                None => return false,
            },
        }
    }
}

pub(super) async fn inspect(s: &mut MysqlSession, params: &Json) -> Result<Json> {
    let objects = params
        .get("objects")
        .and_then(Json::as_array)
        .ok_or_else(|| AppError::invalid("objects 必须是数组"))?;
    let mut conn = s.new_connection().await?;
    let lower_case: u64 = sqlx::query_scalar("SELECT @@lower_case_table_names")
        .fetch_one(&mut conn)
        .await
        .map_err(super::errors::mysql_error)?;
    if !(0..=2).contains(&lower_case) {
        return Err(AppError::new("database", "无法识别 lower_case_table_names"));
    }
    let case_insensitive = lower_case != 0;
    // 只使用 SHOW GRANTS 明确列出的权限，角色继承不作为证明。
    let rows = sqlx::query("SHOW GRANTS")
        .fetch_all(&mut conn)
        .await
        .map_err(super::errors::mysql_error)?;
    let grants: Vec<String> = rows
        .into_iter()
        .map(|row| row.try_get(0))
        .collect::<std::result::Result<_, _>>()?;
    let mut result = Vec::new();
    for object in objects {
        let database: String = param(object, "database")?;
        let name: String = param(object, "name")?;
        quote_ident(&database)?;
        quote_ident(&name)?;
        let write = object.get("write").and_then(Json::as_bool).unwrap_or(false);
        let table_scope = format!("{}.{}", quote_ident(&database)?, quote_ident(&name)?);
        let mut schema_select = false;
        let mut global_select = false;
        let mut object_select = false;
        let mut show_view = false;
        let mut trigger = false;
        for grant in &grants {
            let Some((permissions, rest)) = grant.split_once(" ON ") else {
                continue;
            };
            let Some((scope, _)) = rest.split_once(" TO ") else {
                continue;
            };
            let global = scope == "*.*";
            let schema = grant_schema_matches(scope, &database, case_insensitive);
            let object = scope == table_scope
                || (case_insensitive && scope.eq_ignore_ascii_case(&table_scope));
            if !global && !schema && !object {
                continue;
            }
            let permissions = permissions.to_ascii_uppercase();
            let all = permissions.contains("ALL PRIVILEGES");
            let has_select = all
                || permissions
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .any(|p| p == "SELECT");
            global_select |= global && has_select;
            schema_select |= (global || schema) && has_select;
            object_select |= has_select;
            show_view |= all || permissions.contains("SHOW VIEW");
            trigger |= all
                || permissions
                    .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                    .any(|p| p == "TRIGGER");
        }
        let table = sqlx::query("SELECT TABLE_SCHEMA,TABLE_NAME,TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA=? AND TABLE_NAME=?")
            .bind(&database).bind(&name).fetch_optional(&mut conn).await.map_err(super::errors::mysql_error)?;
        let Some(table) = table else {
            let complete = schema_select;
            result.push(json!({"database":database,"name":name,"write":write,"kind":"unknown","exists":false,"view_definition":null,"triggers":[],"foreign_keys":[],"complete":complete,"reason":if complete { None } else { Some("对象不存在或元数据不可见") }}));
            continue;
        };
        let actual_database: String = table.try_get("TABLE_SCHEMA")?;
        let actual_name: String = table.try_get("TABLE_NAME")?;
        let same = |left: &str, right: &str| {
            left == right || (case_insensitive && left.eq_ignore_ascii_case(right))
        };
        if !same(&actual_database, &database) || !same(&actual_name, &name) {
            result.push(json!({"database":database,"name":name,"write":write,"kind":"unknown","exists":null,"complete":false,"reason":"元数据对象标识符不匹配"}));
            continue;
        }
        let kind = if table
            .try_get::<String, _>("TABLE_TYPE")?
            .eq_ignore_ascii_case("VIEW")
        {
            "view"
        } else {
            "table"
        };
        let definition: Option<String> = if kind == "view" {
            sqlx::query_scalar("SELECT VIEW_DEFINITION FROM information_schema.VIEWS WHERE TABLE_SCHEMA=? AND TABLE_NAME=?")
                .bind(&actual_database).bind(&actual_name).fetch_optional(&mut conn).await.map_err(super::errors::mysql_error)?
        } else {
            None
        };
        let trigger_rows = if write {
            sqlx::query("SELECT TRIGGER_NAME, ACTION_STATEMENT FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA=? AND EVENT_OBJECT_TABLE=?")
            .bind(&actual_database).bind(&actual_name).fetch_all(&mut conn).await.map_err(super::errors::mysql_error)?
        } else {
            Vec::new()
        };
        let mut triggers = Vec::new();
        for row in trigger_rows {
            triggers.push(json!({"name":row.try_get::<String,_>("TRIGGER_NAME")?,"definition":row.try_get::<Option<String>,_>("ACTION_STATEMENT")?}));
        }
        // 同时收集出向和入向外键；后者可能通过 CASCADE 修改其他表。
        // 全局 SELECT 是证明跨库入向引用未被元数据权限过滤的前提。
        let fk_rows = if write {
            sqlx::query("SELECT k.TABLE_SCHEMA,k.TABLE_NAME,k.REFERENCED_TABLE_SCHEMA,k.REFERENCED_TABLE_NAME,r.UPDATE_RULE,r.DELETE_RULE FROM information_schema.KEY_COLUMN_USAGE k LEFT JOIN information_schema.REFERENTIAL_CONSTRAINTS r ON r.CONSTRAINT_SCHEMA=k.CONSTRAINT_SCHEMA AND r.TABLE_NAME=k.TABLE_NAME AND r.CONSTRAINT_NAME=k.CONSTRAINT_NAME WHERE k.REFERENCED_TABLE_NAME IS NOT NULL AND ((k.TABLE_SCHEMA=? AND k.TABLE_NAME=?) OR (k.REFERENCED_TABLE_SCHEMA=? AND k.REFERENCED_TABLE_NAME=?))")
            .bind(&actual_database).bind(&actual_name).bind(&actual_database).bind(&actual_name).fetch_all(&mut conn).await.map_err(super::errors::mysql_error)?
        } else {
            Vec::new()
        };
        let mut foreign_keys = Vec::new();
        let mut foreign_keys_complete = true;
        for row in fk_rows {
            let source_db: String = row.try_get("TABLE_SCHEMA")?;
            let source_name: String = row.try_get("TABLE_NAME")?;
            let target_db: String = row.try_get("REFERENCED_TABLE_SCHEMA")?;
            let target_name: String = row.try_get("REFERENCED_TABLE_NAME")?;
            let outgoing = same(&source_db, &actual_database) && same(&source_name, &actual_name);
            let incoming = same(&target_db, &actual_database) && same(&target_name, &actual_name);
            let update_rule: Option<String> = row.try_get("UPDATE_RULE")?;
            let delete_rule: Option<String> = row.try_get("DELETE_RULE")?;
            let effect = |rule: Option<&str>| match rule {
                Some("CASCADE" | "SET NULL") => Some(true),
                Some("RESTRICT" | "NO ACTION") => Some(false),
                _ => None,
            };
            let update_write = effect(update_rule.as_deref());
            let delete_write = effect(delete_rule.as_deref());
            foreign_keys_complete &= update_write.is_some() && delete_write.is_some();
            if outgoing {
                foreign_keys.push(json!({"database":target_db,"name":target_name,"write":false}));
            }
            if incoming {
                foreign_keys.push(json!({"database":source_db,"name":source_name,"write":update_write.unwrap_or(true) || delete_write.unwrap_or(true)}));
            }
        }
        let complete = object_select
            && (!write || (global_select && trigger))
            && (kind != "view" || (show_view && definition.is_some()))
            && foreign_keys_complete
            && triggers.iter().all(|t| !t["definition"].is_null());
        result.push(json!({"database":database,"name":name,"actual_database":actual_database,"actual_name":actual_name,"write":write,"kind":kind,"exists":true,"view_definition":definition,"triggers":triggers,"foreign_keys":foreign_keys,"complete":complete,"reason":if complete { None } else { Some("无法证明视图、触发器及跨库外键元数据完整可见") }}));
    }
    Ok(json!({"objects":result}))
}

pub(super) async fn list(
    s: &mut MysqlSession,
    params: Json,
    cancel: &CancellationToken,
) -> Result<Json> {
    // 独立连接只用于读取；取消时直接丢弃 future 和连接，释放会话锁。
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(AppError::cancelled()),
        result = list_inner(s, params) => result,
    }
}

async fn list_inner(s: &mut MysqlSession, params: Json) -> Result<Json> {
    let requested = match params.get("database") {
        None | Some(Json::Null) => None,
        Some(v) => Some(serde_json::from_value::<String>(v.clone())?),
    };
    let mut conn = s.new_connection().await?;
    let mut resources = Vec::new();
    if params.get("kind").and_then(Json::as_str) == Some("trigger") {
        let db = requested
            .as_deref()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| AppError::invalid("列出触发器必须指定数据库"))?;
        let table: String = param(&params, "table")?;
        if table.is_empty() {
            return Err(AppError::invalid("列出触发器必须指定所属表"));
        }
        let object = sqlx::query("SELECT TABLE_SCHEMA, TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND TABLE_TYPE = 'BASE TABLE'")
            .bind(db).bind(&table).fetch_optional(&mut conn).await?
            .ok_or_else(|| AppError::new("not_found", "表不存在或元数据不可见"))?;
        let actual_db: String = object.try_get("TABLE_SCHEMA")?;
        let actual_table: String = object.try_get("TABLE_NAME")?;
        let lower_case: u64 = sqlx::query_scalar("SELECT @@lower_case_table_names")
            .fetch_one(&mut conn)
            .await?;
        let grants = sqlx::query("SHOW GRANTS").fetch_all(&mut conn).await?;
        let scope = format!(
            "{}.{}",
            quote_ident(&actual_db)?,
            quote_ident(&actual_table)?
        );
        // TRIGGERS 会按权限过滤；只有明确的 TRIGGER 授权才能证明空列表完整。
        let mut complete = false;
        for row in grants {
            let grant: String = row.try_get(0)?;
            let Some((permissions, rest)) = grant.split_once(" ON ") else {
                continue;
            };
            let Some((grant_scope, _)) = rest.split_once(" TO ") else {
                continue;
            };
            let matches = grant_scope == "*.*"
                || grant_schema_matches(grant_scope, &actual_db, lower_case != 0)
                || grant_scope == scope
                || (lower_case != 0 && grant_scope.eq_ignore_ascii_case(&scope));
            let permissions = permissions.to_ascii_uppercase();
            if matches
                && (permissions.contains("ALL PRIVILEGES")
                    || permissions
                        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                        .any(|p| p == "TRIGGER"))
            {
                complete = true;
            }
        }
        let rows = sqlx::query("SELECT TRIGGER_SCHEMA, TRIGGER_NAME, EVENT_OBJECT_TABLE, ACTION_STATEMENT FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA = ? AND EVENT_OBJECT_TABLE = ? ORDER BY TRIGGER_NAME")
            .bind(&actual_db).bind(&actual_table).fetch_all(&mut conn).await?;
        for row in rows {
            let definition: Option<String> = row.try_get("ACTION_STATEMENT")?;
            complete &= definition.is_some();
            resources.push(Resource {
                name: row.try_get("TRIGGER_NAME")?,
                kind: "trigger".into(),
                database: Some(row.try_get("TRIGGER_SCHEMA")?),
                key_b64: None,
                data: json!({"table": row.try_get::<String, _>("EVENT_OBJECT_TABLE")?, "definition_visible": definition.is_some()}),
            });
        }
        return Ok(serde_json::to_value(ResourcePage {
            resources,
            cursor: None,
            complete,
            resource_limited: false,
        })?);
    }
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
        resource_limited: false,
    })?)
}

pub(super) async fn describe(
    s: &mut MysqlSession,
    params: Json,
    cancel: &CancellationToken,
) -> Result<Json> {
    let target = param(&params, "target")?;
    Ok(serde_json::to_value(schema(s, target, cancel).await?)?)
}

pub(super) async fn schema(
    s: &mut MysqlSession,
    target: ResourceTarget,
    cancel: &CancellationToken,
) -> Result<TableSchema> {
    let mut schema = schema_raw(s, target, cancel).await?;
    if schema.target.kind == "view" {
        match super::view::resolve(s, schema.clone(), cancel).await {
            Ok(mapping) => schema = mapping.schema,
            Err(error) => {
                ensure_not_cancelled(cancel)?;
                schema.details["view_mapping"] = json!({"insert": false, "update": false, "delete": false, "reasons": [error.to_string()]});
            }
        }
    }
    Ok(schema)
}

pub(super) async fn schema_raw(
    s: &mut MysqlSession,
    mut target: ResourceTarget,
    cancel: &CancellationToken,
) -> Result<TableSchema> {
    let db = database(s, target.database.take(), cancel).await?;
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(AppError::cancelled()),
        result = schema_inner(s, target, db) => result,
    }
}

async fn schema_inner(
    s: &mut MysqlSession,
    mut target: ResourceTarget,
    db: String,
) -> Result<TableSchema> {
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

    let rows = sqlx::query("SELECT COLUMN_NAME, COLUMN_TYPE, IS_NULLABLE, COLUMN_KEY, COLUMN_DEFAULT, COLUMN_COMMENT, EXTRA, GENERATION_EXPRESSION FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION")
        .bind(&db).bind(&name).fetch_all(&mut conn).await?;
    let mut columns = Vec::with_capacity(rows.len());
    for row in rows {
        let extra: String = row.try_get("EXTRA")?;
        let expression: Option<String> = row.try_get("GENERATION_EXPRESSION")?;
        let key: String = row.try_get("COLUMN_KEY")?;
        let (auto_increment, generated) = column_flags(&extra, expression.as_deref());
        columns.push(Column {
            name: row.try_get("COLUMN_NAME")?,
            type_name: row.try_get("COLUMN_TYPE")?,
            nullable: row.try_get::<String, _>("IS_NULLABLE")? == "YES",
            generated,
            auto_increment,
            primary_key: key == "PRI",
            default_value: row.try_get("COLUMN_DEFAULT")?,
            comment: Some(row.try_get("COLUMN_COMMENT")?),
        });
    }
    let index_sql = if s.has_index_expression {
        "SELECT INDEX_NAME, CAST(NON_UNIQUE AS SIGNED) AS NON_UNIQUE, CAST(SEQ_IN_INDEX AS SIGNED) AS SEQ_IN_INDEX, COLUMN_NAME, CAST(SUB_PART AS SIGNED) AS SUB_PART, EXPRESSION, INDEX_TYPE, COLLATION FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY (INDEX_NAME = 'PRIMARY') DESC, INDEX_NAME, SEQ_IN_INDEX"
    } else {
        "SELECT INDEX_NAME, CAST(NON_UNIQUE AS SIGNED) AS NON_UNIQUE, CAST(SEQ_IN_INDEX AS SIGNED) AS SEQ_IN_INDEX, COLUMN_NAME, CAST(SUB_PART AS SIGNED) AS SUB_PART, NULL AS EXPRESSION, INDEX_TYPE, COLLATION FROM information_schema.STATISTICS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY (INDEX_NAME = 'PRIMARY') DESC, INDEX_NAME, SEQ_IN_INDEX"
    };
    let rows = sqlx::query(index_sql)
        .bind(&db)
        .bind(&name)
        .fetch_all(&mut conn)
        .await?;
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

pub(super) fn add_filter_arg(args: &mut MySqlArguments, value: &Json) -> Result<()> {
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
        Value::Null => args.add(Option::<String>::None),
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
    let table = schema(s, target, cancel).await?;
    ensure_not_cancelled(cancel)?;
    let db = table
        .target
        .database
        .as_deref()
        .ok_or_else(|| AppError::invalid("缺少数据库"))?;
    // 显式投影 catalog 的列，包含 MySQL 8 的 INVISIBLE 列，保持数据与结构顺序一致。
    let mut sql = String::from("SELECT ");
    for (index, column) in table.columns.iter().enumerate() {
        if index != 0 {
            sql.push(',');
        }
        sql.push_str(&quote_ident(&column.name)?);
    }
    sql.push_str(" FROM ");
    sql.push_str(&quote_ident(db)?);
    sql.push('.');
    sql.push_str(&quote_ident(&table.target.name)?);
    let columns: HashSet<&str> = table.columns.iter().map(|c| c.name.as_str()).collect();
    let mut bindings = Vec::new();
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
            bindings.push(value.clone());
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
    let end = sql.floor_char_boundary(sql.len().min(65536));
    sink.store
        .append_execution_text(
            sink.execution_id,
            &format!("构造的 SQL:\n{}\n", &sql[..end]),
            end < sql.len(),
        )
        .await?;
    let size = params
        .get("page_size")
        .and_then(Json::as_u64)
        .unwrap_or(sink.store.settings().await?.paging.mysql_table_rows as u64)
        .clamp(1, 1000) as usize;
    let context = crate::policy::SqlContext {
        database: Some(db.to_owned()),
        ..Default::default()
    };
    let objects = vec![crate::policy::ObjectAccess {
        database: db.to_owned(),
        name: table.target.name.clone(),
        write: false,
    }];
    if !live::dependencies_replayable(s, objects.clone(), &context, true, cancel).await? {
        let mut conn = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AppError::cancelled()),
            result = s.new_connection() => result?,
        };
        sink.mark_sent(false).await?;
        let (ids, affected) = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AppError::cancelled()),
            result = super::results::execute_statement(&mut conn, &sql, sink, cancel, false, size, &bindings) => result?,
        };
        return Ok(json!({"results": ids, "affected_rows": affected}));
    }
    let recipe = Recipe {
        sql,
        bindings,
        source_columns: Some(table.columns),
        independent: true,
        context: None,
        proof: Some((objects, context)),
    };
    live::start(s, recipe, size, sink, cancel).await
}

#[cfg(test)]
#[path = "../../../tests/unit/mysql/catalog.rs"]
mod column_flag_tests;
