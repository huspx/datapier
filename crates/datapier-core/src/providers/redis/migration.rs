//! Redis 迁移源的独立 WATCH 会话与增量读取。
use super::*;
use crate::model::transfer::{MAX_BATCH_BYTES, MAX_BATCH_ROWS, MAX_VALUE_BYTES, RedisSelection};
use tokio::sync::mpsc;
impl RedisSession {
    /// 返回 Redis 服务端的 run_id；无法确定身份时返回错误。
    pub async fn server_identity(&self) -> Result<String> {
        let mut source = RedisMigrationSource::new(self).await?;
        source
            .instance_identity()
            .await?
            .ok_or_else(|| AppError::new("identity_unknown", "无法确定 Redis 服务端身份"))
    }
}

/// 单个 key 只有在 Verified 后才允许发布到目标。
#[derive(Debug)]
pub(crate) enum RedisMigrationChunk {
    Begin {
        kind: String,
        expires_at_ms: Option<i64>,
    },
    Rows(Vec<Vec<Value>>),
    StringPart(Vec<u8>),
    Verified,
    Expired,
    Missing,
}

/// 此连接仅供迁移源使用，WATCH 生命周期不与其他命令共用。
pub(crate) struct RedisMigrationSource {
    conn: Conn,
    db: u32,
}

impl RedisMigrationSource {
    pub(crate) async fn new(session: &RedisSession) -> Result<Self> {
        Ok(Self {
            conn: connect_database(&session.client, &session.profile, session.secret.as_deref())
                .await?,
            db: session.profile.redis_db(),
        })
    }

    pub(crate) fn database(&self) -> u32 {
        self.db
    }

    /// TIME 取样使用单调时钟衡量往返时间，不依赖客户端墙钟准确。
    pub(crate) async fn sample_time(
        &mut self,
    ) -> Result<(i64, std::time::Instant, std::time::Duration)> {
        let start = std::time::Instant::now();
        let (seconds, micros): (i64, i64) = query(&mut self.conn, &args(&["TIME"])).await?;
        let elapsed = start.elapsed();
        let millis = seconds
            .checked_mul(1000)
            .and_then(|n| n.checked_add(micros / 1000))
            .ok_or_else(|| AppError::new("database", "Redis TIME 超出范围"))?;
        Ok((millis, start + elapsed / 2, elapsed))
    }

    /// Redis run_id 随服务重启改变，缺失时不能推断实例身份。
    pub(crate) async fn instance_identity(&mut self) -> Result<Option<String>> {
        let info: String = query(&mut self.conn, &args(&["INFO", "server"])).await?;
        Ok(info
            .lines()
            .find_map(|line| line.trim_end_matches('\r').strip_prefix("run_id:"))
            .filter(|id| !id.is_empty())
            .map(str::to_owned))
    }

    /// SCAN 的 COUNT 仅为建议；过大的原生响应明确失败，不能截断游标页。
    pub(crate) async fn scan_page(
        &mut self,
        cursor: u64,
        pattern: Option<&str>,
    ) -> Result<(u64, Vec<Vec<u8>>)> {
        let mut command = vec![b"SCAN".to_vec(), cursor.to_string().into_bytes()];
        if let Some(pattern) = pattern {
            command.extend([b"MATCH".to_vec(), pattern.as_bytes().to_vec()]);
        }
        command.extend([b"COUNT".to_vec(), b"128".to_vec()]);
        let (next, keys): (u64, Vec<Vec<u8>>) = query(&mut self.conn, &command).await?;
        let size = keys
            .iter()
            .try_fold(0usize, |sum, key| sum.checked_add(key.len()))
            .ok_or_else(|| AppError::invalid("SCAN 响应过大"))?;
        if size > MAX_BATCH_BYTES
            || keys.len() > MAX_BATCH_ROWS
            || keys.iter().any(|k| k.len() > MAX_VALUE_BYTES)
        {
            return Err(AppError::invalid("SCAN 响应超过迁移内存限制"));
        }
        Ok((next, keys))
    }

    /// 内容经容量为 2 的通道发送；消费者负责先构建临时对象，再等待 Verified。
    pub(crate) async fn stream_key(
        &mut self,
        key: Vec<u8>,
        sender: mpsc::Sender<RedisMigrationChunk>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if sender.max_capacity() > 2 {
            return Err(AppError::invalid("Redis 迁移通道最多容纳两个批次"));
        }
        let result = self.stream_watched_key(key, &sender, cancel, false).await;
        if result.is_err() {
            // 出错后不允许把旧 WATCH 基线带入下一个 key。
            let _: Result<String> = query(&mut self.conn, &args(&["UNWATCH"])).await;
        }
        result
    }

    /// 文件导出按块输出 String，但与迁移共享同一 WATCH 和末尾校验。
    pub(crate) async fn stream_export_key(
        &mut self,
        key: Vec<u8>,
        sender: mpsc::Sender<RedisMigrationChunk>,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let result = self.stream_watched_key(key, &sender, cancel, true).await;
        if result.is_err() {
            let _: Result<String> = query(&mut self.conn, &args(&["UNWATCH"])).await;
        }
        result
    }

    async fn stream_watched_key(
        &mut self,
        key: Vec<u8>,
        sender: &mpsc::Sender<RedisMigrationChunk>,
        cancel: &CancellationToken,
        chunk_strings: bool,
    ) -> Result<()> {
        let _: String = query(&mut self.conn, &[b"WATCH".to_vec(), key.clone()]).await?;
        let metadata = "local k=redis.call('TYPE',KEYS[1]).ok; local p=redis.call('PTTL',KEYS[1]); if p<0 then return {k,false} end; local t=redis.call('TIME'); return {k,tostring(t[1]*1000+math.floor(t[2]/1000)+p)}";
        let (kind, expiry): (String, Option<String>) = query(
            &mut self.conn,
            &[
                b"EVAL".to_vec(),
                metadata.as_bytes().to_vec(),
                b"1".to_vec(),
                key.clone(),
            ],
        )
        .await?;
        if kind == "none" {
            let _: String = query(&mut self.conn, &args(&["UNWATCH"])).await?;
            return send(sender, RedisMigrationChunk::Missing, cancel).await;
        }
        if !matches!(kind.as_str(), "string" | "hash" | "list" | "set" | "zset") {
            return Err(AppError::invalid("不支持的 Redis 类型"));
        }
        let expires_at_ms = expiry
            .map(|v| {
                v.parse::<i64>()
                    .map_err(|e| AppError::invalid(e.to_string()))
            })
            .transpose()?;
        send(
            sender,
            RedisMigrationChunk::Begin {
                kind: kind.clone(),
                expires_at_ms,
            },
            cancel,
        )
        .await?;
        match kind.as_str() {
            "string" if chunk_strings => {
                let length: u64 = query(&mut self.conn, &[b"STRLEN".to_vec(), key.clone()]).await?;
                let mut offset = 0u64;
                // 每个非末块为 3 的倍数，可直接拼接 Base64 编码。
                const CHUNK: u64 = 48 * 1024;
                while offset < length {
                    ensure_not_cancelled(cancel)?;
                    let end = (offset + CHUNK).min(length) - 1;
                    let part: Vec<u8> = query(
                        &mut self.conn,
                        &[
                            b"GETRANGE".to_vec(),
                            key.clone(),
                            offset.to_string().into_bytes(),
                            end.to_string().into_bytes(),
                        ],
                    )
                    .await?;
                    if part.len() as u64 != end - offset + 1 {
                        return Err(AppError::conflict("源 String 在读取期间已变化"));
                    }
                    send(sender, RedisMigrationChunk::StringPart(part), cancel).await?;
                    offset = end + 1;
                }
            }
            "string" => {
                // GETRANGE 的上界在服务端生效，不能在 GET 后才检查响应大小。
                let value: Vec<u8> = query(
                    &mut self.conn,
                    &[
                        b"GETRANGE".to_vec(),
                        key.clone(),
                        b"0".to_vec(),
                        MAX_VALUE_BYTES.to_string().into_bytes(),
                    ],
                )
                .await?;
                if value.len() > MAX_VALUE_BYTES {
                    return Err(AppError::new(
                        "resource_limit",
                        "Redis String 超过迁移单值限制",
                    ));
                }
                emit_rows(sender, vec![vec![value]], &kind, 0, cancel).await?;
            }
            "list" => {
                let mut offset: u64 = 0;
                loop {
                    let end = offset;
                    let values: Vec<Vec<u8>> = query(
                        &mut self.conn,
                        &[
                            b"LRANGE".to_vec(),
                            key.clone(),
                            offset.to_string().into_bytes(),
                            end.to_string().into_bytes(),
                        ],
                    )
                    .await?;
                    if values.is_empty() {
                        break;
                    }
                    let count = values.len() as u64;
                    emit_rows(
                        sender,
                        values.into_iter().map(|v| vec![v]).collect(),
                        &kind,
                        offset,
                        cancel,
                    )
                    .await?;
                    offset = offset
                        .checked_add(count)
                        .ok_or_else(|| AppError::invalid("List 索引溢出"))?;
                }
            }
            _ => {
                let field_expiry_supported = if kind == "hash" {
                    let info: String = query(&mut self.conn, &args(&["INFO", "server"])).await?;
                    transfer::supports_field_expiry(&info)?
                } else {
                    false
                };
                let mut cursor = 0u64;
                loop {
                    let cmd = match kind.as_str() {
                        "hash" => b"HSCAN".as_slice(),
                        "set" => b"SSCAN".as_slice(),
                        _ => b"ZSCAN".as_slice(),
                    };
                    let (next, entries): (u64, Vec<Vec<u8>>) = query(
                        &mut self.conn,
                        &[
                            cmd.to_vec(),
                            key.clone(),
                            cursor.to_string().into_bytes(),
                            b"COUNT".to_vec(),
                            b"128".to_vec(),
                        ],
                    )
                    .await?;
                    let width = if kind == "set" { 1 } else { 2 };
                    if entries.len() % width != 0 {
                        return Err(AppError::new("database", "无效集合扫描响应"));
                    }
                    let mut rows = Vec::with_capacity(entries.len() / width);
                    let mut it = entries.into_iter();
                    while let Some(first) = it.next() {
                        let mut row = vec![first];
                        if width == 2 {
                            row.push(it.next().unwrap());
                        }
                        if kind == "hash" {
                            let expiry = if field_expiry_supported {
                                let values: Vec<i64> = query(
                                    &mut self.conn,
                                    &[
                                        b"HPEXPIRETIME".to_vec(),
                                        key.clone(),
                                        b"FIELDS".to_vec(),
                                        b"1".to_vec(),
                                        row[0].clone(),
                                    ],
                                )
                                .await?;
                                match values.as_slice() {
                                    [expiry] if *expiry >= -1 => *expiry,
                                    _ => {
                                        return Err(AppError::conflict(
                                            "迁移期间 Hash 字段消失或期限无效",
                                        ));
                                    }
                                }
                            } else {
                                -1
                            };
                            row.push(expiry.to_string().into_bytes());
                        }
                        rows.push(row);
                    }
                    emit_rows(sender, rows, &kind, 0, cancel).await?;
                    cursor = next;
                    if cursor == 0 {
                        break;
                    }
                }
            }
        }
        // 只读 EXEC 也能检测读取期间的修改、删除和过期；不能自动换基线重试。
        let _: String = query(&mut self.conn, &args(&["MULTI"])).await?;
        let _: String = query(&mut self.conn, &args(&["PING"])).await?;
        let result: Option<Vec<String>> = query(&mut self.conn, &args(&["EXEC"])).await?;
        match result {
            Some(values) if values.len() == 1 => {
                send(sender, RedisMigrationChunk::Verified, cancel).await
            }
            None => {
                let exists: i64 = query(&mut self.conn, &[b"EXISTS".to_vec(), key]).await?;
                if exists == 0 && source_expired(&mut self.conn, expires_at_ms).await? {
                    send(sender, RedisMigrationChunk::Expired, cancel).await
                } else {
                    Err(AppError::conflict("源 key 在读取期间已变化"))
                }
            }
            _ => Err(AppError::new("database", "无效源 EXEC 响应")),
        }
    }
}

async fn source_expired(conn: &mut Conn, expiry: Option<i64>) -> Result<bool> {
    let Some(expiry) = expiry else {
        return Ok(false);
    };
    let (seconds, micros): (i64, i64) = query(conn, &args(&["TIME"])).await?;
    Ok(expiry <= seconds.saturating_mul(1000).saturating_add(micros / 1000))
}

async fn send(
    sender: &mpsc::Sender<RedisMigrationChunk>,
    chunk: RedisMigrationChunk,
    cancel: &CancellationToken,
) -> Result<()> {
    tokio::select! {
        _ = cancel.cancelled() => Err(AppError::cancelled()),
        result = sender.send(chunk) => result.map_err(|_| AppError::cancelled()),
    }
}

async fn emit_rows(
    sender: &mpsc::Sender<RedisMigrationChunk>,
    rows: Vec<Vec<Vec<u8>>>,
    kind: &str,
    offset: u64,
    cancel: &CancellationToken,
) -> Result<()> {
    let total = rows
        .iter()
        .flatten()
        .try_fold(0usize, |n, v| n.checked_add(v.len()))
        .ok_or_else(|| AppError::invalid("Redis 响应过大"))?;
    if (rows.len() > 1 && total > MAX_BATCH_BYTES)
        || rows.len() > MAX_BATCH_ROWS
        || rows.iter().flatten().any(|v| v.len() > MAX_VALUE_BYTES)
    {
        return Err(AppError::invalid("Redis 原生响应超过迁移内存限制"));
    }
    let mut batch = Vec::new();
    let mut bytes = 0usize;
    for (index, row) in rows.into_iter().enumerate() {
        // Value::Bytes 使用 Base64；按编码后大小分批，避免通道持有超限文本。
        let size: usize = row
            .iter()
            .enumerate()
            .map(|(col, value)| {
                if (kind == "zset" && col == 1) || (kind == "hash" && col == 2) {
                    value.len()
                } else {
                    value.len().div_ceil(3) * 4
                }
            })
            .sum::<usize>()
            + if kind == "list" { 20 } else { 0 };
        if !batch.is_empty() && (bytes + size > MAX_BATCH_BYTES || batch.len() == MAX_BATCH_ROWS) {
            send(
                sender,
                RedisMigrationChunk::Rows(std::mem::take(&mut batch)),
                cancel,
            )
            .await?;
            bytes = 0;
        }
        let mut values = Vec::with_capacity(row.len() + usize::from(kind == "list"));
        if kind == "list" {
            values.push(Value::Unsigned((offset + index as u64).to_string()));
        }
        for (col, value) in row.into_iter().enumerate() {
            if kind == "zset" && col == 1 {
                values.push(Value::Float(
                    String::from_utf8(value).map_err(|e| AppError::invalid(e.to_string()))?,
                ));
            } else if kind == "hash" && col == 2 {
                values.push(Value::Integer(
                    String::from_utf8(value).map_err(|e| AppError::invalid(e.to_string()))?,
                ));
            } else {
                values.push(val(value));
            }
        }
        bytes += size;
        batch.push(values);
    }
    if !batch.is_empty() {
        send(sender, RedisMigrationChunk::Rows(batch), cancel).await?;
    }
    Ok(())
}

/// 按原始字节映射前缀；源 key 不属于所选前缀时不得悄悄改写。
pub(crate) fn map_key(selection: &RedisSelection, key: &[u8]) -> Result<Vec<u8>> {
    let source = selection
        .source_prefix_b64
        .as_ref()
        .map(|s| STANDARD.decode(s))
        .transpose()
        .map_err(|e| AppError::invalid(e.to_string()))?
        .unwrap_or_default();
    let target = selection
        .target_prefix_b64
        .as_ref()
        .map(|s| STANDARD.decode(s))
        .transpose()
        .map_err(|e| AppError::invalid(e.to_string()))?
        .unwrap_or_default();
    let suffix = key
        .strip_prefix(source.as_slice())
        .ok_or_else(|| AppError::invalid("源 key 不属于指定前缀"))?;
    let mut mapped = target;
    mapped.extend_from_slice(suffix);
    Ok(mapped)
}

/// 无法证明源目标不相交时拒绝同 DB 迁移。
pub(crate) fn validate_overlap(
    selection: &RedisSelection,
    same_instance: Option<bool>,
    same_db: bool,
) -> Result<()> {
    if !same_db || same_instance == Some(false) {
        return Ok(());
    }
    if same_instance.is_none() {
        return Err(AppError::invalid("无法确定 Redis 实例身份"));
    }
    if !selection.keys_b64.is_empty() {
        if selection.pattern.is_some() {
            return Err(AppError::invalid("显式 key 和 pattern 不能同时指定"));
        }
        let keys: Vec<Vec<u8>> = selection
            .keys_b64
            .iter()
            .map(|s| {
                STANDARD
                    .decode(s)
                    .map_err(|e| AppError::invalid(e.to_string()))
            })
            .collect::<Result<_>>()?;
        for key in &keys {
            let mapped = map_key(selection, key)?;
            if keys.contains(&mapped) {
                return Err(AppError::invalid("源目标 key 范围重叠"));
            }
        }
        return Ok(());
    }
    let source = selection
        .source_prefix_b64
        .as_ref()
        .map(|s| STANDARD.decode(s))
        .transpose()
        .map_err(|e| AppError::invalid(e.to_string()))?
        .unwrap_or_default();
    let target = selection
        .target_prefix_b64
        .as_ref()
        .map(|s| STANDARD.decode(s))
        .transpose()
        .map_err(|e| AppError::invalid(e.to_string()))?
        .unwrap_or_default();
    if source.is_empty()
        || target.is_empty()
        || source.starts_with(&target)
        || target.starts_with(&source)
    {
        return Err(AppError::invalid("无法证明源目标前缀范围不相交"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../../tests/unit/redis_migration.rs"]
mod tests;
