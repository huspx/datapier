use futures_util::TryStreamExt;
use serde_json::{Value as Json, json};
use sqlx::{
    Connection, Executor, Row, SqlSafeStr, Statement,
    mysql::{MySqlArguments, MySqlConnectOptions, MySqlConnection},
};
use std::{collections::HashSet, future::Future, time::Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{
    MysqlSession,
    catalog::add_filter_arg,
    errors,
    results::{columns, decode_row},
};
use crate::{ExecutionSink, model::*};

#[derive(Clone)]
pub(super) struct Recipe {
    pub sql: String,
    pub bindings: Vec<Json>,
    pub source_columns: Option<Vec<Column>>,
    pub independent: bool,
    pub context: Option<(Option<String>, String)>,
    pub proof: Option<(Vec<crate::policy::ObjectAccess>, crate::policy::SqlContext)>,
}

impl Recipe {
    pub(super) fn arguments(&self) -> Result<MySqlArguments> {
        let mut args = MySqlArguments::default();
        for value in &self.bindings {
            add_filter_arg(&mut args, value)?;
        }
        Ok(args)
    }
    fn wrapped(&self) -> String {
        // 外层分页保留原查询的 LIMIT/OFFSET；不能向原文追加 LIMIT。
        format!(
            "SELECT * FROM (\n{}\n) AS `_datapier_page`",
            self.sql.trim().trim_end_matches(';')
        )
    }
}

pub(super) async fn validate_context(s: &mut MysqlSession, recipe: &Recipe) -> Result<()> {
    if let Some((database, mode)) = &recipe.context {
        let row = sqlx::raw_sql("SELECT DATABASE() AS db, @@SESSION.sql_mode AS sql_mode")
            .fetch_one(s.connection()?)
            .await
            .map_err(errors::mysql_error)?;
        if &row.try_get::<Option<String>, _>("db")? != database
            || &row.try_get::<String, _>("sql_mode")? != mode
        {
            return Err(AppError::new(
                "conflict",
                "会话数据库或 SQL mode 已变化，请重新执行查询",
            ));
        }
    }
    Ok(())
}

// The target connection remains exclusively borrowed until both KILL and the
// response finish; a late KILL must never affect the next request.
async fn cancellable<T>(
    options: &MySqlConnectOptions,
    connect_timeout: Duration,
    thread_id: u64,
    cancel: &CancellationToken,
    future: impl Future<Output = Result<T>>,
) -> (Result<T>, bool) {
    tokio::pin!(future);
    tokio::select! {
        biased;
        result = &mut future => {
            let safe = result.as_ref().map_or_else(|e| e.database_code.is_some() || matches!(e.code.as_str(), "conflict" | "resource_limit" | "unsupported_type"), |_| true);
            (result, safe)
        }
        _ = cancel.cancelled() => {
            let cleanup = tokio::time::timeout(Duration::from_secs(5), async {
                let _killed = match tokio::time::timeout(connect_timeout, MySqlConnection::connect_with(options)).await {
                    Ok(Ok(mut control)) => sqlx::raw_sql(sqlx::AssertSqlSafe(format!("KILL QUERY {thread_id}")))
                        .execute(&mut control).await.map(|_| ()).map_err(errors::mysql_error),
                    Ok(Err(error)) => Err(errors::mysql_error(error)),
                    Err(_) => Err(AppError::new("timeout", "连接超时")),
                };
                future.await
            }).await;
            match cleanup {
                Ok(Ok(_)) => (Err(AppError::cancelled()), true),
                Ok(Err(error)) if error.details.get("mysql_errno").and_then(Json::as_u64) == Some(1317) => (Err(AppError::cancelled()), true),
                Ok(Err(error)) if error.database_code.is_some() => (Err(error), true),
                _ => (Err(AppError::cancelled()), false),
            }
        }
    }
}
async fn dependencies_on(
    conn: &mut MySqlConnection,
    objects: Vec<crate::policy::ObjectAccess>,
    context: &crate::policy::SqlContext,
) -> Result<bool> {
    let mut pending = objects;
    let mut seen = HashSet::new();
    while let Some(object) = pending.pop() {
        if !seen.insert((object.database.clone(), object.name.clone())) {
            continue;
        }
        if seen.len() > 256 {
            return Ok(false);
        }
        let sql = format!(
            "SHOW CREATE TABLE {}.{}",
            super::quote_ident(&object.database)?,
            super::quote_ident(&object.name)?
        );
        let row = match sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
            .fetch_one(&mut *conn)
            .await
        {
            Ok(row) => row,
            Err(sqlx::Error::Database(_)) => return Ok(false),
            Err(error) => return Err(errors::mysql_error(error)),
        };
        let ddl: String = row.try_get(1)?;
        if ddl.starts_with("CREATE TABLE ") || ddl.starts_with("CREATE TEMPORARY TABLE ") {
            continue;
        }
        let mut view_context = context.clone();
        view_context.database = Some(object.database);
        let proof = match crate::policy::analyze_view_ddl(&ddl, &view_context) {
            Ok(proof) if proof.replayable => proof,
            _ => return Ok(false),
        };
        pending.extend(proof.objects);
    }
    Ok(true)
}

pub(super) async fn dependencies_replayable(
    s: &mut MysqlSession,
    objects: Vec<crate::policy::ObjectAccess>,
    context: &crate::policy::SqlContext,
    independent: bool,
    cancel: &CancellationToken,
) -> Result<bool> {
    if independent {
        return tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(AppError::cancelled()),
            result = async { let mut conn = s.new_connection().await?; dependencies_on(&mut conn, objects, context).await } => result,
        };
    }
    let options = s.options.clone();
    let thread = s.thread_id;
    let (result, safe) = cancellable(
        &options,
        s.connect_timeout,
        thread,
        cancel,
        dependencies_on(s.connection()?, objects, context),
    )
    .await;
    if !safe {
        s.invalidate_connection();
    }
    result
}

async fn page(
    conn: &mut MySqlConnection,
    recipe: &Recipe,
    start: u64,
    size: usize,
    budget: usize,
) -> Result<(Vec<Column>, Vec<Vec<Value>>, bool)> {
    let sql = format!(
        "{} LIMIT {} OFFSET {}",
        recipe.wrapped(),
        size.saturating_add(1),
        start
    );
    let stmt = conn
        .prepare(sqlx::AssertSqlSafe(sql).into_sql_str())
        .await
        .map_err(errors::mysql_error)?;
    let mut meta = columns(stmt.columns());
    if let Some(source) = &recipe.source_columns {
        if meta.len() != source.len() || meta.iter().zip(source).any(|(a, b)| a.name != b.name) {
            return Err(AppError::new("conflict", "表结构已变化，请重新浏览"));
        }
        for (column, original) in meta.iter_mut().zip(source) {
            column.comment = original.comment.clone();
        }
    }
    let mut stream = stmt.query_with(recipe.arguments()?).fetch(&mut *conn);
    let mut rows = Vec::with_capacity(size);
    let mut more = false;
    let mut retained_bytes = serde_json::to_vec(&meta)?.len();
    loop {
        let next = stream.try_next().await.map_err(errors::mysql_error)?;
        let Some(row) = next else { break };
        if more || rows.len() == size {
            more = true;
            continue;
        }
        let decoded = match decode_row(&row) {
            Ok(value) => value,
            Err(error) => {
                while stream
                    .try_next()
                    .await
                    .map_err(errors::mysql_error)?
                    .is_some()
                {}
                return Err(error);
            }
        };
        let bytes = serde_json::to_vec(&decoded)?.len();
        if retained_bytes.saturating_add(bytes) > budget {
            if rows.is_empty() {
                while stream
                    .try_next()
                    .await
                    .map_err(errors::mysql_error)?
                    .is_some()
                {}
                return Err(AppError::new("resource_limit", "结果行超过内存预算"));
            }
            more = true;
            continue;
        }
        retained_bytes += bytes;
        rows.push(decoded);
    }
    Ok((meta, rows, more))
}

async fn fetch_page(
    s: &mut MysqlSession,
    recipe: &Recipe,
    start: u64,
    size: usize,
    budget: usize,
    cancel: &CancellationToken,
) -> Result<(Vec<Column>, Vec<Vec<Value>>, bool)> {
    if recipe.independent {
        return tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(AppError::cancelled()),
            result = async { let mut conn = s.new_connection().await?; page(&mut conn, recipe, start, size, budget).await } => result,
        };
    }
    let options = s.options.clone();
    let thread = s.thread_id;
    let (result, safe) = cancellable(
        &options,
        s.connect_timeout,
        thread,
        cancel,
        page(s.connection()?, recipe, start, size, budget),
    )
    .await;
    if !safe {
        s.invalidate_connection();
    }
    result
}

async fn checked_context(
    s: &mut MysqlSession,
    recipe: &Recipe,
    cancel: &CancellationToken,
) -> Result<()> {
    let options = s.options.clone();
    let thread = s.thread_id;
    let timeout = s.connect_timeout;
    let (result, safe) = cancellable(
        &options,
        timeout,
        thread,
        cancel,
        validate_context(s, recipe),
    )
    .await;
    if !safe {
        s.invalidate_connection();
    }
    result
}

pub(super) async fn start(
    s: &mut MysqlSession,
    recipe: Recipe,
    size: usize,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    let budget = sink
        .store
        .settings()
        .await?
        .limits
        .result_memory_bytes_per_client;
    let (meta, rows, more) = fetch_page(s, &recipe, 0, size, budget, cancel).await?;
    let id = sink.start(meta, "rows").await?;
    let count = rows.len();
    if !rows.is_empty() {
        sink.append(id, rows).await?;
    }
    sink.finish(
        id,
        json!({"retrieval_mode":"live_page","has_more":more,"page_start":0,"total_rows":null}),
    )
    .await?;
    s.recipes.insert(id, recipe);
    Ok(json!({"results":[id],"row_count":count}))
}

pub(super) async fn read(
    s: &mut MysqlSession,
    params: &Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    let id: Uuid = param(params, "result_id")?;
    let recipe = s
        .recipes
        .get(&id)
        .cloned()
        .ok_or_else(|| AppError::new("not_found", "结果不属于当前会话或已过期"))?;
    // 已释放的结果不能通过保留的 recipe 复活。
    sink.store.result_meta(id).await?;
    checked_context(s, &recipe, cancel).await?;
    if let Some((objects, context)) = &recipe.proof
        && !dependencies_replayable(s, objects.clone(), context, recipe.independent, cancel).await?
    {
        return Err(AppError::new("conflict", "查询依赖已变化，请重新执行"));
    }
    let start = params.get("start").and_then(Json::as_u64).unwrap_or(0);
    let size = params
        .get("max_rows")
        .and_then(Json::as_u64)
        .unwrap_or(100)
        .clamp(1, 1000) as usize;
    let max_bytes = params
        .get("max_bytes")
        .and_then(Json::as_u64)
        .unwrap_or(1024 * 1024) as usize;
    let budget = sink
        .store
        .settings()
        .await?
        .limits
        .result_memory_bytes_per_client;
    let (meta, rows, more) = fetch_page(s, &recipe, start, size, budget, cancel).await?;
    let previous = sink.store.result_meta(id).await?.summary;
    let summary = json!({"retrieval_mode":"live_page","has_more":more,"page_start":start,"total_rows":previous.get("total_rows"),"counted_at":previous.get("counted_at")});
    sink.store
        .replace_result_page(id, start, meta, rows, summary)
        .await?;
    Ok(serde_json::to_value(
        sink.store.result_page(id, start, size, max_bytes).await?,
    )?)
}

pub(super) async fn count(
    s: &mut MysqlSession,
    params: &Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    let id: Uuid = param(params, "result_id")?;
    let recipe = s
        .recipes
        .get(&id)
        .cloned()
        .ok_or_else(|| AppError::new("not_found", "结果不属于当前会话或已过期"))?;
    checked_context(s, &recipe, cancel).await?;
    if let Some((objects, context)) = &recipe.proof
        && !dependencies_replayable(s, objects.clone(), context, recipe.independent, cancel).await?
    {
        return Err(AppError::new("conflict", "查询依赖已变化，请重新执行"));
    }
    let meta = sink.store.result_meta(id).await?;
    let sql = format!(
        "SELECT COUNT(*) AS n FROM (\n{}\n) AS `_datapier_count`",
        recipe.sql.trim().trim_end_matches(';')
    );
    let args = recipe.arguments()?;
    let row = if recipe.independent {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(AppError::cancelled()),
            result = async {
                let mut conn = s.new_connection().await?;
                sqlx::query_with(sqlx::AssertSqlSafe(sql.as_str()), args).fetch_one(&mut conn).await.map_err(errors::mysql_error)
            } => result?,
        }
    } else {
        let options = s.options.clone();
        let timeout = s.connect_timeout;
        let thread = s.thread_id;
        let future = async {
            sqlx::query_with(sqlx::AssertSqlSafe(sql.as_str()), args)
                .fetch_one(s.connection()?)
                .await
                .map_err(errors::mysql_error)
        };
        let (result, safe) = cancellable(&options, timeout, thread, cancel, future).await;
        if !safe {
            s.invalidate_connection();
        }
        result?
    };
    let total: i64 = row.try_get("n")?;
    let counted_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut summary = meta.summary;
    summary["total_rows"] = json!(total);
    summary["counted_at"] = json!(counted_at);
    sink.store.update_result_summary(id, summary).await?;
    Ok(json!({"total_rows":total,"counted_at":counted_at}))
}
