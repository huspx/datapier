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
mod report;
mod retry;

#[cfg(test)]
pub(super) use retry::selected_file_records;

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
}

#[cfg(test)]
#[path = "../../tests/unit/transfer_sql_selection.rs"]
mod sql_selection_tests;
