//! Redis 迁移逐 key 编排；数据库内容只通过容量为二的内存通道传递。
use super::*;
use crate::{
    execution::data_files::{AtomicOutput, CsvJsonWriter, RowEvent, RowReader},
    model::{Column, ResourceTarget, Value},
    providers::redis::{
        RedisSession,
        migration::{RedisMigrationChunk, RedisMigrationSource, map_key, validate_overlap},
    },
    storage::transfer::KeyReservation,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value as Json, json};
use tokio::sync::{mpsc, oneshot};

fn decode(value: &str) -> Result<Vec<u8>> {
    STANDARD
        .decode(value)
        .map_err(|e| AppError::invalid(e.to_string()))
}
fn selected_prefix(spec: &TransferSpec, key: &[u8]) -> Result<bool> {
    match &spec.redis.source_prefix_b64 {
        Some(prefix) => Ok(key.starts_with(&decode(prefix)?)),
        None => Ok(true),
    }
}
fn resource(key: &[u8]) -> ResourceTarget {
    ResourceTarget {
        database: None,
        name: String::new(),
        key_b64: Some(STANDARD.encode(key)),
        kind: "key".into(),
    }
}
fn key_locator(
    source: &[u8],
    target: &[u8],
    input: Option<usize>,
    record: Option<u64>,
) -> TransferLocator {
    TransferLocator {
        source_digest: Some(format!(
            "{:x}",
            base16ct::HexDisplay(&Sha256::digest(source))
        )),
        target_digest: Some(format!(
            "{:x}",
            base16ct::HexDisplay(&Sha256::digest(target))
        )),
        input,
        first_record: record,
        last_record: record,
        ..Default::default()
    }
}
fn state(error: &AppError) -> TransferStepState {
    if error.code == "outcome_unknown" {
        TransferStepState::OutcomeUnknown
    } else {
        TransferStepState::Failed
    }
}

// 直接复制计量源内容字节，不把 Base64 膨胀或 List 索引算作 payload。
fn payload_bytes(rows: &[Vec<Value>]) -> u64 {
    rows.iter()
        .flatten()
        .map(|value| match value {
            Value::Bytes(encoded) => {
                (encoded.len() / 4 * 3 - encoded.bytes().rev().take_while(|b| *b == b'=').count())
                    as u64
            }
            Value::Float(score) => score.len() as u64,
            _ => 0,
        })
        .sum()
}

fn skip_reason(reason: &str) -> Result<AppError> {
    let code = match reason {
        "expired" => "redis_expired_before_publish",
        "exists" => "redis_target_exists",
        _ => return Err(AppError::new("database", "Redis 跳过原因缺失或未知")),
    };
    Ok(AppError::new(code, "Redis key 按策略跳过"))
}

async fn checkpoint_progress(
    journal: &mut TransferJournal,
    last: &mut std::time::Instant,
) -> Result<()> {
    if last.elapsed() >= std::time::Duration::from_millis(500) {
        journal.checkpoint().await?;
        *last = std::time::Instant::now();
    }
    Ok(())
}
fn cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(AppError::cancelled())
    } else {
        Ok(())
    }
}

async fn validate(
    spec: &TransferSpec,
    source: Option<&RedisSession>,
    target: Option<&RedisSession>,
) -> Result<Option<RedisMigrationSource>> {
    if !spec.redis.keys_b64.is_empty() && spec.redis.pattern.is_some() {
        return Err(AppError::invalid("显式 key 和 pattern 不能同时指定"));
    }
    if let Some(prefix) = &spec.redis.source_prefix_b64 {
        decode(prefix)?;
    }
    if let Some(prefix) = &spec.redis.target_prefix_b64 {
        decode(prefix)?;
    }
    for key in &spec.redis.keys_b64 {
        map_key(&spec.redis, &decode(key)?)?;
    }
    let Some(source) = source else {
        return Ok(None);
    };
    let mut src = RedisMigrationSource::new(source).await?;
    if let Some(target) = target {
        let mut dst = RedisMigrationSource::new(target).await?;
        let same_db = src.database() == dst.database();
        if same_db {
            let a = src.instance_identity().await?;
            let b = dst.instance_identity().await?;
            let same = match (a, b) {
                (Some(a), Some(b)) => Some(a == b),
                _ => None,
            };
            validate_overlap(&spec.redis, same, true)?;
        }
    }
    Ok(Some(src))
}

pub(super) async fn prepare(
    spec: &TransferSpec,
    source: Option<&RedisSession>,
    target: Option<&RedisSession>,
    cancel: &CancellationToken,
) -> Result<(Vec<TransferSample>, Vec<TransferDiagnostic>)> {
    cancelled(cancel)?;
    let mut source = validate(spec, source, target).await?;
    let source_time = if let Some(source) = source.as_mut() {
        Some(source.sample_time().await?)
    } else {
        None
    };
    let target_time = if let Some(target) = target {
        Some(
            RedisMigrationSource::new(target)
                .await?
                .sample_time()
                .await?,
        )
    } else {
        None
    };
    let mut details = Vec::new();
    if let Some((time, _, rtt)) = source_time {
        details.push(format!("源 TIME={time} ms，往返={} ms", rtt.as_millis()));
    }
    if let Some((time, _, rtt)) = target_time {
        details.push(format!("目标 TIME={time} ms，往返={} ms", rtt.as_millis()));
    }
    if let (Some((src, src_mid, src_rtt)), Some((dst, dst_mid, dst_rtt))) =
        (source_time, target_time)
    {
        let elapsed = dst_mid.duration_since(src_mid).as_millis() as i128;
        let delta = i128::from(dst) - i128::from(src) - elapsed;
        let uncertainty = (src_rtt + dst_rtt).as_millis().div_ceil(2) + 2;
        details.push(format!("目标相对源时钟差约 {delta} ms，采样误差最多约 {uncertainty} ms；未校准服务器时钟，时钟差会影响剩余寿命"));
    }
    let diagnostics = if details.is_empty() {
        Vec::new()
    } else {
        vec![TransferDiagnostic {
            code: "redis_server_time".into(),
            message: details.join("；"),
            object: None,
            input: None,
            record: None,
            byte_offset: None,
            blocking: false,
        }]
    };
    let Some(mut source) = source else {
        return Ok((Vec::new(), diagnostics));
    };
    if spec.options.preview_rows == 0 {
        return Ok((Vec::new(), diagnostics));
    }
    let mut cursor = 0;
    let mut explicit = spec.redis.keys_b64.iter();
    let key = loop {
        cancelled(cancel)?;
        if !spec.redis.keys_b64.is_empty() {
            match explicit.next() {
                Some(key) => {
                    let key = decode(key)?;
                    if selected_prefix(spec, &key)? {
                        break Some(key);
                    }
                }
                None => break None,
            }
        } else {
            let (next, keys) = source
                .scan_page(cursor, spec.redis.pattern.as_deref())
                .await?;
            let mut found = None;
            for key in keys {
                if selected_prefix(spec, &key)? {
                    found = Some(key);
                    break;
                }
            }
            if found.is_some() {
                break found;
            }
            cursor = next;
            if cursor == 0 {
                break None;
            }
        }
    };
    let Some(key) = key else {
        return Ok((Vec::new(), diagnostics));
    };
    let (tx, mut rx) = mpsc::channel(2);
    let producer = source.stream_key(key.clone(), tx, cancel);
    let consumer = async move {
        let mut rows = Vec::new();
        let mut kind = String::new();
        let mut bytes = 0usize;
        let mut limited = false;
        while let Some(event) = rx.recv().await {
            match event {
                RedisMigrationChunk::Begin { kind: value, .. } => kind = value,
                RedisMigrationChunk::Rows(batch) => {
                    for row in batch {
                        let size = serde_json::to_vec(&row)?.len();
                        if rows.len() >= spec.options.preview_rows
                            || bytes.saturating_add(size) > MAX_BATCH_BYTES
                        {
                            limited = true;
                            break;
                        }
                        bytes += size;
                        rows.push(row);
                    }
                }
                RedisMigrationChunk::StringPart(_) => {
                    return Err(AppError::invalid("迁移源返回了分块 String"));
                }
                RedisMigrationChunk::Verified
                | RedisMigrationChunk::Expired
                | RedisMigrationChunk::Missing => break,
            }
            if limited {
                break;
            }
        }
        Ok::<_, AppError>((kind, rows, limited))
    };
    let (read, sample) = tokio::join!(producer, consumer);
    let (kind, rows, limited) = sample?;
    if !limited {
        read?;
    }
    if kind.is_empty() {
        return Ok((Vec::new(), diagnostics));
    }
    let names: &[&str] = match kind.as_str() {
        "hash" => &["field", "value", "field_expires_at_ms"],
        "list" => &["index", "value"],
        "zset" => &["member", "score"],
        _ => &["value"],
    };
    let columns = names
        .iter()
        .map(|name| Column::new(*name, "redis"))
        .collect();
    Ok((
        vec![TransferSample {
            object: STANDARD.encode(key),
            columns,
            rows,
            sampled: true,
            errors: Vec::new(),
        }],
        diagnostics,
    ))
}

async fn discard(target: &mut RedisSession, token: &Json, sink: &ExecutionSink) -> Result<()> {
    // 取消之后仍须使用独立 token 清理已登记的临时对象。
    target
        .call(
            "import.discard",
            json!({"token":token}),
            sink,
            &CancellationToken::new(),
        )
        .await
        .map(|_| ())
}
async fn append(
    target: &mut RedisSession,
    token: &Json,
    rows: Vec<Vec<Value>>,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<()> {
    target
        .call(
            "import.append",
            json!({"token":token,"rows":rows}),
            sink,
            cancel,
        )
        .await
        .map(|_| ())
}

async fn copy_key(
    spec: &TransferSpec,
    source: &mut RedisMigrationSource,
    target: &mut RedisSession,
    keys: (Vec<u8>, Vec<u8>),
    journal: &mut TransferJournal,
    cancel: &CancellationToken,
    ticket: TransferTicket,
) -> Result<()> {
    journal.progress.current_object = ticket.locator().source_digest.clone();
    journal.progress.phase = "redis_read".into();
    let mut last_progress = std::time::Instant::now();
    let (key, mapped) = keys;
    let (tx, mut rx) = mpsc::channel(2);
    let reader = source.stream_key(key, tx, cancel);
    let consume = async {
        let mut token: Option<Json> = None;
        let mut verified = false;
        let mut expired = false;
        let mut missing = false;
        let result: Result<()> = async {
            while let Some(event) = rx.recv().await {
                match event {
                    RedisMigrationChunk::Begin { kind, expires_at_ms } => {
                        let reply = target.call("import.begin", json!({"target":resource(&mapped),"type":kind,"expires_at_ms":expires_at_ms.map(|n| n.to_string()),"conflict_policy":spec.options.conflict}), &ticket.sink, cancel).await?;
                        token = Some(reply.get("token").cloned().ok_or_else(|| AppError::new("database", "导入令牌缺失"))?);
                    }
                    RedisMigrationChunk::Rows(rows) => {
                        let bytes = payload_bytes(&rows);
                        append(target, token.as_ref().ok_or_else(|| AppError::invalid("Redis key 缺少头部"))?, rows, &ticket.sink, cancel).await?;
                        journal.progress.processed_bytes = journal.progress.processed_bytes.saturating_add(bytes);
                        checkpoint_progress(journal, &mut last_progress).await?;
                    }
                    RedisMigrationChunk::StringPart(_) => return Err(AppError::invalid("迁移源返回了分块 String")),
                    RedisMigrationChunk::Verified => verified = true,
                    RedisMigrationChunk::Expired => expired = true,
                    RedisMigrationChunk::Missing => missing = true,
                }
            }
            Ok(())
        }.await;
        (token, verified, expired, missing, result)
    };
    let (read, (token, verified, expired, missing, consumed)) = tokio::join!(reader, consume);
    let mut error = consumed.err().or_else(|| read.err());
    if error.is_none() && !verified && !expired && !missing {
        error = Some(AppError::new("database", "源 key 未完成一致性校验"));
    }
    if error.is_none() && cancel.is_cancelled() {
        error = Some(AppError::cancelled());
    }
    if error.is_some() || expired || missing {
        if let Some(token) = &token
            && let Err(cleanup) = discard(target, token, &ticket.sink).await
        {
            error = Some(cleanup);
        }
        let status = error
            .as_ref()
            .map(state)
            .unwrap_or(TransferStepState::Skipped);
        if status == TransferStepState::Skipped {
            journal.progress.skipped += 1;
        } else {
            journal.progress.failed += 1;
        }
        let reason = if expired {
            Some(AppError::new("redis_source_expired", "源 key 已过期"))
        } else if missing {
            Some(AppError::new(
                "redis_source_missing",
                "源 key 不存在或已过期",
            ))
        } else {
            None
        };
        journal
            .finish(
                ticket,
                status,
                0,
                Vec::new(),
                error.as_ref().or(reason.as_ref()),
                None,
            )
            .await?;
        journal.checkpoint().await?;
        return error.map_or(Ok(()), Err);
    }
    journal.progress.phase = "redis_publish".into();
    let token = token.ok_or_else(|| AppError::new("database", "源 key 缺少导入令牌"))?;
    let result = target
        .call(
            "import.finish",
            json!({"token":token}),
            &ticket.sink,
            cancel,
        )
        .await;
    match result {
        Ok(result) => {
            let skipped = result.get("skipped").and_then(Json::as_bool) == Some(true);
            let reason = if skipped {
                Some(skip_reason(
                    result.get("reason").and_then(Json::as_str).unwrap_or(""),
                )?)
            } else {
                None
            };
            if skipped {
                journal.progress.skipped += 1;
            } else {
                journal.progress.written_keys += 1;
            }
            journal
                .finish(
                    ticket,
                    if skipped {
                        TransferStepState::Skipped
                    } else {
                        TransferStepState::Applied
                    },
                    u64::from(!skipped),
                    Vec::new(),
                    reason.as_ref(),
                    None,
                )
                .await?;
        }
        Err(mut error) => {
            if error.code != "outcome_unknown"
                && let Err(cleanup) = discard(target, &token, &ticket.sink).await
            {
                error = cleanup;
            }
            let status = state(&error);
            if status == TransferStepState::OutcomeUnknown {
                journal.progress.outcome_unknown += 1;
            } else {
                journal.progress.failed += 1;
            }
            journal
                .finish(ticket, status, 0, Vec::new(), Some(&error), None)
                .await?;
            return Err(error);
        }
    }
    journal.progress.read_keys += 1;
    journal.checkpoint().await
}

async fn import_files(
    spec: &TransferSpec,
    target: &mut RedisSession,
    inputs: &[InputFingerprint],
    journal: &mut TransferJournal,
    cancel: &CancellationToken,
) -> Result<()> {
    if inputs.len() != spec.inputs.len() {
        return Err(AppError::invalid("输入指纹数量不匹配"));
    }
    journal.progress.total_bytes = Some(inputs.iter().map(|input| input.bytes).sum());
    for (input_index, (input, fingerprint)) in spec
        .inputs
        .iter()
        .cloned()
        .zip(inputs.iter().cloned())
        .enumerate()
    {
        let input_bytes = fingerprint.bytes;
        let mut consumed = 0u64;
        let mut last_progress = std::time::Instant::now();
        let (tx, mut rx) = mpsc::channel::<Result<(RowEvent, u64)>>(2);
        let worker_cancel = cancel.child_token();
        let reader_cancel = worker_cancel.clone();
        let worker = tokio::task::spawn_blocking(move || -> Result<()> {
            let mut reader = RowReader::open_with_cancel(
                &input,
                &fingerprint,
                ProviderKind::Redis,
                &reader_cancel,
            )?;
            while let Some(event) = reader.next_event()? {
                let position = reader.bytes_consumed();
                if tx.blocking_send(Ok((event, position))).is_err() {
                    return Ok(());
                }
            }
            Ok(())
        });
        let mut current: Option<(TransferTicket, Json, u64)> = None;
        let result: Result<()> = async {
            loop {
                let event = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(AppError::cancelled()),
                    event = rx.recv() => event,
                };
                let Some(event) = event else { break; };
                cancelled(cancel)?;
                let (event, position) = event?;
                journal.progress.processed_bytes = journal.progress.processed_bytes.saturating_add(position.saturating_sub(consumed));
                consumed = consumed.max(position);
                match event {
                    RowEvent::RedisKeyHeader { record, key_b64, kind, expires_at_ms, .. } => {
                        if current.is_some() { return Err(AppError::invalid("Redis 输入 key 尚未结束")); }
                        let key = decode(&key_b64)?;
                        let mapped = map_key(&spec.redis, &key)?;
                        let ticket = journal.begin("redis_import", key_locator(&key, &mapped, Some(input_index), Some(record)), format!("{:x}", base16ct::HexDisplay(&Sha256::digest(&key)))).await?;
                        journal.progress.current_object = ticket.locator().source_digest.clone();
                        journal.progress.phase = "redis_import".into();
                        let begin = target.call("import.begin", json!({"target":resource(&mapped),"type":kind,"expires_at_ms":expires_at_ms,"conflict_policy":spec.options.conflict}), &ticket.sink, cancel).await;
                        match begin {
                            Ok(reply) => current = Some((ticket, reply.get("token").cloned().ok_or_else(|| AppError::new("database", "导入令牌缺失"))?, record)),
                            Err(error) => { journal.finish(ticket, state(&error), 0, Vec::new(), Some(&error), Some(record)).await?; return Err(error); }
                        }
                    }
                    RowEvent::RedisChunk { values, .. } => { let (ticket, token, _) = current.as_ref().ok_or_else(|| AppError::invalid("Redis 内容缺少 key"))?; append(target, token, vec![values], &ticket.sink, cancel).await?; checkpoint_progress(journal, &mut last_progress).await?; }
                    RowEvent::RedisKeyEnd { record } => {
                        journal.progress.phase = "redis_publish".into();
                        let (ticket, token, _) = current.take().ok_or_else(|| AppError::invalid("Redis key 结束顺序错误"))?;
                        match target.call("import.finish", json!({"token":token}), &ticket.sink, cancel).await {
                            Ok(reply) => { let skipped = reply.get("skipped").and_then(Json::as_bool) == Some(true); let reason = if skipped { Some(skip_reason(reply.get("reason").and_then(Json::as_str).unwrap_or(""))?) } else { None }; if skipped { journal.progress.skipped += 1; } else { journal.progress.written_keys += 1; } journal.finish(ticket, if skipped { TransferStepState::Skipped } else { TransferStepState::Applied }, u64::from(!skipped), Vec::new(), reason.as_ref(), None).await?; }
                            Err(mut error) => { if error.code != "outcome_unknown" && let Err(cleanup) = discard(target, &token, &ticket.sink).await { error = cleanup; } journal.finish(ticket, state(&error), 0, Vec::new(), Some(&error), Some(record)).await?; return Err(error); }
                        }
                        journal.progress.read_keys += 1; journal.checkpoint().await?;
                    }
                    _ => return Err(AppError::invalid("Redis 输入包含 MySQL 行")),
                }
            }
            Ok(())
        }.await;
        worker_cancel.cancel();
        drop(rx);
        let worker_result = worker
            .await
            .map_err(|e| AppError::new("io", e.to_string()))?;
        let mut failure = result.err().or_else(|| worker_result.err());
        if failure.is_none() && current.is_some() {
            failure = Some(AppError::invalid("Redis 输入 key 未结束"));
        }
        if let Some((ticket, token, record)) = current {
            if let Err(error) = discard(target, &token, &ticket.sink).await {
                failure = Some(error);
            }
            let error = failure.as_ref().expect("未完成的 key 必须带有错误");
            journal
                .finish(
                    ticket,
                    state(error),
                    0,
                    Vec::new(),
                    Some(error),
                    Some(record),
                )
                .await?;
        }
        if let Some(error) = failure {
            return Err(error);
        }
        journal.progress.processed_bytes = journal
            .progress
            .processed_bytes
            .saturating_add(input_bytes.saturating_sub(consumed));
        journal.checkpoint().await?;
    }
    Ok(())
}

enum OutputMessage {
    Event(RowEvent, oneshot::Sender<Result<u64>>),
    Snapshot(oneshot::Sender<Result<crate::execution::data_files::OutputSnapshot>>),
    Finish(oneshot::Sender<Result<u64>>),
}
async fn output_event(
    sender: &mpsc::Sender<OutputMessage>,
    event: Option<RowEvent>,
) -> Result<u64> {
    let (ack, rx) = oneshot::channel();
    let message = match event {
        Some(event) => OutputMessage::Event(event, ack),
        None => OutputMessage::Finish(ack),
    };
    sender
        .send(message)
        .await
        .map_err(|_| AppError::new("io", "输出线程已退出"))?;
    rx.await
        .map_err(|_| AppError::new("io", "输出线程未确认"))?
}

async fn output_snapshot(
    sender: &mpsc::Sender<OutputMessage>,
) -> Result<crate::execution::data_files::OutputSnapshot> {
    let (ack, rx) = oneshot::channel();
    sender
        .send(OutputMessage::Snapshot(ack))
        .await
        .map_err(|_| AppError::new("io", "输出线程已退出"))?;
    rx.await
        .map_err(|_| AppError::new("io", "输出线程未确认"))?
}

async fn export_key(
    source: &mut RedisMigrationSource,
    key: Vec<u8>,
    sender: &mpsc::Sender<OutputMessage>,
    journal: &mut TransferJournal,
    cancel: &CancellationToken,
    ticket: TransferTicket,
) -> Result<()> {
    journal.progress.current_object = ticket.locator().source_digest.clone();
    journal.progress.phase = "redis_export".into();
    let mut last_progress = std::time::Instant::now();
    let record = journal.progress.read_keys + 1;
    cancelled(cancel)?;
    ticket.sink.mark_sent(false).await?;
    let (tx, mut rx) = mpsc::channel(2);
    let reader = source.stream_key(key.clone(), tx, cancel);
    let writer = async {
        let mut verified = false;
        let mut began = false;
        while let Some(chunk) = rx.recv().await {
            match chunk {
                RedisMigrationChunk::Begin {
                    kind,
                    expires_at_ms,
                } => {
                    journal.progress.processed_bytes = output_event(
                        sender,
                        Some(RowEvent::RedisKeyHeader {
                            record,
                            byte_offset: 0,
                            key_b64: STANDARD.encode(&key),
                            kind,
                            expires_at_ms: expires_at_ms
                                .map(|n| Json::String(n.to_string()))
                                .unwrap_or(Json::Null),
                        }),
                    )
                    .await?;
                    began = true;
                }
                RedisMigrationChunk::Rows(rows) => {
                    for values in rows {
                        journal.progress.processed_bytes = output_event(
                            sender,
                            Some(RowEvent::RedisChunk {
                                record,
                                byte_offset: 0,
                                values,
                            }),
                        )
                        .await?;
                    }
                    checkpoint_progress(journal, &mut last_progress).await?;
                }
                RedisMigrationChunk::StringPart(_) => {
                    return Err(AppError::invalid("迁移源返回了分块 String"));
                }
                RedisMigrationChunk::Verified => verified = true,
                RedisMigrationChunk::Missing => return Ok(false),
                RedisMigrationChunk::Expired => {
                    if began {
                        return Err(AppError::conflict(
                            "源 key 导出期间已过期，输出保留为 partial",
                        ));
                    }
                    return Ok(false);
                }
            }
        }
        if !verified {
            return Err(AppError::conflict("源 key 未通过一致性校验"));
        }
        Ok(true)
    };
    let (read, written) = tokio::join!(reader, writer);
    let result: Result<bool> = async {
        read?;
        if written? {
            journal.progress.processed_bytes =
                output_event(sender, Some(RowEvent::RedisKeyEnd { record })).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
    .await;
    match result {
        Ok(true) => {
            journal.progress.read_keys += 1;
            journal
                .finish(
                    ticket,
                    TransferStepState::Applied,
                    1,
                    Vec::new(),
                    None,
                    None,
                )
                .await?;
        }
        Ok(false) => {
            journal.progress.skipped += 1;
            journal
                .finish(
                    ticket,
                    TransferStepState::Skipped,
                    0,
                    Vec::new(),
                    Some(&AppError::new(
                        "redis_source_missing",
                        "源 key 不存在或已过期",
                    )),
                    None,
                )
                .await?;
        }
        Err(error) => {
            journal.progress.failed += 1;
            journal
                .finish(ticket, state(&error), 0, Vec::new(), Some(&error), None)
                .await?;
            return Err(error);
        }
    }
    journal.checkpoint().await
}

pub(super) async fn execute(
    spec: &TransferSpec,
    source: Option<&mut RedisSession>,
    target: Option<&mut RedisSession>,
    inputs: &[InputFingerprint],
    journal: &mut TransferJournal,
    cancel: &CancellationToken,
) -> Result<()> {
    if spec.mode == TransferMode::Import {
        return import_files(
            spec,
            target.ok_or_else(|| AppError::invalid("缺少 Redis 目标"))?,
            inputs,
            journal,
            cancel,
        )
        .await;
    }
    let mut source = validate(spec, source.as_deref(), target.as_deref())
        .await?
        .ok_or_else(|| AppError::invalid("缺少 Redis 源"))?;
    let mut target = target;
    let mut output = None;
    let mut output_task = None;
    let artifact_sink = journal
        .sink
        .clone()
        .with_transfer_step(journal.run_id, journal.next_ordinal());
    let mut output_artifact = None;
    let mut partial_identity = None;
    let mut publication_uncertain = false;
    if spec.mode == TransferMode::Export {
        let config = spec
            .output
            .as_ref()
            .ok_or_else(|| AppError::invalid("缺少输出文件"))?
            .clone();
        let path = config.path.clone();
        let (atomic, initial, partial) = tokio::task::spawn_blocking(move || -> Result<_> {
            let mut atomic = AtomicOutput::create(&path)?;
            let initial = atomic.snapshot(true)?;
            let partial = atomic.partial_path().to_path_buf();
            Ok((atomic, initial, partial))
        })
        .await
        .map_err(|e| AppError::new("io", e.to_string()))??;
        output_artifact = Some(
            artifact_sink
                .register_transfer_artifact("export_partial", identity(&partial, &initial)?)
                .await?,
        );
        partial_identity = Some((partial, initial));
        let (tx, mut rx) = mpsc::channel::<OutputMessage>(2);
        output_task = Some(tokio::task::spawn_blocking(move || -> Result<()> {
            let mut writer = CsvJsonWriter::redis(atomic, config.format, CsvOptions::default())?;
            while let Some(message) = rx.blocking_recv() {
                match message {
                    OutputMessage::Event(event, reply) => {
                        let result = writer.write_event(&event).map(|()| writer.bytes_written());
                        let failed = result.is_err();
                        let _ = reply.send(result);
                        if failed {
                            return Ok(());
                        }
                    }
                    OutputMessage::Snapshot(reply) => {
                        let result = writer.seal().and_then(|()| writer.snapshot(true));
                        let _ = reply.send(result);
                    }
                    OutputMessage::Finish(reply) => {
                        let result = writer.finish().map(|_| 0);
                        let _ = reply.send(result);
                        return Ok(());
                    }
                }
            }
            Ok(())
        }));
        output = Some(tx);
    } else if spec.mode != TransferMode::Copy {
        return Err(AppError::invalid("Redis 不支持此迁移模式"));
    }
    let mut cursor = 0u64;
    let mut explicit = spec.redis.keys_b64.iter();
    let result: Result<()> = async {
        loop {
            cancelled(cancel)?;
            let keys = if !spec.redis.keys_b64.is_empty() {
                match explicit.next() {
                    Some(key) => vec![decode(key)?],
                    None => break,
                }
            } else {
                let (next, keys) = source
                    .scan_page(cursor, spec.redis.pattern.as_deref())
                    .await?;
                cursor = next;
                keys
            };
            for key in keys {
                if !selected_prefix(spec, &key)? {
                    continue;
                }
                let mapped = if spec.mode == TransferMode::Copy {
                    map_key(&spec.redis, &key)?
                } else {
                    key.clone()
                };
                // key 归属引用步骤外键；先登记本次实际步骤，再原子预留源／目标摘要。
                let phase = if target.is_some() {
                    "redis_key"
                } else {
                    "redis_export_key"
                };
                let locator = key_locator(&key, &mapped, None, None);
                let content_digest = locator
                    .source_digest
                    .clone()
                    .ok_or_else(|| AppError::invalid("缺少源 key 摘要"))?;
                let ticket = journal.begin(phase, locator, content_digest).await?;
                match journal.reserve_key(&ticket).await? {
                    KeyReservation::Duplicate => {
                        journal.progress.skipped += 1;
                        journal
                            .finish(
                                ticket,
                                TransferStepState::Skipped,
                                0,
                                Vec::new(),
                                None,
                                None,
                            )
                            .await?;
                        journal.checkpoint().await?;
                        continue;
                    }
                    KeyReservation::TargetConflict => {
                        let error = AppError::conflict("不同源 key 映射到同一目标");
                        journal.progress.failed += 1;
                        journal
                            .finish(
                                ticket,
                                TransferStepState::Failed,
                                0,
                                Vec::new(),
                                Some(&error),
                                None,
                            )
                            .await?;
                        return Err(error);
                    }
                    KeyReservation::Reserved => {}
                }
                if let Some(target) = target.as_deref_mut() {
                    copy_key(
                        spec,
                        &mut source,
                        target,
                        (key, mapped),
                        journal,
                        cancel,
                        ticket,
                    )
                    .await?;
                } else {
                    export_key(
                        &mut source,
                        key,
                        output
                            .as_ref()
                            .ok_or_else(|| AppError::invalid("缺少导出 writer"))?,
                        journal,
                        cancel,
                        ticket,
                    )
                    .await?;
                }
            }
            if spec.redis.keys_b64.is_empty() && cursor == 0 {
                break;
            }
        }
        if let Some(sender) = &output {
            let snapshot = output_snapshot(sender).await?;
            journal.progress.processed_bytes = snapshot.size;
            journal.progress.phase = "redis_export_publish".into();
            journal.checkpoint().await?;
            let (partial, initial) = partial_identity
                .as_ref()
                .ok_or_else(|| AppError::new("internal", "partial 身份缺失"))?;
            same_identity(initial, &snapshot)?;
            artifact_sink
                .update_transfer_artifact(
                    output_artifact.ok_or_else(|| AppError::new("internal", "partial 登记缺失"))?,
                    identity(partial, &snapshot)?,
                )
                .await?;
            let ticket = journal
                .begin(
                    "redis_export_publish",
                    TransferLocator::default(),
                    digest(&spec.output)?,
                )
                .await?;
            ticket.sink.mark_sent(true).await?;
            publication_uncertain = true;
            match output_event(sender, None).await {
                Ok(_) => {
                    journal
                        .finish(
                            ticket,
                            TransferStepState::Applied,
                            1,
                            Vec::new(),
                            None,
                            None,
                        )
                        .await?;
                    let destination = spec
                        .output
                        .as_ref()
                        .ok_or_else(|| AppError::invalid("缺少输出文件"))?
                        .path
                        .clone();
                    let path = destination.clone();
                    let published = tokio::task::spawn_blocking(move || {
                        crate::execution::data_files::output_snapshot(&path, true)
                    })
                    .await
                    .map_err(|e| AppError::new("io", e.to_string()))??;
                    if published.file_id != snapshot.file_id
                        || published.sha256 != snapshot.sha256
                        || published.size != snapshot.size
                    {
                        return Err(AppError::new(
                            "recovery_conflict",
                            "已发布文件身份或内容发生变化",
                        ));
                    }
                    if let Some(id) = output_artifact {
                        artifact_sink
                            .update_transfer_artifact(id, identity(&destination, &published)?)
                            .await?;
                        journal
                            .sink
                            .finish_transfer_artifact(id, "published")
                            .await?;
                    }
                }
                Err(error) => {
                    use crate::execution::data_files::{PublicationState, publication_state};
                    let publication = publication_state(&error);
                    publication_uncertain = publication == PublicationState::Unknown;
                    let mut error = error;
                    if publication_uncertain {
                        error.code = "outcome_unknown".into();
                        if let Some(id) = output_artifact {
                            journal
                                .sink
                                .finish_transfer_artifact(id, "outcome_unknown")
                                .await?;
                        }
                    }
                    let outcome = match publication {
                        PublicationState::NotPublished => TransferStepState::Failed,
                        PublicationState::Published => TransferStepState::Applied,
                        PublicationState::Unknown => TransferStepState::OutcomeUnknown,
                    };
                    journal
                        .finish(
                            ticket,
                            outcome,
                            u64::from(publication == PublicationState::Published),
                            Vec::new(),
                            Some(&error),
                            None,
                        )
                        .await?;
                    return Err(error);
                }
            }
        }
        Ok(())
    }
    .await;
    drop(output);
    let task_result = if let Some(task) = output_task {
        task.await
            .map_err(|e| AppError::new("io", e.to_string()))
            .and_then(|result| result)
    } else {
        Ok(())
    };
    let result = result.and(task_result);
    if result.is_err()
        && !publication_uncertain
        && let (Some(id), Some((partial, initial))) = (output_artifact, partial_identity)
    {
        let path = partial.clone();
        let snapshot = tokio::task::spawn_blocking(move || {
            crate::execution::data_files::output_snapshot(&path, true)
        })
        .await
        .map_err(|e| AppError::new("io", e.to_string()))??;
        same_identity(&initial, &snapshot)?;
        artifact_sink
            .update_transfer_artifact(id, identity(&partial, &snapshot)?)
            .await?;
        journal
            .sink
            .finish_transfer_artifact(id, "cleanup_pending")
            .await?;
    }
    result
}
