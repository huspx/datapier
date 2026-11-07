use super::*;
use crate::i18n::{UiArg, UiMessage};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::{AccessMode, ProviderKind, Value as DataValue};

mod actions;
mod cli;
mod input;
mod redis_edit;
mod sql_edit;

pub(super) fn grid_error(error: AppError) -> UiMessage {
    let key = match error.message.as_str() {
        "目标单元格不在当前页" => "目标单元格不在当前页",
        "目标单元格不存在" => "目标单元格不存在",
        "目标单元格不可编辑" => "目标单元格不可编辑",
        "列不存在" => "列不存在",
        "缺少编辑基线" => "缺少编辑基线",
        "不存在的值或未加载的大值不能复制" => "不存在的值或未加载的大值不能复制",
        "不存在的值或未加载的大值不可编辑" => "不存在的值或未加载的大值不可编辑",
        "非 NULL 的反斜线必须成对出现" => "非 NULL 的反斜线必须成对出现",
        "布尔值必须为 true、false、1 或 0" => "布尔值必须为 true、false、1 或 0",
        "无效布尔值" => "无效布尔值",
        "无效整数" => "无效整数",
        "无效无符号整数" => "无效无符号整数",
        "无效精确小数" => "无效精确小数",
        "无效浮点数" => "无效浮点数",
        "二进制值必须是 Base64" => "二进制值必须是 Base64",
        "粘贴预览已过期或目标不可编辑" => "粘贴预览已过期或目标不可编辑",
        "粘贴范围超出选中行" => "粘贴范围超出选中行",
        "粘贴范围超出选区" => "粘贴范围超出选区",
        "目标单元格不是大字段" => "目标单元格不是大字段",
        "大字段尚未完整读取" => "大字段尚未完整读取",
        "大字段类型与原值不匹配" => "大字段类型与原值不匹配",
        _ => return UiMessage::raw(error.message),
    };
    UiMessage::key(key)
}

pub(super) fn grid_cell_error(error: AppError, grid: &GridState, cell: Cell) -> UiMessage {
    if let Some(column) = grid.page.result.columns.get(cell.column) {
        let template = if error.message == format!("列 {} 不可编辑", column.name) {
            Some("列 {name} 不可编辑")
        } else if error.message == format!("列 {} 不允许 NULL", column.name) {
            Some("列 {name} 不允许 NULL")
        } else if error.message == format!("列 {} 的值类型不匹配", column.name) {
            Some("列 {name} 的值类型不匹配")
        } else {
            None
        };
        if let Some(template) = template {
            return UiMessage::template(template, vec![("name", UiArg::Raw(column.name.clone()))]);
        }
    }
    grid_error(error)
}

pub(super) const KEY_PAGE_SIZE: usize = 200;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum RedisTtlPolicy {
    #[default]
    Keep,
    Set,
    Persist,
}

// 确认期间冻结资源和原始行；绝不提前生成待提交的 RowChange。
#[derive(Clone)]
struct DeleteRowsConfirmation {
    tab: Uuid,
    generation: u64,
    profile: Uuid,
    session: Value,
    target: Value,
    database: Option<String>,
    result_id: String,
    grid_result_id: Uuid,
    schema: Value,
    edit_token: Value,
    columns: Value,
    rows: Vec<(u64, Vec<DataValue>)>,
}

#[derive(Clone)]
pub(super) struct CliRecord {
    pub id: Uuid,
    pub command: String,
    pub instance: String,
    pub database: String,
    pub state: UiMessage,
    pub elapsed: Option<Duration>,
    pub error: UiMessage,
    pub results: Vec<String>,
    pub result_grids: Vec<Option<GridState>>,
    pub result_page_histories: Vec<Vec<u64>>,
}

/// 工作台的临时界面状态；数据库基线和草稿仍由 App 管理。
#[derive(Clone)]
pub(super) struct Workbench {
    pub query_ui: crate::query_ui::QueryUiState,
    pub(super) archery_history: crate::application::ArcheryHistory,
    pub sql_mode: Option<editor::MysqlMode>,
    pub section: String,
    pub overlay: Option<String>,
    pub(super) diagnostic_task_request: Option<(Uuid, String)>,
    delete_rows: Option<DeleteRowsConfirmation>,
    pub(super) redis_row_pending: Option<crate::redis_rows::RedisRowPending>,
    pub connecting: bool,
    pub connection_error: Option<String>,
    pub connection_tls_required: bool,
    pub profile_error: Option<String>,
    pub field_error: Option<forms::FieldError>,
    pub profile_tls_details: bool,
    pub transfer_advanced: bool,
    pub cli: bool,
    pub cli_export: bool,
    pub tree: bool,
    pub redis_expansion: crate::redis_tree::Expansion,
    pub redis_tree: crate::redis_tree::Tree,
    pub redis_tree_owner: Option<(Uuid, u32)>,
    pub cli_grid: Option<GridState>,
    pub cli_cell: Option<Cell>,
    pub cli_large: Option<LargeLoad>,
    pub(super) cli_value_token: Option<Uuid>,
    pub key_page: usize,
    pub key_type_menu: bool,
    pub generic_select: Option<String>,
    pub cli_results: Vec<String>,
    pub cli_result_id: Option<String>,
    pub cli_command: Option<String>,
    // None 表示尚未独立选择；Some(None) 表示命令连接失效、DB 无法确认。
    pub cli_database: Option<Option<u32>>,
    pub cli_result_databases: std::collections::HashMap<String, u32>,
    pub cli_history: Vec<CliRecord>,
    pub cli_selected: Option<Uuid>,
    pub cli_reveal: bool,
    pub cli_running: bool,
    pub cli_started_at: Option<Instant>,
    pub cli_elapsed: Option<Duration>,
    pub cli_error: UiMessage,
    pub(super) cli_page_token: Option<Uuid>,
    pub(super) cli_page_history: Vec<u64>,
    cli_page_transition: Option<(bool, u64)>,
    pub value_format: String,
    pub ttl_policy: RedisTtlPolicy,
    pub scan_dirty: bool,
    pub inline: Option<Cell>,
    // 详情编辑与网格选区独立，并用结果 ID 防止刷新后写入同坐标的新结果。
    pub cell_target: Option<(Uuid, Cell)>,
    pub cell_fields_open: bool,
    pub cell_editor: Editor,
    pub load_resource: bool,
    pub sql_forms: forms::SqlForms,
    pub focus_request: Option<String>,
    // 保留 Redis 基线中的原始身份，避免将成员改名误当作新增。
    redis_identity: Option<(String, String)>,
    pub queued_target: Option<ResourceTarget>,
    pub redis_loading: Option<Cell>,
    pub(super) redis_undo_groups: Vec<(usize, usize)>,
}

impl Default for Workbench {
    fn default() -> Self {
        Self {
            query_ui: crate::query_ui::QueryUiState::default(),
            archery_history: crate::application::ArcheryHistory::default(),
            sql_mode: None,
            section: "data".into(),
            overlay: None,
            diagnostic_task_request: None,
            delete_rows: None,
            redis_row_pending: None,
            connecting: false,
            connection_error: None,
            connection_tls_required: false,
            profile_error: None,
            field_error: None,
            profile_tls_details: false,
            transfer_advanced: false,
            cli: false,
            cli_export: false,
            cli_grid: None,
            cli_cell: None,
            cli_large: None,
            cli_value_token: None,
            key_page: 0,
            key_type_menu: false,
            generic_select: None,
            cli_results: Vec::new(),
            cli_result_id: None,
            cli_command: None,
            cli_database: None,
            cli_result_databases: std::collections::HashMap::new(),
            cli_history: Vec::new(),
            cli_selected: None,
            cli_reveal: false,
            cli_running: false,
            cli_started_at: None,
            cli_elapsed: None,
            cli_error: UiMessage::default(),
            cli_page_token: None,
            cli_page_history: Vec::new(),
            cli_page_transition: None,
            tree: false,
            redis_expansion: crate::redis_tree::Expansion::default(),
            redis_tree: crate::redis_tree::Tree::default(),
            redis_tree_owner: None,
            scan_dirty: false,
            value_format: "text".into(),
            ttl_policy: RedisTtlPolicy::Keep,
            inline: None,
            cell_target: None,
            cell_fields_open: false,
            cell_editor: Editor::default(),
            load_resource: false,
            sql_forms: forms::SqlForms::default(),
            focus_request: None,
            queued_target: None,
            redis_loading: None,
            redis_undo_groups: Vec::new(),
            redis_identity: None,
        }
    }
}

// UTF-8 内容直接展示；Base64 前缀和非 UTF-8 内容保留可逆字节表示。
pub(super) fn redis_field(value: &DataValue) -> std::result::Result<String, UiMessage> {
    match value {
        DataValue::Bytes(encoded) => {
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|e| UiMessage::raw(e.to_string()))?;
            match String::from_utf8(bytes) {
                Ok(text) if !text.starts_with("base64:") => Ok(text),
                _ => Ok(format!("base64:{encoded}")),
            }
        }
        DataValue::Integer(value) | DataValue::Float(value) => Ok(value.clone()),
        DataValue::Large { .. } => Err(UiMessage::key("请先读取完整 Redis 值")),
        _ => Err(UiMessage::key("Redis 结果类型与资源类型不匹配")),
    }
}
pub(super) fn redis_key_text(name: &str, key_b64: Option<&str>) -> String {
    key_b64.map_or_else(
        || name.to_owned(),
        |encoded| {
            redis_field(&DataValue::Bytes(encoded.to_owned()))
                .unwrap_or_else(|_| format!("base64:{encoded}"))
        },
    )
}

fn redis_bytes(value: &str) -> std::result::Result<Vec<u8>, String> {
    match value.strip_prefix("base64:") {
        Some(encoded) => STANDARD.decode(encoded).map_err(|e| e.to_string()),
        None => Ok(value.as_bytes().to_vec()),
    }
}

impl App {
    fn visible_key_count(&self) -> usize {
        if self.workbench.tree {
            if !self.redis_tree_matches_resources() {
                return 0;
            }
            self.workbench
                .redis_tree
                .rows(
                    &self.workbench.redis_expansion,
                    self.profile.as_ref().map(|profile| {
                        (
                            profile.id,
                            self.tabs[self.active]
                                .browse
                                .redis_db
                                .unwrap_or_else(|| profile.redis_db()),
                        )
                    }),
                )
                .len()
        } else {
            self.resources.len()
        }
    }

    pub(super) fn normalize_key_page(&mut self) {
        let last = self.visible_key_count().saturating_sub(1) / KEY_PAGE_SIZE;
        self.workbench.key_page = self.workbench.key_page.min(last);
    }

    pub(super) fn clear_cli_session(&mut self) {
        self.workbench_cli_store_grid();
        self.workbench_clear_cli_value();
        self.workbench.cli_results.clear();
        self.workbench.cli_result_id = None;
        self.workbench.cli_command = None;
        self.workbench.cli_page_token = None;
        self.workbench.cli_page_transition = None;
        self.workbench.cli_page_history.clear();
        self.workbench.cli_error.clear();
        self.workbench.cli_started_at = None;
        self.workbench.cli_elapsed = None;
        self.clear_redis_edit_state();
    }

    pub(super) fn clear_redis_edit_state(&mut self) {
        self.workbench.redis_undo_groups.clear();
        self.workbench.redis_identity = None;
    }

    // 已排队的操作使用执行时索引，界面行始终指向读取时的原始索引。
    pub(super) fn redis_list_index(
        &self,
        original: i64,
        insert: bool,
    ) -> std::result::Result<i64, UiMessage> {
        let mut len = self
            .grid
            .as_ref()
            .map(|g| g.page.result.row_count as i64)
            .ok_or_else(|| UiMessage::key("没有 Redis 结果"))?;
        let mut position = if original < 0 {
            len + original
        } else {
            original
        };
        if position < 0 || position > len || (position == len && !insert) {
            return Err(UiMessage::key("List 原始索引超出范围"));
        }
        let mut deleted = false;
        for change in &self.redis_changes {
            let Some(op) = change.get("operation").and_then(Value::as_str) else {
                continue;
            };
            if !matches!(op, "list_set" | "list_insert" | "list_delete") {
                continue;
            }
            let index = change
                .get("index")
                .and_then(Value::as_i64)
                .ok_or_else(|| UiMessage::key("已暂存 List 索引无效"))?;
            let index = if index < 0 { len + index } else { index };
            match op {
                "list_insert" => {
                    if index < 0 || index > len {
                        return Err(UiMessage::key("已暂存 List 索引超出范围"));
                    }
                    if index <= position {
                        position += 1;
                    }
                    len += 1;
                }
                "list_delete" => {
                    if index < 0 || index >= len {
                        return Err(UiMessage::key("已暂存 List 索引超出范围"));
                    }
                    if index == position {
                        deleted = true;
                    }
                    if index < position {
                        position -= 1;
                    }
                    len -= 1;
                }
                _ => {}
            }
        }
        if deleted {
            return Err(UiMessage::key("选中的原始 List 元素已暂存删除"));
        }
        Ok(position)
    }

    pub(super) fn is_redis(&self) -> bool {
        self.profile
            .as_ref()
            .is_some_and(|p| p.provider == ProviderKind::Redis)
    }

    pub(super) fn navigation_compact(&self) -> bool {
        if self.viewport_class() == "bp-small" {
            !self.navigation.narrow_expanded
        } else {
            !self.navigation.expanded
        }
    }

    pub(super) fn detail_cell(&self) -> Option<Cell> {
        let (result_id, cell) = self.workbench.cell_target?;
        let grid = self.grid.as_ref()?;
        (grid.page.result.id == result_id
            && grid.row_in_page(cell.row)
            && grid.value(cell).is_some())
        .then_some(cell)
    }

    pub(super) fn update_viewport(&mut self, caps: &tern_sdk::Capabilities) -> bool {
        // cell 是取整后的物理像素，缺少 DPR，不能用作 CSS px。
        // TSP 区域以 13px 字号为设计单位；按默认 0.6em 等宽字体估算 pane。
        // cols 有取整误差；其他字宽的字体仍为近似值，不能当作精确几何。
        let width = (f64::from(caps.cols) * 13.0 * 0.6).round();
        if width <= 0.0 || width as u32 == self.viewport_width {
            return false;
        }
        // 窄屏的临时展开仅在当前窄屏停留期间有效，不覆盖持久化偏好。
        if self.viewport_class() != "bp-small" && width as u32 <= 760 {
            self.navigation.narrow_expanded = false;
        }
        self.viewport_width = width as u32;
        true
    }

    // @media 比较整个窗口；布局按 pane 的列数估算，避免分屏误用窗口断点。
    pub(super) fn viewport_class(&self) -> &'static str {
        match self.viewport_width {
            1..=760 => "bp-small",
            761..=900 => "bp-medium",
            901..=1120 => "bp-compact",
            _ => "bp-wide",
        }
    }

    pub(super) fn fit_grid(&mut self) {
        let width = self.viewport_width;
        if width == 0 || self.workbench.inline.is_some() {
            return;
        }
        let sidebar = if width <= 760 || self.navigation_compact() {
            58
        } else {
            224
        };
        let beside = if self.is_redis() {
            match width {
                0..=760 => 210,
                761..=900 => 240,
                _ => 284,
            }
        } else if self.workbench.overlay.as_deref() == Some("cell") && width > 900 {
            if width <= 1120 { 230 } else { 282 }
        } else {
            0
        };
        let mysql = !self.is_redis();
        if let Some(grid) = &mut self.grid {
            grid.fit_columns(width.saturating_sub(sidebar + beside + 38), mysql);
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/workbench.rs"]
mod tests;
