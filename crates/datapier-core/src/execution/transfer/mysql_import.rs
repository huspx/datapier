use super::*;
// 逐字节向 CSV parser 供给数据，避免带引号记录在返回完整行前突破分配预算。
struct BoundedRecordReader {
    file: BufReader<File>,
    used: usize,
    limit: usize,
}
impl Read for BoundedRecordReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.used >= self.limit {
            return Err(std::io::Error::other("record exceeds memory budget"));
        }
        let n = self.file.read(&mut buf[..1])?;
        self.used += n;
        Ok(n)
    }
}
fn import_limit_error() -> AppError {
    AppError::new("resource_limit", "导入行超过内存预算")
}

pub(super) struct MysqlImportInput {
    pub(super) file: File,
    pub(super) budget: usize,
}

pub(super) async fn import_mysql_csv(
    session: &mut ProviderSession,
    params: &Json,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    input: MysqlImportInput,
) -> Result<()> {
    let MysqlImportInput { file, budget } = input;
    let limit = (budget / 64).max(1);
    let input = BoundedRecordReader {
        file: BufReader::new(file),
        used: 0,
        limit,
    };
    let mut reader = csv::ReaderBuilder::new()
        .buffer_capacity(1)
        .from_reader(input);
    let names: Vec<String> = match reader
        .headers()
        .map(|header| header.iter().map(str::to_owned).collect())
    {
        Ok(names) => names,
        Err(_error) if reader.get_ref().used >= limit => return Err(import_limit_error()),
        Err(error) => return Err(csv_error(error)),
    };
    reader.get_mut().used = 0;
    let projection = mapped_columns(
        &names,
        params,
        &schema(session, target, sink, cancel).await?,
    )?;
    let null = params
        .get("null_marker")
        .and_then(Json::as_str)
        .unwrap_or("\\N");
    let escaped = params
        .get("backslash_escape")
        .and_then(Json::as_bool)
        .unwrap_or(true);
    let binary: HashSet<&str> = params
        .get("binary_columns")
        .and_then(Json::as_array)
        .map(|a| a.iter().filter_map(Json::as_str).collect())
        .unwrap_or_default();
    let mut batch = Vec::with_capacity(PAGE);
    let mut batch_bytes = 0usize;
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
        let record_bytes = reader.get_ref().used;
        reader.get_mut().used = 0;
        if !batch.is_empty()
            && batch_bytes.saturating_add(record_bytes.saturating_mul(32)) > budget / 2
        {
            insert_batch(
                session,
                target,
                &projection.columns,
                &mut batch,
                sink,
                cancel,
                p,
            )
            .await?;
            batch_bytes = 0;
        }
        let row = projection.csv_row(&record,&names,null,escaped,&binary).map_err(|e| { p.failed += 1; e.with_details(json!({"record":p.rows + batch.len() as u64 + 1,"buffered_not_executed":batch.len()})) })?;
        batch.push(row);
        batch_bytes = batch_bytes.saturating_add(record_bytes.saturating_mul(32));
        if batch.len() == PAGE {
            insert_batch(
                session,
                target,
                &projection.columns,
                &mut batch,
                sink,
                cancel,
                p,
            )
            .await?;
            batch_bytes = 0;
        }
    }
    insert_batch(
        session,
        target,
        &projection.columns,
        &mut batch,
        sink,
        cancel,
        p,
    )
    .await
}

pub(super) async fn import_mysql_json(
    session: &mut ProviderSession,
    params: &Json,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    input: MysqlImportInput,
) -> Result<()> {
    let MysqlImportInput { file, budget } = input;
    let doc = super::json::json_document_bounded(
        file,
        &sink.store.root,
        "rows",
        cancel,
        (budget / 64).max(1),
    )?;
    let source = doc
        .columns
        .ok_or_else(|| AppError::invalid("JSON 缺少列定义"))?;
    let mut r = doc.data.reader()?;
    r.expect(b'[')?;
    let names: Vec<String> = source.iter().map(|c| c.name.clone()).collect();
    let projection = mapped_columns(
        &names,
        params,
        &schema(session, target, sink, cancel).await?,
    )?;
    let mut first = true;
    let limit = (budget / 64).max(1);
    let mut batch_bytes = 0usize;
    let mut batch = Vec::with_capacity(PAGE);
    while r.array_next(&mut first)? {
        ensure_not_cancelled(cancel)?;
        let (row, bytes): (Vec<Value>, usize) = r.value_bounded(limit)?;
        let charge = bytes.saturating_mul(32);
        if !batch.is_empty() && batch_bytes.saturating_add(charge) > budget / 2 {
            insert_batch(
                session,
                target,
                &projection.columns,
                &mut batch,
                sink,
                cancel,
                p,
            )
            .await?;
            batch_bytes = 0;
        }
        batch.push(projection.project(row)?);
        batch_bytes = batch_bytes.saturating_add(charge);
        if batch.len() == PAGE {
            insert_batch(
                session,
                target,
                &projection.columns,
                &mut batch,
                sink,
                cancel,
                p,
            )
            .await?;
            batch_bytes = 0;
        }
    }
    r.end_document()?;
    insert_batch(
        session,
        target,
        &projection.columns,
        &mut batch,
        sink,
        cancel,
        p,
    )
    .await
}
