use super::mysql_import::{BoundedRecordReader, import_limit_error};
use super::*;

pub(super) async fn import_redis_csv(
    session: &mut ProviderSession,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    file: File,
    budget: usize,
) -> Result<()> {
    let limit = (budget / 64).max(1);
    let input = BoundedRecordReader {
        file: BufReader::new(file),
        used: 0,
        limit,
    };
    let mut reader = csv::ReaderBuilder::new()
        .buffer_capacity(1)
        .from_reader(input);
    let header_matches = match reader.headers() {
        Ok(header) => header.iter().eq(REDIS_HEADER),
        Err(error) => {
            return Err(if reader.get_ref().used >= limit {
                import_limit_error()
            } else {
                csv_error(error)
            });
        }
    };
    reader.get_mut().used = 0;
    if !header_matches {
        return Err(AppError::invalid("Redis CSV 表头不匹配"));
    }
    let mut active: Option<KeyImport> = None;
    let result: Result<()> = async {
        let mut record = csv::StringRecord::new();
        loop {
            ensure_not_cancelled(cancel)?;
            let has_record = reader.read_record(&mut record).map_err(|e| {
                if reader.get_ref().used >= limit {
                    import_limit_error()
                } else {
                    csv_error(e)
                }
            })?;
            if !has_record {
                break;
            }
            let charge = reader.get_ref().used.saturating_mul(32).saturating_add(512);
            reader.get_mut().used = 0;
            if record.len() != REDIS_HEADER.len() {
                return Err(AppError::invalid("Redis CSV 列数不匹配"));
            }
            match &record[1] {
                "key" => {
                    if &record[0] != "2" {
                        return Err(AppError::invalid("不支持的 Redis CSV 版本"));
                    }
                    if let Some(key) = active.as_mut() {
                        finish_key(session, key, sink, cancel, p).await?;
                    }
                    active = None;
                    let expiry = if &record[4] == "permanent" {
                        Json::Null
                    } else {
                        Json::String(record[4].to_owned())
                    };
                    active = Some(
                        begin_key(
                            session,
                            target,
                            RedisKeyHeader {
                                key: record[2].to_owned(),
                                kind: record[3].to_owned(),
                                expiry,
                            },
                            sink,
                            cancel,
                            p,
                        )
                        .await?,
                    );
                }
                "item" => {
                    let key = active
                        .as_mut()
                        .ok_or_else(|| AppError::invalid("Redis CSV 元素缺少 key 记录"))?;
                    let row = csv_item(&record, key)?;
                    push_key(session, key, row, charge, budget, sink, cancel).await?;
                }
                _ => return Err(AppError::invalid("未知 Redis CSV 记录类型")),
            }
        }
        if let Some(key) = active.as_mut() {
            finish_key(session, key, sink, cancel, p).await?;
        }
        active = None;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            p.failed += 1;
            Err(cleanup_key(session, &active, sink, e).await)
        }
    }
}
pub(super) async fn import_redis_json(
    session: &mut ProviderSession,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    file: File,
    budget: usize,
) -> Result<()> {
    let limit = (budget / 64).max(1);
    let doc = super::json::json_document_bounded(file, &sink.store.root, "records", cancel, limit)?;
    let mut r = doc.data.reader_bounded(limit)?;
    r.expect(b'[')?;
    let mut active: Option<KeyImport> = None;
    let result: Result<()> = async {
        let mut first = true;
        while r.array_next(&mut first)? {
            ensure_not_cancelled(cancel)?;
            r.expect(b'{')?;
            let mut fields_first = true;
            let mut key: Option<String> = None;
            let mut kind: Option<String> = None;
            let mut expiry: Option<Json> = None;
            let mut rows: Option<JsonSpool> = None;
            while let Some(field) = r.object_next(&mut fields_first)? {
                ensure_not_cancelled(cancel)?;
                match field.as_str() {
                    "key_b64" if key.is_none() => key = Some(r.value()?),
                    "type" if kind.is_none() => kind = Some(r.value()?),
                    "expires_at_ms" if expiry.is_none() => expiry = Some(r.value()?),
                    "rows" if rows.is_none() => {
                        rows = Some(JsonSpool::copy(&mut r, &sink.store.root, cancel)?)
                    }
                    "key_b64" | "type" | "expires_at_ms" | "rows" => {
                        return Err(AppError::invalid("Redis JSON 字段重复"));
                    }
                    _ => r.copy_value(&mut std::io::sink(), cancel)?,
                }
            }
            let key = key.ok_or_else(|| AppError::invalid("Redis JSON 缺少 key_b64"))?;
            let kind = kind.ok_or_else(|| AppError::invalid("Redis JSON 缺少 type"))?;
            let expiry =
                expiry.ok_or_else(|| AppError::invalid("Redis JSON 缺少 expires_at_ms"))?;
            let rows = rows.ok_or_else(|| AppError::invalid("Redis JSON 缺少 rows"))?;
            let mut row_reader = rows.reader_bounded(limit)?;
            row_reader.expect(b'[')?;
            active = Some(
                begin_key(
                    session,
                    target,
                    RedisKeyHeader { key, kind, expiry },
                    sink,
                    cancel,
                    p,
                )
                .await?,
            );
            let mut row_first = true;
            while row_reader.array_next(&mut row_first)? {
                ensure_not_cancelled(cancel)?;
                let (row, bytes): (Vec<Value>, usize) = row_reader.value_bounded(limit)?;
                let charge = bytes.saturating_mul(32).saturating_add(512);
                push_key(
                    session,
                    active.as_mut().unwrap(),
                    row,
                    charge,
                    budget,
                    sink,
                    cancel,
                )
                .await?;
            }
            row_reader.end_document()?;
            finish_key(session, active.as_mut().unwrap(), sink, cancel, p).await?;
            active = None;
        }
        r.end_document()
    }
    .await;
    match result {
        Ok(()) => Ok(()),
        Err(e) => {
            p.failed += 1;
            Err(cleanup_key(session, &active, sink, e).await)
        }
    }
}
