//! MySQL 文件导入导出：阻塞文件操作由有界 actor 执行。
use super::*;
use crate::{
    execution::data_files::{AtomicOutput, CsvJsonWriter, OutputSnapshot, RowEvent, RowReader},
    model::{Column, Value},
    providers::mysql::{MysqlSession, migration::MigrationTable},
};
use serde_json::json;
use std::path::{Path, PathBuf};
use tokio::sync::{mpsc, oneshot};

fn cancelled(cancel: &CancellationToken) -> Result<()> {
    crate::model::ensure_not_cancelled(cancel)
}
fn io_join(error: tokio::task::JoinError) -> AppError {
    AppError::new("io", error.to_string())
}

struct ImportRow {
    record: u64,
    offset: u64,
    values: Vec<Value>,
}
enum InputMessage {
    Columns(Vec<Column>),
    Row(ImportRow),
    Error(AppError),
}

// JSON 保存源类型；仅对明确指定的目标类型执行可验证的转换。
fn convert_json(value: Value, column: &Column) -> Result<Value> {
    if crate::providers::mysql::validate_import_value(&value, column).is_ok() {
        return Ok(value);
    }
    let base = column
        .type_name
        .split(|c: char| c == '(' || c.is_ascii_whitespace())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let textual = matches!(
        base.as_str(),
        "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext"
    );
    let (text, numeric_source) = match value {
        Value::Text(s) => (s, false),
        Value::Integer(s) | Value::Unsigned(s) | Value::Decimal(s) => (s, true),
        Value::Float(s) if textual => (s, false),
        Value::Date(s) | Value::Time(s) | Value::DateTime(s) if textual => (s, false),
        Value::Bool(v) if textual => (v.to_string(), false),
        Value::Json(v) if textual => (serde_json::to_string(&v)?, false),
        _ => {
            return Err(AppError::invalid(format!(
                "列 {} 的类型转换无法无损完成",
                column.name
            )));
        }
    };
    // 在浮点尾数能完整表示的整数范围内转换，其余精确数值不推断为无损。
    if numeric_source && matches!(base.as_str(), "float" | "double" | "real") {
        let limit = if base == "float" {
            1u128 << 24
        } else {
            1u128 << 53
        };
        let exact = text
            .parse::<i128>()
            .ok()
            .is_some_and(|n| n.unsigned_abs() <= limit);
        if !exact {
            return Err(AppError::invalid(format!(
                "列 {} 的浮点转换无法证明无损",
                column.name
            )));
        }
        let converted = crate::execution::transfer::csv_typed(text, column, false)?;
        crate::providers::mysql::validate_import_value(&converted, column)?;
        return Ok(converted);
    }
    let original = if numeric_source && !textual {
        Some(
            text.parse::<bigdecimal::BigDecimal>()
                .map_err(|_| AppError::invalid("无效精确数值"))?,
        )
    } else {
        None
    };
    let converted = crate::execution::transfer::csv_typed(text, column, false)?;
    if let Some(original) = original {
        let converted_number = match &converted {
            Value::Integer(s) | Value::Unsigned(s) | Value::Decimal(s) => {
                s.parse::<bigdecimal::BigDecimal>().ok()
            }
            _ => None,
        };
        if converted_number.as_ref() != Some(&original) {
            return Err(AppError::invalid(format!(
                "列 {} 的类型转换会损失精度",
                column.name
            )));
        }
    }
    crate::providers::mysql::validate_import_value(&converted, column)?;
    Ok(converted)
}

fn import_object(
    input: &TransferInput,
    table: &MigrationTable,
    columns: &[Column],
    options: &TransferOptions,
) -> Result<(TransferObject, Vec<usize>, Vec<Column>)> {
    let target = input
        .target
        .clone()
        .ok_or_else(|| AppError::invalid("输入缺少目标表"))?;
    let mut mappings = Vec::new();
    let mut indices = Vec::new();
    let mut target_columns = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (index, source) in columns.iter().enumerate() {
        let name = input
            .column_mapping
            .get(&source.name)
            .unwrap_or(&source.name);
        let column = match table
            .schema
            .columns
            .iter()
            .find(|column| &column.name == name)
        {
            Some(column) if column.generated => continue,
            Some(column) => column,
            // 文件格式只记录生成列元数据，不含可用于建表的生成表达式。
            None if source.generated => continue,
            None => return Err(AppError::invalid(format!("目标列不存在：{name}"))),
        };
        if !seen.insert(name.clone()) {
            return Err(AppError::invalid("多个输入列映射到同一目标列"));
        }
        mappings.push(ColumnMapping {
            source: source.name.clone(),
            target: name.clone(),
        });
        indices.push(index);
        target_columns.push(column.clone());
    }
    if mappings.is_empty() {
        return Err(AppError::invalid("没有可写入的目标列"));
    }
    Ok((
        TransferObject {
            source: target.clone(),
            target,
            columns: mappings,
            target_columns: Vec::new(),
            filters: Vec::new(),
            locator_key: options.locator_key.clone(),
            create_if_missing: false,
            sync_schema: false,
            selected_schema_items: Vec::new(),
        },
        indices,
        target_columns,
    ))
}

struct ImportTarget<'a> {
    session: &'a MysqlSession,
    table: &'a MigrationTable,
}

async fn flush(
    target: ImportTarget<'_>,
    object: &TransferObject,
    policy: ConflictPolicy,
    rows: &mut Vec<Vec<Value>>,
    position: (usize, u64, u64),
    journal: &mut TransferJournal,
    cancel: &CancellationToken,
) -> Result<()> {
    let (input, first, last) = position;
    if rows.is_empty() {
        return Ok(());
    }
    let batch = std::mem::take(rows);
    super::mysql::write_rows(
        target.session,
        target.table,
        object,
        policy,
        super::mysql::RowBatch {
            rows: batch,
            locator: TransferLocator {
                object: Some(object.target.name.clone()),
                input: Some(input),
                first_record: Some(first),
                last_record: Some(last),
                ..Default::default()
            },
        },
        journal,
        cancel,
    )
    .await
}

async fn conversion_failure(
    error: AppError,
    input: usize,
    object: &TransferObject,
    record: Option<u64>,
    journal: &mut TransferJournal,
    skip: bool,
) -> Result<()> {
    let locator = TransferLocator {
        object: Some(object.target.name.clone()),
        input: Some(input),
        first_record: record,
        last_record: record,
        byte_start: error
            .details
            .get("byte_offset")
            .and_then(serde_json::Value::as_u64),
        ..Default::default()
    };
    let ticket = journal
        .begin(
            "conversion",
            locator,
            digest(&(input, record, &error.code))?,
        )
        .await?;
    if skip {
        journal.progress.skipped += 1;
    } else {
        journal.progress.failed += 1;
    }
    journal
        .finish(
            ticket,
            if skip {
                TransferStepState::Skipped
            } else {
                TransferStepState::Failed
            },
            0,
            Vec::new(),
            Some(&error),
            record,
        )
        .await?;
    journal.checkpoint().await?;
    if skip { Ok(()) } else { Err(error) }
}

pub(super) async fn import_mysql(
    prepared: &PreparedTransfer,
    target: &mut MysqlSession,
    journal: &mut TransferJournal,
    cancel: &CancellationToken,
) -> Result<()> {
    if prepared.inputs.len() != prepared.spec.inputs.len() {
        return Err(AppError::invalid("输入指纹数量不匹配"));
    }
    let mut fingerprints = BTreeMap::new();
    if prepared.spec.options.include_schema {
        super::mysql::apply_schema_phase(
            prepared,
            target,
            "schema",
            journal,
            &mut fingerprints,
            cancel,
        )
        .await?;
    }
    if prepared.spec.options.include_data {
        for (input_index, (original, fingerprint)) in prepared
            .spec
            .inputs
            .iter()
            .zip(&prepared.inputs)
            .enumerate()
        {
            cancelled(cancel)?;
            let resource = original
                .target
                .clone()
                .ok_or_else(|| AppError::invalid("输入缺少目标表"))?;
            let table = target.describe_migration(resource, cancel).await?;
            let mut input = original.clone();
            input.columns = table.schema.columns.clone();
            let fingerprint = fingerprint.clone();
            let total = fingerprint.bytes;
            let (tx, mut rx) = mpsc::channel(2);
            let worker_cancel = cancel.child_token();
            let worker = tokio::task::spawn_blocking(move || -> Result<()> {
                let mut reader = RowReader::open_with_cancel(
                    &input,
                    &fingerprint,
                    ProviderKind::Mysql,
                    &worker_cancel,
                )?;
                if tx
                    .blocking_send(InputMessage::Columns(reader.columns().to_vec()))
                    .is_err()
                {
                    return Ok(());
                }
                loop {
                    match reader.next_event() {
                        Ok(Some(RowEvent::MysqlRow {
                            record,
                            byte_offset,
                            values,
                        })) => {
                            if tx
                                .blocking_send(InputMessage::Row(ImportRow {
                                    record,
                                    offset: byte_offset,
                                    values,
                                }))
                                .is_err()
                            {
                                break;
                            }
                        }
                        Ok(None) => break,
                        Ok(Some(_)) => return Err(AppError::invalid("MySQL 输入包含非 MySQL 行")),
                        Err(error) => {
                            let recoverable = error.code == "conversion"
                                && error
                                    .details
                                    .get("recoverable")
                                    .and_then(serde_json::Value::as_bool)
                                    == Some(true);
                            if tx.blocking_send(InputMessage::Error(error)).is_err() || !recoverable
                            {
                                break;
                            }
                        }
                    }
                }
                Ok(())
            });
            let mut batch = Vec::new();
            let mut bytes = 0usize;
            let mut first = 0u64;
            let mut last = 0u64;
            let mut offset = 0u64;
            let result: Result<()> = async {
                let columns = match tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), value = rx.recv() => value } { Some(InputMessage::Columns(columns)) => columns, _ => return Err(AppError::new("io", "输入读取器未提供列定义")) };
                let (object, indices, columns) = import_object(original, &table, &columns, &prepared.spec.options)?;
                journal.progress.current_object = Some(object.target.name.clone()); journal.progress.phase = "data".into();
                while let Some(message) = tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), message = rx.recv() => message } {
                    match message {
                        InputMessage::Columns(_) => return Err(AppError::new("internal", "输入列定义重复")),
                        InputMessage::Error(error) => {
                            flush(ImportTarget { session: target, table: &table }, &object, prepared.spec.options.conflict, &mut batch, (input_index, first, last), journal, cancel).await?; bytes = 0;
                            let record = error.details.get("record").and_then(serde_json::Value::as_u64);
                            if let Some(pos) = error.details.get("byte_offset").and_then(serde_json::Value::as_u64) { journal.progress.processed_bytes = journal.progress.processed_bytes.saturating_add(pos.saturating_sub(offset)); offset = pos; }
                            if error.code != "conversion" || error.details.get("recoverable").and_then(serde_json::Value::as_bool) != Some(true) { return Err(error); }
                            journal.progress.read_rows += 1;
                            conversion_failure(error, input_index, &object, record, journal, prepared.spec.options.conversion_errors == ConversionPolicy::Skip).await?;
                        }
                        InputMessage::Row(row) => {
                            journal.progress.processed_bytes = journal.progress.processed_bytes.saturating_add(row.offset.saturating_sub(offset)); offset = row.offset; journal.progress.read_rows += 1;
                            let mut selected = Vec::with_capacity(indices.len());
                            let mut values = row.values.into_iter();
                            let mut next = 0;
                            let conversion: Result<()> = (|| {
                                for (&index, column) in indices.iter().zip(&columns) {
                                    let value = values.nth(index - next).ok_or_else(|| AppError::invalid("输入行列数不匹配"))?;
                                    next = index + 1;
                                    let value = if original.format == TransferFormat::Json { convert_json(value, column)? } else { crate::providers::mysql::validate_import_value(&value, column)?; value };
                                    selected.push(value);
                                }
                                Ok(())
                            })();
                            if let Err(error) = conversion {
                                flush(ImportTarget { session: target, table: &table }, &object, prepared.spec.options.conflict, &mut batch, (input_index, first, last), journal, cancel).await?; bytes = 0;
                                let error = AppError::new("conversion", error.message).with_details(json!({"record":row.record,"byte_offset":row.offset,"recoverable":true}));
                                conversion_failure(error, input_index, &object, Some(row.record), journal, prepared.spec.options.conversion_errors == ConversionPolicy::Skip).await?;
                                continue;
                            }
                            let size = encoded_size(&selected)?;
                            if !batch.is_empty() && (batch.len() >= MAX_BATCH_ROWS || bytes.saturating_add(size) > MAX_BATCH_BYTES || row.record != last + 1) {
                                flush(ImportTarget { session: target, table: &table }, &object, prepared.spec.options.conflict, &mut batch, (input_index, first, last), journal, cancel).await?; bytes = 0;
                            }
                            if batch.is_empty() { first = row.record; } last = row.record; bytes = bytes.saturating_add(size); batch.push(selected);
                        }
                    }
                }
                flush(ImportTarget { session: target, table: &table }, &object, prepared.spec.options.conflict, &mut batch, (input_index, first, last), journal, cancel).await?;
                Ok(())
            }.await;
            drop(rx);
            let worker_result = worker.await.map_err(io_join)?;
            worker_result?;
            result?;
            journal.progress.processed_bytes = journal
                .progress
                .processed_bytes
                .saturating_add(total.saturating_sub(offset));
            journal.checkpoint().await?;
        }
    }
    if prepared.spec.options.include_schema {
        super::mysql::apply_schema_phase(
            prepared,
            target,
            "after_data",
            journal,
            &mut fingerprints,
            cancel,
        )
        .await?;
    }
    Ok(())
}

enum OutputMessage {
    Rows(Vec<Vec<Value>>, oneshot::Sender<Result<()>>),
    Seal(oneshot::Sender<Result<OutputSnapshot>>),
    Publish(oneshot::Sender<Result<()>>),
}
async fn send_rows(sender: &mpsc::Sender<OutputMessage>, rows: Vec<Vec<Value>>) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    sender
        .send(OutputMessage::Rows(rows, tx))
        .await
        .map_err(|_| AppError::new("io", "输出线程已退出"))?;
    rx.await
        .map_err(|_| AppError::new("io", "输出线程未确认"))?
}
async fn seal(sender: &mpsc::Sender<OutputMessage>) -> Result<OutputSnapshot> {
    let (tx, rx) = oneshot::channel();
    sender
        .send(OutputMessage::Seal(tx))
        .await
        .map_err(|_| AppError::new("io", "输出线程已退出"))?;
    rx.await
        .map_err(|_| AppError::new("io", "输出线程未确认"))?
}
async fn publish(sender: &mpsc::Sender<OutputMessage>) -> Result<()> {
    let (tx, rx) = oneshot::channel();
    sender
        .send(OutputMessage::Publish(tx))
        .await
        .map_err(|_| AppError::new("io", "输出线程已退出"))?;
    rx.await
        .map_err(|_| AppError::new("io", "输出线程未确认"))?
}
fn output_path(
    base: &Path,
    object: &TransferObject,
    index: usize,
    multiple: bool,
    format: TransferFormat,
) -> PathBuf {
    if !multiple {
        return base.to_path_buf();
    }
    // 固定序号与哈希避免文件名碰撞、路径穿越及平台保留名称。
    let hash = format!(
        "{:x}",
        base16ct::HexDisplay(&Sha256::digest(
            format!(
                "{}\0{}",
                object.source.database.as_deref().unwrap_or(""),
                object.source.name
            )
            .as_bytes()
        ))
    );
    let extension = match format {
        TransferFormat::Csv => "csv",
        TransferFormat::Json => "json",
        TransferFormat::Sql => "sql",
    };
    base.join(format!("table-{}-{}.{}", index + 1, &hash[..16], extension))
}

pub(super) async fn export_mysql(
    prepared: &PreparedTransfer,
    source: &mut MysqlSession,
    journal: &mut TransferJournal,
    cancel: &CancellationToken,
) -> Result<()> {
    let config = prepared
        .spec
        .output
        .as_ref()
        .ok_or_else(|| AppError::invalid("缺少输出文件"))?;
    let multiple = prepared.spec.objects.len() > 1;
    if multiple {
        let path = config.path.clone();
        if !tokio::task::spawn_blocking(move || path.is_dir())
            .await
            .map_err(io_join)?
        {
            return Err(AppError::invalid("多表导出路径必须是现有目录"));
        }
    }
    let mut tables = Vec::with_capacity(prepared.spec.objects.len());
    let mut columns = Vec::with_capacity(prepared.spec.objects.len());
    for object in &prepared.spec.objects {
        let table = source
            .describe_migration(object.source.clone(), cancel)
            .await?;
        let mut selected = source_columns(&table.schema, object)?;
        if !object.columns.is_empty() {
            for (column, mapping) in selected.iter_mut().zip(&object.columns) {
                column.name = mapping.target.clone();
            }
        }
        columns.push(selected);
        tables.push((table, object.clone()));
    }
    let child = cancel.child_token();
    let mut stream = source
        .open_source_stream(tables, prepared.spec.options.consistency, child.clone())
        .await?;
    let mut pending = None;
    let result: Result<()> = async {
        for (index, object) in prepared.spec.objects.iter().enumerate() {
            cancelled(cancel)?;
            let destination = output_path(&config.path, object, index, multiple, config.format);
            let path = destination.clone();
            let (atomic, initial, partial) = tokio::task::spawn_blocking(move || -> Result<_> {
                let mut atomic = AtomicOutput::create(&path)?;
                let initial = atomic.snapshot(true)?;
                let partial = atomic.partial_path().to_path_buf();
                Ok((atomic, initial, partial))
            }).await.map_err(io_join)??;
            let artifact_sink = journal.sink.clone().with_transfer_step(journal.run_id, journal.next_ordinal());
            let artifact = artifact_sink.register_transfer_artifact("export_partial", identity(&partial, &initial)?).await?;
            let (tx, mut rx) = mpsc::channel(2);
            let format = config.format;
            let output_columns = columns[index].clone();
            let task = tokio::task::spawn_blocking(move || -> Result<()> {
                let mut writer = CsvJsonWriter::mysql(atomic, format, CsvOptions::default(), output_columns)?;
                while let Some(message) = rx.blocking_recv() {
                    match message {
                        OutputMessage::Rows(rows, reply) => {
                            let result = rows.iter().try_for_each(|row| writer.write_row(row));
                            let failed = result.is_err(); let _ = reply.send(result); if failed { break; }
                        }
                        OutputMessage::Seal(reply) => { let result = writer.seal().and_then(|()| writer.snapshot(true)); let _ = reply.send(result); }
                        OutputMessage::Publish(reply) => { let result = writer.finish().map(|_| ()); let _ = reply.send(result); return Ok(()); }
                    }
                }
                Ok(())
            });
            let mut uncertain = false;
            let writing: Result<()> = async {
                journal.progress.phase = "data".into(); journal.progress.current_object = Some(object.source.name.clone());
                loop {
                    let next = if pending.is_some() { pending.take() } else { tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), value = stream.recv() => value.transpose()? } };
                    let Some(batch) = next else { break; };
                    if batch.object > index { pending = Some(batch); break; }
                    if batch.object != index { return Err(AppError::new("internal", "源批次对象顺序错误")); }
                    let count = batch.rows.len() as u64;
                    journal.progress.read_rows += count;
                    journal.progress.processed_bytes = journal.progress.processed_bytes.saturating_add(batch.bytes as u64);
                    send_rows(&tx, batch.rows).await?;
                    journal.progress.written_rows += count;
                    journal.checkpoint().await?;
                }
                let snapshot = seal(&tx).await?;
                same_identity(&initial, &snapshot)?;
                artifact_sink.update_transfer_artifact(artifact, identity(&partial, &snapshot)?).await?;
                let ticket = journal.begin("mysql_export_publish", TransferLocator { object: Some(object.source.name.clone()), ..Default::default() }, digest(&destination)?).await?;
                ticket.sink.mark_sent(true).await?;
                uncertain = true;
                match publish(&tx).await {
                    Ok(()) => {
                        journal.finish(ticket, TransferStepState::Applied, 1, Vec::new(), None, None).await?;
                        let path = destination.clone();
                        let published = tokio::task::spawn_blocking(move || crate::execution::data_files::output_snapshot(&path, true)).await.map_err(io_join)??;
                        if published.file_id != snapshot.file_id || published.sha256 != snapshot.sha256 || published.size != snapshot.size { return Err(AppError::new("recovery_conflict", "已发布文件身份或内容发生变化")); }
                        artifact_sink.update_transfer_artifact(artifact, identity(&destination, &published)?).await?;
                        journal.sink.finish_transfer_artifact(artifact, "published").await?;
                    }
                    Err(error) => {
                        use crate::execution::data_files::{publication_state, PublicationState};
                        let publication = publication_state(&error);
                        uncertain = publication == PublicationState::Unknown;
                        let mut error = error;
                        if uncertain { error.code = "outcome_unknown".into(); journal.sink.finish_transfer_artifact(artifact, "outcome_unknown").await?; }
                        let state = match publication { PublicationState::NotPublished => TransferStepState::Failed, PublicationState::Published => TransferStepState::Applied, PublicationState::Unknown => TransferStepState::OutcomeUnknown };
                        journal.finish(ticket, state, u64::from(publication == PublicationState::Published), Vec::new(), Some(&error), None).await?;
                        return Err(error);
                    }
                }
                Ok(())
            }.await;
            drop(tx);
            let worker_result = task.await.map_err(io_join).and_then(|result| result);
            let result = worker_result.and(writing);
            if result.is_err() && !uncertain {
                let path = partial.clone();
                let snapshot = tokio::task::spawn_blocking(move || crate::execution::data_files::output_snapshot(&path, true)).await.map_err(io_join)??;
                same_identity(&initial, &snapshot)?;
                artifact_sink.update_transfer_artifact(artifact, identity(&partial, &snapshot)?).await?;
                journal.sink.finish_transfer_artifact(artifact, "cleanup_pending").await?;
            }
            result?;
        }
        if pending.is_some() { return Err(AppError::new("internal", "源批次对象序号无效")); }
        Ok(())
    }.await;
    child.cancel();
    drop(stream);
    result
}
