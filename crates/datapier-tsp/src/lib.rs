//! TSP 界面及编辑状态，通过 Engine IPC 操作数据；Session 由运行入口所在 UI 线程独占。

mod action;
mod application;
mod backend;
mod change_preview;
mod clipboard;
#[cfg(test)]
#[path = "../tests/unit/close_regression.rs"]
mod close_regression;
mod connections;
mod editor;
mod forms;
mod grid;
mod i18n;
mod menus;
mod navigation;
mod query_ui;
mod redis_key_selection;
mod redis_rows;
mod redis_selection;
mod redis_tree;
#[cfg(test)]
#[path = "../tests/unit/regression.rs"]
mod regression;
mod response;
mod runtime;
mod tabs;
mod views;
mod workbench;

pub use runtime::run;

use backend::{Backend, Response};
use datapier_core::{
    Result,
    model::{AppError, Profile, Resource, ResourcePage, ResourceTarget, ResultPage},
    model::{AppSettings, LanguagePreference},
};
use editor::{Editor, Key as EditKey};
use grid::{Cell, GridState, Navigation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    time::{Duration, Instant},
};
use tern_sdk::ui::{self, Node, View, html};
use uuid::Uuid;
use workbench::Workbench;

#[derive(Clone)]
enum Msg {
    Action(String),
    Focus(String),
    Edit(String, usize, usize, String, usize, usize),
    Undo(String),
    Profile(usize),
    Tab(usize),
    ResultSet(String),
    Cell(u64, usize, bool),
    CellOpen(u64, usize),
    Row(u64, usize),
    Complete(String, usize, usize),
    CopyText(String),
    CopyRedisValue,
    CopyGrid,
    GridMenu(Cell, menus::GridMenuAction),
    QueryMenu(Uuid, String, usize, usize, String),
    QueryCut(Uuid, String, usize, usize),
}
#[derive(Clone)]
struct LargeLoad {
    cell: Cell,
    id: Uuid,
    offset: u64,
    bytes: Vec<u8>,
}
impl LargeLoad {
    fn consume(&mut self, value: &Value) -> Result<Option<datapier_core::model::Value>> {
        use base64::Engine as _;
        let encoded = value
            .get("bytes_b64")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("缺少字段分块"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| AppError::invalid(error.to_string()))?;
        if value.get("offset").and_then(Value::as_u64) != Some(self.offset) {
            return Err(AppError::invalid("大字段分块偏移不一致"));
        }
        let end = self
            .offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| AppError::invalid("大字段长度溢出"))?;
        self.bytes.extend_from_slice(&bytes);
        if let Some(next) = value.get("next").and_then(Value::as_u64) {
            if next != end || next <= self.offset {
                return Err(AppError::invalid("大字段分块不连续"));
            }
            self.offset = next;
            return Ok(None);
        }
        if value.get("total").and_then(Value::as_u64) != Some(end) {
            return Err(AppError::invalid("大字段长度不一致"));
        }
        Ok(Some(serde_json::from_slice(&self.bytes)?))
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct BrowseSettings {
    pattern: String,
    kind: String,
    filter: String,
    sort: String,
    #[serde(default)]
    object_filter: String,
    #[serde(default)]
    row_search: String,
    #[serde(default)]
    cli_visible: bool,
    database: Option<String>,
    target: Option<ResourceTarget>,
    #[serde(default)]
    redis_db: Option<u32>,
}
impl Default for BrowseSettings {
    fn default() -> Self {
        Self {
            pattern: "*".into(),
            kind: String::new(),
            filter: "[]".into(),
            sort: "[]".into(),
            object_filter: String::new(),
            row_search: String::new(),
            cli_visible: false,
            database: None,
            target: None,
            redis_db: None,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
struct Tab {
    title: String,
    query: Editor,
    #[serde(default)]
    cli_query: Editor,
    profile_id: Option<Uuid>,
    #[serde(default = "Uuid::new_v4")]
    id: Uuid,
    #[serde(default)]
    layout: Vec<grid::ColumnSetting>,
    #[serde(default)]
    browse: BrowseSettings,
    #[serde(default)]
    pending: Value,
    #[serde(skip)]
    pending_live: bool,
    #[serde(skip)]
    runtime: TabRuntime,
}
#[derive(Clone)]
struct DetailContext {
    kind: &'static str,
    connection: Option<String>,
    resource: Option<String>,
}

impl Default for DetailContext {
    fn default() -> Self {
        Self {
            kind: "详情",
            connection: None,
            resource: None,
        }
    }
}

// 活动 tab 的运行状态存放在 App，其余 tab 存放在各自的 runtime；切换时成对交换。
// generation 随连接或资源上下文变化，用于丢弃旧上下文的异步响应。
#[derive(Clone, Default)]
struct TabRuntime {
    profile: Option<Profile>,
    session: Option<Value>,
    resources: Vec<Resource>,
    cursor: Option<String>,
    scan_token: Option<Uuid>,
    database: Option<String>,
    target: Option<ResourceTarget>,
    task: Option<String>,
    operation_pending: bool,
    results: Vec<String>,
    result_id: Option<String>,
    page_history: Vec<u64>,
    page_token: Option<Uuid>,
    grid: Option<GridState>,
    schema: Option<Value>,
    edit_token: Option<Value>,
    pending_rows: Vec<datapier_core::model::RowChange>,
    redis_changes: Vec<Value>,
    redis_changes_revision: u64,
    large: Option<LargeLoad>,
    preview: Option<Value>,
    preview_pending: Option<(Uuid, Value)>,
    detail: String,
    detail_context: DetailContext,
    paste_preview: Option<grid::PastePreview>,
    browse_result: bool,
    generation: u64,
    current_pending: Option<(Uuid, Editor)>,
    file_open_pending: Option<(Uuid, String, u64)>,
    query_edit_epoch: u64,
    workbench: Workbench,
    view: String,
}
#[derive(Serialize, Deserialize)]
struct Workspace {
    tabs: Vec<Tab>,
    active: usize,
    pending: Value,
    #[serde(default = "expanded_by_default")]
    navigation_expanded: bool,
}

fn expanded_by_default() -> bool {
    true
}

/// 每个 UI 会话只上传一次图标，视图复用 blob ID。
struct IconBlobs {
    brand: String,
    mysql: String,
    redis: String,
}

struct App {
    workspace_id: String,
    revision: u64,
    workspace_loaded: bool,
    restore_focus_pending: bool,
    workspace_interacted: bool,
    saving: bool,
    save_blocked: bool,
    copy_pending: Option<String>,
    dirty: bool,
    tabs: Vec<Tab>,
    active: usize,
    fields: BTreeMap<String, Editor>,
    focus: String,
    profiles: Vec<Profile>,
    profile: Option<Profile>,
    profile_list_token: Option<Uuid>,
    profile_saves: std::collections::BTreeSet<Uuid>,
    form_id: Option<Uuid>,
    form_editing: bool,
    session: Option<Value>,
    resources: Vec<Resource>,
    cursor: Option<String>,
    scan_token: Option<Uuid>,
    database: Option<String>,
    target: Option<ResourceTarget>,
    task: Option<String>,
    operation_pending: bool,
    results: Vec<String>,
    result_id: Option<String>,
    page_history: Vec<u64>,
    page_token: Option<Uuid>,
    grid: Option<GridState>,
    schema: Option<Value>,
    edit_token: Option<Value>,
    pending_rows: Vec<datapier_core::model::RowChange>,
    redis_changes: Vec<Value>,
    redis_changes_revision: u64,
    large: Option<LargeLoad>,
    preview: Option<Value>,
    preview_pending: Option<(Uuid, Value)>,
    browse_result: bool,
    generation: u64,
    current_pending: Option<(Uuid, Editor)>,
    file_open_pending: Option<(Uuid, String, u64)>,
    query_edit_epoch: u64,
    paste_preview: Option<grid::PastePreview>,
    status: i18n::UiMessage,
    detail: String,
    detail_context: DetailContext,
    view: String,
    workbench: Workbench,
    last_poll: Instant,
    viewport_width: u32,
    navigation: navigation::NavigationState,
    icon_blobs: Option<IconBlobs>,
    settings: AppSettings,
    settings_epoch: u64,
    settings_save_token: Option<Uuid>,
    settings_unsaved: bool,
    last_settings_poll: Instant,
    tab_close: Option<tabs::CloseOperation>,
    change_preview: change_preview::PreviewState,
    redis_keys: redis_key_selection::RedisKeySelection,
    redis_key_delete: redis_selection::RedisKeyDeleteState,
}
impl App {
    fn new(workspace_id: String) -> Self {
        let mut fields = BTreeMap::new();
        for (name, value) in connections::PROFILE_DEFAULTS.iter().copied().chain([
            ("pattern", "*"),
            ("type", ""),
            ("path", ""),
            ("format", "csv"),
            ("mapping", "{}"),
            ("filter", "[]"),
            ("sort", "[]"),
            ("operation", "set_string"),
            ("redis_value", ""),
            ("redis_field", ""),
            ("redis_member", ""),
            ("redis_index", "0"),
            ("redis_score", "0"),
            ("redis_key", ""),
            ("redis_ttl", ""),
            ("column", "0"),
            ("width", "160"),
            ("object_filter", ""),
            ("history_filter", ""),
            ("row_search", ""),
        ]) {
            fields.insert(name.into(), Editor::new(value));
        }
        fields.insert("null_marker".into(), Editor::new("\\N"));
        fields.insert("backslash_escape".into(), Editor::new("true"));
        fields.insert("binary_columns".into(), Editor::new("[]"));
        Self {
            workspace_id,
            revision: 0,
            workspace_loaded: false,
            saving: false,
            save_blocked: false,
            copy_pending: None,
            dirty: false,
            restore_focus_pending: false,
            workspace_interacted: false,
            tabs: vec![Tab {
                title: "查询 1".into(),
                query: Editor::new(""),
                cli_query: Editor::default(),
                profile_id: None,
                id: Uuid::new_v4(),
                layout: Vec::new(),
                browse: BrowseSettings::default(),
                pending: Value::Null,
                pending_live: false,
                runtime: TabRuntime::default(),
            }],
            active: 0,
            fields,
            focus: "query".into(),
            profiles: vec![],
            profile: None,
            profile_list_token: None,
            profile_saves: Default::default(),
            form_id: None,
            form_editing: false,
            session: None,
            resources: vec![],
            cursor: None,
            scan_token: None,
            database: None,
            target: None,
            task: None,
            operation_pending: false,
            results: Vec::new(),
            result_id: None,
            page_history: Vec::new(),
            page_token: None,
            grid: None,
            schema: None,
            edit_token: None,
            pending_rows: Vec::new(),
            redis_changes: Vec::new(),
            redis_changes_revision: 0,
            large: None,
            preview: None,
            preview_pending: None,
            browse_result: false,
            generation: 0,
            current_pending: None,
            file_open_pending: None,
            query_edit_epoch: 0,
            paste_preview: None,
            status: "正在连接 Engine…".into(),
            detail: String::new(),
            detail_context: DetailContext::default(),
            view: "query".into(),
            workbench: Workbench::default(),
            last_poll: Instant::now(),
            viewport_width: 0,
            navigation: navigation::NavigationState::default(),
            icon_blobs: None,
            settings: AppSettings::default(),
            settings_epoch: 0,
            settings_save_token: None,
            settings_unsaved: false,
            tab_close: None,
            change_preview: change_preview::PreviewState::default(),
            redis_keys: Default::default(),
            redis_key_delete: Default::default(),
            last_settings_poll: Instant::now(),
        }
    }
    fn field(&self, name: &str) -> &str {
        if name == "cli_text" {
            self.tabs
                .get(self.active)
                .map_or("", |tab| tab.cli_query.text.as_str())
        } else if name == "cell" {
            &self.workbench.cell_editor.text
        } else if name == "row_values" {
            &self.workbench.sql_forms.row_json.text
        } else {
            self.fields.get(name).map(|x| x.text.as_str()).unwrap_or("")
        }
    }
    fn editor(&mut self, name: &str) -> Option<&mut Editor> {
        if name == "query" {
            self.tabs.get_mut(self.active).map(|x| &mut x.query)
        } else if name == "cli_text" {
            self.tabs.get_mut(self.active).map(|tab| &mut tab.cli_query)
        } else if name == "cell" {
            Some(&mut self.workbench.cell_editor)
        } else if name == "row_values" || name.starts_with("sql-form-") {
            self.workbench.form_editor(name)
        } else {
            self.fields.get_mut(name)
        }
    }
    fn field_set(&mut self, name: &str, text: impl Into<String>) {
        if self
            .workbench
            .field_error
            .as_ref()
            .is_some_and(|error| error.field == name)
        {
            self.workbench.field_error = None;
        }
        let text = text.into();
        if name == "history_filter" && self.field(name) != text {
            self.workbench.history_selected = None;
        }
        let editor = Editor::new(text);
        if name == "cli_text" {
            if let Some(tab) = self.tabs.get_mut(self.active) {
                tab.cli_query = editor;
            }
        } else if name == "cell" {
            self.workbench.cell_editor = editor;
        } else if name == "row_values" {
            self.workbench.sql_forms.row_json = editor;
        } else {
            self.fields.insert(name.into(), editor);
        }
        if name == "row_search" {
            self.sync_grid_visible_rows();
        }
    }
    fn has_live_drafts(&self) -> bool {
        self.grid.as_ref().is_some_and(|g| !g.edits().is_empty())
            || !self.pending_rows.is_empty()
            || !self.redis_changes.is_empty()
            || self
                .tabs
                .get(self.active)
                .is_some_and(|tab| self.redis_key_delete_tab_pending(tab.id))
            || self.paste_preview.is_some()
    }
    fn changed_field(&mut self, name: &str) {
        if self
            .workbench
            .field_error
            .as_ref()
            .is_some_and(|error| error.field == name)
        {
            self.workbench.field_error = None;
        }
        if name == "row_search" {
            self.sync_grid_visible_rows();
        }
        if matches!(
            name,
            "name"
                | "host"
                | "port"
                | "user"
                | "password"
                | "provider"
                | "tls"
                | "ca"
                | "cert"
                | "key"
                | "database"
                | "redis_db"
        ) {
            self.navigation_form_changed();
        }
        if name == "query" {
            self.query_edit_epoch = self.query_edit_epoch.wrapping_add(1);
            self.workbench.query_ui.menu_open = false;
            self.workbench.query_ui.executed = false;
        }
        self.workbench.form_changed(name);
        self.dirty = true;
        // 查询文本失效时清除旧结果；基表数据继续由资源浏览持有，不能随查询草稿丢弃。
        if name == "query" && !self.browse_result && !self.is_redis() {
            self.grid = None;
            self.results.clear();
            self.result_id = None;
            self.page_history.clear();
            self.page_token = None;
            self.large = None;
        }
        if matches!(name, "pattern" | "type") {
            self.scan_token = None;
            self.cursor = None;
            self.workbench.scan_dirty = true;
        }
    }
    fn migrate_cli_queries(&mut self) {
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

    fn capture_browse(&mut self) {
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
            redis_db: if self.session.is_none() {
                self.tabs[self.active].browse.redis_db.or_else(|| {
                    self.profile
                        .as_ref()
                        .filter(|p| p.provider == datapier_core::model::ProviderKind::Redis)
                        .map(|p| p.redis_db)
                })
            } else {
                self.profile
                    .as_ref()
                    .filter(|p| p.provider == datapier_core::model::ProviderKind::Redis)
                    .map(|p| p.redis_db)
            },
        };
        self.tabs[self.active].browse = browse;
    }
    fn restore_browse(&mut self) {
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
    fn select_tab(&mut self, next: usize) {
        if next >= self.tabs.len() || !self.finish_inline(true) {
            return;
        }
        if next != self.active {
            self.change_preview = change_preview::PreviewState::default();
        }
        self.workbench.overlay.take_if(|kind| kind == "history");
        self.switch_tab(next);
        self.workbench.overlay.take_if(|kind| kind == "history");
        self.focus_current_tab();
        self.dirty = true;
    }
    /// 用户切换需同时恢复键盘路由与宿主字段；后台临时 switch_tab 不触碰焦点。
    fn focus_current_tab(&mut self) {
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
    fn set_detail_context(&mut self, kind: &'static str) {
        let resource = self
            .target
            .as_ref()
            .map(|target| match target.database.as_deref() {
                Some(database) => format!("{database}.{}", target.name),
                None => target.name.clone(),
            });
        self.detail_context = DetailContext {
            kind,
            connection: self.profile.as_ref().map(|profile| profile.name.clone()),
            resource,
        };
    }

    fn set_recovery_detail_context(&mut self) {
        self.set_detail_context("恢复详情");
        self.detail_context.resource = self.tabs[self.active]
            .pending
            .get("target")
            .and_then(|value| serde_json::from_value::<ResourceTarget>(value.clone()).ok())
            .map(|target| match target.database {
                Some(database) => format!("{database}.{}", target.name),
                None => target.name,
            });
    }

    fn switch_tab(&mut self, next: usize) {
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
    fn request_diagnostic_task(&mut self, backend: &Backend) {
        self.workbench.diagnostic_task_request = None;
        if self.detail_context.kind != "任务详情" {
            return;
        }
        let Some(task) = serde_json::from_str::<Value>(&self.detail)
            .ok()
            .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned))
            .filter(|id| Uuid::parse_str(id).is_ok())
        else {
            return;
        };
        let token = Uuid::new_v4();
        self.workbench.diagnostic_task_request = Some((token, task.clone()));
        self.request(
            backend,
            &format!("diagnostics.task/{token}"),
            "execution.get",
            json!({"task_id":task,"include_text":true}),
        );
    }

    fn diagnostic_task_response(&mut self, token: &str, result: &Result<Value>) {
        let Some((expected, task)) = self.workbench.diagnostic_task_request.as_ref() else {
            return;
        };
        if Uuid::parse_str(token).ok() != Some(*expected) {
            return;
        }
        let task = task.clone();
        self.workbench.diagnostic_task_request = None;
        if self.workbench.overlay.as_deref() != Some("diagnostics")
            || self.detail_context.kind != "任务详情"
        {
            return;
        }
        let current = serde_json::from_str::<Value>(&self.detail).ok();
        if current
            .as_ref()
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            != Some(task.as_str())
        {
            return;
        }
        match result {
            Ok(value) if value.get("id").and_then(Value::as_str) == Some(task.as_str()) => {
                self.detail = value.to_string()
            }
            Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
            _ => {}
        }
    }

    // 异步响应先按 tab 和 generation 路由，任务、分页等再核对各自的 ID，
    // 避免迟到响应覆盖切换后的上下文；处理后台 tab 后恢复原活动 tab。
    fn routed_response(&mut self, mut response: Response, backend: &Backend) {
        if self.redis_selection_response(backend, &response.tag, &response.result) {
            return;
        }
        if self.tabs_close_response(&response, backend) {
            return;
        }
        if let Some((kind, id)) = response.tag.split_once('/')
            && matches!(kind, "profile.save" | "profile.test")
            && Uuid::parse_str(id).ok() == self.form_id
        {
            if kind == "profile.save"
                && let Ok(id) = Uuid::parse_str(id)
            {
                self.profile_saves.remove(&id);
            }
            response.tag = kind.to_owned();
            self.response(response, backend);
            return;
        }
        if let Some(token) = response.tag.strip_prefix("profile.list/") {
            if Uuid::parse_str(token).ok() != self.profile_list_token {
                return;
            }
            self.profile_list_token = None;
            response.tag = "profile.list".into();
        }
        if let Some(id) = response
            .tag
            .rsplit(':')
            .next()
            .and_then(|tag| tag.strip_prefix("profile.save/"))
            .and_then(|id| Uuid::parse_str(id).ok())
        {
            self.profile_saves.remove(&id);
        }
        if response.tag.starts_with("settings.") {
            self.settings_response(response, backend);
            return;
        }
        if self.navigation_response(&response, backend) {
            return;
        }
        let mut pieces = response.tag.splitn(3, ':');
        let route = pieces
            .next()
            .and_then(|s| Uuid::parse_str(s).ok())
            .and_then(|id| {
                Some((
                    id,
                    pieces.next()?.parse::<u64>().ok()?,
                    pieces.next()?.to_owned(),
                ))
            });
        if let Some((id, generation, tag)) = route {
            let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
                return;
            };
            if self.tab_is_closing(id) {
                return;
            }
            let old = self.active;
            let old_status = (index != old).then(|| self.status.clone());
            self.switch_tab(index);
            if generation == self.generation {
                if self.redis_row_delete_response(&tag, &response.result) {
                } else if let Some(token) = tag.strip_prefix("change.preview/") {
                    self.change_preview_response(token, &response.result);
                } else if let Some(token) = tag.strip_prefix("diagnostics.task/") {
                    self.diagnostic_task_response(token, &response.result);
                } else if let Some(task) = tag.strip_prefix("execution.get/") {
                    if self.task.as_deref() == Some(task) {
                        response.tag = "execution.get".into();
                        self.response(response, backend);
                    }
                } else if let Some(token) = tag.strip_prefix("resource.list/") {
                    if Uuid::parse_str(token).ok() == self.scan_token {
                        self.scan_token = None;
                        response.tag = "resource.list".into();
                        self.response(response, backend);
                    }
                } else if let Some(token) = tag.strip_prefix("result.page/") {
                    if Uuid::parse_str(token).ok() == self.page_token {
                        self.page_token = None;
                        response.tag = "result.page".into();
                        self.response(response, backend);
                    }
                } else if let Some(token) = tag.strip_prefix("transfer.preview/") {
                    if Uuid::parse_str(token).ok()
                        == self.preview_pending.as_ref().map(|(id, _)| *id)
                    {
                        response.tag = "transfer.preview".into();
                        self.response(response, backend);
                    }
                } else if let Some(token) = tag.strip_prefix("current.info/") {
                    if Uuid::parse_str(token).ok()
                        == self.current_pending.as_ref().map(|(id, _)| *id)
                    {
                        response.tag = "current.info".into();
                        self.response(response, backend);
                    }
                } else if let Some(token) = tag.strip_prefix("file.open/") {
                    if Uuid::parse_str(token).ok()
                        == self.file_open_pending.as_ref().map(|(id, _, _)| *id)
                    {
                        response.tag = "file.open".into();
                        self.response(response, backend);
                    }
                } else if let Some((kind, id)) = tag.split_once('/')
                    && matches!(kind, "profile.save" | "profile.test")
                {
                    if Uuid::parse_str(id).ok() == self.form_id {
                        response.tag = kind.into();
                        self.response(response, backend);
                    } else if kind == "profile.save" {
                        self.request(backend, "profile.list", "profile.list", json!({}));
                    }
                } else {
                    response.tag = tag;
                    self.response(response, backend);
                }
            }
            self.switch_tab(old);
            if let Some(status) = old_status {
                self.status = status;
            }
        } else {
            self.response(response, backend);
        }
    }
    fn request(&mut self, backend: &Backend, tag: &str, method: &str, params: Value) {
        if method == "connection.open"
            && params["profile_id"]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok())
                .is_some_and(|id| self.navigation_profile_delete_pending(id))
        {
            self.status = i18n::UiMessage::key("正在删除连接，请等待");
            return;
        }
        let global = matches!(tag, "profile.list" | "workspace.load" | "workspace.save")
            || method == "profile.save"
            || method == "profile.test";
        let long = matches!(
            method,
            "resource.browse"
                | "connection.open"
                | "execution.start"
                | "change.submit"
                | "transfer.import"
                | "transfer.export"
        );
        if long && (self.operation_pending || self.task.is_some()) {
            self.status = "该 tab 已有正在执行的任务".into();
            return;
        }
        if matches!(
            method,
            "execution.start" | "resource.browse" | "change.open"
        ) && self.has_live_drafts()
        {
            self.status = "请先提交或丢弃待提交变更与粘贴预览".into();
            return;
        }
        let save_id = (method == "profile.save")
            .then(|| {
                params["profile"]["id"]
                    .as_str()
                    .and_then(|id| Uuid::parse_str(id).ok())
            })
            .flatten();
        if let Some(id) = save_id
            && (self.profile_saves.contains(&id) || self.navigation_profile_delete_pending(id))
        {
            self.status = i18n::UiMessage::key("该连接存在草稿或任务，请先处理");
            return;
        }
        let tag = if tag == "profile.list" {
            let token = Uuid::new_v4();
            self.profile_list_token = Some(token);
            format!("profile.list/{token}")
        } else if global {
            tag.to_owned()
        } else {
            format!("{}:{}:{tag}", self.tabs[self.active].id, self.generation)
        };
        match backend.request(tag, method, params) {
            Ok(()) => {
                if let Some(id) = save_id {
                    self.profile_saves.insert(id);
                }
                if long {
                    self.operation_pending = true
                }
                if method == "connection.open" {
                    self.workbench.connecting = true;
                    self.workbench.connection_error = None;
                    self.workbench.connection_tls_required = false;
                }
            }
            Err(e) => {
                let error = e.to_string();
                if method == "connection.open" {
                    self.workbench.connecting = false;
                    self.workbench.connection_error = Some(error.clone());
                    self.workbench.connection_tls_required = e.code == "tls_required";
                } else if matches!(method, "profile.save" | "profile.test") {
                    self.workbench.profile_error = Some(error.clone());
                }
                self.status = i18n::UiMessage::raw(error);
            }
        }
    }
    fn open_cell(&mut self, backend: &Backend) {
        let Some(cell) = self.grid.as_ref().and_then(|grid| grid.focus) else {
            self.status = "没有选中单元格".into();
            return;
        };
        self.open_cell_at(cell, backend);
    }
    fn open_cell_at(&mut self, cell: Cell, backend: &Backend) {
        let Some(g) = self.grid.as_ref() else {
            self.status = "没有结果".into();
            return;
        };
        if !g.row_in_page(cell.row) || g.value(cell).is_none() {
            self.status = "目标单元格已不可用".into();
            return;
        }
        self.workbench.cell_target = Some((g.page.result.id, cell));
        self.workbench.cell_fields_open = false;
        self.large = None;
        if let Some(datapier_core::model::Value::Large { id, .. }) = g.value(cell) {
            let id = *id;
            // 完整值就绪前不展示旧输入，防止旧值被写入新目标。
            if self.workbench.overlay.as_deref() == Some("cell") {
                self.workbench.overlay = None;
            }
            self.large = Some(LargeLoad {
                cell,
                id,
                offset: 0,
                bytes: Vec::new(),
            });
            self.status = "正在读取完整大字段…".into();
            self.request(
                backend,
                &format!("large.value/{id}/0"),
                "result.value",
                json!({"value_id":id,"offset":0,"max_bytes":262144}),
            );
        } else {
            match g.cell_edit_text(cell) {
                Ok(text) => {
                    self.field_set("cell", text);
                    self.workbench.overlay = Some("cell".into());
                    self.workbench.focus_request = Some("input-cell".into());
                }
                Err(e) => self.status = workbench::grid_cell_error(e, g, cell),
            }
        }
    }
    fn large_response(&mut self, tag: &str, value: Value, backend: &Backend) {
        let Some(mut load) = self.large.take() else {
            return;
        };
        if tag != format!("large.value/{}/{}", load.id, load.offset) {
            self.large = Some(load);
            return;
        }
        let full = match load.consume(&value) {
            Ok(Some(full)) => full,
            Ok(None) => {
                let id = load.id;
                let next = load.offset;
                self.large = Some(load);
                self.request(
                    backend,
                    &format!("large.value/{id}/{next}"),
                    "result.value",
                    json!({"value_id":id,"offset":next,"max_bytes":262144}),
                );
                return;
            }
            Err(error) => {
                self.status = i18n::UiMessage::raw(error.to_string());
                return;
            }
        };
        let Some(g) = self.grid.as_mut() else {
            self.status = "结果已关闭".into();
            return;
        };
        if let Err(e) = g.resolve_large(load.cell, full) {
            self.status = workbench::grid_cell_error(e, g, load.cell);
            return;
        }
        if let Some(original) = self.workbench.redis_loading.take() {
            g.focus = Some(original);
            self.workbench_redis_edit(backend);
            return;
        }
        if g.focus != Some(load.cell)
            && self.workbench.cell_target != Some((g.page.result.id, load.cell))
        {
            self.status = "已读取完整字段".into();
            return;
        }
        match g.cell_edit_text(load.cell) {
            Ok(text) => {
                self.field_set("cell", text);
                if self.workbench.inline == Some(load.cell) {
                    self.focus = "cell".into();
                    self.workbench.focus_request = Some("cell-editor".into());
                } else {
                    self.workbench.overlay = Some("cell".into());
                    self.workbench.focus_request = Some("input-cell".into());
                }
                self.status = "已读取完整字段".into();
            }
            Err(e) => self.status = workbench::grid_cell_error(e, g, load.cell),
        }
    }
    fn session_id(&self) -> Option<Value> {
        self.session
            .as_ref()
            .and_then(|s| s.get("session_id"))
            .cloned()
    }
    fn redis_operation(&self) -> std::result::Result<Value, i18n::UiMessage> {
        use base64::Engine as _;
        let value = |name: &str| -> std::result::Result<Value, i18n::UiMessage> {
            let s = self.field(name);
            if let Some(data) = s.strip_prefix("base64:") {
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| i18n::UiMessage::raw(e.to_string()))?;
                Ok(json!({"type":"bytes","value":data}))
            } else {
                Ok(json!({"type":"text","value":s}))
            }
        };
        let op = self.field("operation");
        Ok(match op {
            "set_string" => json!({"operation":op,"value":value("redis_value")?}),
            "hash_set" => {
                json!({"operation":op,"field":value("redis_field")?,"value":value("redis_value")?})
            }
            "hash_delete" => json!({"operation":op,"field":value("redis_field")?}),
            "list_set" | "list_insert" => {
                json!({"operation":op,"index":self.field("redis_index").parse::<i64>().map_err(|e|i18n::UiMessage::raw(e.to_string()))?,"value":value("redis_value")?})
            }
            "list_delete" => {
                json!({"operation":op,"index":self.field("redis_index").parse::<i64>().map_err(|e|i18n::UiMessage::raw(e.to_string()))?})
            }
            "set_add" | "set_remove" | "zset_remove" => {
                json!({"operation":op,"member":value("redis_member")?})
            }
            "zset_set" => {
                json!({"operation":op,"member":value("redis_member")?,"score":self.field("redis_score")})
            }
            "rename" => {
                let name = self.field("redis_key");
                let encoded = if let Some(encoded) = name.strip_prefix("base64:") {
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|e| i18n::UiMessage::raw(e.to_string()))?;
                    encoded.to_string()
                } else {
                    base64::engine::general_purpose::STANDARD.encode(name.as_bytes())
                };
                json!({"operation":op,"new_key_b64":encoded})
            }
            "delete_key" => json!({"operation":op}),
            "set_ttl" => {
                let ttl = if self.workbench.ttl_policy == workbench::RedisTtlPolicy::Persist
                    || self.field("redis_ttl").is_empty()
                {
                    None
                } else {
                    Some(
                        self.field("redis_ttl")
                            .parse::<u64>()
                            .map_err(|e| i18n::UiMessage::raw(e.to_string()))?,
                    )
                };
                json!({"operation":op,"ttl_ms":ttl})
            }
            _ => return Err(i18n::UiMessage::key("未知 Redis 操作")),
        })
    }
    // 持久化编辑意图和布局，而非可继续提交的数据库基线；恢复后必须刷新并重新确认。
    fn workspace_snapshot(&mut self) -> Result<Workspace> {
        self.capture_browse();
        let key_intents: BTreeMap<_, _> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                self.redis_key_delete_snapshot(tab.id)
                    .map(|intent| (tab.id, intent))
            })
            .collect();
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let (target, grid, rows, redis, schema) = if index == self.active {
                (
                    &self.target,
                    &self.grid,
                    &self.pending_rows,
                    &self.redis_changes,
                    &self.schema,
                )
            } else {
                (
                    &tab.runtime.target,
                    &tab.runtime.grid,
                    &tab.runtime.pending_rows,
                    &tab.runtime.redis_changes,
                    &tab.runtime.schema,
                )
            };
            if let Some(g) = grid {
                tab.layout = g.settings.clone();
            }
            if let Some(intent) = key_intents.get(&tab.id) {
                tab.pending = intent.clone();
                tab.pending_live = true;
            } else if grid.as_ref().is_some_and(|g| !g.edits().is_empty())
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
                    match g.changes(&keys) {Ok(updates)=>changes.extend(updates),Err(_)=>fallback=g.edits().iter().map(|(cell,after)|json!({"row":cell.row,"column":cell.column,"before_row":g.original_row(cell.row),"after":after})).collect()}
                }
                tab.pending = json!({"target":target,"rows":changes,"redis":redis,"rows_include_edits":fallback.is_empty(),"edits":fallback,"requires_refresh":true});
                tab.pending_live = true;
            } else if tab.pending_live {
                tab.pending = Value::Null;
                tab.pending_live = false;
            }
        }
        Ok(Workspace {
            tabs: self.tabs.clone(),
            active: self.active,
            pending: Value::Null,
            navigation_expanded: self.navigation.expanded,
        })
    }
    // 保存携带已知 revision；失败后阻止自动重试，避免覆盖另一个 UI 的修改。
    // 请求期间的新修改会再次置 dirty，待确认后按新 revision 保存。
    fn save_workspace(&mut self, backend: &Backend) {
        if !self.workspace_loaded || self.saving || self.save_blocked || !self.dirty {
            return;
        }
        let state = match self.workspace_snapshot() {
            Ok(state) => state,
            Err(e) => {
                self.status = i18n::UiMessage::raw(e.to_string());
                self.save_blocked = true;
                return;
            }
        };
        match backend.request("workspace.save", "workspace.save", json!({"workspace_id":self.workspace_id,"expected_revision":self.revision,"state":state})) {Ok(())=>{self.saving=true;self.dirty=false;},Err(e)=>{self.status=i18n::UiMessage::raw(e.to_string());self.save_blocked=true;}}
    }
    fn save_workspace_copy(&mut self, backend: &Backend) {
        if self.saving || self.copy_pending.is_some() {
            self.status = "工作区保存仍在进行".into();
            return;
        }
        let state = match self.workspace_snapshot() {
            Ok(state) => state,
            Err(e) => {
                self.status = i18n::UiMessage::raw(e.to_string());
                return;
            }
        };
        let id = format!("{}-copy-{}", self.workspace_id, Uuid::new_v4());
        match backend.request(
            "workspace.copy",
            "workspace.save",
            json!({"workspace_id":id,"expected_revision":0,"state":state}),
        ) {
            Ok(()) => {
                self.copy_pending = Some(id);
                self.status = "正在另存工作区副本…".into();
            }
            Err(e) => self.status = i18n::UiMessage::raw(e.to_string()),
        }
    }
    /// 连接与资源各自保留 tab 运行态，切换对象不覆盖另一对象的草稿和结果。
    fn select_profile(&mut self, index: usize, backend: &Backend) {
        let Some(profile) = self.profiles.get(index).cloned() else {
            return;
        };
        if self.tabs.is_empty() {
            self.action("new-tab", backend);
        }
        if let Some(index) = self.tabs.iter().enumerate().find_map(|(index, tab)| {
            let session = if index == self.active {
                &self.session
            } else {
                &tab.runtime.session
            };
            (tab.profile_id == Some(profile.id) && session.is_some()).then_some(index)
        }) {
            self.select_tab(index);
            return;
        }
        if self.session.is_some()
            || self.has_live_drafts()
            || self.operation_pending
            || self.task.is_some()
            || (self.tabs[self.active].profile_id != Some(profile.id)
                && (self.target.is_some()
                    || !self.tabs[self.active].query.text.is_empty()
                    || !self.tabs[self.active].cli_query.text.is_empty()))
        {
            self.action("new-tab", backend);
        }
        self.profile = Some(profile.clone());
        self.tabs[self.active].profile_id = Some(profile.id);
        self.field_set("password", "");
        self.workbench.overlay = None;
        self.dirty = true;
        self.action("open", backend);
        self.focus_current_tab();
    }

    fn open_query_tab(&mut self, backend: &Backend) {
        if !self.finish_inline(true) {
            return;
        }
        self.action("new-tab", backend);
        self.view = if self.is_redis() { "data" } else { "query" }.into();
        self.workbench.overlay = None;
        if self.session.is_none() && self.profile.is_some() {
            self.action("open", backend);
        }
        if self.is_redis() {
            self.workbench.cli = true;
            self.focus = "cli_text".into();
            self.workbench.focus_request = Some("input-cli_text".into());
        } else {
            self.focus = "query".into();
            self.workbench.focus_request = Some("query-editor".into());
        }
    }

    fn select_resource(&mut self, index: usize, backend: &Backend) {
        if !self.finish_inline(true) {
            return;
        }
        let Some(resource) = self.resources.get(index).cloned() else {
            return;
        };
        let target = ResourceTarget {
            database: resource.database.or_else(|| self.database.clone()),
            name: resource.name,
            key_b64: resource.key_b64,
            kind: resource.kind,
        };
        if target.kind == "database" {
            if self.target.is_some()
                || self.has_live_drafts()
                || self.task.is_some()
                || self.operation_pending
            {
                self.action("new-tab", backend);
                self.workbench.queued_target = Some(target);
                self.action("open", backend);
            } else {
                self.generation = self.generation.wrapping_add(1);
                self.database = Some(target.name);
                self.field_set("object_filter", "");
                self.dirty = true;
                self.action("list", backend);
            }
            return;
        }
        let profile_id = self.profile.as_ref().map(|p| p.id);
        if let Some(index) = self.tabs.iter().enumerate().find_map(|(index, tab)| {
            let current = if index == self.active {
                &self.target
            } else {
                // 未访问的恢复 tab 尚无 runtime，资源身份已保存在 browse 中。
                &tab.browse.target
            };
            (tab.profile_id == profile_id
                && current.as_ref().is_some_and(|current| {
                    current.database == target.database
                        && current.name == target.name
                        && current.kind == target.kind
                        && current.key_b64 == target.key_b64
                }))
            .then_some(index)
        }) {
            self.select_tab(index);
            self.view = "data".into();
            self.workbench.overlay = None;
            if self.session_id().is_none() {
                self.action("open", backend);
            }
            return;
        }
        if self.target.is_some()
            || self.task.is_some()
            || self.operation_pending
            || self.has_live_drafts()
            || !self.tabs[self.active].query.text.is_empty()
        {
            // key 有独立 tab；资源导航仍沿用原来的扫描条件与目录展开状态。
            let browser = self.is_redis().then(|| {
                (
                    self.field("pattern").to_owned(),
                    self.field("type").to_owned(),
                    self.workbench.tree,
                    self.workbench.redis_expansion.clone(),
                )
            });
            self.action("new-tab", backend);
            if let Some((pattern, kind, tree, expansion)) = browser {
                self.field_set("pattern", pattern);
                self.field_set("type", kind);
                self.workbench.tree = tree;
                self.workbench.redis_expansion = expansion;
                self.capture_browse();
            }
            self.workbench.queued_target = Some(target);
            self.action("open", backend);
        } else {
            self.load_target(target, backend);
        }
    }

    fn load_target(&mut self, target: ResourceTarget, backend: &Backend) {
        self.generation = self.generation.wrapping_add(1);
        self.scan_token = None;
        self.database = target.database.clone().or_else(|| self.database.clone());
        self.tabs[self.active].title = target.name.clone();
        self.target = Some(target);
        self.edit_token = None;
        self.schema = None;
        self.grid = None;
        self.results.clear();
        self.result_id = None;
        self.page_history.clear();
        self.page_token = None;
        self.large = None;
        self.browse_result = false;
        self.view = "data".into();
        self.workbench.section = "data".into();
        self.workbench.overlay = None;
        self.workbench.load_resource = true;
        self.workbench.focus_request = Some("grid".into());
        self.dirty = true;
        self.action("describe", backend);
    }

    fn take_focus_request(&mut self) -> Option<String> {
        let key = self.workbench.focus_request.take()?;
        if let Some(field) = key.strip_prefix("input-") {
            self.focus = field.into();
        } else if key == "query-editor" {
            self.focus = "query".into();
        } else if key == "cell-editor" {
            self.focus = "cell".into();
        } else if key == "grid" {
            self.focus = "grid".into();
            return Some(String::new());
        }
        Some(key)
    }

    fn request_quit(&mut self, backend: &Backend) -> bool {
        if !self.finish_inline(true) {
            return false;
        }
        if !self.workspace_loaded && self.dirty {
            self.status = "工作区尚未加载，无法保存".into();
            return false;
        }
        self.save_workspace(backend);
        true
    }

    // 模态层的实际节点决定字段归属，避免为条件渲染的表单维护第二份名单。
    fn modal_allows_field(&self, name: &str) -> bool {
        let Some(kind) = self
            .workbench
            .overlay
            .as_deref()
            .filter(|kind| *kind != "key-type")
        else {
            return true;
        };
        fn contains(node: &Node<Msg>, key: &str) -> bool {
            node.props().get("key").and_then(Value::as_str) == Some(key)
                || node.child_nodes().iter().any(|child| contains(child, key))
        }
        contains(&views::overlay(self, kind), &format!("input-{name}"))
    }

    fn reconcile_modal_focus(&mut self) {
        if self
            .workbench
            .field_error
            .as_ref()
            .is_some_and(|error| error.overlay != self.workbench.overlay)
        {
            self.workbench.field_error = None;
        }
        if self
            .workbench
            .overlay
            .as_deref()
            .is_none_or(|kind| kind == "key-type")
        {
            return;
        }
        if let Some(name) = self
            .workbench
            .focus_request
            .as_deref()
            .and_then(|key| key.strip_prefix("input-"))
            && self.modal_allows_field(name)
        {
            self.focus = name.to_owned();
        } else if !self.modal_allows_field(&self.focus) {
            self.focus.clear();
            self.workbench.focus_request = Some(String::new());
        }
    }

    fn dispatch_editor_message(&mut self, message: Msg) {
        if self.workbench.overlay.as_deref() == Some("profile") && self.connection_form_busy() {
            return;
        }
        let old_history_filter = match &message {
            Msg::Edit(name, ..) | Msg::Undo(name) if name == "history_filter" => {
                Some(self.field(name).to_owned())
            }
            _ => None,
        };
        match message {
            Msg::Edit(name, from, to, text, cursor, len) if self.modal_allows_field(&name) => {
                if let Some(ed) = self.editor(&name)
                    && ed.text.encode_utf16().count() == len
                    && ed.edit(from..to, &text)
                {
                    ed.set_selection(cursor, cursor);
                    if from != to || !text.is_empty() {
                        self.changed_field(&name);
                    } else {
                        self.dirty = true;
                    }
                }
            }
            Msg::Undo(name) if self.modal_allows_field(&name) => {
                if let Some(ed) = self.editor(&name)
                    && ed.undo()
                {
                    self.changed_field(&name);
                }
            }
            _ => {}
        }
        if old_history_filter
            .as_deref()
            .is_some_and(|old| self.field("history_filter") != old)
        {
            self.workbench.history_selected = None;
        }
    }

    fn input_key(&mut self, key: &tern_sdk::Key) {
        if self.workbench.overlay.as_deref() == Some("profile") && self.connection_form_busy() {
            return;
        }
        if !self.modal_allows_field(&self.focus) {
            return;
        }
        let old_history_filter =
            (self.focus == "history_filter").then(|| self.field("history_filter").to_owned());
        let name = self.focus.clone();
        if let Some(editor) = self.editor(&name) {
            let previous_len = editor.text.len();
            let edit = match key.name.as_str() {
                "backspace" => Some(EditKey::Backspace),
                "delete" => Some(EditKey::Delete),
                "left" => Some(EditKey::Left),
                "right" => Some(EditKey::Right),
                "up" => Some(EditKey::Up),
                "down" => Some(EditKey::Down),
                "home" => Some(EditKey::Home),
                "end" => Some(EditKey::End),
                "enter" => Some(EditKey::Text("\n")),
                "paste" => key.text.as_deref().map(EditKey::Text),
                "a" if key.ctrl || key.meta => Some(EditKey::SelectAll),
                "z" if key.ctrl || key.meta => Some(EditKey::Undo),
                _ => key.typed().map(EditKey::Text),
            };
            if let Some(edit) = edit {
                // 处理了按键不代表文本被修改；光标、选区与空删除不能改变 NULL／默认模式。
                let text_edit = matches!(&edit, EditKey::Text(text) if !text.is_empty() || editor.cursor != editor.anchor)
                    || matches!(&edit, EditKey::Undo);
                if editor.key(edit, key.shift) {
                    if text_edit || editor.text.len() != previous_len {
                        self.changed_field(&name);
                    } else {
                        self.dirty = true;
                    }
                }
            }
        }
        if old_history_filter
            .as_deref()
            .is_some_and(|old| self.field("history_filter") != old)
        {
            self.workbench.history_selected = None;
        }
    }
}

// SDK 按区域、祖先 key 与子节点序号派生 id；焦点与活动标签 reveal 只沿当前 View 找路径，
// 不复制整棵树，也不把业务字段名误当作终端中的节点 id。
fn focus_node_id(view: &View<Msg>, key: &str) -> Option<String> {
    fn find(node: &Node<Msg>, key: &str, path: &mut String) -> bool {
        use std::fmt::Write as _;
        if node.props().get("key").and_then(Value::as_str) == Some(key) {
            return true;
        }
        for (index, child) in node.child_nodes().iter().enumerate() {
            let length = path.len();
            match child.props().get("key") {
                Some(Value::String(child_key)) => {
                    path.push('.');
                    path.push_str(child_key);
                }
                Some(Value::Number(child_key)) => {
                    let _ = write!(path, ".{child_key}");
                }
                _ => {
                    let _ = write!(path, ".{index}");
                }
            }
            if find(child, key, path) {
                return true;
            }
            path.truncate(length);
        }
        false
    }
    for (region, node) in ["main", "dock", "layer"].into_iter().zip(view.regions()) {
        if let Some(node) = node {
            let mut path = region.to_owned();
            if find(node, key, &mut path) {
                return Some(path);
            }
        }
    }
    None
}

fn tsp_error(error: tern_sdk::Error) -> AppError {
    AppError::new("tsp", error.to_string())
}

// 输入已关闭时无法再向用户询问：等待保存确认，原工作区失败则尝试另存副本。
// 如果保存或副本仍无法确认，返回错误而不是将退出视为成功。
fn finish_after_input_closed(app: &mut App, backend: &mut Backend) -> Result<()> {
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
