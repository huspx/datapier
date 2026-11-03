//! 应用级设置和查询／标签操作；不重建数据库会话或工作区编辑状态。
use super::*;
use crate::i18n::UiMessage;

pub(super) fn mysql_mode(value: &Value) -> Option<editor::MysqlMode> {
    let mode = value.get("sql_mode")?.as_str()?;
    Some(editor::MysqlMode {
        no_backslash_escapes: mode
            .split(',')
            .any(|s| s.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES")),
        ansi_quotes: mode
            .split(',')
            .any(|s| s.eq_ignore_ascii_case("ANSI_QUOTES")),
    })
}

/// 当前历史页及其冻结的查询目标；草稿条件与已提交条件分离。
#[derive(Clone, Default)]
pub(super) struct ArcheryHistory {
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
    pub applied_search: String,
    pub tab: Option<Uuid>,
    pub profile: Option<Uuid>,
    pub database: Option<String>,
}

impl App {
    pub(super) fn refresh_settings(&mut self, backend: &Backend) {
        if self.settings_save_token.is_some() || self.settings_unsaved {
            return;
        }
        if let Err(error) = backend.request(
            format!("settings.get/{}", self.settings_epoch),
            "settings.get",
            json!({}),
        ) {
            self.status = i18n::UiMessage::raw(error.to_string());
        }
    }

    fn save_settings(&mut self, backend: &Backend) {
        let token = Uuid::new_v4();
        self.settings_save_token = Some(token);
        self.settings_unsaved = false;
        if let Err(error) = backend.request(
            format!("settings.set/{token}"),
            "settings.set",
            json!({"settings":self.settings}),
        ) {
            self.settings_save_token = None;
            self.settings_unsaved = true;
            self.status = i18n::UiMessage::template(
                "语言选择仅应用于本次会话，未保存: {error}",
                vec![("error", i18n::UiArg::Raw(error.to_string()))],
            );
        }
    }

    pub(super) fn settings_response(&mut self, response: Response, backend: &Backend) {
        let saving = if let Some(token) = response.tag.strip_prefix("settings.set/") {
            if Uuid::parse_str(token)
                .ok()
                .is_none_or(|token| Some(token) != self.settings_save_token)
            {
                return;
            }
            self.settings_save_token = None;
            true
        } else if let Some(epoch) = response.tag.strip_prefix("settings.get/") {
            if epoch.parse::<u64>().ok() != Some(self.settings_epoch)
                || self.settings_save_token.is_some()
                || self.settings_unsaved
            {
                return;
            }
            false
        } else {
            return;
        };
        let response_pending = saving && self.settings_unsaved;
        match response
            .result
            .and_then(|v| serde_json::from_value::<AppSettings>(v).map_err(Into::into))
        {
            Ok(settings) => {
                if !saving || !self.settings_unsaved {
                    self.settings = settings;
                    self.settings_unsaved = false;
                    i18n::set_preference(self.settings.language);
                }
            }
            Err(error) => {
                self.settings_unsaved |= saving;
                self.status = if saving {
                    i18n::UiMessage::template(
                        "语言选择仅应用于本次会话，未保存: {error}",
                        vec![("error", i18n::UiArg::Raw(error.to_string()))],
                    )
                } else {
                    i18n::UiMessage::raw(error.to_string())
                };
            }
        }
        if response_pending {
            // 只有处理完上一笔写入后才能发送期间选中的最新设置。
            self.save_settings(backend);
        }
    }

    pub(super) fn application_action(&mut self, name: &str, backend: &Backend) -> bool {
        if name == "archery-sql-check" {
            if !self.profile.as_ref().is_some_and(|profile| {
                profile.is_archery()
                    && profile.provider == datapier_core::model::ProviderKind::Mysql
            }) {
                return true;
            }
            let Some(session) = self.session_id() else {
                self.status = i18n::UiMessage::key("请先连接数据库");
                return true;
            };
            let Some(database) = self.database.clone() else {
                self.status = i18n::UiMessage::key("请先选择数据库");
                return true;
            };
            if !self
                .profile
                .as_ref()
                .and_then(|profile| profile.archery())
                .is_some_and(|source| source.databases.contains(&database))
            {
                self.status = i18n::UiMessage::key("请先选择数据库");
                return true;
            }
            if self.operation_pending || self.task.is_some() || self.has_live_drafts() {
                self.status = i18n::UiMessage::key("该 tab 已有正在执行的任务");
                return true;
            }
            let query = &self.tabs[self.active].query;
            let text = query
                .selected_text()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(&query.text)
                .to_owned();
            if text.trim().is_empty() {
                self.status = i18n::UiMessage::key("没有可执行的文本");
                return true;
            }
            self.generation = self.generation.wrapping_add(1);
            self.workbench.query_ui.executed = true;
            self.workbench.query_ui.results_hidden = false;
            self.browse_result = false;
            self.grid = None;
            self.result_id = None;
            self.results.clear();
            self.request(backend, "archery.sql_check", "archery.sql_check",
                json!({"session_id":session,"workspace_id":self.workspace_id,"database":database,"text":text}));
            return true;
        }
        if matches!(
            name,
            "history-search"
                | "history-prev"
                | "history-next"
                | "history-size:20"
                | "history-size:50"
                | "history-size:100"
        ) {
            let draft = self.field("history_filter").to_owned();
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
                    history.limit = name[13..].parse().unwrap_or(20);
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
        if let Some(id) = name
            .strip_prefix("profile-settings:")
            .and_then(|id| Uuid::parse_str(id).ok())
        {
            self.edit_profile_after_close(id, false, backend);
            return true;
        }
        if let Some(id) = name
            .strip_prefix("close-tab:")
            .and_then(|id| Uuid::parse_str(id).ok())
        {
            self.close_tab(id, backend);
            return true;
        }
        for (prefix, direction) in [
            ("tab-close-left:", -1),
            ("tab-close-right:", 1),
            ("tab-close-all:", 0),
        ] {
            if let Some(id) = name
                .strip_prefix(prefix)
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                    let ids = self
                        .tabs
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| {
                            direction == 0
                                || (direction < 0 && *i < index)
                                || (direction > 0 && *i > index)
                        })
                        .map(|(_, tab)| tab.id)
                        .collect();
                    self.close_tabs(ids, None, backend);
                }
                return true;
            }
        }
        if name == "server-new" {
            self.archery_manage_server(None, backend);
            return true;
        }
        if let Some((action, id)) = name.split_once(':')
            && matches!(action, "server-edit" | "server-delete")
            && let Ok(id) = Uuid::parse_str(id)
        {
            self.archery_manage_server(Some(id), backend);
            self.archery.delete_confirm = action == "server-delete";
            return true;
        }
        if name == "app-settings" {
            if self.finish_inline(true) {
                self.workbench.overlay = Some("settings".into());
                self.refresh_archery_servers(backend);
            }
            return true;
        }
        if name == "query-result-close" {
            // 隐藏结果不等于取消任务；任务和独立结果引用继续由所属 tab 管理。
            self.workbench.query_ui.results_hidden = true;
            return true;
        }
        if let Some(language) = name.strip_prefix("language:") {
            let preference = match language {
                "system" => LanguagePreference::System,
                "zh-CN" => LanguagePreference::ZhCn,
                "en" => LanguagePreference::En,
                _ => return true,
            };
            self.workbench.generic_select = None;
            self.settings.language = preference;
            self.settings_epoch = self.settings_epoch.wrapping_add(1);
            i18n::set_preference(preference);
            self.settings_unsaved = true;
            if self.settings_save_token.is_none() {
                self.save_settings(backend);
            }
            return true;
        }
        false
    }

    pub(super) fn close_tab(&mut self, id: Uuid, backend: &Backend) {
        self.close_tabs(vec![id], None, backend);
    }

    pub(super) fn refresh_query_mode(&mut self, backend: &Backend) {
        if !self.is_redis()
            && !self
                .profile
                .as_ref()
                .is_some_and(|profile| profile.is_archery())
            && let Some(session) = self.session_id()
        {
            self.request(
                backend,
                "query.info",
                "connection.info",
                json!({"session_id":session}),
            );
        }
    }

    pub(super) fn execute_query_text(&mut self, text: String, backend: &Backend) {
        if text.trim().is_empty() {
            self.status = i18n::UiMessage::key("没有可执行的文本");
            return;
        }
        if !self.finish_inline(true) {
            return;
        }
        if self.operation_pending || self.task.is_some() {
            self.status = i18n::UiMessage::key("该 tab 已有正在执行的任务");
            return;
        }
        if self.has_live_drafts() {
            self.status = i18n::UiMessage::key("请先提交或丢弃待提交变更与粘贴预览");
            return;
        }
        let Some(session) = self.session_id() else {
            self.status = i18n::UiMessage::key("请先连接数据库");
            return;
        };
        if let Some(source) = self.profile.as_ref().and_then(|profile| profile.archery())
            && !self
                .database
                .as_ref()
                .is_some_and(|database| source.databases.contains(database))
        {
            self.status = i18n::UiMessage::key("请先选择数据库");
            return;
        }
        self.close_result_info(false);
        // SQL 可修改会话 sql_mode；旧模式与旧结果响应均不能用于新执行。
        self.generation = self.generation.wrapping_add(1);
        self.workbench.sql_mode = None;
        self.workbench.query_ui.executed = true;
        self.workbench.query_ui.results_hidden = false;
        self.workbench.query_ui.menu_open = false;
        self.browse_result = false;
        self.grid = None;
        self.schema = None;
        self.edit_token = None;
        self.target = None;
        self.results.clear();
        self.result_id = None;
        self.page_token = None;
        self.page_history.clear();
        self.large = None;
        self.view = "query".into();
        let mut params = json!({"session_id":session,"workspace_id":self.workspace_id,"database":self.database,"text":text});
        if self
            .profile
            .as_ref()
            .is_some_and(|profile| profile.is_archery())
        {
            params["limit"] = json!(self.workbench.query_ui.request_limit);
        }
        self.request(backend, "execution.start", "execution.start", params);
    }
    pub(super) fn open_history(&mut self, backend: &Backend) {
        self.invalidate_history(backend);
        self.field_set("history_filter", "");
        let visible = self.profile.as_ref().is_some_and(|p| p.is_archery());
        self.workbench.archery_history = ArcheryHistory {
            limit: 20,
            visible,
            ..Default::default()
        };
        self.workbench.overlay = Some("history".into());
        self.workbench.focus_request = Some("input-history_filter".into());
        self.request_history(backend);
    }

    pub(super) fn invalidate_history(&mut self, backend: &Backend) {
        let history = &mut self.workbench.archery_history;
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

    pub(super) fn history_response(
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
            || self.workbench.overlay.as_deref() != Some("history")
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

    pub(super) fn history_selected_text(&self) -> Option<&str> {
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

    pub(super) fn history_text_response(&mut self, token: &str, result: &Result<Value>) {
        let history = &self.workbench.archery_history;
        let Some((operation, id)) = &history.text_pending else {
            return;
        };
        if Uuid::parse_str(token).ok() != Some(*operation)
            || history.tab != self.tabs.get(self.active).map(|tab| tab.id)
            || history.profile != self.profile.as_ref().map(|profile| profile.id)
            || history.database != self.database
            || self.workbench.overlay.as_deref() != Some("history")
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

pub(super) fn archery_history_text(item: &Value) -> Option<&str> {
    ["sqllog", "sql_content", "sql", "text", "full_sql"]
        .iter()
        .find_map(|key| item.get(*key).and_then(Value::as_str))
}

#[cfg(test)]
#[path = "../tests/unit/application.rs"]
mod tests;
