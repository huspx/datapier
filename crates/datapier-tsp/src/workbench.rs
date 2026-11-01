use super::*;
use crate::i18n::{UiArg, UiMessage};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::{AccessMode, ProviderKind, Value as DataValue};

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

/// 工作台的临时界面状态；数据库基线和草稿仍由 App 管理。
#[derive(Clone)]
pub(super) struct Workbench {
    pub query_ui: crate::query_ui::QueryUiState,
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
    pub tree: bool,
    pub redis_expansion: crate::redis_tree::Expansion,
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
    pub cli_running: bool,
    pub cli_started_at: Option<Instant>,
    pub cli_elapsed: Option<Duration>,
    pub cli_history_expanded: bool,
    pub cli_error: UiMessage,
    cli_page_token: Option<Uuid>,
    cli_page_history: Vec<u64>,
    cli_page_transition: Option<(bool, u64)>,
    pub value_format: String,
    pub ttl_policy: RedisTtlPolicy,
    pub history: Vec<Value>,
    pub history_selected: Option<usize>,
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
            cli_running: false,
            cli_started_at: None,
            cli_elapsed: None,
            cli_history_expanded: false,
            cli_error: UiMessage::default(),
            cli_page_token: None,
            cli_page_history: Vec::new(),
            cli_page_transition: None,
            tree: false,
            redis_expansion: crate::redis_tree::Expansion::default(),
            scan_dirty: false,
            value_format: "text".into(),
            ttl_policy: RedisTtlPolicy::Keep,
            history: Vec::new(),
            history_selected: None,
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
            crate::redis_tree::Tree::from_resources(&self.resources)
                .rows(
                    &self.workbench.redis_expansion,
                    self.profile
                        .as_ref()
                        .map(|profile| (profile.id, profile.redis_db)),
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
        self.workbench_clear_cli_value();
        self.workbench.cli_grid = None;
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

    /// 在创建草稿前按原始可靠键检查整批目标，避免删除与更新同一身份。
    pub(super) fn check_rows_not_deleted(
        &self,
        rows: impl IntoIterator<Item = u64>,
    ) -> Result<(), UiMessage> {
        if !self
            .pending_rows
            .iter()
            .any(|change| change.operation == "delete")
        {
            return Ok(());
        }
        let invalid = || UiMessage::key("当前结果没有可用的编辑基线或可靠键");
        let grid = self.grid.as_ref().ok_or_else(invalid)?;
        let keys: Vec<String> = self
            .schema
            .as_ref()
            .and_then(|schema| schema.get("keys"))
            .and_then(Value::as_array)
            .and_then(|keys| keys.first())
            .and_then(|keys| serde_json::from_value(keys.clone()).ok())
            .filter(|keys: &Vec<String>| !keys.is_empty())
            .ok_or_else(invalid)?;
        for row in rows {
            if !grid.row_in_page(row) {
                return Err(invalid());
            }
            let original = grid.original_row(row).ok_or_else(invalid)?;
            if original.len() != grid.page.result.columns.len() {
                return Err(invalid());
            }
            let before: std::collections::BTreeMap<_, _> = grid
                .page
                .result
                .columns
                .iter()
                .zip(original)
                .map(|(column, value)| (column.name.clone(), value.clone()))
                .collect();
            if before.len() != original.len() {
                return Err(invalid());
            }
            let mut key = std::collections::BTreeMap::new();
            for name in &keys {
                let value = before.get(name).ok_or_else(invalid)?;
                if matches!(
                    value,
                    DataValue::Null | DataValue::Missing | DataValue::Large { .. }
                ) || key.insert(name.clone(), value.clone()).is_some()
                {
                    return Err(invalid());
                }
            }
            if self
                .pending_rows
                .iter()
                .any(|change| change.operation == "delete" && change.key == key)
            {
                return Err(UiMessage::key("目标行已有待提交变更"));
            }
        }
        Ok(())
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

    fn workbench_can_edit(&self) -> bool {
        self.profile
            .as_ref()
            .is_some_and(|p| p.access_mode == AccessMode::ReadWrite)
            && self.browse_result
            && self.edit_token.is_some()
            && self.target.as_ref().is_some_and(|t| t.kind == "table")
            && self.schema.as_ref().is_some_and(|schema| {
                schema
                    .get("engine")
                    .and_then(Value::as_str)
                    .is_some_and(|engine| !engine.is_empty())
                    && schema
                        .get("keys")
                        .and_then(Value::as_array)
                        .is_some_and(|keys| {
                            keys.first()
                                .and_then(Value::as_array)
                                .is_some_and(|columns| !columns.is_empty())
                        })
            })
            && self.grid.as_ref().is_some_and(|g| g.editable)
    }
    /// 确认弹窗展示的冻结行数。
    pub(super) fn delete_rows_count(&self) -> usize {
        self.workbench
            .delete_rows
            .as_ref()
            .map_or(0, |pending| pending.rows.len())
    }
    /// 返回确认时冻结的库名和表名，避免弹窗展示后来切换的目标。
    pub(super) fn delete_rows_target(&self) -> Option<(&str, &str)> {
        let pending = self.workbench.delete_rows.as_ref()?;
        let database = pending
            .target
            .get("database")
            .and_then(Value::as_str)
            .or(pending.database.as_deref())?;
        Some((database, pending.target.get("name")?.as_str()?))
    }

    pub(super) fn request_delete_rows(&mut self) {
        self.workbench.delete_rows = None;
        if self.workbench.inline.is_some()
            || self.paste_preview.is_some()
            || self.page_token.is_some()
        {
            self.status = UiMessage::key("请先完成或取消单元格编辑及粘贴预览");
            return;
        }
        if !self.tabs[self.active].pending.is_null() && !self.tabs[self.active].pending_live {
            self.status = UiMessage::key("请先恢复或丢弃已保存的编辑意图");
            return;
        }
        if !self.workbench_can_edit() || !self.can_copy_insert() {
            self.status = UiMessage::key("当前结果没有可用的编辑基线或可靠键");
            return;
        }
        let Some(grid) = self.grid.as_ref() else {
            return;
        };
        let rows = grid.selected_rows();
        let (
            Some(profile),
            Some(session),
            Some(target),
            Some(result_id),
            Some(schema),
            Some(edit_token),
        ) = (
            self.profile.as_ref(),
            self.session_id(),
            self.target.as_ref(),
            self.result_id.as_ref(),
            self.schema.as_ref(),
            self.edit_token.as_ref(),
        )
        else {
            self.status = UiMessage::key("当前结果没有可用的编辑基线或可靠键");
            return;
        };
        if rows.is_empty() || result_id != &grid.page.result.id.to_string() {
            self.status = UiMessage::key("目标行已失效");
            return;
        }
        let mut originals = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(original) = grid.original_row(row) else {
                self.status = UiMessage::key("目标行已失效");
                return;
            };
            originals.push((row, original.to_vec()));
        }
        self.workbench.delete_rows = Some(DeleteRowsConfirmation {
            tab: self.tabs[self.active].id,
            generation: self.generation,
            profile: profile.id,
            session,
            target: json!(target),
            database: self.database.clone(),
            result_id: result_id.clone(),
            grid_result_id: grid.page.result.id,
            schema: schema.clone(),
            edit_token: edit_token.clone(),
            columns: json!(grid.page.result.columns),
            rows: originals,
        });
        self.workbench.overlay = Some("delete-rows".into());
    }

    fn confirm_delete_rows(&mut self) {
        if self.workbench.overlay.as_deref() != Some("delete-rows") {
            return;
        }
        self.workbench.overlay = None;
        let Some(pending) = self.workbench.delete_rows.take() else {
            return;
        };
        let valid_context = self.workbench_can_edit()
            && self.can_copy_insert()
            && self.workbench.inline.is_none()
            && self.paste_preview.is_none()
            && self.page_token.is_none()
            && (self.tabs[self.active].pending.is_null() || self.tabs[self.active].pending_live)
            && self.tabs[self.active].id == pending.tab
            && self.generation == pending.generation
            && self
                .profile
                .as_ref()
                .is_some_and(|profile| profile.id == pending.profile)
            && self.session_id().as_ref() == Some(&pending.session)
            && self
                .target
                .as_ref()
                .is_some_and(|target| json!(target) == pending.target)
            && self.database == pending.database
            && self.result_id.as_ref() == Some(&pending.result_id)
            && self.schema.as_ref() == Some(&pending.schema)
            && self.edit_token.as_ref() == Some(&pending.edit_token);
        let Some(grid) = self.grid.as_ref().filter(|grid| {
            valid_context
                && grid.page.result.id == pending.grid_result_id
                && json!(grid.page.result.columns) == pending.columns
                && pending.rows.iter().all(|(row, original)| {
                    grid.row_in_page(*row)
                        && grid.original_row(*row) == Some(original.as_slice())
                        && grid.page.rows.get((*row - grid.page.start) as usize) == Some(original)
                })
        }) else {
            self.status = UiMessage::key("删除确认已过期，请重新选择行");
            return;
        };
        let keys: Vec<String> = pending
            .schema
            .get("keys")
            .and_then(Value::as_array)
            .and_then(|keys| keys.first())
            .and_then(|keys| serde_json::from_value(keys.clone()).ok())
            .unwrap_or_default();
        let rows: Vec<_> = pending.rows.iter().map(|(row, _)| *row).collect();
        let changes = match grid.delete_changes(&rows, &keys) {
            Ok(changes) => changes,
            Err(error) => {
                self.status = UiMessage::local(error.message);
                return;
            }
        };
        // 可能已有恢复或暂存的更新／删除，不能对同一身份重复生成变更。
        if changes.iter().any(|change| {
            self.pending_rows
                .iter()
                .any(|existing| existing.operation != "insert" && existing.key == change.key)
        }) {
            self.status = UiMessage::key("目标行已有待提交变更");
            return;
        }
        self.pending_rows.extend(changes);
        self.detail = serde_json::to_string_pretty(&self.pending_rows).unwrap_or_default();
        self.set_detail_context("草稿详情");
        self.dirty = true;
        self.status = UiMessage::key("删除已加入待提交变更");
    }

    pub(super) fn begin_inline(&mut self, backend: &Backend) {
        if self.workbench.inline.is_some() {
            return;
        }
        if !self.tabs[self.active].pending.is_null() && !self.tabs[self.active].pending_live {
            self.status = UiMessage::key("请先恢复或丢弃已保存的编辑意图");
            return;
        }
        if !self.workbench_can_edit() {
            self.status = UiMessage::key("当前结果没有可用的编辑基线或可靠键");
            return;
        }
        let Some(cell) = self.grid.as_ref().and_then(|g| g.focus) else {
            self.status = UiMessage::key("没有选中单元格");
            return;
        };
        if let Err(error) = self.check_rows_not_deleted([cell.row]) {
            self.status = error;
            return;
        }
        let grid = self.grid.as_ref().unwrap();
        if grid
            .page
            .result
            .columns
            .get(cell.column)
            .is_none_or(|c| c.generated)
        {
            self.status = UiMessage::key("该列不可编辑");
            return;
        }
        if self.workbench.overlay.as_deref() == Some("cell") {
            self.workbench.overlay = None;
            self.workbench.cell_target = None;
            self.workbench.cell_fields_open = false;
        }
        let large = matches!(grid.value(cell), Some(DataValue::Large { .. }));
        if !large {
            match grid.cell_edit_text(cell) {
                Ok(text) => self.field_set("cell", text),
                Err(error) => {
                    self.status = grid_error(error);
                    return;
                }
            }
        }
        self.workbench.inline = Some(cell);
        if large {
            self.open_cell(backend);
            if self.large.is_none() {
                self.workbench.inline = None;
            }
        } else {
            self.focus = "cell".into();
            self.workbench.focus_request = Some("cell-editor".into());
        }
    }

    pub(super) fn finish_inline(&mut self, commit: bool) -> bool {
        let Some(cell) = self.workbench.inline else {
            return true;
        };
        if !commit {
            self.large = None;
            self.workbench.inline = None;
            self.workbench.focus_request = Some(String::new());
            self.focus = "grid".into();
            return true;
        }
        if self.large.is_some() {
            self.status = UiMessage::key("请等待完整字段读取完成");
            return false;
        }
        if !self.workbench_can_edit() {
            self.status = UiMessage::key("编辑基线已失效，请保留草稿并重新打开资源");
            return false;
        }
        if let Err(error) = self.check_rows_not_deleted([cell.row]) {
            self.status = error;
            return false;
        }
        let text = self.field("cell").to_owned();
        let result = self
            .grid
            .as_mut()
            .ok_or_else(|| UiMessage::key("结果已关闭"))
            .and_then(|grid| {
                if grid.focus != Some(cell) {
                    return Err(UiMessage::key("编辑目标已改变"));
                }
                let value = grid
                    .parse_cell_text(cell, &text)
                    .map_err(|error| grid_cell_error(error, grid, cell))?;
                grid.edit(cell, value)
                    .map_err(|error| grid_cell_error(error, grid, cell))
            });
        match result {
            Ok(()) => {
                self.dirty = true;
                self.workbench.inline = None;
                self.workbench.focus_request = Some(String::new());
                self.focus = "grid".into();
                true
            }
            Err(error) => {
                self.field_error("cell", error);
                false
            }
        }
    }

    pub(super) fn workbench_key(&mut self, key: &tern_sdk::Key, backend: &Backend) -> bool {
        // 键盘与鼠标共用类型切换入口，保持默认端口和数据库候选失效规则一致。
        if self.workbench.overlay.as_deref() == Some("profile")
            && let Some(focused) = self.focus.strip_prefix("provider-")
            && !key.ctrl
            && !key.meta
            && !key.alt
            && matches!(
                key.name.as_str(),
                "left" | "right" | "up" | "down" | "home" | "end" | "space" | "enter"
            )
        {
            if !self.connection_form_busy() {
                let provider = match key.name.as_str() {
                    "home" => "mysql",
                    "end" => "redis",
                    "space" | "enter" => focused,
                    _ => {
                        if self.field("provider") == "mysql" {
                            "redis"
                        } else {
                            "mysql"
                        }
                    }
                };
                let action = if provider == "redis" {
                    "field:provider:redis"
                } else {
                    "field:provider:mysql"
                };
                let focus = format!("input-provider-{provider}");
                self.workbench_action(action, backend);
                self.workbench.focus_request = Some(focus);
            }
            return true;
        }
        // 原生下拉先消费关闭与确认键，避免穿透到 CLI 或所属弹窗。
        let picker_open = self.navigation.redis_db_menu
            || self.navigation.form_redis_db_menu
            || self.workbench.key_type_menu
            || self.workbench.cell_fields_open
            || self.workbench.sql_forms.open_select.is_some()
            || self.workbench.generic_select.is_some();
        if picker_open && key.is("escape") {
            self.navigation.redis_db_menu = false;
            self.navigation.form_redis_db_menu = false;
            self.workbench.key_type_menu = false;
            self.workbench.cell_fields_open = false;
            self.workbench.close_form_select();
            self.workbench.generic_select = None;
            return true;
        }
        if picker_open && key.is("enter") {
            return true;
        }
        if self.workbench.overlay.is_some() {
            if key.is("escape") {
                if self.workbench.overlay.as_deref() == Some("profile")
                    && self.connection_form_busy()
                {
                    return true;
                }
                if !self.workbench.close_form_select() {
                    if self.workbench.overlay.as_deref() == Some("redis-key-delete") {
                        self.redis_selection_action(backend, "redis-key-delete-cancel");
                    }
                    self.cancel_redis_row_delete();
                    self.workbench_clear_cli_value();
                    self.workbench.overlay = None;
                    self.workbench.delete_rows = None;
                    self.workbench.redis_identity = None;
                }
                return true;
            }
            return false;
        }
        if self.is_redis() && self.workbench.cli && self.focus == "cli_text" && key.is("enter") {
            self.workbench_action("cli-run", backend);
            return true;
        }
        if self.workbench.inline.is_some() {
            if key.is("escape") {
                self.finish_inline(false);
                return true;
            }
            if key.is("enter") {
                self.finish_inline(true);
                return true;
            }
            if key.name == "tab" && !key.ctrl && !key.alt && !key.meta {
                if !self.finish_inline(true) {
                    return true;
                }
                let mut found = false;
                if let Some(grid) = &mut self.grid {
                    let direction = if key.shift {
                        Navigation::BackTab
                    } else {
                        Navigation::Tab
                    };
                    let max = grid
                        .page
                        .rows
                        .len()
                        .saturating_mul(grid.visible_columns().len());
                    let original = grid.focus;
                    for _ in 0..max {
                        let previous = grid.focus;
                        grid.navigate(direction, false);
                        if grid.focus == previous {
                            break;
                        }
                        if let Some(cell) = grid.focus
                            && grid
                                .page
                                .result
                                .columns
                                .get(cell.column)
                                .is_some_and(|c| !c.generated)
                            && grid
                                .value(cell)
                                .is_some_and(|value| !matches!(value, DataValue::Missing))
                        {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        grid.focus = original;
                        grid.anchor = original;
                    }
                }
                if found {
                    self.begin_inline(backend);
                }
                return true;
            }
            return false;
        }
        if self.focus == "grid" && (key.is("enter") || key.is("f2")) {
            if self.is_redis() {
                self.workbench_redis_edit(backend);
            } else {
                self.begin_inline(backend);
            }
            return true;
        }
        false
    }

    pub(super) fn workbench_redis_edit(&mut self, backend: &Backend) {
        if self
            .tabs
            .get(self.active)
            .is_some_and(|tab| self.redis_key_delete_tab_pending(tab.id))
        {
            self.status = UiMessage::key("请先提交或丢弃当前草稿");
            return;
        }
        if !self.is_redis()
            || self
                .profile
                .as_ref()
                .is_none_or(|p| p.access_mode != AccessMode::ReadWrite)
            || !self.browse_result
            || self.edit_token.is_none()
        {
            self.status = UiMessage::key("当前 Redis 资源没有可用的编辑基线");
            return;
        }
        if !self.tabs[self.active].pending.is_null() && !self.tabs[self.active].pending_live {
            self.status = UiMessage::key("请先恢复或丢弃已保存的编辑意图");
            return;
        }
        let Some(kind) = self
            .schema
            .as_ref()
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
        else {
            self.status = UiMessage::key("缺少 Redis 类型，请重新打开资源");
            return;
        };
        let Some(grid) = self.grid.as_ref() else {
            self.status = UiMessage::key("没有 Redis 结果");
            return;
        };
        let Some(cell) = grid.focus else {
            self.status = UiMessage::key("请先选择一行");
            return;
        };
        let large = (0..grid.page.result.columns.len())
            .map(|column| Cell {
                row: cell.row,
                column,
            })
            .find(|candidate| matches!(grid.value(*candidate), Some(DataValue::Large { .. })));
        if let Some(large) = large {
            self.workbench.redis_loading = Some(cell);
            if let Some(grid) = &mut self.grid {
                grid.focus = Some(large);
            }
            self.open_cell(backend);
            if self.large.is_none() {
                self.workbench.redis_loading = None;
            }
            return;
        }
        let get = |column| {
            grid.value(Cell {
                row: cell.row,
                column,
            })
            .ok_or_else(|| UiMessage::key("Redis 行缺少字段"))
            .and_then(redis_field)
        };
        let parsed = (|| -> std::result::Result<_, UiMessage> {
            Ok(match kind {
                "string" => ("set_string", vec![("redis_value", get(0)?)], None),
                "hash" => {
                    let field = get(0)?;
                    (
                        "hash_set",
                        vec![("redis_field", field.clone()), ("redis_value", get(1)?)],
                        Some(("field", field)),
                    )
                }
                "list" => {
                    let index = get(0)?;
                    (
                        "list_set",
                        vec![("redis_index", index.clone()), ("redis_value", get(1)?)],
                        Some(("index", index)),
                    )
                }
                "set" => {
                    let member = get(0)?;
                    (
                        "set_add",
                        vec![("redis_member", member.clone())],
                        Some(("member", member)),
                    )
                }
                "zset" => {
                    let member = get(0)?;
                    (
                        "zset_set",
                        vec![("redis_member", member.clone()), ("redis_score", get(1)?)],
                        Some(("member", member)),
                    )
                }
                _ => return Err(UiMessage::key("当前 Redis 类型不能编辑行")),
            })
        })();
        match parsed {
            Ok((operation, fields, identity)) => {
                self.field_set("operation", operation);
                for (name, value) in fields {
                    self.field_set(name, value);
                }
                self.workbench.redis_identity = identity.map(|(name, value)| (name.into(), value));
                self.workbench.overlay = Some("redis".into());
                let field = if operation == "hash_set" {
                    "redis_field"
                } else if matches!(operation, "set_add" | "zset_set") {
                    "redis_member"
                } else {
                    "redis_value"
                };
                self.focus = field.into();
                self.workbench.focus_request = Some(format!("input-{field}"));
            }
            Err(error) => self.status = error,
        }
    }

    pub(super) fn workbench_action(&mut self, name: &str, backend: &Backend) -> bool {
        if name == "connection-enable-tls" {
            if self.workbench.connection_tls_required
                && let Some(id) = self.profile.as_ref().map(|profile| profile.id)
            {
                self.edit_profile_after_close(id, true, backend);
            }
            return true;
        }
        if name == "delete-rows-confirm" {
            self.confirm_delete_rows();
            return true;
        }
        if name == "delete-rows-request" {
            if let Some(grid) = self.grid.as_mut()
                && grid.selected_row_count() == 0
                && let Some(cell) = grid.focus
                && let Err(error) = grid.select_row(cell.row, cell.column, false, false)
            {
                self.status = UiMessage::local(error.message);
                return true;
            }
            self.request_delete_rows();
            return true;
        }
        if self.sql_form_action(name, backend) {
            return true;
        }
        if name == "discard" {
            self.workbench.redis_undo_groups.clear();
        }
        if let Some(policy) = name.strip_prefix("redis-ttl:") {
            self.workbench.ttl_policy = match policy {
                "keep" => RedisTtlPolicy::Keep,
                "set" => RedisTtlPolicy::Set,
                "persist" => RedisTtlPolicy::Persist,
                _ => return true,
            };
            return true;
        }
        if let Some(operation) = name.strip_prefix("redis-op:") {
            if self.field("operation") != operation {
                self.workbench.redis_identity = None;
            }
            if matches!(operation, "list_set" | "list_insert" | "list_delete") {
                let index = self.grid.as_ref().and_then(|grid| {
                    grid.focus
                        .and_then(|cell| {
                            grid.value(Cell {
                                row: cell.row,
                                column: 0,
                            })
                        })
                        .and_then(|value| match value {
                            DataValue::Integer(index) => Some(index.clone()),
                            _ => None,
                        })
                        .or_else(|| {
                            (operation == "list_insert" && grid.page.result.row_count == 0)
                                .then(|| "0".into())
                        })
                });
                let Some(index) = index else {
                    self.status = UiMessage::key("请先选择 List 元素");
                    return true;
                };
                self.field_set("redis_index", index.clone());
                self.workbench.redis_identity = Some(("index".into(), index));
            }
            self.field_set("operation", operation);
            if operation == "set_ttl" {
                self.workbench.ttl_policy = RedisTtlPolicy::Keep;
            }
            self.workbench.overlay = Some("redis".into());
            return true;
        }
        if let Some((key, open)) = name
            .strip_prefix("select-open:")
            .and_then(|value| value.rsplit_once(':'))
            && let Ok(open) = open.parse::<bool>()
        {
            if open {
                self.workbench.generic_select = Some(key.to_owned());
            } else if self.workbench.generic_select.as_deref() == Some(key) {
                self.workbench.generic_select = None;
            }
            return true;
        }
        if let Some(open) = name
            .strip_prefix("key-type-menu-open:")
            .and_then(|value| value.parse::<bool>().ok())
        {
            self.workbench.key_type_menu = open;
            return true;
        }
        if let Some(kind) = name.strip_prefix("key-type:") {
            if self.is_redis() && matches!(kind, "" | "string" | "hash" | "list" | "set" | "zset") {
                self.field_set("type", kind);
                self.changed_field("type");
                self.workbench.key_type_menu = false;
                if self.session.is_some() {
                    self.action("list", backend);
                }
            }
            return true;
        }
        if let Some(rest) = name.strip_prefix("field:") {
            if let Some((field, value)) = rest.split_once(':')
                && matches!(field, "provider" | "type" | "access" | "tls" | "format")
            {
                if field == "provider" {
                    if self.connection_form_busy() {
                        return true;
                    }
                    self.focus = format!("provider-{value}");
                    self.workbench.focus_request = Some(format!("input-provider-{value}"));
                    // 再次选择当前类型只定位卡片，不重置自定义端口或数据库范围。
                    if self.field("provider") == value {
                        return true;
                    }
                    self.field_set("port", if value == "redis" { "6379" } else { "3306" });
                }
                self.field_set(field, value);
                self.changed_field(field);
                self.workbench.generic_select = None;
            }
            return true;
        }
        if let Some(section) = name.strip_prefix("section:") {
            if matches!(section, "data" | "structure" | "ddl") && self.finish_inline(true) {
                self.workbench.section = section.into();
            }
            return true;
        }
        if let Some(open) = name
            .strip_prefix("cell-fields-open:")
            .and_then(|value| value.parse::<bool>().ok())
        {
            self.workbench.cell_fields_open = open;
            return true;
        }
        if let Some(column) = name.strip_prefix("cell-field:") {
            if let (Some(cell), Ok(column)) = (self.detail_cell(), column.parse::<usize>()) {
                self.open_cell_at(
                    Cell {
                        row: cell.row,
                        column,
                    },
                    backend,
                );
            }
            return true;
        }
        if let Some(overlay) = name.strip_prefix("overlay:") {
            if matches!(
                overlay,
                "profile"
                    | "import"
                    | "export"
                    | "transfer"
                    | "cell"
                    | "redis"
                    | "columns"
                    | "filters"
                    | "row"
                    | "preview"
                    | "file"
                    | "diagnostics"
                    | "shortcuts"
            ) && self.finish_inline(true)
            {
                if overlay == "cell" {
                    self.open_cell(backend);
                    return true;
                }
                self.workbench.redis_identity = None;
                if overlay == "row" || overlay == "filters" {
                    self.init_sql_form(overlay);
                }
                self.workbench.overlay = Some(overlay.into());
                if overlay == "diagnostics" {
                    self.request_diagnostic_task(backend);
                }
                if overlay == "preview" && self.paste_preview.is_none() {
                    self.request_change_preview(0, backend);
                }
                let field = match overlay {
                    "profile" => Some("name"),
                    "import" | "export" | "transfer" | "file" => Some("path"),
                    "cell" => Some("cell"),
                    "redis" => Some("redis_value"),
                    "columns" => Some("column"),
                    _ => None,
                };
                if let Some(field) = field {
                    self.focus = field.into();
                    self.workbench.focus_request = Some(format!("input-{field}"));
                }
            }
            return true;
        }
        if let Some(format) = name.strip_prefix("format:") {
            if matches!(format, "text" | "json" | "hex") {
                self.workbench.value_format = format.into();
            }
            return true;
        }
        if let Some(rest) = name.strip_prefix("cli-open:") {
            let Some((row, column)) = rest.split_once(':') else {
                return true;
            };
            let (Ok(row), Ok(column)) = (row.parse::<u64>(), column.parse::<usize>()) else {
                return true;
            };
            let cell = Cell { row, column };
            let Some(value) = self
                .workbench
                .cli_grid
                .as_ref()
                .and_then(|grid| grid.value(cell))
                .cloned()
            else {
                return true;
            };
            self.workbench_clear_cli_value();
            self.workbench.cli_cell = Some(cell);
            self.workbench.overlay = Some("cli-value".into());
            if let DataValue::Large { id, .. } = value {
                self.workbench.cli_large = Some(LargeLoad {
                    cell,
                    id,
                    offset: 0,
                    bytes: Vec::new(),
                });
                self.workbench_cli_value(backend);
            }
            return true;
        }
        if let Some(index) = name.strip_prefix("cli-result:") {
            if let Some(id) = index
                .parse::<usize>()
                .ok()
                .and_then(|i| self.workbench.cli_results.get(i))
                .cloned()
            {
                self.workbench.cli_result_id = Some(id);
                self.workbench.cli_grid = None;
                self.workbench.cli_page_history.clear();
                self.workbench_cli_page(backend, 0);
            }
            return true;
        }
        if let Some(index) = name.strip_prefix("history-select:") {
            if let Ok(index) = index.parse::<usize>()
                && index < self.workbench.history.len()
            {
                self.workbench.history_selected = Some(index);
            }
            return true;
        }
        if let Some(index) = name.strip_prefix("history-open:") {
            let Ok(index) = index.parse::<usize>() else {
                return true;
            };
            let Some(text) = self
                .workbench
                .history
                .get(index)
                .and_then(|item| item.get("text"))
                .and_then(Value::as_str)
                .map(str::to_owned)
            else {
                self.status = UiMessage::key("历史记录没有 SQL 文本");
                return true;
            };
            if self.is_redis() {
                self.field_set("cli_text", text);
                self.workbench.history_selected = Some(index);
                self.workbench.overlay = None;
                self.workbench.cli = true;
                self.view = "data".into();
                self.focus = "cli_text".into();
                self.workbench.focus_request = Some("input-cli_text".into());
                self.dirty = true;
                return true;
            }
            if !self.finish_inline(true) {
                return true;
            }
            if self.has_live_drafts() || !self.tabs[self.active].pending.is_null() {
                self.status = UiMessage::key("请先提交或丢弃当前草稿");
                return true;
            }
            if self.task.is_some() || self.operation_pending {
                self.status = UiMessage::key("请等待或取消当前任务");
                return true;
            }
            if self.browse_result && self.target.is_some() {
                // 表 tab 保留结果和编辑基线，历史 SQL 使用独立查询会话。
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
            self.workbench.history_selected = self.workbench.history.get(index).map(|_| index);
            self.workbench.overlay = None;
            self.view = "query".into();
            self.focus = "query".into();
            self.workbench.focus_request = Some("query-editor".into());
            self.dirty = true;
            return true;
        }
        if let Some(prefix) = name.strip_prefix("redis-tree:") {
            if let Some(profile) = self.profile.as_ref() {
                self.workbench
                    .redis_expansion
                    .toggle(profile.id, profile.redis_db, prefix);
            }
            self.normalize_key_page();
            self.redis_prune_key_selection();
            return true;
        }
        match name {
            "navigation-toggle" => {
                if self.viewport_class() == "bp-small" {
                    self.navigation.narrow_expanded = !self.navigation.narrow_expanded;
                } else {
                    self.navigation.expanded = !self.navigation.expanded;
                    self.dirty = true;
                }
            }
            "profile-tls-toggle" => {
                self.workbench.profile_tls_details = !self.workbench.profile_tls_details;
            }
            "transfer-advanced-toggle" => {
                self.workbench.transfer_advanced = !self.workbench.transfer_advanced;
            }
            "keys-previous" | "keys-next" => {
                let last = self.visible_key_count().saturating_sub(1) / KEY_PAGE_SIZE;
                let current = self.workbench.key_page.min(last);
                self.workbench.key_page = if name == "keys-previous" {
                    current.saturating_sub(1)
                } else {
                    current.saturating_add(1).min(last)
                };
                self.redis_prune_key_selection();
            }
            "cli-history-toggle" => {
                self.workbench.cli_history_expanded = !self.workbench.cli_history_expanded;
                if self.workbench.cli_history_expanded {
                    self.request(
                        backend,
                        "history.list",
                        "history.list",
                        json!({"workspace_id":self.workspace_id}),
                    );
                }
            }
            "cli-toggle" => {
                self.workbench.cli = !self.workbench.cli;
                self.dirty = true;
                if self.workbench.cli {
                    self.focus = "cli_text".into();
                    self.workbench.focus_request = Some("input-cli_text".into());
                } else if self.focus == "cli_text" {
                    self.focus = "grid".into();
                    self.workbench.focus_request = Some(String::new());
                }
            }
            "view-query" => {
                if self.finish_inline(true) {
                    self.open_query_tab(backend);
                }
            }
            "cli-run" => {
                if !self.is_redis() {
                    self.status = UiMessage::key("Redis CLI 需要 Redis 连接");
                    return true;
                }
                if self.task.is_some() || self.operation_pending || self.workbench.cli_running {
                    self.status = UiMessage::key("该 tab 已有正在执行的任务");
                    return true;
                }
                if self.has_live_drafts() || !self.tabs[self.active].pending.is_null() {
                    self.status = UiMessage::key("请先提交或丢弃当前草稿");
                    return true;
                }
                let Some(session) = self.session_id() else {
                    self.status = UiMessage::key("请先连接数据库");
                    return true;
                };
                let text = self.field("cli_text").to_owned();
                if text.trim().is_empty() {
                    self.status = UiMessage::key("请输入 Redis 命令");
                    return true;
                }
                self.request(
                    backend,
                    "cli.start",
                    "execution.start",
                    json!({"session_id":session,"workspace_id":self.workspace_id,"text":text}),
                );
                if self.operation_pending {
                    self.workbench_clear_cli_value();
                    self.workbench.cli_command = Some(text);
                    self.workbench.cli_running = true;
                    self.workbench.cli_started_at = Some(Instant::now());
                    self.workbench.cli_elapsed = None;
                    self.workbench.cli_error.clear();
                    self.workbench.cli_results.clear();
                    self.workbench.cli_grid = None;
                    self.workbench.cli_result_id = None;
                    self.workbench.cli_page_history.clear();
                    self.workbench.cli_page_token = None;
                }
            }
            "cli-next" => {
                if self.workbench.cli_page_token.is_none()
                    && let Some(grid) = self.workbench.cli_grid.as_ref()
                    && let Some(next) = grid.page.next
                {
                    let start = grid.page.start;
                    if self.workbench_cli_page(backend, next) {
                        self.workbench.cli_page_transition = Some((true, start));
                    }
                }
            }
            "cli-previous" => {
                if self.workbench.cli_page_token.is_none()
                    && let Some(&start) = self.workbench.cli_page_history.last()
                    && self.workbench_cli_page(backend, start)
                {
                    self.workbench.cli_page_transition = Some((false, start));
                }
            }
            "tree-toggle" => {
                self.workbench.tree = !self.workbench.tree;
                self.workbench.key_page = 0;
                self.redis_prune_key_selection();
            }
            "overlay-close" => {
                if self.workbench.overlay.as_deref() == Some("profile")
                    && self.connection_form_busy()
                {
                    return true;
                }
                if self.workbench.overlay.as_deref() == Some("redis-key-delete") {
                    self.redis_selection_action(backend, "redis-key-delete-cancel");
                }
                self.cancel_redis_row_delete();
                self.workbench_clear_cli_value();
                self.workbench.overlay = None;
                self.workbench.field_error = None;
                self.workbench.delete_rows = None;
                self.workbench.redis_identity = None;
            }
            "inline-begin" => {
                self.begin_inline(backend);
            }
            "inline-commit" => {
                self.finish_inline(true);
            }
            "inline-cancel" => {
                self.finish_inline(false);
            }
            "redis-edit" => self.workbench_redis_edit(backend),
            "redis-undo" => {
                let len = self.redis_changes.len();
                if let Some(&(start, end)) = self.workbench.redis_undo_groups.last()
                    && end == len
                {
                    self.redis_changes.truncate(start);
                    self.redis_changes_revision = self.redis_changes_revision.wrapping_add(1);
                    self.workbench.redis_undo_groups.pop();
                    self.detail =
                        serde_json::to_string_pretty(&self.redis_changes).unwrap_or_default();
                    self.set_detail_context("草稿详情");
                    self.dirty = true;
                    return true;
                }
                self.workbench
                    .redis_undo_groups
                    .retain(|&(_, end)| end < len);
                return false;
            }
            "redis-stage" => {
                if self
                    .tabs
                    .get(self.active)
                    .is_some_and(|tab| self.redis_key_delete_tab_pending(tab.id))
                {
                    self.status = UiMessage::key("请先提交或丢弃当前草稿");
                    return true;
                }
                if !self.is_redis()
                    || self
                        .profile
                        .as_ref()
                        .is_none_or(|p| p.access_mode != AccessMode::ReadWrite)
                    || !self.browse_result
                    || self.edit_token.is_none()
                {
                    self.status = UiMessage::key("编辑基线已失效，请重新打开资源");
                    return true;
                }
                if !self.tabs[self.active].pending.is_null() && !self.tabs[self.active].pending_live
                {
                    self.status = UiMessage::key("请先恢复或丢弃已保存的编辑意图");
                    return true;
                }
                let operation = self.field("operation").to_owned();
                if operation == "set_ttl" {
                    match self.workbench.ttl_policy {
                        RedisTtlPolicy::Keep => {
                            self.workbench.overlay = None;
                            self.status = UiMessage::key("已保留现有 TTL");
                            return true;
                        }
                        RedisTtlPolicy::Persist => {}
                        RedisTtlPolicy::Set if self.field("redis_ttl").parse::<u64>().is_err() => {
                            self.field_error("redis_ttl", UiMessage::key("请输入有效 TTL（毫秒）"));
                            return true;
                        }
                        RedisTtlPolicy::Set => {}
                    }
                }
                if operation == "zset_set" {
                    let raw = self.field("redis_score");
                    let valid = raw.parse::<f64>().is_ok_and(|score| {
                        score.is_finite()
                            || (score.is_infinite()
                                && matches!(
                                    raw.to_ascii_lowercase().as_str(),
                                    "inf" | "+inf" | "-inf"
                                ))
                    });
                    if !valid {
                        self.field_error("redis_score", UiMessage::key("无效 score"));
                        return true;
                    }
                }
                let fields: &[&str] = match operation.as_str() {
                    "set_string" | "list_set" | "list_insert" => &["redis_value"],
                    "hash_set" => &["redis_field", "redis_value"],
                    "hash_delete" => &["redis_field"],
                    "set_add" | "set_remove" | "zset_set" | "zset_remove" => &["redis_member"],
                    "rename" => &["redis_key"],
                    _ => &[],
                };
                for &field in fields {
                    if redis_bytes(self.field(field)).is_err() {
                        self.field_error(field, UiMessage::key("无效 Base64 编码"));
                        return true;
                    }
                }
                let mut change = match self.redis_operation() {
                    Ok(change) => change,
                    Err(error) => {
                        self.status = error;
                        return true;
                    }
                };
                let start = self.redis_changes.len();
                self.workbench
                    .redis_undo_groups
                    .retain(|&(_, end)| end <= start);
                if let Some((field, original)) = self.workbench.redis_identity.clone() {
                    if field == "index" {
                        if !matches!(
                            operation.as_str(),
                            "list_set" | "list_insert" | "list_delete"
                        ) || self.field("redis_index") != original
                        {
                            self.status = UiMessage::key("List 索引不可修改，请重新选择行");
                            return true;
                        }
                        let original = match original.parse::<i64>() {
                            Ok(index) => index,
                            Err(error) => {
                                self.status = UiMessage::raw(error.to_string());
                                return true;
                            }
                        };
                        match self.redis_list_index(original, operation == "list_insert") {
                            Ok(index) => change["index"] = json!(index),
                            Err(error) => {
                                self.status = error;
                                return true;
                            }
                        }
                    } else {
                        let current = if field == "field" {
                            self.field("redis_field")
                        } else {
                            self.field("redis_member")
                        };
                        let old = match redis_bytes(&original) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                self.status = UiMessage::raw(error);
                                return true;
                            }
                        };
                        let new = match redis_bytes(current) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                self.status = UiMessage::raw(error);
                                return true;
                            }
                        };
                        if old != new {
                            let remove = match operation.as_str() {
                                "hash_set" => "hash_delete",
                                "set_add" => "set_remove",
                                "zset_set" => "zset_remove",
                                _ => {
                                    self.status =
                                        UiMessage::key("Redis 编辑操作已改变，请重新选择行");
                                    return true;
                                }
                            };
                            let mut removal = json!({"operation":remove});
                            removal[field.as_str()] =
                                json!({"type":"bytes", "value": STANDARD.encode(old)});
                            self.redis_changes.push(removal);
                        }
                    }
                }
                self.redis_changes.push(change);
                self.redis_changes_revision = self.redis_changes_revision.wrapping_add(1);
                self.workbench
                    .redis_undo_groups
                    .push((start, self.redis_changes.len()));
                self.detail = serde_json::to_string_pretty(&self.redis_changes).unwrap_or_default();
                self.set_detail_context("草稿详情");
                self.dirty = true;
                self.workbench.redis_identity = None;
                self.workbench.overlay = None;
            }
            _ => return false,
        }
        true
    }

    fn workbench_clear_cli_value(&mut self) {
        self.workbench.cli_large = None;
        self.workbench.cli_value_token = None;
        self.workbench.cli_cell = None;
        if self.workbench.overlay.as_deref() == Some("cli-value") {
            self.workbench.overlay = None;
        }
    }

    fn workbench_cli_value(&mut self, backend: &Backend) {
        let Some(load) = self.workbench.cli_large.as_ref() else {
            return;
        };
        let (id, offset) = (load.id, load.offset);
        let token = Uuid::new_v4();
        self.workbench.cli_value_token = Some(token);
        self.request(
            backend,
            &format!("cli.value/{id}/{offset}/{token}"),
            "result.value",
            json!({"value_id":id,"offset":offset,"max_bytes":262144}),
        );
    }

    fn workbench_cli_error(&mut self, error: UiMessage) {
        self.workbench.cli_error.push_line(error);
    }

    pub(super) fn workbench_error(&mut self, tag: &str, error: &AppError) -> bool {
        if tag == "cli.start" || (tag == "execution.get" && self.workbench.cli_running) {
            self.workbench.cli_elapsed = self
                .workbench
                .cli_started_at
                .take()
                .map(|start| start.elapsed());
        }
        let matched = if tag == "cli.start" {
            if !self.workbench.cli_running {
                return true;
            }
            self.workbench.cli_running = false;
            self.task = None;
            self.operation_pending = false;
            true
        } else if tag == "execution.get" && self.workbench.cli_running {
            self.workbench.cli_running = false;
            self.task = None;
            self.operation_pending = false;
            true
        } else if let Some(token) = tag.strip_prefix("cli.page/") {
            if Uuid::parse_str(token).ok() != self.workbench.cli_page_token
                || self.workbench.cli_page_token.is_none()
            {
                return true;
            }
            self.workbench.cli_page_token = None;
            self.workbench.cli_page_transition = None;
            true
        } else if tag.starts_with("cli.value/") {
            let expected = self
                .workbench
                .cli_large
                .as_ref()
                .zip(self.workbench.cli_value_token)
                .map(|(load, token)| format!("cli.value/{}/{}/{token}", load.id, load.offset));
            if expected.as_deref() != Some(tag) {
                return true;
            }
            self.workbench.cli_large = None;
            self.workbench.cli_value_token = None;
            true
        } else if tag.starts_with("cli.") {
            return true;
        } else {
            false
        };
        if matched {
            self.status = UiMessage::raw(error.to_string());
            self.workbench_cli_error(UiMessage::raw(error.to_string()));
        }
        matched
    }

    fn workbench_cli_page(&mut self, backend: &Backend, start: u64) -> bool {
        let Some(id) = self.workbench.cli_result_id.clone() else {
            return false;
        };
        self.workbench_clear_cli_value();
        self.workbench.cli_page_transition = None;
        let token = Uuid::new_v4();
        let tag = format!(
            "{}:{}:cli.page/{token}",
            self.tabs[self.active].id, self.generation
        );
        match backend.request(
            tag,
            "result.page",
            json!({"result_id":id,"start":start,"max_rows":200,"max_bytes":1048576}),
        ) {
            Ok(()) => {
                self.workbench.cli_page_token = Some(token);
                true
            }
            Err(error) => {
                self.workbench.cli_page_token = None;
                self.status = UiMessage::raw(error.to_string());
                false
            }
        }
    }

    /// CLI 结果有独立分页状态，不能替换当前 key 的结果与编辑基线。
    pub(super) fn workbench_response(
        &mut self,
        tag: &str,
        value: Value,
        backend: &Backend,
    ) -> bool {
        if tag.starts_with("cli.value/") {
            let Some(mut load) = self.workbench.cli_large.take() else {
                return true;
            };
            let expected = self
                .workbench
                .cli_value_token
                .map(|token| format!("cli.value/{}/{}/{token}", load.id, load.offset));
            if expected.as_deref() != Some(tag) {
                self.workbench.cli_large = Some(load);
                return true;
            }
            self.workbench.cli_value_token = None;
            match load.consume(&value) {
                Ok(None) => {
                    self.workbench.cli_large = Some(load);
                    self.workbench_cli_value(backend);
                }
                Ok(Some(full)) => {
                    let result = self
                        .workbench
                        .cli_grid
                        .as_mut()
                        .ok_or_else(|| UiMessage::key("CLI 结果已关闭"))
                        .and_then(|grid| grid.resolve_large(load.cell, full).map_err(grid_error));
                    if let Err(error) = result {
                        self.workbench_cli_error(error);
                    }
                }
                Err(error) => self.workbench_cli_error(UiMessage::raw(error.message)),
            }
            return true;
        }
        if tag == "cli.start" {
            self.operation_pending = false;
            self.task = value
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if self.task.is_none() {
                self.workbench.cli_running = false;
                self.workbench.cli_elapsed = self
                    .workbench
                    .cli_started_at
                    .take()
                    .map(|start| start.elapsed());
                self.workbench.cli_error = UiMessage::key("CLI 响应缺少任务 ID");
            } else {
                self.status = UiMessage::key("Redis 命令已提交");
                self.last_poll = Instant::now() - Duration::from_secs(2);
            }
            return true;
        }
        if tag == "execution.get" && self.workbench.cli_running {
            let state = value.get("state").and_then(Value::as_str).unwrap_or("");
            if matches!(state, "queued" | "running" | "cancelling") {
                return true;
            }
            self.task = None;
            self.workbench.cli_running = false;
            self.workbench.cli_elapsed = self
                .workbench
                .cli_started_at
                .take()
                .map(|start| start.elapsed());
            let state_label = match state {
                "succeeded" => UiArg::Key("已完成"),
                "failed" => UiArg::Key("失败"),
                "cancelled" => UiArg::Key("已取消"),
                "outcome_unknown" => UiArg::Key("结果待确认"),
                _ => UiArg::Raw(state.to_owned()),
            };
            self.status = UiMessage::template(
                "Redis 命令任务 {state}",
                vec![("state", state_label.clone())],
            );
            if state != "succeeded" {
                self.workbench.cli_error = value
                    .get("error")
                    .filter(|v| !v.is_null())
                    .map(|v| {
                        UiMessage::raw(
                            v.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| v.to_string()),
                        )
                    })
                    .unwrap_or_else(|| {
                        UiMessage::template("命令任务 {state}", vec![("state", state_label)])
                    });
            }
            self.workbench.cli_results = value
                .get("results")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            self.workbench.cli_result_id = self.workbench.cli_results.first().cloned();
            self.workbench.cli_page_history.clear();
            self.workbench_cli_page(backend, 0);
            return true;
        }
        if let Some(token) = tag.strip_prefix("cli.page/") {
            if self.workbench.cli_page_token.is_none()
                || Uuid::parse_str(token).ok() != self.workbench.cli_page_token
            {
                return true;
            }
            self.workbench.cli_page_token = None;
            let transition = self.workbench.cli_page_transition.take();
            match serde_json::from_value::<ResultPage>(value) {
                Ok(page)
                    if self.workbench.cli_result_id.as_deref()
                        == Some(page.result.id.to_string().as_str()) =>
                {
                    if let Some((forward, start)) = transition {
                        if forward {
                            self.workbench.cli_page_history.push(start);
                        } else {
                            self.workbench.cli_page_history.pop();
                        }
                    }
                    self.workbench.cli_grid = Some(GridState::new(page, false));
                }
                Ok(_) => self.workbench_cli_error(UiMessage::key("CLI 结果页与当前结果不匹配")),
                Err(error) => self.workbench_cli_error(UiMessage::template(
                    "CLI 结果页无效: {error}",
                    vec![("error", UiArg::Raw(error.to_string()))],
                )),
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
#[path = "../tests/unit/workbench.rs"]
mod tests;
