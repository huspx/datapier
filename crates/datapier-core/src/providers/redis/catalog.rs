//! Redis 目录扫描、游标过期和分页提交。

use super::{Conn, RedisSession};
use crate::{
    ExecutionSink,
    model::{AppError, Resource, ResourcePage, Result, ensure_not_cancelled},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value as Json, json};
use std::collections::{HashMap, HashSet, VecDeque};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

// 原始字节用于去重，待输出的超量 SCAN 响应保留到下一逻辑批次。
pub(super) struct KeyScan {
    cursor: u64,
    complete: bool,
    limited: bool,
    pattern: Option<String>,
    kind: Option<String>,
    seen: HashSet<Vec<u8>>,
    pending: VecDeque<(Vec<u8>, String)>,
    bytes: usize,
    pub(super) store: crate::Store,
    pub(super) execution_id: Uuid,
    last_used: std::time::Instant,
}

pub(super) async fn expire_key_scans(
    scans: &mut HashMap<Uuid, KeyScan>,
    idle: std::time::Duration,
) -> Result<()> {
    let expired: Vec<_> = scans
        .iter()
        .filter(|(_, scan)| scan.last_used.elapsed() >= idle)
        .map(|(id, _)| *id)
        .collect();
    for id in expired {
        if let Some(scan) = scans.remove(&id) {
            scan.store.release_auxiliary(id).await?;
        }
    }
    Ok(())
}

impl RedisSession {
    pub(super) async fn list(
        &mut self,
        p: &Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
        conn: &mut Conn,
    ) -> Result<Json> {
        let settings = sink.store.settings().await?;
        let budget = settings.limits.result_memory_bytes_per_client as usize;
        let response_budget = settings.limits.response_bytes as usize;
        let cursor = p.get("cursor").filter(|v| !v.is_null());
        let fresh = cursor.is_none();
        let id = if let Some(v) = cursor {
            v.as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(|| AppError::invalid("无效扫描令牌"))?
        } else {
            let id = Uuid::new_v4();
            self.scans.insert(
                id,
                KeyScan {
                    cursor: 0,
                    complete: false,
                    limited: false,
                    pattern: p.get("pattern").and_then(Json::as_str).map(str::to_owned),
                    kind: p.get("type").and_then(Json::as_str).map(str::to_owned),
                    seen: HashSet::new(),
                    pending: VecDeque::new(),
                    bytes: 0,
                    store: sink.store.clone(),
                    execution_id: sink.execution_id,
                    last_used: std::time::Instant::now(),
                },
            );
            id
        };
        let limit = p
            .get("limit")
            .and_then(Json::as_u64)
            .unwrap_or(settings.paging.redis_key_batch as u64)
            .clamp(1, 1000) as usize;
        let other_bytes: usize = self
            .scans
            .iter()
            .filter(|(key, _)| **key != id)
            .map(|(_, scan)| scan.bytes)
            .sum();
        let budget = budget.saturating_sub(other_bytes);
        let result: Result<ResourcePage> = async {
            let scan = self
                .scans
                .get_mut(&id)
                .ok_or_else(|| AppError::new("handle_expired", "扫描令牌已失效"))?;
            if p.get("pattern")
                .and_then(Json::as_str)
                .is_some_and(|s| Some(s) != scan.pattern.as_deref())
                || p.get("type")
                    .and_then(Json::as_str)
                    .is_some_and(|s| Some(s) != scan.kind.as_deref())
            {
                return Err(AppError::invalid("扫描条件与令牌不匹配"));
            }
            scan.last_used = std::time::Instant::now();
            let mut resources = Vec::with_capacity(limit);
            // 精确计算页封装及最长 TTL 数字，保证实际资源可填满预算。
            let mut size = serde_json::to_vec(&ResourcePage {
                resources: Vec::new(),
                cursor: Some(id.to_string()),
                complete: false,
                resource_limited: false,
            })?
            .len();
            loop {
                ensure_not_cancelled(cancel)?;
                while let Some((key, kind)) = scan.pending.get(resources.len()) {
                    let resource = Resource {
                        name: String::from_utf8_lossy(key).into_owned(),
                        kind: kind.clone(),
                        database: Some(self.profile.redis_db().to_string()),
                        key_b64: Some(STANDARD.encode(key)),
                        data: json!({"ttl_ms":i64::MIN}),
                    };
                    let bytes =
                        serde_json::to_vec(&resource)?.len() + usize::from(!resources.is_empty());
                    if size.saturating_add(bytes) > response_budget {
                        if resources.is_empty() {
                            return Err(AppError::new(
                                "result_too_large",
                                "单个 Redis key 超过分页字节限制",
                            ));
                        }
                        break;
                    }
                    size += bytes;
                    resources.push(resource);
                    if resources.len() == limit {
                        break;
                    }
                }
                if resources.len() == limit
                    || resources.len() < scan.pending.len()
                    || scan.complete
                    || scan.limited
                {
                    break;
                }
                let mut command = redis::cmd("SCAN");
                command.arg(scan.cursor).arg("COUNT").arg(limit);
                if let Some(pattern) = &scan.pattern {
                    command.arg("MATCH").arg(pattern);
                }
                if let Some(kind) = &scan.kind {
                    command.arg("TYPE").arg(kind);
                }
                let (next, keys): (u64, Vec<Vec<u8>>) = command.query_async(&mut *conn).await?;
                let mut types = redis::pipe();
                for key in &keys {
                    types.cmd("TYPE").arg(key);
                }
                let kinds: Vec<String> = if keys.is_empty() {
                    Vec::new()
                } else {
                    types.query_async(&mut *conn).await?
                };
                if kinds.len() != keys.len() {
                    return Err(AppError::new("database", "Redis 类型回复数量不匹配"));
                }
                let extra = keys
                    .iter()
                    .zip(&kinds)
                    .filter(|(key, kind)| {
                        kind.as_str() != "none"
                            && scan
                                .kind
                                .as_deref()
                                .is_none_or(|expected| expected == kind.as_str())
                            && !scan.seen.contains(*key)
                    })
                    .fold(0usize, |size, (key, kind)| {
                        size.saturating_add(
                            key.len()
                                .saturating_mul(2)
                                .saturating_add(kind.len())
                                .saturating_add(128),
                        )
                    });
                let reserved = scan.bytes.saturating_add(extra);
                if reserved > budget {
                    scan.limited = true;
                } else if let Err(error) = scan
                    .store
                    .reserve_auxiliary(scan.execution_id, id, reserved)
                    .await
                {
                    if error.code != "resource_limit" {
                        return Err(error);
                    }
                    scan.limited = true;
                } else {
                    scan.bytes = reserved;
                    for (key, kind) in keys.into_iter().zip(kinds) {
                        if kind == "none"
                            || scan
                                .kind
                                .as_deref()
                                .is_some_and(|expected| expected != kind)
                            || !scan.seen.insert(key.clone())
                        {
                            continue;
                        }
                        scan.pending.push_back((key, kind));
                    }
                }
                scan.cursor = next;
                scan.complete = next == 0 && !scan.limited;
            }
            if !resources.is_empty() {
                let mut ttl_query = redis::pipe();
                for (key, _) in scan.pending.iter().take(resources.len()) {
                    ttl_query.cmd("PTTL").arg(key);
                }
                let ttls: Vec<i64> = ttl_query.query_async(&mut *conn).await?;
                if ttls.len() != resources.len() {
                    return Err(AppError::new("database", "Redis TTL 回复数量不匹配"));
                }
                for (resource, ttl) in resources.iter_mut().zip(ttls) {
                    resource.data = json!({"ttl_ms":ttl});
                }
            }
            scan.pending.drain(..resources.len());
            let complete = scan.complete && scan.pending.is_empty();
            Ok(ResourcePage {
                resources,
                cursor: if complete || (scan.limited && scan.pending.is_empty()) {
                    None
                } else {
                    Some(id.to_string())
                },
                complete,
                resource_limited: scan.limited,
            })
        }
        .await;
        if ((fresh && result.is_err()) || result.as_ref().is_ok_and(|page| page.cursor.is_none()))
            && let Some(scan) = self.scans.remove(&id)
        {
            scan.store.release_auxiliary(id).await?;
        }
        Ok(serde_json::to_value(result?)?)
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/redis_scan_lifecycle.rs"]
mod tests;
