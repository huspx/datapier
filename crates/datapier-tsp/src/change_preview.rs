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
            Ok(
                json!({"session_id":session,"workspace_id":self.workspace_id,"target":target,"token":token,"changes":self.redis_changes}),
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
            Ok(
                json!({"session_id":session,"workspace_id":self.workspace_id,"target":target,"token":token,"rows":rows}),
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
            Ok(value) => self.change_preview.value = Some(value.clone()),
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
