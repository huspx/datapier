use super::*;

#[derive(Default)]
pub(super) struct PreviewState {
    tab: Option<Uuid>,
    generation: u64,
    input: Option<Value>,
    pending: Option<Uuid>,
    value: Option<Value>,
    error: Option<String>,
}

// Redis 草稿保留 Engine 返回的文件登记；收集引用而不读取或内联文件内容。
fn redis_edit_files(
    value: &Value,
    files: &mut std::collections::BTreeSet<crate::grid::EditFileRef>,
) -> std::result::Result<(), i18n::UiMessage> {
    match value {
        Value::Object(map) => {
            if map.contains_key("edit_file_id") || map.contains_key("edit_file_revision") {
                let id = map
                    .get("edit_file_id")
                    .and_then(Value::as_str)
                    .and_then(|id| Uuid::parse_str(id).ok())
                    .ok_or_else(|| i18n::UiMessage::key("草稿文件引用无效"))?;
                let revision = map
                    .get("edit_file_revision")
                    .and_then(Value::as_u64)
                    .filter(|revision| *revision > 0)
                    .ok_or_else(|| i18n::UiMessage::key("草稿文件引用无效"))?;
                if files
                    .iter()
                    .any(|file| file.edit_file_id == id && file.edit_file_revision != revision)
                {
                    return Err(i18n::UiMessage::key("草稿文件版本不一致"));
                }
                files.insert(crate::grid::EditFileRef {
                    edit_file_id: id,
                    edit_file_revision: revision,
                });
            }
            for child in map.values() {
                redis_edit_files(child, files)?;
            }
        }
        Value::Array(items) => {
            for item in items {
                redis_edit_files(item, files)?;
            }
        }
        _ => {}
    }
    Ok(())
}

impl App {
    // 预览与提交使用完全相同的草稿及目标，行身份重复时整体拒绝。
    pub(super) fn change_input(&self) -> std::result::Result<Value, i18n::UiMessage> {
        let session = self
            .session_id()
            .ok_or_else(|| i18n::UiMessage::key("请先打开连接"))?;
        if let Some(input) = self.redis_key_delete_input()? {
            return Ok(input);
        }
        let target = self
            .target
            .as_ref()
            .ok_or_else(|| i18n::UiMessage::key("请先选择资源"))?;
        let token = self
            .edit_token
            .as_ref()
            .ok_or_else(|| i18n::UiMessage::key("请先打开编辑基线"))?;
        if self.is_redis() {
            if self.redis_changes.is_empty() {
                return Err(i18n::UiMessage::key("没有待提交的 Redis 操作"));
            }
            let mut files = std::collections::BTreeSet::new();
            for change in &self.redis_changes {
                redis_edit_files(change, &mut files)?;
            }
            Ok(
                json!({"session_id":session,"workspace_id":self.workspace_id,"target":target,"token":token,"changes":self.redis_changes,"edit_files":files}),
            )
        } else {
            let keys: Vec<String> = self
                .schema
                .as_ref()
                .and_then(|s| s.get("keys"))
                .and_then(Value::as_array)
                .and_then(|keys| keys.first())
                .and_then(|keys| serde_json::from_value(keys.clone()).ok())
                .unwrap_or_default();
            let value_sources = match &self.grid {
                Some(grid) => grid
                    .change_value_sources(self.pending_rows.len())
                    .map_err(workbench::grid_error)?,
                None => Vec::new(),
            };
            let mut rows = self.pending_rows.clone();
            if let Some(grid) = &self.grid {
                rows.extend(grid.changes(&keys).map_err(workbench::grid_error)?);
            }
            if rows.iter().enumerate().any(|(i, row)| {
                row.operation != "insert"
                    && rows[..i]
                        .iter()
                        .any(|old| old.operation != "insert" && old.key == row.key)
            }) {
                return Err(i18n::UiMessage::key("目标行已有待提交变更"));
            }
            if rows.is_empty() {
                return Err(i18n::UiMessage::key("没有待提交的行变更"));
            }
            let mut files = std::collections::BTreeMap::new();
            for file in self
                .pending_edit_files
                .iter()
                .copied()
                .chain(self.grid.iter().flat_map(|grid| grid.edit_files()))
            {
                if files
                    .insert(file.edit_file_id, file.edit_file_revision)
                    .is_some_and(|previous| previous != file.edit_file_revision)
                {
                    return Err(i18n::UiMessage::key("草稿文件版本不一致"));
                }
            }
            let files: Vec<_> = files
                .into_iter()
                .map(
                    |(edit_file_id, edit_file_revision)| crate::grid::EditFileRef {
                        edit_file_id,
                        edit_file_revision,
                    },
                )
                .collect();
            Ok(
                json!({"session_id":session,"workspace_id":self.workspace_id,"target":target,"token":token,"rows":rows,"edit_files":files,"value_sources":value_sources}),
            )
        }
    }

    fn change_preview_current(&self) -> bool {
        self.workbench.overlay.as_deref() == Some("preview")
            && self.paste_preview.is_none()
            && self
                .tabs
                .get(self.active)
                .is_some_and(|tab| Some(tab.id) == self.change_preview.tab)
            && self.generation == self.change_preview.generation
            && self.change_input().ok().as_ref() == self.change_preview.input.as_ref()
    }
    pub(super) fn change_preview_value(&self) -> Option<&Value> {
        self.change_preview_current()
            .then_some(self.change_preview.value.as_ref())
            .flatten()
    }
    pub(super) fn change_preview_loading(&self) -> bool {
        self.change_preview_current() && self.change_preview.pending.is_some()
    }
    pub(super) fn change_preview_error(&self) -> Option<&str> {
        self.change_preview_current()
            .then_some(self.change_preview.error.as_deref())
            .flatten()
    }
    pub(super) fn request_change_preview(&mut self, offset: u64, backend: &Backend) {
        let input = match self.change_input() {
            Ok(input) => input,
            Err(error) => {
                self.change_preview = PreviewState::default();
                self.status = error;
                return;
            }
        };
        let Some(tab) = self.tabs.get(self.active) else {
            return;
        };
        let id = tab.id;
        let token = Uuid::new_v4();
        self.change_preview = PreviewState {
            tab: Some(id),
            generation: self.generation,
            input: Some(input.clone()),
            pending: Some(token),
            value: None,
            error: None,
        };
        let mut params = input;
        params["offset"] = json!(offset);
        params["limit"] = json!(20);
        if let Err(error) = backend.request(
            format!("{id}:{}:change.preview/{token}", self.generation),
            "change.preview",
            params,
        ) {
            self.change_preview.pending = None;
            self.change_preview.error = Some(error.to_string());
        }
    }
    pub(super) fn change_preview_response(&mut self, token: &str, result: &Result<Value>) {
        if Uuid::parse_str(token).ok() != self.change_preview.pending
            || !self.change_preview_current()
        {
            return;
        }
        self.change_preview.pending = None;
        match result {
            Ok(value)
                if value.get("provider").and_then(Value::as_str).is_some()
                    && value
                        .get("transactional")
                        .and_then(Value::as_bool)
                        .is_some()
                    && value.get("offset").and_then(Value::as_u64).is_some()
                    && value.get("items").and_then(Value::as_array).is_some() =>
            {
                self.change_preview.value = Some(value.clone())
            }
            Ok(_) => self.change_preview.error = Some(i18n::tr("变更预览响应无效").into_owned()),
            Err(error) => self.change_preview.error = Some(error.to_string()),
        }
    }
    pub(super) fn change_preview_page(&mut self, next: bool, backend: &Backend) {
        let Some(value) = self.change_preview_value() else {
            return;
        };
        let offset = if next {
            value.get("next_offset").and_then(Value::as_u64)
        } else {
            value
                .get("offset")
                .and_then(Value::as_u64)
                .filter(|offset| *offset > 0)
                .map(|offset| offset.saturating_sub(20))
        };
        if let Some(offset) = offset {
            self.request_change_preview(offset, backend);
        }
    }
}
