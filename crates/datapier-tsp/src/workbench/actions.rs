use super::*;

impl App {
    pub(crate) fn workbench_action(&mut self, name: &str, backend: &Backend) -> bool {
        if name == "result-info-keep" {
            return true;
        }
        if name == "result-info-dismiss" {
            self.close_result_info(false);
            return true;
        }
        if name == "result-info-close" {
            self.close_result_info(true);
            return true;
        }
        if let Some(id) = name.strip_prefix("result-info-toggle:") {
            let owns_result = self
                .grid
                .as_ref()
                .is_some_and(|grid| grid.page.result.id.to_string() == id)
                || self
                    .workbench
                    .cli_history
                    .iter()
                    .any(|record| record.results.iter().any(|result| result == id));
            if owns_result {
                if self.workbench.query_ui.result_info.as_deref() == Some(id) {
                    self.close_result_info(true);
                } else {
                    self.workbench.query_ui.result_info = Some(id.to_owned());
                }
            }
            return true;
        }
        if name == "result-count" {
            if self.page_token.is_none()
                && let Some(id) = self.result_id.clone()
            {
                let size = self.result_page_size(&id);
                let state = self
                    .workbench
                    .query_ui
                    .pagination
                    .entry(id.clone())
                    .or_insert_with(|| crate::query_ui::ResultPaginationState {
                        page_size: size,
                        ..Default::default()
                    });
                if state.count_token.is_none() {
                    let token = Uuid::new_v4();
                    let tag = format!(
                        "{}:{}:result.count/{id}/{token}",
                        self.tabs[self.active].id, self.generation
                    );
                    match backend.request(tag, "result.count", json!({"result_id":id})) {
                        Ok(()) => state.count_token = Some(token),
                        Err(error) => self.status = UiMessage::raw(error.to_string()),
                    }
                }
            }
            return true;
        }
        if let Some(rest) = name.strip_prefix("result-size:") {
            if let Some((kind, size)) = rest.split_once(':')
                && matches!(kind, "grid" | "cli")
                && let Ok(size) = size.parse::<u64>()
                && (1..=1000).contains(&size)
            {
                let cli = kind == "cli";
                if (cli && self.workbench.cli_page_token.is_some())
                    || (!cli && self.page_token.is_some())
                {
                    return true;
                }
                if !cli && self.has_live_drafts() {
                    self.status = "请先提交或丢弃草稿".into();
                    return true;
                }
                let id = if cli {
                    self.workbench.cli_result_id.clone()
                } else {
                    self.result_id.clone()
                };
                if let Some(id) = id {
                    let state = self.workbench.query_ui.pagination.entry(id).or_default();
                    state.page_size = size;
                    state.starts = vec![0];
                    state.start = 0;
                    self.workbench.generic_select = None;
                    if cli {
                        self.workbench.cli_page_history.clear();
                        self.workbench_cli_page(backend, 0);
                    } else {
                        self.page_history.clear();
                        self.page(backend, 0);
                    }
                }
            }
            return true;
        }
        if let Some(limit) = name
            .strip_prefix("archery-request-limit:")
            .and_then(|value| value.parse::<u64>().ok())
        {
            if matches!(limit, 100 | 500 | 1000)
                && self
                    .profile
                    .as_ref()
                    .is_some_and(|profile| profile.is_archery())
                && !self.operation_pending
                && self.task.is_none()
                && !self.workbench.cli_running
            {
                self.workbench.query_ui.request_limit = limit;
            }
            self.workbench.generic_select = None;
            return true;
        }
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
                && matches!(
                    field,
                    "provider" | "type" | "access" | "tls" | "format" | "external_mode"
                )
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
        if name == "cli-export" {
            if self.workbench.cli_result_id.is_some() {
                self.workbench_action("overlay:export", backend);
                self.workbench.cli_export = true;
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
                self.workbench.cli_export = false;
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
        if name == "redis-view-string" {
            if self.is_redis()
                && self
                    .target
                    .as_ref()
                    .is_some_and(|target| target.kind == "string")
                && let Some(grid) = self.grid.as_ref()
            {
                self.open_cell_at(
                    Cell {
                        row: grid.page.start,
                        column: 0,
                    },
                    backend,
                );
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
            else {
                return true;
            };
            let large_id = match value {
                DataValue::Large { id, .. } => Some(*id),
                _ => None,
            };
            self.workbench_clear_cli_value();
            self.workbench.cli_cell = Some(cell);
            self.workbench.overlay = Some("cli-value".into());
            if let Some(id) = large_id {
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
        if let Some(rest) = name.strip_prefix("cli-record:") {
            if let Some((record_id, result_index)) = rest.split_once(':')
                && let (Ok(record_id), Ok(result_index)) =
                    (Uuid::parse_str(record_id), result_index.parse::<usize>())
                && let Some(record) = self
                    .workbench
                    .cli_history
                    .iter()
                    .find(|record| record.id == record_id)
                && let Some(result_id) = record.results.get(result_index).cloned()
            {
                if self.workbench.cli_selected == Some(record_id)
                    && self.workbench.cli_result_id.as_ref() == Some(&result_id)
                {
                    return true;
                }
                let results = record.results.clone();
                self.close_result_info(false);
                self.workbench_cli_preserve_pending_page(backend);
                self.workbench.cli_page_token = None;
                self.workbench.cli_page_transition = None;
                self.workbench_cli_store_grid();
                self.workbench.cli_selected = Some(record_id);
                self.workbench.cli_results = results;
                self.workbench.cli_result_id = Some(result_id);
                self.workbench.cli_grid = self
                    .workbench
                    .cli_history
                    .iter_mut()
                    .find(|record| record.id == record_id)
                    .and_then(|record| record.result_grids.get_mut(result_index))
                    .and_then(Option::take);
                self.workbench.cli_page_history = self
                    .workbench
                    .cli_history
                    .iter_mut()
                    .find(|record| record.id == record_id)
                    .and_then(|record| record.result_page_histories.get_mut(result_index))
                    .map(std::mem::take)
                    .unwrap_or_default();
                if self.workbench.cli_grid.is_none() {
                    self.workbench_cli_page(backend, 0);
                } else {
                    self.workbench_clear_cli_value();
                    self.workbench.cli_page_token = None;
                    self.workbench.cli_page_transition = None;
                }
            }
            return true;
        }
        if let Some(prefix) = name.strip_prefix("redis-tree:") {
            if let Some(profile) = self.profile.as_ref() {
                self.workbench.redis_expansion.toggle(
                    profile.id,
                    self.tabs[self.active]
                        .browse
                        .redis_db
                        .unwrap_or_else(|| profile.redis_db()),
                    prefix,
                );
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
            "cli-toggle" => {
                self.workbench.cli = !self.workbench.cli;
                self.dirty = true;
                if self.workbench.cli {
                    self.focus = "cli_text".into();
                    self.workbench.focus_request = Some("input-cli_text".into());
                } else if matches!(self.focus.as_str(), "cli_text" | "cli_height") {
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
                // AUTH 的参数是凭据；在创建记录或请求之前拒绝，避免进入任务和复制历史。
                match datapier_core::providers::redis_contains_auth(&text) {
                    Ok(false) => {}
                    Ok(true) => {
                        self.status = UiMessage::key("不支持在 CLI 中执行 AUTH");
                        return true;
                    }
                    Err(_) => {
                        self.status = UiMessage::key("Redis 命令格式无效");
                        return true;
                    }
                }
                let database = self.tabs[self.active]
                    .browse
                    .redis_db
                    .or_else(|| self.profile.as_ref().map(|profile| profile.redis_db()))
                    .map(|db| db.to_string());
                self.close_result_info(false);
                let mut params = json!({"session_id":session,"workspace_id":self.workspace_id,"text":text,"database":database});
                if self
                    .profile
                    .as_ref()
                    .is_some_and(|profile| profile.is_archery())
                {
                    params["limit"] = json!(self.workbench.query_ui.request_limit);
                }
                self.request(backend, "cli.start", "execution.start", params);
                if self.operation_pending {
                    self.workbench_cli_preserve_pending_page(backend);
                    self.workbench_cli_store_grid();
                    self.workbench.cli_history.push(CliRecord {
                        id: Uuid::new_v4(),
                        command: text.clone(),
                        instance: self.profile.as_ref().map_or_else(String::new, |p| {
                            p.archery().map_or_else(
                                || p.name.clone(),
                                |source| source.instance_name.clone(),
                            )
                        }),
                        database: self
                            .workbench
                            .cli_database
                            .unwrap_or_else(|| database.as_deref().and_then(|db| db.parse().ok()))
                            .map(|db| db.to_string())
                            .unwrap_or_else(|| crate::i18n::tr("未知").into_owned()),
                        state: UiMessage::key("执行中…"),
                        elapsed: None,
                        error: UiMessage::default(),
                        results: Vec::new(),
                        result_grids: Vec::new(),
                        result_page_histories: Vec::new(),
                    });
                    self.workbench.cli_selected =
                        self.workbench.cli_history.last().map(|record| record.id);
                    self.workbench.cli_reveal = true;
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
                    self.draft_revision = self.draft_revision.wrapping_add(1);
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
            "redis-stage" => return self.workbench_redis_stage(),
            _ => return false,
        }
        true
    }
}
