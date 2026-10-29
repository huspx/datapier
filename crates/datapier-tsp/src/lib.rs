//! TSP 界面及编辑状态，通过 Engine IPC 操作数据；Session 由运行入口所在 UI 线程独占。

mod backend;
#[cfg(test)]
mod close_regression;
mod editor;
mod grid;
#[cfg(test)]
mod regression;
mod views;
mod workbench;

use backend::{Backend, Response};
use datapier_core::{
    Result,
    model::{AppError, Profile, Resource, ResourcePage, ResourceTarget, ResultPage},
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
use tern_sdk::{
    Input, Options, Session, SurfaceOptions,
    ui::{self, Node, View, html},
};
use uuid::Uuid;
use workbench::Workbench;

#[derive(Clone)]
enum Msg {
    Action(String),
    Focus(String),
    Edit(String, usize, usize, String, usize, usize),
    Undo(String),
    Profile(usize),
    Resource(usize),
    Tab(usize),
    ResultSet(String),
    Cell(u64, usize, bool),
    CellOpen(u64, usize),
    Row(u64, usize),
    Complete(String, usize, usize),
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
    large: Option<LargeLoad>,
    preview: Option<Value>,
    preview_pending: Option<(Uuid, Value)>,
    paste_preview: Option<grid::PastePreview>,
    browse_result: bool,
    generation: u64,
    workbench: Workbench,
    view: String,
}
#[derive(Serialize, Deserialize)]
struct Workspace {
    tabs: Vec<Tab>,
    active: usize,
    pending: Value,
}

struct App {
    workspace_id: String,
    revision: u64,
    workspace_loaded: bool,
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
    form_id: Option<Uuid>,
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
    large: Option<LargeLoad>,
    preview: Option<Value>,
    preview_pending: Option<(Uuid, Value)>,
    browse_result: bool,
    generation: u64,
    paste_preview: Option<grid::PastePreview>,
    status: String,
    detail: String,
    view: String,
    workbench: Workbench,
    last_poll: Instant,
}
impl App {
    fn new(workspace_id: String) -> Self {
        let mut fields = BTreeMap::new();
        for (name, value) in [
            ("name", ""),
            ("host", "127.0.0.1"),
            ("port", "3306"),
            ("user", ""),
            ("password", ""),
            ("database", ""),
            ("redis_db", "0"),
            ("provider", "mysql"),
            ("access", "read_only"),
            ("tls", "false"),
            ("ca", ""),
            ("cert", ""),
            ("key", ""),
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
            ("row_values", "{}"),
            ("cell", ""),
            ("column", "0"),
            ("width", "160"),
            ("object_filter", ""),
            ("history_filter", ""),
            ("row_search", ""),
        ] {
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
            form_id: None,
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
            large: None,
            preview: None,
            preview_pending: None,
            browse_result: false,
            generation: 0,
            paste_preview: None,
            status: "正在连接 Engine…".into(),
            detail: String::new(),
            view: "query".into(),
            workbench: Workbench::default(),
            last_poll: Instant::now(),
        }
    }
    fn field(&self, name: &str) -> &str {
        if name == "cli_text" {
            &self.tabs[self.active].cli_query.text
        } else {
            self.fields.get(name).map(|x| x.text.as_str()).unwrap_or("")
        }
    }
    fn editor(&mut self, name: &str) -> Option<&mut Editor> {
        if name == "query" {
            self.tabs.get_mut(self.active).map(|x| &mut x.query)
        } else if name == "cli_text" {
            self.tabs.get_mut(self.active).map(|tab| &mut tab.cli_query)
        } else {
            self.fields.get_mut(name)
        }
    }
    fn field_set(&mut self, name: &str, text: impl Into<String>) {
        let editor = Editor::new(text.into());
        if name == "cli_text" {
            self.tabs[self.active].cli_query = editor;
        } else {
            self.fields.insert(name.into(), editor);
        }
    }
    fn has_live_drafts(&self) -> bool {
        self.grid.as_ref().is_some_and(|g| !g.edits().is_empty())
            || !self.pending_rows.is_empty()
            || !self.redis_changes.is_empty()
            || self.paste_preview.is_some()
    }
    fn changed_field(&mut self, name: &str) {
        self.dirty = true;
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
        };
        self.tabs[self.active].browse = browse;
    }
    fn restore_browse(&mut self) {
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
    fn switch_tab(&mut self, next: usize) {
        if next == self.active || next >= self.tabs.len() {
            return;
        }
        self.capture_browse();
        let dark = self.workbench.dark;
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
        swap_state!(large);
        swap_state!(preview);
        swap_state!(preview_pending);
        swap_state!(paste_preview);
        swap_state!(browse_result);
        swap_state!(generation);
        swap_state!(workbench);
        swap_state!(view);
        self.workbench.dark = dark;
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
        if !self.tabs[next].pending.is_null() && self.edit_token.is_none() {
            self.detail = format!(
                "恢复的编辑意图（基线已失效，请刷新并重新确认）：{}",
                self.tabs[next].pending
            );
        }
    }
    // 异步响应先按 tab 和 generation 路由，任务、分页等再核对各自的 ID，
    // 避免迟到响应覆盖切换后的上下文；处理后台 tab 后恢复原活动 tab。
    fn routed_response(&mut self, mut response: Response, backend: &Backend) {
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
            let old = self.active;
            self.switch_tab(index);
            if generation == self.generation {
                if let Some(task) = tag.strip_prefix("execution.get/") {
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
                } else if let Some(id) = tag.strip_prefix("profile.save/") {
                    if Uuid::parse_str(id).ok() == self.form_id {
                        response.tag = "profile.save".into();
                        self.response(response, backend);
                    } else {
                        self.request(backend, "profile.list", "profile.list", json!({}));
                    }
                } else {
                    response.tag = tag;
                    self.response(response, backend);
                }
            }
            self.switch_tab(old);
        } else {
            self.response(response, backend);
        }
    }
    fn request(&mut self, backend: &Backend, tag: &str, method: &str, params: Value) {
        let global = matches!(
            tag,
            "profile.list"
                | "profile.delete"
                | "profile.test"
                | "workspace.load"
                | "workspace.save"
        );
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
        let tag = if global {
            tag.to_owned()
        } else {
            format!("{}:{}:{tag}", self.tabs[self.active].id, self.generation)
        };
        match backend.request(tag, method, params) {
            Ok(()) => {
                if long {
                    self.operation_pending = true
                }
            }
            Err(e) => self.status = e.to_string(),
        }
    }
    fn open_cell(&mut self, backend: &Backend) {
        let Some(g) = self.grid.as_ref() else {
            self.status = "没有结果".into();
            return;
        };
        let Some(cell) = g.focus else {
            self.status = "没有选中单元格".into();
            return;
        };
        if let Some(datapier_core::model::Value::Large { id, .. }) = g.value(cell) {
            let id = *id;
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
                Err(e) => self.status = e.to_string(),
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
                self.status = error.to_string();
                return;
            }
        };
        let Some(g) = self.grid.as_mut() else {
            self.status = "结果已关闭".into();
            return;
        };
        if let Err(e) = g.resolve_large(load.cell, full) {
            self.status = e.to_string();
            return;
        }
        if let Some(original) = self.workbench.redis_loading.take() {
            g.focus = Some(original);
            self.workbench_redis_edit(backend);
            return;
        }
        if g.focus != Some(load.cell) {
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
            Err(e) => self.status = e.to_string(),
        }
    }
    fn session_id(&self) -> Option<Value> {
        self.session
            .as_ref()
            .and_then(|s| s.get("session_id"))
            .cloned()
    }
    fn redis_operation(&self) -> std::result::Result<Value, String> {
        use base64::Engine as _;
        let value = |name: &str| -> std::result::Result<Value, String> {
            let s = self.field(name);
            if let Some(data) = s.strip_prefix("base64:") {
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| e.to_string())?;
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
                json!({"operation":op,"index":self.field("redis_index").parse::<i64>().map_err(|e|e.to_string())?,"value":value("redis_value")?})
            }
            "list_delete" => {
                json!({"operation":op,"index":self.field("redis_index").parse::<i64>().map_err(|e|e.to_string())?})
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
                        .map_err(|e| e.to_string())?;
                    encoded.to_string()
                } else {
                    base64::engine::general_purpose::STANDARD.encode(name.as_bytes())
                };
                json!({"operation":op,"new_key_b64":encoded})
            }
            "delete_key" => json!({"operation":op}),
            "set_ttl" => {
                let ttl = if self.field("redis_ttl").is_empty() {
                    None
                } else {
                    Some(
                        self.field("redis_ttl")
                            .parse::<u64>()
                            .map_err(|e| e.to_string())?,
                    )
                };
                json!({"operation":op,"ttl_ms":ttl})
            }
            _ => return Err("未知 Redis 操作".into()),
        })
    }
    // 持久化编辑意图和布局，而非可继续提交的数据库基线；恢复后必须刷新并重新确认。
    fn workspace_snapshot(&mut self) -> Result<Workspace> {
        self.capture_browse();
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
            if grid.as_ref().is_some_and(|g| !g.edits().is_empty())
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
                self.status = e.to_string();
                self.save_blocked = true;
                return;
            }
        };
        match backend.request("workspace.save", "workspace.save", json!({"workspace_id":self.workspace_id,"expected_revision":self.revision,"state":state})) {Ok(())=>{self.saving=true;self.dirty=false;},Err(e)=>{self.status=e.to_string();self.save_blocked=true;}}
    }
    fn save_workspace_copy(&mut self, backend: &Backend) {
        if self.saving || self.copy_pending.is_some() {
            self.status = "工作区保存仍在进行".into();
            return;
        }
        let state = match self.workspace_snapshot() {
            Ok(state) => state,
            Err(e) => {
                self.status = e.to_string();
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
            Err(e) => self.status = e.to_string(),
        }
    }
    fn profile_form(&mut self, p: Option<Profile>) {
        self.form_id = Some(
            p.as_ref()
                .map(|profile| profile.id)
                .unwrap_or_else(Uuid::new_v4),
        );
        if let Some(p) = p {
            self.field_set("name", p.name);
            self.field_set("host", p.host);
            self.field_set("port", p.port.to_string());
            self.field_set("user", p.username);
            self.field_set("database", p.database.unwrap_or_default());
            self.field_set("redis_db", p.redis_db.to_string());
            self.field_set(
                "provider",
                match p.provider {
                    datapier_core::model::ProviderKind::Mysql => "mysql",
                    _ => "redis",
                },
            );
            self.field_set(
                "access",
                match p.access_mode {
                    datapier_core::model::AccessMode::ReadOnly => "read_only",
                    _ => "read_write",
                },
            );
            self.field_set("tls", p.tls.enabled.to_string());
            self.field_set("ca", p.tls.ca_path.unwrap_or_default());
            self.field_set("cert", p.tls.client_cert_path.unwrap_or_default());
            self.field_set("key", p.tls.client_key_path.unwrap_or_default());
        }
        self.field_set("password", "");
        self.workbench.overlay = Some("profile".into());
        self.workbench.focus_request = Some("input-name".into());
    }
    fn form_profile(&self) -> std::result::Result<Profile, String> {
        let optional = |s: &str| {
            if s.is_empty() {
                None
            } else {
                Some(s.to_owned())
            }
        };
        serde_json::from_value(json!({"id":self.form_id.ok_or("请先新建或编辑连接")?,"name":self.field("name"),"host":self.field("host"),"port":self.field("port").parse::<u16>().map_err(|e|e.to_string())?,"username":self.field("user"),"database":optional(self.field("database")),"redis_db":self.field("redis_db").parse::<u32>().map_err(|e|e.to_string())?,"provider":self.field("provider"),"access_mode":self.field("access"),"tls":{"enabled":self.field("tls").parse::<bool>().map_err(|e|e.to_string())?,"ca_path":optional(self.field("ca")),"client_cert_path":optional(self.field("cert")),"client_key_path":optional(self.field("key"))}})).map_err(|e|e.to_string())
    }
    /// 连接与资源各自保留 tab 运行态，切换对象不覆盖另一对象的草稿和结果。
    fn select_profile(&mut self, index: usize, backend: &Backend) {
        let Some(profile) = self.profiles.get(index).cloned() else {
            return;
        };
        if let Some(index) = self.tabs.iter().enumerate().find_map(|(index, tab)| {
            let session = if index == self.active {
                &self.session
            } else {
                &tab.runtime.session
            };
            (tab.profile_id == Some(profile.id) && session.is_some()).then_some(index)
        }) {
            self.switch_tab(index);
            return;
        }
        if self.session.is_some()
            || self.has_live_drafts()
            || self.operation_pending
            || self.task.is_some()
        {
            self.action("new-tab", backend);
        }
        self.profile = Some(profile.clone());
        self.tabs[self.active].profile_id = Some(profile.id);
        self.field_set("password", "");
        self.workbench.overlay = None;
        self.dirty = true;
        self.action("open", backend);
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
                &tab.runtime.target
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
            self.switch_tab(index);
            self.view = "data".into();
            self.workbench.overlay = None;
            return;
        }
        if self.target.is_some()
            || self.task.is_some()
            || self.operation_pending
            || self.has_live_drafts()
            || !self.tabs[self.active].query.text.is_empty()
        {
            self.action("new-tab", backend);
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
        self.dirty = true;
        self.action("describe", backend);
    }

    fn action(&mut self, name: &str, backend: &Backend) {
        if name == "discard" {
            self.finish_inline(false);
        } else if matches!(
            name,
            "history"
                | "new-tab"
                | "close-tab"
                | "new-profile"
                | "edit-profile"
                | "run"
                | "selection"
                | "current"
                | "submit"
                | "redis-submit"
                | "open-cell"
                | "open"
                | "next"
                | "previous"
        ) && !self.finish_inline(true)
        {
            return;
        }
        if self.workbench_action(name, backend) {
            return;
        }
        let session = self.session_id();
        if matches!(
            name,
            "set-cell"
                | "set-null"
                | "set-json"
                | "insert-row"
                | "delete-row"
                | "redis-stage"
                | "paste-confirm"
        ) && !self.tabs[self.active].pending.is_null()
            && !self.tabs[self.active].pending_live
        {
            self.status = "请先恢复或丢弃已保存的编辑意图".into();
            return;
        }
        if name == "open-cell" {
            self.open_cell(backend);
            return;
        }
        match name {
            "workspace-copy" => self.save_workspace_copy(backend),
            "databases" => {
                if self.target.is_some() || self.has_live_drafts() || self.task.is_some() {
                    self.action("new-tab", backend);
                    self.workbench.queued_target = Some(ResourceTarget {
                        database: None,
                        name: String::new(),
                        key_b64: None,
                        kind: "database".into(),
                    });
                    self.action("open", backend);
                } else {
                    self.database = None;
                    self.field_set("object_filter", "");
                    self.action("list", backend);
                }
            }
            "new-profile" => self.profile_form(None),
            "edit-profile" => self.profile_form(self.profile.clone()),
            "save-profile" | "test-profile" => match self.form_profile() {
                Ok(p) => {
                    let secret = if self.field("password").is_empty() {
                        Value::Null
                    } else {
                        json!(self.field("password"))
                    };
                    let method = if name == "save-profile" {
                        "profile.save"
                    } else {
                        "profile.test"
                    };
                    let tag = if name == "save-profile" {
                        format!("profile.save/{}", p.id)
                    } else {
                        method.into()
                    };
                    self.request(
                        backend,
                        &tag,
                        method,
                        json!({"profile":p,"secret":secret,"save_secret":true}),
                    );
                }
                Err(e) => self.status = e,
            },
            "open" => {
                if let Some(p) = &self.profile {
                    let id = p.id;
                    if self.task.is_some() || self.operation_pending {
                        self.status = "请等待或取消当前任务".into();
                    } else if self.has_live_drafts() {
                        self.status = "请先提交或丢弃待提交变更".into();
                    } else {
                        if let Some(old) = self.session_id() {
                            self.request(
                                backend,
                                "connection.close",
                                "connection.close",
                                json!({"session_id":old}),
                            );
                            self.session = None;
                        }
                        if self.workbench.queued_target.is_none() {
                            self.workbench.queued_target = self.target.take();
                        }
                        self.edit_token = None;
                        self.schema = None;
                        self.grid = None;
                        self.results.clear();
                        self.result_id = None;
                        self.page_token = None;
                        self.page_history.clear();
                        self.large = None;
                        self.browse_result = false;
                        self.generation = self.generation.wrapping_add(1);
                        self.request(backend, "connection.open", "connection.open", json!({"profile_id":id,"secret":if self.field("password").is_empty(){None}else{Some(self.field("password"))}}));
                    }
                }
            }
            "delete-profile" => {
                if let Some(p) = &self.profile {
                    self.request(
                        backend,
                        "profile.delete",
                        "profile.delete",
                        json!({"profile_id":p.id}),
                    );
                }
            }
            "list" | "more" => {
                if let Some(s) = session {
                    if name == "more" && self.scan_token.is_some() {
                        self.status = "正在读取资源，请等待".into();
                    } else if name == "more" && self.cursor.is_none() {
                        self.status = "扫描已完成".into();
                    } else {
                        let token = Uuid::new_v4();
                        self.scan_token = Some(token);
                        if name == "list" {
                            self.cursor = None;
                            self.resources.clear();
                            self.workbench.scan_dirty = false;
                            self.workbench.key_page = 0;
                        }
                        self.request(backend,&format!("resource.list/{token}"),"resource.list",json!({"session_id":s,"database":self.database,"cursor":if name == "more" {self.cursor.clone()} else {None},"pattern":self.field("pattern"),"type":if self.field("type").is_empty(){None}else{Some(self.field("type"))},"limit":200}));
                    }
                }
            }
            "browse" | "describe" | "edit-open" => {
                if let (Some(s), Some(target)) = (session, self.target.clone()) {
                    let method = match name {
                        "browse" => "resource.browse",
                        "describe" => "resource.describe",
                        _ => "change.open",
                    };
                    let filters: Value = match serde_json::from_str(self.field("filter")) {
                        Ok(v) => v,
                        Err(e) => {
                            self.status = format!("过滤格式错误: {e}");
                            return;
                        }
                    };
                    let sort: Value = match serde_json::from_str(self.field("sort")) {
                        Ok(v) => v,
                        Err(e) => {
                            self.status = format!("排序格式错误: {e}");
                            return;
                        }
                    };
                    self.request(backend, method, method, json!({"session_id":s,"target":target,"filters":filters,"sort":sort,"workspace_id":self.workspace_id}));
                }
            }
            "run" | "selection" => {
                if let Some(s) = session {
                    let query = &self.tabs[self.active].query;
                    let text = if name == "selection" {
                        query.selected_text().unwrap_or("").to_owned()
                    } else {
                        query.text.clone()
                    };
                    if text.is_empty() {
                        self.status = "没有可执行的文本".into();
                    } else {
                        self.request(
                            backend,
                            "execution.start",
                            "execution.start",
                            json!({"session_id":s,"workspace_id":self.workspace_id,"text":text}),
                        );
                    }
                }
            }
            "current" => {
                if let Some(s) = session {
                    if self
                        .profile
                        .as_ref()
                        .is_some_and(|p| p.provider == datapier_core::model::ProviderKind::Mysql)
                    {
                        self.request(
                            backend,
                            "current.info",
                            "connection.info",
                            json!({"session_id":s}),
                        );
                    } else {
                        self.status = "当前语句定位仅支持 MySQL".into();
                    }
                }
            }
            "cancel" => {
                if let Some(task) = self.task.clone() {
                    self.request(
                        backend,
                        "execution.cancel",
                        "execution.cancel",
                        json!({"task_id":task}),
                    );
                }
            }
            "next" => {
                if self.paste_preview.is_some() {
                    self.status = "请先确认或丢弃粘贴预览".into();
                } else if let Some(g) = &self.grid
                    && let Some(next) = g.page.next
                {
                    let start = g.page.start;
                    self.page_history.push(start);
                    self.page(backend, next);
                }
            }
            "previous" => {
                if self.paste_preview.is_some() {
                    self.status = "请先确认或丢弃粘贴预览".into();
                } else if let Some(start) = self.page_history.pop() {
                    self.page(backend, start);
                }
            }
            "new-tab" => {
                self.tabs.push(Tab {
                    title: format!("查询 {}", self.tabs.len() + 1),
                    query: Editor::new(""),
                    cli_query: Editor::default(),
                    profile_id: self.profile.as_ref().map(|p| p.id),
                    id: Uuid::new_v4(),
                    layout: Vec::new(),
                    browse: BrowseSettings::default(),
                    pending: Value::Null,
                    pending_live: false,
                    runtime: TabRuntime::default(),
                });
                self.switch_tab(self.tabs.len() - 1);
                self.dirty = true;
            }
            "close-tab" => {
                if self.tabs.len() > 1 {
                    if self.has_live_drafts() || !self.tabs[self.active].pending.is_null() {
                        self.status = "请先提交或丢弃该 tab 的待提交变更".into();
                    } else {
                        if let Some(s) = self.session_id() {
                            self.request(
                                backend,
                                "connection.close",
                                "connection.close",
                                json!({"session_id":s}),
                            );
                        }
                        let index = self.active;
                        let next = if index == 0 { 1 } else { index - 1 };
                        self.switch_tab(next);
                        self.tabs.remove(index);
                        if index < self.active {
                            self.active -= 1;
                        }
                        self.dirty = true;
                    }
                }
            }
            "history" => {
                self.view = "history".into();
                self.request(
                    backend,
                    "history.list",
                    "history.list",
                    json!({"workspace_id":self.workspace_id}),
                );
            }
            "file-open" | "file-save" => {
                let method = if name == "file-open" {
                    "file.open"
                } else {
                    "file.save"
                };
                self.request(
                    backend,
                    method,
                    method,
                    json!({"path":self.field("path"),"text":self.tabs[self.active].query.text}),
                );
            }
            "preview-import" | "import" | "export" => {
                if let Some(s) = session {
                    let mapping: Value = match serde_json::from_str(self.field("mapping")) {
                        Ok(v) => v,
                        Err(e) => {
                            self.status = format!("列映射格式错误: {e}");
                            return;
                        }
                    };
                    let binary: Value = match serde_json::from_str(self.field("binary_columns")) {
                        Ok(v) => v,
                        Err(e) => {
                            self.status = format!("二进制列格式错误: {e}");
                            return;
                        }
                    };
                    let escaped = match self.field("backslash_escape").parse::<bool>() {
                        Ok(v) => v,
                        Err(e) => {
                            self.status = e.to_string();
                            return;
                        }
                    };
                    let mut params = json!({"session_id":s,"workspace_id":self.workspace_id,"path":self.field("path"),"format":self.field("format"),"column_mapping":mapping,"null_marker":self.field("null_marker"),"backslash_escape":escaped,"binary_columns":binary});
                    if let Some(target) = self.target.clone() {
                        params["target"] = json!(target);
                    } else if name != "export"
                        && self.profile.as_ref().is_some_and(|p| {
                            p.provider == datapier_core::model::ProviderKind::Redis
                        })
                    {
                        params["target"] = json!({"name":"","database":self.profile.as_ref().map(|p|p.redis_db.to_string()),"kind":"database"});
                    }
                    if name == "export" {
                        if let Some(id) = self.result_id.clone() {
                            params["result_id"] = json!(id);
                        }
                        self.request(backend, "transfer.export", "transfer.export", params);
                    } else if name == "preview-import" {
                        let id = Uuid::new_v4();
                        self.preview = None;
                        self.preview_pending = Some((id, params.clone()));
                        self.request(
                            backend,
                            &format!("transfer.preview/{id}"),
                            "transfer.preview",
                            params,
                        );
                    } else if self.preview.as_ref() == Some(&params) {
                        self.preview = None;
                        self.request(backend, "transfer.import", "transfer.import", params);
                    } else {
                        self.status = "请先预览并确认当前导入设置".into();
                    }
                }
            }
            "undo-grid" => {
                if let Some(g) = &mut self.grid {
                    g.undo();
                    self.dirty = true;
                }
            }
            "insert-row" => {
                if self.edit_token.is_none() {
                    self.status = "请先打开编辑基线".into();
                } else {
                    match serde_json::from_str::<BTreeMap<String, datapier_core::model::Value>>(
                        self.field("row_values"),
                    ) {
                        Ok(values) => {
                            self.pending_rows.push(datapier_core::model::RowChange {
                                operation: "insert".into(),
                                key: BTreeMap::new(),
                                before: BTreeMap::new(),
                                values,
                            });
                            self.detail = serde_json::to_string_pretty(&self.pending_rows)
                                .unwrap_or_default();
                            self.dirty = true;
                        }
                        Err(e) => self.status = format!("新增行值格式错误: {e}"),
                    }
                }
            }
            "restore-pending" => {
                let pending = self.tabs[self.active].pending.clone();
                if pending.is_null() {
                    self.status = "没有已恢复的编辑意图".into();
                    return;
                }
                if self.edit_token.is_none() || pending.get("target") != Some(&json!(self.target)) {
                    self.status = "请先选择原目标并重新打开编辑基线".into();
                    return;
                }
                let redis = pending
                    .get("redis")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if redis.iter().any(|change| {
                    matches!(
                        change.get("operation").and_then(Value::as_str),
                        Some("list_set" | "list_insert" | "list_delete")
                    )
                }) {
                    self.detail = serde_json::to_string_pretty(&pending).unwrap_or_default();
                    self.status =
                        "List 索引基线已失效；请根据刷新后的列表重新录入 List 操作".into();
                    return;
                }
                let mut rows: Vec<datapier_core::model::RowChange> = match serde_json::from_value(
                    pending.get("rows").cloned().unwrap_or_else(|| json!([])),
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        self.status = format!("恢复行草稿失败: {e}");
                        return;
                    }
                };
                if !pending
                    .get("rows_include_edits")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    let edits = pending
                        .get("edits")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if !edits.is_empty() {
                        let schema: datapier_core::model::TableSchema = match self
                            .schema
                            .clone()
                            .and_then(|v| serde_json::from_value(v).ok())
                        {
                            Some(v) => v,
                            None => {
                                self.status = "请重新打开表编辑基线".into();
                                return;
                            }
                        };
                        let Some(keys) = schema.keys.first().filter(|keys| !keys.is_empty()) else {
                            self.status = "表没有可靠行标识".into();
                            return;
                        };
                        let mut updates: BTreeMap<u64, datapier_core::model::RowChange> =
                            BTreeMap::new();
                        for item in edits {
                            let Some(row) = item["row"].as_u64() else {
                                self.status = "恢复数据缺少行号".into();
                                return;
                            };
                            let Some(column) = item["column"]
                                .as_u64()
                                .and_then(|x| usize::try_from(x).ok())
                                .and_then(|i| schema.columns.get(i))
                            else {
                                self.status = "恢复数据缺少列".into();
                                return;
                            };
                            let before: Vec<datapier_core::model::Value> =
                                match serde_json::from_value(item["before_row"].clone()) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        self.status = format!("恢复基线失败: {e}");
                                        return;
                                    }
                                };
                            if before.len() != schema.columns.len() {
                                self.status = "恢复基线列数不匹配".into();
                                return;
                            }
                            let before: BTreeMap<_, _> = schema
                                .columns
                                .iter()
                                .zip(before)
                                .map(|(c, v)| (c.name.clone(), v))
                                .collect();
                            let key: BTreeMap<_, _> = keys
                                .iter()
                                .filter_map(|k| before.get(k).cloned().map(|v| (k.clone(), v)))
                                .collect();
                            if key.len() != keys.len()
                                || key.values().any(|v| {
                                    matches!(
                                        v,
                                        datapier_core::model::Value::Null
                                            | datapier_core::model::Value::Missing
                                    )
                                })
                            {
                                self.status = "恢复基线没有可靠行标识".into();
                                return;
                            }
                            let after = match serde_json::from_value(item["after"].clone()) {
                                Ok(v) => v,
                                Err(e) => {
                                    self.status = format!("恢复值无效: {e}");
                                    return;
                                }
                            };
                            let entry = updates.entry(row).or_insert_with(|| {
                                datapier_core::model::RowChange {
                                    operation: "update".into(),
                                    key,
                                    before: before.clone(),
                                    values: BTreeMap::new(),
                                }
                            });
                            if entry.before != before {
                                self.status = "恢复数据包含不一致的行基线".into();
                                return;
                            }
                            entry.values.insert(column.name.clone(), after);
                        }
                        rows.extend(updates.into_values());
                    }
                }
                self.pending_rows = rows;
                self.redis_changes = redis;
                self.clear_redis_previews();
                self.tabs[self.active].pending = Value::Null;
                self.dirty = true;
                self.detail = serde_json::to_string_pretty(
                    &json!({"rows":self.pending_rows,"redis":self.redis_changes}),
                )
                .unwrap_or_default();
                self.status = "编辑意图已恢复；提交时将检查原值，请先确认预览".into();
            }
            "delete-row" => {
                if let Some(g) = &self.grid {
                    if !g.editable {
                        self.status = "当前结果不可编辑".into();
                    } else if let Some(cell) = g.focus {
                        if g.edits().keys().any(|c| c.row == cell.row) {
                            self.status = "请先撤销该行的单元格修改".into();
                        } else if let Some(row) =
                            g.page.rows.get((cell.row - g.page.start) as usize)
                        {
                            let before: BTreeMap<_, _> = g
                                .page
                                .result
                                .columns
                                .iter()
                                .zip(row)
                                .map(|(c, v)| (c.name.clone(), v.clone()))
                                .collect();
                            let keys: Vec<String> = self
                                .schema
                                .as_ref()
                                .and_then(|s| s.get("keys"))
                                .and_then(Value::as_array)
                                .and_then(|k| k.first())
                                .and_then(|k| serde_json::from_value(k.clone()).ok())
                                .unwrap_or_default();
                            let key: BTreeMap<_, _> = keys
                                .iter()
                                .filter_map(|k| before.get(k).cloned().map(|v| (k.clone(), v)))
                                .collect();
                            if keys.is_empty() || key.len() != keys.len() {
                                self.status = "没有可靠行标识".into();
                            } else {
                                self.pending_rows.push(datapier_core::model::RowChange {
                                    operation: "delete".into(),
                                    key,
                                    before,
                                    values: BTreeMap::new(),
                                });
                                self.detail = serde_json::to_string_pretty(&self.pending_rows)
                                    .unwrap_or_default();
                                self.dirty = true;
                            }
                        }
                    }
                }
            }
            "undo-row" => {
                self.pending_rows.pop();
                self.detail = serde_json::to_string_pretty(&self.pending_rows).unwrap_or_default();
                self.dirty = true;
            }
            "paste-confirm" => {
                if let (Some(g), Some(preview)) = (&mut self.grid, self.paste_preview.take()) {
                    match g.apply_paste(preview) {
                        Ok(()) => {
                            self.dirty = true;
                            self.status = "粘贴已加入待提交变更".into()
                        }
                        Err(e) => self.status = e.to_string(),
                    }
                }
            }
            "column-width" | "column-hide" | "column-freeze" => {
                let column = self.field("column").parse::<usize>();
                let width = self.field("width").parse::<u16>();
                if let (Some(g), Ok(column)) = (&mut self.grid, column) {
                    if let Some(mut setting) = g.settings.get(column).cloned() {
                        match name {
                            "column-width" => {
                                if let Ok(width) = width {
                                    setting.width = width
                                }
                            }
                            "column-hide" => setting.hidden = !setting.hidden,
                            "column-freeze" => setting.frozen = !setting.frozen,
                            _ => {}
                        }
                        if let Err(e) = g.set_column(column, setting) {
                            self.status = e.to_string();
                        } else {
                            self.dirty = true;
                        }
                    } else {
                        self.status = "列不存在".into();
                    }
                }
            }
            "discard" => {
                if let Some(g) = &mut self.grid {
                    g.discard();
                }
                self.pending_rows.clear();
                self.redis_changes.clear();
                self.paste_preview = None;
                self.tabs[self.active].pending = Value::Null;
                self.tabs[self.active].pending_live = false;
                if let (Some(s), Some(token)) = (session, self.edit_token.take()) {
                    self.request(
                        backend,
                        "change.discard",
                        "change.discard",
                        json!({"session_id":s,"token":token}),
                    );
                }
                self.dirty = true;
            }
            "submit" => {
                if let (Some(s), Some(target), Some(token)) =
                    (session, self.target.clone(), self.edit_token.clone())
                {
                    let keys: Vec<String> = self
                        .schema
                        .as_ref()
                        .and_then(|x| x.get("keys"))
                        .and_then(|x| x.as_array())
                        .and_then(|x| x.first())
                        .and_then(|x| serde_json::from_value(x.clone()).ok())
                        .unwrap_or_default();
                    let mut rows = self.pending_rows.clone();
                    if let Some(g) = self.grid.as_ref() {
                        match g.changes(&keys) {
                            Ok(changes) => rows.extend(changes),
                            Err(e) => {
                                self.status = e.to_string();
                                return;
                            }
                        }
                    }
                    if rows.is_empty() {
                        self.status = "没有待提交的行变更".into();
                    } else {
                        self.request(backend,"change.submit","change.submit",json!({"session_id":s,"workspace_id":self.workspace_id,"target":target,"token":token,"rows":rows}));
                    }
                }
            }
            "redis-undo" => {
                self.redis_changes.pop();
                self.detail = serde_json::to_string_pretty(&self.redis_changes).unwrap_or_default();
                self.dirty = true;
            }
            "redis-submit" => {
                if let (Some(s), Some(target), Some(token)) =
                    (session, self.target.clone(), self.edit_token.clone())
                {
                    if self.redis_changes.is_empty() {
                        self.status = "没有待提交的 Redis 操作".into();
                    } else {
                        self.request(backend,"change.submit","change.submit",json!({"session_id":s,"workspace_id":self.workspace_id,"target":target,"token":token,"changes":self.redis_changes}));
                    }
                }
            }
            "set-cell" | "set-null" | "set-json" => {
                if let Some(g) = &mut self.grid
                    && let Some(cell) = g.focus
                {
                    let text = self
                        .fields
                        .get("cell")
                        .map(|e| e.text.clone())
                        .unwrap_or_default();
                    let val = match name {
                        "set-null" => datapier_core::model::Value::Null,
                        "set-json" => match serde_json::from_str(&text) {
                            Ok(v) => datapier_core::model::Value::Json(v),
                            Err(e) => {
                                self.status = e.to_string();
                                return;
                            }
                        },
                        _ => match g.parse_cell_text(cell, &text) {
                            Ok(v) => v,
                            Err(e) => {
                                self.status = e.to_string();
                                return;
                            }
                        },
                    };
                    if let Err(e) = g.edit(cell, val) {
                        self.status = e.to_string()
                    } else {
                        self.dirty = true;
                    }
                }
            }
            _ => {}
        }
    }
    fn page(&mut self, backend: &Backend, start: u64) {
        if let Some(id) = self.result_id.clone() {
            let token = Uuid::new_v4();
            self.page_token = Some(token);
            self.request(
                backend,
                &format!("result.page/{token}"),
                "result.page",
                json!({"result_id":id,"start":start,"max_rows":200,"max_bytes":1048576}),
            );
        }
    }
    fn response(&mut self, response: Response, backend: &Backend) {
        let value = match response.result {
            Ok(v) => v,
            Err(e) => {
                if self.workbench_error(&response.tag, &e) {
                    return;
                }
                self.status = e.to_string();
                if response.tag == "workspace.save" {
                    self.saving = false;
                    self.dirty = true;
                    self.save_blocked = true;
                }
                if response.tag == "workspace.copy" {
                    self.copy_pending = None;
                }
                if response.tag == "transfer.preview" {
                    self.preview_pending = None;
                    self.preview = None;
                }
                if response.tag.starts_with("large.value/") {
                    self.large = None;
                    self.workbench.inline = None;
                    self.workbench.redis_loading = None;
                }
                if response.tag == "resource.describe" {
                    self.workbench.load_resource = false;
                }
                if matches!(
                    response.tag.as_str(),
                    "connection.open"
                        | "execution.start"
                        | "resource.browse"
                        | "change.submit"
                        | "transfer.import"
                        | "transfer.export"
                ) {
                    self.operation_pending = false;
                }
                return;
            }
        };
        if response.tag.starts_with("cli.")
            || (self.workbench.cli_running && response.tag == "execution.get")
        {
            self.workbench_response(&response.tag, value, backend);
            return;
        }
        if response.tag.starts_with("large.value/") {
            self.large_response(&response.tag, value, backend);
            return;
        }
        match response.tag.as_str() {
            // 事件仅提示查询任务状态；事件丢失或早于任务 ID 返回时由周期轮询补足。
            "execution.event" => {
                if value.get("kind").and_then(Value::as_str) == Some("execution.state")
                    && let Some(task) = value.get("task_id").and_then(Value::as_str)
                    && self.task.as_deref() == Some(task)
                {
                    self.request(
                        backend,
                        &format!("execution.get/{task}"),
                        "execution.get",
                        json!({"task_id": task}),
                    );
                }
            }
            "connect" => {
                self.status = "已连接 Engine".into();
                self.request(backend, "profile.list", "profile.list", json!({}));
                self.request(
                    backend,
                    "workspace.load",
                    "workspace.load",
                    json!({"workspace_id":self.workspace_id}),
                );
            }
            "profile.list" => {
                self.profiles =
                    serde_json::from_value(value.get("profiles").cloned().unwrap_or_default())
                        .unwrap_or_default();
                self.migrate_cli_queries();
                if self.profile.is_none() {
                    self.profile = self.tabs[self.active]
                        .profile_id
                        .and_then(|id| self.profiles.iter().find(|p| p.id == id).cloned());
                }
            }
            "profile.save" => {
                if let Ok(profile) = serde_json::from_value::<Profile>(value["profile"].clone()) {
                    self.form_id = Some(profile.id);
                    self.field_set("password", "");
                    if self.session.is_none() {
                        self.profile = Some(profile.clone());
                        self.tabs[self.active].profile_id = Some(profile.id);
                        self.dirty = true;
                    }
                }
                self.status = value.to_string();
                if self.session.is_some() {
                    self.status
                        .push_str("；已有会话保持原配置，请在新 tab 连接，或重连以应用改动");
                }
                self.request(backend, "profile.list", "profile.list", json!({}));
            }
            "profile.delete" => {
                self.status = value.to_string();
                self.request(backend, "profile.list", "profile.list", json!({}));
            }
            "workspace.load" => {
                self.revision = value["revision"].as_u64().unwrap_or(0);
                if let Ok(state) = serde_json::from_value::<Workspace>(value["state"].clone())
                    && !state.tabs.is_empty()
                {
                    self.tabs = state.tabs;
                    self.active = state.active.min(self.tabs.len() - 1);
                    self.migrate_cli_queries();
                    self.restore_browse();
                    self.profile = self.tabs[self.active]
                        .profile_id
                        .and_then(|id| self.profiles.iter().find(|p| p.id == id).cloned());
                    if !self.tabs[self.active].pending.is_null() {
                        self.detail = format!(
                            "恢复的编辑意图（基线已失效，请刷新并重新确认）：{}",
                            self.tabs[self.active].pending
                        );
                    }
                }
                self.workspace_loaded = true;
            }
            "workspace.save" => {
                self.revision = value["revision"].as_u64().unwrap_or(self.revision);
                self.saving = false;
            }
            "workspace.copy" => {
                if let Some(copy) = self.copy_pending.take() {
                    let original = std::mem::replace(&mut self.workspace_id, copy.clone());
                    self.revision = value["revision"].as_u64().unwrap_or(1);
                    self.save_blocked = false;
                    self.dirty = true;
                    self.status = format!("已另存工作区副本 {copy}；原工作区 {original} 未覆盖");
                }
            }
            "connection.open" => {
                self.operation_pending = false;
                match serde_json::from_value::<Profile>(
                    value.get("profile").cloned().unwrap_or(Value::Null),
                ) {
                    Ok(profile) => {
                        self.tabs[self.active].profile_id = Some(profile.id);
                        self.database = profile.database.clone();
                        self.profile = Some(profile);
                        self.session = Some(value);
                        self.status = "数据库连接已打开".into();
                        if let Some(target) = self.workbench.queued_target.take() {
                            if target.kind == "database" {
                                self.database = (!target.name.is_empty()).then_some(target.name);
                            } else {
                                self.load_target(target, backend);
                            }
                        }
                        self.action("list", backend);
                    }
                    Err(e) => self.status = format!("数据库会话缺少有效连接信息: {e}"),
                }
            }
            "resource.list" => match serde_json::from_value::<ResourcePage>(value) {
                Ok(page) => {
                    if self.is_redis() {
                        // Core 的扫描缓存按原始 key 去重，各页只发出尚未交付的项。
                        self.resources.extend(page.resources);
                        self.workbench.key_page =
                            self.resources.len().saturating_sub(1) / workbench::KEY_PAGE_SIZE;
                    } else {
                        self.resources = page.resources;
                    }
                    self.cursor = page.cursor;
                }
                Err(e) => self.status = e.to_string(),
            },
            "history.list" => {
                self.workbench.history = value
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                self.workbench.history_selected = (!self.workbench.history.is_empty()).then_some(0);
            }
            "transfer.preview" => {
                let valid = value
                    .get("errors")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty);
                self.detail = serde_json::to_string_pretty(&value).unwrap_or_default();
                self.preview = if valid {
                    self.preview_pending.take().map(|(_, p)| p)
                } else {
                    self.preview_pending = None;
                    None
                };
                self.status = if valid {
                    "预览完成；检查后可确认导入".into()
                } else {
                    "预览发现错误，请修正后重新预览".into()
                };
            }
            "resource.describe" => {
                self.schema = Some(value.clone());
                self.detail = value.to_string();
                if std::mem::take(&mut self.workbench.load_resource) {
                    let writable = self.profile.as_ref().is_some_and(|p| {
                        p.access_mode == datapier_core::model::AccessMode::ReadWrite
                    });
                    let editable = writable
                        && (self.is_redis()
                            || (self.target.as_ref().is_some_and(|t| t.kind == "table")
                                && self
                                    .schema
                                    .as_ref()
                                    .and_then(|s| s["keys"].as_array())
                                    .is_some_and(|keys| !keys.is_empty())));
                    self.action(if editable { "edit-open" } else { "browse" }, backend);
                }
            }
            "current.info" => {
                if let Some(mode) = value.get("sql_mode").and_then(Value::as_str) {
                    let settings = editor::MysqlMode {
                        no_backslash_escapes: mode
                            .split(',')
                            .any(|m| m.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES")),
                        ansi_quotes: mode
                            .split(',')
                            .any(|m| m.eq_ignore_ascii_case("ANSI_QUOTES")),
                    };
                    let query = &self.tabs[self.active].query;
                    match editor::current_statement(&query.text, query.cursor, settings) {
                        Ok(range) => {
                            if let (Some(start), Some(end), Some(s)) = (
                                editor::utf16_to_byte(&query.text, range.start),
                                editor::utf16_to_byte(&query.text, range.end),
                                self.session_id(),
                            ) {
                                let text = query.text[start..end].to_string();
                                self.request(backend,"execution.start","execution.start",json!({"session_id":s,"workspace_id":self.workspace_id,"text":text}));
                            }
                        }
                        Err(e) => self.status = e,
                    }
                } else {
                    self.status = "数据库未返回 sql_mode，无法安全定位当前语句".into();
                }
            }
            "change.open" => {
                self.edit_token = value.get("token").cloned();
                if let Some(schema) = value.get("schema") {
                    self.schema = Some(schema.clone());
                } else if self.is_redis() {
                    self.schema = Some(value.clone());
                }
                self.detail = value.to_string();
                if let Some(id) = value.get("result_id").and_then(Value::as_str) {
                    self.browse_result = true;
                    self.results = vec![id.to_owned()];
                    self.page_history.clear();
                    self.result_id = Some(id.into());
                    self.page(backend, 0);
                } else {
                    // MySQL change.open 只返回 token/schema；行数据必须再通过 browse 获取。
                    self.action("browse", backend);
                }
            }
            "change.discard" => {
                self.workbench.load_resource = true;
                self.action("describe", backend);
            }
            "execution.start" | "resource.browse" | "change.submit" | "transfer.import"
            | "transfer.export" => {
                self.operation_pending = false;
                self.large = None;
                self.task = value
                    .get("task_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.status = format!("任务已提交: {}", value);
                self.last_poll = Instant::now() - Duration::from_secs(2);
            }
            "execution.get" => {
                self.status = format!("任务 {}", value["state"]);
                let state = value["state"].as_str().unwrap_or("");
                if !matches!(state, "queued" | "running" | "cancelling") {
                    self.detail = value.to_string();
                    self.task = None;
                    if value["method"].as_str() == Some("change.submit") {
                        self.edit_token = None;
                        if let Some(g) = &mut self.grid {
                            g.editable = false;
                        }
                        let applied = state == "succeeded"
                            && value["summary"]["items"].as_array().is_some_and(|items| {
                                items
                                    .iter()
                                    .all(|item| item["status"].as_str() == Some("applied"))
                            });
                        if applied {
                            let target_invalidated = self.redis_changes.iter().any(|change| {
                                matches!(
                                    change["operation"].as_str(),
                                    Some("rename" | "delete_key")
                                )
                            });
                            // 提交成功后旧行基线不再可靠；保留布局并重新浏览取得新结果。
                            if let Some(g) = self.grid.take() {
                                self.tabs[self.active].layout = g.settings;
                            }
                            self.pending_rows.clear();
                            self.redis_changes.clear();
                            self.clear_redis_previews();
                            self.workbench.overlay = None;
                            self.tabs[self.active].pending = Value::Null;
                            self.dirty = true;
                            self.tabs[self.active].pending_live = false;
                            self.results.clear();
                            self.result_id = None;
                            self.page_history.clear();
                            self.page_token = None;
                            self.large = None;
                            self.schema = None;
                            self.generation = self.generation.wrapping_add(1);
                            if target_invalidated {
                                self.target = None;
                                self.resources.clear();
                                self.action("list", backend);
                            } else {
                                self.workbench.load_resource = true;
                                self.action("describe", backend);
                            }
                            return;
                        } else {
                            self.status.push_str("；基线已失效，检查结果后重新打开");
                        }
                    }
                    self.results = value
                        .get("results")
                        .and_then(Value::as_array)
                        .map(|v| {
                            v.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(id) = self.results.first().cloned() {
                        self.browse_result = value["method"].as_str() == Some("resource.browse");
                        self.result_id = Some(id);
                        self.page(backend, 0);
                    }
                }
            }
            "result.page" => match serde_json::from_value::<ResultPage>(value) {
                Ok(page) => {
                    if let Some(g) = &mut self.grid
                        && g.page.result.id == page.result.id
                    {
                        if let Err(e) = g.set_page(page) {
                            self.status = e.to_string()
                        }
                        return;
                    }
                    if self.has_live_drafts() {
                        self.status = "结果已就绪；请先提交或丢弃当前草稿".into();
                        return;
                    }
                    let editable = self.browse_result
                        && self.edit_token.is_some()
                        && self.profile.as_ref().is_some_and(|p| {
                            p.provider == datapier_core::model::ProviderKind::Mysql
                        })
                        && self
                            .schema
                            .as_ref()
                            .and_then(|s| s.get("keys"))
                            .and_then(Value::as_array)
                            .is_some_and(|keys| !keys.is_empty());
                    let mut g = GridState::new(page, editable);
                    if self.tabs[self.active].layout.len() == g.settings.len() {
                        g.settings = self.tabs[self.active].layout.clone();
                    }
                    self.grid = Some(g);
                }
                Err(e) => self.status = e.to_string(),
            },
            "file.open" => {
                if let Some(text) = value.get("text").and_then(Value::as_str) {
                    self.tabs[self.active].query = Editor::new(text);
                    self.dirty = true;
                }
            }
            _ => {
                self.detail = value.to_string();
                self.status = response.tag;
            }
        }
    }
    fn input_key(&mut self, key: &tern_sdk::Key) {
        let name = self.focus.clone();
        if let Some(editor) = self.editor(&name) {
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
            if let Some(edit) = edit
                && editor.key(edit, key.shift)
            {
                self.changed_field(&name);
            }
        }
    }
}

// SDK 按区域、祖先 key 与子节点序号派生 id；只在请求焦点时沿当前 View 找路径，
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
                    return Err(AppError::new("workspace_save", app.status.clone()));
                }
                copy_attempted = true;
                app.save_workspace_copy(backend);
                if app.copy_pending.is_none() {
                    return Err(AppError::new("workspace_save", app.status.clone()));
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

/// 在调用线程运行同步 TSP 界面，Session 始终由该线程独占。
/// Engine IPC 在独立 Tokio 线程运行；终端不可用、保存未确认或 TSP 失败时返回错误。
pub fn run(root: PathBuf, workspace_id: String) -> Result<()> {
    let options = Options::new()
        .app("Datapier")
        .version("0.1.0")
        .feature("edit")
        .feature("undo");
    let mut session = Session::<Msg>::connect(options)
        .map_err(tsp_error)?
        .ok_or_else(|| AppError::new("tsp_unavailable", "需要在 Tern 的 TSP pane 中运行"))?;
    let surface = session
        .open(
            SurfaceOptions::screen()
                .title("Datapier")
                .role("datapier.database")
                .keep(false),
        )
        .map_err(tsp_error)?;
    let waker = session
        .waker()
        .ok_or_else(|| AppError::new("tsp_unavailable", "TSP 终端没有提供唤醒句柄"))?;
    let mut backend = Backend::start(root, waker)?;
    let mut app = App::new(workspace_id);
    let mut css = String::new();
    let mut changed = true;
    let mut last_save = Instant::now();
    let mut quitting = false;
    loop {
        while let Some(reply) = backend.receive() {
            app.routed_response(reply, &backend);
            changed = true;
        }
        if quitting {
            if app.save_blocked {
                quitting = false;
                app.status = format!("保存失败，未退出：{}", app.status);
                changed = true;
            } else if app.dirty {
                app.save_workspace(&backend);
            } else if !app.saving {
                break;
            }
        }
        // Engine 事件只是提示，定期查询每个 tab 的当前任务以补偿遗漏事件。
        if app.last_poll.elapsed() >= Duration::from_millis(700) {
            app.last_poll = Instant::now();
            for (index, tab) in app.tabs.iter().enumerate() {
                let (task, generation) = if index == app.active {
                    (&app.task, app.generation)
                } else {
                    (&tab.runtime.task, tab.runtime.generation)
                };
                if let Some(task) = task
                    && let Err(e) = backend.request(
                        format!("{}:{}:execution.get/{task}", tab.id, generation),
                        "execution.get",
                        json!({"task_id":task}),
                    )
                {
                    app.status = e.to_string();
                    changed = true;
                }
            }
        }
        if app.dirty && last_save.elapsed() >= Duration::from_millis(800) {
            app.save_workspace(&backend);
            last_save = Instant::now();
        }
        if changed {
            let next_css = views::stylesheet(&app);
            if css != next_css {
                session
                    .stylesheet(surface, "datapier", Some(&next_css))
                    .map_err(tsp_error)?;
                css = next_css;
            }
            let view = views::render(&app);
            let focus = app
                .workbench
                .focus_request
                .take()
                .map(|key| focus_node_id(&view, &key));
            session.render(surface, view).map_err(tsp_error)?;
            if let Some(id) = focus {
                session.focus(surface, id.as_deref()).map_err(tsp_error)?;
            }
            changed = false;
        }
        match session.next(Some(Duration::from_millis(250))) {
            Ok(Some(Input::Msg(message, event))) => {
                match message {
                    Msg::Action(name) => app.action(&name, &backend),
                    Msg::Focus(name) => {
                        app.focus = name;
                        if let tern_sdk::Event::Focus(f) = event {
                            session.focus(surface, Some(&f.id)).map_err(tsp_error)?;
                        }
                    }
                    Msg::Edit(name, from, to, text, cursor, len) => {
                        if let Some(ed) = app.editor(&name)
                            && ed.text.encode_utf16().count() == len
                            && ed.edit(from..to, &text)
                        {
                            ed.set_selection(cursor, cursor);
                            app.changed_field(&name);
                        }
                    }
                    Msg::Undo(name) => {
                        if let Some(ed) = app.editor(&name)
                            && ed.undo()
                        {
                            app.changed_field(&name);
                        }
                    }
                    Msg::Profile(index) => {
                        if app.finish_inline(true) {
                            app.select_profile(index, &backend);
                        }
                    }
                    Msg::Resource(index) => {
                        if app.finish_inline(true) {
                            app.select_resource(index, &backend);
                        }
                    }
                    Msg::Tab(index) => {
                        if index < app.tabs.len() && app.finish_inline(true) {
                            app.switch_tab(index);
                            app.dirty = true;
                        }
                    }
                    Msg::ResultSet(id) => {
                        if !app.finish_inline(true) {
                            changed = true;
                            continue;
                        }
                        if app.has_live_drafts() {
                            app.status = "请先提交或丢弃待提交变更与粘贴预览".into();
                        } else {
                            app.large = None;
                            app.result_id = Some(id);
                            app.page_history.clear();
                            app.grid = None;
                            app.page(&backend, 0);
                        }
                    }
                    Msg::Complete(text, start, end) => {
                        if app.tabs[app.active].query.edit(start..end, &text) {
                            app.dirty = true;
                        }
                    }
                    Msg::Cell(row, column, extend) => {
                        if !app.finish_inline(true) {
                            changed = true;
                            continue;
                        }
                        if let Some(grid) = &mut app.grid {
                            if let Err(e) = grid.select(Cell { row, column }, extend, false) {
                                app.status = e.to_string();
                            }
                            app.focus = "grid".into();
                            session.focus(surface, None).map_err(tsp_error)?;
                        }
                    }
                    Msg::CellOpen(row, column) => {
                        if let Some(grid) = &mut app.grid {
                            let _ = grid.select(Cell { row, column }, false, false);
                        }
                        if app.is_redis() {
                            app.workbench_redis_edit(&backend);
                        } else {
                            app.begin_inline(&backend);
                        }
                    }
                    Msg::Row(row, column) => {
                        if !app.finish_inline(true) {
                            changed = true;
                            continue;
                        }
                        if let Some(grid) = &mut app.grid {
                            if let Err(e) = grid.select(Cell { row, column }, false, true) {
                                app.status = e.to_string();
                            }
                            app.focus = "grid".into();
                            session.focus(surface, None).map_err(tsp_error)?;
                        }
                    }
                }
                changed = true;
            }
            Ok(Some(Input::Key(key))) => {
                if app.workbench_key(&key, &backend) {
                    changed = true;
                    continue;
                }
                if key.is("ctrl+q") {
                    if !app.finish_inline(true) {
                        changed = true;
                        continue;
                    }
                    if !app.workspace_loaded && app.dirty {
                        app.status = "工作区尚未加载，无法保存".into();
                    } else {
                        quitting = true;
                        app.save_workspace(&backend);
                    }
                    changed = true;
                    continue;
                }
                if app.focus == "grid" {
                    if key.is("enter") || key.is("f2") {
                        if app.is_redis() {
                            app.workbench_redis_edit(&backend);
                        } else {
                            app.begin_inline(&backend);
                        }
                        changed = true;
                        continue;
                    }
                    if let Some(grid) = &mut app.grid {
                        let nav = match key.name.as_str() {
                            "up" => Some(Navigation::Up),
                            "down" => Some(Navigation::Down),
                            "left" => Some(Navigation::Left),
                            "right" => Some(Navigation::Right),
                            "home" => Some(Navigation::Home),
                            "end" => Some(Navigation::End),
                            "tab" if key.shift => Some(Navigation::BackTab),
                            "tab" => Some(Navigation::Tab),
                            _ => None,
                        };
                        if let Some(nav) = nav {
                            grid.navigate(nav, key.shift);
                        } else if key.is("ctrl+z") {
                            grid.undo();
                            app.dirty = true;
                        } else if key.is("ctrl+c") || key.is("meta+c") {
                            use base64::Engine as _;
                            match grid.copy_tsv() {
                                Ok(text) => {
                                    let encoded =
                                        base64::engine::general_purpose::STANDARD.encode(text);
                                    session
                                        .write_raw(format!("\x1b]52;c;{encoded}\x1b\\").as_bytes())
                                        .map_err(tsp_error)?;
                                    app.status = "已复制选区".into();
                                }
                                Err(e) => app.status = e.to_string(),
                            }
                        } else if key.name == "paste"
                            && let Some(text) = key.text.as_deref()
                        {
                            match grid.preview_paste(text) {
                                Ok(preview) => {
                                    app.detail = format!(
                                        "粘贴预览：{} 个单元格\n{}",
                                        preview.edits.len(),
                                        preview
                                            .edits
                                            .iter()
                                            .map(|e| format!(
                                                "{}:{}  {} → {}",
                                                e.cell.row + 1,
                                                e.cell.column,
                                                e.before.text(),
                                                e.after.text()
                                            ))
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    );
                                    app.paste_preview = Some(preview);
                                    app.workbench.overlay = Some("preview".into());
                                }
                                Err(e) => app.status = e.to_string(),
                            }
                        }
                    }
                } else {
                    app.input_key(&key);
                }
                changed = true;
            }
            Ok(Some(Input::Event(event))) => {
                if let tern_sdk::Event::Error(error) = event {
                    app.status = format!("Tern: {error:?}");
                    changed = true;
                }
            }
            // None 也可能来自超时或 waker，只有 InputClosed 才表示输入结束。
            Ok(Some(Input::Clipboard(_))) | Ok(None) => {}
            Err(tern_sdk::Error::InputClosed) => {
                finish_after_input_closed(&mut app, &mut backend)?;
                break;
            }
            Err(e) => return Err(tsp_error(e)),
        }
    }
    session.close().map_err(tsp_error)?;
    Ok(())
}
