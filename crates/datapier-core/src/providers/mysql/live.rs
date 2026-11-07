use futures_util::TryStreamExt;
use serde_json::{Value as Json, json};
use sqlx::{
    Executor, Row, SqlSafeStr, Statement,
    mysql::{MySqlArguments, MySqlConnection},
};
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

async fn page(
    conn: &mut MySqlConnection,
    recipe: &Recipe,
    start: u64,
    size: usize,
    budget: usize,
    cancel: &CancellationToken,
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
        let next = tokio::select! { item = stream.try_next() => item.map_err(errors::mysql_error)?, _ = cancel.cancelled() => return Err(AppError::cancelled()) };
        let Some(row) = next else { break };
        if more || rows.len() == size {
            more = true;
            continue;
        }
        let decoded = decode_row(&row)?;
        let bytes = serde_json::to_vec(&decoded)?.len();
        if retained_bytes.saturating_add(bytes) > budget {
            if rows.is_empty() {
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
    let (meta, rows, more) = if recipe.independent {
        let mut conn = s.new_connection().await?;
        page(&mut conn, &recipe, 0, size, budget, cancel).await?
    } else {
        match page(s.connection()?, &recipe, 0, size, budget, cancel).await {
            Ok(value) => value,
            Err(error) => {
                s.invalidate_connection();
                return Err(error);
            }
        }
    };
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
    validate_context(s, &recipe).await?;
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
    let (meta, rows, more) = if recipe.independent {
        let mut conn = s.new_connection().await?;
        page(&mut conn, &recipe, start, size, budget, cancel).await?
    } else {
        match page(s.connection()?, &recipe, start, size, budget, cancel).await {
            Ok(value) => value,
            Err(error) => {
                s.invalidate_connection();
                return Err(error);
            }
        }
    };
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
) -> Result<Json> {
    let id: Uuid = param(params, "result_id")?;
    let recipe = s
        .recipes
        .get(&id)
        .cloned()
        .ok_or_else(|| AppError::new("not_found", "结果不属于当前会话或已过期"))?;
    validate_context(s, &recipe).await?;
    let meta = sink.store.result_meta(id).await?;
    let sql = format!(
        "SELECT COUNT(*) AS n FROM (\n{}\n) AS `_datapier_count`",
        recipe.sql.trim().trim_end_matches(';')
    );
    let row = if recipe.independent {
        let mut conn = s.new_connection().await?;
        sqlx::query_with(sqlx::AssertSqlSafe(sql.as_str()), recipe.arguments()?)
            .fetch_one(&mut conn)
            .await
            .map_err(errors::mysql_error)?
    } else {
        sqlx::query_with(sqlx::AssertSqlSafe(sql.as_str()), recipe.arguments()?)
            .fetch_one(s.connection()?)
            .await
            .map_err(errors::mysql_error)?
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
