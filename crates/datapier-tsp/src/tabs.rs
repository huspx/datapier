use super::*;

pub(super) struct CloseOperation {
    remaining: Vec<Uuid>,
    waiting: Option<(Uuid, Uuid)>,
    edit: Option<(Profile, bool)>,
    after_action: Option<String>,
}

impl App {
    pub(crate) fn tab_has_advanced_work(&self, index: usize) -> bool {
        let tab = &self.tabs[index];
        let (schema, replace, value, files, restore_rows) = if index == self.active {
            (
                &self.advanced_schema,
                &self.advanced_replace,
                &self.advanced_value,
                &self.advanced_files,
                &self.restore_rows,
            )
        } else {
            let state = &tab.runtime;
            (
                &state.advanced_schema,
                &state.advanced_replace,
                &state.advanced_value,
                &state.advanced_files,
                &state.restore_rows,
            )
        };
        schema
            .as_ref()
            .is_some_and(|state| state.dirty || state.summary.is_some() || state.busy())
            || replace.target.is_some()
            || replace.has_live_plan()
            || value.session.is_some()
            || value.pending
            || files.file_session.is_some()
            || restore_rows.is_some()
            || tab.schema_draft.is_some()
            || self.navigation_database_create_for_tab(tab.id)
            || tab.replace_draft.is_some()
    }

    /// 仅创建并切换标签；由调用方决定是否先结束原位编辑。
    pub(super) fn create_query_tab(&mut self) {
        self.archery.open_continuation = None;
        let database = self.database.clone();
        let redis_db = self.is_redis().then(|| self.current_redis_db());
        self.tabs.push(Tab {
            title: format!("查询 {}", self.tabs.len() + 1),
            query: Editor::new(""),
            cli_query: Editor::default(),
            profile_id: self.profile.as_ref().map(|p| p.id),
            id: Uuid::new_v4(),
            layout: Vec::new(),
            browse: BrowseSettings {
                database,
                redis_db,
                ..BrowseSettings::default()
            },
            schema_draft: None,
            replace_draft: None,
            pending: Value::Null,
            pending_live: false,
            runtime: TabRuntime::default(),
        });
        self.tabs.last_mut().unwrap().runtime.profile = self.profile.clone();
        self.empty_context = None;
        self.switch_tab(self.tabs.len() - 1);
        self.focus_current_tab();
        self.dirty = true;
    }

    pub(super) fn close_tabs(
        &mut self,
        ids: Vec<Uuid>,
        edit: Option<(Profile, bool)>,
        backend: &Backend,
    ) {
        self.close_tabs_plan(ids, edit, None, backend);
    }

    pub(crate) fn close_profile_tabs_then(
        &mut self,
        profiles: &[Uuid],
        continuation: &str,
        backend: &Backend,
    ) -> bool {
        let ids = self
            .tabs
            .iter()
            .filter(|tab| tab.profile_id.is_some_and(|id| profiles.contains(&id)))
            .map(|tab| tab.id)
            .collect();
        self.close_tabs_plan(ids, None, Some(continuation.to_owned()), backend)
    }

    fn close_tabs_plan(
        &mut self,
        ids: Vec<Uuid>,
        edit: Option<(Profile, bool)>,
        after_action: Option<String>,
        backend: &Backend,
    ) -> bool {
        if self.tab_close.is_some() {
            return false;
        }
        // 先检查整个冻结集合；不得为了检查原位编辑而隐式提交它。
        for id in &ids {
            let Some(index) = self.tabs.iter().position(|tab| tab.id == *id) else {
                return false;
            };
            let tab = &self.tabs[index];
            if self.tab_has_advanced_work(index) {
                self.status = i18n::UiMessage::key("请先处理该标签的高级编辑草稿或会话");
                return false;
            }
            let (drafts, running) = if index == self.active {
                (
                    self.has_live_drafts()
                        || self.workbench.inline.is_some()
                        || self.workbench.draft_scope_pending.is_some()
                        || !self.workbench.draft_scope_recovery.is_empty(),
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
                        || state.workbench.inline.is_some()
                        || state.workbench.draft_scope_pending.is_some()
                        || !state.workbench.draft_scope_recovery.is_empty(),
                    state.task.is_some()
                        || state.operation_pending
                        || state.workbench.cli_running
                        || state.workbench.connecting,
                )
            };
            if drafts || !tab.pending.is_null() {
                self.status = i18n::UiMessage::key("请先提交或丢弃该 tab 的待提交变更");
                return false;
            }
            if running {
                self.status = i18n::UiMessage::key("请等待或取消该 tab 的任务后再关闭");
                return false;
            }
        }
        self.tab_close = Some(CloseOperation {
            remaining: ids,
            waiting: None,
            edit,
            after_action,
        });
        self.close_tabs_next(backend);
        true
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
        if let Some(source) = profile.archery() {
            self.archery_open_server(source.server_id, source.instance_id, backend);
            return;
        }
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
                let operation = self.tab_close.take().unwrap();
                if let Some((profile, tls)) = operation.edit {
                    self.profile_form(Some(profile));
                    if tls {
                        self.field_set("tls", "true");
                        self.workbench.profile_tls_details = true;
                    }
                }
                if let Some(action) = operation.after_action {
                    if let Some(id) = action
                        .strip_prefix("profile-close-finished:")
                        .and_then(|value| Uuid::parse_str(value).ok())
                    {
                        // 关闭收尾不能经过活动标签的草稿或弹窗拦截，否则旧目录会残留。
                        self.navigation_invalidate_profile(id);
                        self.status = i18n::UiMessage::key("连接已关闭");
                    } else if let Some(token) = action
                        .strip_prefix("profile-delete-tabs-closed:")
                        .and_then(|value| Uuid::parse_str(value).ok())
                    {
                        self.navigation_profile_delete_after_tabs(token, backend);
                    } else {
                        self.action(&action, backend);
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
                    let delete_token = operation
                        .after_action
                        .as_deref()
                        .and_then(|action| action.strip_prefix("profile-delete-tabs-closed:"))
                        .and_then(|value| Uuid::parse_str(value).ok());
                    if operation.after_action.is_some() {
                        self.archery.error = Some(error.to_string());
                    }
                    self.tab_close = None;
                    if let Some(token) = delete_token {
                        self.navigation_profile_delete_abort(token, error.to_string());
                    }
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
            let delete_token = self
                .tab_close
                .as_ref()
                .and_then(|operation| operation.after_action.as_deref())
                .and_then(|action| action.strip_prefix("profile-delete-tabs-closed:"))
                .and_then(|value| Uuid::parse_str(value).ok());
            if self
                .tab_close
                .as_ref()
                .is_some_and(|operation| operation.after_action.is_some())
            {
                self.archery.error = Some(error.to_string());
            }
            self.tab_close = None;
            if let Some(token) = delete_token {
                self.navigation_profile_delete_abort(token, error.to_string());
            }
            self.status = i18n::UiMessage::raw(error.to_string());
            return true;
        }
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
            let profile_id = self.tabs[index].profile_id;
            let protected_close = self
                .tab_close
                .as_ref()
                .and_then(|operation| operation.after_action.as_deref())
                .is_some_and(|action| {
                    action.starts_with("profile-delete-tabs-closed:")
                        || action.starts_with("profile-close-finished:")
                });
            if protected_close
                && profile_id
                    .is_some_and(|profile| self.navigation_profile_has_pending_work(profile))
            {
                // 服务端已关闭该会话；保留新草稿但禁止按旧基线提交。
                if index == self.active {
                    self.session = None;
                    self.generation = self.generation.wrapping_add(1);
                    if let Some(grid) = &mut self.grid {
                        grid.editable = false;
                    }
                } else {
                    let runtime = &mut self.tabs[index].runtime;
                    runtime.session = None;
                    runtime.generation = runtime.generation.wrapping_add(1);
                    if let Some(grid) = &mut runtime.grid {
                        grid.editable = false;
                    }
                }
                let delete_token = self
                    .tab_close
                    .as_ref()
                    .and_then(|operation| operation.after_action.as_deref())
                    .and_then(|action| action.strip_prefix("profile-delete-tabs-closed:"))
                    .and_then(|value| Uuid::parse_str(value).ok());
                self.tab_close = None;
                if let Some(token) = delete_token {
                    self.navigation_profile_delete_abort(
                        token,
                        "该连接存在草稿或任务，请先处理".into(),
                    );
                }
                self.status = i18n::UiMessage::key("该连接存在草稿或任务，请先处理");
                return true;
            }
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
        // 批量配置仍在管理弹窗中；关闭最后一个标签不能清掉待提交计划的展示层。
        let overlay = self
            .tab_close
            .as_ref()
            .filter(|operation| operation.after_action.is_some())
            .and_then(|_| self.workbench.overlay.clone());
        self.close_result_info(false);
        if self.tabs.len() == 1 {
            self.capture_empty_context();
        }
        if index == self.active && self.tabs.len() > 1 {
            self.archery.open_continuation = None;
            self.switch_tab(if index == 0 { 1 } else { index - 1 });
        }
        self.tabs.remove(index);
        if self.tabs.is_empty() {
            // 空工作区没有活动 runtime；后续迟到响应也无法再找到原 tab ID。
            self.session = None;
            self.resources.clear();
            self.cursor = None;
            self.scan_token = None;
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
            self.pending_edit_files.clear();
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
        if let Some(overlay) = overlay {
            self.workbench.overlay = Some(overlay);
        }
        self.dirty = true;
    }
}
