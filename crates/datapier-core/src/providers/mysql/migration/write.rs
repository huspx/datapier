//! MySQL 迁移、文件导入和 SQL 恢复共用的目标批次写入。

use super::super::{MysqlSession, changes, quote_ident};
use super::{MigrationTable, MigrationWriteOutcome, add_arg, fingerprint, qualified, row_bytes};
use crate::{
    ExecutionSink,
    model::{
        AppError, Column, Result, Value,
        transfer::{
            ConflictPolicy, MAX_BATCH_BYTES, MAX_BATCH_ROWS, TransferObject, TransferStepState,
        },
    },
};
use serde_json::json;
use sqlx::{Arguments, Row, mysql::MySqlArguments};
use tokio_util::sync::CancellationToken;

pub(super) fn mapped_columns<'a>(
    table: &'a MigrationTable,
    object: &TransferObject,
) -> Result<Vec<&'a Column>> {
    let names: Vec<&str> = if object.columns.is_empty() {
        table
            .schema
            .columns
            .iter()
            .filter(|c| !c.generated)
            .map(|c| c.name.as_str())
            .collect()
    } else {
        object.columns.iter().map(|m| m.target.as_str()).collect()
    };
    let mut seen = std::collections::HashSet::new();
    names
        .into_iter()
        .map(|name| {
            if !seen.insert(name) {
                return Err(AppError::invalid("目标列映射重复"));
            }
            let column = table
                .schema
                .columns
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| AppError::invalid(format!("目标列不存在: {name}")))?;
            if column.generated {
                return Err(AppError::invalid(format!("生成列不可写: {name}")));
            }
            Ok(column)
        })
        .collect()
}
fn bind_row(
    args: &mut MySqlArguments,
    columns: &[&Column],
    row: &[Value],
    indices: &[usize],
) -> Result<()> {
    for &i in indices {
        let v = &row[i];
        if matches!(v, Value::Null) {
            args.add(Option::<String>::None)
                .map_err(|e| AppError::invalid(e.to_string()))?;
        } else if columns[i]
            .type_name
            .to_ascii_lowercase()
            .starts_with("year")
        {
            let n = match v {
                Value::Integer(s) | Value::Unsigned(s) => s
                    .parse::<u16>()
                    .map_err(|_| AppError::invalid("年份无效"))?,
                _ => return Err(AppError::invalid("年份类型无效")),
            };
            args.add(n).map_err(|e| AppError::invalid(e.to_string()))?;
        } else {
            add_arg(args, v)?;
        }
    }
    Ok(())
}
pub(in crate::providers::mysql) enum MigrationBatchSource<'a> {
    Transfer(&'a CancellationToken),
    SqlRestore(&'a [String]),
}

impl MysqlSession {
    /// 写入一个有界批次。调用方在调用前后登记批次日志；每次发送由 sink 保护。
    pub async fn write_batch(
        &self,
        table: &MigrationTable,
        object: &TransferObject,
        policy: ConflictPolicy,
        rows: Vec<Vec<Value>>,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<MigrationWriteOutcome> {
        self.write_batch_with_context(
            table,
            object,
            policy,
            rows,
            sink,
            MigrationBatchSource::Transfer(cancel),
        )
        .await
    }

    pub(in crate::providers::mysql) async fn write_batch_with_context(
        &self,
        table: &MigrationTable,
        object: &TransferObject,
        policy: ConflictPolicy,
        rows: Vec<Vec<Value>>,
        sink: &ExecutionSink,
        source: MigrationBatchSource<'_>,
    ) -> Result<MigrationWriteOutcome> {
        let (settings, cancel) = match source {
            MigrationBatchSource::Transfer(cancel) => (&[][..], Some(cancel)),
            MigrationBatchSource::SqlRestore(settings) => (settings, None),
        };
        self.profile.ensure_write()?;
        if !table.schema.engine.eq_ignore_ascii_case("innodb") && rows.len() > 1 {
            return Err(AppError::invalid("非事务表必须逐行登记和写入"));
        }
        if rows.len() > MAX_BATCH_ROWS {
            return Err(AppError::invalid("批次超过 500 行"));
        }
        let columns = mapped_columns(table, object)?;
        if columns.is_empty() {
            return Err(AppError::invalid("目标列为空"));
        }
        let mut bytes = 0usize;
        for row in &rows {
            if row.len() != columns.len() {
                return Err(AppError::invalid("数据列数与映射不一致"));
            }
            bytes = bytes.saturating_add(row_bytes(row)?);
            for (v, c) in row.iter().zip(&columns) {
                changes::validate_import_value(v, c)?;
            }
        }
        if rows.len() > 1 && bytes > MAX_BATCH_BYTES {
            return Err(AppError::invalid("批次超过 1 MiB"));
        }
        let key: Vec<usize> = if policy == ConflictPolicy::Insert {
            Vec::new()
        } else {
            if !table.schema.engine.eq_ignore_ascii_case("innodb") {
                return Err(AppError::new(
                    "unsupported_engine",
                    "跳过和覆盖需要 InnoDB 行锁",
                ));
            }
            if object.locator_key.is_empty()
                || !table.schema.keys.iter().any(|k| k == &object.locator_key)
            {
                return Err(AppError::invalid("必须显式选择完整可靠唯一键"));
            }
            object
                .locator_key
                .iter()
                .map(|name| {
                    columns
                        .iter()
                        .position(|c| &c.name == name)
                        .ok_or_else(|| AppError::invalid(format!("定位键未映射: {name}")))
                })
                .collect::<Result<Vec<_>>>()?
        };
        let mut conn = self.new_connection().await?;
        sqlx::raw_sql("SET SESSION time_zone = '+00:00'")
            .execute(&mut conn)
            .await?;
        // 恢复字面量按 dump 的会话时区解释；直接复制仍使用 UTC。
        for setting in settings {
            sqlx::raw_sql(sqlx::AssertSqlSafe(setting.as_str()))
                .execute(&mut conn)
                .await?;
        }
        if fingerprint(&mut conn, &table.schema.target).await? != table.fingerprint {
            return Err(AppError::conflict("目标表结构已变化"));
        }
        // 保留显式源值 0；由目标生成自增值时已省略该列，不能借 MySQL 的零值规则悄悄换键。
        sqlx::raw_sql("SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, IF(@@SESSION.sql_mode = '', '', ','), 'STRICT_ALL_TABLES,NO_AUTO_VALUE_ON_ZERO')").execute(&mut conn).await?;
        let transactional = table.schema.engine.eq_ignore_ascii_case("innodb");
        if transactional {
            sqlx::raw_sql("START TRANSACTION")
                .execute(&mut conn)
                .await?;
        }
        let target = qualified(&table.schema.target)?;
        let names = columns
            .iter()
            .map(|c| quote_ident(&c.name))
            .collect::<Result<Vec<_>>>()?;
        let all: Vec<usize> = (0..columns.len()).collect();
        let mut states = vec![TransferStepState::NotSent; rows.len()];
        let mut marked_sent = false;
        let check_cancelled = || {
            sink.check_cancelled()?;
            cancel.map_or(Ok(()), crate::model::ensure_not_cancelled)
        };
        // 最后一轮只检查提交边界；任何取消都沿相同的回滚确认路径处理。
        for index in 0..=rows.len() {
            let mut dispatched = false;
            let result: Result<Option<bool>> = async {
                check_cancelled()?;
                let Some(row) = rows.get(index) else {
                    return Ok(None);
                };
                let exists = if key.is_empty() {
                    false
                } else {
                    if key.iter().any(|i| matches!(row[*i], Value::Null)) {
                        return Err(AppError::invalid("定位键不能为 NULL"));
                    }
                    let predicates = key
                        .iter()
                        .map(|i| format!("{} = ?", names[*i]))
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    let mut args = MySqlArguments::default();
                    bind_row(&mut args, &columns, row, &key)?;
                    let sql = format!("SELECT 1 FROM {target} WHERE {predicates} FOR UPDATE");
                    sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                        .fetch_optional(&mut conn)
                        .await?
                        .is_some()
                };
                if exists && policy == ConflictPolicy::Skip {
                    return Ok(Some(false));
                }
                let mut args = MySqlArguments::default();
                let sql = if exists {
                    let update: Vec<usize> =
                        all.iter().copied().filter(|i| !key.contains(i)).collect();
                    if update.is_empty() {
                        return Ok(Some(false));
                    }
                    bind_row(&mut args, &columns, row, &update)?;
                    bind_row(&mut args, &columns, row, &key)?;
                    format!(
                        "UPDATE {target} SET {} WHERE {}",
                        update
                            .iter()
                            .map(|i| format!("{} = ?", names[*i]))
                            .collect::<Vec<_>>()
                            .join(", "),
                        key.iter()
                            .map(|i| format!("{} = ?", names[*i]))
                            .collect::<Vec<_>>()
                            .join(" AND ")
                    )
                } else {
                    bind_row(&mut args, &columns, row, &all)?;
                    format!(
                        "INSERT INTO {target} ({}) VALUES ({})",
                        names.join(", "),
                        vec!["?"; names.len()].join(", ")
                    )
                };
                if !marked_sent {
                    sink.mark_sent(true).await?;
                    marked_sent = true;
                }
                // 上方的锁定读取或发送登记可能等待，真正派发前再次检查。
                check_cancelled()?;
                dispatched = true;
                sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                    .execute(&mut conn)
                    .await?;
                Ok(Some(true))
            }
            .await;
            match result {
                Ok(Some(true)) => states[index] = TransferStepState::Applied,
                Ok(Some(false)) => states[index] = TransferStepState::Skipped,
                Ok(None) => break,
                Err(mut error) => {
                    if error.code != "cancelled"
                        && let Some(state) = states.get_mut(index)
                    {
                        *state = TransferStepState::Failed;
                    }
                    if transactional {
                        let rollback = sqlx::raw_sql("ROLLBACK").execute(&mut conn).await;
                        let confirmed = if rollback.is_ok() {
                            sqlx::raw_sql("SHOW COUNT(*) WARNINGS")
                                .fetch_one(&mut conn)
                                .await
                                .ok()
                                .is_some_and(|r| {
                                    r.try_get::<u64, _>(0).is_ok_and(|v| v == 0)
                                        || r.try_get::<i64, _>(0).is_ok_and(|v| v == 0)
                                })
                        } else {
                            false
                        };
                        for state in &mut states[..index] {
                            if *state == TransferStepState::Applied {
                                *state = if confirmed {
                                    TransferStepState::RolledBack
                                } else {
                                    TransferStepState::OutcomeUnknown
                                };
                            }
                        }
                        if !confirmed {
                            if error.code != "cancelled"
                                && let Some(state) = states.get_mut(index)
                            {
                                *state = TransferStepState::OutcomeUnknown;
                            }
                            error = AppError::unknown(format!("回滚结果无法确认: {error}"));
                        }
                    } else if dispatched {
                        states[index] = TransferStepState::OutcomeUnknown;
                        error.code = "outcome_unknown".into();
                        error.message = format!("写入结果无法确认：{}", error.message);
                    }
                    if !error.details.is_object() && !error.details.is_null() {
                        let cause = std::mem::take(error.details.as_mut());
                        error.details = Box::new(json!({"cause":cause}));
                    }
                    error.details["index"] = json!(index);
                    error.details["states"] = json!(states);
                    return Err(error);
                }
            }
        }
        if transactional && let Err(e) = sqlx::raw_sql("COMMIT").execute(&mut conn).await {
            for state in &mut states {
                if *state == TransferStepState::Applied {
                    *state = TransferStepState::OutcomeUnknown;
                }
            }
            return Err(AppError::unknown(format!("事务提交结果无法确认: {e}"))
                .with_details(json!({"states":states})));
        }
        let affected = states
            .iter()
            .filter(|s| **s == TransferStepState::Applied)
            .count() as u64;
        Ok(MigrationWriteOutcome { states, affected })
    }
}
