use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::TryStreamExt;
use serde_json::{Value as Json, json};
use sqlx::mysql::{MySqlArguments, MySqlConnection, MySqlRow};
use sqlx::{Column as _, Executor, Row, SqlSafeStr, Statement, TypeInfo, ValueRef};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{errors, sql};
use crate::{ExecutionSink, model::*};

fn columns(cols: &[sqlx::mysql::MySqlColumn]) -> Vec<Column> {
    cols.iter()
        .map(|c| Column::new(c.name(), c.type_info().name()))
        .collect()
}

// 按原生类型解码：大整数和 DECIMAL 用字符串保留精度，二进制用 Base64
// 可逆传递；未知类型显式报错，不能退回可能有损的显示文本。
fn decode_row(row: &MySqlRow) -> Result<Vec<Value>> {
    row.columns()
        .iter()
        .enumerate()
        .map(|(i, col)| {
            if row.try_get_raw(i)?.is_null() {
                return Ok(Value::Null);
            }
            let ty = col.type_info().name();
            let value = match ty {
                "BOOLEAN" | "TINYINT" => {
                    Value::Integer(row.try_get_unchecked::<i8, _>(i)?.to_string())
                }
                "SMALLINT" => Value::Integer(row.try_get_unchecked::<i16, _>(i)?.to_string()),
                "INT" | "MEDIUMINT" => {
                    Value::Integer(row.try_get_unchecked::<i32, _>(i)?.to_string())
                }
                "BIGINT" => Value::Integer(row.try_get_unchecked::<i64, _>(i)?.to_string()),
                "TINYINT UNSIGNED" => {
                    Value::Unsigned(row.try_get_unchecked::<u8, _>(i)?.to_string())
                }
                "SMALLINT UNSIGNED" | "YEAR" => {
                    Value::Unsigned(row.try_get_unchecked::<u16, _>(i)?.to_string())
                }
                "INT UNSIGNED" | "MEDIUMINT UNSIGNED" => {
                    Value::Unsigned(row.try_get_unchecked::<u32, _>(i)?.to_string())
                }
                "BIGINT UNSIGNED" => {
                    Value::Unsigned(row.try_get_unchecked::<u64, _>(i)?.to_string())
                }
                "DECIMAL" => Value::Decimal(
                    row.try_get_unchecked::<bigdecimal::BigDecimal, _>(i)?
                        .to_string(),
                ),
                "FLOAT" => Value::Float(row.try_get_unchecked::<f32, _>(i)?.to_string()),
                "DOUBLE" => Value::Float(row.try_get_unchecked::<f64, _>(i)?.to_string()),
                "DATE" => Value::Date(
                    row.try_get_unchecked::<chrono::NaiveDate, _>(i)?
                        .to_string(),
                ),
                "DATETIME" | "TIMESTAMP" => Value::DateTime(
                    row.try_get_unchecked::<chrono::NaiveDateTime, _>(i)?
                        .to_string(),
                ),
                "TIME" => Value::Time(
                    row.try_get_unchecked::<sqlx::mysql::types::MySqlTime, _>(i)?
                        .to_string(),
                ),
                "JSON" => Value::Json(row.try_get_unchecked::<Json, _>(i)?),
                "BINARY" | "VARBINARY" | "TINYBLOB" | "BLOB" | "MEDIUMBLOB" | "LONGBLOB"
                | "BIT" | "GEOMETRY" => {
                    Value::Bytes(STANDARD.encode(row.try_get_unchecked::<Vec<u8>, _>(i)?))
                }
                "CHAR" | "VARCHAR" | "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM"
                | "SET" => Value::Text(row.try_get_unchecked::<String, _>(i)?),
                _ => {
                    return Err(AppError::new(
                        "unsupported_type",
                        format!("不支持无损读取 MySQL 类型 {ty}"),
                    ));
                }
            };
            Ok(value)
        })
        .collect()
}

pub(super) async fn stream_query(
    conn: &mut MySqlConnection,
    sql: &str,
    args: MySqlArguments,
    source_columns: Vec<Column>,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    ensure_not_cancelled(cancel)?;
    // 先 prepare 取得列定义，保证零行结果也能建立带列信息的结果集。
    let stmt = conn
        .prepare(sqlx::AssertSqlSafe(sql).into_sql_str())
        .await?;
    let mut metadata = columns(stmt.columns());
    // schema 与 prepare 之间可能发生 DDL；不能把注释错配到新增或重排后的列。
    if metadata.len() != source_columns.len() {
        return Err(AppError::new("conflict", "表结构已变化，请重新浏览"));
    }
    for (column, source) in metadata.iter_mut().zip(source_columns) {
        if column.name != source.name {
            return Err(AppError::new("conflict", "表结构已变化，请重新浏览"));
        }
        column.comment = source.comment;
    }
    let id = sink.start(metadata, "rows").await?;
    let mut stream = stmt.query_with(args).fetch(&mut *conn);
    let mut rows = Vec::with_capacity(128);
    let mut count = 0u64;
    loop {
        let row = tokio::select! { row = stream.try_next() => row?, _ = cancel.cancelled() => return Err(AppError::cancelled()) };
        let Some(row) = row else { break };
        rows.push(decode_row(&row)?);
        count += 1;
        if rows.len() == 128 {
            sink.append(id, std::mem::take(&mut rows)).await?;
        }
    }
    if !rows.is_empty() {
        sink.append(id, rows).await?;
    }
    sink.finish(id, json!({"row_count":count})).await?;
    Ok(json!({"results":[id],"row_count":count}))
}

pub(super) async fn execute_statement(
    conn: &mut MySqlConnection,
    text: &str,
    sink: &ExecutionSink,
) -> Result<(Vec<Uuid>, u64)> {
    // 普通语句的 prepare 元数据用于保留空结果的列；prepare 失败时仍执行。
    // CALL／EXECUTE 的结果由运行时决定，SQLx 不暴露空动态结果元数据，
    // 且边界事件也可能是命令 OK 包，不能据此伪造列或断言存在结果集。
    let dynamic = sql::is_dynamic(text);
    // 动态语句可能在结果集后继续写入；只有读完整个流才知道服务端已结束。
    let local_error = |mut error: AppError| {
        error.details["result_stream_incomplete"] = json!(true);
        if dynamic {
            AppError::unknown(format!(
                "动态语句结果未读完，写入结果无法确认: {}",
                error.message
            ))
            .with_details(json!({"local_error": error, "result_stream_incomplete": true}))
        } else {
            error
        }
    };
    let initial = conn
        .prepare(sqlx::AssertSqlSafe(text).into_sql_str())
        .await
        .ok()
        .map(|stmt| columns(stmt.columns()));
    let mut stream = sqlx::raw_sql(sqlx::AssertSqlSafe(text.to_owned())).fetch_many(&mut *conn);
    let mut results = Vec::new();
    let mut id = None;
    let mut pending = if dynamic { None } else { initial };
    let mut rows = Vec::with_capacity(128);
    let mut count = 0u64;
    let mut affected = 0u64;
    loop {
        let item = match stream.try_next().await {
            Ok(Some(item)) => item,
            Ok(None) => break,
            Err(error) => {
                let mut error = errors::mysql_error(error);
                let mut details = match error.details.as_ref() {
                    Json::Object(map) => map.clone(),
                    _ => serde_json::Map::new(),
                };
                details.insert("current_affected_rows".into(), json!(affected));
                details.insert("current_results".into(), json!(results));
                error.details = Box::new(Json::Object(details));
                return Err(error);
            }
        };
        match item {
            sqlx::Either::Right(row) => {
                if id.is_none() {
                    let meta = pending
                        .take()
                        .filter(|m| !m.is_empty())
                        .unwrap_or_else(|| columns(row.columns()));
                    let result = sink.start(meta, "rows").await.map_err(local_error)?;
                    results.push(result);
                    id = Some(result);
                }
                rows.push(decode_row(&row).map_err(local_error)?);
                count += 1;
                if rows.len() == 128 {
                    sink.append(id.unwrap(), std::mem::take(&mut rows))
                        .await
                        .map_err(local_error)?;
                }
            }
            sqlx::Either::Left(done) => {
                affected = affected.saturating_add(done.rows_affected());
                if id.is_none() && pending.as_ref().is_some_and(|c| !c.is_empty()) {
                    let result = sink
                        .start(pending.take().unwrap(), "rows")
                        .await
                        .map_err(local_error)?;
                    results.push(result);
                    id = Some(result);
                }
                if id.is_none() && dynamic {
                    let result = sink
                        .start(Vec::new(), "columns_unknown")
                        .await
                        .map_err(local_error)?;
                    results.push(result);
                    sink.finish(result, json!({"columns_known":false,"result_set_presence":"unknown","reason":"driver_does_not_expose_empty_dynamic_result_metadata"})).await.map_err(local_error)?;
                }
                if let Some(result) = id.take() {
                    if !rows.is_empty() {
                        sink.append(result, std::mem::take(&mut rows))
                            .await
                            .map_err(local_error)?;
                    }
                    sink.finish(result, json!({"row_count":count}))
                        .await
                        .map_err(local_error)?;
                    count = 0;
                }
                pending = None;
            }
        }
    }
    if let Some(result) = id {
        if !rows.is_empty() {
            sink.append(result, rows).await?;
        }
        sink.finish(result, json!({"row_count":count})).await?;
    }
    Ok((results, affected))
}
