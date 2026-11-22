//! 补全目录仅保存有界元数据，网络读取不持有查询会话或 Engine 注册表锁。
use super::*;
use datapier_core::{
    model::{CompletionPage, CompletionRequest, CompletionScope},
    providers::{archery, mysql},
};

#[cfg(test)]
#[path = "../../tests/unit/server/completion.rs"]
mod tests;
use std::{
    collections::HashMap,
    sync::{Weak, atomic::AtomicUsize},
};
use tokio::sync::{Semaphore, watch};

const PAGE_BYTES: usize = 256 * 1024 - 8192;
const CONNECTION_BYTES: usize = 8 * 1024 * 1024;
const TOTAL_BYTES: usize = 32 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(60);

struct ByteCount(usize);
impl std::io::Write for ByteCount {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(bytes.len());
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn serialized_size(value: &impl serde::Serialize) -> Result<usize> {
    let mut count = ByteCount(0);
    serde_json::to_writer(&mut count, value)?;
    Ok(count.0)
}

fn snapshot_page(
    snapshot: &CompletionPage,
    prefix: &str,
    after: Option<&str>,
    limit: usize,
) -> CompletionPage {
    let prefix = prefix.to_lowercase();
    let mut items: Vec<_> = snapshot
        .items
        .iter()
        .filter(|item| {
            let mut name = item.name.chars().flat_map(char::to_lowercase);
            prefix
                .chars()
                .all(|character| name.next() == Some(character))
                && after.is_none_or(|after| item.name.as_str() > after)
        })
        .take(limit + 1)
        .cloned()
        .collect();
    let more = items.len() > limit;
    items.truncate(limit);
    let next_cursor = more.then(|| items.last().unwrap().name.clone());
    CompletionPage {
        items,
        completeness: snapshot.completeness.clone(),
        next_cursor,
        reason: snapshot.reason.clone(),
    }
}

#[derive(Clone)]
pub(super) enum Context {
    Mysql(mysql::CompletionContext),
    Archery(archery::CompletionContext),
    Unsupported,
}
impl Context {
    pub(super) fn from_session(session: &ProviderSession) -> Self {
        match session {
            ProviderSession::Mysql(session) => Self::Mysql(session.completion_context()),
            ProviderSession::Archery(session) => Self::Archery(session.completion_context()),
            ProviderSession::Redis(_) => Self::Unsupported,
        }
    }
    async fn generation(&self) -> Result<u64> {
        match self {
            Self::Mysql(_) => Ok(0),
            Self::Archery(context) => context.generation().await,
            Self::Unsupported => Err(AppError::new("unsupported", "此连接不支持 SQL 目录补全")),
        }
    }
    async fn catalog(
        &self,
        request: &CompletionRequest,
        after: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<CompletionPage> {
        match self {
            Self::Mysql(context) => context.catalog(request, after).await,
            Self::Archery(context) => context.catalog(request, after, cancel).await,
            Self::Unsupported => Err(AppError::new("unsupported", "此连接不支持 SQL 目录补全")),
        }
    }
}
struct Cached {
    profile: Uuid,
    session: Uuid,
    page: Arc<CompletionPage>,
    snapshot_id: Uuid,
    bytes: usize,
    created: Instant,
    used: Instant,
}
struct Cursor {
    session: Uuid,
    binding: String,
    generation: u64,
    after: String,
    snapshot_id: Option<Uuid>,
    created: Instant,
}
struct Session {
    profile: Uuid,
    auth_generation: u64,
    generation: u64,
    cancel: CancellationToken,
    permits: Arc<Semaphore>,
}
struct Flight {
    cancel: CancellationToken,
    subscribers: AtomicUsize,
    result: watch::Receiver<Option<Result<Arc<CompletionPage>>>>,
}
impl Flight {
    fn release(&self) {
        if self.subscribers.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.cancel.cancel();
        }
    }
}
struct ActiveSubscription {
    session: Uuid,
    id: Uuid,
    request_id: Value,
    cancel: CancellationToken,
}
struct Subscription<'a> {
    engine: &'a Engine,
    owner: Uuid,
    editor: Uuid,
    id: Uuid,
    cancel: CancellationToken,
    flight: Arc<Flight>,
}
impl Drop for Subscription<'_> {
    fn drop(&mut self) {
        let mut state = self.engine.state.lock().unwrap();
        let subscriptions = &mut state.completion.subscriptions;
        if subscriptions
            .get(&(self.owner, self.editor))
            .is_some_and(|active| active.id == self.id)
        {
            subscriptions.remove(&(self.owner, self.editor));
        }
        self.flight.release();
    }
}
pub(super) struct CatalogState {
    sessions: HashMap<Uuid, Session>,
    cache: HashMap<String, Cached>,
    cursors: HashMap<Uuid, Cursor>,
    flights: HashMap<String, Weak<Flight>>,
    subscriptions: HashMap<(Uuid, Uuid), ActiveSubscription>,
    cancelled: HashMap<(Uuid, Uuid, String), Instant>,
    latest: HashMap<(Uuid, Uuid), (u64, Instant)>,
    connection_permits: HashMap<Uuid, Weak<Semaphore>>,
    permits: Arc<Semaphore>,
}
impl Default for CatalogState {
    fn default() -> Self {
        Self {
            sessions: HashMap::new(),
            cache: HashMap::new(),
            cursors: HashMap::new(),
            flights: HashMap::new(),
            subscriptions: HashMap::new(),
            cancelled: HashMap::new(),
            latest: HashMap::new(),
            permits: Arc::new(Semaphore::new(8)),
            connection_permits: HashMap::new(),
        }
    }
}
impl CatalogState {
    pub(super) fn invalidate_session(&mut self, id: Uuid) {
        if let Some(session) = self.sessions.get_mut(&id) {
            session.cancel.cancel();
            session.cancel = CancellationToken::new();
            session.generation = session.generation.wrapping_add(1);
        }
        self.cache.retain(|_, item| item.session != id);
        self.cursors.retain(|_, cursor| cursor.session != id);
        self.subscriptions.retain(|_, subscription| {
            if subscription.session == id {
                subscription.cancel.cancel();
                false
            } else {
                true
            }
        });
    }
    pub(super) fn forget_session(&mut self, id: Uuid) {
        self.invalidate_session(id);
        self.sessions.remove(&id);
    }
    pub(super) fn invalidate_profile(&mut self, profile: Uuid) {
        let ids: Vec<_> = self
            .sessions
            .iter()
            .filter_map(|(id, session)| (session.profile == profile).then_some(*id))
            .collect();
        for id in ids {
            self.invalidate_session(id);
        }
    }
    fn admit(
        &mut self,
        owner: Uuid,
        editor: Uuid,
        request_id: &Value,
        generation: u64,
    ) -> Result<()> {
        let key = serde_json::to_string(request_id)?;
        if self.cancelled.contains_key(&(owner, editor, key))
            || self
                .latest
                .get(&(owner, editor))
                .is_some_and(|(old, _)| *old >= generation)
        {
            return Err(AppError::cancelled());
        }
        self.latest
            .insert((owner, editor), (generation, Instant::now()));
        Ok(())
    }
    fn unsubscribe(
        &mut self,
        owner: Uuid,
        session: Uuid,
        editor: Uuid,
        request_id: &Value,
        key: String,
    ) {
        self.cancelled.insert((owner, editor, key), Instant::now());
        if self
            .subscriptions
            .get(&(owner, editor))
            .is_some_and(|active| active.session == session && &active.request_id == request_id)
            && let Some(active) = self.subscriptions.remove(&(owner, editor))
        {
            active.cancel.cancel();
        }
    }
    fn trim(&mut self, profile: Uuid, incoming: usize) {
        self.cache.retain(|_, item| item.created.elapsed() < TTL);
        self.cursors
            .retain(|_, cursor| cursor.created.elapsed() < TTL);
        self.cancelled.retain(|_, at| at.elapsed() < TTL);
        self.latest.retain(|_, (_, at)| at.elapsed() < TTL);
        while self.cancelled.len() >= 4096 {
            if let Some(key) = self
                .cancelled
                .iter()
                .min_by_key(|(_, at)| *at)
                .map(|(key, _)| key.clone())
            {
                self.cancelled.remove(&key);
            } else {
                break;
            }
        }
        while self.latest.len() >= 4096 {
            if let Some(key) = self
                .latest
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(key, _)| *key)
            {
                self.latest.remove(&key);
            } else {
                break;
            }
        }
        loop {
            let total: usize = self.cache.values().map(|item| item.bytes).sum();
            let connection: usize = self
                .cache
                .values()
                .filter(|item| item.profile == profile)
                .map(|item| item.bytes)
                .sum();
            if total + incoming <= TOTAL_BYTES && connection + incoming <= CONNECTION_BYTES {
                break;
            }
            let victim = self
                .cache
                .iter()
                .filter(|(_, item)| {
                    connection + incoming <= CONNECTION_BYTES || item.profile == profile
                })
                .min_by_key(|(_, item)| item.used)
                .map(|(key, _)| key.clone());
            if let Some(victim) = victim {
                self.cache.remove(&victim);
            } else {
                break;
            }
        }
        while self.cursors.len() >= 4096 {
            if let Some(id) = self
                .cursors
                .iter()
                .min_by_key(|(_, cursor)| cursor.created)
                .map(|(id, _)| *id)
            {
                self.cursors.remove(&id);
            } else {
                break;
            }
        }
        self.flights.retain(|_, flight| flight.strong_count() != 0);
        self.connection_permits
            .retain(|_, permits| permits.strong_count() != 0);
    }
}

impl Engine {
    pub(super) fn completion_unsubscribe(&self, owner: Uuid, params: Value) -> Result<Value> {
        let session_id: Uuid = model::param(&params, "session_id")?;
        let editor_id: Uuid = model::param(&params, "editor_id")?;
        let request_id = params
            .get("request_id")
            .ok_or_else(|| AppError::invalid("缺少 request_id"))?
            .clone();
        let request_key = serde_json::to_string(&request_id)?;
        if request_key.len() > 4096 {
            return Err(AppError::invalid("request_id 超过大小限制"));
        }
        let db = self.database(owner, &params)?;
        let mut state = self.state.lock().unwrap();
        let catalog = &mut state.completion;
        catalog.trim(db.profile.id, 0);
        catalog.unsubscribe(owner, session_id, editor_id, &request_id, request_key);
        Ok(json!({}))
    }
    pub(super) async fn completion_catalog(&self, owner: Uuid, params: Value) -> Result<Value> {
        let request: CompletionRequest =
            serde_json::from_value(params).map_err(|e| AppError::invalid(e.to_string()))?;
        if request.limit == 0
            || request.limit > 200
            || request.prefix.len() > 1024
            || request
                .database
                .as_ref()
                .is_some_and(|name| name.len() > 1024)
            || request
                .relation
                .as_ref()
                .is_some_and(|name| name.len() > 1024)
            || serde_json::to_vec(&request.request_id)?.len() > 4096
        {
            return Err(AppError::invalid(
                "目录参数超过大小限制或 limit 不在 1 到 200 之间",
            ));
        }
        if matches!(
            request.scope,
            CompletionScope::Relations | CompletionScope::Columns
        ) && request.database.as_deref().is_none_or(str::is_empty)
            || matches!(request.scope, CompletionScope::Columns)
                && request.relation.as_deref().is_none_or(str::is_empty)
        {
            return Err(AppError::invalid("目录范围缺少数据库或关系名称"));
        }
        let db = self.database(owner, &json!({"session_id":request.session_id}))?;
        if self.store.profile(db.profile.id).await?.revision != db.profile.revision {
            return Err(AppError::new("invalidated", "连接设置已变化，请重新连接"));
        }
        let auth_generation = db.completion_context.generation().await?;
        let binding = serde_json::to_string(&(
            request.session_id,
            db.profile.id,
            db.profile.revision,
            auth_generation,
            request.scope,
            &request.database,
            &request.relation,
            &request.prefix,
            request.limit,
        ))?;
        let archery_snapshot = matches!(&db.completion_context, Context::Archery(_))
            && matches!(
                request.scope,
                CompletionScope::Relations | CompletionScope::Columns
            );
        let (generation, cancel, subscription, key, after, cursor_snapshot) = {
            let mut state = self.state.lock().unwrap();
            if !state.clients.contains(&owner)
                || state
                    .databases
                    .get(&request.session_id)
                    .is_none_or(|active| {
                        !Arc::ptr_eq(active, &db) || active.closing.load(Ordering::Acquire)
                    })
            {
                return Err(AppError::new("invalidated", "数据库会话已关闭"));
            }
            let catalog = &mut state.completion;
            catalog.trim(db.profile.id, 0);
            catalog.admit(
                owner,
                request.editor_id,
                &request.request_id,
                request.context_generation,
            )?;
            let permits = catalog
                .connection_permits
                .get(&db.profile.id)
                .and_then(Weak::upgrade)
                .unwrap_or_else(|| {
                    let permits = Arc::new(Semaphore::new(2));
                    catalog
                        .connection_permits
                        .insert(db.profile.id, Arc::downgrade(&permits));
                    permits
                });
            if catalog
                .sessions
                .get(&request.session_id)
                .is_some_and(|session| session.auth_generation != auth_generation)
            {
                catalog.invalidate_session(request.session_id);
                if let Some(session) = catalog.sessions.get_mut(&request.session_id) {
                    session.auth_generation = auth_generation;
                }
            }
            let session = catalog
                .sessions
                .entry(request.session_id)
                .or_insert_with(|| Session {
                    profile: db.profile.id,
                    auth_generation,
                    generation: 1,
                    cancel: CancellationToken::new(),
                    permits,
                });
            let generation = session.generation;
            let cancel = session.cancel.clone();
            let local = session.permits.clone();
            let (after, cursor_snapshot) = match &request.cursor {
                Some(token) => {
                    let id =
                        Uuid::parse_str(token).map_err(|_| AppError::invalid("目录游标无效"))?;
                    let cursor = catalog
                        .cursors
                        .get(&id)
                        .filter(|cursor| {
                            cursor.session == request.session_id
                                && cursor.binding == binding
                                && cursor.generation == generation
                        })
                        .ok_or_else(|| AppError::new("invalidated", "目录游标已失效"))?;
                    (Some(cursor.after.clone()), cursor.snapshot_id)
                }
                None => (None, None),
            };
            let key = if archery_snapshot {
                serde_json::to_string(&(
                    request.session_id,
                    db.profile.id,
                    db.profile.revision,
                    auth_generation,
                    request.scope,
                    &request.database,
                    &request.relation,
                    generation,
                ))?
            } else {
                serde_json::to_string(&(&binding, generation, &after))?
            };
            let cached = catalog
                .cache
                .get_mut(&key)
                .filter(|item| item.created.elapsed() < TTL)
                .map(|item| {
                    item.used = Instant::now();
                    (item.page.clone(), item.snapshot_id)
                });
            if archery_snapshot
                && request.cursor.is_some()
                && cached
                    .as_ref()
                    .is_none_or(|(_, id)| Some(*id) != cursor_snapshot)
            {
                return Err(AppError::new(
                    "invalidated",
                    "平台目录快照已过期，请重新读取",
                ));
            }
            let flight = if let Some((page, _)) = cached {
                let (_, result) = watch::channel(Some(Ok(page)));
                Arc::new(Flight {
                    cancel: cancel.child_token(),
                    subscribers: AtomicUsize::new(0),
                    result,
                })
            } else if let Some(flight) = catalog
                .flights
                .get(&key)
                .and_then(Weak::upgrade)
                .filter(|flight| !flight.cancel.is_cancelled())
            {
                flight
            } else {
                let flight_cancel = cancel.child_token();
                let (sender, result) = watch::channel(None);
                let flight = Arc::new(Flight {
                    cancel: flight_cancel.clone(),
                    subscribers: AtomicUsize::new(0),
                    result,
                });
                let context = db.completion_context.clone();
                let mut fetch_request = request.clone();
                let fetch_after = if archery_snapshot {
                    None
                } else {
                    after.clone()
                };
                if archery_snapshot {
                    fetch_request.prefix.clear();
                    fetch_request.cursor = None;
                    fetch_request.limit = usize::MAX;
                }
                let global = catalog.permits.clone();
                tokio::spawn(async move {
                    let work = async {
                        let _global = global
                            .acquire()
                            .await
                            .map_err(|_| AppError::new("internal", "目录额度已关闭"))?;
                        let _local = local
                            .acquire()
                            .await
                            .map_err(|_| AppError::new("internal", "目录额度已关闭"))?;
                        context
                            .catalog(&fetch_request, fetch_after.as_deref(), &flight_cancel)
                            .await
                            .map(Arc::new)
                    };
                    let result = tokio::select! {
                        biased;
                        _ = flight_cancel.cancelled() => Err(AppError::cancelled()),
                        result = tokio::time::timeout(Duration::from_secs(3), work) => result.unwrap_or_else(|_| Err(AppError::new("timeout", "目录读取超时"))),
                    };
                    sender.send_replace(Some(result));
                });
                catalog.flights.insert(key.clone(), Arc::downgrade(&flight));
                flight
            };
            flight.subscribers.fetch_add(1, Ordering::AcqRel);
            let id = Uuid::new_v4();
            let subscriber_cancel = CancellationToken::new();
            if let Some(previous) = catalog.subscriptions.insert(
                (owner, request.editor_id),
                ActiveSubscription {
                    session: request.session_id,
                    id,
                    request_id: request.request_id.clone(),
                    cancel: subscriber_cancel.clone(),
                },
            ) {
                previous.cancel.cancel();
            }
            let subscription = Subscription {
                engine: self,
                owner,
                editor: request.editor_id,
                id,
                cancel: subscriber_cancel,
                flight,
            };
            (
                generation,
                cancel,
                subscription,
                key,
                after,
                cursor_snapshot,
            )
        };
        let mut result = subscription.flight.result.clone();
        let snapshot = loop {
            let ready = result.borrow().clone();
            if let Some(page) = ready {
                break page?;
            }
            tokio::select! {
                biased;
                _ = subscription.cancel.cancelled() => return Err(AppError::cancelled()),
                _ = cancel.cancelled() => return Err(AppError::new("invalidated", "目录已失效")),
                changed = result.changed() => changed.map_err(|_| AppError::new("internal", "目录读取中断"))?,
            }
        };
        if self.store.profile(db.profile.id).await?.revision != db.profile.revision
            || db.completion_context.generation().await? != auth_generation
        {
            return Err(AppError::new("invalidated", "目录身份已变化"));
        }
        self.database(owner, &json!({"session_id":request.session_id}))?;
        let mut page = if archery_snapshot {
            snapshot_page(&snapshot, &request.prefix, after.as_deref(), request.limit)
        } else {
            (*snapshot).clone()
        };
        let snapshot_bytes = if archery_snapshot {
            serialized_size(snapshot.as_ref())?
        } else {
            0
        };
        if snapshot_bytes > CONNECTION_BYTES {
            return Err(AppError::new(
                "response_too_large",
                "平台目录快照超过连接缓存预算",
            ));
        }
        // 超出预算时在完整名称边界截页，游标指向最后实际交付的名称。
        while serialized_size(&page)? > PAGE_BYTES {
            if page.items.len() <= 1 {
                return Err(AppError::new(
                    "response_too_large",
                    "单条目录元数据超过大小限制",
                ));
            }
            page.items.pop();
            page.next_cursor = page.items.last().map(|item| item.name.clone());
        }
        let mut state = self.state.lock().unwrap();
        if !state.clients.contains(&owner)
            || state
                .databases
                .get(&request.session_id)
                .is_none_or(|active| {
                    !Arc::ptr_eq(active, &db) || active.closing.load(Ordering::Acquire)
                })
        {
            return Err(AppError::new("invalidated", "数据库会话已关闭"));
        }
        let catalog = &mut state.completion;
        if subscription.cancel.is_cancelled()
            || cancel.is_cancelled()
            || catalog
                .sessions
                .get(&request.session_id)
                .is_none_or(|s| s.generation != generation)
        {
            return Err(AppError::new("invalidated", "目录已失效"));
        }
        if archery_snapshot
            && request.cursor.is_some()
            && catalog.cache.get(&key).is_none_or(|cached| {
                Some(cached.snapshot_id) != cursor_snapshot || cached.created.elapsed() >= TTL
            })
        {
            return Err(AppError::new(
                "invalidated",
                "平台目录快照已过期，请重新读取",
            ));
        }
        if !catalog.cache.contains_key(&key) {
            let bytes = if archery_snapshot {
                snapshot_bytes
            } else {
                serialized_size(&page)?
            };
            catalog.trim(db.profile.id, bytes);
            let cached_page = if archery_snapshot {
                snapshot.clone()
            } else {
                Arc::new(page.clone())
            };
            catalog.cache.insert(
                key.clone(),
                Cached {
                    profile: db.profile.id,
                    session: request.session_id,
                    page: cached_page,
                    snapshot_id: Uuid::new_v4(),
                    bytes,
                    created: Instant::now(),
                    used: Instant::now(),
                },
            );
        }
        let snapshot_id = archery_snapshot.then(|| catalog.cache[&key].snapshot_id);
        let next_cursor = page.next_cursor.take().map(|after| {
            let id = Uuid::new_v4();
            catalog.cursors.insert(
                id,
                Cursor {
                    session: request.session_id,
                    binding,
                    generation,
                    after,
                    snapshot_id,
                    created: Instant::now(),
                },
            );
            id.to_string()
        });
        Ok(
            json!({"request_id":request.request_id,"context_generation":request.context_generation,"catalog_generation":generation,"items":page.items,"completeness":page.completeness,"next_cursor":next_cursor,"reason":page.reason}),
        )
    }
}
