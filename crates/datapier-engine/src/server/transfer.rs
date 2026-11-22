//! 数据交换计划注册、任务生命周期及有界报告入口。
use super::*;
use datapier_core::{
    execution::migration,
    model::transfer::{
        MAX_PLAN_BYTES, PreparationProgress, PreparedTransfer, RecordRange, TransferSpec,
        TransferStepState,
    },
};
use std::sync::atomic::AtomicUsize;

mod catalog;

const TTL: Duration = Duration::from_secs(300);
const GLOBAL_BUDGET: usize = 64 * 1024 * 1024;
const PAGE: usize = 768 * 1024;
const CHUNK: usize = 128 * 1024;
fn transfer_run_value(
    run: &datapier_core::storage::transfer::TransferRun,
    storage_error: Option<&AppError>,
) -> Result<Value> {
    let mut value = serialize(run)?;
    if let Some(error) = storage_error {
        value["error"] = serialize(error)?;
        value["progress_is_persisted_snapshot"] = json!(true);
    }
    Ok(value)
}

fn append_record_range(ranges: &mut Vec<RecordRange>, range: RecordRange) {
    if let Some(previous) = ranges.last_mut()
        && previous.last.checked_add(1) == Some(range.first)
    {
        previous.last = range.last;
    } else {
        ranges.push(range);
    }
}

pub(super) fn selected_file_records(
    item: &datapier_core::model::transfer::TransferStepOutcome,
    states: &[TransferStepState],
    ranges: &mut Vec<RecordRange>,
) -> Result<bool> {
    let Some(first) = item.locator.first_record else {
        return Ok(false);
    };
    if item.state == TransferStepState::Sent {
        return Ok(false);
    }
    let mut any = false;
    if item.item_states.is_empty() {
        if states.contains(&item.state) {
            append_record_range(
                ranges,
                RecordRange {
                    first,
                    last: item.locator.last_record.unwrap_or(first),
                },
            );
            any = true;
        }
    } else {
        for (offset, state) in item.item_states.iter().enumerate() {
            if states.contains(state) {
                let record = first
                    .checked_add(offset as u64)
                    .ok_or_else(|| AppError::invalid("记录号溢出"))?;
                if item.locator.last_record.is_some_and(|last| record > last) {
                    return Err(AppError::new("storage", "批次状态超出记录范围"));
                }
                append_record_range(
                    ranges,
                    RecordRange {
                        first: record,
                        last: record,
                    },
                );
                any = true;
            }
        }
    }
    Ok(any)
}

fn sql_object_selection(
    items: &[datapier_core::model::transfer::TransferPlanItem],
    requested: Vec<(String, bool)>,
    create_missing: bool,
) -> Result<Vec<(String, bool)>> {
    let mut states: Vec<_> = items
        .iter()
        .map(|item| (item.id.clone(), item.selected))
        .collect();
    for (id, selected) in requested {
        let item = items
            .iter()
            .find(|item| item.id == id)
            .ok_or_else(|| AppError::invalid("选择的计划项不存在"))?;
        let source = item
            .source
            .as_ref()
            .filter(|_| item.target.is_some())
            .ok_or_else(|| AppError::invalid("此步骤不能按对象选择"))?;
        for (entry, (_, state)) in items.iter().zip(states.iter_mut()) {
            if entry.source.as_ref().is_some_and(|other| {
                other.database == source.database
                    && other.name == source.name
                    && other.kind == source.kind
            }) {
                *state = selected;
            }
        }
    }
    let any_object = items
        .iter()
        .zip(&states)
        .any(|(item, (_, selected))| item.action == "object_scope" && *selected);
    for (item, (_, selected)) in items.iter().zip(states.iter_mut()) {
        if item.phase == "database" {
            *selected = any_object && create_missing;
        }
    }
    Ok(states)
}

#[derive(Default)]
pub(super) struct TransferState {
    plans: HashMap<Uuid, PlanEntry>,
    catalogs: HashMap<Uuid, catalog::CatalogLease>,
}
impl TransferState {
    pub(super) fn forget_catalog(&mut self, id: Uuid) {
        self.catalogs.remove(&id);
    }

    /// 客户端离开时释放未被任务持有的计划；运行任务仍计入预算直至结束。
    pub(super) fn remove_owner(&mut self, owner: Uuid) {
        self.catalogs.retain(|_, lease| lease.owner != owner);
        self.plans.retain(|_, entry| {
            if entry.owner != owner {
                true
            } else if Arc::strong_count(&entry.slot) > 1 {
                entry.expires = Instant::now();
                true
            } else {
                false
            }
        });
    }
}
struct PlanEntry {
    owner: Uuid,
    workspace: String,
    expires: Instant,
    bytes: Arc<AtomicUsize>,
    slot: Arc<AsyncMutex<PlanSlot>>,
    binding: Option<(Uuid, String, String)>,
}
struct PlanSlot {
    revision: u64,
    state: String,
    progress: tokio::sync::watch::Sender<PreparationProgress>,
    prepared: Option<Arc<PreparedTransfer>>,
    task_id: Uuid,
    submission: Option<Uuid>,
    error: Option<AppError>,
    retry_of: Option<Uuid>,
    retry_states: Vec<TransferStepState>,
    _budget: MemoryReservation,
}

struct PreparationIdentity {
    owner: Uuid,
    plan_id: Uuid,
    execution_id: Uuid,
}

// 注册表和后台任务共同持有额度；最后一个持有者释放后归还客户端共享预算。
pub(super) struct MemoryReservation {
    store: Store,
    id: Uuid,
}
impl MemoryReservation {
    pub(super) async fn reserve(store: &Store, owner: Uuid, bytes: usize) -> Result<Self> {
        let id = Uuid::new_v4();
        store.reserve_auxiliary_for_owner(owner, id, bytes).await?;
        Ok(Self {
            store: store.clone(),
            id,
        })
    }
    pub(super) async fn resize(&self, owner: Uuid, bytes: usize) -> Result<()> {
        self.store
            .reserve_auxiliary_for_owner(owner, self.id, bytes)
            .await
    }
}
impl Drop for MemoryReservation {
    fn drop(&mut self) {
        let store = self.store.clone();
        let id = self.id;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = store.release_auxiliary(id).await {
                    tracing::error!(%id, %error, "释放迁移计划内存额度失败");
                }
            });
        }
    }
}

fn merge(target: &mut Value, patch: &Value) -> Result<()> {
    let patch = patch
        .as_object()
        .ok_or_else(|| AppError::invalid("计划 patch 必须是对象"))?;
    let object = target
        .as_object_mut()
        .ok_or_else(|| AppError::invalid("计划对象无效"))?;
    for (key, value) in patch {
        if value.is_object() && object.get(key).is_some_and(Value::is_object) {
            merge(object.get_mut(key).unwrap(), value)?;
        } else {
            object.insert(key.clone(), value.clone());
        }
    }
    Ok(())
}
fn page_params(params: &Value) -> Result<(usize, usize)> {
    let offset: usize = optional(params, "offset")?.unwrap_or(0);
    let limit: usize = optional(params, "limit")?.unwrap_or(100);
    if limit == 0 || limit > 1000 {
        return Err(AppError::invalid("分页参数无效"));
    }
    Ok((offset, limit))
}
fn utf8_end(s: &str, start: usize, max: usize) -> usize {
    let mut end = (start + max).min(s.len());
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}
fn page<T: serde::Serialize>(
    items: &[T],
    offset: usize,
    limit: usize,
    chunk_offset: usize,
) -> Result<Value> {
    if offset > items.len() {
        return Err(AppError::invalid("分页偏移无效"));
    }
    let mut result = Vec::new();
    let mut used = 256;
    for item in items.iter().skip(offset).take(limit) {
        let encoded = serde_json::to_string(item)?;
        if encoded.len() > CHUNK {
            if !result.is_empty() {
                break;
            }
            if chunk_offset > encoded.len() || !encoded.is_char_boundary(chunk_offset) {
                return Err(AppError::invalid("分块偏移无效"));
            }
            let end = utf8_end(&encoded, chunk_offset, CHUNK);
            return Ok(
                json!({"items":[],"offset":offset,"chunk":{"encoding":"json_utf8","data":&encoded[chunk_offset..end],"offset":chunk_offset,"total_bytes":encoded.len(),"next":if end<encoded.len(){Some(end)}else{None}},"next_offset":if end==encoded.len(){Some(offset+1)}else{Some(offset)}}),
            );
        }
        if chunk_offset != 0 {
            return Err(AppError::invalid("该项不需要分块"));
        }
        // JSON 字符串转义可能扩展，按实际 RPC 编码估算。
        let value: Value = serde_json::from_str(&encoded)?;
        let bytes = serde_json::to_vec(&value)?.len() + 1;
        if used + bytes > PAGE {
            break;
        }
        used += bytes;
        result.push(value);
    }
    let next = offset + result.len();
    Ok(
        json!({"items":result,"offset":offset,"next_offset":if next<items.len(){Some(next)}else{None},"total":items.len()}),
    )
}
impl Engine {
    async fn transfer_binding_guard(
        &self,
        params: &Value,
    ) -> Result<Option<tokio::sync::OwnedMutexGuard<()>>> {
        if let Some(project) = optional::<Uuid>(params, "project_id")? {
            let group: String =
                optional(params, "binding_group")?.unwrap_or_else(|| "default".into());
            Ok(Some(
                self.project_binding_gate(project, &group)
                    .lock_owned()
                    .await,
            ))
        } else {
            Ok(None)
        }
    }
    async fn transfer_binding_digest(&self, project: Uuid, group: &str) -> Result<String> {
        let mut offset = 0u64;
        let mut hash = String::new();
        loop {
            let bindings = self
                .store
                .project_bindings(project, group, offset, 1000)
                .await?;
            if bindings.is_empty() {
                break;
            }
            offset = offset
                .checked_add(bindings.len() as u64)
                .ok_or_else(|| AppError::invalid("绑定数量超限"))?;
            hash = migration::digest(&(hash, &bindings))?;
        }
        Ok(hash)
    }
    async fn transfer_binding_snapshot(
        &self,
        params: &Value,
    ) -> Result<Option<(Uuid, String, String)>> {
        if let Some(project) = optional::<Uuid>(params, "project_id")? {
            let group: String =
                optional(params, "binding_group")?.unwrap_or_else(|| "default".into());
            let hash = self.transfer_binding_digest(project, &group).await?;
            Ok(Some((project, group, hash)))
        } else {
            Ok(None)
        }
    }
    fn transfer_slot(&self, owner: Uuid, id: Uuid) -> Result<Arc<AsyncMutex<PlanSlot>>> {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .transfers
            .plans
            .get_mut(&id)
            .ok_or_else(|| AppError::new("not_found", "迁移计划不存在"))?;
        if entry.owner != owner {
            return Err(AppError::new("forbidden", "迁移计划属于其他客户端"));
        }
        if entry.expires <= Instant::now() && Arc::strong_count(&entry.slot) == 1 {
            return Err(AppError::new("expired", "迁移计划已过期，请重新准备"));
        }
        Ok(entry.slot.clone())
    }
    async fn transfer_start_prepare(
        self: &Arc<Self>,
        owner: Uuid,
        ws: String,
        spec: TransferSpec,
        retry: Option<(Uuid, Vec<TransferStepState>)>,
        binding: Option<(Uuid, String, String)>,
    ) -> Result<Value> {
        let (retry_of, retry_states) = match retry {
            Some((id, states)) => (Some(id), states),
            None => (None, Vec::new()),
        };
        spec.validate()?;
        let id = Uuid::new_v4();
        let bytes = Arc::new(AtomicUsize::new(MAX_PLAN_BYTES));
        let expired = {
            let mut state = self.state.lock().unwrap();
            let expired: Vec<_> = state
                .transfers
                .plans
                .iter()
                .filter(|(_, entry)| {
                    entry.expires <= Instant::now() && Arc::strong_count(&entry.slot) == 1
                })
                .map(|(&id, _)| id)
                .collect();
            for id in &expired {
                state.transfers.plans.remove(id);
            }
            expired
        };
        for id in expired {
            self.store.release_auxiliary(id).await?;
        }
        self.store
            .reserve_auxiliary_for_owner(owner, id, MAX_PLAN_BYTES)
            .await?;
        let budget = MemoryReservation {
            store: self.store.clone(),
            id,
        };
        // 准备阶段预留完整上限，避免并发准备的临时内存突破总预算。
        {
            let mut state = self.state.lock().unwrap();
            state
                .transfers
                .plans
                .retain(|_, p| p.expires > Instant::now() || Arc::strong_count(&p.slot) > 1);
            let global: usize = state
                .transfers
                .plans
                .values()
                .map(|p| p.bytes.load(Ordering::Relaxed))
                .sum();
            let client: usize = state
                .transfers
                .plans
                .values()
                .filter(|p| p.owner == owner)
                .map(|p| p.bytes.load(Ordering::Relaxed))
                .sum();
            if state.transfers.plans.len() >= 64
                || state
                    .transfers
                    .plans
                    .values()
                    .filter(|p| p.owner == owner)
                    .count()
                    >= 16
            {
                return Err(AppError::new("resource_limit", "迁移计划数量超限"));
            }
            if global + MAX_PLAN_BYTES > GLOBAL_BUDGET || client + MAX_PLAN_BYTES > MAX_PLAN_BYTES {
                return Err(AppError::new(
                    "resource_limit",
                    "迁移计划内存预算不足，请先丢弃旧计划",
                ));
            }
            // 占位防止并发请求同时越过预算；失败时下方删除。
            state.transfers.plans.insert(
                id,
                PlanEntry {
                    owner,
                    workspace: ws.clone(),
                    expires: Instant::now() + TTL,
                    bytes: bytes.clone(),
                    slot: Arc::new(AsyncMutex::new(PlanSlot {
                        revision: 1,
                        state: "preparing".into(),
                        progress: tokio::sync::watch::channel(PreparationProgress::default()).0,
                        prepared: None,
                        task_id: Uuid::nil(),
                        submission: None,
                        error: None,
                        retry_of,
                        retry_states,
                        _budget: budget,
                    })),
                    binding,
                },
            );
        }
        let record = match self
            .store
            .create_task(owner, &ws, "transfer.prepare", json!({"plan_id":id}), false)
            .await
        {
            Ok(record) => record,
            Err(error) => {
                self.state.lock().unwrap().transfers.plans.remove(&id);
                return Err(error);
            }
        };
        let slot = self.transfer_slot(owner, id)?;
        {
            slot.lock().await.task_id = record.id;
        }
        self.launch_prepare(
            PreparationIdentity {
                owner,
                plan_id: id,
                execution_id: record.id,
            },
            slot,
            bytes,
            spec,
            None,
        )
        .await?;
        Ok(json!({"plan_id":id,"id":id,"task_id":record.id,"revision":1,"state":"preparing"}))
    }
    async fn launch_prepare(
        self: &Arc<Self>,
        identity: PreparationIdentity,
        slot: Arc<AsyncMutex<PlanSlot>>,
        bytes: Arc<AtomicUsize>,
        spec: TransferSpec,
        selection: Option<Vec<(String, bool)>>,
    ) -> Result<()> {
        let PreparationIdentity {
            owner,
            plan_id: id,
            execution_id: task_id,
        } = identity;
        let cancel = CancellationToken::new();
        let transition = Arc::new(AsyncMutex::new(()));
        {
            let mut state = self.state.lock().unwrap();
            if !state.clients.contains(&owner) {
                cancel.cancel();
            }
            state.tasks.insert(
                task_id,
                Running {
                    owner,
                    session_id: Uuid::nil(),
                    cancel: cancel.clone(),
                    transition: transition.clone(),
                    abort: None,
                    started: None,
                },
            );
        }
        self.emit_state(owner, task_id, "queued");
        let engine = self.clone();
        let handle = tokio::spawn(async move {
            let started = Instant::now();
            let has_destination = spec.destination.is_some();
            let result: Result<Value> = async {
                {
                    let _guard = transition.lock().await;
                    if cancel.is_cancelled() {
                        return Err(AppError::cancelled());
                    }
                    engine
                        .store
                        .update_task(task_id, "running", json!({}), None)
                        .await?;
                    if let Some(task) = engine.state.lock().unwrap().tasks.get_mut(&task_id) {
                        task.started = Some(started);
                    }
                }
                engine.emit_state(owner, task_id, "running");
                let (retry, progress) = {
                    let slot = slot.lock().await;
                    (slot.retry_of, slot.progress.clone())
                };
                let sink = ExecutionSink::new(engine.store.clone(), task_id).with_owner(owner);
                let mut plan = if let Some(old) = retry {
                    migration::prepare_retry_with_progress(&sink, spec, old, &cancel, &progress)
                        .await?
                } else {
                    migration::prepare_with_progress(&sink, spec, &cancel, &progress).await?
                };
                let mut slot = slot.lock().await;
                plan.id = id;
                plan.revision = slot.revision;
                plan.retry_states = slot.retry_states.clone();
                if let Some(selection) = selection {
                    for (item_id, selected) in selection {
                        let item = plan
                            .items
                            .iter_mut()
                            .find(|item| item.id == item_id)
                            .ok_or_else(|| AppError::invalid("选择的计划项不存在"))?;
                        item.selected = selected;
                    }
                }
                plan.digest = migration::plan_digest(&plan)?;
                let size = migration::encoded_size(&plan)?;
                if size > MAX_PLAN_BYTES {
                    return Err(AppError::new("resource_limit", "计划超过 32 MiB"));
                }
                engine
                    .store
                    .reserve_auxiliary_for_owner(owner, id, size)
                    .await?;
                bytes.store(size, Ordering::Relaxed);
                slot.state = plan.state.clone();
                slot.prepared = Some(Arc::new(plan));
                // 大文件准备可能超过 TTL；确认窗口从计划真正就绪后开始计算。
                if let Some(entry) = engine.state.lock().unwrap().transfers.plans.get_mut(&id) {
                    entry.expires = Instant::now() + TTL;
                }
                Ok(json!({"plan_id":id,"revision":slot.revision,"state":slot.state}))
            }
            .await;
            if let Err(error) = &result {
                let mut slot = slot.lock().await;
                slot.state = "failed".into();
                // 计划响应也可能携带数据库输入值；仅放行已知的静态预检诊断。
                slot.error = Some(
                    if error.code == "permission_denied" && error.message == "连接为只读模式"
                    {
                        AppError::new(
                            "transfer_target_read_only",
                            "目标连接为只读模式，请返回来源与目标选择可写连接，或在连接设置中调整访问模式",
                        )
                    } else if has_destination
                        && error.code == "permission_denied"
                        && error.message == "无法证明 SQL 的访问范围"
                    {
                        AppError::new(
                            "transfer_scope_unknown",
                            "无法证明 SQL 的访问范围；请返回来源与目标检查数据库选择和连接的可见范围，再检查输入 SQL 是否明确指定数据库",
                        )
                    } else {
                        AppError::new(&error.code, format!("数据交换准备失败（{}）", error.code))
                    },
                );
                slot.prepared = None;
                bytes.store(0, Ordering::Relaxed);
                // 长预检失败后仍需向客户端展示诊断，不能让旧 TTL 抢先遮住真实错误。
                if let Some(entry) = engine.state.lock().unwrap().transfers.plans.get_mut(&id) {
                    entry.expires = Instant::now() + TTL;
                }
                if let Err(release) = engine.store.release_auxiliary(id).await {
                    tracing::error!(%id, %release, "释放失败计划额度失败");
                }
            }
            engine
                .finish_task(owner, task_id, &result, Some(started))
                .await;
        });
        if let Some(task) = self.state.lock().unwrap().tasks.get_mut(&task_id) {
            task.abort = Some(handle.abort_handle());
        }
        Ok(())
    }
    async fn transfer_owned_run(
        &self,
        owner: Uuid,
        id: Uuid,
    ) -> Result<(
        datapier_core::storage::transfer::TransferRun,
        Option<AppError>,
    )> {
        let mut run = self.store.transfer_run(id).await?;
        if self.store.task(run.execution_id).await?.owner != owner {
            return Err(AppError::new("forbidden", "任务属于其他客户端"));
        }
        // 最终记录写入失败时，所有消费者共用内存终态；进度仍是磁盘快照。
        let storage_error = self
            .state
            .lock()
            .unwrap()
            .unpersisted
            .get(&run.execution_id)
            .map(|completion| {
                run.state = completion.state.to_owned();
                completion.error.clone()
            });
        Ok((run, storage_error))
    }
    async fn resolve_transfer_spec(
        &self,
        owner: Uuid,
        params: &Value,
        spec: &mut TransferSpec,
    ) -> Result<()> {
        let has_alias = spec
            .source
            .as_ref()
            .is_some_and(|v| v.connection_alias.is_some())
            || spec
                .destination
                .as_ref()
                .is_some_and(|v| v.connection_alias.is_some());
        if has_alias {
            let project: Uuid = optional(params, "project_id")?
                .ok_or_else(|| AppError::new("binding_required", "逻辑连接需要明确项目绑定"))?;
            let group: String =
                optional(params, "binding_group")?.unwrap_or_else(|| "default".into());
            self.resolve_project_spec(owner, project, &group, spec)
                .await?;
        }
        for endpoint in [&spec.source, &spec.destination].into_iter().flatten() {
            if endpoint.profile_id.is_none() {
                return Err(AppError::new("binding_required", "请明确本机连接绑定"));
            }
        }
        Ok(())
    }
    pub(super) async fn transfer_rpc(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.active(owner)?;
        if let (Some(id), Some(ws)) = (
            optional::<Uuid>(&params, "plan_id")?,
            params.get("workspace_id").and_then(Value::as_str),
        ) {
            let state = self.state.lock().unwrap();
            let entry = state
                .transfers
                .plans
                .get(&id)
                .ok_or_else(|| AppError::new("not_found", "迁移计划不存在"))?;
            if entry.owner != owner || entry.workspace != ws {
                return Err(AppError::new("forbidden", "计划不属于当前客户端工作区"));
            }
        }
        match method {
            "transfer.catalog" => self.transfer_catalog(owner, &params).await,
            "transfer.input.inspect" => self.transfer_input_inspect(owner, &params).await,
            "transfer.prepare" => {
                let mut spec: TransferSpec = model::param(&params, "spec")?;
                let _binding_guard = self.transfer_binding_guard(&params).await?;
                self.resolve_transfer_spec(owner, &params, &mut spec)
                    .await?;
                let binding = self.transfer_binding_snapshot(&params).await?;
                self.transfer_start_prepare(
                    owner,
                    workspace(&params).to_owned(),
                    spec,
                    None,
                    binding,
                )
                .await
            }
            "transfer.plan.read" => {
                let id: Uuid = model::param(&params, "plan_id")?;
                let slot = self.transfer_slot(owner, id)?;
                let slot = slot.lock().await;
                if let Some(revision) = optional::<u64>(&params, "revision")?
                    && revision != slot.revision
                {
                    return Err(AppError::new("conflict", "计划版本已变化"));
                }
                let section: String =
                    optional(&params, "section")?.unwrap_or_else(|| "summary".into());
                let mut response = json!({"id":id,"plan_id":id,"task_id":slot.task_id,"revision":slot.revision,"state":slot.state,"error":slot.error});
                if slot.state == "preparing" {
                    response["preparation_progress"] = json!(*slot.progress.borrow());
                }
                if let Some(plan) = &slot.prepared {
                    response["digest"] = json!(plan.digest);
                    response["item_count"] = json!(plan.items.len());
                    response["sample_count"] = json!(plan.samples.len());
                    response["diagnostic_count"] = json!(plan.diagnostics.len());
                    if section == "summary" && serde_json::to_vec(&plan.spec)?.len() <= CHUNK {
                        response["spec"] = serde_json::to_value(&plan.spec)?;
                    }
                    if let Some(old) = slot.retry_of {
                        response["retry_of"] = json!(old);
                        response["retry_note"] = json!(if plan.spec.mode
                            == datapier_core::model::transfer::TransferMode::SqlRestore
                        {
                            "已生效、跳过和待确认的 SQL 范围不会自动重放"
                        } else if plan.spec.mode
                            == datapier_core::model::transfer::TransferMode::Import
                            && plan.spec.provider == datapier_core::model::ProviderKind::Mysql
                        {
                            "新计划仅包含选定的待处理记录；已完成、跳过及待确认的记录不会自动重放"
                        } else {
                            "文件按已确认记录范围续作；直接迁移按当前对象与过滤条件读取新快照"
                        });
                    }
                    if section == "summary" {
                        let value = page(&plan.items, 0, 100, 0)?;
                        response["items"] = value["items"].clone();
                        response["next_offset"] = value["next_offset"].clone();
                        response["chunk"] = value.get("chunk").cloned().unwrap_or(Value::Null);
                    } else {
                        let (offset, limit) = page_params(&params)?;
                        let chunk_offset = optional(&params, "chunk_offset")?.unwrap_or(0);
                        let value = match section.as_str() {
                            "items" => page(&plan.items, offset, limit, chunk_offset)?,
                            "samples" => page(&plan.samples, offset, limit, chunk_offset)?,
                            "diagnostics" => page(&plan.diagnostics, offset, limit, chunk_offset)?,
                            "spec" => page(
                                std::slice::from_ref(&plan.spec),
                                offset,
                                limit,
                                chunk_offset,
                            )?,
                            _ => return Err(AppError::invalid("未知计划分页区段")),
                        };
                        response["section"] = json!(section);
                        response["page"] = value;
                    }
                } else if section != "summary" {
                    return Err(AppError::new("busy", "计划尚未准备完成"));
                }
                Ok(response)
            }
            "transfer.plan.update" => {
                let id: Uuid = model::param(&params, "plan_id")?;
                let expected: u64 = model::param(&params, "expected_revision")?;
                let old_binding = {
                    let state = self.state.lock().unwrap();
                    state
                        .transfers
                        .plans
                        .get(&id)
                        .and_then(|entry| entry.binding.clone())
                };
                let mut binding_params = params.clone();
                if binding_params.get("project_id").is_none()
                    && let Some((project, group, _)) = &old_binding
                {
                    binding_params["project_id"] = json!(project);
                    binding_params["binding_group"] = json!(group);
                }
                let _binding_guard = self.transfer_binding_guard(&binding_params).await?;
                let slot = self.transfer_slot(owner, id)?;
                let mut locked = slot.lock().await;
                if locked.revision != expected
                    || locked.submission.is_some()
                    || locked.state == "preparing"
                {
                    return Err(AppError::new("conflict", "计划版本已变化或正在使用"));
                }
                let existing = locked
                    .prepared
                    .as_ref()
                    .ok_or_else(|| AppError::new("conflict", "计划未准备完成"))?;
                let mut spec = if let Some(value) = params.get("spec") {
                    serde_json::from_value(value.clone())?
                } else {
                    existing.spec.clone()
                };
                if let Some(patch) = params.get("patch") {
                    let mut value = serde_json::to_value(&spec)?;
                    merge(&mut value, patch)?;
                    spec = serde_json::from_value(value)?;
                }
                self.resolve_transfer_spec(owner, &binding_params, &mut spec)
                    .await?;
                spec.validate()?;
                let binding = self.transfer_binding_snapshot(&binding_params).await?;
                let selection: Option<Vec<Value>> = optional(&params, "item_selection")?;
                let selection = selection
                    .map(|items| {
                        items
                            .into_iter()
                            .map(|v| Ok((model::param(&v, "id")?, model::param(&v, "selected")?)))
                            .collect::<Result<Vec<(String, bool)>>>()
                    })
                    .transpose()?;
                let selection =
                    if spec.mode == datapier_core::model::transfer::TransferMode::SqlRestore {
                        Some(sql_object_selection(
                            &existing.items,
                            selection.unwrap_or_default(),
                            spec.options.create_missing,
                        )?)
                    } else {
                        selection
                    };
                if spec.mode != datapier_core::model::transfer::TransferMode::SqlRestore
                    && let Some(selection) = &selection
                {
                    let default_db = spec
                        .destination
                        .as_ref()
                        .or(spec.source.as_ref())
                        .and_then(|e| e.database.clone());
                    for (item_id, selected) in selection {
                        let item = existing
                            .items
                            .iter()
                            .find(|item| &item.id == item_id)
                            .ok_or_else(|| AppError::invalid("选择的计划项不存在"))?;
                        if let Some(rest) = item_id.strip_prefix("input:") {
                            let index = rest
                                .split(':')
                                .next()
                                .and_then(|n| n.parse::<usize>().ok())
                                .ok_or_else(|| AppError::invalid("输入计划项 ID 无效"))?;
                            let input = spec
                                .inputs
                                .get_mut(index)
                                .ok_or_else(|| AppError::invalid("输入计划项索引无效"))?;
                            input.selected_schema_items.retain(|id| id != item_id);
                            if *selected {
                                input.selected_schema_items.push(item_id.clone());
                            }
                        } else if let Some(target) = &item.target {
                            for object in &mut spec.objects {
                                if object.target.database.as_ref().or(default_db.as_ref())
                                    == target.database.as_ref()
                                    && object.target.name == target.name
                                    && (object.target.kind.is_empty()
                                        || object.target.kind == target.kind)
                                {
                                    object.selected_schema_items.retain(|id| id != item_id);
                                    if *selected {
                                        object.selected_schema_items.push(item_id.clone());
                                    }
                                }
                            }
                        }
                    }
                }
                let (ws, bytes, previous_bytes) = {
                    let state = self.state.lock().unwrap();
                    let entry = state
                        .transfers
                        .plans
                        .get(&id)
                        .ok_or_else(|| AppError::new("not_found", "计划不存在"))?;
                    let current = entry.bytes.load(Ordering::Relaxed);
                    let client: usize = state
                        .transfers
                        .plans
                        .values()
                        .filter(|p| p.owner == owner)
                        .map(|p| p.bytes.load(Ordering::Relaxed))
                        .sum();
                    let total: usize = state
                        .transfers
                        .plans
                        .values()
                        .map(|p| p.bytes.load(Ordering::Relaxed))
                        .sum();
                    if client - current + MAX_PLAN_BYTES > MAX_PLAN_BYTES
                        || total - current + MAX_PLAN_BYTES > GLOBAL_BUDGET
                    {
                        return Err(AppError::new("resource_limit", "计划内存预算不足"));
                    }
                    let ws = entry.workspace.clone();
                    let bytes = entry.bytes.clone();
                    bytes.store(MAX_PLAN_BYTES, Ordering::Relaxed);
                    (ws, bytes, current)
                };
                if let Err(error) = self
                    .store
                    .reserve_auxiliary_for_owner(owner, id, MAX_PLAN_BYTES)
                    .await
                {
                    bytes.store(previous_bytes, Ordering::Relaxed);
                    return Err(error);
                }
                let record = match self
                    .store
                    .create_task(owner, &ws, "transfer.prepare", json!({"plan_id":id}), false)
                    .await
                {
                    Ok(record) => record,
                    Err(error) => {
                        bytes.store(previous_bytes, Ordering::Relaxed);
                        if let Err(restore) = self
                            .store
                            .reserve_auxiliary_for_owner(owner, id, previous_bytes)
                            .await
                        {
                            tracing::error!(%id,%restore,"恢复计划内存额度失败");
                        }
                        return Err(error);
                    }
                };
                {
                    let mut state = self.state.lock().unwrap();
                    let entry = state
                        .transfers
                        .plans
                        .get_mut(&id)
                        .ok_or_else(|| AppError::new("not_found", "计划不存在"))?;
                    entry.expires = Instant::now() + TTL;
                    entry.binding = binding;
                }
                locked.revision = locked
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| AppError::invalid("计划版本溢出"))?;
                locked.state = "preparing".into();
                locked.progress.send_replace(PreparationProgress::default());
                locked.prepared = None;
                locked.error = None;
                locked.task_id = record.id;
                let revision = locked.revision;
                drop(locked);
                self.launch_prepare(
                    PreparationIdentity {
                        owner,
                        plan_id: id,
                        execution_id: record.id,
                    },
                    slot,
                    bytes,
                    spec,
                    selection,
                )
                .await?;
                Ok(
                    json!({"plan_id":id,"id":id,"task_id":record.id,"revision":revision,"state":"preparing"}),
                )
            }
            "transfer.discard" => {
                let id: Uuid = model::param(&params, "plan_id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let slot = {
                    let state = self.state.lock().unwrap();
                    let entry = state
                        .transfers
                        .plans
                        .get(&id)
                        .ok_or_else(|| AppError::new("not_found", "迁移计划不存在"))?;
                    if entry.owner != owner {
                        return Err(AppError::new("forbidden", "迁移计划属于其他客户端"));
                    }
                    entry.slot.clone()
                };
                let locked = slot.lock().await;
                let running = locked
                    .submission
                    .is_some_and(|task| self.state.lock().unwrap().tasks.contains_key(&task));
                if locked.revision != revision || running || locked.state == "preparing" {
                    return Err(AppError::new("conflict", "计划正在使用或版本已变化"));
                }
                self.state.lock().unwrap().transfers.plans.remove(&id);
                drop(locked);
                drop(slot);
                self.store.release_auxiliary(id).await?;
                Ok(json!({"discarded":true}))
            }
            "transfer.submit" => self.transfer_submit(owner, &params).await,
            "transfer.run.get" => {
                let id: Uuid = model::param(&params, "run_id")?;
                match self.transfer_owned_run(owner, id).await {
                    Ok((run, storage_error)) => {
                        let mut value = transfer_run_value(&run, storage_error.as_ref())?;
                        value["run_id"] = json!(id);
                        value["task_id"] = value["execution_id"].clone();
                        Ok(value)
                    }
                    Err(error) if error.code == "not_found" => {
                        let task = self.task(id).await?;
                        if task.owner != owner || task.method != "transfer.submit" {
                            return Err(AppError::new("forbidden", "任务属于其他客户端"));
                        }
                        Ok(json!({"run_id":id,"task_id":id,"state":task.state,"error":task.error}))
                    }
                    Err(error) => Err(error),
                }
            }
            "transfer.run.items" => {
                let id: Uuid = model::param(&params, "run_id")?;
                self.transfer_owned_run(owner, id).await?;
                let (offset, limit) = page_params(&params)?;
                let after: Option<u64> = optional(&params, "after")?.or(if offset > 0 {
                    Some(offset as u64 - 1)
                } else {
                    None
                });
                let rows = self.store.transfer_items(id, after, limit as u64).await?;
                let mut states = optional::<Vec<String>>(&params, "states")?;
                if let Some(state) = optional::<String>(&params, "state")? {
                    states.get_or_insert_with(Vec::new).push(state);
                }
                let mut items = Vec::new();
                let mut bytes = 128;
                let mut next = None;
                for item in rows {
                    if states
                        .as_ref()
                        .is_none_or(|states| states.iter().any(|s| s == item.state.as_str()))
                    {
                        let size = serde_json::to_vec(&item)?.len() + 1;
                        if bytes + size > PAGE {
                            break;
                        }
                        bytes += size;
                        next = Some(item.ordinal);
                        items.push(item);
                    } else {
                        next = Some(item.ordinal);
                    }
                }
                Ok(json!({"items":items,"next":next}))
            }
            "transfer.preset.list" => {
                let (offset, limit) = page_params(&params)?;
                let items = self
                    .store
                    .transfer_presets(workspace(&params), offset as u64, limit as u64)
                    .await?;
                Ok(json!({"items":items}))
            }
            "transfer.preset.save" => {
                let id = optional(&params, "id")?.unwrap_or_else(Uuid::new_v4);
                let revision = optional(&params, "expected_revision")?.unwrap_or(0);
                let name: String = model::param(&params, "name")?;
                let spec: TransferSpec = model::param(&params, "spec")?;
                let next = self
                    .store
                    .transfer_save_preset(id, workspace(&params), &name, &spec, revision)
                    .await?;
                Ok(json!({"id":id,"name":name,"revision":next,"spec":spec}))
            }
            "transfer.preset.delete" => {
                let id: Uuid = model::param(&params, "id")?;
                let revision: u64 = model::param(&params, "expected_revision")?;
                self.store
                    .transfer_delete_preset(id, workspace(&params), revision)
                    .await?;
                Ok(json!({"deleted":true}))
            }
            "transfer.retry.prepare" => self.transfer_retry_prepare(owner, &params).await,
            "transfer.report.export" => self.transfer_report_export(owner, &params).await,
            _ => Err(AppError::new("method_not_found", "未知数据交换方法")),
        }
    }
    pub(super) fn transfer_sink(
        self: &Arc<Self>,
        owner: Uuid,
        task_id: Uuid,
        transition: Arc<AsyncMutex<()>>,
        cancel: CancellationToken,
        revisions: Vec<(Uuid, u64, Arc<AsyncMutex<()>>)>,
    ) -> ExecutionSink {
        let engine = Arc::downgrade(self);
        ExecutionSink::new(self.store.clone(), task_id)
            .with_owner(owner)
            .with_dispatch_control(transition, cancel)
            .with_transfer_profiles(revisions)
            .with_send_gate(
                self.send_gate.clone(),
                Arc::new(move || {
                    let engine = engine
                        .upgrade()
                        .ok_or_else(|| AppError::new("disconnected", "引擎已关闭"))?;
                    let state = engine.state.lock().unwrap();
                    if !state.clients.contains(&owner) || !state.contexts.contains_key(&owner) {
                        return Err(AppError::new("disconnected", "客户端会话已关闭"));
                    }
                    Ok(())
                }),
            )
    }

    async fn transfer_submit(self: &Arc<Self>, owner: Uuid, params: &Value) -> Result<Value> {
        let id: Uuid = model::param(params, "plan_id")?;
        let revision: u64 = model::param(params, "revision")?;
        let digest: String = model::param(params, "digest")?;
        let confirmations: Vec<String> =
            optional(params, "destructive_confirmations")?.unwrap_or_default();
        let binding = {
            let state = self.state.lock().unwrap();
            state
                .transfers
                .plans
                .get(&id)
                .and_then(|entry| entry.binding.clone())
        };
        let _binding_guard = if let Some((project, group, _)) = &binding {
            Some(
                self.project_binding_gate(*project, group)
                    .lock_owned()
                    .await,
            )
        } else {
            None
        };
        let slot = self.transfer_slot(owner, id)?;
        let mut locked = slot.lock().await;
        if locked.revision != revision {
            return Err(AppError::new("conflict", "计划版本已变化"));
        }
        let plan = locked
            .prepared
            .as_ref()
            .cloned()
            .ok_or_else(|| AppError::new("busy", "计划尚未准备完成"))?;
        if plan.digest != digest {
            return Err(AppError::new("conflict", "计划摘要已变化"));
        }
        if let Some(task_id) = locked.submission {
            return Ok(json!({"run_id":task_id,"task_id":task_id,"plan_id":id}));
        }
        if let Some((project, group, expected)) = &binding
            && self.transfer_binding_digest(*project, group).await? != *expected
        {
            locked.state = "invalidated".into();
            return Err(AppError::new("conflict", "项目绑定已变化，请重新准备"));
        }
        if locked.state != "ready"
            || plan.state != "ready"
            || plan.diagnostics.iter().any(|d| d.blocking)
        {
            return Err(AppError::invalid("计划尚未就绪"));
        }
        if plan
            .items
            .iter()
            .any(|item| item.selected && item.destructive && !confirmations.contains(&item.id))
        {
            return Err(AppError::new(
                "confirmation_required",
                "请逐项确认破坏性变更",
            ));
        }
        let ws = {
            let state = self.state.lock().unwrap();
            state
                .transfers
                .plans
                .get(&id)
                .ok_or_else(|| AppError::new("not_found", "计划不存在"))?
                .workspace
                .clone()
        };
        let operation_guard = self.reserve_transfer_resources(owner, &plan, id)?;
        let record = self
            .store
            .create_task(
                owner,
                &ws,
                "transfer.submit",
                json!({"plan_id":id,"revision":revision,"digest":digest}),
                false,
            )
            .await?;
        let task_id = record.id;
        let retry_of = locked.retry_of;
        locked.submission = Some(task_id);
        drop(locked);
        let cancel = CancellationToken::new();
        let transition = Arc::new(AsyncMutex::new(()));
        {
            let mut state = self.state.lock().unwrap();
            if !state.clients.contains(&owner) {
                cancel.cancel();
            }
            state.tasks.insert(
                task_id,
                Running {
                    owner,
                    session_id: Uuid::nil(),
                    cancel: cancel.clone(),
                    transition: transition.clone(),
                    abort: None,
                    started: None,
                },
            );
        }
        self.emit_state(owner, task_id, "queued");
        let engine = self.clone();
        let handle = tokio::spawn(async move {
            let _plan_registration = slot;
            let _operation_guard = operation_guard;
            let started = Instant::now();
            let result:Result<Value>=async {
                // 仅在发送边界持有配置门；等待数据库行锁时必须允许原事务结束。
                let mut profiles: Vec<Uuid> = plan.profile_digests.keys().copied().collect();
                profiles.sort_unstable();
                let mut revisions = Vec::with_capacity(profiles.len());
                for profile in profiles {
                    let gate = engine.profile_lock(profile);
                    let guard = tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), guard = gate.lock() => guard };
                    let revision = engine.store.profile(profile).await?.revision;
                    drop(guard);
                    revisions.push((profile, revision, gate));
                }
                {let _guard=transition.lock().await; if cancel.is_cancelled(){return Err(AppError::cancelled());}
                    engine.store.update_task(task_id,"running",json!({}),None).await?;
                    if let Some(task)=engine.state.lock().unwrap().tasks.get_mut(&task_id){task.started=Some(started);} }
                engine.emit_state(owner,task_id,"running");
                let sink = engine.transfer_sink(owner, task_id, transition.clone(), cancel.clone(), revisions);
                let progress = migration::execute(&plan, &sink, task_id, &ws, retry_of, &confirmations, &cancel).await?;
                Ok(json!({"run_id":task_id,"progress":progress}))
            }.await;
            engine
                .finish_task(owner, task_id, &result, Some(started))
                .await;
        });
        if let Some(task) = self.state.lock().unwrap().tasks.get_mut(&task_id) {
            task.abort = Some(handle.abort_handle());
        }
        Ok(json!({"run_id":task_id,"task_id":task_id,"plan_id":id}))
    }
    async fn transfer_retry_prepare(
        self: &Arc<Self>,
        owner: Uuid,
        params: &Value,
    ) -> Result<Value> {
        let id: Uuid = model::param(params, "run_id")?;
        let (run, _) = self.transfer_owned_run(owner, id).await?;
        if matches!(run.state.as_str(), "running" | "cancelling") {
            return Err(AppError::new("busy", "原任务仍在执行"));
        }
        let states: Vec<TransferStepState> = optional(params, "states")?.unwrap_or_else(|| {
            vec![
                TransferStepState::Failed,
                TransferStepState::RolledBack,
                TransferStepState::NotSent,
            ]
        });
        if states.is_empty()
            || states.iter().any(|state| {
                !matches!(
                    state,
                    TransferStepState::Failed
                        | TransferStepState::RolledBack
                        | TransferStepState::NotSent
                )
            })
        {
            return Err(AppError::invalid("仅允许选择失败、已回滚或未发送范围"));
        }
        let _binding_guard = self.transfer_binding_guard(params).await?;
        let mut spec: TransferSpec = model::param(params, "spec")?;
        self.resolve_transfer_spec(owner, params, &mut spec).await?;
        let binding = self.transfer_binding_snapshot(params).await?;
        if spec.mode == datapier_core::model::transfer::TransferMode::SqlRestore {
            if migration::digest(&spec)? != run.spec_digest {
                return Err(AppError::new("conflict", "SQL 续作配置必须与原任务一致"));
            }
            if spec.inputs.len() != run.context.inputs.len() {
                return Err(AppError::invalid("SQL 输入数量与原任务不同"));
            }
            for (input, expected) in spec.inputs.iter().zip(&run.context.inputs) {
                let path = input.path.clone();
                let fingerprint = tokio::task::spawn_blocking(move || {
                    datapier_core::execution::data_files::fingerprint(
                        &path,
                        1024 * 1024,
                        16 * 1024 * 1024,
                    )
                })
                .await
                .map_err(|e| AppError::new("io", e.to_string()))??;
                if fingerprint.sha256 != expected.sha256 || fingerprint.bytes != expected.bytes {
                    return Err(AppError::new("conflict", "SQL 续作输入与原任务指纹不同"));
                }
            }
            return self
                .transfer_start_prepare(owner, run.workspace_id, spec, Some((id, states)), binding)
                .await;
        }
        let file_import = spec.provider == datapier_core::model::ProviderKind::Mysql
            && spec.mode == datapier_core::model::transfer::TransferMode::Import;
        if file_import {
            let normalize = |target: &mut datapier_core::model::ResourceTarget,
                             endpoint: Option<
                &datapier_core::model::transfer::TransferEndpoint,
            >| {
                if target.database.is_none() {
                    target.database = endpoint.and_then(|endpoint| endpoint.database.clone());
                }
                if target.kind.is_empty() {
                    target.kind = "table".into();
                }
            };
            for object in &mut spec.objects {
                normalize(&mut object.source, spec.source.as_ref());
                normalize(
                    &mut object.target,
                    spec.destination.as_ref().or(spec.source.as_ref()),
                );
            }
            for input in &mut spec.inputs {
                if let Some(target) = &mut input.target {
                    normalize(target, spec.destination.as_ref());
                }
            }
            if migration::digest(&spec)? != run.spec_digest {
                return Err(AppError::new("conflict", "文件续作配置必须与原任务一致"));
            }
        }
        let mut frontier: Option<(usize, u64)> = None;
        let mut after_data = false;
        let mut ranges: Vec<Vec<RecordRange>> = vec![Vec::new(); spec.inputs.len()];
        let mut range_count = 0usize;
        let mut after = None;
        let mut any = false;
        let mut unknown = false;
        loop {
            let items = self.store.transfer_items(id, after, 500).await?;
            if items.is_empty() {
                break;
            }
            after = items.last().map(|item| item.ordinal);
            for item in items {
                if item.state == TransferStepState::OutcomeUnknown
                    || item.state == TransferStepState::Sent
                    || item.item_states.iter().any(|state| {
                        *state == TransferStepState::OutcomeUnknown
                            || *state == TransferStepState::Sent
                    })
                {
                    unknown = true;
                }
                if let (Some(index), Some(first)) = (item.locator.input, item.locator.first_record)
                {
                    if file_import && matches!(item.phase.as_str(), "data" | "conversion") {
                        let last = item.locator.last_record.unwrap_or(first);
                        frontier =
                            Some(frontier.map_or((index, last), |old| old.max((index, last))));
                    }
                    if index >= ranges.len() {
                        return Err(AppError::invalid("续作配置的输入文件与原任务不匹配"));
                    }
                    let before = ranges[index].len();
                    any |= selected_file_records(&item, &states, &mut ranges[index])?;
                    range_count += ranges[index].len() - before;
                    if range_count > MAX_PLAN_BYTES / std::mem::size_of::<RecordRange>() {
                        return Err(AppError::new("resource_limit", "续作范围超过内存预算"));
                    }
                } else if states.contains(&item.state) {
                    any = true;
                }
                if file_import && frontier.is_some() && item.phase == "schema" {
                    after_data = true;
                }
            }
        }
        if file_import
            && spec.options.include_data
            && run.state != "succeeded"
            && states.contains(&TransferStepState::NotSent)
            && !after_data
        {
            for (index, input) in spec.inputs.iter().enumerate() {
                let start = match frontier {
                    Some((last_input, _)) if index < last_input => continue,
                    Some((last_input, last)) if index == last_input => match last.checked_add(1) {
                        Some(start) => start,
                        None => continue,
                    },
                    _ => 1,
                };
                let before = ranges[index].len();
                if input.record_ranges.is_empty() {
                    append_record_range(
                        &mut ranges[index],
                        RecordRange {
                            first: start,
                            last: u64::MAX,
                        },
                    );
                    any = true;
                } else {
                    for range in &input.record_ranges {
                        let first = range.first.max(start);
                        if first <= range.last {
                            append_record_range(
                                &mut ranges[index],
                                RecordRange {
                                    first,
                                    last: range.last,
                                },
                            );
                            any = true;
                        }
                    }
                }
                range_count += ranges[index].len() - before;
                if range_count > MAX_PLAN_BYTES / std::mem::size_of::<RecordRange>() {
                    return Err(AppError::new("resource_limit", "续作范围超过内存预算"));
                }
            }
        }
        if !any {
            return Err(AppError::invalid("原任务没有选定的可续作范围"));
        }
        if !spec.inputs.is_empty() {
            if spec.inputs.len() != run.context.inputs.len() {
                return Err(AppError::invalid("续作输入数量与原任务不同"));
            }
            let mut retained = Vec::new();
            for (index, mut input) in spec.inputs.into_iter().enumerate() {
                let path = input.path.clone();
                let fingerprint = tokio::task::spawn_blocking(move || {
                    datapier_core::execution::data_files::fingerprint(
                        &path,
                        1024 * 1024,
                        16 * 1024 * 1024,
                    )
                })
                .await
                .map_err(|e| AppError::new("io", e.to_string()))??;
                let expected = &run.context.inputs[index];
                if fingerprint.sha256 != expected.sha256 || fingerprint.bytes != expected.bytes {
                    return Err(AppError::new("conflict", "续作输入与原任务指纹不同"));
                }
                if ranges[index].is_empty() {
                    continue;
                }
                input.record_ranges = std::mem::take(&mut ranges[index]);
                retained.push(input);
            }
            if retained.is_empty() {
                return Err(AppError::invalid("没有可续作的文件记录"));
            }
            spec.inputs = retained;
        } else if unknown {
            // 无可还原 key／行定位时，无法证明新快照不会包含待确认写入。
            return Err(AppError::new(
                "outcome_unknown",
                "原任务含待确认写入，先人工核对并重新配置迁移",
            ));
        }
        self.transfer_start_prepare(owner, run.workspace_id, spec, Some((id, states)), binding)
            .await
    }
    async fn transfer_report_export(&self, owner: Uuid, params: &Value) -> Result<Value> {
        use std::io::Write;
        let id: Uuid = model::param(params, "run_id")?;
        let (run, storage_error) = self.transfer_owned_run(owner, id).await?;
        if matches!(run.state.as_str(), "running" | "cancelling") {
            return Err(AppError::new("busy", "运行中的任务不能导出最终报告"));
        }
        let format: String = optional(params, "format")?.unwrap_or_else(|| "json".into());
        if !matches!(format.as_str(), "json" | "csv") {
            return Err(AppError::invalid("报告格式仅支持 JSON 或 CSV"));
        }
        let csv = format == "csv";
        let report_run = transfer_run_value(&run, storage_error.as_ref())?;
        let media = if csv { "text/csv" } else { "application/json" };
        let include: bool = optional(params, "include_failed_input")?.unwrap_or(false);
        let mut spec: Option<TransferSpec> = if include {
            Some(model::param(params, "spec")?)
        } else {
            None
        };
        if let Some(spec) = &spec
            && migration::digest(spec)? != run.spec_digest
        {
            return Err(AppError::new("conflict", "报告配置与原任务不同"));
        }
        if let Some(paths) = optional::<Vec<std::path::PathBuf>>(params, "inputs")? {
            let spec = spec
                .as_mut()
                .ok_or_else(|| AppError::invalid("替代输入路径需要包含失败输入"))?;
            if paths.len() != spec.inputs.len() {
                return Err(AppError::invalid("替代输入路径数量不符"));
            }
            for (input, path) in spec.inputs.iter_mut().zip(paths) {
                input.path = path;
            }
        }
        if let Some(spec) = &spec
            && spec.inputs.len() != run.context.inputs.len()
        {
            return Err(AppError::invalid("输入数量与原任务不同"));
        }
        let path: std::path::PathBuf = model::param(params, "path")?;
        let mut initial = datapier_core::execution::data_files::AtomicOutput::create(&path)?;
        let baseline = initial.snapshot(false)?;
        let partial = initial.partial_path().to_path_buf();
        let identity = json!({"path":baseline.canonical_path,"canonical_path":baseline.canonical_path,"file_id":baseline.file_id,"size":baseline.size,"export_name":path.file_name().and_then(|v|v.to_str()),"media_type":media});
        let artifact = self
            .store
            .transfer_register_artifact(owner, Some(id), "report", &identity)
            .await?;
        let mut output = Some(initial);
        let result: Result<()> = async {
            let writer = output.as_mut().unwrap();
            if csv {
                write!(writer, "section,payload\r\nrun,\"")?;
                serde_json::to_writer(CsvQuoted(&mut *writer), &report_run)?;
                write!(writer, "\"\r\n")?;
            } else {
                write!(writer, "{{\"run\":")?;
                serde_json::to_writer(&mut *writer, &report_run)?;
                write!(writer, ",\"items\":[")?;
            }
            let mut first = true;
            let mut after = None;
            loop {
                let items = self.store.transfer_items(id, after, 250).await?;
                if items.is_empty() {
                    break;
                }
                after = items.last().map(|v| v.ordinal);
                for item in items {
                    let writer = output.as_mut().unwrap();
                    if csv {
                        write!(writer, "outcome,\"")?;
                        serde_json::to_writer(CsvQuoted(&mut *writer), &item)?;
                        write!(writer, "\"\r\n")?;
                    } else {
                        if !first {
                            write!(writer, ",")?;
                        }
                        first = false;
                        write!(writer, "{{\"outcome\":")?;
                        serde_json::to_writer(&mut *writer, &item)?;
                    }
                    if include
                        && item.locator.input.is_some()
                        && (item.state == TransferStepState::Failed
                            || item.item_states.contains(&TransferStepState::Failed)
                            || item.error_record.is_some())
                    {
                        if csv {
                            write!(writer, "input_events,\"")?;
                        } else {
                            write!(writer, ",\"input_events\":[")?;
                        }
                        let spec = spec.as_ref().unwrap().clone();
                        let identities = run.context.inputs.clone();
                        let mut writer = output.take().unwrap();
                        let (returned, written) = tokio::task::spawn_blocking(move || {
                            let result = (|| -> Result<()> {
                                if csv {
                                    let mut quoted = CsvQuoted(&mut writer);
                                    write!(quoted, "[")?;
                                    write_failed_events(&mut quoted, &item, &spec, &identities)?;
                                    write!(quoted, "]")?;
                                } else {
                                    write_failed_events(&mut writer, &item, &spec, &identities)?;
                                }
                                Ok(())
                            })();
                            (writer, result)
                        })
                        .await
                        .map_err(|e| AppError::new("io", e.to_string()))?;
                        output = Some(returned);
                        written?;
                        let writer = output.as_mut().unwrap();
                        if csv {
                            write!(writer, "\"\r\n")?;
                        } else {
                            write!(writer, "]")?;
                        }
                    }
                    if !csv {
                        write!(output.as_mut().unwrap(), "}}")?;
                    }
                }
            }
            if !csv {
                write!(output.as_mut().unwrap(), "]}}")?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            drop(output.take());
            self.transfer_report_partial(owner, artifact, &partial, &path, media, &baseline)
                .await?;
            return Err(error);
        }
        let output = output.take().unwrap();
        let published = tokio::task::spawn_blocking(move || output.publish())
            .await
            .map_err(|e| AppError::new("io", e.to_string()))
            .and_then(|result| result);
        let path = match published {
            Ok(path) => path,
            Err(mut error) => {
                use datapier_core::execution::data_files::{PublicationState, publication_state};
                match publication_state(&error) {
                    PublicationState::NotPublished => {
                        self.transfer_report_partial(
                            owner, artifact, &partial, &path, media, &baseline,
                        )
                        .await?
                    }
                    PublicationState::Published => {
                        let destination = path.clone();
                        let snapshot = tokio::task::spawn_blocking(move || {
                            datapier_core::execution::data_files::output_snapshot(
                                &destination,
                                true,
                            )
                        })
                        .await
                        .ok()
                        .and_then(|result| result.ok());
                        let identity=snapshot.filter(|snapshot| snapshot.file_id.is_some() && snapshot.file_id==baseline.file_id).map(|snapshot| {
                            json!({"path":snapshot.canonical_path,"canonical_path":snapshot.canonical_path,"file_id":snapshot.file_id,"size":snapshot.size,"sha256":snapshot.sha256,"export_name":path.file_name().and_then(|v|v.to_str()),"media_type":media})
                        });
                        self.store
                            .transfer_update_artifact(
                                artifact,
                                owner,
                                None,
                                "published",
                                identity.as_ref(),
                            )
                            .await?;
                    }
                    PublicationState::Unknown => {
                        error.code = "outcome_unknown".into();
                        self.store
                            .transfer_update_artifact(
                                artifact,
                                owner,
                                None,
                                "outcome_unknown",
                                None,
                            )
                            .await?;
                    }
                }
                return Err(error);
            }
        };
        let identity_path = path.clone();
        let snapshot = tokio::task::spawn_blocking(move || {
            datapier_core::execution::data_files::output_snapshot(&identity_path, true)
        })
        .await
        .map_err(|e| AppError::new("io", e.to_string()))??;
        if snapshot.file_id != baseline.file_id {
            return Err(AppError::new("recovery_conflict", "发布的报告身份已变化"));
        }
        let identity = json!({"path":snapshot.canonical_path,"canonical_path":snapshot.canonical_path,"file_id":snapshot.file_id,"size":snapshot.size,"sha256":snapshot.sha256,"export_name":path.file_name().and_then(|v|v.to_str()),"media_type":media});
        self.store
            .transfer_update_artifact(artifact, owner, Some(1), "published", Some(&identity))
            .await?;
        Ok(json!({"path":path,"artifact_id":artifact,"run_id":id}))
    }
    async fn transfer_report_partial(
        &self,
        owner: Uuid,
        artifact: Uuid,
        partial: &std::path::Path,
        destination: &std::path::Path,
        media: &str,
        baseline: &datapier_core::execution::data_files::OutputSnapshot,
    ) -> Result<()> {
        let snapshot_path = partial.to_owned();
        let snapshot = tokio::task::spawn_blocking(move || {
            datapier_core::execution::data_files::output_snapshot(&snapshot_path, true)
        })
        .await
        .map_err(|e| AppError::new("io", e.to_string()))??;
        if baseline.file_id.is_none()
            || snapshot.canonical_path != baseline.canonical_path
            || snapshot.file_id != baseline.file_id
        {
            return Err(AppError::new(
                "recovery_conflict",
                "报告临时文件身份已变化或无法验证",
            ));
        }
        let identity = json!({"path":snapshot.canonical_path,"canonical_path":snapshot.canonical_path,"file_id":snapshot.file_id,"size":snapshot.size,"sha256":snapshot.sha256,"export_name":destination.file_name().and_then(|v|v.to_str()),"media_type":media});
        self.store
            .transfer_update_artifact(artifact, owner, Some(1), "cleanup_pending", Some(&identity))
            .await?;
        Ok(())
    }
}

/// CSV 单元格内逐块转义，避免将整个报告或 Redis 集合缓存在内存中。
struct CsvQuoted<'a, W: std::io::Write>(&'a mut W);
impl<W: std::io::Write> std::io::Write for CsvQuoted<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut start = 0;
        for (index, byte) in buf.iter().enumerate() {
            if *byte == b'"' {
                self.0.write_all(&buf[start..index])?;
                self.0.write_all(b"\"\"")?;
                start = index + 1;
            }
        }
        self.0.write_all(&buf[start..])?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// 仅将选中失败记录写入用户显式选择的文件；输入内容不进入执行日志。
fn write_failed_events<W: std::io::Write>(
    output: &mut W,
    item: &datapier_core::model::transfer::TransferStepOutcome,
    spec: &TransferSpec,
    identities: &[datapier_core::model::transfer::TransferInputIdentity],
) -> Result<()> {
    use datapier_core::execution::data_files;
    use datapier_core::model::transfer::TransferFormat;
    let index = item
        .locator
        .input
        .ok_or_else(|| AppError::invalid("失败项缺少输入定位"))?;
    let input = spec
        .inputs
        .get(index)
        .ok_or_else(|| AppError::invalid("失败项输入索引无效"))?;
    let expected = identities
        .get(index)
        .ok_or_else(|| AppError::invalid("缺少冻结输入指纹"))?;
    let fingerprint = data_files::fingerprint(&input.path, 1024 * 1024, 16 * 1024 * 1024)?;
    if fingerprint.sha256 != expected.sha256 || fingerprint.bytes != expected.bytes {
        return Err(AppError::new("conflict", "报告输入与原任务指纹不同"));
    }
    let mut first = true;
    #[derive(serde::Serialize)]
    struct RawRecord<'a> {
        record: u64,
        encoding: &'static str,
        raw_bytes: &'a [u8],
    }
    if input.format == TransferFormat::Sql {
        let mut emit = |value: Value| -> Result<()> {
            if !first {
                write!(output, ",")?;
            }
            first = false;
            serde_json::to_writer(&mut *output, &value)?;
            Ok(())
        };
        let index = datapier_core::execution::sql_restore::inspect(&input.path)?;
        if index.sha256 != expected.sha256 || index.bytes != expected.bytes {
            return Err(AppError::new("conflict", "SQL 输入已变化"));
        }
        let offset = item
            .locator
            .byte_start
            .ok_or_else(|| AppError::invalid("SQL 失败项缺少字节定位"))?;
        let number = index
            .statements
            .iter()
            .position(|statement| statement.start <= offset && offset < statement.end)
            .ok_or_else(|| AppError::invalid("SQL 失败定位不属于已校验语句"))?;
        let statement = &index.statements[number];
        if statement.kind == datapier_core::execution::sql_restore::SqlKind::Insert {
            if item.locator.record_offsets.is_empty() {
                datapier_core::execution::sql_restore::read_insert_statement(
                    &input.path,
                    &index,
                    number,
                    |columns, values, pos| {
                        if pos == offset {
                            emit(json!({"byte_offset":pos,"columns":columns,"values":values}))?;
                        }
                        Ok(())
                    },
                )?;
            } else {
                let mut selected = item
                    .locator
                    .record_offsets
                    .iter()
                    .copied()
                    .enumerate()
                    .filter_map(|(i, offset)| {
                        let is_error = item.error_record == Some(offset);
                        let failed = if item.item_states.is_empty() {
                            item.error_record
                                .map_or(item.state == TransferStepState::Failed, |_| is_error)
                        } else {
                            item.item_states.get(i) == Some(&TransferStepState::Failed) || is_error
                        };
                        failed.then_some(offset)
                    })
                    .peekable();
                while let Some(&offset) = selected.peek() {
                    let number = index
                        .statements
                        .iter()
                        .position(|statement| statement.start <= offset && offset < statement.end)
                        .ok_or_else(|| AppError::invalid("SQL 失败定位不属于已校验语句"))?;
                    if index.statements[number].kind
                        != datapier_core::execution::sql_restore::SqlKind::Insert
                    {
                        return Err(AppError::invalid("SQL 失败记录定位不属于 INSERT 语句"));
                    }
                    datapier_core::execution::sql_restore::read_insert_statement(
                        &input.path,
                        &index,
                        number,
                        |columns, values, pos| {
                            if selected.peek() == Some(&pos) {
                                emit(json!({"byte_offset":pos,"columns":columns,"values":values}))?;
                                selected.next();
                            }
                            Ok(())
                        },
                    )?;
                    if selected.peek() == Some(&offset) {
                        return Err(AppError::invalid("未能在已校验输入中找到失败记录"));
                    }
                }
            }
        } else {
            let sql = datapier_core::execution::sql_restore::read_ddl(&input.path, &index, number)?;
            emit(json!({"byte_offset":statement.start,"sql":sql}))?;
        }
    } else {
        let first_record = item
            .locator
            .first_record
            .or(item.error_record)
            .ok_or_else(|| AppError::invalid("失败项缺少记录定位"))?;
        let last = item.locator.last_record.unwrap_or(first_record);
        let selected = |record: u64| -> bool {
            if record < first_record || record > last {
                return false;
            }
            if !item.item_states.is_empty() {
                return item.item_states.get((record - first_record) as usize)
                    == Some(&TransferStepState::Failed)
                    || item.error_record == Some(record);
            }
            item.error_record
                .map_or(item.state == TransferStepState::Failed, |number| {
                    number == record
                })
        };
        for record in first_record..=last {
            if !selected(record) {
                continue;
            }
            let bytes = data_files::read_raw_record(input, &fingerprint, spec.provider, record)?;
            if !first {
                write!(output, ",")?;
            }
            first = false;
            serde_json::to_writer(
                &mut *output,
                &RawRecord {
                    record,
                    encoding: "byte_array",
                    raw_bytes: &bytes,
                },
            )?;
        }
    }
    if first {
        return Err(AppError::invalid("未能在已校验输入中找到失败记录"));
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/transfer_sql_selection.rs"]
mod sql_selection_tests;
