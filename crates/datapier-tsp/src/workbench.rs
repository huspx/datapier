use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::{AccessMode, ProviderKind, Value as DataValue};

pub(super) const KEY_PAGE_SIZE: usize = 200;

#[derive(Clone)]
pub(super) struct RedisDraftPreview {
    pub start: usize,
    pub end: usize,
    pub before: Value,
}

/// 工作台的临时界面状态；数据库基线和草稿仍由 App 管理。
#[derive(Clone)]
pub(super) struct Workbench {
    pub dark: bool,
    pub compact_override: Option<bool>,
    pub section: String,
    pub overlay: Option<String>,
    pub cli: bool,
    pub tree: bool,
    pub cli_grid: Option<GridState>,
    pub cli_cell: Option<Cell>,
    pub cli_large: Option<LargeLoad>,
    pub(super) cli_value_token: Option<Uuid>,
    pub key_page: usize,
    pub cli_results: Vec<String>,
    pub cli_result_id: Option<String>,
    pub cli_running: bool,
    pub cli_error: String,
    cli_page_token: Option<Uuid>,
    cli_page_history: Vec<u64>,
    pub value_format: String,
    pub history: Vec<Value>,
    pub history_selected: Option<usize>,
    pub scan_dirty: bool,
    pub inline: Option<Cell>,
    pub load_resource: bool,
    pub focus_request: Option<String>,
    // 保留 Redis 基线中的原始身份，避免将成员改名误当作新增。
    redis_identity: Option<(String, String)>,
    pub queued_target: Option<ResourceTarget>,
    pub redis_loading: Option<Cell>,
    redis_undo_groups: Vec<(usize, usize)>,
    pub redis_previews: Vec<RedisDraftPreview>,
    redis_edit_before: Value,
}

impl Default for Workbench {
    fn default() -> Self {
        Self {
            dark: true,
            compact_override: None,
            section: "data".into(),
            overlay: None,
            cli: false,
            cli_grid: None,
            cli_cell: None,
            cli_large: None,
            cli_value_token: None,
            key_page: 0,
            cli_results: Vec::new(),
            cli_result_id: None,
            cli_running: false,
            cli_error: String::new(),
            cli_page_token: None,
            cli_page_history: Vec::new(),
            tree: false,
            scan_dirty: false,
            value_format: "text".into(),
            history: Vec::new(),
            history_selected: None,
            inline: None,
            load_resource: false,
            focus_request: None,
            queued_target: None,
            redis_loading: None,
            redis_undo_groups: Vec::new(),
            redis_identity: None,
            redis_previews: Vec::new(),
            redis_edit_before: Value::Null,
        }
    }
}

// UTF-8 内容直接展示；Base64 前缀和非 UTF-8 内容保留可逆字节表示。
fn redis_field(value: &DataValue) -> std::result::Result<String, String> {
    match value {
        DataValue::Bytes(encoded) => {
            let bytes = STANDARD.decode(encoded).map_err(|e| e.to_string())?;
            match String::from_utf8(bytes) {
                Ok(text) if !text.starts_with("base64:") => Ok(text),
                _ => Ok(format!("base64:{encoded}")),
            }
        }
        DataValue::Integer(value) | DataValue::Float(value) => Ok(value.clone()),
        DataValue::Large { .. } => Err("请先读取完整 Redis 值".into()),
        _ => Err("Redis 结果类型与资源类型不匹配".into()),
    }
}

fn redis_bytes(value: &str) -> std::result::Result<Vec<u8>, String> {
    match value.strip_prefix("base64:") {
        Some(encoded) => STANDARD.decode(encoded).map_err(|e| e.to_string()),
        None => Ok(value.as_bytes().to_vec()),
    }
}

impl App {
    pub(super) fn clear_redis_previews(&mut self) {
        self.workbench.redis_previews.clear();
        self.workbench.redis_undo_groups.clear();
        self.workbench.redis_edit_before = Value::Null;
        self.workbench.redis_identity = None;
    }

    // 已排队的操作使用执行时索引，界面行始终指向读取时的原始索引。
    fn redis_list_index(&self, original: i64, insert: bool) -> std::result::Result<i64, String> {
        let mut len = self
            .grid
            .as_ref()
            .map(|g| g.page.result.row_count as i64)
            .ok_or_else(|| "没有 Redis 结果".to_string())?;
        let mut position = if original < 0 {
            len + original
        } else {
            original
        };
        if position < 0 || position > len || (position == len && !insert) {
            return Err("List 原始索引超出范围".into());
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
                .ok_or_else(|| "已暂存 List 索引无效".to_string())?;
            let index = if index < 0 { len + index } else { index };
            match op {
                "list_insert" => {
                    if index < 0 || index > len {
                        return Err("已暂存 List 索引超出范围".into());
                    }
                    if index <= position {
                        position += 1;
                    }
                    len += 1;
                }
                "list_delete" => {
                    if index < 0 || index >= len {
                        return Err("已暂存 List 索引超出范围".into());
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
            return Err("选中的原始 List 元素已暂存删除".into());
        }
        Ok(position)
    }

    fn redis_selected_before(&self) -> Value {
        let Some(grid) = self.grid.as_ref() else {
            return Value::Null;
        };
        let Some(cell) = grid.focus else {
            return Value::Null;
        };
        let Some(kind) = self
            .schema
            .as_ref()
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
        else {
            return Value::Null;
        };
        let names: &[&str] = match kind {
            "string" => &["value"],
            "hash" => &["field", "value"],
            "list" => &["index", "value"],
            "set" => &["member"],
            "zset" => &["member", "score"],
            _ => return Value::Null,
        };
        let mut fields = serde_json::Map::new();
        for (column, name) in names.iter().enumerate() {
            let Some(value) = grid
                .value(Cell {
                    row: cell.row,
                    column,
                })
                .and_then(|v| redis_field(v).ok())
            else {
                return Value::Null;
            };
            fields.insert((*name).into(), Value::String(value));
        }
        Value::Object(fields)
    }

    pub(super) fn is_redis(&self) -> bool {
        self.profile
            .as_ref()
            .is_some_and(|p| p.provider == ProviderKind::Redis)
    }

    pub(super) fn navigation_compact(&self) -> bool {
        self.workbench
            .compact_override
            .unwrap_or_else(|| self.is_redis())
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

    pub(super) fn begin_inline(&mut self, backend: &Backend) {
        if self.workbench.inline.is_some() {
            return;
        }
        if !self.tabs[self.active].pending.is_null() && !self.tabs[self.active].pending_live {
            self.status = "请先恢复或丢弃已保存的编辑意图".into();
            return;
        }
        if !self.workbench_can_edit() {
            self.status = "当前结果没有可用的编辑基线或可靠键".into();
            return;
        }
        let Some(cell) = self.grid.as_ref().and_then(|g| g.focus) else {
            self.status = "没有选中单元格".into();
            return;
        };
        let grid = self.grid.as_ref().unwrap();
        if grid
            .page
            .result
            .columns
            .get(cell.column)
            .is_none_or(|c| c.generated)
        {
            self.status = "该列不可编辑".into();
            return;
        }
        let large = matches!(grid.value(cell), Some(DataValue::Large { .. }));
        if !large {
            match grid.cell_edit_text(cell) {
                Ok(text) => self.field_set("cell", text),
                Err(error) => {
                    self.status = error.to_string();
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
            self.status = "请等待完整字段读取完成".into();
            return false;
        }
        if !self.workbench_can_edit() {
            self.status = "编辑基线已失效，请保留草稿并重新打开资源".into();
            return false;
        }
        let text = self.field("cell").to_owned();
        let result = self
            .grid
            .as_mut()
            .ok_or_else(|| "结果已关闭".to_string())
            .and_then(|grid| {
                if grid.focus != Some(cell) {
                    return Err("编辑目标已改变".into());
                }
                let value = grid
                    .parse_cell_text(cell, &text)
                    .map_err(|e| e.to_string())?;
                grid.edit(cell, value).map_err(|e| e.to_string())
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
                self.status = error;
                false
            }
        }
    }

    pub(super) fn workbench_key(&mut self, key: &tern_sdk::Key, backend: &Backend) -> bool {
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
        if key.is("escape") && self.workbench.overlay.is_some() {
            self.workbench_clear_cli_value();
            self.workbench.overlay = None;
            self.workbench.redis_identity = None;
            return true;
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
        if !self.is_redis()
            || self
                .profile
                .as_ref()
                .is_none_or(|p| p.access_mode != AccessMode::ReadWrite)
            || !self.browse_result
            || self.edit_token.is_none()
        {
            self.status = "当前 Redis 资源没有可用的编辑基线".into();
            return;
        }
        if !self.tabs[self.active].pending.is_null() && !self.tabs[self.active].pending_live {
            self.status = "请先恢复或丢弃已保存的编辑意图".into();
            return;
        }
        let Some(kind) = self
            .schema
            .as_ref()
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
        else {
            self.status = "缺少 Redis 类型，请重新打开资源".into();
            return;
        };
        let Some(grid) = self.grid.as_ref() else {
            self.status = "没有 Redis 结果".into();
            return;
        };
        let Some(cell) = grid.focus else {
            self.status = "请先选择一行".into();
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
            .ok_or_else(|| "Redis 行缺少字段".to_string())
            .and_then(redis_field)
        };
        let parsed = (|| -> std::result::Result<_, String> {
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
                _ => return Err("当前 Redis 类型不能编辑行".into()),
            })
        })();
        match parsed {
            Ok((operation, fields, identity)) => {
                // 编辑器会被用户改写；另存原项用于预览，分页切换后仍保留原值。
                self.workbench.redis_edit_before = Value::Object(
                    fields
                        .iter()
                        .map(|(name, value)| {
                            (
                                name.trim_start_matches("redis_").to_owned(),
                                Value::String(value.clone()),
                            )
                        })
                        .collect(),
                );
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
        if name == "discard" {
            self.workbench.redis_previews.clear();
            self.workbench.redis_edit_before = Value::Null;
            self.workbench.redis_undo_groups.clear();
        }
        if let Some(operation) = name.strip_prefix("redis-op:") {
            if self.field("operation") != operation {
                self.workbench.redis_identity = None;
                self.workbench.redis_edit_before = Value::Null;
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
                    self.status = "请先选择 List 元素".into();
                    return true;
                };
                self.field_set("redis_index", index.clone());
                self.workbench.redis_identity = Some(("index".into(), index));
                self.workbench.redis_edit_before = self.redis_selected_before();
            }
            self.field_set("operation", operation);
            return true;
        }
        if let Some(rest) = name.strip_prefix("field:") {
            if let Some((field, value)) = rest.split_once(':')
                && matches!(field, "provider" | "type" | "access" | "tls")
            {
                if field == "provider" {
                    self.field_set("port", if value == "redis" { "6379" } else { "3306" });
                }
                self.field_set(field, value);
                self.changed_field(field);
            }
            return true;
        }
        if let Some(section) = name.strip_prefix("section:") {
            if matches!(section, "data" | "structure" | "ddl") && self.finish_inline(true) {
                self.workbench.section = section.into();
            }
            return true;
        }
        if let Some(overlay) = name.strip_prefix("overlay:") {
            if matches!(
                overlay,
                "profile"
                    | "transfer"
                    | "cell"
                    | "redis"
                    | "columns"
                    | "filters"
                    | "row"
                    | "preview"
                    | "file"
                    | "diagnostics"
            ) && self.finish_inline(true)
            {
                if overlay == "cell" {
                    self.open_cell(backend);
                    return true;
                }
                self.workbench.redis_identity = None;
                self.workbench.redis_edit_before = Value::Null;
                self.workbench.overlay = Some(overlay.into());
                let field = match overlay {
                    "profile" => Some("name"),
                    "transfer" | "file" => Some("path"),
                    "cell" => Some("cell"),
                    "redis" => Some("redis_value"),
                    "columns" => Some("column"),
                    "filters" => Some("filter"),
                    "row" => Some("row_values"),
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
                self.status = "历史记录没有 SQL 文本".into();
                return true;
            };
            if self.is_redis() {
                if self.task.is_some() || self.operation_pending || self.workbench.cli_running {
                    self.status = "请等待或取消当前任务".into();
                    return true;
                }
                self.workbench_clear_cli_value();
                self.workbench.cli_page_token = None;
                self.workbench.cli_results.clear();
                self.workbench.cli_result_id = None;
                self.workbench.cli_grid = None;
                self.workbench.cli_page_history.clear();
                self.workbench.cli_error.clear();
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
                self.status = "请先提交或丢弃当前草稿".into();
                return true;
            }
            if self.task.is_some() || self.operation_pending {
                self.status = "请等待或取消当前任务".into();
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
        match name {
            "theme-toggle" => self.workbench.dark = !self.workbench.dark,
            "navigation-toggle" => {
                self.workbench.compact_override = Some(!self.navigation_compact())
            }
            "keys-previous" => self.workbench.key_page = self.workbench.key_page.saturating_sub(1),
            "keys-next" => {
                if (self.workbench.key_page + 1).saturating_mul(KEY_PAGE_SIZE)
                    < self.resources.len()
                {
                    self.workbench.key_page += 1;
                }
            }
            "cli-toggle" => {
                self.workbench.cli = !self.workbench.cli;
                if self.workbench.cli {
                    self.focus = "cli_text".into();
                    self.workbench.focus_request = Some("input-cli_text".into());
                }
            }
            "view-query" => {
                if self.finish_inline(true) {
                    self.open_query_tab(backend);
                }
            }
            "cli-run" => {
                if !self.is_redis() {
                    self.status = "Redis CLI 需要 Redis 连接".into();
                    return true;
                }
                if self.task.is_some() || self.operation_pending || self.workbench.cli_running {
                    self.status = "该 tab 已有正在执行的任务".into();
                    return true;
                }
                if self.has_live_drafts() || !self.tabs[self.active].pending.is_null() {
                    self.status = "请先提交或丢弃当前草稿".into();
                    return true;
                }
                let Some(session) = self.session_id() else {
                    self.status = "请先连接数据库".into();
                    return true;
                };
                let text = self.field("cli_text").to_owned();
                if text.trim().is_empty() {
                    self.status = "请输入 Redis 命令".into();
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
                    self.workbench.cli_running = true;
                    self.workbench.cli_error.clear();
                    self.workbench.cli_results.clear();
                    self.workbench.cli_grid = None;
                    self.workbench.cli_result_id = None;
                    self.workbench.cli_page_history.clear();
                    self.workbench.cli_page_token = None;
                }
            }
            "cli-next" => {
                if let Some(grid) = self.workbench.cli_grid.as_ref()
                    && let Some(next) = grid.page.next
                {
                    self.workbench.cli_page_history.push(grid.page.start);
                    self.workbench_cli_page(backend, next);
                }
            }
            "cli-previous" => {
                if let Some(start) = self.workbench.cli_page_history.pop() {
                    self.workbench_cli_page(backend, start);
                }
            }
            "tree-toggle" => self.workbench.tree = !self.workbench.tree,
            "overlay-close" => {
                self.workbench_clear_cli_value();
                self.workbench.overlay = None;
                self.workbench.redis_identity = None;
            }
            "inline-begin" => self.begin_inline(backend),
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
                    self.workbench
                        .redis_previews
                        .retain(|preview| preview.end <= start);
                    self.workbench.redis_undo_groups.pop();
                    self.detail =
                        serde_json::to_string_pretty(&self.redis_changes).unwrap_or_default();
                    self.dirty = true;
                    return true;
                }
                self.workbench
                    .redis_undo_groups
                    .retain(|&(_, end)| end < len);
                return false;
            }
            "redis-stage" => {
                if !self.is_redis()
                    || self
                        .profile
                        .as_ref()
                        .is_none_or(|p| p.access_mode != AccessMode::ReadWrite)
                    || !self.browse_result
                    || self.edit_token.is_none()
                {
                    self.status = "编辑基线已失效，请重新打开资源".into();
                    return true;
                }
                if !self.tabs[self.active].pending.is_null() && !self.tabs[self.active].pending_live
                {
                    self.status = "请先恢复或丢弃已保存的编辑意图".into();
                    return true;
                }
                let operation = self.field("operation").to_owned();
                if operation == "zset_set"
                    && self
                        .field("redis_score")
                        .parse::<f64>()
                        .map_or(true, |score| score.is_nan())
                {
                    self.status = "无效 score".into();
                    return true;
                }
                let mut change = match self.redis_operation() {
                    Ok(change) => change,
                    Err(error) => {
                        self.status = error;
                        return true;
                    }
                };
                if self.workbench.redis_edit_before.is_null()
                    && self.workbench.redis_identity.is_none()
                {
                    let before = self.redis_selected_before();
                    let target_matches = match operation.as_str() {
                        "set_string" => {
                            before.get("value").is_some()
                                && self
                                    .schema
                                    .as_ref()
                                    .and_then(|s| s.get("type"))
                                    .and_then(Value::as_str)
                                    == Some("string")
                        }
                        "hash_set" | "hash_delete" => before
                            .get("field")
                            .and_then(Value::as_str)
                            .is_some_and(|s| {
                                redis_bytes(s).ok() == redis_bytes(self.field("redis_field")).ok()
                            }),
                        "set_add" | "set_remove" | "zset_set" | "zset_remove" => {
                            let kind = self
                                .schema
                                .as_ref()
                                .and_then(|s| s.get("type"))
                                .and_then(Value::as_str);
                            let expected = if operation.starts_with("zset_") {
                                "zset"
                            } else {
                                "set"
                            };
                            kind == Some(expected)
                                && before
                                    .get("member")
                                    .and_then(Value::as_str)
                                    .is_some_and(|s| {
                                        redis_bytes(s).ok()
                                            == redis_bytes(self.field("redis_member")).ok()
                                    })
                        }
                        _ => false,
                    };
                    if target_matches {
                        self.workbench.redis_edit_before = before;
                    }
                }
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
                            self.status = "List 索引不可修改，请重新选择行".into();
                            return true;
                        }
                        let original = match original.parse::<i64>() {
                            Ok(index) => index,
                            Err(error) => {
                                self.status = error.to_string();
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
                                self.status = error;
                                return true;
                            }
                        };
                        let new = match redis_bytes(current) {
                            Ok(bytes) => bytes,
                            Err(error) => {
                                self.status = error;
                                return true;
                            }
                        };
                        if old != new {
                            let remove = match operation.as_str() {
                                "hash_set" => "hash_delete",
                                "set_add" => "set_remove",
                                "zset_set" => "zset_remove",
                                _ => {
                                    self.status = "Redis 编辑操作已改变，请重新选择行".into();
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
                self.workbench.redis_previews.push(RedisDraftPreview {
                    start,
                    end: self.redis_changes.len(),
                    before: std::mem::take(&mut self.workbench.redis_edit_before),
                });
                self.workbench
                    .redis_undo_groups
                    .push((start, self.redis_changes.len()));
                self.detail = serde_json::to_string_pretty(&self.redis_changes).unwrap_or_default();
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

    fn workbench_cli_error(&mut self, error: String) {
        if !self.workbench.cli_error.is_empty() {
            self.workbench.cli_error.push('\n');
        }
        self.workbench.cli_error.push_str(&error);
    }

    pub(super) fn workbench_error(&mut self, tag: &str, error: &AppError) -> bool {
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
            self.status = error.to_string();
            self.workbench_cli_error(error.to_string());
        }
        matched
    }

    fn workbench_cli_page(&mut self, backend: &Backend, start: u64) {
        let Some(id) = self.workbench.cli_result_id.clone() else {
            return;
        };
        self.workbench_clear_cli_value();
        let token = Uuid::new_v4();
        self.workbench.cli_page_token = Some(token);
        self.request(
            backend,
            &format!("cli.page/{token}"),
            "result.page",
            json!({"result_id":id,"start":start,"max_rows":200,"max_bytes":1048576}),
        );
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
                        .ok_or_else(|| "CLI 结果已关闭".to_string())
                        .and_then(|grid| {
                            grid.resolve_large(load.cell, full)
                                .map_err(|e| e.to_string())
                        });
                    if let Err(error) = result {
                        self.workbench_cli_error(error);
                    }
                }
                Err(error) => self.workbench_cli_error(error.to_string()),
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
                self.workbench.cli_error = "CLI 响应缺少任务 ID".into();
            } else {
                self.status = "Redis 命令已提交".into();
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
            self.status = format!("Redis 命令任务 {state}");
            if state != "succeeded" {
                self.workbench.cli_error = value
                    .get("error")
                    .filter(|v| !v.is_null())
                    .map(|v| {
                        v.as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| v.to_string())
                    })
                    .unwrap_or_else(|| format!("命令任务 {state}"));
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
            match serde_json::from_value::<ResultPage>(value) {
                Ok(page)
                    if self.workbench.cli_result_id.as_deref()
                        == Some(page.result.id.to_string().as_str()) =>
                {
                    self.workbench.cli_grid = Some(GridState::new(page, false));
                }
                Ok(_) => self.workbench_cli_error("CLI 结果页与当前结果不匹配".into()),
                Err(error) => self.workbench_cli_error(format!("CLI 结果页无效: {error}")),
            }
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redis_editor_preserves_binary_member_identity() {
        let bytes = vec![0, 0xff, b'\\', b'\n', b'a'];
        let value = DataValue::Bytes(STANDARD.encode(&bytes));
        let editor_text = redis_field(&value).unwrap();
        assert_eq!(redis_bytes(&editor_text).unwrap(), bytes);
        assert_eq!(redis_bytes("hello").unwrap(), b"hello");
        assert_eq!(
            redis_field(&DataValue::Bytes(STANDARD.encode("中文"))).unwrap(),
            "中文"
        );
        let prefixed = DataValue::Bytes(STANDARD.encode("base64:literal"));
        assert_eq!(
            redis_bytes(&redis_field(&prefixed).unwrap()).unwrap(),
            b"base64:literal"
        );
        assert!(redis_bytes("base64:!").is_err());
    }

    use datapier_core::model::{Column, ResultMeta};

    fn page(columns: &[&str], rows: Vec<Vec<DataValue>>) -> ResultPage {
        ResultPage {
            result: ResultMeta {
                id: Uuid::new_v4(),
                execution_id: Uuid::new_v4(),
                columns: columns
                    .iter()
                    .map(|name| Column::new(*name, "text"))
                    .collect(),
                kind: "rows".into(),
                row_count: rows.len() as u64,
                complete: true,
                summary: Value::Null,
            },
            rows,
            start: 0,
            next: None,
        }
    }

    fn app_for(provider: &str, kind: &str, page: ResultPage) -> App {
        let mut app = App::new("test".into());
        app.profile_form(None);
        app.field_set("provider", provider);
        app.field_set("access", "read_write");
        app.profile = Some(app.form_profile().unwrap());
        app.session = Some(json!({"session_id":Uuid::new_v4()}));
        app.target = Some(ResourceTarget {
            database: None,
            name: "item".into(),
            key_b64: None,
            kind: kind.into(),
        });
        app.edit_token = Some(json!(Uuid::new_v4()));
        app.browse_result = true;
        app.schema = Some(if provider == "redis" {
            json!({"type":kind})
        } else {
            json!({"keys":[["a"]],"engine":"InnoDB"})
        });
        app.grid = Some(GridState::new(page, provider != "redis"));
        app.focus = "grid".into();
        app
    }

    fn key(name: &str, shift: bool) -> tern_sdk::Key {
        let mut key = tern_sdk::Key::named(name);
        key.shift = shift;
        key
    }

    #[test]
    fn inline_commit_cancel_and_tab_do_not_send_writes() {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = app_for(
            "mysql",
            "table",
            page(
                &["a", "b"],
                vec![vec![
                    DataValue::Text("旧".into()),
                    DataValue::Text("二".into()),
                ]],
            ),
        );
        assert!(app.workbench_key(&key("enter", false), &backend));
        app.field_set("cell", "修改😀");
        assert!(app.workbench_key(&key("tab", false), &backend));
        assert_eq!(
            app.grid.as_ref().unwrap().value(Cell { row: 0, column: 0 }),
            Some(&DataValue::Text("修改😀".into()))
        );
        assert_eq!(app.workbench.inline, Some(Cell { row: 0, column: 1 }));
        app.field_set("cell", "放弃");
        assert!(app.workbench_key(&key("escape", false), &backend));
        assert_eq!(
            app.grid.as_ref().unwrap().value(Cell { row: 0, column: 1 }),
            Some(&DataValue::Text("二".into()))
        );
        assert!(app.workbench_key(&key("f2", false), &backend));
        app.field_set("cell", "确认");
        assert!(app.workbench_key(&key("enter", false), &backend));
        assert_eq!(
            app.grid.as_ref().unwrap().value(Cell { row: 0, column: 1 }),
            Some(&DataValue::Text("确认".into()))
        );
        assert!(app.workbench_key(&key("f2", false), &backend));
        assert!(app.workbench_key(&key("tab", true), &backend));
        assert_eq!(app.workbench.inline, Some(Cell { row: 0, column: 0 }));
        let mut numeric = app_for(
            "mysql",
            "table",
            page(&["a"], vec![vec![DataValue::Integer("1".into())]]),
        );
        numeric.workbench_key(&key("enter", false), &backend);
        numeric.field_set("cell", "invalid");
        numeric.workbench_key(&key("enter", false), &backend);
        assert!(numeric.workbench.inline.is_some());
        assert_eq!(numeric.field("cell"), "invalid");
        assert!(numeric.grid.as_ref().unwrap().edits().is_empty());
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn redis_hash_rename_preserves_old_binary_field_and_multiline_value() {
        let (backend, mut requests, _) = Backend::scripted();
        let original = vec![0xff, 0, b'x'];
        let mut app = app_for(
            "redis",
            "hash",
            page(
                &["field", "value"],
                vec![vec![
                    DataValue::Bytes(STANDARD.encode(&original)),
                    DataValue::Bytes(STANDARD.encode("第一行\n第二行")),
                ]],
            ),
        );
        app.workbench_action("redis-edit", &backend);
        assert_eq!(app.field("redis_value"), "第一行\n第二行");
        app.field_set("redis_field", "新字段");
        app.workbench_action("redis-stage", &backend);
        assert_eq!(app.redis_changes.len(), 2);
        assert_eq!(
            app.redis_changes[0],
            json!({"operation":"hash_delete","field":{"type":"bytes","value":STANDARD.encode(original)}})
        );
        assert_eq!(
            app.redis_changes[1],
            json!({"operation":"hash_set","field":{"type":"text","value":"新字段"},"value":{"type":"text","value":"第一行\n第二行"}})
        );
        app.workbench_action("redis-undo", &backend);
        assert!(app.redis_changes.is_empty());
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn redis_list_duplicate_values_keep_selected_index_and_zset_rejects_nan() {
        let (backend, mut requests, _) = Backend::scripted();
        let rows = vec![
            vec![
                DataValue::Integer("0".into()),
                DataValue::Bytes(STANDARD.encode("same")),
            ],
            vec![
                DataValue::Integer("1".into()),
                DataValue::Bytes(STANDARD.encode("same")),
            ],
        ];
        let mut app = app_for("redis", "list", page(&["index", "value"], rows));
        app.grid
            .as_mut()
            .unwrap()
            .select(Cell { row: 1, column: 1 }, false, false)
            .unwrap();
        app.workbench_action("redis-edit", &backend);
        app.field_set("redis_index", "0");
        app.workbench_action("redis-stage", &backend);
        assert!(app.redis_changes.is_empty());
        app.field_set("redis_index", "1");
        app.field_set("redis_value", "changed");
        app.workbench_action("redis-stage", &backend);
        assert_eq!(app.redis_changes[0]["index"], json!(1));
        let mut zset = app_for(
            "redis",
            "zset",
            page(
                &["member", "score"],
                vec![vec![
                    DataValue::Bytes(STANDARD.encode("x")),
                    DataValue::Float("1".into()),
                ]],
            ),
        );
        zset.workbench_action("redis-edit", &backend);
        zset.field_set("redis_score", "NaN");
        zset.workbench_action("redis-stage", &backend);
        assert!(zset.redis_changes.is_empty());
        assert!(requests.try_recv().is_err());
    }

    #[test]
    fn cli_pages_do_not_replace_key_grid_or_baseline() {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = app_for(
            "redis",
            "string",
            page(
                &["value"],
                vec![vec![DataValue::Bytes(STANDARD.encode("original"))]],
            ),
        );
        let key_id = app.grid.as_ref().unwrap().page.result.id;
        let baseline = app.edit_token.clone();
        app.field_set("cli_text", "PING");
        app.workbench_action("cli-run", &backend);
        let start = requests.try_recv().unwrap();
        assert_eq!(start.method, "execution.start");
        app.workbench_response("cli.start", json!({"task_id":"task"}), &backend);
        let result = page(&["reply"], vec![vec![DataValue::Text("PONG".into())]]);
        app.workbench_response(
            "execution.get",
            json!({"state":"succeeded","results":[result.result.id]}),
            &backend,
        );
        let request = requests.try_recv().unwrap();
        assert_eq!(request.method, "result.page");
        let tag = request.tag.splitn(3, ':').nth(2).unwrap();
        app.workbench_response(tag, serde_json::to_value(result).unwrap(), &backend);
        assert_eq!(app.grid.as_ref().unwrap().page.result.id, key_id);
        assert_eq!(app.edit_token, baseline);
        assert_eq!(
            app.workbench.cli_grid.as_ref().unwrap().page.rows[0][0],
            DataValue::Text("PONG".into())
        );
    }

    #[test]
    fn history_from_table_opens_query_and_keeps_table_baseline() {
        let (backend, _requests, _) = Backend::scripted();
        let mut app = app_for(
            "mysql",
            "table",
            page(&["a"], vec![vec![DataValue::Text("old".into())]]),
        );
        app.results = vec!["old-result".into()];
        app.result_id = Some("old-result".into());
        let original_result = app.grid.as_ref().unwrap().page.result.id;
        let original_token = app.edit_token.clone();
        app.workbench.history.push(json!({"text":"SELECT 2"}));
        app.grid
            .as_mut()
            .unwrap()
            .edit(Cell { row: 0, column: 0 }, DataValue::Text("draft".into()))
            .unwrap();
        app.workbench_action("history-open:0", &backend);
        assert_eq!(app.tabs[0].query.text, "");
        assert!(app.grid.is_some());
        app.grid.as_mut().unwrap().discard();
        app.workbench_action("history-open:0", &backend);
        assert_eq!(app.tabs[app.active].query.text, "SELECT 2");
        assert!(app.grid.is_none() && app.result_id.is_none() && app.results.is_empty());
        assert!(app.target.is_none());
        app.switch_tab(0);
        assert_eq!(app.grid.as_ref().unwrap().page.result.id, original_result);
        assert_eq!(app.edit_token, original_token);
        assert!(app.browse_result);
        assert_eq!(app.view, "data");
        assert_eq!(app.target.as_ref().unwrap().name, "item");
    }

    #[test]
    fn list_staged_edits_follow_original_rows_and_undo() {
        let (backend, _, _) = Backend::scripted();
        let rows = (0..3)
            .map(|i| {
                vec![
                    DataValue::Integer(i.to_string()),
                    DataValue::Bytes(STANDARD.encode("same")),
                ]
            })
            .collect();
        let mut app = app_for("redis", "list", page(&["index", "value"], rows));
        app.workbench_action("redis-edit", &backend);
        app.workbench_action("redis-op:list_delete", &backend);
        app.workbench_action("redis-stage", &backend);
        assert_eq!(app.redis_changes[0]["index"], json!(0));
        app.grid
            .as_mut()
            .unwrap()
            .select(Cell { row: 1, column: 1 }, false, false)
            .unwrap();
        app.workbench_action("redis-edit", &backend);
        app.field_set("redis_value", "changed");
        app.workbench_action("redis-stage", &backend);
        assert_eq!(app.redis_changes[1]["index"], json!(0));
        assert_eq!(app.workbench.redis_previews[1].before["index"], "1");
        app.workbench_action("redis-undo", &backend);
        app.workbench_action("redis-undo", &backend);
        app.workbench_action("redis-edit", &backend);
        app.workbench_action("redis-op:list_insert", &backend);
        app.field_set("redis_value", "inserted");
        app.workbench_action("redis-stage", &backend);
        app.grid
            .as_mut()
            .unwrap()
            .select(Cell { row: 2, column: 1 }, false, false)
            .unwrap();
        app.workbench_action("redis-edit", &backend);
        app.workbench_action("redis-stage", &backend);
        assert_eq!(app.redis_changes[1]["index"], json!(3));
        app.workbench_action("redis-undo", &backend);
        app.workbench_action("redis-undo", &backend);
        app.workbench_action("redis-edit", &backend);
        app.workbench_action("redis-stage", &backend);
        assert_eq!(app.redis_changes[0]["index"], json!(2));
        assert_eq!(app.redis_list_index(-1, false).unwrap(), 2);
        assert_eq!(app.redis_list_index(-3, false).unwrap(), 0);
        app.grid
            .as_mut()
            .unwrap()
            .select(Cell { row: 0, column: 1 }, false, false)
            .unwrap();
        app.workbench_action("redis-edit", &backend);
        app.workbench_action("redis-op:list_delete", &backend);
        app.workbench_action("redis-stage", &backend);
        app.workbench_action("redis-edit", &backend);
        app.workbench_action("redis-stage", &backend);
        assert_eq!(app.redis_changes.len(), 2);
        assert!(app.status.contains("已暂存删除"));
    }

    #[test]
    fn cli_failure_keeps_partial_results_and_chunked_value_separate() {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = app_for(
            "redis",
            "string",
            page(
                &["value"],
                vec![vec![DataValue::Bytes(STANDARD.encode("key"))]],
            ),
        );
        let key_id = app.grid.as_ref().unwrap().page.result.id;
        app.field_set("cli_text", "PING\nGET x");
        app.workbench_action("cli-run", &backend);
        requests.try_recv().unwrap();
        app.workbench_response("cli.start", json!({"task_id":"t"}), &backend);
        let full = DataValue::Text("完整😀".into());
        let bytes = serde_json::to_vec(&full).unwrap();
        let id = Uuid::new_v4();
        let result = page(
            &["reply"],
            vec![
                vec![DataValue::Text("PONG".into())],
                vec![DataValue::Large {
                    id,
                    original_type: "text".into(),
                    bytes: bytes.len() as u64,
                    preview: "...".into(),
                }],
            ],
        );
        app.workbench_response(
            "execution.get",
            json!({"state":"failed","error":"bad command","results":[result.result.id]}),
            &backend,
        );
        let request = requests.try_recv().unwrap();
        let tag = request.tag.splitn(3, ':').nth(2).unwrap();
        app.workbench_response(tag, serde_json::to_value(result).unwrap(), &backend);
        assert_eq!(
            app.workbench.cli_grid.as_ref().unwrap().page.rows[0][0],
            DataValue::Text("PONG".into())
        );
        assert!(app.workbench.cli_error.contains("bad command"));
        app.workbench_action("cli-open:1:0", &backend);
        let request = requests.try_recv().unwrap();
        let tag = request.tag.splitn(3, ':').nth(2).unwrap();
        let stale_tag = tag.to_owned();
        app.workbench_action("overlay-close", &backend);
        app.workbench_action("cli-open:1:0", &backend);
        let request = requests.try_recv().unwrap();
        let tag = request.tag.splitn(3, ':').nth(2).unwrap();
        app.workbench_response(
            &stale_tag,
            json!({"bytes_b64":STANDARD.encode(&bytes),"offset":0,"next":null,"total":bytes.len()}),
            &backend,
        );
        assert_eq!(app.workbench.cli_large.as_ref().unwrap().offset, 0);
        let split = bytes.len() / 2;
        app.workbench_response(tag, json!({"bytes_b64":STANDARD.encode(&bytes[..split]),"offset":0,"next":split,"total":bytes.len()}), &backend);
        let request = requests.try_recv().unwrap();
        let tag = request.tag.splitn(3, ':').nth(2).unwrap();
        app.workbench_response(tag, json!({"bytes_b64":STANDARD.encode(&bytes[split..]),"offset":split,"next":null,"total":bytes.len()}), &backend);
        assert_eq!(
            app.workbench
                .cli_grid
                .as_ref()
                .unwrap()
                .value(Cell { row: 1, column: 0 }),
            Some(&full)
        );
        assert_eq!(app.grid.as_ref().unwrap().page.result.id, key_id);
        assert!(app.workbench.cli_error.contains("bad command"));
    }

    #[test]
    fn redis_history_opens_persistent_cli_without_touching_drafts() {
        let (backend, _, _) = Backend::scripted();
        let mut app = app_for(
            "redis",
            "string",
            page(
                &["value"],
                vec![vec![DataValue::Bytes(STANDARD.encode("key"))]],
            ),
        );
        let key_id = app.grid.as_ref().unwrap().page.result.id;
        app.redis_changes
            .push(json!({"operation":"set_string","value":{"type":"text","value":"draft"}}));
        app.workbench.history.push(json!({"text":"PING"}));
        app.workbench.cli_results.push("old".into());
        app.workbench_action("history-open:0", &backend);
        assert_eq!(app.field("cli_text"), "PING");
        assert_eq!(app.tabs[app.active].cli_query.text, "PING");
        assert!(app.workbench.cli && app.workbench.cli_results.is_empty());
        assert_eq!(app.view, "data");
        assert_eq!(app.grid.as_ref().unwrap().page.result.id, key_id);
        assert_eq!(app.redis_changes.len(), 1);
    }
}
