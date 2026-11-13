//! MySQL 结构快照与独立连接上的 DDL 执行。
mod catalog;
mod dependencies;
mod plan;
mod verify;
use super::MysqlSession;
use crate::model::{AppError, Result, schema::*};
use crate::{
    ExecutionSink,
    model::{ResourceTarget, ensure_not_cancelled, param},
};
pub use plan::build_plan;
use plan::{qualified, version};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::{MySqlConnection, Row};
use tokio_util::sync::CancellationToken;

pub(super) fn fingerprint(ddl: &str, mode: &str) -> String {
    // 仅移除 CREATE TABLE 顶层选项的计数；字符串、标识符与列定义原样保留。
    let bytes = ddl.as_bytes();
    let mut range = None;
    if ddl.starts_with("CREATE TABLE ") {
        let mut depth = 0usize;
        let mut quote = 0u8;
        let mut i = 0usize;
        while i < bytes.len() {
            let b = bytes[i];
            if quote != 0 {
                if b == b'\\'
                    && quote != b'`'
                    && !mode
                        .split(',')
                        .any(|v| v.trim().eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"))
                {
                    i += 1;
                } else if b == quote {
                    if bytes.get(i + 1) == Some(&quote) {
                        i += 1;
                    } else {
                        quote = 0;
                    }
                }
            } else if b == b'\'' || b == b'"' || b == b'`' {
                quote = b;
            } else if b == b'(' {
                depth += 1;
            } else if b == b')' {
                depth = depth.saturating_sub(1);
            } else if depth == 0 && bytes[i..].starts_with(b" AUTO_INCREMENT=") {
                let start = i;
                i += b" AUTO_INCREMENT=".len();
                let digits = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i > digits {
                    range = Some(start..i);
                }
                break;
            }
            i += 1;
        }
    }
    let mut hash = Sha256::new();
    if let Some(range) = range {
        hash.update(&bytes[..range.start]);
        hash.update(&bytes[range.end..]);
    } else {
        hash.update(bytes);
    }
    hash.update(b"\0");
    hash.update(mode.as_bytes());
    format!("{:x}", hash.finalize())
}
fn capability(available: bool, reason: &str) -> SchemaCapability {
    SchemaCapability {
        state: if available { "unknown" } else { "unsupported" }.into(),
        reason: Some(
            if available {
                "服务端版本支持，但元数据完整性及执行权限尚未证明"
            } else {
                reason
            }
            .into(),
        ),
    }
}
fn capabilities(version_str: &str) -> std::collections::BTreeMap<String, SchemaCapability> {
    let v = version(version_str);
    [
        ("check", v >= (8, 0, 16)),
        ("expression_default", v >= (8, 0, 13)),
        ("functional_index", v >= (8, 0, 13)),
        ("descending_index", v >= (8, 0, 0)),
        ("index_visibility", v >= (8, 0, 0)),
        ("invisible_column", v >= (8, 0, 23)),
    ]
    .into_iter()
    .map(|(k, available)| (k.into(), capability(available, "当前服务端版本不支持")))
    .collect()
}
async fn describe_on(
    conn: &mut MySqlConnection,
    target: ResourceTarget,
) -> Result<Option<SchemaSnapshot>> {
    if target.kind.eq_ignore_ascii_case("database") {
        let row = sqlx::query("SELECT SCHEMA_NAME, DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?")
            .bind(&target.name).fetch_optional(&mut *conn).await?;
        let Some(row) = row else { return Ok(None) };
        let context = sqlx::query("SELECT VERSION() AS version, @@SESSION.sql_mode AS sql_mode")
            .fetch_one(&mut *conn)
            .await?;
        let server_version: String = context.try_get("version")?;
        let sql_mode: String = context.try_get("sql_mode")?;
        let name = super::quote_ident(&target.name)?;
        let ddlrow = sqlx::query(sqlx::AssertSqlSafe(format!("SHOW CREATE DATABASE {name}")))
            .fetch_one(&mut *conn)
            .await?;
        let ddl: String = ddlrow.try_get(1)?;
        return Ok(Some(SchemaSnapshot {
            definition: SchemaDefinition::Database(DatabaseDefinition {
                charset: Some(row.try_get("DEFAULT_CHARACTER_SET_NAME")?),
                collation: Some(row.try_get("DEFAULT_COLLATION_NAME")?),
            }),
            fingerprint: fingerprint(&ddl, &sql_mode),
            target,
            server_version: server_version.clone(),
            sql_mode,
            ddl,
            dependencies: vec![],
            completeness: Completeness {
                state: CompletenessState::Complete,
                reasons: vec![],
            },
            capabilities: capabilities(&server_version),
        }));
    }
    let db = target
        .database
        .as_deref()
        .filter(|x| !x.is_empty())
        .ok_or_else(|| AppError::invalid("必须指定数据库"))?;
    let context = sqlx::query("SELECT VERSION() AS version, @@SESSION.sql_mode AS sql_mode")
        .fetch_one(&mut *conn)
        .await?;
    let server_version: String = context.try_get("version")?;
    let sql_mode: String = context.try_get("sql_mode")?;
    let name = qualified(db, &target.name)?;
    let (definition, ddl, mut completeness) = if target.kind.eq_ignore_ascii_case("trigger") {
        let row = sqlx::query("SELECT EVENT_OBJECT_TABLE, ACTION_TIMING, EVENT_MANIPULATION, ACTION_STATEMENT, DEFINER, SQL_MODE, CHARACTER_SET_CLIENT, COLLATION_CONNECTION, CAST(ACTION_ORDER AS SIGNED) AS ACTION_ORDER FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA=? AND TRIGGER_NAME=?").bind(db).bind(&target.name).fetch_optional(&mut *conn).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let ddlrow = sqlx::query(sqlx::AssertSqlSafe(format!("SHOW CREATE TRIGGER {name}")))
            .fetch_one(&mut *conn)
            .await?;
        let ddl: String = ddlrow.try_get(2)?;
        let timing: String = row.try_get("ACTION_TIMING")?;
        let event: String = row.try_get("EVENT_MANIPULATION")?;
        let table: String = row.try_get("EVENT_OBJECT_TABLE")?;
        let action_order: i64 = row.try_get("ACTION_ORDER")?;
        let previous: Option<String> = if action_order > 1 {
            sqlx::query_scalar("SELECT TRIGGER_NAME FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA=? AND EVENT_OBJECT_TABLE=? AND ACTION_TIMING=? AND EVENT_MANIPULATION=? AND ACTION_ORDER < ? ORDER BY ACTION_ORDER DESC LIMIT 1").bind(db).bind(&table).bind(&timing).bind(&event).bind(action_order).fetch_optional(&mut *conn).await?
        } else {
            None
        };
        // 第一个触发器也必须保留其位置：若有后继则使用 PRECEDES。
        let next: Option<String> = if action_order == 1 {
            sqlx::query_scalar("SELECT TRIGGER_NAME FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA=? AND EVENT_OBJECT_TABLE=? AND ACTION_TIMING=? AND EVENT_MANIPULATION=? AND ACTION_ORDER > ? ORDER BY ACTION_ORDER LIMIT 1").bind(db).bind(&table).bind(&timing).bind(&event).bind(action_order).fetch_optional(&mut *conn).await?
        } else {
            None
        };
        let order = previous
            .map(|trigger| TriggerOrder {
                follows: true,
                trigger,
            })
            .or_else(|| {
                next.map(|trigger| TriggerOrder {
                    follows: false,
                    trigger,
                })
            });
        (
            SchemaDefinition::Trigger(TriggerDefinition {
                database: db.into(),
                name: target.name.clone(),
                table,
                timing,
                event,
                body: row.try_get("ACTION_STATEMENT")?,
                definer: row.try_get("DEFINER")?,
                sql_mode: row.try_get("SQL_MODE")?,
                character_set_client: row.try_get("CHARACTER_SET_CLIENT")?,
                collation_connection: row.try_get("COLLATION_CONNECTION")?,
                order,
            }),
            ddl,
            Completeness {
                state: CompletenessState::Complete,
                reasons: vec![],
            },
        )
    } else {
        let row = sqlx::query("SELECT TABLE_TYPE, ENGINE, TABLE_COLLATION, TABLE_COMMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA=? AND TABLE_NAME=?").bind(db).bind(&target.name).fetch_optional(&mut *conn).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let kind: String = row.try_get("TABLE_TYPE")?;
        let is_view = kind.eq_ignore_ascii_case("VIEW");
        if !target.kind.is_empty()
            && !target
                .kind
                .eq_ignore_ascii_case(if is_view { "view" } else { "table" })
        {
            return Err(AppError::invalid("对象类型不匹配"));
        }
        let ddlrow = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SHOW CREATE {} {name}",
            if is_view { "VIEW" } else { "TABLE" }
        )))
        .fetch_one(&mut *conn)
        .await?;
        let ddl: String = ddlrow.try_get(1)?;
        if is_view {
            let v = sqlx::query("SELECT VIEW_DEFINITION, DEFINER, SECURITY_TYPE, CHECK_OPTION, CHARACTER_SET_CLIENT, COLLATION_CONNECTION FROM information_schema.VIEWS WHERE TABLE_SCHEMA=? AND TABLE_NAME=?").bind(db).bind(&target.name).fetch_one(&mut *conn).await?;
            let algorithm = ddl
                .split_whitespace()
                .find_map(|p| p.strip_prefix("ALGORITHM="))
                .unwrap_or("UNDEFINED")
                .to_owned();
            let check: String = v.try_get("CHECK_OPTION")?;
            let cols = sqlx::query_scalar::<_,String>("SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA=? AND TABLE_NAME=? ORDER BY ORDINAL_POSITION").bind(db).bind(&target.name).fetch_all(&mut *conn).await?;
            (
                SchemaDefinition::View(ViewDefinition {
                    select: v.try_get("VIEW_DEFINITION")?,
                    columns: cols,
                    algorithm,
                    definer: v.try_get("DEFINER")?,
                    security: v.try_get("SECURITY_TYPE")?,
                    check_option: if check == "NONE" { None } else { Some(check) },
                    character_set_client: v.try_get("CHARACTER_SET_CLIENT")?,
                    collation_connection: v.try_get("COLLATION_CONNECTION")?,
                }),
                ddl,
                Completeness {
                    state: CompletenessState::Complete,
                    reasons: vec![],
                },
            )
        } else {
            match catalog::read_table(conn, db, &target.name, &ddl, &server_version, &sql_mode)
                .await
            {
                Ok((definition, completeness)) => {
                    (SchemaDefinition::Table(definition), ddl, completeness)
                }
                Err(error) => {
                    let definition = TableDefinition {
                        columns: vec![],
                        indexes: vec![],
                        constraints: vec![],
                        engine: row
                            .try_get::<Option<String>, _>("ENGINE")?
                            .unwrap_or_default(),
                        charset: None,
                        collation: row.try_get("TABLE_COLLATION")?,
                        comment: row.try_get("TABLE_COMMENT")?,
                        options: vec![],
                    };
                    (
                        SchemaDefinition::Table(definition),
                        ddl,
                        Completeness {
                            state: CompletenessState::Incomplete,
                            reasons: vec![error.to_string()],
                        },
                    )
                }
            }
        }
    };
    let mut dependencies = Vec::new();
    if let SchemaDefinition::Table(table) = &definition {
        for constraint in &table.constraints {
            if let ConstraintDefinition::ForeignKey {
                referenced_database,
                referenced_table,
                ..
            } = constraint
            {
                let reference = qualified(referenced_database, referenced_table)?;
                let row = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "SHOW CREATE TABLE {reference}"
                )))
                .fetch_one(&mut *conn)
                .await?;
                let ddl: String = row.try_get(1)?;
                dependencies.push(format!("{}:{}", reference, fingerprint(&ddl, "")));
            }
        }
        dependencies.sort();
        dependencies.dedup();
    }
    if let SchemaDefinition::View(view) = &definition {
        match dependencies::view_dependencies(conn, db, &target.name, &view.select, &sql_mode).await
        {
            Ok(value) => dependencies = value,
            Err(error) => {
                completeness.state = CompletenessState::Incomplete;
                completeness.reasons.push(error.to_string());
            }
        }
    }
    if let SchemaDefinition::Trigger(trigger) = &definition {
        let reference = qualified(&trigger.database, &trigger.table)?;
        match sqlx::query(sqlx::AssertSqlSafe(format!(
            "SHOW CREATE TABLE {reference}"
        )))
        .fetch_one(&mut *conn)
        .await
        {
            Ok(row) => {
                let table_ddl: String = row.try_get(1)?;
                dependencies.push(format!(
                    "{}:{}",
                    reference,
                    fingerprint(&table_ddl, &sql_mode)
                ));
            }
            Err(error) => {
                completeness.state = CompletenessState::Incomplete;
                completeness
                    .reasons
                    .push(format!("无法读取触发器所属表: {error}"));
            }
        }
    }
    let context = serde_json::to_string(&(&definition, &dependencies))?;
    let fingerprint = format!(
        "{:x}",
        Sha256::digest(format!("{}\0{}", fingerprint(&ddl, &sql_mode), context).as_bytes())
    );
    Ok(Some(SchemaSnapshot {
        target,
        server_version: server_version.clone(),
        sql_mode,
        definition,
        ddl,
        dependencies,
        completeness,
        fingerprint,
        capabilities: capabilities(&server_version),
    }))
}
fn equivalent(
    expected: &SchemaDefinition,
    actual: &SchemaDefinition,
    database: &str,
    creation_defaults: Option<&(String, String)>,
) -> bool {
    match (expected, actual) {
        (SchemaDefinition::Table(a), SchemaDefinition::Table(b)) => {
            let mut a = a.clone();
            let mut b = b.clone();
            if let Some((charset, collation)) = creation_defaults {
                if a.charset.is_none() {
                    a.charset = Some(charset.clone());
                }
                if a.collation.is_none() {
                    a.collation = Some(collation.clone());
                }
            }
            for value in [
                &mut a.charset,
                &mut a.collation,
                &mut b.charset,
                &mut b.collation,
            ]
            .into_iter()
            .flatten()
            {
                value.make_ascii_lowercase();
            }
            a.engine.make_ascii_lowercase();
            b.engine.make_ascii_lowercase();
            // SHOW CREATE 会省略与表默认值相同的字段字符属性。
            for (expected, actual) in a.columns.iter_mut().zip(&mut b.columns) {
                for (left, right, inherited) in [
                    (&mut expected.charset, &mut actual.charset, &a.charset),
                    (&mut expected.collation, &mut actual.collation, &a.collation),
                ] {
                    if let Some(value) = left.as_mut() {
                        value.make_ascii_lowercase();
                    }
                    if let Some(value) = right.as_mut() {
                        value.make_ascii_lowercase();
                    }
                    if left.is_none() && right.as_ref() == inherited.as_ref() {
                        *left = inherited.clone();
                    }
                    if right.is_none() && left.as_ref() == inherited.as_ref() {
                        *right = inherited.clone();
                    }
                }
            }
            for c in &mut a.columns {
                c.id = c.name.clone();
                c.native_type = verify::normalized_type(&c.native_type);
                if c.nullable && c.generated.is_none() && c.default == ColumnDefault::Unspecified {
                    c.default = ColumnDefault::Null;
                }
            }
            for c in &mut b.columns {
                c.id = c.name.clone();
                c.native_type = verify::normalized_type(&c.native_type);
                if c.nullable && c.generated.is_none() && c.default == ColumnDefault::Unspecified {
                    c.default = ColumnDefault::Null;
                }
            }
            verify::include_implicit_foreign_indexes(&mut a, &b);
            for i in &mut a.indexes {
                i.id = i.name.clone();
            }
            for i in &mut b.indexes {
                i.id = i.name.clone();
            }
            for c in &mut a.constraints {
                match c {
                    ConstraintDefinition::ForeignKey { id, name, .. }
                    | ConstraintDefinition::Check { id, name, .. } => *id = name.clone(),
                }
            }
            for c in &mut b.constraints {
                match c {
                    ConstraintDefinition::ForeignKey { id, name, .. }
                    | ConstraintDefinition::Check { id, name, .. } => *id = name.clone(),
                }
            }
            a.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            b.indexes.sort_by(|a, b| a.name.cmp(&b.name));
            a == b
        }
        (SchemaDefinition::View(a), SchemaDefinition::View(b)) => {
            verify::views_equal(a, b, database)
        }
        (SchemaDefinition::Trigger(a), SchemaDefinition::Trigger(b)) => {
            verify::triggers_equal(a, b)
        }
        _ => expected == actual,
    }
}

/// 执行冻结的结构计划时只保留连接配置，不借用查询会话。
pub struct MysqlSchemaExecutor {
    options: sqlx::mysql::MySqlConnectOptions,
    access_mode: crate::model::AccessMode,
    connect_timeout: std::time::Duration,
}

impl MysqlSchemaExecutor {
    pub async fn execute(
        &self,
        params: Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        execute_plan(self, params, sink, cancel).await
    }
}

impl MysqlSession {
    /// 从独立连接读取计划所绑定的版本和会话模式。
    pub async fn schema_context(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<(String, String, String, String)> {
        ensure_not_cancelled(cancel)?;
        let mut conn = self.new_connection().await?;
        let row = tokio::select! { biased; _ = cancel.cancelled() => return Err(AppError::cancelled()), result = sqlx::query("SELECT VERSION() AS version, @@SESSION.sql_mode AS sql_mode, @@SESSION.character_set_client AS charset, @@SESSION.collation_connection AS collation").fetch_one(&mut conn) => result? };
        Ok((
            row.try_get("version")?,
            row.try_get("sql_mode")?,
            row.try_get("charset")?,
            row.try_get("collation")?,
        ))
    }
    /// 读取结构快照；不存在与权限不足均不伪装成空结构。
    pub async fn schema_describe(
        &mut self,
        params: Json,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        let target: ResourceTarget = param(&params, "target")?;
        ensure_not_cancelled(cancel)?;
        let mut conn = self.new_connection().await?;
        if target.kind == "database_options" {
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AppError::cancelled()),
                result = async {
                    let defaults = sqlx::query("SELECT @@GLOBAL.character_set_server AS charset, @@GLOBAL.collation_server AS collation").fetch_one(&mut conn).await?;
                    let rows = sqlx::query("SELECT CHARACTER_SET_NAME, COLLATION_NAME, IS_DEFAULT FROM information_schema.COLLATIONS ORDER BY CHARACTER_SET_NAME, COLLATION_NAME").fetch_all(&mut conn).await?;
                    let mut collations = Vec::with_capacity(rows.len());
                    for row in rows {
                        collations.push(json!({"charset":row.try_get::<String,_>("CHARACTER_SET_NAME")?, "name":row.try_get::<String,_>("COLLATION_NAME")?, "default":row.try_get::<Option<String>,_>("IS_DEFAULT")?.as_deref() == Some("Yes")}));
                    }
                    Ok::<_, sqlx::Error>(json!({"charset":defaults.try_get::<String,_>("charset")?, "collation":defaults.try_get::<String,_>("collation")?, "collations":collations}))
                } => result?,
            };
            return Ok(result);
        }
        let value = tokio::select! { biased; _ = cancel.cancelled() => return Err(AppError::cancelled()), result = describe_on(&mut conn,target) => result? };
        Ok(serde_json::to_value(value.ok_or_else(|| {
            AppError::new("not_found", "对象不存在或不可见")
        })?)?)
    }
    /// 冻结执行所需的连接配置，不创建额外查询连接。
    pub fn schema_executor(&self) -> Result<MysqlSchemaExecutor> {
        self.profile.ensure_write()?;
        if self.connection.is_none() {
            return Err(AppError::new("connection_closed", "连接已关闭"));
        }
        Ok(MysqlSchemaExecutor {
            options: self.options.clone(),
            access_mode: self.profile.access_mode,
            connect_timeout: self.connect_timeout,
        })
    }
}

async fn execute_plan(
    executor: &MysqlSchemaExecutor,
    params: Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    let plan: SchemaPlan = param(&params, "plan")?;
    let expected = build_plan(&plan.intent)?;
    if expected.plan_digest != plan.plan_digest
        || serde_json::to_vec(&expected.steps)? != serde_json::to_vec(&plan.steps)?
    {
        return Err(AppError::invalid("计划摘要或内容不匹配"));
    }
    let mut conn = super::connect_initialized(
        &executor.options,
        executor.access_mode,
        executor.connect_timeout,
    )
    .await?;
    let context = sqlx::query("SELECT VERSION() AS version, @@SESSION.sql_mode AS sql_mode, @@SESSION.character_set_client AS character_set_client, @@SESSION.collation_connection AS collation_connection")
        .fetch_one(&mut conn)
        .await?;
    let actual_version: String = context.try_get("version")?;
    let actual_mode: String = context.try_get("sql_mode")?;
    let actual_charset: String = context.try_get("character_set_client")?;
    let actual_collation: String = context.try_get("collation_connection")?;
    if actual_version != plan.intent.server_version || actual_mode != plan.intent.sql_mode {
        return Err(AppError::new(
            "schema_conflict",
            "服务端版本或 SQL 模式已变化，请重新生成计划",
        ));
    }
    let context_matches = match &plan.intent.definition {
        Some(SchemaDefinition::View(view)) => {
            view.character_set_client.as_deref() == Some(actual_charset.as_str())
                && view.collation_connection.as_deref() == Some(actual_collation.as_str())
        }
        Some(SchemaDefinition::Trigger(trigger)) => {
            trigger.sql_mode == actual_mode
                && trigger.character_set_client.as_deref() == Some(actual_charset.as_str())
                && trigger.collation_connection.as_deref() == Some(actual_collation.as_str())
        }
        _ => true,
    };
    if !context_matches {
        return Err(AppError::new(
            "schema_conflict",
            "连接字符集或排序规则已变化，请重新生成计划",
        ));
    }
    let creation_defaults = match (&plan.intent.baseline, &plan.intent.definition) {
        (None, Some(SchemaDefinition::Table(table))) => Some(
            catalog::creation_defaults(
                &mut conn,
                plan.intent.target.database.as_deref().unwrap_or(""),
                table,
            )
            .await?,
        ),
        _ => None,
    };
    let database_defaults = if let Some(SchemaDefinition::Database(database)) =
        &plan.intent.definition
    {
        let charset = match &database.charset {
            Some(charset) => charset.clone(),
            None => {
                sqlx::query_scalar::<_, String>("SELECT @@GLOBAL.character_set_server")
                    .fetch_one(&mut conn)
                    .await?
            }
        };
        let row = sqlx::query("SELECT CHARACTER_SET_NAME, DEFAULT_COLLATE_NAME FROM information_schema.CHARACTER_SETS WHERE CHARACTER_SET_NAME = ?")
            .bind(&charset).fetch_optional(&mut conn).await?
            .ok_or_else(|| AppError::invalid("数据库字符集不存在"))?;
        let charset: String = row.try_get("CHARACTER_SET_NAME")?;
        let collation: String = if let Some(collation) = &database.collation {
            sqlx::query_scalar("SELECT COLLATION_NAME FROM information_schema.COLLATIONS WHERE COLLATION_NAME = ? AND CHARACTER_SET_NAME = ?")
                .bind(collation).bind(&charset).fetch_optional(&mut conn).await?
                .ok_or_else(|| AppError::invalid("排序规则不属于所选字符集"))?
        } else if database.charset.is_none() {
            sqlx::query_scalar("SELECT @@GLOBAL.collation_server")
                .fetch_one(&mut conn)
                .await?
        } else {
            row.try_get("DEFAULT_COLLATE_NAME")?
        };
        Some((charset, collation))
    } else {
        None
    };
    let mut results = vec![json!({"state":"not_sent"}); plan.steps.len()];
    for (idx, step) in plan.steps.iter().enumerate() {
        if cancel.is_cancelled() {
            return Ok(json!({"state":"cancelled", "steps":results}));
        }
        let actual = describe_on(&mut conn, plan.intent.target.clone()).await?;
        let matches = match (&step.precondition, &actual) {
            (SchemaPrecondition::Absent, None) => true,
            (SchemaPrecondition::Fingerprint(expected), Some(s)) => expected == &s.fingerprint,
            _ => false,
        };
        if !matches {
            results[idx] = json!({"state":"failed","reason":"结构前置条件已变化"});
            return Ok(json!({"state":"conflict","steps":results}));
        }
        if idx > 0
            && let (Some(baseline), Some(SchemaDefinition::Trigger(trigger))) =
                (&plan.intent.baseline, &plan.intent.definition)
        {
            let table = qualified(&trigger.database, &trigger.table)?;
            let row = sqlx::query(sqlx::AssertSqlSafe(format!("SHOW CREATE TABLE {table}")))
                .fetch_one(&mut conn)
                .await?;
            let ddl: String = row.try_get(1)?;
            let dependency = format!("{}:{}", table, fingerprint(&ddl, &baseline.sql_mode));
            if !baseline.dependencies.contains(&dependency) {
                results[idx] = json!({"state":"failed", "reason":"触发器所属表的结构已变化"});
                return Ok(json!({"state":"conflict", "steps":results}));
            }
        }
        sink.mark_sent(true).await?;
        // 一旦发送，等待服务端终态；取消不能销毁连接并假装撤销 DDL。
        match sqlx::raw_sql(sqlx::AssertSqlSafe(step.sql.as_str()))
            .execute(&mut conn)
            .await
        {
            Err(e) => {
                let state = if matches!(&e, sqlx::Error::Database(_)) {
                    "failed"
                } else {
                    "outcome_unknown"
                };
                results[idx] = json!({"state":state,"error":e.to_string()});
                return Ok(json!({"state":state,"steps":results}));
            }
            Ok(_) => {
                results[idx] = json!({"state":"applied"});
            }
        }
    }
    sink.mark_remote_completed().await?;
    let actual = match describe_on(&mut conn, plan.intent.target.clone()).await {
        Ok(value) => value,
        Err(error) => {
            return Ok(
                json!({"state":"verification_mismatch","steps":results,"verification_error":error.to_string()}),
            );
        }
    };
    // 对象的历史创建上下文仅是基线；新定义按本次独立连接的真实上下文核对。
    let mut expected_definition = plan.intent.definition.clone();
    match expected_definition.as_mut() {
        Some(SchemaDefinition::Database(database)) => {
            if let Some((charset, collation)) = database_defaults {
                database.charset = Some(charset);
                database.collation = Some(collation);
            }
        }
        Some(SchemaDefinition::View(view)) => {
            view.character_set_client = Some(actual_charset.clone());
            view.collation_connection = Some(actual_collation.clone());
        }
        Some(SchemaDefinition::Trigger(trigger)) => {
            trigger.sql_mode = actual_mode;
            trigger.character_set_client = Some(actual_charset);
            trigger.collation_connection = Some(actual_collation);
        }
        _ => {}
    }
    let verified = match (&expected_definition, &actual) {
        (None, None) => true,
        (Some(expected), Some(actual)) => {
            actual.completeness.state == CompletenessState::Complete
                && equivalent(
                    expected,
                    &actual.definition,
                    plan.intent.target.database.as_deref().unwrap_or(""),
                    creation_defaults.as_ref(),
                )
        }
        _ => false,
    };
    Ok(
        json!({"state":if verified {"applied"} else {"verification_mismatch"},"steps":results,"snapshot":actual}),
    )
}
