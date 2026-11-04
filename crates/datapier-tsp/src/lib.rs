//! TSP 界面及编辑状态，通过 Engine IPC 操作数据；Session 由运行入口所在 UI 线程独占。

mod action;
mod app;
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
mod input;
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
mod requests;
mod resource_context;
mod response;
mod runtime;
mod tabs;
mod values;
mod views;
mod workbench;
mod workspace;

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
    CliResize(f64),
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
// 零标签只保留连接身份与目标，不保留已关闭的物理会话、结果或编辑基线。
#[derive(Clone, Serialize, Deserialize)]
struct EmptyConnectionContext {
    profile_id: Uuid,
    database: Option<String>,
    redis_db: Option<u32>,
}

#[derive(Serialize, Deserialize)]
struct Workspace {
    tabs: Vec<Tab>,
    active: usize,
    pending: Value,
    #[serde(default = "expanded_by_default")]
    navigation_expanded: bool,
    #[serde(default)]
    empty_context: Option<EmptyConnectionContext>,
}

fn expanded_by_default() -> bool {
    true
}

/// 每个 UI 会话只上传一次图标，视图复用 blob ID。
struct IconBlobs {
    brand: String,
    mysql: String,
    redis: String,
    archery: String,
    archery_badge: String,
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
    empty_context: Option<EmptyConnectionContext>,
    profile_list_token: Option<Uuid>,
    server_list_token: Option<Uuid>,
    profile_saves: std::collections::BTreeSet<Uuid>,
    form_id: Option<Uuid>,
    form_editing: bool,
    archery: connections::archery::Wizard,
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
    // 高度偏好属于当前 UI 工作区会话，切标签与收起 CLI 都不覆盖它。
    cli_share: f64,
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
fn tsp_error(error: tern_sdk::Error) -> AppError {
    AppError::new("tsp", error.to_string())
}
