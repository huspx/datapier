//! SQL 恢复计划和执行：输入预检先于任何目标写入。
mod indexes;
use indexes::inputs;
#[cfg(test)]
use indexes::inputs_with_budget;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};
use tokio_util::sync::CancellationToken;

use super::{MysqlSession, quote_ident};
use crate::{
    execution::{
        migration::TransferJournal,
        sql_restore::{self, SqlIndex, SqlKind, SqlStatement},
    },
    model::{
        AppError, ResourceTarget, Result, Value, ensure_not_cancelled,
        transfer::{
            ColumnMapping, PreparedTransfer, TransferLocator, TransferObject, TransferPlanItem,
            TransferSpec, TransferStepState,
        },
    },
};

fn object_kind(kind: &SqlKind) -> Option<&'static str> {
    match kind {
        SqlKind::Table | SqlKind::Insert => Some("table"),
        SqlKind::View => Some("view"),
        SqlKind::Trigger => Some("trigger"),
        SqlKind::Procedure => Some("procedure"),
        SqlKind::Function => Some("function"),
        SqlKind::Event => Some("event"),
        _ => None,
    }
}

/// SQL 字面量按目标列解释；与类型化 JSON 的显式源类型转换不同，复用严格 CSV 类型解码及 MySQL 校验。
fn coerce_sql_literal(value: &mut Value, column: &crate::model::Column) -> Result<()> {
    if crate::providers::mysql::validate_import_value(value, column).is_ok() {
        return Ok(());
    }
    let text = match std::mem::replace(value, Value::Null) {
        Value::Text(text)
        | Value::Integer(text)
        | Value::Unsigned(text)
        | Value::Decimal(text)
        | Value::Float(text) => text,
        _ => return Err(AppError::invalid("SQL 字面量与目标类型不匹配")),
    };
    let base = column
        .type_name
        .split(|c: char| c == '(' || c.is_ascii_whitespace())
        .next()
        .unwrap_or("");
    let binary = [
        "binary",
        "varbinary",
        "tinyblob",
        "blob",
        "mediumblob",
        "longblob",
    ]
    .iter()
    .any(|name| base.eq_ignore_ascii_case(name));
    let converted = if binary {
        Value::Bytes(STANDARD.encode(text.as_bytes()))
    } else {
        crate::execution::transfer::csv_typed(text, column, false)?
    };
    crate::providers::mysql::validate_import_value(&converted, column)?;
    *value = converted;
    Ok(())
}
fn mapping(spec: &TransferSpec, statement: &SqlStatement, input: usize) -> Result<TransferObject> {
    let kind =
        object_kind(&statement.kind).ok_or_else(|| AppError::invalid("语句没有可映射的对象"))?;
    let name = statement
        .source_name
        .as_ref()
        .ok_or_else(|| AppError::invalid("SQL 对象缺少名称"))?;
    let db = statement
        .source_database
        .as_ref()
        .or(statement.context.database.as_ref())
        .or_else(|| spec.source.as_ref().and_then(|e| e.database.as_ref()))
        .or_else(|| spec.destination.as_ref().and_then(|e| e.database.as_ref()))
        .or_else(|| {
            spec.inputs
                .get(input)
                .and_then(|i| i.target.as_ref())
                .and_then(|t| t.database.as_ref())
        })
        .ok_or_else(|| AppError::invalid(format!("SQL 对象 {name} 缺少来源数据库")))?;
    if let Some(object) = spec.objects.iter().find(|o| {
        o.source.name == *name
            && o.source.database.as_ref() == Some(db)
            && (o.source.kind.is_empty() || o.source.kind == kind)
    }) {
        let mut object = object.clone();
        if object.source.kind.is_empty() {
            object.source.kind = kind.into();
        }
        if object.target.kind.is_empty() {
            object.target.kind = kind.into();
        }
        if object.target.database.is_none() {
            object.target.database = spec.destination.as_ref().and_then(|e| e.database.clone());
        }
        return Ok(object);
    }
    let input_target = spec
        .inputs
        .get(input)
        .and_then(|input| input.target.clone())
        .filter(|_| kind == "table");
    let dest = spec
        .destination
        .as_ref()
        .and_then(|e| e.database.clone())
        .or_else(|| {
            input_target
                .as_ref()
                .and_then(|target| target.database.clone())
        })
        .ok_or_else(|| AppError::invalid(format!("请为 {db}.{name} 指定目标库或映射")))?;
    let source = ResourceTarget {
        database: Some(db.clone()),
        name: name.clone(),
        key_b64: None,
        kind: kind.into(),
    };
    let mut target = input_target.unwrap_or(ResourceTarget {
        database: Some(dest.clone()),
        name: name.clone(),
        key_b64: None,
        kind: kind.into(),
    });
    if target.database.is_none() {
        target.database = Some(dest);
    }
    if target.kind.is_empty() {
        target.kind = kind.into();
    }
    Ok(TransferObject {
        source,
        target,
        columns: Vec::new(),
        target_columns: Vec::new(),
        filters: Vec::new(),
        locator_key: spec.options.locator_key.clone(),
        create_if_missing: spec.options.create_missing,
        sync_schema: true,
        selected_schema_items: Vec::new(),
    })
}
fn statement_id(input: usize, statement: usize) -> String {
    format!("sql:{input}:{statement}")
}
fn item(
    input: usize,
    number: usize,
    action: &str,
    object: &TransferObject,
    sql: Option<String>,
) -> TransferPlanItem {
    TransferPlanItem {
        id: statement_id(input, number),
        phase: "restore".into(),
        action: action.into(),
        source: Some(object.source.clone()),
        target: Some(object.target.clone()),
        sql,
        destructive: false,
        selected: true,
        precondition: None,
        notes: Vec::new(),
    }
}
fn sql_mode(statement: &SqlStatement) -> String {
    let mut modes = Vec::new();
    if statement.context.no_backslash_escapes {
        modes.push("NO_BACKSLASH_ESCAPES");
    }
    if statement.context.ansi_quotes {
        modes.push("ANSI_QUOTES");
    }
    modes.join(",")
}
async fn target_table(
    session: &mut MysqlSession,
    object: &TransferObject,
    cancel: &CancellationToken,
) -> Result<Option<super::MigrationTable>> {
    let db = object
        .target
        .database
        .as_deref()
        .ok_or_else(|| AppError::invalid("目标库缺失"))?;
    let mut conn = session.new_connection().await?;
    let kind: Option<String> = sqlx::query_scalar(
        "SELECT TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_SCHEMA=? AND TABLE_NAME=?",
    )
    .bind(db)
    .bind(&object.target.name)
    .fetch_optional(&mut conn)
    .await?;
    match kind.as_deref() {
        None => Ok(None),
        Some("BASE TABLE") => Ok(Some(
            session
                .describe_migration(object.target.clone(), cancel)
                .await?,
        )),
        Some(_) => Err(AppError::conflict(format!(
            "目标 {} 存在非基表对象",
            object.target.name
        ))),
    }
}
async fn existing_object(session: &MysqlSession, object: &TransferObject) -> Result<bool> {
    let db = object
        .target
        .database
        .as_deref()
        .ok_or_else(|| AppError::invalid("目标库缺失"))?;
    let (sql, extra) = match object.target.kind.as_str() {
        "view" => (
            "SELECT COUNT(*) FROM information_schema.VIEWS WHERE TABLE_SCHEMA=? AND TABLE_NAME=?",
            None,
        ),
        "trigger" => (
            "SELECT COUNT(*) FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA=? AND TRIGGER_NAME=?",
            None,
        ),
        "procedure" => (
            "SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA=? AND ROUTINE_NAME=? AND ROUTINE_TYPE=?",
            Some("PROCEDURE"),
        ),
        "function" => (
            "SELECT COUNT(*) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA=? AND ROUTINE_NAME=? AND ROUTINE_TYPE=?",
            Some("FUNCTION"),
        ),
        "event" => (
            "SELECT COUNT(*) FROM information_schema.EVENTS WHERE EVENT_SCHEMA=? AND EVENT_NAME=?",
            None,
        ),
        _ => return Err(AppError::invalid("不支持的恢复对象")),
    };
    let mut conn = session.new_connection().await?;
    let query = sqlx::query_scalar::<_, i64>(sql)
        .bind(db)
        .bind(&object.target.name);
    let count = if let Some(extra) = extra {
        query.bind(extra).fetch_one(&mut conn).await?
    } else {
        query.fetch_one(&mut conn).await?
    };
    Ok(count != 0)
}

struct RestoreInput {
    path: PathBuf,
    index: std::sync::Arc<SqlIndex>,
}

struct RestoreRowsContext<'a> {
    spec: &'a TransferSpec,
    input: usize,
    number: usize,
    source: &'a RestoreInput,
    object: &'a TransferObject,
    entry: &'a TransferPlanItem,
}

struct RestoreBatchContext<'a> {
    spec: &'a TransferSpec,
    input: usize,
    number: usize,
    source: &'a RestoreInput,
    target: &'a super::MigrationTable,
    object: &'a TransferObject,
}

async fn database_defaults(session: &MysqlSession, db: &str) -> Result<(String, String)> {
    let mut conn = session.new_connection().await?;
    let row = sqlx::query("SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME=?").bind(db).fetch_optional(&mut conn).await?.ok_or_else(|| AppError::new("schema_incomplete", format!("目标数据库 {db} 不存在或不可见")))?;
    Ok((row.try_get(0)?, row.try_get(1)?))
}

async fn database_context(
    session: &MysqlSession,
    source: &RestoreInput,
    statement: &SqlStatement,
    object: &TransferObject,
) -> Result<Option<((String, String), (String, String))>> {
    let Some(context) = &statement.database_context else {
        return Ok(None);
    };
    let ddl = sql_restore::read_ddl(&source.path, &source.index, context.before_statement)?;
    let tokens = sqlparser::tokenizer::Tokenizer::new(&sqlparser::dialect::MySqlDialect {}, &ddl)
        .tokenize()
        .map_err(|e| AppError::invalid(format!("数据库上下文解析失败: {e}")))?;
    let words: Vec<String> = tokens
        .into_iter()
        .map(|t| t.to_string())
        .filter(|s| !s.trim().is_empty() && s != "=")
        .collect();
    let mut charset = None;
    let mut collation = None;
    for (i, word) in words.iter().enumerate() {
        if word.eq_ignore_ascii_case("COLLATE") {
            collation = words.get(i + 1).cloned();
        }
        if word.eq_ignore_ascii_case("CHARSET")
            || (word.eq_ignore_ascii_case("SET")
                && i > 0
                && words[i - 1].eq_ignore_ascii_case("CHARACTER"))
        {
            charset = words.get(i + 1).cloned();
        }
    }
    let db = object
        .target
        .database
        .as_deref()
        .ok_or_else(|| AppError::invalid("目标库缺失"))?;
    let before = database_defaults(session, db).await?;
    let mut conn = session.new_connection().await?;
    let collation = collation
        .ok_or_else(|| AppError::new("schema_incomplete", "数据库创建上下文缺少明确排序规则"))?;
    super::token(&collation)?;
    let resolved: String = sqlx::query_scalar(
        "SELECT CHARACTER_SET_NAME FROM information_schema.COLLATIONS WHERE COLLATION_NAME=?",
    )
    .bind(&collation)
    .fetch_one(&mut conn)
    .await?;
    if charset
        .as_ref()
        .is_some_and(|c| !c.eq_ignore_ascii_case(&resolved))
    {
        return Err(AppError::invalid("数据库上下文字符集与排序规则不匹配"));
    }
    super::token(&resolved)?;
    let after = (resolved, collation);
    Ok((before != after).then_some((before, after)))
}

impl MysqlSession {
    /// 扫描所有输入并比较目标结构；对象依赖或定义不可确定时拒绝准备。
    pub async fn prepare_restore(
        &mut self,
        spec: &TransferSpec,
        sink: &crate::ExecutionSink,
        retry_of: Option<uuid::Uuid>,
        cancel: &CancellationToken,
    ) -> Result<Vec<TransferPlanItem>> {
        let sources = inputs(spec, sink, cancel).await?;
        self.restore_plan(spec, &sources, &sink.store, retry_of, cancel)
            .await
    }

    async fn restore_plan(
        &mut self,
        spec: &TransferSpec,
        sources: &[RestoreInput],
        store: &crate::Store,
        retry_of: Option<uuid::Uuid>,
        cancel: &CancellationToken,
    ) -> Result<Vec<TransferPlanItem>> {
        let mut conn = self.new_connection().await?;
        let version: String = sqlx::query_scalar("SELECT VERSION()")
            .fetch_one(&mut conn)
            .await?;
        let mut parts = version.split('.').take(3).map(|part| {
            part.chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse::<u32>()
        });
        let major = parts.next().and_then(|v| v.ok()).ok_or_else(|| {
            AppError::new("unsupported_version", format!("无法识别目标版本 {version}"))
        })?;
        let minor = parts.next().and_then(|v| v.ok()).ok_or_else(|| {
            AppError::new("unsupported_version", format!("无法识别目标版本 {version}"))
        })?;
        let patch = parts.next().and_then(|v| v.ok()).ok_or_else(|| {
            AppError::new("unsupported_version", format!("无法识别目标版本 {version}"))
        })?;
        let target_version = major
            .saturating_mul(10000)
            .saturating_add(minor.saturating_mul(100))
            .saturating_add(patch);
        let mut checked_contexts = BTreeSet::new();
        for source in sources {
            for statement in &source.index.statements {
                for constraint in &statement.version_constraints {
                    if constraint
                        .minimum
                        .is_some_and(|minimum| minimum > target_version)
                    {
                        return Err(AppError::new(
                            "unsupported_version",
                            format!(
                                "SQL 第 {} 行、字节 {} 的版本注释在目标 MySQL {version} 上被忽略，不能声明完整恢复",
                                statement.line, constraint.start
                            ),
                        ));
                    }
                }
                if spec.options.include_schema
                    && matches!(
                        statement.kind,
                        SqlKind::View
                            | SqlKind::Trigger
                            | SqlKind::Procedure
                            | SqlKind::Function
                            | SqlKind::Event
                    )
                    && checked_contexts.insert(&statement.context.settings)
                {
                    // 在独立空会话验证源创建上下文，不能到数据已导入后才发现目标不接受 SQL_MODE 等设置。
                    let mut probe = self.new_connection().await?;
                    for setting in &statement.context.settings {
                        ensure_not_cancelled(cancel)?;
                        if let Err(error) = sqlx::raw_sql(sqlx::AssertSqlSafe(setting.as_str()))
                            .execute(&mut probe)
                            .await
                        {
                            let database_code = error
                                .as_database_error()
                                .and_then(|error| error.code())
                                .map(|code| code.into_owned());
                            let mut error = AppError::new(
                                "unsupported_context",
                                format!(
                                    "SQL 第 {} 行的对象创建上下文不被目标 MySQL {version} 接受；请修订源 SET 后重新准备",
                                    statement.line
                                ),
                            );
                            error.database_code = database_code;
                            return Err(error);
                        }
                    }
                }
            }
        }
        let mut schema = Vec::new();
        let mut data = Vec::new();
        let mut deferred = Vec::new();
        let mut objects = Vec::new();
        let mut definitions = BTreeSet::new();
        let mut mappings = spec.objects.clone();
        for (input, source) in sources.iter().enumerate() {
            for statement in &source.index.statements {
                if statement.placeholder_view {
                    continue;
                }
                if let Some(kind) = object_kind(&statement.kind)
                    && statement.kind != SqlKind::Insert
                    && statement.is_create
                {
                    let object = mapping(spec, statement, input)?;
                    definitions.insert((
                        object.source.database.clone().unwrap_or_default(),
                        object.source.name.clone(),
                        kind.to_owned(),
                    ));
                    if !mappings.iter().any(|m| {
                        m.source.database == object.source.database
                            && m.source.name == object.source.name
                            && m.source.kind == object.source.kind
                    }) {
                        mappings.push(object);
                    }
                }
            }
        }
        let mut source_tables: BTreeMap<(String, String), super::MigrationTable> = BTreeMap::new();
        if spec.options.include_schema {
            for (input, source) in sources.iter().enumerate() {
                for (number, statement) in source.index.statements.iter().enumerate() {
                    let drop_index = statement.kind == SqlKind::Drop
                        && statement.drop_kind == Some(SqlKind::Index);
                    if statement.placeholder_view
                        || (!matches!(statement.kind, SqlKind::Table | SqlKind::Index)
                            && !drop_index)
                    {
                        continue;
                    }
                    let mut owner = statement.clone();
                    if statement.kind == SqlKind::Index || drop_index {
                        owner.kind = SqlKind::Table;
                        owner.source_database = statement.index_table_database.clone();
                        owner.source_name = statement.index_table_name.clone();
                    }
                    let object = mapping(spec, &owner, input)?;
                    let key = (
                        object.source.database.clone().unwrap_or_default(),
                        object.source.name.clone(),
                    );
                    let ddl = if drop_index {
                        let index = statement
                            .source_name
                            .as_deref()
                            .ok_or_else(|| AppError::invalid("DROP INDEX 缺少索引名"))?;
                        format!(
                            "ALTER TABLE {}.{} DROP INDEX {}",
                            quote_ident(
                                object
                                    .source
                                    .database
                                    .as_deref()
                                    .ok_or_else(|| AppError::invalid("索引源库缺失"))?
                            )?,
                            quote_ident(&object.source.name)?,
                            quote_ident(index)?
                        )
                    } else {
                        sql_restore::read_ddl(&source.path, &source.index, number)?
                    };
                    if statement.kind == SqlKind::Table && statement.is_create {
                        let table =
                            self.from_restore_ddl(object.source, &ddl, &sql_mode(statement))?;
                        if source_tables.insert(key, table).is_some() {
                            return Err(AppError::new(
                                "schema_incomplete",
                                format!("SQL 第 {} 行重复创建表", statement.line),
                            ));
                        }
                    } else {
                        let table = source_tables.get(&key).ok_or_else(|| {
                            AppError::new(
                                "source_schema_required",
                                format!(
                                    "SQL 第 {} 行的结构变更缺少先前 CREATE TABLE 基线",
                                    statement.line
                                ),
                            )
                        })?;
                        let updated = self.apply_restore_structure(table, &ddl)?;
                        source_tables.insert(key, updated);
                    }
                }
            }
        }
        if spec.options.include_schema {
            super::super::schema::validate_restore_dependencies(&source_tables, &mappings)?;
        }
        for (input, source) in sources.iter().enumerate() {
            for (number, statement) in source.index.statements.iter().enumerate() {
                ensure_not_cancelled(cancel)?;
                if statement.placeholder_view {
                    continue;
                }
                if statement.unresolved_references {
                    return Err(AppError::new(
                        "unresolved_reference",
                        format!(
                            "SQL 第 {} 行含动态 SQL 或无法确定的引用，编辑后重新准备",
                            statement.line
                        ),
                    ));
                }
                if statement.alters_database_defaults {
                    continue;
                }
                if statement.kind == SqlKind::Database && statement.is_create {
                    if let Some(state) =
                        prior_state(store, retry_of, input, statement.start, "database:create")
                            .await?
                        && matches!(
                            state,
                            TransferStepState::Sent | TransferStepState::OutcomeUnknown
                        )
                    {
                        return Err(AppError::unknown("数据库创建结果未知，请先人工核对"));
                    }
                    let source_db = statement
                        .source_name
                        .as_deref()
                        .or(statement.object.as_deref())
                        .ok_or_else(|| AppError::invalid("CREATE DATABASE 缺少数据库名称"))?;
                    let target_db = spec
                        .destination
                        .as_ref()
                        .and_then(|endpoint| endpoint.database.as_deref())
                        .unwrap_or(source_db);
                    if self
                        .profile
                        .direct()?
                        .visible_databases
                        .as_ref()
                        .is_some_and(|allowed| !allowed.iter().any(|db| db == target_db))
                    {
                        return Err(AppError::new(
                            "permission_denied",
                            format!("目标数据库 {target_db} 不在连接可见范围内"),
                        ));
                    }
                    let mut conn = self.new_connection().await?;
                    let exists: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME=?",
                    )
                    .bind(target_db)
                    .fetch_one(&mut conn)
                    .await?;
                    if exists == 0 {
                        let raw = sql_restore::read_ddl(&source.path, &source.index, number)?;
                        let words: Vec<String> = sqlparser::tokenizer::Tokenizer::new(
                            &sqlparser::dialect::MySqlDialect {},
                            &raw,
                        )
                        .tokenize()
                        .map_err(|e| AppError::invalid(format!("CREATE DATABASE 解析失败: {e}")))?
                        .into_iter()
                        .map(|token| token.to_string())
                        .filter(|s| !s.trim().is_empty() && s != "=")
                        .collect();
                        let mut i = 2;
                        if words.get(i).is_some_and(|s| s.eq_ignore_ascii_case("IF")) {
                            i += 3;
                        }
                        if words.get(i).is_none() {
                            return Err(AppError::invalid("CREATE DATABASE 缺少名称"));
                        }
                        i += 1;
                        let mut suffix = String::new();
                        while i < words.len() {
                            if words[i].eq_ignore_ascii_case("DEFAULT") {
                                i += 1;
                            }
                            let word = words
                                .get(i)
                                .ok_or_else(|| AppError::invalid("CREATE DATABASE 子句不完整"))?;
                            let collation = word.eq_ignore_ascii_case("COLLATE");
                            if word.eq_ignore_ascii_case("CHARACTER")
                                && words
                                    .get(i + 1)
                                    .is_some_and(|s| s.eq_ignore_ascii_case("SET"))
                            {
                                i += 2;
                            } else if collation || word.eq_ignore_ascii_case("CHARSET") {
                                i += 1;
                            } else {
                                return Err(AppError::new(
                                    "schema_incomplete",
                                    format!("不支持的 CREATE DATABASE 子句 {word}"),
                                ));
                            }
                            let value = words.get(i).ok_or_else(|| {
                                AppError::invalid("CREATE DATABASE 缺少字符集或排序规则")
                            })?;
                            super::token(value)?;
                            suffix.push_str(if collation {
                                " COLLATE "
                            } else {
                                " CHARACTER SET "
                            });
                            suffix.push_str(value);
                            i += 1;
                        }
                        let target = ResourceTarget {
                            database: Some(target_db.into()),
                            name: target_db.into(),
                            key_b64: None,
                            kind: "database".into(),
                        };
                        schema.push(TransferPlanItem {
                            id: statement_id(input, number),
                            phase: "database".into(),
                            action: "create_database".into(),
                            source: None,
                            target: Some(target),
                            sql: Some(format!(
                                "CREATE DATABASE {}{suffix}",
                                quote_ident(target_db)?
                            )),
                            destructive: true,
                            selected: spec.options.create_missing,
                            precondition: Some(source.index.sha256.clone()),
                            notes: vec!["创建数据库需要 CREATE 权限，属于独立隐式提交步骤".into()],
                        });
                    }
                    continue;
                }
                if statement.kind == SqlKind::Index
                    || (statement.kind == SqlKind::Drop
                        && statement.drop_kind == Some(SqlKind::Index))
                    || (statement.kind == SqlKind::Table && !statement.is_create)
                {
                    continue;
                }
                if statement.kind == SqlKind::Drop {
                    let name = statement
                        .source_name
                        .as_deref()
                        .ok_or_else(|| AppError::invalid("DROP 缺少对象名"))?;
                    let db = statement
                        .source_database
                        .as_deref()
                        .or(statement.context.database.as_deref())
                        .or_else(|| spec.source.as_ref().and_then(|e| e.database.as_deref()))
                        .or_else(|| {
                            spec.destination
                                .as_ref()
                                .and_then(|e| e.database.as_deref())
                        })
                        .ok_or_else(|| AppError::invalid("DROP 缺少数据库"))?;
                    let drop_kind = statement
                        .drop_kind
                        .as_ref()
                        .and_then(object_kind)
                        .ok_or_else(|| AppError::invalid("DROP 对象类型不支持"))?;
                    if !definitions.iter().any(|(d, n, kind)| {
                        d == db
                            && n == name
                            && (kind == drop_kind
                                || (drop_kind == "table"
                                    && kind == "view"
                                    && source.index.statements.iter().any(|s| {
                                        s.placeholder_view
                                            && s.source_database
                                                .as_deref()
                                                .or(s.context.database.as_deref())
                                                == Some(db)
                                            && s.source_name.as_deref() == Some(name)
                                    })))
                    }) {
                        return Err(AppError::new(
                            "unsupported_statement",
                            format!(
                                "SQL 第 {} 行的独立 DROP 没有对应创建定义，不能按恢复执行",
                                statement.line
                            ),
                        ));
                    }
                    objects.push(TransferPlanItem {
                        id: statement_id(input, number),
                        phase: "restore".into(),
                        action: "wrapper_conversion".into(),
                        source: None,
                        target: None,
                        sql: None,
                        destructive: false,
                        selected: true,
                        precondition: Some(source.index.sha256.clone()),
                        notes: vec![
                            "输入 DROP 已转换为目标差异和单独确认；已有表不删除重建".into(),
                        ],
                    });
                    continue;
                }
                let Some(kind) = object_kind(&statement.kind) else {
                    if statement.kind == SqlKind::Wrapper {
                        let mut wrapper = TransferPlanItem {
                            id: statement_id(input, number),
                            phase: "restore".into(),
                            action: "wrapper_conversion".into(),
                            source: None,
                            target: None,
                            sql: None,
                            destructive: false,
                            selected: true,
                            precondition: Some(source.index.sha256.clone()),
                            notes: vec![
                                "输入锁表或检查开关不直接执行；恢复保持约束检查并使用独立事务"
                                    .into(),
                            ],
                        };
                        wrapper.selected = true;
                        objects.push(wrapper);
                    }
                    continue;
                };
                let object = mapping(spec, statement, input)?;
                let target_db = object
                    .target
                    .database
                    .as_deref()
                    .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                if self
                    .profile
                    .direct()?
                    .visible_databases
                    .as_ref()
                    .is_some_and(|allowed| !allowed.iter().any(|db| db == target_db))
                {
                    return Err(AppError::new(
                        "permission_denied",
                        format!("目标数据库 {target_db} 不在连接可见范围内"),
                    ));
                }
                // 创建上下文随对象一起调度，不能作为普通会话 SET 丢弃。
                for reference in &statement.references {
                    let (db, name) = reference.rsplit_once('.').unwrap_or((
                        object.source.database.as_deref().unwrap_or_default(),
                        reference.as_str(),
                    ));
                    let matches: BTreeSet<_> = definitions
                        .iter()
                        .filter(|(d, n, _)| {
                            d.eq_ignore_ascii_case(db) && n.eq_ignore_ascii_case(name)
                        })
                        .map(|(d, n, _)| (d.as_str(), n.as_str()))
                        .collect();
                    if matches.len() != 1 || !matches.contains(&(db, name)) {
                        return Err(AppError::new(
                            "unresolved_reference",
                            format!(
                                "SQL 第 {} 行外部或缺失引用 {reference}，请修订定义或纳入映射范围",
                                statement.line
                            ),
                        ));
                    }
                }
                if statement.kind == SqlKind::Insert {
                    if !spec.options.include_data {
                        continue;
                    }
                    if statement.source_columns.is_none() && spec.inputs[input].columns.is_empty() {
                        return Err(AppError::new(
                            "source_columns_required",
                            format!(
                                "SQL 第 {} 行无法安全推导源列顺序，请明确提供列序",
                                statement.line
                            ),
                        ));
                    }
                    let mut entry = item(input, number, "insert", &object, None);
                    entry
                        .notes
                        .push("每批按目标冲突策略写入，保留外键检查；目标现有触发器会执行".into());
                    let mut conn = self.new_connection().await?;
                    let db = object
                        .target
                        .database
                        .as_deref()
                        .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                    let triggers: Vec<String> = sqlx::query_scalar("SELECT TRIGGER_NAME FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA=? AND EVENT_OBJECT_TABLE=? ORDER BY TRIGGER_NAME").bind(db).bind(&object.target.name).fetch_all(&mut conn).await?;
                    entry.precondition = Some(format!(
                        "{:x}",
                        Sha256::digest(serde_json::to_vec(&(&source.index.sha256, &triggers))?)
                    ));
                    if !triggers.is_empty() {
                        entry.destructive = true;
                        entry.notes.push(format!(
                            "目标表已有触发器 {}，写入将执行其业务逻辑，必须单独确认",
                            triggers.join(", ")
                        ));
                    }
                    data.push(entry);
                    continue;
                }
                if !spec.options.include_schema {
                    continue;
                }
                let ddl = sql_restore::read_ddl(&source.path, &source.index, number)?;
                if kind == "table" {
                    if let Some(state) = prior_state(
                        store,
                        retry_of,
                        input,
                        statement.start,
                        "schema:create_table",
                    )
                    .await?
                        && matches!(
                            state,
                            TransferStepState::Sent | TransferStepState::OutcomeUnknown
                        )
                    {
                        return Err(AppError::unknown(format!(
                            "表 {} 的创建结果未知，请先人工核对后重新准备",
                            object.target.name
                        )));
                    }
                    let key = (
                        object.source.database.clone().unwrap_or_default(),
                        object.source.name.clone(),
                    );
                    let source_table = source_tables
                        .get(&key)
                        .ok_or_else(|| AppError::new("source_schema_required", "缺少表结构基线"))?;
                    let target = target_table(self, &object, cancel).await?;
                    if target.is_none()
                        && !(object.create_if_missing || spec.options.create_missing)
                    {
                        return Err(AppError::new(
                            "target_missing",
                            format!("目标表 {} 不存在，需确认创建", object.target.name),
                        ));
                    }
                    let mut changes = self
                        .prepare_schema(source_table, target.as_ref(), &object, &mappings)
                        .await?;
                    if let Some(incomplete) = changes
                        .iter()
                        .find(|change| change.action == "unknown_attribute")
                    {
                        return Err(AppError::new(
                            "schema_incomplete",
                            incomplete.notes.join("; "),
                        ));
                    }
                    for change in &mut changes {
                        let old_id = change.id.clone();
                        change.id = format!("{}:{old_id}", statement_id(input, number));
                        change.selected = true;
                        change
                            .notes
                            .push(format!("SQL 输入第 {} 行", statement.line));
                    }
                    for change in changes {
                        if change.phase == "after_data" {
                            deferred.push(change);
                        } else {
                            schema.push(change);
                        }
                    }
                } else {
                    if retry_of.is_some() {
                        let prior = prior_state(
                            store,
                            retry_of,
                            input,
                            statement.start,
                            "object:create_object",
                        )
                        .await?;
                        if matches!(
                            prior,
                            Some(
                                TransferStepState::Applied
                                    | TransferStepState::Skipped
                                    | TransferStepState::Sent
                                    | TransferStepState::OutcomeUnknown
                            )
                        ) {
                            let unknown = matches!(
                                prior,
                                Some(TransferStepState::Sent | TransferStepState::OutcomeUnknown)
                            );
                            let mut done = item(
                                input,
                                number,
                                if unknown {
                                    "prior_unknown"
                                } else {
                                    "prior_object"
                                },
                                &object,
                                None,
                            );
                            done.precondition = Some(source.index.sha256.clone());
                            done.notes.push(
                                if matches!(
                                    prior,
                                    Some(
                                        TransferStepState::Sent | TransferStepState::OutcomeUnknown
                                    )
                                ) {
                                    "此前对象创建结果未知，不重复执行；请人工核对目标".into()
                                } else {
                                    "此前对象创建已完成，不重复执行".into()
                                },
                            );
                            objects.push(done);
                            continue;
                        }
                    }
                    let mut resolved_statement = statement.clone();
                    if resolved_statement.source_database.is_none() {
                        resolved_statement.source_database = object.source.database.clone();
                    }
                    let rewritten = sql_restore::rewrite_ddl(
                        &ddl,
                        &resolved_statement,
                        &mappings,
                        &spec.options.definer,
                    )?;
                    let exists = existing_object(self, &object).await?;
                    if exists {
                        let mut drop_item = item(
                            input,
                            number,
                            "drop_object",
                            &object,
                            Some(format!(
                                "DROP {} {}.{}",
                                kind.to_ascii_uppercase(),
                                quote_ident(
                                    object
                                        .target
                                        .database
                                        .as_deref()
                                        .ok_or_else(|| AppError::invalid("目标库缺失"))?
                                )?,
                                quote_ident(&object.target.name)?
                            )),
                        );
                        let (show, index) = super::show_command(&object.target)?;
                        let mut conn = self.new_connection().await?;
                        let row = sqlx::query(sqlx::AssertSqlSafe(show))
                            .fetch_one(&mut conn)
                            .await?;
                        let existing = super::string_at(&row, index, "目标对象定义")?;
                        drop_item.id.push_str(":drop");
                        drop_item.destructive = true;
                        drop_item.precondition =
                            Some(format!("{:x}", Sha256::digest(existing.as_bytes())));
                        drop_item
                            .notes
                            .push("DROP 和 CREATE 分别隐式提交，失败不会自动恢复原定义".into());
                        objects.push(drop_item);
                    }
                    let context = database_context(self, source, statement, &object).await?;
                    if let Some((before, desired)) = &context {
                        let db = object
                            .target
                            .database
                            .as_deref()
                            .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                        let mut enter = item(
                            input,
                            number,
                            "database_context_enter",
                            &object,
                            Some(format!(
                                "ALTER DATABASE {} CHARACTER SET {} COLLATE {}",
                                quote_ident(db)?,
                                desired.0,
                                desired.1
                            )),
                        );
                        enter.id.push_str(":context-enter");
                        enter.destructive = true;
                        enter.precondition = Some(serde_json::to_string(&(before, desired))?);
                        enter.notes.push(format!(
                            "库级默认值 {} / {} 临时改为 {} / {}；需 ALTER 权限，建议协调变更窗口",
                            before.0, before.1, desired.0, desired.1
                        ));
                        objects.push(enter);
                    }
                    let mut create = item(input, number, "create_object", &object, Some(rewritten));
                    create.precondition = Some(source.index.sha256.clone());
                    create.notes.push(format!(
                        "原 DEFINER {:?}；SQL SECURITY {:?}；事件默认禁用",
                        statement.definer, statement.sql_security
                    ));
                    objects.push(create);
                    if let Some((before, desired)) = &context {
                        let db = object
                            .target
                            .database
                            .as_deref()
                            .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                        let mut leave = item(
                            input,
                            number,
                            "database_context_leave",
                            &object,
                            Some(format!(
                                "ALTER DATABASE {} CHARACTER SET {} COLLATE {}",
                                quote_ident(db)?,
                                before.0,
                                before.1
                            )),
                        );
                        leave.id.push_str(":context-leave");
                        leave.destructive = true;
                        leave.precondition = Some(serde_json::to_string(&(before, desired))?);
                        leave.notes.push(
                            "仅在默认值仍为本次设置值时恢复；外部变更或结果未知需人工核对".into(),
                        );
                        objects.push(leave);
                    }
                }
            }
        }
        // 对象依赖按源引用拓扑排序；触发器与事件保留数据之后的安装顺序。
        let mut ordered = Vec::new();
        let mut completed = BTreeSet::new();
        for s in &schema {
            if let Some(t) = &s.source {
                completed.insert((t.database.clone().unwrap_or_default(), t.name.clone()));
            }
        }
        for s in &data {
            if let Some(t) = &s.source {
                completed.insert((t.database.clone().unwrap_or_default(), t.name.clone()));
            }
        }
        while !objects.is_empty() {
            let pos = objects.iter().position(|entry| {
                if entry.action == "wrapper_conversion" {
                    return true;
                }
                let Ok((i, n)) = index_for_item(&entry.id) else {
                    return false;
                };
                let stmt = &sources[i].index.statements[n];
                stmt.references.iter().all(|r| {
                    let (db, name) = r
                        .rsplit_once('.')
                        .map(|(d, n)| (d.to_owned(), n))
                        .unwrap_or((
                            entry
                                .source
                                .as_ref()
                                .and_then(|t| t.database.clone())
                                .unwrap_or_default(),
                            r.as_str(),
                        ));
                    !definitions.iter().any(|(d, n, _)| d == &db && n == name)
                        || completed.iter().any(|(d, n)| d == &db && n == name)
                })
            });
            let Some(pos) = pos else {
                return Err(AppError::new(
                    "dependency_cycle",
                    "对象引用存在循环，无法安全排列恢复顺序",
                ));
            };
            let key = index_for_item(&objects[pos].id)?;
            let mut i = 0;
            while i < objects.len() {
                if index_for_item(&objects[i].id)? != key {
                    i += 1;
                    continue;
                }
                let entry = objects.remove(i);
                if (entry.action == "create_object" || entry.action == "prior_object")
                    && let Some(t) = &entry.source
                {
                    completed.insert((t.database.clone().unwrap_or_default(), t.name.clone()));
                }
                ordered.push(entry);
            }
        }
        schema.extend(data);
        schema.extend(deferred);
        schema.extend(ordered);
        let mut scoped = BTreeSet::new();
        for (input, source) in sources.iter().enumerate() {
            for (number, statement) in source.index.statements.iter().enumerate() {
                if statement.placeholder_view
                    || object_kind(&statement.kind).is_none()
                    || (statement.kind != SqlKind::Insert && !statement.is_create)
                    || (statement.kind == SqlKind::Insert && !spec.options.include_data)
                    || (statement.kind != SqlKind::Insert && !spec.options.include_schema)
                {
                    continue;
                }
                let object = mapping(spec, statement, input)?;
                let key = (
                    object.source.database.clone(),
                    object.source.name.clone(),
                    object.source.kind.clone(),
                );
                if scoped.insert(key) {
                    let mut scope = item(input, number, "object_scope", &object, None);
                    scope.id.push_str(":object-scope");
                    scope.precondition = Some(source.index.sha256.clone());
                    scope
                        .notes
                        .push("选择或排除该对象及其所有结构和数据步骤".into());
                    schema.push(scope);
                }
            }
        }
        if !schema
            .iter()
            .any(|entry| entry.source.is_some() && entry.selected)
        {
            for entry in &mut schema {
                if entry.phase == "database" {
                    entry.selected = false;
                }
            }
        }
        Ok(schema)
    }
}
fn validate_restore_selection(
    items: &[TransferPlanItem],
    sources: &[RestoreInput],
    spec: &TransferSpec,
) -> Result<()> {
    let mut selected = BTreeMap::<(String, String, String), bool>::new();
    for entry in items.iter().filter(|entry| entry.action == "object_scope") {
        if let Some(source) = &entry.source {
            let key = (
                source.database.clone().unwrap_or_default(),
                source.name.clone(),
                source.kind.clone(),
            );
            if selected.insert(key, entry.selected).is_some() {
                return Err(AppError::invalid("恢复对象选择重复"));
            }
        }
    }
    for entry in items.iter().filter(|entry| entry.action != "object_scope") {
        if let Some(source) = &entry.source {
            let key = (
                source.database.clone().unwrap_or_default(),
                source.name.clone(),
                source.kind.clone(),
            );
            if let Some(enabled) = selected.get(&key)
                && ((!*enabled && entry.selected)
                    || (*enabled
                        && (entry.phase == "restore" || entry.action == "add_foreign_key")
                        && !entry.selected))
            {
                return Err(AppError::invalid("对象及其恢复步骤的选择不一致"));
            }
        }
    }
    if !selected.values().any(|value| *value)
        && items
            .iter()
            .any(|entry| entry.phase == "database" && entry.selected)
    {
        return Err(AppError::invalid("未选择恢复对象时不能创建数据库"));
    }
    for entry in items.iter().filter(|entry| {
        entry.action == "database_context_enter" || entry.action == "database_context_leave"
    }) {
        let (input, number) = index_for_item(&entry.id)?;
        let create_id = statement_id(input, number);
        if !items.iter().any(|item| {
            item.id == create_id
                && item.action == "create_object"
                && item.selected == entry.selected
        }) {
            return Err(AppError::new(
                "confirmation_required",
                "数据库创建上下文及其恢复动作必须一起确认",
            ));
        }
    }
    for (input, source) in sources.iter().enumerate() {
        for statement in &source.index.statements {
            let mut owner = statement.clone();
            if statement.kind == SqlKind::Index
                || (statement.kind == SqlKind::Drop && statement.drop_kind == Some(SqlKind::Index))
            {
                owner.kind = SqlKind::Table;
                owner.source_database = statement.index_table_database.clone();
                owner.source_name = statement.index_table_name.clone();
            }
            if object_kind(&owner.kind).is_none()
                || owner.source_name.is_none()
                || statement.placeholder_view
            {
                continue;
            }
            let object = mapping(spec, &owner, input)?;
            let db = object.source.database.clone().unwrap_or_default();
            let key = (
                db.clone(),
                object.source.name.clone(),
                object.source.kind.clone(),
            );
            if !selected.get(&key).copied().unwrap_or(false) {
                continue;
            }
            if statement.unresolved_references {
                return Err(AppError::new(
                    "unresolved_reference",
                    "所选对象包含无法确认作用域的引用",
                ));
            }
            for reference in &statement.references {
                let (ref_db, name) = reference.rsplit_once('.').unwrap_or((&db, reference));
                if selected
                    .iter()
                    .any(|((d, n, _), enabled)| !enabled && d == ref_db && n == name)
                {
                    return Err(AppError::new(
                        "unresolved_reference",
                        format!(
                            "已选对象 {} 依赖未选对象 {reference}，请一并选择",
                            object.source.name
                        ),
                    ));
                }
            }
        }
    }
    Ok(())
}

fn index_for_item(id: &str) -> Result<(usize, usize)> {
    let mut parts = id.split(':');
    if parts.next() != Some("sql") {
        return Err(AppError::invalid("恢复步骤标识无效"));
    }
    let input = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| AppError::invalid("恢复输入序号无效"))?;
    let statement = parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| AppError::invalid("恢复语句序号无效"))?;
    Ok((input, statement))
}

async fn prior_state(
    store: &crate::Store,
    prior: Option<uuid::Uuid>,
    input: usize,
    offset: u64,
    phase: &str,
) -> Result<Option<TransferStepState>> {
    match prior {
        Some(run) => {
            store
                .transfer_prior_input_state(run, input, offset, phase)
                .await
        }
        None => Ok(None),
    }
}

fn skip_prior_state(journal: &mut TransferJournal, state: Option<TransferStepState>) -> bool {
    if journal.retry_of.is_none() {
        return false;
    }
    match state {
        Some(TransferStepState::Applied | TransferStepState::Skipped) => {
            journal.progress.skipped += 1;
            true
        }
        Some(TransferStepState::Sent | TransferStepState::OutcomeUnknown) => {
            journal.progress.outcome_unknown += 1;
            true
        }
        other
            if !journal.retry_states.is_empty()
                && !journal
                    .retry_states
                    .contains(&other.unwrap_or(TransferStepState::NotSent)) =>
        {
            journal.progress.skipped += 1;
            true
        }
        _ => false,
    }
}

async fn prior_skip(
    journal: &mut TransferJournal,
    input: usize,
    offset: u64,
    phase: &str,
) -> Result<bool> {
    let state = prior_state(&journal.sink.store, journal.retry_of, input, offset, phase).await?;
    Ok(skip_prior_state(journal, state))
}

impl MysqlSession {
    /// 重新校验完整输入及目标计划，逐步骤登记发送和提交结果。
    pub async fn restore_sql(
        &mut self,
        prepared: &PreparedTransfer,
        journal: &mut TransferJournal,
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.profile.ensure_write()?;
        let spec = &prepared.spec;
        let index_sink = journal
            .sink
            .clone()
            .with_transfer_step(journal.run_id, journal.next_ordinal());
        let sources = inputs(spec, &index_sink, cancel).await?;
        let mut current = self
            .restore_plan(
                spec,
                &sources,
                &journal.sink.store,
                prepared.retry_of,
                cancel,
            )
            .await?;
        if current.len() != prepared.items.len() {
            return Err(AppError::conflict("恢复计划已变化，请重新准备"));
        }
        let mut confirmed_by_id = BTreeMap::new();
        for confirmed in &prepared.items {
            if confirmed_by_id
                .insert(confirmed.id.as_str(), confirmed)
                .is_some()
            {
                return Err(AppError::invalid("恢复计划含重复步骤 ID"));
            }
        }
        for now in &mut current {
            let confirmed = confirmed_by_id
                .get(now.id.as_str())
                .ok_or_else(|| AppError::conflict("恢复计划步骤已变化"))?;
            if now.sql != confirmed.sql
                || now.precondition != confirmed.precondition
                || now.action != confirmed.action
                || serde_json::to_value(&now.source)? != serde_json::to_value(&confirmed.source)?
                || serde_json::to_value(&now.target)? != serde_json::to_value(&confirmed.target)?
                || now.phase != confirmed.phase
                || now.destructive != confirmed.destructive
            {
                return Err(AppError::conflict(
                    "恢复输入、目标结构或确认选择已变化，请重新准备",
                ));
            }
            now.selected = confirmed.selected;
            if (now.phase == "schema" || now.phase == "after_data")
                && now.selected
                && now.sql.is_none()
            {
                return Err(AppError::new(
                    "schema_incomplete",
                    format!("结构项 {} 尚不能安全执行", now.id),
                ));
            }
        }
        validate_restore_selection(&current, &sources, spec)?;
        let mut fingerprints: BTreeMap<(String, String), String> = BTreeMap::new();
        let mut active_context: Option<TransferPlanItem> = None;
        let result: Result<()> = async {
            for entry in &current {
                ensure_not_cancelled(cancel)?;
                if !entry.selected
                    || entry.action == "wrapper_conversion"
                    || entry.action == "object_scope"
                {
                    continue;
                }
                if entry.action == "prior_unknown" {
                    journal.progress.outcome_unknown += 1;
                    journal.checkpoint().await?;
                    continue;
                }
                if entry.action == "prior_object" {
                    journal.progress.skipped += 1;
                    journal.checkpoint().await?;
                    continue;
                }
                let (input, number) = index_for_item(&entry.id)?;
                let source = sources
                    .get(input)
                    .ok_or_else(|| AppError::invalid("恢复输入序号越界"))?;
                let statement = source
                    .index
                    .statements
                    .get(number)
                    .ok_or_else(|| AppError::invalid("恢复语句序号越界"))?;
                if entry.action == "create_database" {
                    let db = entry
                        .target
                        .as_ref()
                        .and_then(|target| target.database.as_deref())
                        .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                    if self
                        .profile
                        .direct()?
                        .visible_databases
                        .as_ref()
                        .is_some_and(|allowed| !allowed.iter().any(|name| name == db))
                    {
                        return Err(AppError::new(
                            "permission_denied",
                            format!("目标数据库 {db} 不在连接可见范围内"),
                        ));
                    }
                    if prior_skip(journal, input, statement.start, "database:create").await? {
                        continue;
                    }
                    let mut conn = self.new_connection().await?;
                    let exists: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME=?",
                    )
                    .bind(db)
                    .fetch_one(&mut conn)
                    .await?;
                    if exists != 0 {
                        return Err(AppError::conflict(format!(
                            "目标数据库 {db} 已出现，请重新准备"
                        )));
                    }
                    let sql = entry
                        .sql
                        .as_deref()
                        .ok_or_else(|| AppError::invalid("缺少建库 SQL"))?;
                    let ticket = journal
                        .begin(
                            "database:create",
                            TransferLocator {
                                object: Some(db.into()),
                                input: Some(input),
                                byte_start: Some(statement.start),
                                byte_end: Some(statement.end),
                                ..Default::default()
                            },
                            format!("{:x}", Sha256::digest(sql.as_bytes())),
                        )
                        .await?;
                    if let Err(error) = ticket.sink.mark_sent(true).await {
                        journal
                            .finish(
                                ticket,
                                TransferStepState::NotSent,
                                0,
                                Vec::new(),
                                Some(&error),
                                None,
                            )
                            .await?;
                        return Err(error);
                    }
                    match sqlx::query(sqlx::AssertSqlSafe(sql))
                        .execute(&mut conn)
                        .await
                    {
                        Ok(_) if database_defaults(self, db).await.is_ok() => {
                            journal
                                .finish(
                                    ticket,
                                    TransferStepState::Applied,
                                    1,
                                    Vec::new(),
                                    None,
                                    None,
                                )
                                .await?
                        }
                        result => {
                            let error = match result {
                                Err(error) if error.as_database_error().is_some() => {
                                    AppError::from(error)
                                }
                                Err(error) => {
                                    AppError::unknown(format!("创建数据库结果无法确认: {error}"))
                                }
                                Ok(_) => AppError::unknown("数据库已创建但读回失败"),
                            };
                            let state = if error.code == "outcome_unknown" {
                                TransferStepState::OutcomeUnknown
                            } else {
                                TransferStepState::Failed
                            };
                            journal
                                .finish(ticket, state, 0, Vec::new(), Some(&error), None)
                                .await?;
                            return Err(error);
                        }
                    }
                    journal.checkpoint().await?;
                    continue;
                }
                let object = mapping(spec, statement, input)?;
                let target_db = object
                    .target
                    .database
                    .as_deref()
                    .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                if self
                    .profile
                    .direct()?
                    .visible_databases
                    .as_ref()
                    .is_some_and(|allowed| !allowed.iter().any(|db| db == target_db))
                {
                    return Err(AppError::new(
                        "permission_denied",
                        format!("目标数据库 {target_db} 不在连接可见范围内"),
                    ));
                }
                if entry.action == "database_context_enter"
                    || entry.action == "database_context_leave"
                {
                    self.apply_database_context(entry, input, statement, journal)
                        .await?;
                    if entry.action == "database_context_enter" {
                        let leave_id = format!("{}:context-leave", statement_id(input, number));
                        active_context = Some(
                            prepared
                                .items
                                .iter()
                                .find(|item| item.id == leave_id && item.selected)
                                .ok_or_else(|| {
                                    AppError::invalid("数据库上下文缺少已确认的恢复步骤")
                                })?
                                .clone(),
                        );
                    } else {
                        active_context = None;
                    }
                } else if entry.phase == "schema" || entry.phase == "after_data" {
                    let db = object
                        .target
                        .database
                        .clone()
                        .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                    let key = (db, object.target.name.clone());
                    let expected = fingerprints.get(&key).cloned().or_else(|| {
                        entry
                            .precondition
                            .as_ref()
                            .filter(|value| value.as_str() != "absent")
                            .cloned()
                    });
                    let phase = if entry.action == "create_table" {
                        "schema:create_table".to_owned()
                    } else {
                        format!("schema:{:x}", Sha256::digest(entry.id.as_bytes()))
                    };
                    if prior_skip(journal, input, statement.start, &phase).await? {
                        continue;
                    }
                    let locator = TransferLocator {
                        object: Some(object.target.name.clone()),
                        input: Some(input),
                        byte_start: Some(statement.start),
                        byte_end: Some(statement.end),
                        ..Default::default()
                    };
                    let ticket = journal
                        .begin(
                            &phase,
                            locator,
                            format!("{:x}", Sha256::digest(entry.id.as_bytes())),
                        )
                        .await?;
                    let result = self
                        .apply_schema(&object.target, entry, expected.as_deref(), &ticket.sink)
                        .await;
                    match result {
                        Ok(fingerprint) => {
                            fingerprints.insert(key, fingerprint);
                            journal
                                .finish(
                                    ticket,
                                    TransferStepState::Applied,
                                    1,
                                    Vec::new(),
                                    None,
                                    None,
                                )
                                .await?;
                        }
                        Err(error) => {
                            let state = if error.code == "outcome_unknown" {
                                TransferStepState::OutcomeUnknown
                            } else {
                                TransferStepState::Failed
                            };
                            journal
                                .finish(ticket, state, 0, Vec::new(), Some(&error), None)
                                .await?;
                            return Err(error);
                        }
                    }
                } else if entry.action == "insert" {
                    self.restore_rows(
                        RestoreRowsContext {
                            spec,
                            input,
                            number,
                            source,
                            object: &object,
                            entry,
                        },
                        journal,
                        cancel,
                    )
                    .await?;
                } else if entry.action == "create_object" || entry.action == "drop_object" {
                    if entry.action == "drop_object" {
                        let (show, index) = super::show_command(&object.target)?;
                        let mut conn = self.new_connection().await?;
                        let row = sqlx::query(sqlx::AssertSqlSafe(show))
                            .fetch_one(&mut conn)
                            .await?;
                        let ddl = super::string_at(&row, index, "目标对象定义")?;
                        let current = format!("{:x}", Sha256::digest(ddl.as_bytes()));
                        if entry.precondition.as_deref() != Some(current.as_str()) {
                            return Err(AppError::conflict("目标对象定义已变化，请重新准备"));
                        }
                    }
                    let sql = entry
                        .sql
                        .as_deref()
                        .ok_or_else(|| AppError::invalid("对象步骤缺少 SQL"))?;
                    let mut conn = self.new_connection().await?;
                    let db = object
                        .target
                        .database
                        .as_deref()
                        .ok_or_else(|| AppError::invalid("目标库缺失"))?;
                    sqlx::raw_sql(sqlx::AssertSqlSafe(format!("USE {}", quote_ident(db)?)))
                        .execute(&mut conn)
                        .await?;
                    if entry.action == "create_object" {
                        for setting in &statement.context.settings {
                            sqlx::raw_sql(sqlx::AssertSqlSafe(setting.as_str()))
                                .execute(&mut conn)
                                .await?;
                        }
                    }
                    let locator = TransferLocator {
                        object: Some(object.target.name.clone()),
                        input: Some(input),
                        byte_start: Some(statement.start),
                        byte_end: Some(statement.end),
                        ..Default::default()
                    };
                    let phase = format!("object:{}", entry.action);
                    if prior_skip(journal, input, statement.start, &phase).await? {
                        continue;
                    }
                    let ticket = journal
                        .begin(
                            &phase,
                            locator,
                            format!("{:x}", Sha256::digest(sql.as_bytes())),
                        )
                        .await?;
                    ticket.sink.mark_sent(true).await?;
                    // CREATE PROCEDURE/FUNCTION/TRIGGER 不受 MySQL prepared protocol 支持；正文已由受控 reader 限定为单个 DDL。
                    let result = sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                        .execute(&mut conn)
                        .await;
                    match result {
                        Ok(_) => {
                            let exists = existing_object(self, &object).await;
                            match exists {
                                Ok(exists) if exists == (entry.action == "create_object") => {
                                    journal
                                        .finish(
                                            ticket,
                                            TransferStepState::Applied,
                                            1,
                                            Vec::new(),
                                            None,
                                            None,
                                        )
                                        .await?
                                }
                                _ => {
                                    let error = AppError::unknown(format!(
                                        "对象 {} 的 DDL 已发送，但读回无法确认",
                                        object.target.name
                                    ));
                                    journal
                                        .finish(
                                            ticket,
                                            TransferStepState::OutcomeUnknown,
                                            0,
                                            Vec::new(),
                                            Some(&error),
                                            None,
                                        )
                                        .await?;
                                    return Err(error);
                                }
                            }
                        }
                        Err(e) => {
                            let error = if e.as_database_error().is_some() {
                                AppError::from(e)
                            } else {
                                AppError::unknown(format!("对象 DDL 结果无法确认: {e}"))
                            };
                            let state = if error.code == "outcome_unknown" {
                                TransferStepState::OutcomeUnknown
                            } else {
                                TransferStepState::Failed
                            };
                            journal
                                .finish(ticket, state, 0, Vec::new(), Some(&error), None)
                                .await?;
                            return Err(error);
                        }
                    }
                }
                journal.checkpoint().await?;
            }
            Ok(())
        }
        .await;
        if let (Err(error), Some(leave)) = (&result, active_context) {
            let (input, number) = index_for_item(&leave.id)?;
            let cleanup = self
                .apply_database_context(
                    &leave,
                    input,
                    &sources[input].index.statements[number],
                    journal,
                )
                .await;
            if let Err(cleanup_error) = cleanup {
                return Err(AppError::unknown(format!(
                    "恢复失败: {error}；数据库默认值收尾未确认: {cleanup_error}"
                )));
            }
        }
        result
    }

    async fn apply_database_context(
        &self,
        entry: &TransferPlanItem,
        input: usize,
        statement: &SqlStatement,
        journal: &mut TransferJournal,
    ) -> Result<()> {
        let target = entry
            .target
            .as_ref()
            .ok_or_else(|| AppError::invalid("数据库上下文缺少目标"))?;
        let db = target
            .database
            .as_deref()
            .ok_or_else(|| AppError::invalid("目标库缺失"))?;
        let (before, desired): ((String, String), (String, String)) = serde_json::from_str(
            entry
                .precondition
                .as_deref()
                .ok_or_else(|| AppError::invalid("数据库上下文缺少前置条件"))?,
        )?;
        let enter = entry.action == "database_context_enter";
        let expected = if enter { &before } else { &desired };
        let result = if enter { &desired } else { &before };
        if &database_defaults(self, db).await? != expected {
            return Err(AppError::conflict(format!(
                "数据库 {db} 默认值已被外部修改，未执行上下文变更"
            )));
        }
        let sql = entry
            .sql
            .as_deref()
            .ok_or_else(|| AppError::invalid("数据库上下文缺少 SQL"))?;
        let mut conn = self.new_connection().await?;
        let ticket = journal
            .begin(
                "database_context",
                TransferLocator {
                    object: Some(db.to_owned()),
                    input: Some(input),
                    byte_start: Some(statement.start),
                    byte_end: Some(statement.end),
                    ..Default::default()
                },
                format!("{:x}", Sha256::digest(sql.as_bytes())),
            )
            .await?;
        if let Err(error) = ticket.sink.mark_sent(true).await {
            journal
                .finish(
                    ticket,
                    TransferStepState::NotSent,
                    0,
                    Vec::new(),
                    Some(&error),
                    None,
                )
                .await?;
            return Err(error);
        }
        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&mut conn)
            .await
        {
            let error = if e.as_database_error().is_some() {
                AppError::from(e)
            } else {
                AppError::unknown(format!("数据库默认值变更结果无法确认: {e}"))
            };
            let state = if error.code == "outcome_unknown" {
                TransferStepState::OutcomeUnknown
            } else {
                TransferStepState::Failed
            };
            journal
                .finish(ticket, state, 0, Vec::new(), Some(&error), None)
                .await?;
            return Err(error);
        }
        if database_defaults(self, db).await.ok().as_ref() != Some(result) {
            let error = AppError::unknown(format!("数据库 {db} 默认值变更已发送，读回无法确认"));
            journal
                .finish(
                    ticket,
                    TransferStepState::OutcomeUnknown,
                    0,
                    Vec::new(),
                    Some(&error),
                    None,
                )
                .await?;
            return Err(error);
        }
        journal
            .finish(
                ticket,
                TransferStepState::Applied,
                1,
                Vec::new(),
                None,
                None,
            )
            .await
    }

    async fn restore_rows(
        &mut self,
        context: RestoreRowsContext<'_>,
        journal: &mut TransferJournal,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let RestoreRowsContext {
            spec,
            input,
            number,
            source,
            object,
            entry,
        } = context;
        let statement = &source.index.statements[number];
        let db = object
            .target
            .database
            .as_deref()
            .ok_or_else(|| AppError::invalid("目标库缺失"))?;
        let mut conn = self.new_connection().await?;
        let triggers: Vec<String> = sqlx::query_scalar("SELECT TRIGGER_NAME FROM information_schema.TRIGGERS WHERE EVENT_OBJECT_SCHEMA=? AND EVENT_OBJECT_TABLE=? ORDER BY TRIGGER_NAME").bind(db).bind(&object.target.name).fetch_all(&mut conn).await?;
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(&source.index.sha256, &triggers))?)
        );
        if entry.precondition.as_deref() != Some(fingerprint.as_str()) {
            return Err(AppError::conflict("目标触发器清单已变化，请重新准备"));
        }
        let target = target_table(self, object, cancel).await?.ok_or_else(|| {
            AppError::new(
                "target_missing",
                format!("目标表 {} 不存在", object.target.name),
            )
        })?;
        let columns = if let Some(columns) = &statement.source_columns {
            columns.clone()
        } else {
            let columns: Vec<String> = spec.inputs[input]
                .columns
                .iter()
                .filter(|c| !c.generated)
                .map(|c| c.name.clone())
                .collect();
            if columns.is_empty() {
                return Err(AppError::new(
                    "source_columns_required",
                    "INSERT 源列顺序未知，请明确提供",
                ));
            }
            columns
        };
        let mut mapped = object.clone();
        mapped.columns = columns
            .iter()
            .map(|name| ColumnMapping {
                source: name.clone(),
                target: object
                    .columns
                    .iter()
                    .find(|m| m.source == *name)
                    .map(|m| m.target.clone())
                    .or_else(|| spec.inputs[input].column_mapping.get(name).cloned())
                    .unwrap_or_else(|| name.clone()),
            })
            .collect();
        let target_columns = mapped
            .columns
            .iter()
            .map(|mapping| {
                target
                    .schema
                    .columns
                    .iter()
                    .find(|column| column.name == mapping.target)
                    .ok_or_else(|| AppError::invalid(format!("目标字段不存在: {}", mapping.target)))
            })
            .collect::<Result<Vec<_>>>()?;
        let path = source.path.clone();
        let index = source.index.clone();
        // 单槽队列与 reader、当前行和写入批次共同受执行阶段的四份行预算约束。
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<Result<(Vec<Value>, u64)>>(1);
        let reader_stop = cancel.child_token();
        let reader_cancel = reader_stop.clone();
        let reservation = uuid::Uuid::new_v4();
        let store = journal.sink.store.clone();
        let execution_id = journal.sink.execution_id;
        let runtime = tokio::runtime::Handle::current();
        let mut reserved = 0usize;
        let mut scan_budget = sql_restore::IndexBudget::new();
        scan_budget.set_tuple_reserver(Box::new(move |bytes| {
            // reader、单槽队列、当前行与写入批次可能同时持有不同的行。
            let required = bytes.saturating_mul(4);
            if required > reserved {
                let next = required.saturating_add(1024 * 1024 - 1) / (1024 * 1024) * (1024 * 1024);
                runtime.block_on(store.reserve_auxiliary(execution_id, reservation, next))?;
                reserved = next;
            }
            Ok(())
        }));
        let task = tokio::task::spawn_blocking(move || {
            let result = sql_restore::read_insert_statement_with_budget(
                path,
                &index,
                number,
                &reader_cancel,
                &mut scan_budget,
                |_, row, offset| {
                    sender
                        .blocking_send(Ok((row, offset)))
                        .map_err(|_| AppError::cancelled())
                },
            );
            if let Err(error) = result {
                let _ = sender.blocking_send(Err(error));
            }
        });
        let batch_context = RestoreBatchContext {
            spec,
            input,
            number,
            source,
            target: &target,
            object: &mapped,
        };
        let mut batch = Vec::new();
        let mut offsets = Vec::new();
        let mut bytes = 0usize;
        // 绝对 offset 只在当前语句内求差，避免按对象重排或多文件执行时重复累计。
        // 当前记录的尾部在下一条边界（或语句完成）后计入；写入确认仍独立统计。
        let mut consumed_to = statement.start;
        let result: Result<()> = async {
        while let Some(row) = tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), row = receiver.recv() => row }
        {
            ensure_not_cancelled(cancel)?;
            let (mut row, offset) = match row {
                Ok(row) => row,
                Err(error) => {
                    self.flush_restore_batch(&batch_context, &mut batch, &mut offsets, journal)
                        .await?;
                    return Err(error);
                }
            };
            journal.progress.processed_bytes = journal
                .progress
                .processed_bytes
                .saturating_add(offset.saturating_sub(consumed_to));
            consumed_to = offset;
            let previous =
                prior_state(&journal.sink.store, journal.retry_of, input, offset, "data").await?;
            if skip_prior_state(journal, previous) {
                self.flush_restore_batch(&batch_context, &mut batch, &mut offsets, journal)
                    .await?;
                bytes = 0;
                continue;
            }
            if row.len() != columns.len() {
                self.flush_restore_batch(&batch_context, &mut batch, &mut offsets, journal)
                    .await?;
                return Err(AppError::invalid(format!(
                    "字节 {offset} 的 INSERT 列数不符"
                )));
            }
            if let Err(cause) = row
                .iter_mut()
                .zip(&target_columns)
                .try_for_each(|(value, column)| coerce_sql_literal(value, column))
            {
                self.flush_restore_batch(&batch_context, &mut batch, &mut offsets, journal)
                    .await?;
                bytes = 0;
                let error = AppError::new("conversion", "SQL 记录无法按目标列类型无损转换")
                    .with_details(serde_json::json!({"input":input,"byte_offset":offset,"recoverable":true,"cause_code":cause.code}));
                let ticket = journal
                    .begin(
                        "data",
                        TransferLocator {
                            object: Some(mapped.target.name.clone()),
                            input: Some(input),
                            byte_start: Some(offset),
                            byte_end: Some(offset),
                            record_offsets: vec![offset],
                            ..Default::default()
                        },
                        crate::execution::migration::digest(&(
                            &source.index.sha256,
                            number,
                            offset,
                            "conversion",
                        ))?,
                    )
                    .await?;
                let skip = spec.options.conversion_errors
                    == crate::model::transfer::ConversionPolicy::Skip;
                let state = if skip {
                    journal.progress.skipped += 1;
                    TransferStepState::Skipped
                } else {
                    journal.progress.failed += 1;
                    TransferStepState::Failed
                };
                journal.progress.read_rows += 1;
                journal
                    .finish(ticket, state, 0, vec![state], Some(&error), None)
                    .await?;
                journal.checkpoint().await?;
                if !skip {
                    return Err(error);
                }
                continue;
            }
            let size = crate::execution::migration::encoded_size(&row)?;
            if !batch.is_empty()
                && (batch.len() >= crate::model::transfer::MAX_BATCH_ROWS
                    || bytes.saturating_add(size) > crate::model::transfer::MAX_BATCH_BYTES
                    || !target.schema.engine.eq_ignore_ascii_case("innodb"))
            {
                self.flush_restore_batch(&batch_context, &mut batch, &mut offsets, journal)
                    .await?;
                bytes = 0;
            }
            bytes = bytes.saturating_add(size);
            batch.push(row);
            offsets.push(offset);
        }
        self.flush_restore_batch(&batch_context, &mut batch, &mut offsets, journal)
            .await?;
        Ok(())
        }.await;
        if result.is_err() {
            reader_stop.cancel();
        }
        drop(receiver);
        let joined = task
            .await
            .map_err(|e| AppError::new("internal", format!("SQL 数据读取任务失败: {e}")));
        journal.sink.store.release_auxiliary(reservation).await?;
        joined?;
        result?;
        journal.progress.processed_bytes = journal
            .progress
            .processed_bytes
            .saturating_add(statement.end.saturating_sub(consumed_to));
        journal.checkpoint().await?;
        Ok(())
    }

    async fn flush_restore_batch(
        &self,
        context: &RestoreBatchContext<'_>,
        rows: &mut Vec<Vec<Value>>,
        offsets: &mut Vec<u64>,
        journal: &mut TransferJournal,
    ) -> Result<()> {
        let RestoreBatchContext {
            spec,
            input,
            number,
            source,
            target,
            object,
        } = *context;
        if rows.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(rows);
        let offsets = std::mem::take(offsets);
        let count = offsets.len() as u64;
        let digest =
            crate::execution::migration::digest(&(&source.index.sha256, number, &offsets))?;
        let locator = TransferLocator {
            object: Some(object.target.name.clone()),
            input: Some(input),
            byte_start: offsets.first().copied(),
            byte_end: offsets.last().copied(),
            record_offsets: offsets,
            ..Default::default()
        };
        let ticket = journal.begin("data", locator, digest).await?;
        match self
            .write_batch_with_context(
                target,
                object,
                spec.options.conflict,
                rows,
                &ticket.sink,
                &source.index.statements[number].context.settings,
            )
            .await
        {
            Ok(outcome) => {
                let state = if outcome.affected == 0 {
                    TransferStepState::Skipped
                } else {
                    TransferStepState::Applied
                };
                journal.progress.read_rows += count;
                journal.progress.written_rows += outcome.affected;
                journal.progress.skipped += outcome
                    .states
                    .iter()
                    .filter(|state| **state == TransferStepState::Skipped)
                    .count() as u64;
                journal
                    .finish(ticket, state, outcome.affected, outcome.states, None, None)
                    .await?;
                journal.checkpoint().await
            }
            Err(mut error) => {
                let states: Vec<TransferStepState> = error
                    .details
                    .get("states")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();
                let failed = error
                    .details
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|index| ticket.locator().record_offsets.get(index as usize))
                    .copied();
                if let (Some(offset), Some(details)) = (failed, error.details.as_object_mut()) {
                    details.insert("byte_offset".into(), serde_json::json!(offset));
                    details.insert("input".into(), serde_json::json!(input));
                }
                let state = if error.code == "outcome_unknown" {
                    TransferStepState::OutcomeUnknown
                } else {
                    TransferStepState::Failed
                };
                journal
                    .finish(ticket, state, 0, states, Some(&error), failed)
                    .await?;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
#[path = "../../../../tests/unit/sql_restore_selection.rs"]
mod selection_tests;
