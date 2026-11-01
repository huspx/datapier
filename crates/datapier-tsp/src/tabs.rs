use super::*;

pub(super) struct CloseOperation {
    remaining: Vec<Uuid>,
    waiting: Option<(Uuid, Uuid)>,
    edit: Option<(Profile, bool)>,
}

impl App {
    pub(super) fn close_tabs(
        &mut self,
        ids: Vec<Uuid>,
        edit: Option<(Profile, bool)>,
        backend: &Backend,
    ) {
        if self.tab_close.is_some() {
            return;
        }
        // 先检查整个冻结集合；不得为了检查原位编辑而隐式提交它。
        for id in &ids {
            let Some(index) = self.tabs.iter().position(|tab| tab.id == *id) else {
                return;
            };
            let tab = &self.tabs[index];
            let (drafts, running) = if index == self.active {
                (
                    self.has_live_drafts() || self.workbench.inline.is_some(),
                    self.task.is_some()
                        || self.operation_pending
                        || self.workbench.cli_running
                        || self.workbench.connecting,
                )
            } else {
                let state = &tab.runtime;
                (
                    state.grid.as_ref().is_some_and(|g| !g.edits().is_empty())
                        || !state.pending_rows.is_empty()
                        || !state.redis_changes.is_empty()
                        || state.paste_preview.is_some()
                        || self.redis_key_delete_tab_pending(tab.id)
                        || state.workbench.inline.is_some(),
                    state.task.is_some()
                        || state.operation_pending
                        || state.workbench.cli_running
                        || state.workbench.connecting,
                )
            };
            if drafts || !tab.pending.is_null() {
                self.status = i18n::UiMessage::key("请先提交或丢弃该 tab 的待提交变更");
                return;
            }
            if running {
                self.status = i18n::UiMessage::key("请等待或取消该 tab 的任务后再关闭");
                return;
            }
        }
        self.tab_close = Some(CloseOperation {
            remaining: ids,
            waiting: None,
            edit,
        });
        self.close_tabs_next(backend);
    }

    pub(super) fn edit_profile_after_close(&mut self, id: Uuid, tls: bool, backend: &Backend) {
        let Some(profile) = self
            .profiles
            .iter()
            .find(|p| p.id == id)
            .cloned()
            .or_else(|| self.profile.as_ref().filter(|p| p.id == id).cloned())
        else {
            return;
        };
        let ids = self
            .tabs
            .iter()
            .filter(|tab| tab.profile_id == Some(id))
            .map(|tab| tab.id)
            .collect();
        self.close_tabs(ids, Some((profile, tls)), backend);
    }

    fn close_tabs_next(&mut self, backend: &Backend) {
        loop {
            let Some(operation) = self.tab_close.as_mut() else {
                return;
            };
            let Some(id) = operation.remaining.pop() else {
                let edit = self.tab_close.take().and_then(|op| op.edit);
                if let Some((profile, tls)) = edit {
                    self.profile_form(Some(profile));
                    if tls {
                        self.field_set("tls", "true");
                        self.workbench.profile_tls_details = true;
                    }
                }
                return;
            };
            let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
                continue;
            };
            let session = if index == self.active {
                &self.session
            } else {
                &self.tabs[index].runtime.session
            };
            if let Some(session) = session.as_ref().and_then(|s| s.get("session_id")).cloned() {
                let token = Uuid::new_v4();
                operation.waiting = Some((id, token));
                if let Err(error) = backend.request(
                    format!("tabs.close/{token}"),
                    "connection.close",
                    json!({"session_id":session}),
                ) {
                    self.tab_close = None;
                    self.status = i18n::UiMessage::raw(error.to_string());
                }
                return;
            }
            self.remove_closed_tab(index);
        }
    }

    pub(super) fn tabs_close_response(&mut self, response: &Response, backend: &Backend) -> bool {
        let Some(token) = response
            .tag
            .strip_prefix("tabs.close/")
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            return false;
        };
        let Some((id, expected)) = self.tab_close.as_ref().and_then(|op| op.waiting) else {
            return true;
        };
        if token != expected {
            return true;
        }
        if let Err(error) = &response.result
            && error.code != "not_found"
        {
            self.tab_close = None;
            self.status = i18n::UiMessage::raw(error.to_string());
            return true;
        }
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            self.remove_closed_tab(index);
        }
        if let Some(operation) = self.tab_close.as_mut() {
            operation.waiting = None;
        }
        self.close_tabs_next(backend);
        true
    }

    pub(super) fn tab_is_closing(&self, id: Uuid) -> bool {
        self.tab_close.as_ref().is_some_and(|op| {
            op.remaining.contains(&id) || op.waiting.is_some_and(|(tab, _)| tab == id)
        })
    }

    fn remove_closed_tab(&mut self, index: usize) {
        if index == self.active && self.tabs.len() > 1 {
            self.switch_tab(if index == 0 { 1 } else { index - 1 });
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            // 空工作区没有活动 runtime；后续迟到响应也无法再找到原 tab ID。
            self.profile = None;
            self.session = None;
            self.resources.clear();
            self.cursor = None;
            self.scan_token = None;
            self.database = None;
            self.target = None;
            self.task = None;
            self.operation_pending = false;
            self.results.clear();
            self.result_id = None;
            self.page_history.clear();
            self.page_token = None;
            self.grid = None;
            self.schema = None;
            self.edit_token = None;
            self.pending_rows.clear();
            self.redis_changes.clear();
            self.large = None;
            self.detail.clear();
            self.change_preview = change_preview::PreviewState::default();
            self.redis_changes_revision = 0;
            self.query_edit_epoch = 0;
            self.preview = None;
            self.preview_pending = None;
            self.paste_preview = None;
            self.browse_result = false;
            self.current_pending = None;
            self.file_open_pending = None;
            self.workbench = Workbench::default();
            self.view = "query".into();
            self.focus.clear();
            self.generation = self.generation.wrapping_add(1);
            self.active = 0;
        } else {
            if index < self.active {
                self.active -= 1;
            }
            self.focus_current_tab();
        }
        self.dirty = true;
    }
}
