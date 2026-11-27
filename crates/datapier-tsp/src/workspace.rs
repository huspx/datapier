//! 工作区快照、标签运行态交换与退出保存。
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::{RowChange, Value as DataValue};

mod row_restore;
pub(super) use row_restore::RowRestore;

/// 单次保存的冻结快照及有界分段传输状态。
pub(super) struct WorkspaceUpload {
    snapshot: Value,
    bytes: Vec<u8>,
    received: Vec<u8>,
    upload_id: Option<Uuid>,
    offset: usize,
    chunk_bytes: usize,
    revision: u64,
    destination: String,
    copy: bool,
    active_editors: bool,
    activating: bool,
}

fn workspace_redis_intent(mut change: Value) -> Value {
    if let Some(object) = change.as_object_mut()
        && object.remove("field_expires_at_ms").is_some()
    {
        object.insert("requires_field_expiry_refresh".into(), Value::Bool(true));
    }
    change
}

fn workspace_redis_check(changes: &[Value]) -> Result<()> {
    for change in changes {
        let operation = change
            .get("operation")
            .and_then(Value::as_str)
            .unwrap_or("");
        for name in [
            "field",
            "new_field",
            "member",
            "new_member",
            "new_key_b64",
            "value",
        ] {
            let Some(value) = change
                .get(name)
                .filter(|value| value.get("type").and_then(Value::as_str) == Some("large"))
            else {
                continue;
            };
            let id = value
                .pointer("/value/id")
                .and_then(Value::as_str)
                .and_then(|id| Uuid::parse_str(id).ok())
                .ok_or_else(|| AppError::invalid("大值草稿标识无效"))?;
            let identity = name == "field"
                || (name == "member"
                    && matches!(
                        operation,
                        "set_remove" | "set_replace_member" | "zset_remove" | "zset_replace_member"
                    ));
            let descriptor = match name {
                "new_field" => change.get("new_field_edit_file"),
                "new_key_b64" => change.get("new_key_edit_file"),
                _ if identity => change.get("identity_edit_file"),
                _ => Some(change),
            };
            let declared = descriptor.is_some_and(|file| {
                file.get("edit_file_id")
                    .and_then(Value::as_str)
                    .and_then(|id| Uuid::parse_str(id).ok())
                    == Some(id)
                    && file
                        .get("edit_file_revision")
                        .and_then(Value::as_u64)
                        .is_some_and(|revision| revision > 0)
            });
            if !declared {
                return Err(AppError::invalid("Redis 大值或身份缺少受控文件登记"));
            }
        }
    }
    Ok(())
}

impl App {
    pub(super) fn request_quit(&mut self, backend: &Backend) -> bool {
        // 按正常关闭流程检查每个标签，遇到待确认或异步操作时停留在该标签。
        let active = self.active;
        for index in std::iter::once(active).chain((0..self.tabs.len()).filter(|&i| i != active)) {
            self.switch_tab(index);
            if !self.finish_inline(true) {
                return false;
            }
            if self.workbench.project.is_pending() || self.workbench.transfer.busy() {
                self.status = i18n::UiMessage::key("请等待或取消该 tab 的任务后再关闭");
                return false;
            }
            if self.workbench.overlay.is_some() {
                if !self.workbench.overlay_close_confirm
                    && !self.advanced_value.confirm_close
                    && self.archery.discard_action.is_none()
                    && self.field("settings-reload-confirm") != "true"
                    && self.advanced_files.confirm_clean.is_none()
                {
                    self.action("overlay-close", backend);
                }
                if self.workbench.overlay.is_some() {
                    return false;
                }
            }
        }
        self.switch_tab(active);
        if let Err(error) = self.workspace_draft_scopes_check() {
            self.status = i18n::UiMessage::raw(error.to_string());
            return false;
        }
        // 每个标签的原位输入已在退出预检中暂存。
        if let Err(error) = self.workspace_draft_check() {
            self.status = i18n::UiMessage::raw(error.to_string());
            return false;
        }
        if !self.workspace_loaded && self.dirty {
            self.status = "工作区尚未加载，无法保存".into();
            return false;
        }
        self.save_workspace(backend);
        true
    }

    fn workspace_draft_scopes_check(&mut self) -> Result<()> {
        let active = self.active;
        let mut result = Ok(());
        for index in 0..self.tabs.len() {
            self.switch_tab(index);
            if let Err(error) = self.draft_scope_save_check() {
                result = Err(error);
                break;
            }
        }
        self.switch_tab(active);
        result
    }
    // 没有可靠键或已登记文件时不能把行号当作身份写入工作区。
    fn workspace_draft_check(&self) -> Result<()> {
        for (index, tab) in self.tabs.iter().enumerate() {
            let (grid, schema, pending_rows, pending_files) = if index == self.active {
                (
                    &self.grid,
                    &self.schema,
                    &self.pending_rows,
                    &self.pending_edit_files,
                )
            } else {
                (
                    &tab.runtime.grid,
                    &tab.runtime.schema,
                    &tab.runtime.pending_rows,
                    &tab.runtime.pending_edit_files,
                )
            };
            let redis = if index == self.active {
                &self.redis_changes
            } else {
                &tab.runtime.redis_changes
            };
            workspace_redis_check(redis)?;
            if !tab.pending_live
                && let Some(redis) = tab.pending.get("redis").and_then(Value::as_array)
            {
                workspace_redis_check(redis)?;
            }
            if !tab.pending_live
                && tab
                    .pending
                    .get("edits")
                    .and_then(Value::as_array)
                    .is_some_and(|edits| !edits.is_empty())
            {
                return Err(AppError::invalid("旧草稿缺少可靠行标识，请先恢复或丢弃"));
            }
            if !tab.pending_live
                && tab
                    .pending
                    .get("rows")
                    .and_then(Value::as_array)
                    .is_some_and(|rows| {
                        rows.iter().any(|row| {
                            row.get("before")
                                .and_then(Value::as_object)
                                .is_some_and(|before| !before.is_empty())
                        })
                    })
            {
                return Err(AppError::invalid("旧草稿包含原始基线，请先恢复或丢弃"));
            }
            if !tab.pending_live
                && let Some(rows) = tab.pending.get("rows").and_then(Value::as_array)
            {
                let saved_files: Vec<grid::EditFileRef> = serde_json::from_value(
                    tab.pending
                        .get("edit_files")
                        .cloned()
                        .unwrap_or_else(|| json!([])),
                )?;
                for row in rows {
                    let row: RowChange = serde_json::from_value(row.clone())?;
                    if row.operation != "insert"
                        && (row.key.is_empty()
                            || row.key.values().any(|value| {
                                matches!(
                                    value,
                                    DataValue::Null | DataValue::Missing | DataValue::Large { .. }
                                )
                            }))
                    {
                        return Err(AppError::invalid("保存草稿缺少可靠行标识"));
                    }
                    for value in row.values.values() {
                        if let DataValue::Large { id, .. } = value
                            && !saved_files
                                .iter()
                                .any(|file| file.edit_file_id == *id && file.edit_file_revision > 0)
                        {
                            return Err(AppError::invalid("大值草稿缺少受控文件登记"));
                        }
                    }
                }
            }
            for row in pending_rows {
                if row.operation != "insert"
                    && (row.key.is_empty()
                        || row.key.values().any(|value| {
                            matches!(
                                value,
                                DataValue::Null | DataValue::Missing | DataValue::Large { .. }
                            )
                        }))
                {
                    return Err(AppError::invalid("保存草稿缺少可靠行标识"));
                }
                for value in row.values.values() {
                    if let DataValue::Large { id, .. } = value
                        && !pending_files.iter().any(|file| file.edit_file_id == *id)
                    {
                        return Err(AppError::invalid("大值草稿缺少受控文件登记"));
                    }
                }
            }
            if let Some(grid) = grid.as_ref().filter(|grid| !grid.edits().is_empty()) {
                let keys: Vec<String> = schema
                    .as_ref()
                    .and_then(|schema| schema.get("keys"))
                    .and_then(Value::as_array)
                    .and_then(|keys| keys.first())
                    .and_then(|keys| serde_json::from_value(keys.clone()).ok())
                    .unwrap_or_default();
                let rows = grid.changes(&keys)?;
                let files = grid.edit_files();
                for value in rows.iter().flat_map(|row| row.values.values()) {
                    if let datapier_core::model::Value::Large { id, .. } = value
                        && !files.iter().any(|file| file.edit_file_id == *id)
                    {
                        return Err(AppError::invalid("大值草稿缺少受控文件登记"));
                    }
                }
            }
        }
        Ok(())
    }

    // 持久化编辑意图和布局，而非可继续提交的数据库基线；恢复后必须刷新并重新确认。
    pub(super) fn workspace_snapshot(&mut self) -> Workspace {
        self.capture_browse();
        let key_intents: BTreeMap<_, _> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                self.redis_key_delete_snapshot(tab.id)
                    .map(|intent| (tab.id, intent))
            })
            .collect();
        let mut saved_pending = BTreeMap::new();
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let (target, grid, rows, redis, schema, pending_files) = if index == self.active {
                (
                    &self.target,
                    &self.grid,
                    &self.pending_rows,
                    &self.redis_changes,
                    &self.schema,
                    &self.pending_edit_files,
                )
            } else {
                (
                    &tab.runtime.target,
                    &tab.runtime.grid,
                    &tab.runtime.pending_rows,
                    &tab.runtime.redis_changes,
                    &tab.runtime.schema,
                    &tab.runtime.pending_edit_files,
                )
            };
            if let Some(g) = grid {
                tab.layout = g.settings.clone();
            }
            if let Some(intent) = key_intents.get(&tab.id) {
                tab.pending = intent.clone();
                tab.pending_live = true;
            } else if grid.as_ref().is_some_and(|g| !g.edits().is_empty())
                || grid
                    .as_ref()
                    .is_some_and(|g| !g.retained_edit_files().is_empty())
                || !rows.is_empty()
                || !redis.is_empty()
            {
                let mut changes = rows.clone();
                let mut fallback = Vec::new();
                if let Some(g) = grid
                    && !g.edits().is_empty()
                {
                    let keys: Vec<String> = schema
                        .as_ref()
                        .and_then(|s| s.get("keys"))
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    match g.changes(&keys) {
                        Ok(updates) => changes.extend(updates),
                        Err(_) => fallback = g.edits().iter().map(|(cell, after)|
                            json!({"row":cell.row,"column":cell.column,"after":after})).collect(),
                    }
                }
                // 只保存目标身份和用户新值；完整行基线永远不进入工作区。
                let mut intent_rows: Vec<Value> = changes
                    .iter()
                    .map(|change| {
                        json!({
                            "operation":change.operation,"key":change.key,"values":change.values
                        })
                    })
                    .collect();
                let mut intent_redis: Vec<Value> =
                    redis.iter().cloned().map(workspace_redis_intent).collect();
                let mut files = grid
                    .as_ref()
                    .map_or_else(Vec::new, |grid| grid.retained_edit_files());
                files.extend(pending_files.iter().copied());
                files.sort();
                files.dedup();
                if !tab.pending_live && !tab.pending.is_null() {
                    if let Some(saved) = tab.pending.get("rows").and_then(Value::as_array) {
                        intent_rows.extend(saved.iter().cloned().map(|mut row| {
                            if let Some(object) = row.as_object_mut() {
                                object.remove("before");
                            }
                            row
                        }));
                    }
                    if let Some(saved) = tab.pending.get("redis").and_then(Value::as_array) {
                        intent_redis.extend(saved.iter().cloned().map(workspace_redis_intent));
                    }
                    if let Some(saved) = tab.pending.get("edit_files").cloned()
                        && let Ok(saved) = serde_json::from_value::<Vec<grid::EditFileRef>>(saved)
                    {
                        files.extend(saved);
                    }
                    files.sort();
                    files.dedup();
                }
                let value = json!({"target":target,"rows":intent_rows,"redis":intent_redis,"edit_files":files,"rows_include_edits":fallback.is_empty(),"edits":fallback,"requires_refresh":true});
                if !tab.pending_live && !tab.pending.is_null() {
                    saved_pending.insert(tab.id, value);
                } else {
                    tab.pending = value;
                    tab.pending_live = true;
                }
            } else if tab.pending_live {
                tab.pending = Value::Null;
                tab.pending_live = false;
            }
        }
        Workspace {
            tabs: self
                .tabs
                .iter()
                .enumerate()
                .map(|(index, tab)| {
                    let redis = self.profiles.iter().any(|profile| {
                        Some(profile.id) == tab.profile_id
                            && profile.provider == datapier_core::model::ProviderKind::Redis
                    });
                    // 认证命令不进入工作区；只过滤序列化副本，保留内存输入供用户修改。
                    // 同时覆盖旧版将 Redis CLI 文本保存在 query 中的工作区。
                    let saved_editor = |editor: &Editor| {
                        if redis
                            && datapier_core::providers::redis_contains_auth(&editor.text)
                                .unwrap_or(true)
                        {
                            Editor::default()
                        } else {
                            editor.clone()
                        }
                    };
                    Tab {
                        title: tab.title.clone(),
                        default_query_ordinal: tab.default_query_ordinal,
                        query: saved_editor(&tab.query),
                        cli_query: saved_editor(&tab.cli_query),
                        profile_id: tab.profile_id,
                        id: tab.id,
                        layout: tab.layout.clone(),
                        project_query: tab.project_query.clone(),
                        project_columns: tab.project_columns.clone(),
                        browse: tab.browse.clone(),
                        schema_draft: self
                            .advanced_schema_draft(index)
                            .or_else(|| tab.schema_draft.clone()),
                        replace_draft: if index == self.active {
                            self.replace_draft()
                        } else {
                            tab.runtime
                                .advanced_replace
                                .draft()
                                .map(|intent| json!({"profile_id":tab.profile_id,"intent":intent}))
                        }
                        .or_else(|| tab.replace_draft.clone()),
                        pending: saved_pending.get(&tab.id).cloned().unwrap_or_else(|| {
                            let mut pending = tab.pending.clone();
                            if let Some(rows) =
                                pending.get_mut("rows").and_then(Value::as_array_mut)
                            {
                                for row in rows {
                                    if let Some(object) = row.as_object_mut() {
                                        object.remove("before");
                                    }
                                }
                            }
                            if let Some(redis) =
                                pending.get_mut("redis").and_then(Value::as_array_mut)
                            {
                                for change in redis {
                                    *change = workspace_redis_intent(std::mem::take(change));
                                }
                            }
                            pending
                        }),
                        workbench_draft: {
                            let (workbench, view) = if index == self.active {
                                (&self.workbench, &self.view)
                            } else {
                                (&tab.runtime.workbench, &tab.runtime.view)
                            };
                            let project = &workbench.project;
                            let saved_project = project.project.as_ref().map(|p| json!({
                                "root": p["root"], "manifest": p["manifest"], "digests": p["digests"], "baseline_manifest": project.baseline_manifest
                            })).or_else(|| project.restored.clone());
                            let project_prefix = format!("project-{}-", tab.id);
                            let transfer_prefix = format!("transfer-{}-", tab.id);
                            let fields: serde_json::Map<String, Value> = self.fields.iter()
                                .filter(|(key, _)| key.starts_with(&project_prefix) || key.starts_with(&transfer_prefix))
                                // 授权确认属于一次预览，不能随草稿恢复。
                                .map(|(key, editor)| (key.clone(), if key.strip_prefix(&transfer_prefix) == Some("triggers-confirmed") { json!("false") } else { json!(editor.text) }))
                                .collect();
                            json!({"view":view,"fields":fields,"project":saved_project,
                                "project_unsaved":project.unsaved,"transfer_files":project.transfer_files,
                                "project_section":project.section,"project_git_query_edit":project.git_query_edit,
                                "transfer_files_count":workbench.transfer.files,
                                "transfer_objects_count":workbench.transfer.objects,
                                "transfer_stage":workbench.transfer.stage,
                                "transfer_object_filters":workbench.transfer.object_filters,
                                "transfer_redis_keys":workbench.transfer.redis_keys,
                                "transfer_project_mode":workbench.transfer.project_mode})
                        },
                        pending_live: tab.pending_live,
                        runtime: TabRuntime::default(),
                    }
                })
                .collect(),
            active: self.active,
            project_splits: self.project_layout.groups.clone(),
            pending: Value::Null,
            navigation_expanded: self.navigation.expanded,
            empty_context: if self.tabs.is_empty() {
                self.empty_context.clone()
            } else {
                None
            },
        }
    }
    // 保存携带已知 revision；失败后阻止自动重试，避免覆盖另一个 UI 的修改。
    // 请求期间的新修改会再次置 dirty，待确认后按新 revision 保存。
    fn begin_workspace_upload(
        &mut self,
        backend: &Backend,
        destination: String,
        copy: bool,
    ) -> Result<()> {
        let active_editors = copy && self.workspace_copy_has_active_editors();
        let snapshot = serde_json::to_value(self.workspace_snapshot())?;
        let bytes = serde_json::to_vec(&snapshot)?;
        backend.request("workspace.upload.begin", "workspace.save.begin", json!({"workspace_id":destination,"expected_revision":if copy {0} else {self.revision},"bytes":bytes.len(),"copy_from":if copy {Some(self.workspace_id.clone())} else {None}}))?;
        self.workspace_upload = Some(WorkspaceUpload {
            snapshot,
            bytes,
            received: Vec::new(),
            upload_id: None,
            offset: 0,
            chunk_bytes: 0,
            revision: 0,
            destination,
            copy,
            active_editors,
            activating: false,
        });
        self.saving = true;
        self.dirty = false;
        Ok(())
    }
    pub(super) fn workspace_switch_pending(&self) -> bool {
        self.workspace_upload
            .as_ref()
            .is_some_and(|upload| upload.activating)
    }

    pub(super) fn workspace_upload_response(&mut self, response: Response, backend: &Backend) {
        let Some(mut upload) = self.workspace_upload.take() else {
            return;
        };
        let result = (|| -> Result<bool> {
            let value = response.result?;
            match response.tag.as_str() {
                "workspace.upload.begin" => {
                    upload.upload_id = Some(serde_json::from_value(value["upload_id"].clone())?);
                    upload.chunk_bytes = value["chunk_bytes"]
                        .as_u64()
                        .and_then(|v| usize::try_from(v).ok())
                        .filter(|v| *v > 0 && *v <= 256 * 1024)
                        .ok_or_else(|| AppError::invalid("工作区上传块大小无效"))?;
                }
                "workspace.upload.append" => {
                    let received = value["received"]
                        .as_u64()
                        .and_then(|v| usize::try_from(v).ok())
                        .ok_or_else(|| AppError::invalid("工作区上传偏移无效"))?;
                    if received <= upload.offset
                        || received > upload.bytes.len()
                        || received - upload.offset > upload.chunk_bytes
                    {
                        return Err(AppError::invalid("工作区上传偏移不匹配"));
                    }
                    upload.offset = received;
                }
                "workspace.upload.commit" => {
                    upload.revision = value["revision"]
                        .as_u64()
                        .filter(|v| *v > 0)
                        .ok_or_else(|| AppError::invalid("工作区版本无效"))?;
                    let total = value["bytes"]
                        .as_u64()
                        .and_then(|v| usize::try_from(v).ok())
                        .ok_or_else(|| AppError::invalid("工作区读取长度无效"))?;
                    upload
                        .received
                        .try_reserve_exact(total)
                        .map_err(|_| AppError::new("resource_limit", "无法预留工作区读取缓冲"))?;
                    upload.offset = 0;
                    upload.bytes = Vec::new();
                }
                "workspace.upload.read" => {
                    let chunk = STANDARD
                        .decode(
                            value["data_b64"]
                                .as_str()
                                .ok_or_else(|| AppError::invalid("工作区读取数据无效"))?,
                        )
                        .map_err(|_| AppError::invalid("工作区读取数据编码无效"))?;
                    if chunk.len() > 256 * 1024 {
                        return Err(AppError::invalid("工作区读取块过大"));
                    }
                    upload.received.extend_from_slice(&chunk);
                    let total = value["total_bytes"]
                        .as_u64()
                        .and_then(|v| usize::try_from(v).ok())
                        .ok_or_else(|| AppError::invalid("工作区读取长度无效"))?;
                    if upload.received.len() > total {
                        return Err(AppError::invalid("工作区读取长度不匹配"));
                    }
                    if value["next"].is_null() {
                        if upload.received.len() != total {
                            return Err(AppError::invalid("工作区读取未完成"));
                        }
                        let canonical: Value = serde_json::from_slice(&upload.received)?;
                        // 异步文件回执也可能修改草稿，不能只依赖 dirty 标志。
                        let unchanged = !upload.copy
                            || (!upload.active_editors
                                && !self.workspace_copy_has_active_editors()
                                && !self.dirty
                                && serde_json::to_value(self.workspace_snapshot())?
                                    == upload.snapshot);
                        if unchanged && upload.copy {
                            backend.request("workspace.upload.activate", "workspace.activate", json!({"workspace_id":upload.destination,"expected_workspace_id":self.workspace_id}))?;
                            upload.activating = true;
                            return Ok(false);
                        } else if unchanged {
                            self.rebind_workspace_drafts(&upload.snapshot, &canonical, false);
                            self.revision = upload.revision;
                            self.save_blocked = false;
                        } else {
                            self.dirty = true;
                            self.status = i18n::UiMessage::template(
                                "副本 {workspace} 已保存；当前编辑或任务尚未结束，继续使用原工作区",
                                vec![("workspace", i18n::UiArg::Raw(upload.destination.clone()))],
                            );
                        }
                        let _ = backend.request(
                            "workspace.cleanup",
                            "workspace.save.close",
                            json!({"upload_id":upload.upload_id}),
                        );
                        return Ok(true);
                    }
                    let next = value["next"].as_u64().and_then(|v| usize::try_from(v).ok());
                    if next != Some(upload.received.len()) || chunk.is_empty() {
                        return Err(AppError::invalid("工作区读取下一偏移无效"));
                    }
                }
                "workspace.upload.activate" => {
                    if value["workspace_id"].as_str() != Some(upload.destination.as_str()) {
                        return Err(AppError::invalid("工作区激活响应不匹配"));
                    }
                    let canonical: Value = serde_json::from_slice(&upload.received)?;
                    self.rebind_workspace_drafts(&upload.snapshot, &canonical, true);
                    let original =
                        std::mem::replace(&mut self.workspace_id, upload.destination.clone());
                    self.revision = upload.revision;
                    self.save_blocked = false;
                    self.status = i18n::UiMessage::template(
                        "已另存工作区副本 {copy}；原工作区 {original} 未覆盖",
                        vec![
                            ("copy", i18n::UiArg::Raw(upload.destination.clone())),
                            ("original", i18n::UiArg::Raw(original)),
                        ],
                    );
                    let _ = backend.request(
                        "workspace.cleanup",
                        "workspace.save.close",
                        json!({"upload_id":upload.upload_id}),
                    );
                    return Ok(true);
                }
                _ => return Ok(false),
            }
            let id = upload
                .upload_id
                .ok_or_else(|| AppError::invalid("工作区上传句柄无效"))?;
            if upload.revision != 0 {
                backend.request(
                    "workspace.upload.read",
                    "workspace.save.read",
                    json!({"upload_id":id,"offset":upload.received.len(),"max_bytes":256*1024}),
                )?;
            } else if upload.offset < upload.bytes.len() {
                let end = upload
                    .offset
                    .saturating_add(upload.chunk_bytes)
                    .min(upload.bytes.len());
                backend.request("workspace.upload.append", "workspace.save.append", json!({"upload_id":id,"offset":upload.offset,"data_b64":STANDARD.encode(&upload.bytes[upload.offset..end])}))?;
            } else {
                backend.request(
                    "workspace.upload.commit",
                    "workspace.save.commit",
                    json!({"upload_id":id}),
                )?;
            }
            Ok(false)
        })();
        match result {
            Ok(true) => {
                self.saving = false;
                self.copy_pending = None;
            }
            Ok(false) => self.workspace_upload = Some(upload),
            Err(error) => {
                if let Some(id) = upload.upload_id {
                    let _ = backend.request(
                        "workspace.cleanup",
                        "workspace.save.abort",
                        json!({"upload_id":id}),
                    );
                }
                self.saving = false;
                self.copy_pending = None;
                self.dirty = true;
                self.save_blocked = true;
                self.status = i18n::UiMessage::raw(error.to_string());
            }
        }
    }

    pub(super) fn save_workspace(&mut self, backend: &Backend) {
        if !self.workspace_loaded || self.saving || self.save_blocked || !self.dirty {
            return;
        }
        // 工作区序列化临时占用剩余额度；项目请求结束后再保存，避免抢占其准备预算。
        if self.workbench.project.is_pending()
            || self.tabs.iter().enumerate().any(|(index, tab)| {
                index != self.active && tab.runtime.workbench.project.is_pending()
            })
        {
            return;
        }
        if let Err(error) = self.workspace_draft_scopes_check() {
            self.status = i18n::UiMessage::raw(error.to_string());
            return;
        }
        if let Err(error) = self
            .workspace_draft_check()
            .and_then(|_| self.begin_workspace_upload(backend, self.workspace_id.clone(), false))
        {
            self.status = i18n::UiMessage::raw(error.to_string());
            self.save_blocked = true;
        }
    }
    pub(super) fn save_workspace_copy(&mut self, backend: &Backend) {
        if self.saving || self.copy_pending.is_some() {
            self.status = "工作区保存仍在进行".into();
            return;
        }
        if let Err(error) = self.workspace_draft_scopes_check() {
            self.status = i18n::UiMessage::raw(error.to_string());
            return;
        }
        let id = format!("{}-copy-{}", self.workspace_id, Uuid::new_v4());
        match self
            .workspace_draft_check()
            .and_then(|_| self.begin_workspace_upload(backend, id.clone(), true))
        {
            Ok(()) => {
                self.copy_pending = Some(id);
                self.status = "正在另存工作区副本…".into();
            }
            Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
        }
    }
    pub(super) fn switch_tab(&mut self, next: usize) {
        if next == self.active || next >= self.tabs.len() {
            return;
        }
        self.capture_browse();
        macro_rules! swap_state {
            ($field:ident) => {
                std::mem::swap(&mut self.$field, &mut self.tabs[self.active].runtime.$field);
                std::mem::swap(&mut self.$field, &mut self.tabs[next].runtime.$field);
            };
        }
        swap_state!(profile);
        swap_state!(session);
        swap_state!(resources);
        swap_state!(cursor);
        swap_state!(scan_token);
        swap_state!(database);
        swap_state!(target);
        swap_state!(task);
        swap_state!(operation_pending);
        swap_state!(results);
        swap_state!(result_id);
        swap_state!(page_history);
        swap_state!(page_token);
        swap_state!(grid);
        swap_state!(schema);
        swap_state!(edit_token);
        swap_state!(pending_rows);
        swap_state!(restore_rows);
        swap_state!(pending_edit_files);
        swap_state!(draft_revision);
        swap_state!(advanced_schema);
        swap_state!(advanced_replace);
        swap_state!(advanced_value);
        swap_state!(advanced_files);
        swap_state!(redis_changes);
        swap_state!(redis_changes_revision);
        swap_state!(large);
        swap_state!(preview);
        swap_state!(preview_pending);
        swap_state!(paste_preview);
        swap_state!(detail);
        swap_state!(detail_context);
        swap_state!(browse_result);
        swap_state!(generation);
        swap_state!(current_pending);
        swap_state!(file_open_pending);
        swap_state!(query_edit_epoch);
        swap_state!(workbench);
        swap_state!(view);
        self.active = next;
        self.restore_browse();
        if self.profile.is_none() {
            self.profile = self.tabs[next]
                .profile_id
                .and_then(|id| self.profiles.iter().find(|p| p.id == id).cloned());
        }
        if self.view.is_empty() {
            self.view = if self.is_redis() { "data" } else { "query" }.into();
        }
        if self.detail.is_empty() && !self.tabs[next].pending.is_null() && self.edit_token.is_none()
        {
            self.detail = format!(
                "恢复的编辑意图（基线已失效，请刷新并重新确认）：{}",
                self.tabs[next].pending
            );
            self.set_recovery_detail_context();
        }
    }
    pub(super) fn migrate_cli_queries(&mut self) {
        for tab in &mut self.tabs {
            if tab.cli_query.text.is_empty()
                && !tab.query.text.is_empty()
                && self.profiles.iter().any(|profile| {
                    Some(profile.id) == tab.profile_id
                        && profile.provider == datapier_core::model::ProviderKind::Redis
                })
            {
                tab.cli_query = std::mem::take(&mut tab.query);
                self.dirty = true;
                tab.browse.cli_visible = true;
            }
        }
    }

    pub(super) fn current_redis_db(&self) -> u32 {
        self.tabs
            .get(self.active)
            .and_then(|tab| tab.browse.redis_db)
            .or_else(|| {
                self.empty_context
                    .as_ref()
                    .filter(|context| {
                        self.profile
                            .as_ref()
                            .is_some_and(|p| p.id == context.profile_id)
                    })
                    .and_then(|context| context.redis_db)
            })
            .unwrap_or_else(|| self.profile.as_ref().map_or(0, Profile::redis_db))
    }

    pub(super) fn capture_empty_context(&mut self) {
        self.empty_context = self.profile.as_ref().map(|profile| EmptyConnectionContext {
            profile_id: profile.id,
            database: self.database.clone(),
            redis_db: self.is_redis().then(|| self.current_redis_db()),
        });
    }

    pub(super) fn restore_empty_context(&mut self, clear_missing: bool) {
        if !self.tabs.is_empty() {
            return;
        }
        let profile = self.empty_context.as_ref().and_then(|context| {
            self.profiles
                .iter()
                .find(|profile| profile.id == context.profile_id)
                .cloned()
        });
        self.database = profile.as_ref().and_then(|_| {
            self.empty_context
                .as_ref()
                .and_then(|context| context.database.clone())
        });
        self.profile = profile;
        // 配置列表确认删除后清除悬空引用；首次工作区先于配置到达时保留待恢复目标。
        if clear_missing && self.profile.is_none() {
            self.empty_context = None;
        }
    }

    pub(super) fn empty_context_action(&mut self, name: &str, backend: &Backend) -> bool {
        if !self.tabs.is_empty() || self.profile.is_none() {
            return false;
        }
        if let Some(open) = name.strip_prefix("redis-db-menu-open:") {
            self.navigation.redis_db_menu = self.is_redis() && open == "true";
            return true;
        }
        if name.starts_with("redis-db:") && self.is_redis() {
            if name
                .strip_prefix("redis-db:")
                .and_then(|db| db.parse::<u32>().ok())
                == Some(self.current_redis_db())
            {
                self.navigation.redis_db_menu = false;
                return true;
            }
            self.create_query_tab();
            self.navigation_action(name, backend);
            return true;
        }
        if name == "open" {
            self.create_query_tab();
            self.action(name, backend);
            return true;
        }
        if name == "edit-profile" {
            let id = self.profile.as_ref().unwrap().id;
            self.edit_profile_after_close(id, false, backend);
            return true;
        }
        false
    }

    pub(super) fn capture_browse(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        let browse = BrowseSettings {
            pattern: self.field("pattern").into(),
            kind: self.field("type").into(),
            filter: self.field("filter").into(),
            sort: self.field("sort").into(),
            object_filter: self.field("object_filter").into(),
            row_search: self.field("row_search").into(),
            cli_visible: self.workbench.cli,
            database: self.database.clone(),
            target: self.target.clone(),
            redis_db: self.tabs[self.active].browse.redis_db.or_else(|| {
                self.profile
                    .as_ref()
                    .filter(|p| p.provider == datapier_core::model::ProviderKind::Redis)
                    .map(|p| p.redis_db())
            }),
        };
        self.tabs[self.active].browse = browse;
    }
    pub(super) fn restore_browse(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        let browse = self.tabs[self.active].browse.clone();
        self.field_set("pattern", browse.pattern);
        self.field_set("type", browse.kind);
        self.field_set("filter", browse.filter);
        self.field_set("sort", browse.sort);
        self.field_set("object_filter", browse.object_filter);
        self.field_set("row_search", browse.row_search);
        self.workbench.cli = browse.cli_visible;
        self.database = browse.database;
        self.target = browse.target;
    }
    // 用户选择 tab 要回到其内容；后台响应的临时切换仍只交换 runtime。
    pub(super) fn select_tab(&mut self, next: usize) {
        if next >= self.tabs.len() || !self.finish_inline(true) {
            return;
        }
        self.close_result_info(false);
        if next != self.active {
            self.archery.open_continuation = None;
            self.change_preview = change_preview::PreviewState::default();
        }
        self.workbench.overlay.take_if(|kind| kind == "history");
        self.switch_tab(next);
        self.workbench.overlay.take_if(|kind| kind == "history");
        self.focus_current_tab();
        self.dirty = true;
    }
    /// 用户切换需同时恢复键盘路由与宿主字段；后台临时 switch_tab 不触碰焦点。
    pub(super) fn focus_current_tab(&mut self) {
        if let Some(key) = self.project_split_editor_key() {
            self.focus = "query".into();
            self.workbench.focus_request = Some(key);
            return;
        }
        let (focus, key) = if self.is_redis() && self.workbench.cli && self.target.is_none() {
            ("cli_text", "input-cli_text")
        } else if !self.is_redis() && self.target.is_none() {
            ("query", "query-editor")
        } else {
            ("grid", "grid")
        };
        self.focus = focus.into();
        self.workbench.focus_request = Some(key.into());
    }
}
// 输入已关闭时无法再向用户询问：等待保存确认，原工作区失败则尝试另存副本。
// 如果保存或副本仍无法确认，返回错误而不是将退出视为成功。
pub(super) fn finish_after_input_closed(app: &mut App, backend: &mut Backend) -> Result<()> {
    let mut copy_attempted = false;
    loop {
        while let Some(reply) = backend.receive() {
            app.routed_response(reply, backend);
        }
        if app.save_blocked {
            if app.copy_pending.is_none() {
                if copy_attempted {
                    return Err(AppError::new(
                        "workspace_save",
                        app.status.render().into_owned(),
                    ));
                }
                copy_attempted = true;
                app.save_workspace_copy(backend);
                if app.copy_pending.is_none() {
                    return Err(AppError::new(
                        "workspace_save",
                        app.status.render().into_owned(),
                    ));
                }
            }
        } else if app.dirty {
            if app.workspace_loaded {
                app.save_workspace(backend);
            } else if backend.is_closed() {
                return Err(AppError::new(
                    "workspace_save",
                    "工作区未加载，无法保存草稿",
                ));
            }
        } else if !app.saving && app.copy_pending.is_none() {
            return Ok(());
        }
        let Some(reply) = backend.receive_blocking() else {
            return Err(AppError::new(
                "workspace_save",
                "Engine 连接已关闭，工作区保存未确认",
            ));
        };
        app.routed_response(reply, backend);
    }
}
