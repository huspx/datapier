//! 应用级设置和查询／标签操作；不重建数据库会话或工作区编辑状态。
use super::*;
use crate::i18n::UiMessage;

pub(super) const SETTINGS_FIELDS: &[(&str, &str, &str)] = &[
    ("paging", "mysql_table_rows", "MySQL 表页行数"),
    ("paging", "sql_rows", "SQL 页行数"),
    ("paging", "redis_value_rows", "Redis 值页行数"),
    ("paging", "history_rows", "历史页行数"),
    ("paging", "archery_grid_rows", "平台结果页行数"),
    ("paging", "redis_key_batch", "Redis key 批量"),
    ("archery", "request_rows", "平台请求行数"),
    ("limits", "mcp_response_rows", "MCP 最大行数"),
    ("limits", "response_bytes", "响应字节上限"),
    ("limits", "value_preview_bytes", "值预览字节数"),
    ("limits", "connect_timeout_seconds", "连接超时（秒）"),
    ("limits", "query_timeout_seconds", "查询超时（秒）"),
    ("limits", "count_timeout_seconds", "计数超时（秒）"),
    ("limits", "result_idle_seconds", "结果空闲回收（秒）"),
    (
        "limits",
        "result_memory_bytes_per_client",
        "客户端结果内存字节数",
    ),
    ("limits", "running_tasks_per_mcp_client", "MCP 并发任务数"),
    ("approval", "ttl_seconds", "审批有效期（秒）"),
    ("history", "retention_days", "历史保留天数（可留空）"),
    ("history", "max_entries", "历史最大条数（可留空）"),
];
pub(super) const SETTINGS_SWITCHES: &[(&str, &str, &str)] = &[
    ("mcp", "fine_grained_access", "MCP 细粒度权限"),
    ("notifications", "approval", "审批通知"),
    ("notifications", "authentication", "认证通知"),
    ("history", "auto_cleanup_enabled", "自动清理历史"),
];

pub(super) fn settings_is_mcp(group: &str, key: &str) -> bool {
    matches!(group, "mcp" | "notifications" | "approval")
        || (group == "limits"
            && matches!(key, "mcp_response_rows" | "running_tasks_per_mcp_client"))
}
fn settings_field(group: &str, key: &str) -> String {
    format!("settings-{group}-{key}")
}
fn settings_text(value: &Value) -> String {
    if value.is_null() {
        String::new()
    } else if let Some(text) = value.as_str() {
        text.to_owned()
    } else {
        value.to_string()
    }
}

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
    fn settings_form_snapshot(&self) -> String {
        let mut values = serde_json::Map::new();
        for &(group, key, _) in SETTINGS_FIELDS.iter().chain(SETTINGS_SWITCHES) {
            let name = settings_field(group, key);
            values.insert(name.clone(), json!(self.field(&name)));
        }
        for name in [
            "settings-approval-default_policy",
            "settings-language-draft",
            "editing_plan_items",
            "editing_plan_bytes",
            "editing_value_bytes",
            "editing_text_bytes",
            "editing_preview_rows",
            "external_mode",
            "external_program",
            "external_args",
        ] {
            values.insert(name.to_owned(), json!(self.field(name)));
        }
        Value::Object(values).to_string()
    }

    pub(super) fn settings_form_dirty(&self) -> bool {
        self.settings_loaded
            && self.settings_form_snapshot() != self.field("settings-form-snapshot")
    }

    pub(super) fn settings_discard_form(&mut self) {
        if let Ok(Value::Object(snapshot)) =
            serde_json::from_str::<Value>(self.field("settings-form-snapshot"))
        {
            for (name, value) in snapshot {
                if let Some(value) = value.as_str() {
                    self.field_set(&name, value);
                }
            }
        }
        self.field_set("settings-reload-confirm", "false");
    }

    fn settings_load_fields(&mut self) {
        let value = serde_json::to_value(&self.settings).unwrap_or_default();
        for &(group, key, _) in SETTINGS_FIELDS.iter().chain(SETTINGS_SWITCHES) {
            self.field_set(
                &settings_field(group, key),
                settings_text(&value[group][key]),
            );
        }
        self.field_set(
            "settings-approval-default_policy",
            settings_text(&value["approval"]["default_policy"]),
        );
        let limits = self.settings.editing.clone();
        for (name, value) in [
            ("editing_plan_items", limits.max_plan_items),
            ("editing_plan_bytes", limits.max_plan_bytes),
            ("editing_value_bytes", limits.max_value_bytes),
            ("editing_text_bytes", limits.max_text_editor_bytes),
            ("editing_preview_rows", limits.preview_rows),
        ] {
            self.field_set(name, value.to_string());
        }
        let editor = self.settings.external_editor.clone();
        self.field_set(
            "external_mode",
            editor.as_ref().map_or("disabled", |e| match e.mode {
                datapier_core::model::editing::ExternalEditorMode::Gui => "gui",
                datapier_core::model::editing::ExternalEditorMode::Terminal => "terminal",
            }),
        );
        self.field_set(
            "external_program",
            editor.as_ref().map_or("", |e| e.program.as_str()),
        );
        self.field_set(
            "external_args",
            editor.map_or_else(String::new, |e| e.args.join("\n")),
        );
        self.field_set("settings-language-draft", settings_text(&value["language"]));
        self.field_set("settings-form-snapshot", self.settings_form_snapshot());
    }
    fn mcp_rules_request(
        &mut self,
        backend: &Backend,
        operation: &str,
        method: &str,
        params: Value,
        retry: &str,
    ) {
        let tag = format!(
            "{operation}/{}/{}",
            self.field("mcp-rule-profile"),
            Uuid::new_v4()
        );
        self.field_set("mcp-rule-pending", &tag);
        self.field_set("mcp-rule-error", "");
        self.field_set("mcp-rule-retry", retry);
        if let Err(error) = backend.request(&tag, method, params) {
            self.field_set("mcp-rule-pending", "");
            self.field_set("mcp-rule-error", error.to_string());
        }
    }

    pub(super) fn mcp_rules_response(
        &mut self,
        tag: &str,
        result: &Result<Value>,
        backend: &Backend,
    ) {
        let Some((operation, rest)) = tag.split_once('/') else {
            return;
        };
        let Some((id, _)) = rest.split_once('/') else {
            return;
        };
        if self.field("mcp-rule-profile") != id || self.field("mcp-rule-pending") != tag {
            return;
        }
        self.field_set("mcp-rule-pending", "");
        match result {
            Ok(value) if operation == "mcp-rules-get" => {
                self.field_set("mcp-rule-data", value.to_string());
                self.field_set("mcp-rule-baseline", value.to_string());
            }
            Ok(value) if operation == "mcp-rules-set" || operation == "mcp-policy-set" => {
                if let Some(policy) = value["profile"]["approval_policy"].as_str() {
                    self.field_set("mcp-rule-policy", policy);
                }
                self.mcp_rules_request(
                    backend,
                    "mcp-rules-get",
                    "profile.rules.get",
                    json!({"profile_id":id}),
                    &format!("mcp-rules-load:{id}"),
                );
                self.request(backend, "profile.list", "profile.list", json!({}));
            }
            Err(error) => self.field_set("mcp-rule-error", error.to_string()),
            _ => {}
        }
    }

    pub(super) fn refresh_settings(&mut self, backend: &Backend) {
        if self.settings_save_token.is_some() || self.settings_unsaved {
            return;
        }
        if let Err(error) = backend.request(
            format!("settings.get/{}", self.settings_epoch),
            "settings.get",
            json!({}),
        ) {
            if self.field("settings-reload") == "true" {
                self.settings_unsaved = self.field("settings-reload-unsaved") == "true";
            }
            self.field_set("settings-reload", "false");
            self.field_set("settings-error", error.to_string());
            self.status = i18n::UiMessage::raw(error.to_string());
        }
    }

    fn save_settings(&mut self, backend: &Backend) {
        self.field_set("settings-form-saving", self.settings_form_snapshot());
        self.field_set("settings-error", "");
        let token = Uuid::new_v4();
        self.settings_save_token = Some(token);
        self.settings_unsaved = false;
        if let Err(error) = backend.request(
            format!("settings.set/{token}"),
            "settings.set",
            json!({"settings":self.settings}),
        ) {
            self.settings_save_token = None;
            self.field_set("settings-error", error.to_string());
            self.settings_unsaved = true;
            self.status = i18n::UiMessage::template(
                "设置仅应用于本次会话，未保存: {error}",
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
            // 后台刷新不得给未保存草稿换用更新后的 revision；否则整份旧表单会覆盖另一窗口的修改。
            if self.field("settings-reload") != "true" && self.settings_form_dirty() {
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
                self.settings_loaded = true;
                if saving {
                    self.field_set(
                        "settings-form-snapshot",
                        self.field("settings-form-saving").to_owned(),
                    );
                }
                self.field_set("settings-error", "");
                if saving && self.settings_unsaved {
                    self.settings.revision = settings.revision;
                }
                if !saving || !self.settings_unsaved {
                    self.settings = settings;
                    self.settings_unsaved = false;
                    if !saving {
                        self.settings_load_fields();
                        self.field_set("settings-reload", "false");
                    }
                    i18n::set_preference(self.settings.language);
                }
            }
            Err(error) => {
                if !saving && self.field("settings-reload") == "true" {
                    self.settings_unsaved = self.field("settings-reload-unsaved") == "true";
                }
                self.field_set("settings-reload", "false");
                self.field_set("settings-error", error.to_string());
                self.settings_unsaved |= saving;
                self.status = i18n::UiMessage::template(
                    "设置未保存: {error}；请重新加载设置后再保存",
                    vec![("error", i18n::UiArg::Raw(error.to_string()))],
                );
                return;
            }
        }
        if response_pending {
            // 只有处理完上一笔写入后才能发送期间选中的最新设置。
            self.save_settings(backend);
        }
    }

    pub(super) fn application_action(&mut self, name: &str, backend: &Backend) -> bool {
        if name == "redis-row-delete-load-identities" {
            self.load_redis_row_delete_targets(backend);
            return true;
        }
        if self.editing_files_action(name, backend) {
            return true;
        }
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
                self.field_set("editing_file_path", "");
                self.editing_files_list(backend, 0);
                self.workbench.overlay = Some("settings".into());
                self.refresh_archery_servers(backend);
                self.field_set("settings-tab", "general");
                self.focus = "settings-tab-general".into();
                self.workbench.focus_request = Some(String::new());
            }
            return true;
        }
        if let Some(tab) = name.strip_prefix("settings-tab:") {
            if matches!(tab, "general" | "editing" | "archery" | "mcp") {
                self.field_set("settings-tab", tab);
                self.workbench.generic_select = None;
                self.focus = format!("settings-tab-{tab}");
                self.workbench.focus_request = Some(String::new());
            }
            return true;
        }
        if name == "query-result-close" {
            // 隐藏结果不等于取消任务；任务和独立结果引用继续由所属 tab 管理。
            self.workbench.query_ui.results_hidden = true;
            return true;
        }
        if let Some(language) = name.strip_prefix("settings-language:") {
            if matches!(language, "system" | "zh-CN" | "en") {
                self.field_set("settings-language-draft", language);
                self.workbench.generic_select = None;
            }
            return true;
        }
        if let Some(id) = name.strip_prefix("mcp-rules-load:") {
            self.workbench.generic_select = None;
            if let Some(policy) = self
                .profiles
                .iter()
                .find(|profile| profile.id.to_string() == id)
                .and_then(|profile| serde_json::to_value(profile.approval_policy).ok())
                .and_then(|value| value.as_str().map(str::to_owned))
            {
                if !self.field("mcp-rule-pending").is_empty() {
                    return true;
                }
                let previous = self.field("mcp-rule-profile").to_owned();
                if !previous.is_empty() && previous != id {
                    for name in ["data", "scope", "baseline"] {
                        let value = self.field(&format!("mcp-rule-{name}")).to_owned();
                        self.field_set(&format!("mcp-rule-cache-{previous}-{name}"), value);
                    }
                }
                if previous != id {
                    self.field_set("mcp-rule-profile", id);
                    self.field_set("mcp-rule-policy", policy);
                    let cached = self.field(&format!("mcp-rule-cache-{id}-data")).to_owned();
                    if !cached.is_empty() {
                        for name in ["data", "scope", "baseline"] {
                            let value = self
                                .field(&format!("mcp-rule-cache-{id}-{name}"))
                                .to_owned();
                            self.field_set(&format!("mcp-rule-{name}"), value);
                        }
                        return true;
                    }
                    self.field_set("mcp-rule-data", "");
                    self.field_set("mcp-rule-scope", "");
                } else if !self.field("mcp-rule-data").is_empty() {
                    return true;
                }
                self.mcp_rules_request(
                    backend,
                    "mcp-rules-get",
                    "profile.rules.get",
                    json!({"profile_id":id}),
                    name,
                );
            }
            return true;
        }
        if let Some(level) = name.strip_prefix("mcp-rules-level:") {
            self.workbench.generic_select = None;
            if !self.field("mcp-rule-pending").is_empty() {
                return true;
            }
            if !matches!(level, "disabled" | "read_only" | "read_write") {
                return true;
            }
            let id = self.field("mcp-rule-profile").to_owned();
            let Some(profile) = self
                .profiles
                .iter()
                .find(|profile| profile.id.to_string() == id)
            else {
                return true;
            };
            let redis = profile.provider == datapier_core::model::ProviderKind::Redis;
            let scope = if redis {
                "*".to_owned()
            } else {
                self.field("mcp-rule-scope").trim().to_owned()
            };
            if scope.is_empty() {
                self.field_set("mcp-rule-error", i18n::tr("请填写数据库名称").into_owned());
                self.field_set("mcp-rule-retry", "");
                return true;
            }
            let Ok(mut data) = serde_json::from_str::<Value>(self.field("mcp-rule-data")) else {
                return true;
            };
            let Some(rules) = data["rules"].as_array_mut() else {
                return true;
            };
            let kind = if redis { "instance" } else { "database" };
            rules.retain(|rule| !(rule["scope_kind"] == kind && rule["scope_key"] == scope));
            rules.push(json!({"profile_id":id,"scope_kind":kind,"scope_key":scope,"level":level}));
            self.field_set("mcp-rule-data", data.to_string());
            self.field_set("mcp-rule-error", "");
            self.field_set("mcp-rule-retry", "");
            return true;
        }
        if let Some(policy) = name.strip_prefix("mcp-policy-set:") {
            self.workbench.generic_select = None;
            if self.field("mcp-rule-data") != self.field("mcp-rule-baseline") {
                return true;
            }
            if self.field("mcp-rule-pending").is_empty()
                && matches!(policy, "off" | "dangerous" | "all_writes")
            {
                let id = self.field("mcp-rule-profile").to_owned();
                if let Ok(data) = serde_json::from_str::<Value>(self.field("mcp-rule-data")) {
                    self.mcp_rules_request(backend, "mcp-policy-set", "profile.approval.set", json!({"profile_id":id,"expected_revision":data["revision"],"approval_policy":policy}), name);
                }
            }
            return true;
        }
        if name == "mcp-rules-save" {
            if self.field("mcp-rule-pending").is_empty() {
                let id = self.field("mcp-rule-profile").to_owned();
                if let Ok(data) = serde_json::from_str::<Value>(self.field("mcp-rule-data")) {
                    self.mcp_rules_request(backend, "mcp-rules-set", "profile.rules.set", json!({"profile_id":id,"expected_revision":data["revision"],"rules":data["rules"]}), name);
                }
            }
            return true;
        }
        if name == "settings-reload-cancel" {
            self.field_set("settings-reload-confirm", "false");
            return true;
        }
        if name == "settings-reload" && self.settings_form_dirty() {
            self.field_set("settings-reload-confirm", "true");
            return true;
        }
        if name == "settings-reload" || name == "settings-reload-confirm" {
            self.field_set("settings-reload-confirm", "false");
            if self.settings_save_token.is_none() && self.field("settings-reload") != "true" {
                self.field_set("settings-reload-unsaved", self.settings_unsaved.to_string());
                self.settings_unsaved = false;
                self.settings_epoch = self.settings_epoch.wrapping_add(1);
                self.field_set("settings-reload", "true");
                self.refresh_settings(backend);
            }
            return true;
        }
        if let Some(rest) = name.strip_prefix("settings-toggle:") {
            if let Some((group, key)) = rest.split_once(':') {
                let field = settings_field(group, key);
                let next = self.field(&field) != "true";
                self.field_set(&field, next.to_string());
            }
            return true;
        }
        if let Some(policy) = name.strip_prefix("settings-policy:") {
            if matches!(policy, "dangerous" | "all_writes") {
                self.field_set("settings-approval-default_policy", policy);
            }
            return true;
        }
        if name == "settings-save" {
            if !self.settings_loaded
                || self.field("settings-reload") == "true"
                || self.settings_save_token.is_some()
            {
                return true;
            }
            self.field_set("settings-error", "");
            for field in [
                "editing_plan_items",
                "editing_plan_bytes",
                "editing_value_bytes",
                "editing_text_bytes",
                "editing_preview_rows",
            ] {
                if !self
                    .field(field)
                    .parse::<usize>()
                    .is_ok_and(|value| value > 0)
                {
                    self.field_set("settings-tab", "editing");
                    self.field_error(field, UiMessage::key("请输入大于零的整数"));
                    return true;
                }
            }
            if self.field("editing_text_bytes").parse::<usize>().ok()
                > self.field("editing_value_bytes").parse::<usize>().ok()
            {
                self.field_set("settings-tab", "editing");
                self.field_error(
                    "editing_text_bytes",
                    UiMessage::key("文本编辑上限不能超过单值上限"),
                );
                return true;
            }
            if self.field("external_mode") != "disabled" {
                if self.field("external_program").trim().is_empty()
                    || self.field("external_program").contains('\0')
                {
                    self.field_set("settings-tab", "editing");
                    self.field_error("external_program", UiMessage::key("请填写有效的编辑器程序"));
                    return true;
                }
                let args = self.field("external_args");
                if args.contains('\0') || args.matches("{file}").count() != 1 {
                    self.field_set("settings-tab", "editing");
                    self.field_error(
                        "external_args",
                        UiMessage::key("参数必须恰好包含一个 {file}"),
                    );
                    return true;
                }
            }
            for &(group, key, _) in SETTINGS_FIELDS {
                let name = settings_field(group, key);
                let text = self.field(&name).trim();
                if group == "history" && text.is_empty() {
                    continue;
                }
                let parsed = text.parse::<u64>();
                let upper = if (group == "paging" && key != "redis_key_batch")
                    || key == "mcp_response_rows"
                {
                    1000
                } else if key == "response_bytes" {
                    1_048_576
                } else {
                    u64::MAX
                };
                let lower = if key == "response_bytes" { 1024 } else { 1 };
                if !parsed.is_ok_and(|number| (lower..=upper).contains(&number)) {
                    self.field_set(
                        "settings-tab",
                        if settings_is_mcp(group, key) {
                            "mcp"
                        } else {
                            "general"
                        },
                    );
                    self.field_error(&name, UiMessage::key("设置超出允许范围"));
                    return true;
                }
            }
            let response = self
                .field("settings-limits-response_bytes")
                .parse::<u64>()
                .unwrap_or(0);
            for (field, valid) in [
                (
                    "settings-limits-value_preview_bytes",
                    self.field("settings-limits-value_preview_bytes")
                        .parse::<u64>()
                        .is_ok_and(|v| v <= response),
                ),
                (
                    "settings-limits-result_memory_bytes_per_client",
                    self.field("settings-limits-result_memory_bytes_per_client")
                        .parse::<u64>()
                        .is_ok_and(|v| v >= response),
                ),
            ] {
                if !valid {
                    self.field_set("settings-tab", "general");
                    self.field_error(field, UiMessage::key("设置超出允许范围"));
                    return true;
                }
            }
            use datapier_core::model::editing::{
                EditingLimits, ExternalEditorConfig, ExternalEditorMode,
            };
            let editing = (|| -> Result<(EditingLimits, Option<ExternalEditorConfig>)> {
                let number = |name: &str| {
                    self.field(name)
                        .parse::<usize>()
                        .map_err(|_| AppError::invalid("编辑限制必须为正整数"))
                };
                let limits = EditingLimits {
                    max_plan_items: number("editing_plan_items")?,
                    max_plan_bytes: number("editing_plan_bytes")?,
                    max_value_bytes: number("editing_value_bytes")?,
                    max_text_editor_bytes: number("editing_text_bytes")?,
                    preview_rows: number("editing_preview_rows")?,
                };
                limits.validate()?;
                let editor = match self.field("external_mode") {
                    "disabled" => None,
                    mode @ ("gui" | "terminal") => {
                        let config = ExternalEditorConfig {
                            mode: if mode == "gui" {
                                ExternalEditorMode::Gui
                            } else {
                                ExternalEditorMode::Terminal
                            },
                            program: self.field("external_program").to_owned(),
                            args: if self.field("external_args").is_empty() {
                                Vec::new()
                            } else {
                                self.field("external_args")
                                    .split('\n')
                                    .map(str::to_owned)
                                    .collect()
                            },
                        };
                        config.validate()?;
                        Some(config)
                    }
                    _ => return Err(AppError::invalid("请选择外部编辑器模式")),
                };
                Ok((limits, editor))
            })();
            let (limits, editor) = match editing {
                Ok(value) => value,
                Err(error) => {
                    self.field_set("settings-error", i18n::tr(&error.message).into_owned());
                    self.status = i18n::UiMessage::local(error.message);
                    return true;
                }
            };
            let Ok(mut value) = serde_json::to_value(&self.settings) else {
                return true;
            };
            value["editing"] = json!(limits);
            value["external_editor"] = json!(editor);
            value["language"] = json!(self.field("settings-language-draft"));
            for &(group, key, label) in SETTINGS_FIELDS {
                let text = self.field(&settings_field(group, key)).trim();
                let parsed = if text.is_empty() && group == "history" {
                    Value::Null
                } else if let Ok(number) = text.parse::<u64>() {
                    json!(number)
                } else {
                    self.field_error(
                        &settings_field(group, key),
                        UiMessage::key("请输入有效的非负整数"),
                    );
                    self.field_set(
                        "settings-error",
                        i18n::format_message(
                            "无效设置: {setting}",
                            &[("setting", &i18n::tr(label))],
                        ),
                    );
                    self.status = i18n::UiMessage::template(
                        "无效设置: {setting}",
                        vec![("setting", i18n::UiArg::Raw(format!("{group}.{key}")))],
                    );
                    return true;
                };
                value[group][key] = parsed;
            }
            for &(group, key, _) in SETTINGS_SWITCHES {
                value[group][key] = json!(self.field(&settings_field(group, key)) == "true");
            }
            value["approval"]["default_policy"] =
                json!(self.field("settings-approval-default_policy"));
            match serde_json::from_value::<AppSettings>(value) {
                Ok(settings) => {
                    self.settings = settings;
                    self.settings_epoch = self.settings_epoch.wrapping_add(1);
                    self.settings_unsaved = true;
                    if self.settings_save_token.is_none() {
                        self.save_settings(backend);
                    }
                }
                Err(error) => {
                    self.field_set("settings-error", error.to_string());
                    self.status = i18n::UiMessage::raw(error.to_string());
                }
            }
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
        let visible = self.profile.as_ref().is_some_and(|p| p.is_archery());
        let history = &self.workbench.archery_history;
        let same_target = history.limit != 0
            && history.visible == visible
            && history.tab == self.tabs.get(self.active).map(|tab| tab.id)
            && history.profile == self.profile.as_ref().map(|profile| profile.id)
            && history.database == self.database;
        if !same_target {
            self.invalidate_history(backend);
            self.field_set("history_filter", "");
            self.workbench.archery_history = ArcheryHistory {
                limit: 20,
                visible,
                ..Default::default()
            };
        }
        self.workbench.overlay = Some("history".into());
        self.workbench.focus_request = Some("input-history_filter".into());
        if !same_target {
            self.request_history(backend);
        }
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
