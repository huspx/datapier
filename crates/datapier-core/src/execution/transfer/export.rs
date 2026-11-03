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
    let result = if params.get("result_id").is_some_and(|v| !v.is_null()) {
        let id: Uuid = param(params, "result_id")?;
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
    finish_export(result, &tmp, &dst, cancel, progress, fmt)
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
    let t: ResourceTarget = param(params, "target")?;
    let id = browse(session, &t, sink, cancel).await?;
    export_cached(id, sink, cancel, p, fmt, file, true).await
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
    if !meta.complete {
        return Err(AppError::new(
            "incomplete_result",
            "结果尚未完整，不能作为全量导出",
        ));
    }
    let mut writer = BufWriter::new(file);
    if fmt == "json" {
        if archery {
            write!(
                writer,
                "{{\"format\":\"datapier-query-result\",\"version\":1,\"columns\":{},\"summary\":{},\"rows\":[",
                serde_json::to_string(&meta.columns)?,
                serde_json::to_string(&meta.summary)?
            )?;
        } else {
            write!(
                writer,
                "{{\"format\":\"datapier\",\"version\":1,\"columns\":{},\"rows\":[",
                serde_json::to_string(&meta.columns)?
            )?;
        }
    }
    let mut csv = if fmt == "csv" {
        let mut c = csv::Writer::from_writer(Vec::new());
        c.write_record(meta.columns.iter().map(|c| c.name.as_str()))
            .map_err(csv_error)?;
        Some(c)
    } else {
        None
    };
    let mut start = 0;
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
