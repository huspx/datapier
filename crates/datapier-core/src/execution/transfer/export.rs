use super::*;

pub(super) async fn export(
    session: &mut ProviderSession,
    params: &Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    progress: &mut Progress,
) -> Result<Json> {
    if session.profile().is_archery() && params.get("result_id").is_none_or(Json::is_null) {
        return Err(AppError::new(
            "capability_unavailable",
            "Archery 只能导出已缓存的结果",
        ));
    }
    let fmt = format(params)?;
    let dst = path(params)?;
    match fs::symlink_metadata(&dst) {
        Ok(_) => return Err(AppError::conflict("导出目标文件已存在")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    let (tmp, file) = create_temp(&dst, progress)?;
    let retained = params.get("result_id").is_some_and(|v| !v.is_null());
    let mut coverage = Json::Null;
    let result = if retained
        && matches!(session, ProviderSession::Mysql(_))
        && sink
            .store
            .result_meta(param(params, "result_id")?)
            .await?
            .summary
            .get("retrieval_mode")
            .and_then(Json::as_str)
            == Some("live_page")
    {
        coverage = json!({"scope":"requery","snapshot":"may_differ"});
        export_mysql(session, params, sink, cancel, progress, fmt, file).await
    } else if retained {
        let id: Uuid = param(params, "result_id")?;
        let meta = sink.store.result_meta(id).await?;
        coverage = json!({"scope":"retained_rows","page_start":meta.summary.get("page_start").and_then(Json::as_u64).unwrap_or(0),"complete":retained_complete(meta.complete, &meta.summary),"source_coverage":meta.summary.get("coverage")});
        export_cached(id, sink, cancel, progress, fmt, file, false).await
    } else {
        match session.profile().provider {
            ProviderKind::Mysql => {
                export_mysql(session, params, sink, cancel, progress, fmt, file).await
            }
            ProviderKind::Redis => {
                export_redis(session, params, sink, cancel, progress, fmt, file).await
            }
        }
    };
    let mut output = finish_export(result, &tmp, &dst, cancel, progress, fmt)?;
    if retained {
        output["coverage"] = coverage;
    }
    Ok(output)
}
fn retained_complete(complete: bool, summary: &Json) -> bool {
    complete
        && !summary
            .get("resource_limited")
            .and_then(Json::as_bool)
            .unwrap_or(false)
        && !summary
            .get("retained_prefix")
            .and_then(Json::as_bool)
            .unwrap_or(false)
}

#[test]
fn retained_prefix_is_not_complete_coverage() {
    assert!(!retained_complete(true, &json!({"retained_prefix": true})));
    assert!(retained_complete(true, &json!({"retained_prefix": false})));
}
pub(super) fn finish_export(
    result: Result<()>,
    tmp: &Path,
    dst: &Path,
    cancel: &CancellationToken,
    progress: &mut Progress,
    fmt: &str,
) -> Result<Json> {
    result?;
    ensure_not_cancelled(cancel)?;
    publish_export(tmp, dst)?;
    progress.partial = None;
    Ok(json!({"path":dst,"format":fmt,"progress":progress.details()}))
}

async fn export_mysql(
    session: &mut ProviderSession,
    params: &Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    fmt: &str,
    file: File,
) -> Result<()> {
    ensure_not_cancelled(cancel)?;
    let mut request = params.clone();
    if request.get("result_id").is_none_or(Json::is_null) {
        let target: ResourceTarget = param(params, "target")?;
        let id = browse(session, &target, sink, cancel).await?;
        request["result_id"] = json!(id);
    }
    let ProviderSession::Mysql(mysql) = session else {
        return Err(AppError::invalid("需要 MySQL 直连"));
    };
    match mysql.export_query(&request, cancel).await {
        Ok((mut conn, sql, args)) => {
            write_mysql_stream(&mut conn, &sql, args, cancel, p, fmt, file)
                .await
                .0
        }
        Err(error) if error.code == "unsupported" => {
            let (conn, sql, args) = mysql.export_session_query(&request).await?;
            let (result, clean) = write_mysql_stream(conn, &sql, args, cancel, p, fmt, file).await;
            if !clean {
                mysql.invalidate_connection();
            }
            result
        }
        Err(error) => Err(error),
    }
}

async fn write_mysql_stream(
    conn: &mut sqlx::MySqlConnection,
    sql: &str,
    args: sqlx::mysql::MySqlArguments,
    cancel: &CancellationToken,
    p: &mut Progress,
    fmt: &str,
    file: File,
) -> (Result<()>, bool) {
    use futures_util::TryStreamExt;
    use sqlx::{Executor, SqlSafeStr, Statement};
    let statement = match conn.prepare(sqlx::AssertSqlSafe(sql).into_sql_str()).await {
        Ok(statement) => statement,
        Err(error) => return (Err(error.into()), false),
    };
    let columns = crate::providers::mysql::export_columns(statement.columns());
    let mut out = BufWriter::new(file);
    let init: Result<Option<csv::Writer<Vec<u8>>>> = (|| {
        if fmt == "csv" {
            let mut writer = csv::Writer::from_writer(Vec::new());
            writer
                .write_record(columns.iter().map(|column| column.name.as_str()))
                .map_err(csv_error)?;
            Ok(Some(writer))
        } else {
            write!(
                out,
                "{{\"format\":\"datapier\",\"version\":1,\"columns\":{},\"rows\":[",
                serde_json::to_string(&columns)?
            )?;
            Ok(None)
        }
    })();
    let mut csv = match init {
        Ok(csv) => csv,
        Err(error) => return (Err(error), true),
    };
    let mut first = true;
    let mut stream = statement.query_with(args).fetch(conn);
    let mut failure: Option<AppError> = None;
    let mut deadline = None;
    loop {
        let next = if let Some(until) = deadline {
            match tokio::time::timeout_at(until, stream.try_next()).await {
                Ok(value) => value,
                Err(_) => {
                    return (
                        Err(failure.unwrap_or_else(|| AppError::new("timeout", "导出收尾超时"))),
                        false,
                    );
                }
            }
        } else {
            tokio::select! {
                row = stream.try_next() => row,
                _ = cancel.cancelled() => {
                    failure = Some(AppError::cancelled());
                    deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
                    continue;
                }
            }
        };
        let row = match next {
            Ok(Some(row)) => row,
            Ok(None) => break,
            Err(error) => return (Err(failure.unwrap_or_else(|| error.into())), false),
        };
        if failure.is_some() {
            continue;
        }
        let written: Result<()> = (|| {
            let row = crate::providers::mysql::export_decode_row(&row)?;
            if let Some(writer) = csv.as_mut() {
                writer
                    .write_record(row.iter().map(csv_value).collect::<Result<Vec<_>>>()?)
                    .map_err(csv_error)?;
                flush_csv(writer, &mut out, false)?;
            } else {
                if !first {
                    out.write_all(b",")?;
                }
                serde_json::to_writer(&mut out, &row)?;
                first = false;
            }
            p.rows += 1;
            Ok(())
        })();
        if let Err(error) = written {
            failure = Some(error);
            deadline = Some(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
        }
    }
    if let Some(error) = failure {
        return (Err(error), true);
    }
    let finish: Result<()> = (|| {
        if let Some(writer) = csv.as_mut() {
            flush_csv(writer, &mut out, true)?;
        } else {
            out.write_all(b"]}")?;
        }
        out.flush()?;
        out.get_ref().sync_all()?;
        Ok(())
    })();
    (finish, true)
}

pub(super) trait ExportFile: Write {
    fn sync_all(&self) -> std::io::Result<()>;
}
impl ExportFile for File {
    fn sync_all(&self) -> std::io::Result<()> {
        File::sync_all(self)
    }
}

pub(super) async fn export_cached<W: ExportFile>(
    id: Uuid,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    fmt: &str,
    file: W,
    freshly_browsed_direct: bool,
) -> Result<()> {
    let meta = sink.store.result_meta(id).await?;
    if meta
        .summary
        .get("origin")
        .is_some_and(|value| !value.is_object())
    {
        return Err(AppError::invalid("结果来源不完整，无法导出"));
    }
    let origin = meta.summary.get("origin").and_then(Json::as_object);
    let source = origin.and_then(|o| o.get("source")).and_then(Json::as_str);
    let archery = match source {
        Some("archery") => true,
        Some("direct") => false,
        Some(_) => return Err(AppError::invalid("结果来源未知，无法导出")),
        None if origin.is_some() => return Err(AppError::invalid("结果来源不完整，无法导出")),
        None if freshly_browsed_direct => false,
        None => {
            let task = sink.store.task(meta.execution_id).await?;
            let context = task
                .context
                .ok_or_else(|| AppError::invalid("结果来源未记录，无法导出"))?;
            match context.source.as_deref() {
                Some("direct") => false,
                None if task
                    .summary
                    .pointer("/_migration/source")
                    .and_then(Json::as_str)
                    == Some("direct")
                    && task
                        .summary
                        .pointer("/_migration/config_schema")
                        .and_then(Json::as_u64)
                        == Some(1) =>
                {
                    false
                }
                _ => return Err(AppError::invalid("结果来源未确认，无法导出")),
            }
        }
    };
    let mut writer = BufWriter::new(file);
    if fmt == "json" {
        let format = if archery {
            "datapier-query-result"
        } else {
            "datapier"
        };
        write!(
            writer,
            "{{\"format\":{},\"version\":1,\"columns\":{},\"summary\":{},\"rows\":[",
            serde_json::to_string(format)?,
            serde_json::to_string(&meta.columns)?,
            serde_json::to_string(&meta.summary)?
        )?;
    }
    let mut csv = if fmt == "csv" {
        let mut c = csv::Writer::from_writer(Vec::new());
        c.write_record(meta.columns.iter().map(|c| c.name.as_str()))
            .map_err(csv_error)?;
        Some(c)
    } else {
        None
    };
    let mut start = meta
        .summary
        .get("page_start")
        .and_then(Json::as_u64)
        .unwrap_or(0);
    let mut first = true;
    loop {
        ensure_not_cancelled(cancel)?;
        let page = sink.store.result_page(id, start, PAGE, BYTES).await?;
        for row in page.rows {
            let row = full_row(sink, row).await?;
            if let Some(c) = csv.as_mut() {
                c.write_record(row.iter().map(csv_value).collect::<Result<Vec<_>>>()?)
                    .map_err(csv_error)?;
                flush_csv(c, &mut writer, false)?;
            } else {
                if !first {
                    writer.write_all(b",")?;
                }
                serde_json::to_writer(&mut writer, &row)?;
                first = false;
            }
            p.rows += 1;
        }
        match page.next {
            Some(next) if next > start => start = next,
            Some(_) => return Err(AppError::new("result_page", "结果分页没有推进")),
            None => break,
        }
    }
    if let Some(c) = csv.as_mut() {
        flush_csv(c, &mut writer, true)?;
    } else {
        writer.write_all(b"]}")?;
    }
    writer.flush()?;
    writer.get_ref().sync_all()?;
    Ok(())
}
pub(super) fn csv_error(e: csv::Error) -> AppError {
    AppError::new("csv", e.to_string())
}
pub(super) fn flush_csv(
    c: &mut csv::Writer<Vec<u8>>,
    out: &mut impl Write,
    force: bool,
) -> Result<()> {
    c.flush()?;
    if force || c.get_ref().len() >= 64 * 1024 {
        let old = std::mem::replace(c, csv::Writer::from_writer(Vec::new()));
        let mut data = old
            .into_inner()
            .map_err(|e| AppError::new("csv", e.to_string()))?;
        out.write_all(&data)?;
        data.clear();
        *c = csv::Writer::from_writer(data);
    }
    Ok(())
}
// NULL 使用独立标记；普通值的反斜线加倍，使字面量 `\N` 可以往返。
fn csv_value(v: &Value) -> Result<String> {
    let s = match v {
        Value::Null => return Ok("\\N".into()),
        Value::Missing | Value::Large { .. } => {
            return Err(AppError::invalid("无法导出缺失值或未展开的大字段"));
        }
        Value::Bool(b) => {
            if *b {
                "1".into()
            } else {
                "0".into()
            }
        }
        Value::Bytes(s)
        | Value::Text(s)
        | Value::Integer(s)
        | Value::Unsigned(s)
        | Value::Decimal(s)
        | Value::Float(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s) => s.clone(),
        Value::Json(v) => serde_json::to_string(v)?,
        Value::Array(_) | Value::Map(_) => return Err(AppError::invalid("CSV 不支持复合值")),
    };
    Ok(s.replace('\\', "\\\\"))
}
