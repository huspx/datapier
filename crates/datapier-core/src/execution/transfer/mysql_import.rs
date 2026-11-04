use super::*;

pub(super) async fn import_mysql_csv(
    session: &mut ProviderSession,
    params: &Json,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    file: File,
) -> Result<()> {
    let mut reader = csv::Reader::from_reader(BufReader::new(file));
    let names: Vec<String> = reader
        .headers()
        .map_err(csv_error)?
        .iter()
        .map(str::to_owned)
        .collect();
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
    for record in reader.records() {
        ensure_not_cancelled(cancel)?;
        let record = record.map_err(csv_error)?;
        let row = projection.csv_row(&record,&names,null,escaped,&binary).map_err(|e| { p.failed += 1; e.with_details(json!({"record":p.rows + batch.len() as u64 + 1,"buffered_not_executed":batch.len()})) })?;
        batch.push(row);
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
    file: File,
) -> Result<()> {
    let doc = json_document(file, &sink.store.root, "rows", cancel)?;
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
    let mut batch = Vec::with_capacity(PAGE);
    while r.array_next(&mut first)? {
        ensure_not_cancelled(cancel)?;
        let row: Vec<Value> = r.value()?;
        batch.push(projection.project(row)?);
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
