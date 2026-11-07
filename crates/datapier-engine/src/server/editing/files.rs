//! 编辑文件登记、外部程序与一次性终端交接。
use super::*;
use datapier_core::model::editing::{
    EditFilePurpose, EditFileRegistration, EditFileState, ExternalEditorConfig, ExternalEditorMode,
};
use std::{
    io::{Read, Write},
    path::Path,
    process::Stdio,
};

#[derive(Default)]
pub(super) struct FileState {
    gate: Arc<AsyncMutex<()>>,
    owners: HashMap<Uuid, Uuid>,
    sessions: HashMap<Uuid, FileSession>,
    handoffs: HashMap<Uuid, Handoff>,
}
impl FileState {
    pub(super) fn has_session(&self, owner: Uuid) -> bool {
        self.sessions.values().any(|session| session.owner == owner)
            || self.handoffs.values().any(|handoff| handoff.owner == owner)
    }
}
struct FileSession {
    owner: Uuid,
    value: Uuid,
    value_revision: u64,
}
struct Handoff {
    owner: Uuid,
    file: Uuid,
    file_revision: u64,
    settings_revision: u64,
    config: ExternalEditorConfig,
    expires: Instant,
    helper: Option<Uuid>,
}

fn file_id(params: &Value, name: &str) -> Result<Uuid> {
    model::param(params, name)
}
fn revision(params: &Value) -> Result<u64> {
    model::param(params, "revision")
}
// 运行期草稿/值从 0 起始；持久登记使用 1 起始的关联编辑代次，不改变 IPC 的 CAS 修订。
fn file_draft_revision(current: u64) -> Result<u64> {
    current
        .checked_add(1)
        .filter(|n| *n <= i64::MAX as u64)
        .ok_or_else(|| AppError::invalid("编辑文件关联修订溢出"))
}
fn user_path(params: &Value) -> Result<PathBuf> {
    let path: String = model::param(params, "path")?;
    if path.is_empty() {
        return Err(AppError::invalid("文件路径不能为空"));
    }
    Ok(PathBuf::from(path))
}
fn is_regular(path: &Path) -> Result<std::fs::Metadata> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(AppError::invalid("只能使用普通文件"));
    }
    Ok(metadata)
}
fn stable_user_read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let before = is_regular(path)?;
    if before.len() > limit as u64 {
        return Err(AppError::new("resource_limit", "文件超过单值字节上限"));
    }
    let mut file = std::fs::File::open(path)?;
    let opened = file.metadata()?;
    if !opened.is_file()
        || !same_file(&opened, &before)
        || opened.len() != before.len()
        || opened.modified()? != before.modified()?
    {
        return Err(AppError::new("file_busy", "文件读取时发生变化"));
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    Read::by_ref(&mut file)
        .take(limit as u64)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    let current = is_regular(path)?;
    if bytes.len() > limit
        || !same_file(&before, &current)
        || after.len() != before.len()
        || after.modified()? != before.modified()?
        || current.len() != before.len()
        || current.modified()? != before.modified()?
    {
        return Err(AppError::new("file_busy", "文件读取时发生变化"));
    }
    Ok(bytes)
}
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        a.dev() == b.dev() && a.ino() == b.ino()
    }
    #[cfg(not(unix))]
    {
        a.created().ok() == b.created().ok()
            && a.len() == b.len()
            && a.modified().ok() == b.modified().ok()
    }
}
fn publish_file(path: &Path, bytes: &[u8], overwrite: bool) -> Result<()> {
    let target = if overwrite {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Some(is_regular(path)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        }
    } else {
        None
    };
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temp = parent.join(format!(".datapier-edit-export-{}", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if let Some(before) = target {
            // 发布前重新检查目标身份；同一用户并发改名仍受系统调用边界约束。
            let current = is_regular(path).map_err(|_| AppError::conflict("导出目标已变化"))?;
            if !same_file(&before, &current) {
                return Err(AppError::conflict("导出目标已变化"));
            }
            std::fs::rename(&temp, path)?;
        } else {
            std::fs::hard_link(&temp, path)?;
        }
        Ok(())
    })();
    match std::fs::remove_file(&temp) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) if result.is_ok() => return Err(e.into()),
        Err(_) => (),
    }
    result
}

impl Engine {
    pub(in crate::server) fn emit_file_changed(
        &self,
        record: &model::editing::EditorFileRecord,
        removed: bool,
    ) {
        let mut state = self.state.lock().unwrap();
        state.event_seq += 1;
        let mut scope = json!({"profile_id":record.profile_id});
        for key in ["database", "name"] {
            if let Some(value) = record.target.get(key).filter(|value| value.is_string()) {
                scope[key] = value.clone();
            }
        }
        let event = EngineEvent {
            seq: state.event_seq,
            kind: "value_file.changed".into(),
            task_id: record.id,
            payload: json!({"id":record.id,"revision":record.revision,"state":if removed { json!("removed") } else { json!(record.state) },"scope":scope}),
        };
        for sender in state.event_senders.values() {
            let _ = sender.send(event.clone());
        }
    }

    pub(super) async fn file_rpc_with_events(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let mutating = matches!(
            method,
            "value.external.open"
                | "value.external.launch"
                | "value.external.read"
                | "value.external.close"
                | "value.files.claim"
                | "value.files.cleanup"
        );
        let id = ["file_session", "file_id"]
            .iter()
            .find_map(|key| optional::<Uuid>(&params, key).ok().flatten());
        let before = if mutating {
            match id {
                Some(id) => self.store.edit_file(id).await.ok(),
                None => None,
            }
        } else {
            None
        };
        let result = self.file_rpc(owner, method, params).await?;
        if mutating {
            let id = id.or_else(|| optional::<Uuid>(&result, "file_session").ok().flatten());
            if let Some(id) = id {
                match self.store.edit_file(id).await {
                    Ok(record) => self.emit_file_changed(&record, false),
                    Err(_) => {
                        if let Some(record) = before {
                            self.emit_file_changed(&record, true);
                        }
                    }
                }
            }
        }
        Ok(result)
    }
    pub(in crate::server) fn files_gate(&self) -> Arc<AsyncMutex<()>> {
        self.state.lock().unwrap().editing.files.gate.clone()
    }
    fn file_owner(&self, owner: Uuid, id: Uuid) -> Result<()> {
        if self.state.lock().unwrap().editing.files.owners.get(&id) != Some(&owner) {
            return Err(AppError::new("permission_denied", "请先领取编辑文件"));
        }
        Ok(())
    }
    pub(in crate::server) fn editing_owns_file(&self, owner: Uuid, id: Uuid) -> bool {
        self.state.lock().unwrap().editing.files.owners.get(&id) == Some(&owner)
    }
    async fn file_bytes_limit(&self, owner: Uuid) -> Result<usize> {
        Ok(self
            .store
            .settings()
            .await?
            .editing
            .max_value_bytes
            .min(self.store.available_memory_for_owner(owner).await?))
    }
    /// 发布用户新值或类型化替换意图，登记先于文件创建。
    pub(super) async fn editing_publish_draft(
        &self,
        owner: Uuid,
        draft_revision: u64,
        format: &str,
        export: &ValueFileExport,
    ) -> Result<model::editing::EditorFileRecord> {
        self.ensure_editing_client(owner)?;
        let record = self
            .store
            .register_edit_file(EditFileRegistration {
                workspace_id: &export.workspace_id,
                profile_id: &export.profile_id.to_string(),
                purpose: EditFilePurpose::Draft,
                target: export.target.clone(),
                format,
                baseline_digest: &export.baseline_digest,
                draft_revision: file_draft_revision(draft_revision)?,
            })
            .await?;
        self.state
            .lock()
            .unwrap()
            .editing
            .files
            .owners
            .insert(record.id, owner);
        let result = self
            .store
            .publish_edit_file(
                record.id,
                record.revision,
                &export.bytes,
                export.bytes.len(),
            )
            .await;
        if result.is_err() {
            let _ = self
                .store
                .transition_edit_file(record.id, record.revision, EditFileState::Orphaned)
                .await;
            self.state
                .lock()
                .unwrap()
                .editing
                .files
                .owners
                .remove(&record.id);
        }
        if let Ok(record) = &result {
            self.emit_file_changed(record, false);
        }
        result
    }
    async fn reserved_export(
        self: &Arc<Self>,
        owner: Uuid,
        value: Uuid,
        revision: u64,
    ) -> Result<(ValueFileExport, Uuid)> {
        let summary = self
            .value_rpc(
                owner,
                "value.read",
                json!({"value_session":value,"revision":revision,"offset":0,"max_bytes":1}),
            )
            .await?;
        let size = summary
            .get("total_bytes")
            .and_then(Value::as_u64)
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| AppError::invalid("值摘要缺少字节数"))?;
        let handle = Uuid::new_v4();
        self.store
            .reserve_auxiliary_for_owner(owner, handle, size)
            .await?;
        match self.editing_export_value(owner, value, revision).await {
            Ok(export) => Ok((export, handle)),
            Err(error) => {
                self.store.release_auxiliary(handle).await?;
                Err(error)
            }
        }
    }
    pub(super) async fn file_rpc(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        if method != "value.editor.claim" && method != "value.editor.finish" {
            self.ensure_editing_client(owner)?;
        }
        let gate = self.files_gate();
        let _guard = gate.lock().await;
        match method {
            "value.file.export" => {
                let id = file_id(&params, "value_session")?;
                let path = user_path(&params)?;
                let overwrite = optional::<bool>(&params, "overwrite")?.unwrap_or(false);
                let (export, handle) = self.reserved_export(owner, id, revision(&params)?).await?;
                let result = publish_file(&path, &export.bytes, overwrite)
                    .map(|_| json!({"path":path,"bytes":export.bytes.len()}));
                drop(export);
                self.store.release_auxiliary(handle).await?;
                result
            }
            "value.file.import" => {
                let id = file_id(&params, "value_session")?;
                let rev = revision(&params)?;
                self.editing_check_value_baseline(owner, id).await?;
                let path = user_path(&params)?;
                let limit = self.file_bytes_limit(owner).await?;
                let size = usize::try_from(is_regular(&path)?.len())
                    .map_err(|_| AppError::new("resource_limit", "文件过大"))?;
                if size > limit {
                    return Err(AppError::new("resource_limit", "文件超过可用内存"));
                }
                let handle = Uuid::new_v4();
                self.store
                    .reserve_auxiliary_for_owner(owner, handle, size)
                    .await?;
                let result = match stable_user_read(&path, size) {
                    Ok(bytes) => self.editing_import_value(owner, id, rev, bytes).await,
                    Err(e) => Err(e),
                };
                self.store.release_auxiliary(handle).await?;
                result
            }
            "value.external.open" => {
                let value = file_id(&params, "value_session")?;
                let rev = revision(&params)?;
                let format: String = model::param(&params, "format")?;
                let (export, handle) = self.reserved_export(owner, value, rev).await?;
                let result = async {
                    let record = self.store.register_edit_file(EditFileRegistration { workspace_id:&export.workspace_id, profile_id:&export.profile_id.to_string(), purpose:EditFilePurpose::External, target:export.target.clone(), format:&format, baseline_digest:&export.baseline_digest, draft_revision:file_draft_revision(rev)? }).await?;
                    let record = match self.store.publish_edit_file(record.id, record.revision, &export.bytes, self.store.settings().await?.editing.max_value_bytes).await {
                        Ok(published) => published,
                        Err(error) => {
                            let _ = self.store.transition_edit_file(record.id, record.revision, EditFileState::Orphaned).await;
                            return Err(error);
                        }
                    };
                    let path = self.store.edit_file_path(record.id, record.revision).await?;
                    let mut state = self.state.lock().unwrap();
                    state.editing.files.owners.insert(record.id, owner);
                    state.editing.files.sessions.insert(record.id, FileSession { owner, value, value_revision: rev });
                    Ok(json!({"file_session":record.id,"revision":record.revision,"file_revision":record.revision,"path":path}))
                }.await;
                drop(export);
                self.store.release_auxiliary(handle).await?;
                result
            }
            "value.external.launch" => {
                let id = file_id(&params, "file_session")?;
                let rev = revision(&params)?;
                self.file_owner(owner, id)?;
                let record = self.store.edit_file(id).await?;
                if record.revision != rev {
                    return Err(AppError::conflict("编辑文件版本已变化"));
                }
                {
                    let mut state = self.state.lock().unwrap();
                    state
                        .editing
                        .files
                        .handoffs
                        .retain(|_, h| h.helper.is_some() || h.expires > Instant::now());
                    if state.editing.files.handoffs.values().any(|h| h.file == id) {
                        return Err(AppError::new("busy", "编辑器交接仍在使用"));
                    }
                }
                let path = self.store.edit_file_path(id, rev).await?;
                let settings = self.store.settings().await?;
                let config = settings
                    .external_editor
                    .ok_or_else(|| AppError::invalid("尚未配置外部编辑器"))?;
                let args = config.arguments(&path)?;
                match config.mode {
                    ExternalEditorMode::Gui => {
                        let mut child = std::process::Command::new(&config.program)
                            .args(args)
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn()?;
                        std::thread::spawn(move || {
                            let _ = child.wait();
                        });
                        let record = if record.state == EditFileState::Opened {
                            record
                        } else {
                            self.store
                                .transition_edit_file(id, rev, EditFileState::Opened)
                                .await?
                        };
                        Ok(
                            json!({"file_session":id,"revision":record.revision,"file_revision":record.revision,"mode":"gui"}),
                        )
                    }
                    ExternalEditorMode::Terminal => {
                        let available = self.state.lock().unwrap().windows.values().any(|window| {
                            window.owner == owner
                                && window.window_id.is_some()
                                && window.pane_id.is_some()
                                && window
                                    .seen
                                    .is_some_and(|seen| seen.elapsed() < Duration::from_secs(45))
                        });
                        if !available {
                            return Err(AppError::new(
                                "terminal_unavailable",
                                "当前窗口没有可用的终端桥接",
                            ));
                        }
                        let record = if record.state == EditFileState::Opened {
                            record
                        } else {
                            self.store
                                .transition_edit_file(id, rev, EditFileState::Opened)
                                .await?
                        };
                        let handoff = Uuid::new_v4();
                        self.state.lock().unwrap().editing.files.handoffs.insert(
                            handoff,
                            Handoff {
                                owner,
                                file: id,
                                file_revision: record.revision,
                                settings_revision: settings.revision,
                                config,
                                expires: Instant::now() + Duration::from_secs(60),
                                helper: None,
                            },
                        );
                        Ok(
                            json!({"file_session":id,"revision":record.revision,"file_revision":record.revision,"mode":"terminal","action":"value_editor","handoff":handoff}),
                        )
                    }
                }
            }
            "value.external.read" => {
                let id = file_id(&params, "file_session")?;
                let rev = revision(&params)?;
                self.file_owner(owner, id)?;
                let (value, value_revision) = {
                    let state = self.state.lock().unwrap();
                    let session = state
                        .editing
                        .files
                        .sessions
                        .get(&id)
                        .filter(|s| s.owner == owner)
                        .ok_or_else(|| AppError::conflict("编辑会话已失效"))?;
                    (session.value, session.value_revision)
                };
                let limit = self.file_bytes_limit(owner).await?;
                let handle = Uuid::new_v4();
                self.store
                    .reserve_auxiliary_for_owner(owner, handle, limit)
                    .await?;
                let result = match self.store.read_edit_file(id, rev, limit).await {
                    Ok(bytes) => match self
                        .store
                        .reserve_auxiliary_for_owner(owner, handle, bytes.len())
                        .await
                    {
                        Ok(()) => {
                            self.editing_import_value(owner, value, value_revision, bytes)
                                .await
                        }
                        Err(error) => Err(error),
                    },
                    Err(e) => Err(e),
                };
                self.store.release_auxiliary(handle).await?;
                let mut result = result?;
                let next_revision = result
                    .get("revision")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| AppError::invalid("值编辑结果缺少版本"))?;
                let record = self.store.edit_file(id).await?;
                let file_revision =
                    if matches!(record.state, EditFileState::Ready | EditFileState::Opened) {
                        self.store
                            .transition_edit_file(id, rev, EditFileState::Returned)
                            .await?
                            .revision
                    } else {
                        rev
                    };
                self.state
                    .lock()
                    .unwrap()
                    .editing
                    .files
                    .sessions
                    .get_mut(&id)
                    .unwrap()
                    .value_revision = next_revision;
                result["file_session"] = json!(id);
                result["file_revision"] = json!(file_revision);
                Ok(result)
            }
            "value.external.close" => {
                let id = file_id(&params, "file_session")?;
                let rev = revision(&params)?;
                self.file_owner(owner, id)?;
                if self
                    .state
                    .lock()
                    .unwrap()
                    .editing
                    .files
                    .handoffs
                    .values()
                    .any(|h| h.file == id && h.helper.is_some())
                {
                    return Err(AppError::new("busy", "终端编辑器仍在运行"));
                }
                let keep = optional::<bool>(&params, "keep")?.unwrap_or(false);
                if keep {
                    self.store
                        .transition_edit_file(id, rev, EditFileState::Orphaned)
                        .await?;
                } else {
                    self.store.cleanup_edit_file(id, rev).await?;
                }
                let mut state = self.state.lock().unwrap();
                state.editing.files.owners.remove(&id);
                state.editing.files.sessions.remove(&id);
                state.editing.files.handoffs.retain(|_, h| h.file != id);
                Ok(json!({"removed":!keep}))
            }
            "value.files.list" => {
                let offset = optional::<u64>(&params, "offset")?.unwrap_or(0);
                let limit = optional::<u64>(&params, "limit")?.unwrap_or(50);
                if limit == 0 || limit > 500 {
                    return Err(AppError::invalid("编辑文件分页参数无效"));
                }
                let workspace = optional::<String>(&params, "workspace_id")?;
                let state_filter = optional::<EditFileState>(&params, "state")?;
                let purpose = optional::<EditFilePurpose>(&params, "purpose")?;
                let mut cursor = offset;
                let mut items = Vec::new();
                let mut exhausted = false;
                while items.len() < limit as usize {
                    let batch = self.store.list_edit_files(cursor, 100).await?;
                    if batch.is_empty() {
                        exhausted = true;
                        break;
                    }
                    for record in batch {
                        cursor += 1;
                        if workspace
                            .as_ref()
                            .is_some_and(|v| v != &record.workspace_id)
                            || state_filter.is_some_and(|v| v != record.state)
                            || purpose.is_some_and(|v| v != record.purpose)
                        {
                            continue;
                        }
                        let active = self
                            .state
                            .lock()
                            .unwrap()
                            .editing
                            .files
                            .owners
                            .contains_key(&record.id);
                        let referenced = self.store.edit_file_referenced(record.id).await?;
                        let mut item = serialize(record)?;
                        item["active"] = json!(active);
                        item["referenced"] = json!(referenced);
                        items.push(item);
                        if items.len() == limit as usize {
                            break;
                        }
                    }
                }
                Ok(
                    json!({"items":items,"offset":offset,"next":if exhausted {None} else {Some(cursor)}}),
                )
            }
            "value.files.get" => {
                let id = file_id(&params, "file_id")?;
                let record = self.store.edit_file(id).await?;
                let active = self
                    .state
                    .lock()
                    .unwrap()
                    .editing
                    .files
                    .owners
                    .contains_key(&id);
                Ok(
                    json!({"record":record,"active":active,"referenced":self.store.edit_file_referenced(id).await?}),
                )
            }
            "value.files.claim" => {
                let id = file_id(&params, "file_id")?;
                if self
                    .state
                    .lock()
                    .unwrap()
                    .editing
                    .files
                    .owners
                    .contains_key(&id)
                {
                    return Err(AppError::new("busy", "编辑文件正在使用"));
                }
                let record = self.store.claim_edit_file(id, revision(&params)?).await?;
                self.state
                    .lock()
                    .unwrap()
                    .editing
                    .files
                    .owners
                    .insert(id, owner);
                Ok(json!({"record":record}))
            }
            "value.files.export" => {
                let id = file_id(&params, "file_id")?;
                let rev = revision(&params)?;
                self.file_owner(owner, id)?;
                let path = user_path(&params)?;
                let overwrite = optional::<bool>(&params, "overwrite")?.unwrap_or(false);
                let limit = self.store.available_memory_for_owner(owner).await?;
                let handle = Uuid::new_v4();
                self.store
                    .reserve_auxiliary_for_owner(owner, handle, limit)
                    .await?;
                let result = match self.store.read_edit_file(id, rev, limit).await {
                    Ok(bytes) => publish_file(&path, &bytes, overwrite).map(|_| bytes.len()),
                    Err(e) => Err(e),
                };
                self.store.release_auxiliary(handle).await?;
                Ok(json!({"path":path,"bytes":result?}))
            }
            "value.files.cleanup" => {
                let id = file_id(&params, "file_id")?;
                self.file_owner(owner, id)?;
                if self
                    .state
                    .lock()
                    .unwrap()
                    .editing
                    .files
                    .handoffs
                    .values()
                    .any(|h| h.file == id && h.helper.is_some())
                {
                    return Err(AppError::new("busy", "终端编辑器仍在运行"));
                }
                self.store.cleanup_edit_file(id, revision(&params)?).await?;
                self.state.lock().unwrap().editing.files.owners.remove(&id);
                Ok(json!({"removed":true}))
            }
            "value.editor.claim" => self.editor_claim(owner, &params).await,
            "value.editor.finish" => self.editor_finish(owner, &params).await,
            _ => Err(AppError::new("method_not_found", "未知文件编辑方法")),
        }
    }

    async fn editor_claim(&self, owner: Uuid, params: &Value) -> Result<Value> {
        let helper = self
            .state
            .lock()
            .unwrap()
            .contexts
            .get(&owner)
            .is_some_and(|c| c.kind == model::ClientKind::ValueEditor);
        if !helper {
            return Err(AppError::new(
                "permission_denied",
                "只有编辑器 helper 可以领取交接",
            ));
        }
        let id = file_id(params, "handoff")?;
        let handoff = self
            .state
            .lock()
            .unwrap()
            .editing
            .files
            .handoffs
            .remove(&id)
            .ok_or_else(|| AppError::new("expired", "编辑器交接不存在或已消费"))?;
        if handoff.helper.is_some() || handoff.expires <= Instant::now() {
            return Err(AppError::new("expired", "编辑器交接已失效"));
        }
        let source_active = self
            .state
            .lock()
            .unwrap()
            .contexts
            .get(&handoff.owner)
            .is_some_and(|c| c.kind == model::ClientKind::Tsp);
        if !source_active || self.store.settings().await?.revision != handoff.settings_revision {
            return Err(AppError::conflict("源会话或编辑器配置已变化"));
        }
        self.file_owner(handoff.owner, handoff.file)?;
        let path = self
            .store
            .edit_file_path(handoff.file, handoff.file_revision)
            .await?;
        let args = handoff.config.arguments(&path)?;
        let program = handoff.config.program.clone();
        let file = handoff.file;
        let file_revision = handoff.file_revision;
        self.state.lock().unwrap().editing.files.handoffs.insert(
            id,
            Handoff {
                helper: Some(owner),
                ..handoff
            },
        );
        Ok(json!({"program":program,"args":args,"file_session":file,"file_revision":file_revision}))
    }
    async fn editor_finish(&self, owner: Uuid, params: &Value) -> Result<Value> {
        if !self
            .state
            .lock()
            .unwrap()
            .contexts
            .get(&owner)
            .is_some_and(|c| c.kind == model::ClientKind::ValueEditor)
        {
            return Err(AppError::new(
                "permission_denied",
                "只有编辑器 helper 可以结束交接",
            ));
        }
        let id = file_id(params, "handoff")?;
        let handoff = {
            let mut state = self.state.lock().unwrap();
            if !state
                .editing
                .files
                .handoffs
                .get(&id)
                .is_some_and(|h| h.helper == Some(owner))
            {
                return Err(AppError::new(
                    "permission_denied",
                    "编辑器交接不属于此客户端",
                ));
            }
            state.editing.files.handoffs.remove(&id).unwrap()
        };
        let status = params
            .get("status")
            .cloned()
            .ok_or_else(|| AppError::invalid("缺少编辑器退出状态"))?;
        let source_active = self
            .state
            .lock()
            .unwrap()
            .contexts
            .contains_key(&handoff.owner);
        if !source_active {
            let record = self.store.edit_file(handoff.file).await?;
            if matches!(
                record.state,
                EditFileState::Ready | EditFileState::Opened | EditFileState::Returned
            ) {
                self.store
                    .transition_edit_file(record.id, record.revision, EditFileState::Orphaned)
                    .await?;
            }
            self.state
                .lock()
                .unwrap()
                .editing
                .files
                .owners
                .remove(&handoff.file);
        }
        Ok(json!({"finished":true,"status":status}))
    }
    /// 释放断开的客户端的文件所有权；仍在运行的 helper 保留文件占用。
    pub(in crate::server) async fn editing_disconnect_files(&self, owner: Uuid) -> Result<()> {
        let gate = self.files_gate();
        let _guard = gate.lock().await;
        let files = {
            let mut state = self.state.lock().unwrap();
            state
                .editing
                .files
                .handoffs
                .retain(|_, h| h.owner != owner || h.helper.is_some());
            let helper_files: Vec<Uuid> = state
                .editing
                .files
                .handoffs
                .values()
                .filter(|h| h.helper == Some(owner))
                .map(|h| h.file)
                .collect();
            state
                .editing
                .files
                .handoffs
                .retain(|_, h| h.helper != Some(owner));
            state.editing.files.sessions.retain(|_, s| s.owner != owner);
            let owned: Vec<Uuid> = state
                .editing
                .files
                .owners
                .iter()
                .filter_map(|(&file, &holder)| (holder == owner).then_some(file))
                .chain(helper_files)
                .collect();
            owned
                .into_iter()
                .filter(|id| {
                    !state
                        .editing
                        .files
                        .handoffs
                        .values()
                        .any(|h| h.file == *id && h.helper.is_some())
                })
                .filter(|id| {
                    let holder = state.editing.files.owners.get(id).copied();
                    holder == Some(owner)
                        || holder.is_some_and(|holder| !state.contexts.contains_key(&holder))
                })
                .collect::<Vec<_>>()
        };
        let mut failure = None;
        for id in files {
            match self.store.edit_file(id).await {
                Ok(record) => {
                    if record.purpose == EditFilePurpose::External
                        && matches!(
                            record.state,
                            EditFileState::Preparing
                                | EditFileState::Ready
                                | EditFileState::Opened
                                | EditFileState::Returned
                        )
                        && let Err(error) = self
                            .store
                            .transition_edit_file(id, record.revision, EditFileState::Orphaned)
                            .await
                        && failure.is_none()
                    {
                        failure = Some(error);
                    }
                }
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
            self.state.lock().unwrap().editing.files.owners.remove(&id);
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(())
    }
}
