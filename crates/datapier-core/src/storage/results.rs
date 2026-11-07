use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;
mod editing;

pub(super) struct ResultStore {
    entries: HashMap<Uuid, MemoryResult>,
    auxiliary: HashMap<Uuid, (Uuid, usize)>,
    values: HashMap<Uuid, (Uuid, Vec<u8>)>,
    next_sequence: u64,
    activity: watch::Sender<u64>,
}
struct MemoryResult {
    meta: ResultMeta,
    sequence: u64,
    owner: Uuid,
    base: u64,
    rows: Vec<Vec<Value>>,
    bytes: usize,
    demand: u64,
    limited: bool,
    touched: std::time::Instant,
    notify: Arc<Notify>,
    wait: Arc<WaitState>,
}
#[derive(Default)]
struct WaitState {
    waiting: AtomicBool,
    completed: AtomicU64,
}
struct WaitGuard {
    state: Arc<WaitState>,
    activity: watch::Sender<u64>,
}
impl Drop for WaitGuard {
    fn drop(&mut self) {
        self.state.completed.fetch_add(1, Ordering::SeqCst);
        self.state.waiting.store(false, Ordering::SeqCst);
        self.activity
            .send_modify(|version| *version = version.wrapping_add(1));
    }
}
impl Default for ResultStore {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            auxiliary: HashMap::new(),
            values: HashMap::new(),
            next_sequence: 0,
            activity: watch::channel(0).0,
        }
    }
}
fn decoded_value_memory_bound(raw: &[u8]) -> usize {
    let mut estimate = crate::model::editing::JsonMemoryEstimate::default();
    estimate.feed(raw);
    raw.len().saturating_add(estimate.heap_bound())
}

impl ResultStore {
    fn expire(&mut self, idle: Duration) {
        let ids: Vec<Uuid> = self
            .entries
            .iter()
            .filter(|(_, r)| r.touched.elapsed() >= idle)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.remove(id);
        }
    }
    /// 调用方持有结果锁期间，返回尚未过期的执行 ID。
    pub(super) fn live_execution_ids(&mut self, idle: Duration) -> std::collections::HashSet<Uuid> {
        self.expire(idle);
        self.entries
            .values()
            .map(|entry| entry.meta.execution_id)
            .collect()
    }
    fn remove(&mut self, id: Uuid) {
        if let Some(entry) = self.entries.remove(&id) {
            entry.notify.notify_waiters();
        }
        self.values.retain(|_, (result, _)| *result != id);
    }
}
fn missing() -> AppError {
    AppError::new("not_found", "结果不存在或已释放")
}
impl Store {
    pub async fn start_result(
        &self,
        execution_id: Uuid,
        columns: Vec<Column>,
        kind: &str,
    ) -> Result<Uuid> {
        let settings = self.settings().await?;
        // 与历史清理采用同一锁顺序，避免清理在取得 owner 后删除执行记录。
        let id = Uuid::new_v4();
        let meta = ResultMeta {
            id,
            execution_id,
            columns,
            kind: kind.to_owned(),
            row_count: 0,
            complete: false,
            summary: json!({}),
        };
        let bytes = serde_json::to_vec(&meta)?.len();
        let mut state = self.results.lock().await;
        state.expire(Duration::from_secs(settings.limits.result_idle_seconds));
        let owner: Option<String> = sqlx::query_scalar("SELECT owner FROM executions WHERE id=?")
            .bind(execution_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        let owner = parse_id(&owner.ok_or_else(|| AppError::new("not_found", "任务不存在"))?)?;
        let used: usize = state
            .entries
            .values()
            .filter(|r| r.owner == owner)
            .map(|r| r.bytes)
            .sum::<usize>()
            + state
                .auxiliary
                .values()
                .filter(|(o, _)| *o == owner)
                .map(|(_, n)| *n)
                .sum::<usize>();
        if used.saturating_add(bytes) > settings.limits.result_memory_bytes_per_client {
            return Err(AppError::new("resource_limit", "结果内存预算已用尽"));
        }
        let sequence = state.next_sequence;
        state.next_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| AppError::new("resource_limit", "结果编号溢出"))?;
        state.entries.insert(
            id,
            MemoryResult {
                meta,
                owner,
                sequence,
                base: 0,
                rows: Vec::new(),
                bytes,
                demand: if kind == "redis" {
                    settings.paging.redis_value_rows as u64
                } else {
                    settings.paging.sql_rows as u64
                },
                limited: false,
                touched: std::time::Instant::now(),
                notify: Arc::new(Notify::new()),
                wait: Arc::new(WaitState::default()),
            },
        );
        Ok(id)
    }
    pub async fn append_rows(&self, id: Uuid, rows: Vec<Vec<Value>>) -> Result<()> {
        let settings = self.settings().await?;
        let mut state = self.results.lock().await;
        let owner = state.entries.get(&id).ok_or_else(missing)?.owner;
        let mut used: usize = state
            .entries
            .values()
            .filter(|r| r.owner == owner)
            .map(|r| r.bytes)
            .sum::<usize>()
            + state
                .auxiliary
                .values()
                .filter(|(o, _)| *o == owner)
                .map(|(_, n)| *n)
                .sum::<usize>();
        for mut row in rows {
            let mut additions = Vec::new();
            for value in &mut row {
                let raw = serde_json::to_vec(value)?;
                if raw.len() > settings.limits.value_preview_bytes
                    && !matches!(value, Value::Large { .. })
                {
                    let value_id = Uuid::new_v4();
                    let preview = value_preview(value, &raw, settings.limits.value_preview_bytes);
                    let original_type = value_type(value).to_owned();
                    let bytes = raw.len() as u64;
                    additions.push((value_id, raw));
                    *value = Value::Large {
                        id: value_id,
                        original_type,
                        bytes,
                        preview,
                    };
                }
            }
            let size = serde_json::to_vec(&row)?
                .len()
                .saturating_add(additions.iter().map(|(_, v)| v.len()).sum::<usize>());
            if used.saturating_add(size) > settings.limits.result_memory_bytes_per_client {
                let result = state.entries.get_mut(&id).ok_or_else(missing)?;
                result.limited = true;
                result.meta.summary["resource_limited"] = json!(true);
                result.notify.notify_waiters();
                return Err(AppError::new("resource_limit", "结果内存预算已用尽"));
            }
            for (value_id, raw) in additions {
                state.values.insert(value_id, (id, raw));
            }
            let result = state.entries.get_mut(&id).ok_or_else(missing)?;
            result.rows.push(row);
            result.bytes += size;
            result.meta.row_count = result.base + result.rows.len() as u64;
            result.touched = std::time::Instant::now();
            result.notify.notify_waiters();
            used += size;
        }
        Ok(())
    }
    pub async fn finish_result(&self, id: Uuid, summary: Json) -> Result<()> {
        let mut state = self.results.lock().await;
        let result = state.entries.get_mut(&id).ok_or_else(missing)?;
        merge_summary(&mut result.meta.summary, summary);
        result.meta.complete = true;
        result.touched = std::time::Instant::now();
        result.notify.notify_waiters();
        Ok(())
    }
    pub async fn update_result_summary(&self, id: Uuid, summary: Json) -> Result<()> {
        let mut state = self.results.lock().await;
        let result = state.entries.get_mut(&id).ok_or_else(missing)?;
        merge_summary(&mut result.meta.summary, summary);
        result.notify.notify_waiters();
        Ok(())
    }
    pub async fn peek_result_meta(&self, id: Uuid) -> Result<ResultMeta> {
        let idle = self.settings().await?.limits.result_idle_seconds;
        let mut state = self.results.lock().await;
        state.expire(Duration::from_secs(idle));
        Ok(state.entries.get(&id).ok_or_else(missing)?.meta.clone())
    }
    pub async fn result_meta(&self, id: Uuid) -> Result<ResultMeta> {
        let idle = self.settings().await?.limits.result_idle_seconds;
        let mut state = self.results.lock().await;
        state.expire(Duration::from_secs(idle));
        let result = state.entries.get_mut(&id).ok_or_else(missing)?;
        result.touched = std::time::Instant::now();
        Ok(result.meta.clone())
    }
    pub async fn result_ids(&self, task: Uuid) -> Result<Vec<Uuid>> {
        let idle = self.settings().await?.limits.result_idle_seconds;
        let mut state = self.results.lock().await;
        state.expire(Duration::from_secs(idle));
        let mut entries: Vec<_> = state
            .entries
            .values()
            .filter(|r| r.meta.execution_id == task)
            .collect();
        entries.sort_by_key(|r| r.sequence);
        Ok(entries.into_iter().map(|r| r.meta.id).collect())
    }
    pub(super) async fn result_summaries(&self, task: Uuid) -> Result<Vec<TaskResultSummary>> {
        let idle = self.settings().await?.limits.result_idle_seconds;
        let mut state = self.results.lock().await;
        state.expire(Duration::from_secs(idle));
        let mut entries: Vec<_> = state
            .entries
            .values()
            .filter(|r| r.meta.execution_id == task)
            .collect();
        entries.sort_by_key(|r| r.sequence);
        Ok(entries
            .into_iter()
            .map(|r| {
                let meta = &r.meta;
                TaskResultSummary {
                    id: meta.id,
                    row_count: meta.row_count,
                    complete: meta.complete,
                    kind: meta.kind.clone(),
                    column_count: (meta.kind != "columns_unknown")
                        .then_some(meta.columns.len() as u64),
                    redis_type: meta
                        .summary
                        .get("redis_type")
                        .and_then(Json::as_str)
                        .map(str::to_owned),
                }
            })
            .collect())
    }
    pub async fn result_page(
        &self,
        id: Uuid,
        start: u64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<ResultPage> {
        self.result_page_inner(id, start, max_rows, max_bytes, true)
            .await
    }
    pub async fn peek_result_page(
        &self,
        id: Uuid,
        start: u64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<ResultPage> {
        self.result_page_inner(id, start, max_rows, max_bytes, false)
            .await
    }
    async fn result_page_inner(
        &self,
        id: Uuid,
        start: u64,
        max_rows: usize,
        max_bytes: usize,
        touch: bool,
    ) -> Result<ResultPage> {
        if !(1..=1000).contains(&max_rows) || !(1024..=1_048_576).contains(&max_bytes) {
            return Err(AppError::invalid("分页预算无效"));
        }
        let idle = self.settings().await?.limits.result_idle_seconds;
        let mut state = self.results.lock().await;
        state.expire(Duration::from_secs(idle));
        let result = state.entries.get_mut(&id).ok_or_else(missing)?;
        if start < result.base || start > result.meta.row_count {
            return Err(AppError::invalid("结果页已失效或超出范围"));
        }
        let mut size = serde_json::to_vec(&result.meta)?.len();
        let mut rows = Vec::new();
        for row in result
            .rows
            .iter()
            .skip((start - result.base) as usize)
            .take(max_rows)
        {
            let mut projected = row.clone();
            let mut n = serde_json::to_vec(&projected)?.len() + 1;
            if size + n > max_bytes && rows.is_empty() {
                for value in &mut projected {
                    if let Value::Large { preview, .. } = value {
                        preview.clear();
                    }
                }
                n = serde_json::to_vec(&projected)?.len() + 1;
            }
            if size + n > max_bytes {
                if rows.is_empty() {
                    return Err(AppError::new("result_too_wide", "结果行超过响应预算"));
                }
                break;
            }
            size += n;
            rows.push(projected);
        }
        let end = start + rows.len() as u64;
        let next = (end < result.meta.row_count
            || (end > start
                && result.meta.summary.get("has_more").and_then(Json::as_bool) == Some(true)))
        .then_some(end);
        if touch {
            result.touched = std::time::Instant::now();
        }
        Ok(ResultPage {
            result: result.meta.clone(),
            rows,
            start,
            next,
        })
    }
    pub async fn result_pagination(
        &self,
        id: Uuid,
        page_size: usize,
        max_bytes: usize,
    ) -> Result<ResultPagination> {
        let meta = self.result_meta(id).await?;
        let base = {
            let s = self.results.lock().await;
            s.entries.get(&id).ok_or_else(missing)?.base
        };
        let mut starts = vec![base];
        let mut current = base;
        while current < meta.row_count {
            let page = self.result_page(id, current, page_size, max_bytes).await?;
            match page.next {
                Some(next) if next > current => {
                    starts.push(next);
                    current = next;
                }
                _ => break,
            }
        }
        Ok(ResultPagination {
            starts,
            row_count: meta.row_count,
            complete: meta.complete,
            page_size: page_size as u64,
        })
    }
    pub async fn set_result_demand(&self, id: Uuid, through: u64) -> Result<()> {
        let idle = self.settings().await?.limits.result_idle_seconds;
        let mut state = self.results.lock().await;
        state.expire(Duration::from_secs(idle));
        let r = state.entries.get_mut(&id).ok_or_else(missing)?;
        r.demand = r.demand.max(through);
        r.touched = std::time::Instant::now();
        r.notify.notify_waiters();
        Ok(())
    }
    /// 订阅结果读取状态变化，用于暂停执行超时计时。
    pub async fn subscribe_result_activity(&self) -> watch::Receiver<u64> {
        self.results.lock().await.activity.subscribe()
    }
    /// 返回该执行是否正在等待消费者，以及已结束的等待次数。
    pub async fn execution_wait_state(&self, execution_id: Uuid) -> (bool, u64) {
        let state = self.results.lock().await;
        let mut waiting = false;
        let mut completed = 0u64;
        for result in state
            .entries
            .values()
            .filter(|r| r.meta.execution_id == execution_id)
        {
            waiting |= result.wait.waiting.load(Ordering::SeqCst);
            completed = completed.wrapping_add(result.wait.completed.load(Ordering::SeqCst));
        }
        (waiting, completed)
    }
    pub async fn wait_result_demand(
        &self,
        id: Uuid,
        produced: u64,
        cancel: &CancellationToken,
    ) -> Result<bool> {
        let mut wait_guard: Option<WaitGuard> = None;
        loop {
            let notify = {
                let state = self.results.lock().await;
                match state.entries.get(&id) {
                    Some(r) if !r.limited => r.notify.clone(),
                    _ => return Ok(false),
                }
            };
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let idle = self.settings().await?.limits.result_idle_seconds;
            {
                let mut state = self.results.lock().await;
                match state.entries.get(&id) {
                    None => return Ok(false),
                    Some(r) if r.limited => return Ok(false),
                    Some(r) if produced < r.demand => return Ok(true),
                    Some(r) if r.touched.elapsed() >= Duration::from_secs(idle) => {
                        state.remove(id);
                        return Ok(false);
                    }
                    Some(r) => {
                        if wait_guard.is_none() {
                            r.wait.waiting.store(true, Ordering::SeqCst);
                            state
                                .activity
                                .send_modify(|version| *version = version.wrapping_add(1));
                            wait_guard = Some(WaitGuard {
                                state: r.wait.clone(),
                                activity: state.activity.clone(),
                            });
                        }
                    }
                }
            }
            tokio::select! { _ = cancel.cancelled() => return Ok(false), _ = &mut notified => {}, _ = tokio::time::sleep(Duration::from_secs(idle)) => {} }
        }
    }
    pub async fn value_result_id(&self, id: Uuid) -> Result<Uuid> {
        let idle = self.settings().await?.limits.result_idle_seconds;
        let mut state = self.results.lock().await;
        state.expire(Duration::from_secs(idle));
        Ok(state.values.get(&id).ok_or_else(missing)?.0)
    }
    pub async fn wait_result_rows(&self, id: Uuid, through: u64) -> Result<()> {
        loop {
            let notify = {
                let state = self.results.lock().await;
                state.entries.get(&id).ok_or_else(missing)?.notify.clone()
            };
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.results.lock().await;
                let r = state.entries.get(&id).ok_or_else(missing)?;
                if r.meta.row_count >= through || r.meta.complete || r.limited {
                    return Ok(());
                }
            }
            notified.await;
        }
    }
    pub async fn replace_result_page(
        &self,
        id: Uuid,
        start: u64,
        columns: Vec<Column>,
        rows: Vec<Vec<Value>>,
        summary: Json,
    ) -> Result<()> {
        {
            let mut state = self.results.lock().await;
            state.values.retain(|_, (owner, _)| *owner != id);
            let r = state.entries.get_mut(&id).ok_or_else(missing)?;
            r.rows.clear();
            r.base = start;
            r.meta.row_count = start;
            r.meta.columns = columns;
            r.bytes = serde_json::to_vec(&r.meta)?.len();
            r.limited = false;
            merge_summary(&mut r.meta.summary, summary);
        }
        self.append_rows(id, rows).await
    }
    pub async fn release_result(&self, id: Uuid) -> Result<()> {
        self.results.lock().await.remove(id);
        Ok(())
    }
    pub async fn release_owner_results(&self, owner: Uuid) -> Result<()> {
        let mut state = self.results.lock().await;
        let ids: Vec<_> = state
            .entries
            .iter()
            .filter(|(_, r)| r.owner == owner)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            state.remove(id);
        }
        state.auxiliary.retain(|_, (o, _)| *o != owner);
        Ok(())
    }
    pub async fn reserve_auxiliary(
        &self,
        execution_id: Uuid,
        handle: Uuid,
        bytes: usize,
    ) -> Result<()> {
        let owner = self.task(execution_id).await?.owner;
        let budget = self.settings().await?.limits.result_memory_bytes_per_client;
        let mut state = self.results.lock().await;
        if state
            .auxiliary
            .get(&handle)
            .is_some_and(|(o, _)| *o != owner)
        {
            return Err(AppError::new("forbidden", "句柄属于其他客户端"));
        }
        let used = state
            .entries
            .values()
            .filter(|r| r.owner == owner)
            .map(|r| r.bytes)
            .sum::<usize>()
            + state
                .auxiliary
                .iter()
                .filter(|(id, (o, _))| **id != handle && *o == owner)
                .map(|(_, (_, n))| *n)
                .sum::<usize>();
        if used.saturating_add(bytes) > budget {
            return Err(AppError::new("resource_limit", "客户端内存预算已用尽"));
        }
        state.auxiliary.insert(handle, (owner, bytes));
        Ok(())
    }
    pub async fn release_auxiliary(&self, handle: Uuid) -> Result<()> {
        self.results.lock().await.auxiliary.remove(&handle);
        Ok(())
    }
    /// 计算缓存字段解码及编码的空间上界，不分配完整 JSON 树。
    pub async fn cached_value_memory_bound(&self, id: Uuid) -> Result<usize> {
        let idle = self.settings().await?.limits.result_idle_seconds;
        let mut state = self.results.lock().await;
        state.expire(Duration::from_secs(idle));
        let (_, raw) = state.values.get(&id).ok_or_else(missing)?;
        Ok(decoded_value_memory_bound(raw))
    }

    /// 只读取查询缓存；调用方的预留上界不能扩大服务器内存限制。
    pub async fn read_cached_value_bounded(&self, id: Uuid, budget: usize) -> Result<Value> {
        let limits = self.settings().await?.limits;
        let budget = budget.min(limits.result_memory_bytes_per_client);
        let mut state = self.results.lock().await;
        state.expire(Duration::from_secs(limits.result_idle_seconds));
        let (result, raw) = state.values.get(&id).ok_or_else(missing)?;
        let result = *result;
        if decoded_value_memory_bound(raw) > budget {
            return Err(AppError::new(
                "resource_limit",
                "字段解码超过客户端内存预算",
            ));
        }
        let value = serde_json::from_slice(raw)?;
        if let Some(result) = state.entries.get_mut(&result) {
            result.touched = std::time::Instant::now();
        }
        Ok(value)
    }

    pub async fn read_value(&self, id: Uuid) -> Result<Value> {
        if let Some(value) = self.read_draft_value(id).await? {
            return Ok(value);
        }
        self.read_cached_value_bounded(id, usize::MAX).await
    }
    pub async fn large_value(&self, id: Uuid, offset: u64, max_bytes: usize) -> Result<Json> {
        self.large_value_inner(id, offset, max_bytes, true).await
    }
    pub async fn peek_large_value(&self, id: Uuid, offset: u64, max_bytes: usize) -> Result<Json> {
        self.large_value_inner(id, offset, max_bytes, false).await
    }
    async fn large_value_inner(
        &self,
        id: Uuid,
        offset: u64,
        max_bytes: usize,
        touch: bool,
    ) -> Result<Json> {
        if max_bytes == 0 || max_bytes > 262144 {
            return Err(AppError::invalid("字段块大小无效"));
        }
        let idle = self.settings().await?.limits.result_idle_seconds;
        let mut state = self.results.lock().await;
        state.expire(Duration::from_secs(idle));
        let (result, raw) = state.values.get(&id).ok_or_else(missing)?;
        let result = *result;
        let start = usize::try_from(offset).map_err(|_| AppError::invalid("字段偏移无效"))?;
        if start > raw.len() {
            return Err(AppError::invalid("字段偏移无效"));
        }
        let end = start.saturating_add(max_bytes).min(raw.len());
        let chunk = json!({"id":id,"offset":offset,"data_b64":STANDARD.encode(&raw[start..end]),"next":(end < raw.len()).then_some(end),"total_bytes":raw.len(),"content_type":"application/vnd.datapier.value+json"});
        if touch && let Some(r) = state.entries.get_mut(&result) {
            r.touched = std::time::Instant::now();
        }
        Ok(chunk)
    }
}
fn merge_summary(target: &mut Json, update: Json) {
    if let (Some(dst), Some(src)) = (target.as_object_mut(), update.as_object()) {
        dst.extend(src.clone());
    } else {
        *target = update;
    }
}
fn value_preview(value: &Value, raw: &[u8], limit: usize) -> String {
    fn prefix(s: &str, limit: usize) -> &str {
        let mut end = limit.min(s.len());
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
    match value {
        Value::Text(s)
        | Value::Integer(s)
        | Value::Unsigned(s)
        | Value::Decimal(s)
        | Value::Float(s)
        | Value::Date(s)
        | Value::Time(s)
        | Value::DateTime(s) => prefix(s, limit).to_owned(),
        Value::Bytes(s) => {
            let mut result = String::from("base64:");
            result.push_str(prefix(s, limit.saturating_sub(result.len())));
            result
        }
        _ => prefix(std::str::from_utf8(raw).unwrap_or(""), limit).to_owned(),
    }
}
