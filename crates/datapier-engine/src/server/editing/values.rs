//! 单值会话：绑定原始结果、编辑令牌及客户端，所有写入只产生草稿。
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::Value as TypedValue;
use datapier_core::value::{self, ValueBuffer, ValueKind};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

const CHUNK: usize = 256 * 1024;
const FIRST: usize = 64 * 1024;

enum PreviewResult {
    Image {
        png: Vec<u8>,
        width: u32,
        height: u32,
    },
    Formatted(Vec<u8>),
    Validation(Option<String>),
}

struct Session {
    owner: Uuid,
    session_id: Uuid,
    profile_id: Uuid,
    workspace: String,
    result_id: Uuid,
    row_no: u64,
    column: String,
    target: Value,
    draft_revision: u64,
    writable: bool,
    provider_params: Value,
    baseline: Value,
    baseline_epoch: u64,
    profile_epoch: Arc<std::sync::atomic::AtomicU64>,
    digest: String,
    buffer: ValueBuffer,
    max_bytes: usize,
    buffer_charge: usize,
    original_kind: ValueKind,
    reserved: usize,
    touched: Instant,
    previews: HashMap<Uuid, (u64, Vec<u8>)>,
}
struct OwnedSession {
    owner: Uuid,
    value: Arc<AsyncMutex<Session>>,
}

pub(super) struct ValueState {
    sessions: HashMap<Uuid, OwnedSession>,
    profile_epochs: HashMap<Uuid, Arc<std::sync::atomic::AtomicU64>>,
    workers: Arc<Semaphore>,
}
impl Default for ValueState {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            profile_epochs: HashMap::new(),
            workers: Arc::new(Semaphore::new(2)),
        }
    }
}

impl ValueState {
    /// 调用方持有 State 锁时检查客户端是否仍拥有值会话。
    pub(in crate::server) fn has_owner(&self, owner: Uuid) -> bool {
        self.sessions.values().any(|session| session.owner == owner)
    }
}

// 预留 IPC 外层字段空间，并按实际 JSON 摘要和 Base64 膨胀计算分页大小。
fn bounded_chunk(
    mut response: Value,
    bytes: Option<&[u8]>,
    offset: usize,
    requested: usize,
    limit: usize,
) -> Result<Value> {
    response["data_b64"] = if bytes.is_some() {
        json!("")
    } else {
        Value::Null
    };
    response["next"] = json!(u64::MAX);
    let overhead = serde_json::to_vec(&response)?.len().saturating_add(256);
    let capacity = limit.saturating_sub(overhead) / 4 * 3;
    if bytes.is_some_and(|data| offset < data.len()) && capacity == 0 {
        return Err(AppError::new("response_limit", "响应预算不足以读取值"));
    }
    if let Some(data) = bytes {
        if offset > data.len() {
            return Err(AppError::invalid("读取偏移超出值长度"));
        }
        let end = offset
            .saturating_add(requested.min(CHUNK).min(capacity))
            .min(data.len());
        response["data_b64"] = json!(STANDARD.encode(&data[offset..end]));
        response["next"] = if end < data.len() {
            json!(end)
        } else {
            Value::Null
        };
    } else {
        response["next"] = Value::Null;
    }
    Ok(response)
}

fn summary(id: Uuid, s: &Session, offset: usize, length: usize, limit: usize) -> Result<Value> {
    let total = s.buffer.len().unwrap_or(0);
    bounded_chunk(
        json!({"value_session":id,"revision":s.buffer.revision(),"kind":format!("{:?}",s.buffer.kind()).to_ascii_lowercase(),"total_bytes":total,"writable":s.writable,"is_null":s.buffer.kind()==ValueKind::Null,"offset":offset}),
        s.buffer.bytes(),
        offset,
        length,
        limit,
    )
}
fn revision(s: &Session, expected: u64) -> Result<()> {
    if s.buffer.revision() != expected {
        return Err(AppError::conflict("值 revision 已变化"));
    }
    Ok(())
}
fn writable(s: &Session) -> Result<()> {
    if !s.writable {
        return Err(AppError::new("permission_denied", "只读值不能修改或暂存"));
    }
    Ok(())
}

// serde_json 的对象和数组除序列化字节外还持有节点与哈希表空间。
fn json_memory(value: &Value) -> usize {
    match value {
        Value::String(text) => text.capacity(),
        Value::Array(items) => items.iter().fold(
            items
                .capacity()
                .saturating_mul(std::mem::size_of::<Value>()),
            |n, item| n.saturating_add(json_memory(item)),
        ),
        Value::Object(items) => items.iter().fold(0usize, |n, (key, value)| {
            n.saturating_add(key.capacity())
                .saturating_add(json_memory(value))
                .saturating_add(std::mem::size_of::<(String, Value)>() + 32)
        }),
        _ => 0,
    }
}

impl Engine {
    fn value_entry(&self, owner: Uuid, id: Uuid) -> Result<Arc<AsyncMutex<Session>>> {
        self.ensure_editing_client(owner)?;
        let state = self.state.lock().unwrap();
        let entry = state
            .editing
            .values
            .sessions
            .get(&id)
            .ok_or_else(|| AppError::new("expired", "值会话已失效"))?;
        if entry.owner != owner {
            return Err(AppError::new("permission_denied", "值会话不属于此客户端"));
        }
        Ok(entry.value.clone())
    }
    fn value_live(&self, id: Uuid, entry: &Arc<AsyncMutex<Session>>) -> Result<()> {
        if self
            .state
            .lock()
            .unwrap()
            .editing
            .values
            .sessions
            .get(&id)
            .is_none_or(|current| !Arc::ptr_eq(&current.value, entry))
        {
            return Err(AppError::new("expired", "值会话已失效"));
        }
        Ok(())
    }

    async fn value_source(&self, owner: Uuid, params: &Value) -> Result<(Uuid, Session)> {
        let id = Uuid::new_v4();
        let available = self.store.available_memory_for_owner(owner).await?;
        self.store
            .reserve_auxiliary_for_owner(owner, id, available)
            .await?;
        let result = self
            .value_source_reserved(owner, params, id, available)
            .await;
        if result.is_err() {
            self.store.release_auxiliary(id).await?;
        }
        result
    }

    async fn value_source_reserved(
        &self,
        owner: Uuid,
        params: &Value,
        id: Uuid,
        available: usize,
    ) -> Result<(Uuid, Session)> {
        let result_id: Uuid = model::param(params, "result_id")?;
        let session_id: Uuid = model::param(params, "session_id")?;
        let row_no: u64 = model::param(params, "row_no")?;
        let column: String = model::param(params, "column")?;
        let meta = self.owned_result(owner, result_id).await?;
        let task = self.store.task(meta.execution_id).await?;
        let bound = self
            .state
            .lock()
            .unwrap()
            .result_sessions
            .get(&meta.execution_id)
            .copied();
        if bound != Some(session_id) {
            return Err(AppError::new(
                "permission_denied",
                "结果不属于当前数据库会话",
            ));
        }
        let db = self
            .state
            .lock()
            .unwrap()
            .databases
            .get(&session_id)
            .cloned()
            .ok_or_else(|| AppError::new("expired", "来源会话已关闭"))?;
        if db.closing.load(Ordering::Acquire) {
            return Err(AppError::new("expired", "来源会话正在关闭"));
        }
        // 在抓取数据库基线之前固定 epoch；DDL 即使与打开会话并发也会使其过期。
        let profile_epoch = self
            .state
            .lock()
            .unwrap()
            .editing
            .values
            .profile_epochs
            .entry(db.profile.id)
            .or_default()
            .clone();
        let baseline_epoch = profile_epoch.load(Ordering::Acquire);
        let current = self.store.profile(db.profile.id).await?;
        if current.revision != db.profile.revision {
            return Err(AppError::conflict("连接设置已变化"));
        }
        let requested_write = !params
            .get("readonly")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && params.get("edit_token").is_some_and(|v| !v.is_null())
            && task.owner == owner
            && !db.profile.is_archery();
        let target = if requested_write {
            serde_json::to_value(model::param::<model::ResourceTarget>(params, "target")?)?
        } else {
            params.get("target").cloned().unwrap_or(Value::Null)
        };
        if requested_write
            && task.context.as_ref().is_none_or(|ctx| {
                !ctx.targets
                    .iter()
                    .any(|t| serde_json::to_value(t).ok().as_ref() == Some(&target))
            })
        {
            return Err(AppError::new("permission_denied", "结果与编辑目标不匹配"));
        }
        if available < 8192 {
            return Err(AppError::new("resource_limit", "结果行没有足够的内存预算"));
        }
        let page_budget = (available / 8).clamp(1024, 1_048_576);
        let page = self
            .store
            .peek_result_page(result_id, row_no, 1, page_budget)
            .await?;
        let row = page
            .rows
            .into_iter()
            .next()
            .ok_or_else(|| AppError::new("expired", "结果行已失效"))?;
        if row.len() != meta.columns.len() {
            return Err(AppError::conflict("结果列与行不匹配"));
        }
        if meta.columns.iter().filter(|c| c.name == column).count() != 1 {
            return Err(AppError::invalid("结果列名不唯一"));
        }
        let mut values = std::collections::BTreeMap::new();
        let mut source_refs = std::collections::BTreeMap::new();
        let mut source_bytes = 0usize;
        for (column, mut value) in meta.columns.iter().zip(row) {
            if let TypedValue::Large { id, bytes, .. } = value {
                source_refs.insert(column.name.clone(), id);
                let bytes = usize::try_from(bytes)
                    .map_err(|_| AppError::new("resource_limit", "结果值超过预算"))?;
                source_bytes = source_bytes.saturating_add(bytes.saturating_mul(8));
                if source_bytes > available {
                    return Err(AppError::new(
                        "resource_limit",
                        "完整结果行超过可用内存预算",
                    ));
                }
                value = self.store.read_value(id).await?;
            }
            if values.insert(column.name.clone(), value).is_some() {
                return Err(AppError::invalid("结果列名不唯一"));
            }
        }
        let value = values
            .get(&column)
            .cloned()
            .ok_or_else(|| AppError::invalid("结果值不存在"))?;
        let mut provider_params = params.clone();
        let object = provider_params
            .as_object_mut()
            .ok_or_else(|| AppError::invalid("值参数无效"))?;
        if let Some(token) = object.get("edit_token").cloned() {
            object.insert("token".into(), token);
        }
        object.insert("target".into(), target.clone());
        object.insert("row".into(), serde_json::to_value(&values)?);
        object.insert("max_bytes".into(), json!(available));
        let baseline = if requested_write {
            let cancel = self.editing_cancel(owner)?;
            let mut provider = db.session.lock().await;
            let snapshot = match &mut *provider {
                ProviderSession::Mysql(mysql) => {
                    mysql.value_snapshot(&provider_params, &cancel).await?
                }
                ProviderSession::Redis(redis) => {
                    let role = params
                        .get("role")
                        .and_then(Value::as_str)
                        .ok_or_else(|| AppError::invalid("缺少 Redis 值角色"))?;
                    let expected_column = match role {
                        "string" | "string_value" | "hash_value" | "list_value" => "value",
                        "member" | "set_member" | "zset_member" => "member",
                        _ => return Err(AppError::invalid("无效 Redis 值角色")),
                    };
                    if column != expected_column {
                        return Err(AppError::invalid("结果列与 Redis 值角色不匹配"));
                    }
                    let identity_column = match role {
                        "hash_value" => Some("field"),
                        "list_value" => Some("index"),
                        "member" | "set_member" | "zset_member" => Some("member"),
                        _ => None,
                    };
                    if let Some(name) = identity_column {
                        let actual = values
                            .get(name)
                            .ok_or_else(|| AppError::invalid("结果缺少成员身份"))?;
                        let supplied = params
                            .get(name)
                            .ok_or_else(|| AppError::invalid("缺少成员身份"))?;
                        let expected = if name == "index" {
                            match actual {
                                TypedValue::Integer(v) => json!(
                                    v.parse::<u64>()
                                        .map_err(|_| AppError::invalid("List index 无效"))?
                                ),
                                _ => return Err(AppError::invalid("List index 类型无效")),
                            }
                        } else {
                            serde_json::to_value(actual)?
                        };
                        let supplied_ref = supplied
                            .pointer("/value/id")
                            .and_then(Value::as_str)
                            .and_then(|id| Uuid::parse_str(id).ok());
                        if supplied != &expected
                            && !(supplied.get("type").and_then(Value::as_str) == Some("large")
                                && supplied_ref.is_some()
                                && supplied_ref == source_refs.get(name).copied())
                        {
                            return Err(AppError::conflict("结果成员身份不匹配"));
                        }
                        provider_params[name] = expected;
                    }
                    redis.value_snapshot(&provider_params).await?
                }
                ProviderSession::Archery(_) => {
                    return Err(AppError::new("permission_denied", "只读来源不可写"));
                }
            };
            let original: TypedValue = model::param(&snapshot, "value")?;
            if original != value {
                return Err(AppError::conflict("结果值与数据库基线不一致"));
            }
            snapshot
        } else {
            Value::Null
        };
        let max_value = self
            .store
            .settings()
            .await?
            .editing
            .max_value_bytes
            .min(available);
        let buffer = ValueBuffer::new(&value, max_value)?;
        let original_kind = buffer.kind();
        let digest = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(&target, &baseline, &value))?)
        );
        let buffer_charge = buffer.len().unwrap_or(0).saturating_mul(2);
        let reserved = json_memory(&provider_params)
            .saturating_add(json_memory(&baseline))
            .saturating_add(json_memory(&target))
            .saturating_add(buffer_charge)
            .saturating_add(4096);
        self.store
            .reserve_auxiliary_for_owner(owner, id, reserved)
            .await?;
        Ok((
            id,
            Session {
                owner,
                session_id,
                profile_id: db.profile.id,
                workspace: task.workspace_id,
                result_id,
                row_no,
                column,
                target,
                draft_revision: optional(params, "draft_revision")?.unwrap_or(0),
                writable: requested_write,
                provider_params,
                baseline_epoch,
                profile_epoch,
                baseline,
                digest,
                buffer,
                max_bytes: max_value,
                buffer_charge,
                original_kind,
                reserved,
                touched: Instant::now(),
                previews: HashMap::new(),
            },
        ))
    }

    async fn check_session_access(&self, s: &Session) -> Result<()> {
        let meta = self.owned_result(s.owner, s.result_id).await?;
        let db = {
            let state = self.state.lock().unwrap();
            if state.result_sessions.get(&meta.execution_id).copied() != Some(s.session_id) {
                return Err(AppError::conflict("来源会话已变化"));
            }
            state
                .databases
                .get(&s.session_id)
                .cloned()
                .ok_or_else(|| AppError::new("expired", "来源会话已关闭"))?
        };
        if db.closing.load(Ordering::Acquire)
            || self.store.profile(s.profile_id).await?.revision != db.profile.revision
        {
            return Err(AppError::conflict("来源连接已变化"));
        }
        let available = self.store.available_memory_for_owner(s.owner).await?;
        if available < 1024 {
            return Err(AppError::new("resource_limit", "内存不足以核对结果行"));
        }
        let budget = available.min(1_048_576);
        let handle = Uuid::new_v4();
        self.store
            .reserve_auxiliary_for_owner(s.owner, handle, budget)
            .await?;
        let page = self
            .store
            .peek_result_page(s.result_id, s.row_no, 1, budget)
            .await;
        self.store.release_auxiliary(handle).await?;
        if page?.rows.is_empty() {
            return Err(AppError::new("expired", "结果行已失效"));
        }
        Ok(())
    }

    async fn check_session_baseline(&self, s: &Session) -> Result<()> {
        if s.baseline_epoch != s.profile_epoch.load(Ordering::Acquire) {
            return Err(AppError::conflict(
                "结构已变化，值编辑基线已失效；新值仍保留",
            ));
        }
        self.check_session_access(s).await?;
        if !s.writable {
            return Ok(());
        }
        let handle = Uuid::new_v4();
        let available = self.store.available_memory_for_owner(s.owner).await?;
        if available < s.reserved.saturating_mul(2) {
            return Err(AppError::new(
                "resource_limit",
                "内存不足以重新核对完整编辑基线",
            ));
        }
        self.store
            .reserve_auxiliary_for_owner(s.owner, handle, available)
            .await?;
        let result = self.verify_session_reserved(s, available).await;
        self.store.release_auxiliary(handle).await?;
        result
    }

    async fn verify_session_reserved(&self, s: &Session, available: usize) -> Result<()> {
        let db = self.editing_profile(s.owner, s.session_id).await?;
        let mut params = s.provider_params.clone();
        let map = params
            .as_object_mut()
            .ok_or_else(|| AppError::invalid("值参数无效"))?;
        map.insert("max_bytes".into(), json!(available));
        for name in ["before", "fingerprint", "field_expires_at_ms", "score"] {
            if let Some(value) = s.baseline.get(name) {
                map.insert(name.into(), value.clone());
            }
        }
        if matches!(&*db.session.lock().await, ProviderSession::Redis(_)) {
            map.insert(
                "before".into(),
                s.baseline
                    .get("value")
                    .cloned()
                    .ok_or_else(|| AppError::invalid("缺少 Redis 基线"))?,
            );
        }
        let cancel = self.editing_cancel(s.owner)?;
        let mut provider = db.session.lock().await;
        if s.baseline_epoch != s.profile_epoch.load(Ordering::Acquire) {
            return Err(AppError::conflict(
                "结构已变化，值编辑基线已失效；新值仍保留",
            ));
        }
        match &mut *provider {
            ProviderSession::Mysql(mysql) => mysql.value_verify(&params, &cancel).await,
            ProviderSession::Redis(redis) => redis.value_verify(&params).await,
            ProviderSession::Archery(_) => {
                Err(AppError::new("permission_denied", "只读来源不可写"))
            }
        }
    }

    pub(super) async fn value_rpc(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.ensure_editing_client(owner)?;
        let response_limit = self.store.settings().await?.limits.response_bytes;
        if method == "value.open" {
            let (id, session) = self.value_source(owner, &params).await?;
            let result = summary(id, &session, 0, FIRST, response_limit)?;
            self.state.lock().unwrap().editing.values.sessions.insert(
                id,
                OwnedSession {
                    owner,
                    value: Arc::new(AsyncMutex::new(session)),
                },
            );
            return Ok(result);
        }
        let id: Uuid = model::param(&params, "value_session")?;
        let rev: u64 = model::param(&params, "revision")?;
        let direct = {
            let state = self.state.lock().unwrap();
            if state
                .editing
                .values
                .sessions
                .get(&id)
                .is_some_and(|entry| entry.owner != owner)
            {
                return Err(AppError::new("permission_denied", "值会话不属于此客户端"));
            }
            state
                .editing
                .values
                .sessions
                .get(&id)
                .map(|entry| entry.value.clone())
        };
        if direct.is_none() && method == "value.release" {
            let sessions: Vec<_> = self
                .state
                .lock()
                .unwrap()
                .editing
                .values
                .sessions
                .iter()
                .filter(|(_, entry)| entry.owner == owner)
                .map(|(&key, entry)| (key, entry.value.clone()))
                .collect();
            for (parent, entry) in sessions {
                let mut s = entry.lock().await;
                if s.owner != owner
                    || !s.previews.contains_key(&id)
                    || self.value_live(parent, &entry).is_err()
                {
                    continue;
                }
                let (parent_revision, bytes) = s
                    .previews
                    .get(&id)
                    .ok_or_else(|| AppError::new("expired", "图片预览已失效"))?;
                if *parent_revision != rev || *parent_revision != s.buffer.revision() {
                    return Err(AppError::conflict("图片预览已失效"));
                }
                let len = bytes.len();
                s.previews.remove(&id);
                s.reserved = s.reserved.saturating_sub(len);
                self.store
                    .reserve_auxiliary_for_owner(owner, parent, s.reserved)
                    .await?;
                return Ok(json!({"released":true}));
            }
            return Err(AppError::new("expired", "图片预览已失效"));
        }
        if direct.is_none() && method == "value.read" {
            let sessions: Vec<_> = self
                .state
                .lock()
                .unwrap()
                .editing
                .values
                .sessions
                .iter()
                .filter(|(_, entry)| entry.owner == owner)
                .map(|(&key, entry)| (key, entry.value.clone()))
                .collect();
            for (parent, entry) in sessions {
                let mut s = entry.lock().await;
                if s.owner != owner
                    || !s.previews.contains_key(&id)
                    || self.value_live(parent, &entry).is_err()
                {
                    continue;
                }
                self.check_session_access(&s).await?;
                s.touched = Instant::now();
                if let Some((parent_revision, bytes)) = s.previews.get(&id) {
                    if *parent_revision != s.buffer.revision() || rev != *parent_revision {
                        return Err(AppError::conflict("图片预览已失效"));
                    }
                    let offset: usize = optional(&params, "offset")?.unwrap_or(0);
                    let len: usize = optional::<usize>(&params, "max_bytes")?
                        .unwrap_or(CHUNK)
                        .clamp(1, CHUNK);
                    if offset > bytes.len() {
                        return Err(AppError::invalid("读取偏移超出预览长度"));
                    }
                    return bounded_chunk(
                        json!({"value_session":id,"revision":rev,"kind":"bytes","total_bytes":bytes.len(),"writable":false,"is_null":false,"offset":offset}),
                        Some(bytes),
                        offset,
                        len,
                        response_limit,
                    );
                }
            }
            return Err(AppError::new("expired", "值或预览会话已失效"));
        }
        let entry = direct.ok_or_else(|| AppError::new("expired", "值会话已失效"))?;
        // 与 DDL 使用同一 profile 门，基线核验到暂存结束之间不能发生结构变更。
        let gate = if method == "value.stage" {
            let profile_id = entry.lock().await.profile_id;
            Some(self.editing_gate(profile_id))
        } else {
            None
        };
        let _gate = match &gate {
            Some(gate) => Some(gate.lock().await),
            None => None,
        };
        let mut s = entry.lock().await;
        self.value_live(id, &entry)?;
        if s.owner != owner {
            return Err(AppError::new("permission_denied", "值会话不属于此客户端"));
        }
        revision(&s, rev)?;
        if method != "value.release" {
            self.check_session_access(&s).await?;
        }
        s.touched = Instant::now();
        match method {
            "value.read" => {
                let offset = optional(&params, "offset")?.unwrap_or(0);
                let length = optional::<usize>(&params, "max_bytes")?
                    .unwrap_or(CHUNK)
                    .clamp(1, CHUNK);
                summary(id, &s, offset, length, response_limit)
            }
            "value.patch" => {
                writable(&s)?;
                let offset: usize = model::param(&params, "offset")?;
                let delete: usize = model::param(&params, "delete_bytes")?;
                let end = offset
                    .checked_add(delete)
                    .ok_or_else(|| AppError::invalid("patch 范围无效"))?;
                let encoded: String = model::param(&params, "data_b64")?;
                let additional = encoded.len().saturating_mul(3) / 4;
                let previous = s.reserved;
                let possible = s.buffer.len().unwrap_or(0).saturating_add(additional);
                self.store
                    .reserve_auxiliary_for_owner(
                        owner,
                        id,
                        previous
                            .saturating_add(possible.saturating_mul(2))
                            .saturating_add(additional),
                    )
                    .await?;
                let result = (|| {
                    let data = STANDARD
                        .decode(encoded)
                        .map_err(|e| AppError::invalid(e.to_string()))?;
                    let text = optional::<bool>(&params, "text")?
                        .unwrap_or(s.buffer.kind() != ValueKind::Bytes);
                    s.buffer.patch(rev, offset..end, &data, text)?;
                    Ok::<_, AppError>(())
                })();
                if let Err(error) = result {
                    self.store
                        .reserve_auxiliary_for_owner(owner, id, previous)
                        .await?;
                    return Err(error);
                }
                let previews: usize = s.previews.values().map(|(_, data)| data.len()).sum();
                s.previews.clear();
                let charge = s
                    .buffer_charge
                    .max(s.buffer.len().unwrap_or(0).saturating_mul(2));
                s.reserved = previous
                    .saturating_sub(s.buffer_charge)
                    .saturating_add(charge)
                    .saturating_sub(previews);
                s.buffer_charge = charge;
                self.store
                    .reserve_auxiliary_for_owner(owner, id, s.reserved)
                    .await?;
                summary(id, &s, 0, FIRST, response_limit)
            }
            "value.set_null" => {
                writable(&s)?;
                if s.baseline.get("role").is_some() {
                    return Err(AppError::invalid("Redis 值不能设为 NULL"));
                }
                let previous_charge = s.buffer_charge;
                s.buffer.replace(rev, ValueKind::Null, None)?;
                let previews: usize = s.previews.values().map(|(_, data)| data.len()).sum();
                s.previews.clear();
                s.reserved = s
                    .reserved
                    .saturating_sub(previous_charge)
                    .saturating_sub(previews);
                s.buffer_charge = 0;
                self.store
                    .reserve_auxiliary_for_owner(owner, id, s.reserved)
                    .await?;
                summary(id, &s, 0, FIRST, response_limit)
            }
            "value.replace" => {
                writable(&s)?;
                let name: String = model::param(&params, "kind")?;
                let kind = match name.as_str() {
                    "bool" => ValueKind::Bool,
                    "integer" => ValueKind::Integer,
                    "unsigned" => ValueKind::Unsigned,
                    "decimal" => ValueKind::Decimal,
                    "float" => ValueKind::Float,
                    "text" => ValueKind::Text,
                    "bytes" => ValueKind::Bytes,
                    "date" => ValueKind::Date,
                    "time" => ValueKind::Time,
                    "datetime" => ValueKind::DateTime,
                    "json" => ValueKind::Json,
                    _ => return Err(AppError::invalid("无效的新值类型")),
                };
                if s.original_kind != ValueKind::Null && kind != s.original_kind {
                    return Err(AppError::invalid("新值类型与原值不一致"));
                }
                let encoded: String = model::param(&params, "data_b64")?;
                let previous = s.reserved;
                let old_charge = s.buffer_charge;
                let allowance = encoded.len().saturating_mul(3) / 4;
                let factor = if kind == ValueKind::Json { 129 } else { 5 };
                self.store
                    .reserve_auxiliary_for_owner(
                        owner,
                        id,
                        previous
                            .saturating_add(allowance.saturating_mul(factor))
                            .saturating_add(4096),
                    )
                    .await?;
                let result = (|| {
                    let data = STANDARD
                        .decode(encoded)
                        .map_err(|e| AppError::invalid(e.to_string()))?;
                    let len = data.len();
                    s.buffer.replace(rev, kind, Some(data))?;
                    Ok::<_, AppError>(len)
                })();
                let len = match result {
                    Ok(len) => len,
                    Err(error) => {
                        self.store
                            .reserve_auxiliary_for_owner(owner, id, previous)
                            .await?;
                        return Err(error);
                    }
                };
                if s.original_kind == ValueKind::Null {
                    s.original_kind = kind;
                }
                let previews: usize = s.previews.values().map(|(_, data)| data.len()).sum();
                s.previews.clear();
                s.buffer_charge = len.saturating_mul(2);
                s.reserved = previous
                    .saturating_sub(old_charge)
                    .saturating_sub(previews)
                    .saturating_add(s.buffer_charge);
                self.store
                    .reserve_auxiliary_for_owner(owner, id, s.reserved)
                    .await?;
                summary(id, &s, 0, FIRST, response_limit)
            }
            "value.preview" => {
                let mode: String = model::param(&params, "mode")?;
                if matches!(mode.as_str(), "json_format" | "xml_format") {
                    writable(&s)?;
                }
                let source = s
                    .buffer
                    .bytes()
                    .ok_or_else(|| AppError::invalid("NULL 没有可预览的内容"))?;
                let workers = self.state.lock().unwrap().editing.values.workers.clone();
                let _permit = workers
                    .acquire_owned()
                    .await
                    .map_err(|_| AppError::new("cancelled", "预览工作线程已关闭"))?;
                let budget = self.store.available_memory_for_owner(owner).await?;
                let handle = Uuid::new_v4();
                let factor = if mode == "image" { 1 } else { 128 };
                if source.len().saturating_mul(factor) > budget {
                    return Err(AppError::new("resource_limit", "预览超过可用内存预算"));
                }
                self.store
                    .reserve_auxiliary_for_owner(owner, handle, budget)
                    .await?;
                let data = source.to_vec();
                let work_mode = mode.clone();
                let work = tokio::task::spawn_blocking(move || -> Result<PreviewResult> {
                    if work_mode == "image" {
                        let p = value::preview_image(&data, budget)?;
                        return Ok(PreviewResult::Image {
                            png: p.png,
                            width: p.width,
                            height: p.height,
                        });
                    }
                    let text =
                        std::str::from_utf8(&data).map_err(|e| AppError::invalid(e.to_string()))?;
                    match work_mode.as_str() {
                        "json" | "xml" => {
                            let result = if work_mode == "json" {
                                value::validate_json(text)
                            } else {
                                value::validate_xml(text)
                            };
                            Ok(PreviewResult::Validation(result.err().map(|e| e.message)))
                        }
                        "json_format" => Ok(PreviewResult::Formatted(
                            value::format_json(text, budget)?.into_bytes(),
                        )),
                        "xml_format" => Ok(PreviewResult::Formatted(
                            value::format_xml(text, budget)?.into_bytes(),
                        )),
                        _ => Err(AppError::invalid("未知预览模式")),
                    }
                })
                .await
                .map_err(|e| AppError::new("internal", e.to_string()));
                let result = match work {
                    Ok(result) => result,
                    Err(error) => Err(error),
                };
                let result = match result {
                    Ok(v) => v,
                    Err(error) => {
                        self.store.release_auxiliary(handle).await?;
                        return Err(error);
                    }
                };
                match result {
                    PreviewResult::Image { png, width, height } => {
                        let preview = Uuid::new_v4();
                        self.store.release_auxiliary(handle).await?;
                        self.store
                            .reserve_auxiliary_for_owner(
                                owner,
                                id,
                                s.reserved.saturating_add(png.len()),
                            )
                            .await?;
                        s.reserved += png.len();
                        let total = png.len();
                        s.previews.insert(preview, (rev, png));
                        Ok(
                            json!({"preview_id":preview,"revision":rev,"mime":"image/png","total_bytes":total,"width":width,"height":height}),
                        )
                    }
                    PreviewResult::Formatted(output) => {
                        let new = output.len();
                        let kind = s.buffer.kind();
                        if new > s.max_bytes {
                            self.store.release_auxiliary(handle).await?;
                            return Err(AppError::new(
                                "resource_limit",
                                "格式化后的值超过单值限制",
                            ));
                        }
                        if let Err(error) = value::from_bytes(kind, Some(&output)) {
                            self.store.release_auxiliary(handle).await?;
                            return Err(error);
                        }
                        let previews: usize = s.previews.values().map(|(_, data)| data.len()).sum();
                        let next_reserved = s
                            .reserved
                            .saturating_sub(s.buffer_charge)
                            .saturating_sub(previews)
                            .saturating_add(new.saturating_mul(2));
                        self.store.release_auxiliary(handle).await?;
                        self.store
                            .reserve_auxiliary_for_owner(owner, id, next_reserved)
                            .await?;
                        if let Err(error) = s.buffer.replace(rev, kind, Some(output)) {
                            self.store
                                .reserve_auxiliary_for_owner(owner, id, s.reserved)
                                .await?;
                            return Err(error);
                        }
                        s.previews.clear();
                        s.reserved = next_reserved;
                        s.buffer_charge = new.saturating_mul(2);
                        summary(id, &s, 0, FIRST, response_limit)
                    }
                    PreviewResult::Validation(validation) => {
                        self.store.release_auxiliary(handle).await?;
                        let response = json!({"valid":validation.is_none(),"errors":validation.into_iter().collect::<Vec<_>>()});
                        if serde_json::to_vec(&response)?.len().saturating_add(128) > response_limit
                        {
                            return Err(AppError::new(
                                "response_limit",
                                "校验结果超过响应字节限制",
                            ));
                        }
                        Ok(response)
                    }
                }
            }
            "value.stage" => self.stage_value(id, &mut s, &params).await,
            "value.release" => {
                self.state
                    .lock()
                    .unwrap()
                    .editing
                    .values
                    .sessions
                    .remove(&id);
                self.store.release_auxiliary(id).await?;
                Ok(json!({"released":true}))
            }
            _ => Err(AppError::new("method_not_found", "未知值操作")),
        }
    }
    /// DDL 使同一连接的旧基线失效，但保留用户编辑的缓冲以供恢复。
    pub(in crate::server) async fn editing_invalidate_value_baselines(&self, profile_id: Uuid) {
        let epoch = self
            .state
            .lock()
            .unwrap()
            .editing
            .values
            .profile_epochs
            .entry(profile_id)
            .or_default()
            .clone();
        epoch.fetch_add(1, Ordering::AcqRel);
    }

    async fn stage_identity(
        &self,
        s: &Session,
        identity: &Value,
    ) -> Result<(Value, Option<Value>)> {
        if json_memory(identity).saturating_mul(2).saturating_add(768)
            <= self.store.settings().await?.limits.response_bytes
        {
            return Ok((identity.clone(), None));
        }
        let original_type = identity
            .get("type")
            .and_then(Value::as_str)
            .filter(|kind| matches!(*kind, "bytes" | "text"))
            .ok_or_else(|| AppError::invalid("成员定位必须是文本或字节"))?;
        let bytes = serde_json::to_vec(identity)?;
        let export = ValueFileExport {
            profile_id: s.profile_id,
            workspace_id: s.workspace.clone(),
            target: s.target.clone(),
            baseline_digest: s.digest.clone(),
            bytes,
        };
        let record = self
            .editing_publish_draft(s.owner, s.draft_revision, "intent", &export)
            .await?;
        let value = TypedValue::Large {
            id: record.id,
            original_type: original_type.into(),
            bytes: export.bytes.len() as u64,
            preview: String::new(),
        };
        Ok((
            serde_json::to_value(value)?,
            Some(json!({"edit_file_id":record.id,"edit_file_revision":record.revision})),
        ))
    }

    async fn stage_value(&self, id: Uuid, s: &mut Session, params: &Value) -> Result<Value> {
        writable(s)?;
        let draft: u64 = model::param(params, "draft_revision")?;
        if draft != s.draft_revision {
            return Err(AppError::conflict("父草稿已变化，请重新打开值"));
        }
        self.check_session_baseline(s).await?;
        let handle = Uuid::new_v4();
        let available = self.store.available_memory_for_owner(s.owner).await?;
        let factor = if s.buffer.kind() == ValueKind::Json {
            128
        } else {
            8
        };
        let role = s.baseline.get("role").and_then(Value::as_str).unwrap_or("");
        let identity_bytes = match role {
            "hash_value" => s.provider_params.get("field"),
            "set_member" | "zset_member" => s.provider_params.get("member"),
            _ => None,
        }
        .map_or(0, json_memory);
        if s.buffer
            .len()
            .unwrap_or(0)
            .saturating_mul(factor)
            .saturating_add(identity_bytes.saturating_mul(4))
            .saturating_add(4096)
            > available
        {
            return Err(AppError::new("resource_limit", "暂存值超过可用内存预算"));
        }
        let response_limit = self.store.settings().await?.limits.response_bytes;
        self.store
            .reserve_auxiliary_for_owner(s.owner, handle, available)
            .await?;
        let staged = async {
            let typed = s.buffer.value()?;
            if matches!(typed, TypedValue::Missing | TypedValue::Large { .. }) { return Err(AppError::invalid("该类型不能作为新值")); }
            let type_name = match &typed {
                TypedValue::Null => "null", TypedValue::Bool(_) => "bool", TypedValue::Integer(_) => "integer",
                TypedValue::Unsigned(_) => "unsigned", TypedValue::Decimal(_) => "decimal", TypedValue::Float(_) => "float",
                TypedValue::Text(_) => "text", TypedValue::Bytes(_) => "bytes", TypedValue::Date(_) => "date",
                TypedValue::Time(_) => "time", TypedValue::DateTime(_) => "date_time", TypedValue::Json(_) => "json",
                _ => return Err(AppError::invalid("该类型不能作为新值")),
            };
            let role = s.baseline.get("role").and_then(Value::as_str).unwrap_or("");
            let same_identity = if matches!(role, "set_member" | "zset_member") {
                s.provider_params.get("member").map(|v| serde_json::from_value::<TypedValue>(v.clone())).transpose()?.as_ref() == Some(&typed)
            } else { false };
            if same_identity { return Ok(json!({"value_session":id,"revision":s.buffer.revision(),"draft_revision":s.draft_revision,"result_id":s.result_id,"row_no":s.row_no,"column":s.column,"target":s.target,"redis_changes":[]})); }
            let db = self.editing_profile(s.owner, s.session_id).await?;
            if let ProviderSession::Mysql(mysql) = &*db.session.lock().await { mysql.value_validate(&s.baseline, &s.column, &typed)?; }
            if matches!(role, "set_member" | "zset_member") {
                let db = self.editing_profile(s.owner, s.session_id).await?;
                let mut provider = db.session.lock().await;
                match &mut *provider {
                    ProviderSession::Redis(redis) => redis.value_check_replacement(&s.provider_params, &typed).await?,
                    _ => return Err(AppError::conflict("成员来源已变化")),
                }
            }
            let encoded = serde_json::to_vec(&typed)?;
            let mut file = None;
            let staged_value = if encoded.len().saturating_mul(2).saturating_add(768) > response_limit.min(CHUNK) {
                let export = ValueFileExport { profile_id:s.profile_id, workspace_id:s.workspace.clone(), target:s.target.clone(), baseline_digest:s.digest.clone(), bytes:encoded };
                let record = self.editing_publish_draft(s.owner, s.draft_revision, "json", &export).await?;
                file = Some((record.id, record.revision));
                TypedValue::Large { id:record.id, bytes:export.bytes.len() as u64, preview:String::new(), original_type:type_name.into() }
            } else { typed };
            let mut result = json!({"value_session":id,"revision":s.buffer.revision(),"draft_revision":s.draft_revision,"result_id":s.result_id,"row_no":s.row_no,"column":s.column,"target":s.target,"value":staged_value});
            if let Some((file_id, file_revision)) = file { result["edit_file_id"] = json!(file_id); result["edit_file_revision"] = json!(file_revision); }
            if !role.is_empty() {
                let value = result["value"].clone();
                let identity_name = if role == "hash_value" { Some("field") } else if matches!(role, "set_member" | "zset_member") { Some("member") } else { None };
                let (identity, identity_file) = match identity_name {
                    Some(name) => self.stage_identity(s, s.provider_params.get(name).ok_or_else(|| AppError::invalid("缺少成员身份"))?).await?,
                    None => (Value::Null, None),
                };
                let mut changes = match role {
                    "string" => vec![json!({"operation":"set_string","value":value})],
                    "hash_value" => vec![json!({"operation":"hash_set","field":identity,"value":value,"preserve_field_ttl":true,"field_expires_at_ms":s.baseline["field_expires_at_ms"]})],
                    "list_value" => vec![json!({"operation":"list_set","index":s.provider_params["index"],"value":value})],
                    "set_member" => vec![json!({"operation":"set_replace_member","member":identity,"new_member":value})],
                    "zset_member" => vec![json!({"operation":"zset_replace_member","member":identity,"new_member":value,"preserve_score":true})],
                    _ => return Err(AppError::invalid("不支持的 Redis 值角色")),
                };
                if let Some(identity_file) = identity_file { changes[0]["identity_edit_file"] = identity_file; }
                if let Some((file_id, file_revision)) = file && let Some(change) = changes.first_mut() {
                    change["edit_file_id"] = json!(file_id); change["edit_file_revision"] = json!(file_revision);
                }
                result["redis_changes"] = json!(changes);
            }
            if serde_json::to_vec(&result)?.len().saturating_add(128) > response_limit {
                return Err(AppError::new("response_limit", "暂存结果超过响应字节限制"));
            }
            Ok(result)
        }.await;
        self.store.release_auxiliary(handle).await?;
        staged
    }

    pub(super) async fn editing_check_value_baseline(&self, owner: Uuid, id: Uuid) -> Result<()> {
        let entry = self.value_entry(owner, id)?;
        let s = entry.lock().await;
        self.value_live(id, &entry)?;
        if s.owner != owner {
            return Err(AppError::new("permission_denied", "值会话不属于此客户端"));
        }
        self.check_session_baseline(&s).await
    }

    /// 返回已验证的完整可见行和可靠键；调用者完成后释放返回的预算句柄。
    pub(in crate::server) async fn editing_staged_mysql_baseline(
        &self,
        owner: Uuid,
        id: Uuid,
        rev: u64,
        expected_session_id: Uuid,
        expected_target: &Value,
        expected_column: &str,
    ) -> Result<(Value, Value, Uuid)> {
        let entry = self.value_entry(owner, id)?;
        let s = entry.lock().await;
        self.value_live(id, &entry)?;
        if s.owner != owner {
            return Err(AppError::new("permission_denied", "值会话不属于此客户端"));
        }
        let target: model::ResourceTarget = serde_json::from_value(expected_target.clone())?;
        if s.session_id != expected_session_id
            || s.target != serde_json::to_value(target)?
            || s.column != expected_column
        {
            return Err(AppError::conflict("值基线与提交目标不匹配"));
        }
        revision(&s, rev)?;
        writable(&s)?;
        if s.baseline.get("role").is_some() {
            return Err(AppError::invalid("值会话不是 MySQL 来源"));
        }
        self.check_session_baseline(&s).await?;
        let before = s
            .baseline
            .get("before")
            .ok_or_else(|| AppError::conflict("缺少完整行基线"))?;
        let key = s
            .baseline
            .get("key")
            .ok_or_else(|| AppError::conflict("缺少可靠键"))?;
        let handle = Uuid::new_v4();
        self.store
            .reserve_auxiliary_for_owner(owner, handle, s.reserved)
            .await?;
        Ok((before.clone(), key.clone(), handle))
    }

    pub(super) async fn editing_export_value(
        &self,
        owner: Uuid,
        id: Uuid,
        rev: u64,
    ) -> Result<ValueFileExport> {
        let entry = self.value_entry(owner, id)?;
        let s = entry.lock().await;
        self.value_live(id, &entry)?;
        if s.owner != owner {
            return Err(AppError::new("permission_denied", "值会话不属于此客户端"));
        }
        revision(&s, rev)?;
        self.check_session_baseline(&s).await?;
        let bytes = s
            .buffer
            .bytes()
            .ok_or_else(|| AppError::invalid("NULL 没有可导出的字节"))?
            .to_vec();
        Ok(ValueFileExport {
            profile_id: s.profile_id,
            workspace_id: s.workspace.clone(),
            target: s.target.clone(),
            bytes,
            baseline_digest: s.digest.clone(),
        })
    }

    pub(super) async fn editing_import_value(
        &self,
        owner: Uuid,
        id: Uuid,
        rev: u64,
        bytes: Vec<u8>,
    ) -> Result<Value> {
        let entry = self.value_entry(owner, id)?;
        let profile_id = entry.lock().await.profile_id;
        let gate = self.editing_gate(profile_id);
        let _gate = gate.lock().await;
        let mut s = entry.lock().await;
        self.value_live(id, &entry)?;
        if s.owner != owner {
            return Err(AppError::new("permission_denied", "值会话不属于此客户端"));
        }
        revision(&s, rev)?;
        writable(&s)?;
        self.check_session_baseline(&s).await?;
        let kind = s.original_kind;
        if matches!(kind, ValueKind::Null | ValueKind::Missing) {
            return Err(AppError::invalid("NULL 需要先明确选择新值类型"));
        }
        let new = bytes.len();
        if new > s.max_bytes {
            return Err(AppError::new("resource_limit", "导入值超过单值限制"));
        }
        let previous = s.reserved;
        let validation = Uuid::new_v4();
        let factor = if kind == ValueKind::Json { 128 } else { 4 };
        self.store
            .reserve_auxiliary_for_owner(
                owner,
                validation,
                new.saturating_mul(factor).saturating_add(4096),
            )
            .await?;
        if let Err(error) = self
            .store
            .reserve_auxiliary_for_owner(owner, id, previous.saturating_add(new.saturating_mul(2)))
            .await
        {
            self.store.release_auxiliary(validation).await?;
            return Err(error);
        }
        if let Err(error) = s.buffer.replace(rev, kind, Some(bytes)) {
            self.store
                .reserve_auxiliary_for_owner(owner, id, previous)
                .await?;
            self.store.release_auxiliary(validation).await?;
            return Err(error);
        }
        let previews: usize = s.previews.values().map(|(_, data)| data.len()).sum();
        s.previews.clear();
        s.reserved = previous
            .saturating_sub(s.buffer_charge)
            .saturating_sub(previews)
            .saturating_add(new.saturating_mul(2));
        s.buffer_charge = new.saturating_mul(2);
        self.store
            .reserve_auxiliary_for_owner(owner, id, s.reserved)
            .await?;
        self.store.release_auxiliary(validation).await?;
        s.touched = Instant::now();
        summary(
            id,
            &s,
            0,
            FIRST,
            self.store.settings().await?.limits.response_bytes,
        )
    }

    /// 客户端断开时释放其全部值缓冲和派生预览。
    pub(in crate::server) async fn editing_disconnect_values(&self, owner: Uuid) -> Result<()> {
        let entries: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .editing
            .values
            .sessions
            .iter()
            .filter(|(_, entry)| entry.owner == owner)
            .map(|(&id, entry)| (id, entry.value.clone()))
            .collect();
        for (id, entry) in entries {
            let s = entry.lock().await;
            if s.owner == owner {
                self.state
                    .lock()
                    .unwrap()
                    .editing
                    .values
                    .sessions
                    .remove(&id);
                self.store.release_auxiliary(id).await?;
            }
        }
        Ok(())
    }

    /// 回收空闲值会话；正在使用的会话不被定时清理中断。
    pub(in crate::server) async fn editing_expire_values(&self, idle: Duration) -> Result<()> {
        let entries: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .editing
            .values
            .sessions
            .iter()
            .map(|(&id, entry)| (id, entry.value.clone()))
            .collect();
        for (id, entry) in entries {
            if let Ok(s) = entry.try_lock()
                && s.touched.elapsed() >= idle
            {
                self.state
                    .lock()
                    .unwrap()
                    .editing
                    .values
                    .sessions
                    .remove(&id);
                self.store.release_auxiliary(id).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/server/editing_values.rs"]
mod tests;
