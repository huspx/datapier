use super::*;
use crate::i18n::{UiMessage, tr};

/// 查询编辑器的临时 UI 状态，随所属 tab 一起切换。
#[derive(Clone, Default)]
pub struct QueryUiState {
    pub menu_open: bool,
    pub results_hidden: bool,
    pub executed: bool,
}

fn plan_text(query: &editor::Editor, mode: editor::MysqlMode) -> std::result::Result<&str, String> {
    let selected = query.selected_text();
    if query.anchor != query.cursor && selected.is_none() {
        return Err("光标不在有效的 UTF-16 字符边界".into());
    }
    let target = if let Some(text) = selected.filter(|text| !text.trim().is_empty()) {
        text
    } else {
        let range = editor::current_statement(&query.text, query.cursor, mode)?;
        let start = editor::utf16_to_byte(&query.text, range.start).ok_or("无效的语句范围")?;
        let end = editor::utf16_to_byte(&query.text, range.end).ok_or("无效的语句范围")?;
        &query.text[start..end]
    };
    editor::validate_explain_selection(target, mode)?;
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
    let mode = app
        .workbench
        .sql_mode
        .ok_or("数据库未返回 sql_mode，无法安全定位当前语句")?;
    plan_text(&app.tabs[app.active].query, mode)
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
    let mut cancel = ui::button(tr("取消执行").into_owned())
        .icon("stop")
        .on_click(Msg::Action("cancel".into()));
    if app.task.is_none() {
        cancel = cancel.disabled(true);
    }
    html::div()
        .class("toolbar query-toolbar")
        .child(
            ui::button(tr("执行全部").into_owned())
                .variant(ui::ButtonVariant::Primary)
                .icon("play")
                .on_click(Msg::Action("run".into())),
        )
        .child(selection)
        .child(ui::button(tr("当前语句").into_owned()).on_click(Msg::Action("current".into())))
        .child(plan)
        .child(cancel)
        .child(html::span("").class("spacer"))
        .child(
            ui::button(tr("查询文件").into_owned())
                .variant(ui::ButtonVariant::Quiet)
                .icon("file")
                .on_click(Msg::Action("overlay:file".into())),
        )
        .child(
            ui::button(tr("历史").into_owned())
                .variant(ui::ButtonVariant::Quiet)
                .icon("history")
                .on_click(Msg::Action("history".into())),
        )
        .child(html::span(tr("SQL 结果只读").into_owned()).class("tag"))
        .into()
}

/// 原生编辑器和行号共用外层滚动容器；编辑器本身不设置行数上限。
pub(super) fn panel(app: &App) -> Node<Msg> {
    let query = &app.tabs[app.active].query;
    let selected = query.selected_text().filter(|text| !text.trim().is_empty());
    let statements = if query.text.trim().is_empty() {
        Ok(Vec::new())
    } else {
        app.workbench
            .sql_mode
            .ok_or_else(|| "数据库未返回 sql_mode，无法安全定位当前语句".to_owned())
            .and_then(|mode| editor::statements(&query.text, mode))
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
        if let Some(mode) = app.workbench.sql_mode {
            if editor::validate_explain_selection(text, mode).is_ok() {
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
                .workbench
                .sql_mode
                .ok_or_else(|| "数据库未返回 sql_mode，无法安全定位当前语句".to_owned())
                .and_then(|mode| editor::statements(&query.text, mode))
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
                let result = self
                    .workbench
                    .sql_mode
                    .ok_or_else(|| "数据库未返回 sql_mode，无法安全处理选区".to_owned())
                    .and_then(|mode| {
                        if name == "query-explain" {
                            editor::explain_selection(&text, mode)
                        } else {
                            editor::format_range(&query.text, query.selection(), mode)
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
