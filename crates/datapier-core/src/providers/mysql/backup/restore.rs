//! SQL 恢复计划和执行：输入预检先于任何目标写入。
mod indexes;
mod plan;
mod run;
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
use crate::providers::mysql::migration::write::MigrationBatchSource;
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

#[cfg(test)]
#[path = "../../../../tests/unit/sql_restore_selection.rs"]
mod selection_tests;
