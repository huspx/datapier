use super::*;
use crate::i18n::{UiMessage, tr};

/// 查询编辑器的临时 UI 状态，随所属 tab 一起切换。
#[derive(Clone)]
pub struct QueryUiState {
    pub menu_open: bool,
    pub results_hidden: bool,
    pub executed: bool,
    pub request_limit: u64,
    pub result_info: Option<String>,
    pub pagination: std::collections::HashMap<String, ResultPaginationState>,
}

#[derive(Clone)]
pub struct ResultPaginationState {
    pub page_size: u64,
    pub starts: Vec<u64>,
    pub token: Option<Uuid>,
    pub start: u64,
    pub complete: Option<bool>,
    pub row_count: Option<u64>,
}

impl Default for ResultPaginationState {
    fn default() -> Self {
        Self {
            page_size: 20,
            starts: Vec::new(),
            token: None,
            start: 0,
            complete: None,
            row_count: None,
        }
    }
}

impl Default for QueryUiState {
    fn default() -> Self {
        Self {
            menu_open: false,
            results_hidden: false,
            executed: false,
            request_limit: 100,
            result_info: None,
            pagination: Default::default(),
        }
    }
}

impl App {
    pub(super) fn close_result_info(&mut self, restore_focus: bool) {
        if self.workbench.query_ui.result_info.take().is_some() && restore_focus {
            self.focus = "result_info".into();
            self.workbench.focus_request = Some("result-info".into());
        }
    }
    pub(super) fn result_saved_start(&self, id: &str) -> u64 {
        self.workbench
            .query_ui
            .pagination
            .get(id)
            .map_or(0, |state| state.start)
    }

    pub(super) fn remember_result_start(&mut self, page: &ResultPage) {
        self.workbench
            .query_ui
            .pagination
            .entry(page.result.id.to_string())
            .or_default()
            .start = page.start;
    }
    pub(super) fn refresh_changed_result_plan(&mut self, backend: &Backend, page: &ResultPage) {
        let id = page.result.id.to_string();
        let changed = self
            .workbench
            .query_ui
            .pagination
            .get(&id)
            .is_some_and(|state| {
                state.token.is_none()
                    && (state
                        .row_count
                        .is_some_and(|count| count != page.result.row_count)
                        || state
                            .complete
                            .is_some_and(|complete| complete != page.result.complete))
            });
        if changed {
            if let Some(state) = self.workbench.query_ui.pagination.get_mut(&id) {
                state.starts.clear();
            }
            self.request_result_pagination(backend, &id);
        }
    }

    pub(super) fn result_page_size(&self, id: &str) -> u64 {
        self.workbench
            .query_ui
            .pagination
            .get(id)
            .map_or(20, |state| state.page_size)
    }

    // 页计划可能外置超宽行；取得稳定边界后才能读取对应页面。
    pub(super) fn prepare_result_page(&mut self, backend: &Backend, id: &str, start: u64) -> bool {
        let state = self
            .workbench
            .query_ui
            .pagination
            .entry(id.to_owned())
            .or_default();
        state.start = start;
        if !state.starts.is_empty() && state.token.is_none() {
            return true;
        }
        if state.token.is_none() {
            self.request_result_pagination(backend, id);
        }
        false
    }

    pub(super) fn request_result_pagination(&mut self, backend: &Backend, id: &str) {
        let token = Uuid::new_v4();
        let state = self
            .workbench
            .query_ui
            .pagination
            .entry(id.to_owned())
            .or_default();
        state.token = Some(token);
        let size = state.page_size;
        let tag = format!(
            "{}:{}:result.pagination/{token}/{id}",
            self.tabs[self.active].id, self.generation
        );
        if let Err(error) = backend.request(
            tag,
            "result.pagination",
            json!({"result_id":id,"page_size":size,"max_bytes":1048576}),
        ) {
            self.workbench
                .query_ui
                .pagination
                .get_mut(id)
                .unwrap()
                .token = None;
            self.status = UiMessage::raw(error.to_string());
        }
    }

    pub(super) fn accept_result_pagination(&mut self, tag: &str, value: Value, backend: &Backend) {
        let Some((token, id)) = tag
            .strip_prefix("result.pagination/")
            .and_then(|s| s.split_once('/'))
        else {
            return;
        };
        let Some(state) = self.workbench.query_ui.pagination.get_mut(id) else {
            return;
        };
        if Uuid::parse_str(token).ok() != state.token || state.token.is_none() {
            return;
        }
        state.token = None;
        let Some(size) = value.get("page_size").and_then(Value::as_u64) else {
            return;
        };
        let Some(complete) = value.get("complete").and_then(Value::as_bool) else {
            return;
        };
        let Some(row_count) = value.get("row_count").and_then(Value::as_u64) else {
            return;
        };
        if size != state.page_size {
            return;
        }
        let Some(starts) = value
            .get("starts")
            .and_then(Value::as_array)
            .and_then(|items| items.iter().map(Value::as_u64).collect::<Option<Vec<_>>>())
        else {
            return;
        };
        if starts.first() != Some(&0) || !starts.windows(2).all(|pair| pair[0] < pair[1]) {
            return;
        }
        state.starts = starts;
        state.complete = Some(complete);
        state.row_count = Some(row_count);
        let start = state
            .starts
            .iter()
            .rev()
            .find(|start| **start <= state.start)
            .copied()
            .unwrap_or(0);
        state.start = start;
        if self.result_id.as_deref() == Some(id) {
            self.page(backend, start);
        } else if self.workbench.cli_result_id.as_deref() == Some(id) {
            self.workbench_cli_page(backend, start);
        }
    }
}

pub(super) fn request_limit_picker(app: &App) -> Node<Msg> {
    let mut select = ui::select()
        .key("archery-request-limit")
        .value(app.workbench.query_ui.request_limit.to_string())
        .disabled(app.operation_pending || app.task.is_some() || app.workbench.cli_running)
        .open(app.workbench.generic_select.as_deref() == Some("archery-request-limit"))
        .on_toggle(|collapsed| {
            Msg::Action(format!("select-open:archery-request-limit:{}", !collapsed))
        })
        .on_change(|change| {
            Msg::Action(format!(
                "archery-request-limit:{}",
                change.value.as_str().unwrap_or_default()
            ))
        });
    for size in [100, 500, 1000] {
        select = select.option(ui::SelectOption::new(size.to_string(), size.to_string()));
    }
    html::div()
        .class("request-limit")
        .child(html::span(tr("最多返回").into_owned()))
        .child(select)
        .into()
}

fn plan_text(
    query: &editor::Editor,
    context: editor::SqlContext,
) -> std::result::Result<&str, String> {
    let selected = query.selected_text();
    if query.anchor != query.cursor && selected.is_none() {
        return Err("光标不在有效的 UTF-16 字符边界".into());
    }
    let target = if let Some(text) = selected.filter(|text| !text.trim().is_empty()) {
        text
    } else {
        let range = editor::current_statement_in(&query.text, query.cursor, context)?;
        let start = editor::utf16_to_byte(&query.text, range.start).ok_or("无效的语句范围")?;
        let end = editor::utf16_to_byte(&query.text, range.end).ok_or("无效的语句范围")?;
        &query.text[start..end]
    };
    editor::validate_explain_selection_in(target, context)?;
    Ok(target)
}

fn plan_request(app: &App) -> std::result::Result<&str, String> {
    if app.session_id().is_none() || app.is_redis() {
        return Err("请先连接数据库".into());
    }
    if app.operation_pending || app.task.is_some() {
        return Err("该 tab 已有正在执行的任务".into());
    }
    if app.has_live_drafts() {
        return Err("请先提交或丢弃待提交变更与粘贴预览".into());
    }
    plan_text(&app.tabs[app.active].query, app.sql_context()?)
}

impl App {
    pub(super) fn sql_context(&self) -> std::result::Result<editor::SqlContext, String> {
        if self
            .profile
            .as_ref()
            .is_some_and(|profile| profile.is_archery())
        {
            Ok(editor::SqlContext::Stateless)
        } else {
            self.workbench
                .sql_mode
                .map(editor::SqlContext::Session)
                .ok_or_else(|| "数据库未返回 sql_mode，无法安全定位当前语句".to_owned())
        }
    }
}
pub(super) fn toolbar(app: &App) -> Node<Msg> {
    let selected = app.tabs[app.active]
        .query
        .selected_text()
        .is_some_and(|text| !text.trim().is_empty());
    let mut selection =
        ui::button(tr("执行选区").into_owned()).on_click(Msg::Action("selection".into()));
    if !selected {
        selection = selection.disabled(tr("没有有效的 SQL 选区").into_owned());
    }
    let mut plan =
        ui::button(tr("查看执行计划").into_owned()).on_click(Msg::Action("query-plan".into()));
    if let Err(reason) = plan_request(app) {
        plan = plan.disabled(tr(&reason).into_owned());
    }
    let archery = app
        .profile
        .as_ref()
        .is_some_and(|profile| profile.is_archery());
    let cancel_label = tr(if archery {
        "停止等待"
    } else {
        "取消执行"
    })
    .into_owned();
    let mut cancel = ui::button("")
        .variant(ui::ButtonVariant::Quiet)
        .size(ui::ButtonSize::Md)
        .title(cancel_label.clone())
        .aria(cancel_label)
        .icon("stop")
        .on_click(Msg::Action("cancel".into()));
    if app.task.is_none() {
        cancel = cancel.disabled(true);
    }
    let enabled = app.session_id().is_some() && !app.operation_pending && app.task.is_none();
    if !enabled {
        selection = selection.disabled(true);
    }
    let mut toolbar = html::div()
        .class("toolbar query-toolbar")
        .child(
            ui::button(tr("执行全部").into_owned())
                .variant(ui::ButtonVariant::Primary)
                .icon("play")
                .disabled(!enabled)
                .on_click(Msg::Action("run".into())),
        )
        .child(selection)
        .child(
            ui::button(tr("当前语句").into_owned())
                .disabled(!enabled)
                .on_click(Msg::Action("current".into())),
        )
        .child(plan);
    if archery
        && app
            .profile
            .as_ref()
            .is_some_and(|profile| profile.capabilities().sql_check.is_supported())
    {
        let label = tr("检查上线 SQL（不执行）").into_owned();
        toolbar = toolbar.child(
            html::button("")
                .class("query-check-icon")
                .title(label.clone())
                .attr("aria-label", label)
                .disabled(!enabled)
                .child(ui::icon("file"))
                .child(
                    html::span("")
                        .class("query-check-search")
                        .child(ui::icon("search")),
                )
                .on_click(Msg::Action("archery-sql-check".into())),
        );
    }
    toolbar = toolbar.child(cancel);
    if archery {
        toolbar = toolbar.child(request_limit_picker(app));
    }
    toolbar = toolbar.child(html::span("").class("spacer"));
    for (icon, label, action) in [
        ("file", "查询文件", "overlay:file"),
        ("history", "历史", "history"),
    ] {
        let label = tr(label).into_owned();
        toolbar = toolbar.child(
            ui::button("")
                .variant(ui::ButtonVariant::Quiet)
                .size(ui::ButtonSize::Md)
                .icon(icon)
                .title(label.clone())
                .aria(label)
                .on_click(Msg::Action(action.into())),
        );
    }
    toolbar.into()
}

/// 原生编辑器和行号共用外层滚动容器；编辑器本身不设置行数上限。
pub(super) fn panel(app: &App) -> Node<Msg> {
    let query = &app.tabs[app.active].query;
    let selected = query.selected_text().filter(|text| !text.trim().is_empty());
    let statements = if query.text.trim().is_empty() {
        Ok(Vec::new())
    } else {
        app.sql_context()
            .and_then(|context| editor::statements_in(&query.text, context))
    };
    let mut gutter = html::div().class("query-gutter");
    for line in 1..=query.text.bytes().filter(|byte| *byte == b'\n').count() + 1 {
        let mut row = html::div()
            .class("query-gutter-line")
            .child(html::span(line.to_string()).class("query-line-number"));
        if let Ok(items) = &statements {
            for (index, item) in items
                .iter()
                .enumerate()
                .filter(|(_, item)| item.first_line == line)
            {
                let title = format!(
                    "{} {} · {} {}–{}",
                    tr("执行语句"),
                    index + 1,
                    tr("行"),
                    item.first_line,
                    item.last_line
                );
                row = row.child(
                    html::div().class("query-line-run").child(
                        ui::button("")
                            .variant(ui::ButtonVariant::Quiet)
                            .size(ui::ButtonSize::Sm)
                            .icon("play")
                            .title(title)
                            .on_click(Msg::Action(format!("query-statement:{index}"))),
                    ),
                );
            }
        }
        gutter = gutter.child(row);
    }
    let mut input = ui::editor()
        .aria(tr("SQL 查询").into_owned())
        .key("query-editor")
        .text(query.text.clone())
        .cursor(query.cursor)
        .anchor(query.anchor)
        .lang("sql")
        .mode("insert")
        .prop("nowrap", true)
        .on_focus(Msg::Focus("query".into()))
        .on_undo(Msg::Undo("query".into()))
        .on_edit(|e| {
            Msg::Edit(
                "query".into(),
                e.from,
                e.to,
                e.text.clone(),
                e.cursor,
                e.len,
            )
        });
    if let Some(text) = selected {
        let tab = app.tabs[app.active].id;
        let anchor = query.anchor;
        let cursor = query.cursor;
        let expected = text.to_owned();
        let mut actions = ui::Actions::new().menu(
            ui::MenuItem::new("query-selection", tr("执行选中 SQL").into_owned()).icon("play"),
        );
        if let Ok(context) = app.sql_context() {
            if editor::validate_explain_selection_in(text, context).is_ok() {
                actions = actions.menu(
                    ui::MenuItem::new("query-explain", tr("查看执行计划").into_owned())
                        .icon("chart"),
                );
            }
            actions = actions.menu(
                ui::MenuItem::new("query-format", tr("格式化选区").into_owned()).icon("wand"),
            );
        }
        actions = actions
            .menu(ui::MenuEntry::Separator)
            .menu(ui::MenuItem::new("query-copy", tr("复制").into_owned()).icon("copy"))
            .menu(ui::MenuItem::new("query-cut", tr("剪切").into_owned()).icon("scissors"))
            .menu(ui::MenuItem::new("query-select-all", tr("全选").into_owned()).icon("selection"));
        input = input
            .actions(actions)
            .on_action(
                "query-selection",
                Msg::QueryMenu(
                    tab,
                    "query-selection".into(),
                    anchor,
                    cursor,
                    expected.clone(),
                ),
            )
            .on_action(
                "query-explain",
                Msg::QueryMenu(
                    tab,
                    "query-explain".into(),
                    anchor,
                    cursor,
                    expected.clone(),
                ),
            )
            .on_action(
                "query-format",
                Msg::QueryMenu(tab, "query-format".into(), anchor, cursor, expected.clone()),
            )
            .on_action("query-copy", Msg::CopyText(expected.clone()))
            .on_action(
                "query-cut",
                Msg::QueryCut(
                    tab,
                    expected.clone(),
                    query.selection().start,
                    query.selection().end,
                ),
            )
            .on_action(
                "query-select-all",
                Msg::QueryMenu(tab, "query-select-all".into(), anchor, cursor, expected),
            );
    }
    let mut panel = html::div().class("sql-editor");
    if let Err(reason) = statements {
        panel = panel.child(
            html::div()
                .class("query-boundary-hint")
                .child(html::span(tr(&reason).into_owned())),
        );
    }
    panel = panel.child(html::div().class("query-scroll").child(gutter).child(input));
    let mut completion = html::div().class("completion");
    if let Some(byte) = editor::utf16_to_byte(&query.text, query.cursor) {
        let before = &query.text[..byte];
        let start = before
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
            .map_or(0, |(i, c)| i + c.len_utf8());
        let prefix = &before[start..];
        if !prefix.is_empty() {
            let catalog: Vec<String> = app
                .resources
                .iter()
                .map(|resource| resource.name.clone())
                .collect();
            let from = before[..start].encode_utf16().count();
            for item in editor::completions(prefix, &catalog).into_iter().take(8) {
                completion = completion.child(
                    ui::button(item.clone())
                        .variant(ui::ButtonVariant::Quiet)
                        .size(ui::ButtonSize::Sm)
                        .on_click(Msg::Complete(item, from, query.cursor)),
                );
            }
        }
    }
    panel.child(completion).into()
}

fn restore_menu_selection(
    query: &mut editor::Editor,
    anchor: usize,
    cursor: usize,
    expected: &str,
) -> bool {
    let actual = editor::utf16_to_byte(&query.text, anchor.min(cursor))
        .zip(editor::utf16_to_byte(&query.text, anchor.max(cursor)))
        .and_then(|(a, b)| query.text.get(a..b));
    actual == Some(expected) && query.set_selection(anchor, cursor)
}

impl App {
    /// 右键菜单使用打开前的 UTF-16 选区，若文本已变化则拒绝过期操作。
    pub(super) fn query_menu(
        &mut self,
        tab: Uuid,
        name: &str,
        start: usize,
        end: usize,
        expected: &str,
        backend: &Backend,
    ) {
        if self.tabs[self.active].id != tab {
            return;
        }
        let query = &mut self.tabs[self.active].query;
        if !restore_menu_selection(query, start, end, expected) {
            self.status = UiMessage::key("SQL 选区已变化，请重新选择");
            return;
        }
        self.query_action(name, backend);
    }

    /// 处理 SQL 编辑器专属操作；执行统一经过应用已有数据库路径。
    pub(super) fn query_action(&mut self, name: &str, backend: &Backend) -> bool {
        if let Some(index) = name
            .strip_prefix("query-statement:")
            .and_then(|value| value.parse::<usize>().ok())
        {
            let query = &self.tabs[self.active].query;
            let result = self
                .sql_context()
                .and_then(|context| editor::statements_in(&query.text, context))
                .and_then(|items| {
                    items
                        .get(index)
                        .cloned()
                        .ok_or_else(|| "语句已变化，请重试".to_owned())
                })
                .and_then(|item| {
                    let start = editor::utf16_to_byte(&query.text, item.range.start)
                        .ok_or("无效的语句范围")?;
                    let end = editor::utf16_to_byte(&query.text, item.range.end)
                        .ok_or("无效的语句范围")?;
                    Ok(query.text[start..end].to_owned())
                });
            match result {
                Ok(text) => self.execute_query_text(text, backend),
                Err(reason) => self.status = UiMessage::local(reason),
            }
            return true;
        }
        match name {
            "query-plan" => {
                match plan_request(self).map(|text| format!("EXPLAIN FORMAT=JSON {text}")) {
                    Ok(text) => self.execute_query_text(text, backend),
                    Err(reason) => self.status = UiMessage::local(reason),
                }
                true
            }
            "query-selection" | "query-explain" | "query-format" => {
                let query = &self.tabs[self.active].query;
                let Some(text) = query
                    .selected_text()
                    .filter(|text| !text.trim().is_empty())
                    .map(str::to_owned)
                else {
                    self.status = UiMessage::key("没有有效的 SQL 选区");
                    return true;
                };
                if name == "query-selection" {
                    self.execute_query_text(text, backend);
                    return true;
                }
                let result = self.sql_context().and_then(|context| {
                    if name == "query-explain" {
                        editor::explain_selection_in(&text, context)
                    } else {
                        editor::format_range_in(&query.text, query.selection(), context)
                    }
                });
                match result {
                    Ok(text) if name == "query-explain" => self.execute_query_text(text, backend),
                    Ok(text) => {
                        let query = &mut self.tabs[self.active].query;
                        let range = query.selection();
                        if query.edit(range, &text) {
                            self.changed_field("query");
                        }
                    }
                    Err(reason) => self.status = UiMessage::local(reason),
                }
                true
            }
            "query-select-all" => {
                self.tabs[self.active]
                    .query
                    .key(editor::Key::SelectAll, false);
                self.focus = "query".into();
                self.workbench.focus_request = Some("query-editor".into());
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/query_ui.rs"]
mod tests;
