//! 工作区快照的有界上传；输入、解析及规范化结果只在所属客户端的预算内存中存活。
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};

const CHUNK_BYTES: usize = 256 * 1024;
// 同时容纳一个输入帧的 Base64、解码块以及固定协议/登记元数据。
const WORK_BYTES: usize = 3 * CHUNK_BYTES + 16 * 1024;

#[derive(Default)]
pub(super) struct WorkspaceUploadState {
    entries: HashMap<Uuid, OwnedUpload>,
}

impl WorkspaceUploadState {
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

struct OwnedUpload {
    owner: Uuid,
    upload: Arc<AsyncMutex<Upload>>,
}

struct Upload {
    workspace: String,
    expected_revision: u64,
    copy_from: Option<String>,
    expected_bytes: usize,
    metadata_bytes: usize,
    bytes: Vec<u8>,
    charge: JsonCharge,
    phase: Phase,
    touched: Instant,
    closed: bool,
}

enum Phase {
    Receiving,
    Committed(u64),
    Failed(AppError),
}

/// 在分配 JSON 树之前计入结构节点；字符串内部的标点不是节点。
/// 三倍文本空间覆盖解码字符串、解析器 scratch 及序列化临时空间，
/// 结构计数额外覆盖 Value、数组容量及对象哈希表，不能仅按网络字节估算 JSON。
#[derive(Clone, Copy, Default)]
struct JsonCharge {
    estimate: model::editing::JsonMemoryEstimate,
}

impl JsonCharge {
    fn scan(&mut self, bytes: &[u8]) {
        self.estimate.feed(bytes);
    }

    fn reserved(&self, total: usize, _received: usize) -> usize {
        total
            .saturating_add(self.estimate.heap_bound())
            .saturating_add(WORK_BYTES)
    }
}

#[derive(Default)]
struct SmallSnapshotSize {
    bytes: usize,
    charge: JsonCharge,
}

impl std::io::Write for SmallSnapshotSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self
            .bytes
            .checked_add(bytes.len())
            .filter(|next| *next <= 256 * 1024)
            .ok_or_else(|| std::io::Error::other("工作区快照需要分段保存"))?;
        self.bytes = next;
        self.charge.scan(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
/// 先计算恢复树和编码缓冲的合计上界，再分配完整的响应字节。
struct LoadedSnapshotSize {
    bytes: usize,
    charge: JsonCharge,
    budget: usize,
}

impl std::io::Write for LoadedSnapshotSize {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self.bytes.saturating_add(bytes.len());
        let mut charge = self.charge;
        charge.scan(bytes);
        if charge.reserved(next, next) > self.budget {
            return Err(std::io::Error::other("工作区恢复超过客户端内存预算"));
        }
        self.bytes = next;
        self.charge = charge;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Engine {
    pub(super) async fn workspace_load(&self, owner: Uuid, params: Value) -> Result<Value> {
        self.ensure_editing_client(owner)?;
        let workspace: String = model::param(&params, "workspace_id")?;
        let id = Uuid::new_v4();
        let budget = self.store.available_memory_for_owner(owner).await?;
        self.store
            .reserve_auxiliary_for_owner(owner, id, budget)
            .await?;
        let result = self
            .load_workspace_reserved(owner, id, workspace, budget)
            .await;
        if !result
            .as_ref()
            .is_ok_and(|value| value.get("load_handle").is_some())
        {
            self.store.release_auxiliary(id).await?;
        }
        result
    }

    async fn load_workspace_reserved(
        &self,
        owner: Uuid,
        id: Uuid,
        workspace: String,
        budget: usize,
    ) -> Result<Value> {
        let metadata_bytes = workspace.capacity().saturating_add(
            std::mem::size_of::<Upload>() + std::mem::size_of::<OwnedUpload>() + 256,
        );
        let content_budget = budget
            .checked_sub(metadata_bytes.saturating_add(WORK_BYTES))
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| AppError::new("resource_limit", "客户端没有足够内存恢复工作区"))?;
        let gate = self.files_gate();
        let file_guard = gate.lock().await;
        let mut loaded = self
            .store
            .workspace_load_prepared(&workspace, content_budget)
            .await?;
        drop(file_guard);
        self.active(owner)?;
        let revision: u64 = model::param(&loaded, "revision")?;
        let mut size = LoadedSnapshotSize {
            bytes: 0,
            charge: JsonCharge::default(),
            budget: budget.saturating_sub(metadata_bytes),
        };
        serde_json::to_writer(&mut size, &loaded)
            .map_err(|_| AppError::new("resource_limit", "工作区恢复超过客户端内存预算"))?;
        let response_bytes = self
            .store
            .settings()
            .await?
            .limits
            .response_bytes
            .min(8 * 1024 * 1024);
        if size.bytes <= response_bytes.saturating_sub(512) {
            return Ok(loaded);
        }
        let state = loaded
            .as_object_mut()
            .and_then(|object| object.remove("state"))
            .ok_or_else(|| AppError::invalid("已保存工作区缺少状态"))?;
        drop(loaded);
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(size.bytes)
            .map_err(|_| AppError::new("resource_limit", "无法预留工作区恢复缓冲"))?;
        serde_json::to_writer(&mut bytes, &state)?;
        drop(state);
        self.store
            .reserve_auxiliary_for_owner(
                owner,
                id,
                bytes
                    .capacity()
                    .saturating_add(metadata_bytes)
                    .saturating_add(WORK_BYTES),
            )
            .await?;
        let total = bytes.len();
        let upload = Arc::new(AsyncMutex::new(Upload {
            workspace,
            expected_revision: revision,
            copy_from: None,
            expected_bytes: total,
            metadata_bytes,
            bytes,
            charge: size.charge,
            phase: Phase::Committed(revision),
            touched: Instant::now(),
            closed: false,
        }));
        let mut state = self.state.lock().unwrap();
        if !state.accepting || !state.clients.contains(&owner) {
            return Err(AppError::new("disconnected", "客户端会话已关闭"));
        }
        state
            .workspace_uploads
            .entries
            .insert(id, OwnedUpload { owner, upload });
        Ok(json!({"revision":revision,"load_handle":id,"bytes":total}))
    }

    fn workspace_upload(&self, owner: Uuid, id: Uuid) -> Result<Arc<AsyncMutex<Upload>>> {
        self.ensure_editing_client(owner)?;
        let state = self.state.lock().unwrap();
        let entry = state
            .workspace_uploads
            .entries
            .get(&id)
            .ok_or_else(|| AppError::new("handle_expired", "工作区保存句柄已失效"))?;
        if entry.owner != owner {
            return Err(AppError::new(
                "permission_denied",
                "工作区保存属于其他客户端",
            ));
        }
        Ok(entry.upload.clone())
    }

    fn workspace_upload_live(&self, id: Uuid, upload: &Arc<AsyncMutex<Upload>>) -> Result<()> {
        let state = self.state.lock().unwrap();
        if state
            .workspace_uploads
            .entries
            .get(&id)
            .is_some_and(|entry| Arc::ptr_eq(&entry.upload, upload))
        {
            Ok(())
        } else {
            Err(AppError::new("handle_expired", "工作区保存句柄已失效"))
        }
    }

    async fn begin_workspace_upload(&self, owner: Uuid, mut params: Value) -> Result<Value> {
        self.ensure_editing_client(owner)?;
        let workspace = match params.get_mut("workspace_id").map(std::mem::take) {
            Some(Value::String(value)) => value,
            _ => return Err(AppError::invalid("缺少工作区名称")),
        };
        let expected_revision: u64 = model::param(&params, "expected_revision")?;
        let total: usize = model::param(&params, "bytes")?;
        let copy_from = match params.get_mut("copy_from").map(std::mem::take) {
            None | Some(Value::Null) => None,
            Some(Value::String(value)) => Some(value),
            _ => return Err(AppError::invalid("工作区副本来源无效")),
        };
        if workspace.is_empty() || total == 0 {
            return Err(AppError::invalid("工作区名称与快照不能为空"));
        }
        if copy_from.as_deref() == Some(workspace.as_str()) {
            return Err(AppError::invalid("工作区副本必须使用不同名称"));
        }
        let id = Uuid::new_v4();
        let charge = JsonCharge::default();
        let metadata_bytes = workspace
            .capacity()
            .saturating_add(copy_from.as_ref().map_or(0, String::capacity))
            .saturating_add(
                std::mem::size_of::<Upload>() + std::mem::size_of::<OwnedUpload>() + 256,
            );
        self.store
            .reserve_auxiliary_for_owner(
                owner,
                id,
                charge.reserved(total, 0).saturating_add(metadata_bytes),
            )
            .await?;
        let mut bytes = Vec::new();
        if bytes.try_reserve_exact(total).is_err() {
            self.store.release_auxiliary(id).await?;
            return Err(AppError::new("resource_limit", "无法预留工作区保存缓冲"));
        }
        let upload = Arc::new(AsyncMutex::new(Upload {
            workspace,
            expected_revision,
            copy_from,
            expected_bytes: total,
            metadata_bytes,
            bytes,
            charge,
            phase: Phase::Receiving,
            touched: Instant::now(),
            closed: false,
        }));
        let inserted = {
            let mut state = self.state.lock().unwrap();
            if state.accepting && state.clients.contains(&owner) {
                state
                    .workspace_uploads
                    .entries
                    .insert(id, OwnedUpload { owner, upload });
                true
            } else {
                false
            }
        };
        if !inserted {
            self.store.release_auxiliary(id).await?;
            return Err(AppError::new("disconnected", "客户端会话已关闭"));
        }
        Ok(json!({"upload_id":id,"chunk_bytes":CHUNK_BYTES}))
    }

    async fn validate_workspace_file_sources(
        &self,
        owner: Uuid,
        destination: &str,
        source: Option<&str>,
        state: &Value,
    ) -> Result<Vec<Uuid>> {
        let refs = Store::edit_file_references(state)?;
        let mut files = Vec::with_capacity(refs.len());
        for (id, expected_revision) in refs {
            let record = self.store.edit_file(id).await?;
            let origin = source.unwrap_or(destination);
            if record.purpose != model::editing::EditFilePurpose::Draft
                || record.revision != expected_revision
                || (record.workspace_id != origin && record.workspace_id != destination)
            {
                return Err(AppError::conflict("草稿文件与保存工作区或版本不匹配"));
            }
            // 既有工作区可恢复其已提交引用；未提交文件必须仍由发起客户端持有。
            if !self.editing_owns_file(owner, id)
                && !self
                    .store
                    .edit_file_referenced_in_workspace(id, origin)
                    .await?
                && !self
                    .store
                    .edit_file_referenced_in_workspace(id, destination)
                    .await?
            {
                return Err(AppError::new(
                    "permission_denied",
                    "草稿文件不属于此保存请求",
                ));
            }
            files.push(id);
        }
        Ok(files)
    }

    async fn save_workspace_snapshot(
        &self,
        owner: Uuid,
        workspace: &str,
        expected_revision: u64,
        state: Value,
        copy_from: Option<&str>,
        memory_budget: usize,
    ) -> Result<(u64, Value)> {
        let gate = self.files_gate();
        let _file_guard = gate.lock().await;
        let old_files = self
            .validate_workspace_file_sources(owner, workspace, copy_from, &state)
            .await?;
        self.active(owner)?;
        let (revision, canonical) = self
            .store
            .workspace_save_prepared(
                workspace,
                expected_revision,
                state,
                copy_from,
                memory_budget,
            )
            .await?;
        // 文件状态只是重取提示，不能使已经提交的工作区因通知失败变成“保存失败”。
        let mut changed = old_files;
        match Store::edit_file_references(&canonical) {
            Ok(refs) => changed.extend(refs.into_keys()),
            Err(error) => tracing::warn!(code = %error.code, "已保存工作区的文件引用通知无法解析"),
        }
        changed.sort_unstable();
        changed.dedup();
        for id in changed {
            match self.store.edit_file(id).await {
                Ok(record) => {
                    use model::editing::EditFileState;
                    let state = match record.state {
                        EditFileState::Preparing => "preparing",
                        EditFileState::Ready => "ready",
                        EditFileState::Opened => "opened",
                        EditFileState::Returned => "returned",
                        EditFileState::Orphaned => "orphaned",
                        EditFileState::CleanupPending => "cleanup_pending",
                    };
                    self.emit_editing(owner, "value_file.changed", id, record.revision, state);
                }
                Err(error) if error.code == "not_found" => {
                    self.emit_editing(owner, "value_file.changed", id, 0, "removed")
                }
                Err(error) => {
                    tracing::warn!(file_id = %id, code = %error.code, "工作区保存后的文件状态通知查询失败")
                }
            }
        }
        Ok((revision, canonical))
    }

    async fn commit_workspace_upload(
        &self,
        owner: Uuid,
        id: Uuid,
        upload: &mut Upload,
    ) -> Result<(u64, Vec<u8>)> {
        if upload.bytes.len() != upload.expected_bytes {
            return Err(AppError::invalid("工作区快照尚未传输完整"));
        }
        let reserved = upload
            .charge
            .reserved(upload.expected_bytes, upload.expected_bytes)
            .saturating_add(upload.metadata_bytes);
        let budget = self
            .store
            .available_memory_for_owner(owner)
            .await?
            .saturating_add(reserved);
        self.store
            .reserve_auxiliary_for_owner(owner, id, budget)
            .await?;
        let memory_budget = budget
            .checked_sub(WORK_BYTES.saturating_add(upload.metadata_bytes))
            .ok_or_else(|| AppError::new("resource_limit", "客户端没有足够内存保存工作区"))?;
        let state: Value = serde_json::from_slice(&upload.bytes)
            .map_err(|error| AppError::invalid(format!("工作区快照不是有效 JSON: {error}")))?;
        // 解析预算已提前预留，释放网络原文后再复制文件/规范化，避免同时留两份快照。
        upload.bytes = Vec::new();
        let (revision, canonical) = self
            .save_workspace_snapshot(
                owner,
                &upload.workspace,
                upload.expected_revision,
                state,
                upload.copy_from.as_deref(),
                memory_budget,
            )
            .await?;
        Ok((revision, serde_json::to_vec(&canonical)?))
    }

    pub(super) async fn workspace_save_small(
        &self,
        owner: Uuid,
        mut params: Value,
    ) -> Result<Value> {
        self.ensure_editing_client(owner)?;
        let workspace = match params.get_mut("workspace_id").map(std::mem::take) {
            Some(Value::String(value)) if !value.is_empty() => value,
            _ => return Err(AppError::invalid("缺少工作区名称")),
        };
        let expected_revision: u64 = model::param(&params, "expected_revision")?;
        if params
            .get("copy_from")
            .is_some_and(|value| !value.is_null())
        {
            return Err(AppError::invalid("工作区副本请使用分段保存"));
        }
        let state = params
            .as_object_mut()
            .and_then(|params| params.remove("state"))
            .ok_or_else(|| AppError::invalid("缺少工作区快照"))?;
        let mut size = SmallSnapshotSize::default();
        serde_json::to_writer(&mut size, &state).map_err(|_| {
            AppError::new("resource_limit", "超过 256 KiB 的工作区快照请使用分段保存")
        })?;
        let id = Uuid::new_v4();
        let budget = self.store.available_memory_for_owner(owner).await?;
        let memory_budget = budget
            .checked_sub(WORK_BYTES.saturating_add(workspace.capacity()))
            .filter(|budget| *budget >= size.charge.estimate.heap_bound())
            .ok_or_else(|| AppError::new("resource_limit", "客户端没有足够内存保存工作区"))?;
        self.store
            .reserve_auxiliary_for_owner(owner, id, budget)
            .await?;
        let result = self
            .save_workspace_snapshot(
                owner,
                &workspace,
                expected_revision,
                state,
                None,
                memory_budget,
            )
            .await
            .map(|(revision, _)| json!({"revision":revision}));
        self.store.release_auxiliary(id).await?;
        result
    }

    pub(super) async fn workspace_save_rpc(
        &self,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        if method == "workspace.save.begin" {
            return self.begin_workspace_upload(owner, params).await;
        }
        let id: Uuid = model::param(&params, "upload_id")?;
        let entry = self.workspace_upload(owner, id)?;
        let mut upload = entry.lock().await;
        self.workspace_upload_live(id, &entry)?;
        if upload.closed {
            return Err(AppError::new("handle_expired", "工作区保存句柄已关闭"));
        }
        if matches!(method, "workspace.save.close" | "workspace.save.abort") {
            upload.closed = true;
            upload.bytes = Vec::new();
            self.state
                .lock()
                .unwrap()
                .workspace_uploads
                .entries
                .remove(&id);
            drop(upload);
            self.store.release_auxiliary(id).await?;
            return Ok(json!({"released":true}));
        }
        match method {
            "workspace.save.append" => {
                if !matches!(upload.phase, Phase::Receiving) {
                    return Err(AppError::conflict("工作区保存已结束，不能继续传输"));
                }
                let offset: usize = model::param(&params, "offset")?;
                let encoded = params
                    .get("data_b64")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AppError::invalid("缺少工作区数据块"))?;
                if encoded.len() > CHUNK_BYTES.div_ceil(3) * 4 {
                    return Err(AppError::new("resource_limit", "工作区数据块超过 256 KiB"));
                }
                let data = STANDARD
                    .decode(encoded)
                    .map_err(|_| AppError::invalid("工作区数据块不是有效 Base64"))?;
                if data.is_empty()
                    || data.len() > CHUNK_BYTES
                    || offset != upload.bytes.len()
                    || offset
                        .checked_add(data.len())
                        .is_none_or(|end| end > upload.expected_bytes)
                {
                    return Err(AppError::conflict("工作区数据块偏移或长度不匹配"));
                }
                let mut charge = upload.charge;
                charge.scan(&data);
                let received = offset + data.len();
                self.store
                    .reserve_auxiliary_for_owner(
                        owner,
                        id,
                        charge
                            .reserved(upload.expected_bytes, received)
                            .saturating_add(upload.metadata_bytes),
                    )
                    .await?;
                upload.bytes.extend_from_slice(&data);
                upload.charge = charge;
                upload.touched = Instant::now();
                Ok(json!({"received":received}))
            }
            "workspace.save.commit" => {
                match &upload.phase {
                    Phase::Committed(revision) => {
                        return Ok(json!({"revision":revision,"bytes":upload.bytes.len()}));
                    }
                    Phase::Failed(error) => return Err(error.clone()),
                    Phase::Receiving => {}
                }
                if upload.bytes.len() != upload.expected_bytes {
                    return Err(AppError::invalid("工作区快照尚未传输完整"));
                }
                match self.commit_workspace_upload(owner, id, &mut upload).await {
                    Ok((revision, bytes)) => {
                        upload.bytes = bytes;
                        upload.phase = Phase::Committed(revision);
                        upload.touched = Instant::now();
                        if let Err(error) = self
                            .store
                            .reserve_auxiliary_for_owner(
                                owner,
                                id,
                                upload
                                    .bytes
                                    .capacity()
                                    .saturating_add(upload.metadata_bytes)
                                    .saturating_add(WORK_BYTES),
                            )
                            .await
                        {
                            tracing::warn!(code = %error.code, "已保存工作区的缓冲预算缩减失败，保留原预留直到句柄关闭");
                        }
                        Ok(json!({"revision":revision,"bytes":upload.bytes.len()}))
                    }
                    Err(error) => {
                        upload.bytes = Vec::new();
                        upload.phase = Phase::Failed(error.clone());
                        upload.touched = Instant::now();
                        if let Err(shrink) = self
                            .store
                            .reserve_auxiliary_for_owner(
                                owner,
                                id,
                                upload.metadata_bytes.saturating_add(WORK_BYTES),
                            )
                            .await
                        {
                            tracing::warn!(code = %shrink.code, "失败工作区保存的预算缩减失败，保留原预留直到句柄关闭");
                        }
                        Err(error)
                    }
                }
            }
            "workspace.save.read" => {
                if !matches!(upload.phase, Phase::Committed(_)) {
                    return Err(AppError::conflict("工作区尚未完成保存"));
                }
                let offset: usize = model::param(&params, "offset")?;
                if offset > upload.bytes.len() {
                    return Err(AppError::invalid("工作区读取偏移超出范围"));
                }
                let requested = optional::<usize>(&params, "max_bytes")?.unwrap_or(CHUNK_BYTES);
                if requested == 0 {
                    return Err(AppError::invalid("工作区读取块大小必须为正"));
                }
                let limit = self.store.settings().await?.limits.response_bytes;
                // 固定摘要只含偏移/长度；512字节预留覆盖JSON字段、数字及IPC封装。
                let allowed = (limit.saturating_sub(512) / 4 * 3).min(CHUNK_BYTES);
                if allowed == 0 {
                    return Err(AppError::new("resource_limit", "响应预算不足以读取工作区"));
                }
                let end = offset
                    .saturating_add(requested.min(allowed))
                    .min(upload.bytes.len());
                let response = json!({"offset":offset,"total_bytes":upload.bytes.len(),"data_b64":STANDARD.encode(&upload.bytes[offset..end]),"next":if end<upload.bytes.len(){Some(end)}else{None}});
                upload.touched = Instant::now();
                Ok(response)
            }
            _ => Err(AppError::new("method_not_found", "未知工作区保存操作")),
        }
    }

    pub(super) async fn disconnect_workspace_uploads(&self, owner: Uuid) -> Result<()> {
        let entries: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .workspace_uploads
            .entries
            .iter()
            .filter(|(_, entry)| entry.owner == owner)
            .map(|(&id, entry)| (id, entry.upload.clone()))
            .collect();
        for (id, entry) in entries {
            let mut upload = entry.lock().await;
            upload.closed = true;
            upload.bytes = Vec::new();
            self.state
                .lock()
                .unwrap()
                .workspace_uploads
                .entries
                .remove(&id);
            drop(upload);
            self.store.release_auxiliary(id).await?;
        }
        Ok(())
    }

    pub(super) async fn sweep_workspace_uploads(&self, idle: Duration) -> Result<()> {
        let entries: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .workspace_uploads
            .entries
            .iter()
            .map(|(&id, entry)| (id, entry.upload.clone()))
            .collect();
        for (id, entry) in entries {
            if let Ok(mut upload) = entry.try_lock()
                && upload.touched.elapsed() >= idle
            {
                upload.closed = true;
                upload.bytes = Vec::new();
                self.state
                    .lock()
                    .unwrap()
                    .workspace_uploads
                    .entries
                    .remove(&id);
                drop(upload);
                self.store.release_auxiliary(id).await?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/server/workspace_uploads.rs"]
mod tests;
