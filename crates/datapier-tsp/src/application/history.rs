//! 查询历史的状态、动作与分页。
use super::*;

/// 当前历史页及其冻结的查询目标；草稿条件与已提交条件分离。
#[derive(Clone, Default)]
pub(crate) struct ArcheryHistory {
    pub items: Vec<Value>,
    pub selected: Option<usize>,
    pub pending: Option<Uuid>,
    pub text_pending: Option<(Uuid, String)>,
    pub full_text: Option<String>,
    pub text_error: Option<String>,
    pub session: Option<Value>,
    pub error: Option<String>,
    pub auth_required: bool,
    pub offset: u64,
    pub limit: u64,
    pub total: u64,
    pub visible: bool,
    pub draft: Editor,
    pub reload_page: bool,
    pub applied_search: String,
    pub tab: Option<Uuid>,
    pub profile: Option<Uuid>,
    pub database: Option<String>,
}

impl App {
    pub(super) fn history_action(&mut self, name: &str, backend: &Backend) -> bool {
        let history_size = name
            .strip_prefix("history-size:")
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|size| (1..=1000).contains(size));
        if matches!(name, "history-search" | "history-prev" | "history-next")
            || history_size.is_some()
        {
            let draft = self.workbench.archery_history.draft.text.clone();
            let history = &mut self.workbench.archery_history;
            match name {
                "history-search" => {
                    history.applied_search = draft;
                    history.offset = 0;
                }
                "history-prev" => history.offset = history.offset.saturating_sub(history.limit),
                "history-next" => {
                    if history.offset.saturating_add(history.limit) >= history.total {
                        return true;
                    }
                    history.offset += history.limit;
                }
                _ => {
                    history.limit = history_size.unwrap();
                    history.offset = 0;
                }
            }
            self.request_history(backend);
            return true;
        }
        if name == "archery-history-reauth" {
            if let Some(server_id) = self
                .profile
                .as_ref()
                .and_then(|p| p.archery())
                .map(|s| s.server_id)
            {
                self.archery_reauthenticate(server_id, backend);
            }
            return true;
        }
        if let Some(index) = name.strip_prefix("history-select:") {
            let history = &mut self.workbench.archery_history;
            history.selected = index
                .parse::<usize>()
                .ok()
                .filter(|i| *i < history.items.len());
            self.load_history_text(backend);
            return true;
        }
        if name == "history-open" {
            let text = self.history_selected_text().map(str::to_owned);
            let Some(text) = text else {
                return true;
            };
            if self.tabs.get(self.active).is_none() {
                self.status = UiMessage::key("请先连接数据库");
                return true;
            }
            if self.workbench.inline.is_some()
                || self.has_live_drafts()
                || !self.tabs[self.active].pending.is_null()
            {
                self.status = UiMessage::key("请先提交或丢弃当前草稿");
                return true;
            }
            if self.task.is_some() || self.operation_pending || self.workbench.cli_running {
                self.status = UiMessage::key("请等待或取消当前任务");
                return true;
            }
            if self.is_redis() {
                self.field_set("cli_text", text);
                self.workbench.overlay = None;
                self.workbench.cli = true;
                self.view = "data".into();
                self.focus = "cli_text".into();
                self.workbench.focus_request = Some("input-cli_text".into());
            } else {
                if self.browse_result && self.target.is_some() {
                    self.view = "data".into();
                    self.open_query_tab(backend);
                } else {
                    self.generation = self.generation.wrapping_add(1);
                }
                self.tabs[self.active].query = Editor::new(text);
                self.results.clear();
                self.result_id = None;
                self.grid = None;
                self.page_history.clear();
                self.page_token = None;
                self.large = None;
                self.browse_result = false;
                self.workbench.query_ui.executed = false;
                self.workbench.overlay = None;
                self.view = "query".into();
                self.focus = "query".into();
                self.workbench.focus_request = Some("query-editor".into());
            }
            self.dirty = true;
            return true;
        }
        false
    }

    pub(crate) fn open_history(&mut self, backend: &Backend) {
        let visible = self.profile.as_ref().is_some_and(|p| p.is_archery());
        let history = &self.workbench.archery_history;
        let same_target = history.limit != 0
            && history.visible == visible
            && history.tab == self.tabs.get(self.active).map(|tab| tab.id)
            && history.profile == self.profile.as_ref().map(|profile| profile.id)
            && history.database == self.database
            && (!visible || history.session == self.session_id());
        if !same_target {
            self.invalidate_history(backend);
            self.workbench.archery_history = ArcheryHistory {
                limit: self.settings.paging.history_rows as u64,
                visible,
                ..Default::default()
            };
        }
        self.workbench.overlay = Some("history".into());
        self.workbench.focus_request = Some("input-history_filter".into());
        if !same_target || self.workbench.archery_history.reload_page {
            self.request_history(backend);
        } else if self.workbench.archery_history.selected.is_some()
            && self.workbench.archery_history.full_text.is_none()
        {
            self.load_history_text(backend);
        }
    }

    pub(crate) fn invalidate_history(&mut self, backend: &Backend) {
        let history = &mut self.workbench.archery_history;
        history.reload_page |= history.pending.is_some();
        history.text_pending = None;
        history.full_text = None;
        history.text_error = None;
        if let Some(operation) = history.pending.take()
            && history.visible
        {
            let _ = backend.request(
                "archery.history.cancel",
                "archery.operation.cancel",
                json!({"operation_id":operation}),
            );
        }
    }

    fn request_history(&mut self, backend: &Backend) {
        self.invalidate_history(backend);
        let operation = Uuid::new_v4();
        let session = self.session_id();
        let history = &mut self.workbench.archery_history;
        history.reload_page = false;
        history.items.clear();
        history.selected = None;
        history.total = 0;
        history.error = None;
        history.auth_required = false;
        history.session = session.clone();
        history.tab = self.tabs.get(self.active).map(|tab| tab.id);
        history.profile = self.profile.as_ref().map(|profile| profile.id);
        history.database = self.database.clone();
        if history.visible && session.is_none() {
            history.error = Some(i18n::tr("请先连接数据库").into_owned());
            return;
        }
        history.pending = Some(operation);
        let (method, params) = if history.visible {
            (
                "archery.query_history",
                json!({"operation_id":operation,"session_id":session,"offset":history.offset,"limit":history.limit,"search":history.applied_search}),
            )
        } else {
            let needle = history.applied_search.to_lowercase();
            let search_states: Vec<_> = [
                "queued",
                "running",
                "cancelling",
                "succeeded",
                "failed",
                "cancelled",
                "wait_stopped",
                "outcome_unknown",
            ]
            .into_iter()
            .filter(|state| {
                !needle.is_empty()
                    && crate::views::task_state_text(state)
                        .to_lowercase()
                        .contains(&needle)
            })
            .collect();
            (
                "history.list",
                json!({"workspace_id":self.workspace_id,"offset":history.offset,"limit":history.limit,"search":history.applied_search,"search_states":search_states}),
            )
        };
        if let Err(error) = backend.request(format!("history.page/{operation}"), method, params) {
            history.pending = None;
            history.error = Some(error.to_string());
        }
    }

    pub(crate) fn history_response(
        &mut self,
        token: &str,
        result: &Result<Value>,
        backend: &Backend,
    ) {
        let Ok(operation) = Uuid::parse_str(token) else {
            return;
        };
        let history = &self.workbench.archery_history;
        if Some(operation) != history.pending
            || history.tab != self.tabs.get(self.active).map(|tab| tab.id)
            || history.profile != self.profile.as_ref().map(|profile| profile.id)
            || history.database != self.database
            || (history.visible && history.session != self.session_id())
        {
            return;
        }
        let history = &mut self.workbench.archery_history;
        history.pending = None;
        match result {
            Ok(value) => {
                let key = if history.visible { "rows" } else { "items" };
                match (
                    value.get(key).and_then(Value::as_array),
                    value.get("total").and_then(Value::as_u64),
                    value.get("offset").and_then(Value::as_u64),
                    value.get("limit").and_then(Value::as_u64),
                ) {
                    (Some(items), Some(total), Some(offset), Some(limit))
                        if offset == history.offset && limit == history.limit =>
                    {
                        if total > 0 && offset >= total {
                            history.offset = (total - 1) / limit.max(1) * limit.max(1);
                            self.request_history(backend);
                            return;
                        }
                        history.items = items.clone();
                        history.total = total;
                        history.selected = (!items.is_empty()).then_some(0);
                    }
                    _ => history.error = Some(i18n::tr("历史响应格式无效").into_owned()),
                }
            }
            Err(error) => {
                history.auth_required = error.code == "auth_required";
                history.error = Some(error.to_string());
            }
        }
        self.load_history_text(backend);
    }

    pub(crate) fn history_selected_text(&self) -> Option<&str> {
        let history = &self.workbench.archery_history;
        if history.pending.is_some()
            || history.text_pending.is_some()
            || history.text_error.is_some()
        {
            return None;
        }
        let item = history
            .selected
            .and_then(|index| history.items.get(index))?;
        if item.get("text_truncated").and_then(Value::as_bool) == Some(true) {
            history.full_text.as_deref()
        } else {
            archery_history_text(item)
        }
    }

    fn load_history_text(&mut self, backend: &Backend) {
        let history = &mut self.workbench.archery_history;
        history.text_pending = None;
        history.full_text = None;
        history.text_error = None;
        let Some(item) = history.selected.and_then(|index| history.items.get(index)) else {
            return;
        };
        if history.visible || item.get("text_truncated").and_then(Value::as_bool) != Some(true) {
            return;
        }
        let Some(id) = item.get("id").and_then(Value::as_str) else {
            history.text_error = Some(i18n::tr("历史响应格式无效").into_owned());
            return;
        };
        let operation = Uuid::new_v4();
        history.text_pending = Some((operation, id.to_owned()));
        if let Err(error) = backend.request(
            format!("history.text/{operation}"),
            "execution.get",
            json!({"task_id":id,"include_text":true}),
        ) {
            history.text_pending = None;
            history.text_error = Some(error.to_string());
        }
    }

    pub(crate) fn history_text_response(&mut self, token: &str, result: &Result<Value>) {
        let history = &self.workbench.archery_history;
        let Some((operation, id)) = &history.text_pending else {
            return;
        };
        if Uuid::parse_str(token).ok() != Some(*operation)
            || history.tab != self.tabs.get(self.active).map(|tab| tab.id)
            || history.profile != self.profile.as_ref().map(|profile| profile.id)
            || history.database != self.database
            || history
                .selected
                .and_then(|index| history.items.get(index))
                .and_then(|item| item.get("id"))
                .and_then(Value::as_str)
                != Some(id.as_str())
        {
            return;
        }
        let id = id.clone();
        let history = &mut self.workbench.archery_history;
        history.text_pending = None;
        match result {
            Ok(value) if value.get("id").and_then(Value::as_str) == Some(id.as_str()) => {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    history.full_text = Some(text.to_owned());
                } else {
                    history.text_error = Some(i18n::tr("历史响应格式无效").into_owned());
                }
            }
            Ok(_) => history.text_error = Some(i18n::tr("历史响应格式无效").into_owned()),
            Err(error) => history.text_error = Some(error.to_string()),
        }
    }
}

pub(crate) fn archery_history_text(item: &Value) -> Option<&str> {
    ["sqllog", "sql_content", "sql", "text", "full_sql"]
        .iter()
        .find_map(|key| item.get(*key).and_then(Value::as_str))
}
