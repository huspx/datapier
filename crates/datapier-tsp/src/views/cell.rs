//! 冻结单元格目标的详情与编辑抽屉。
use super::*;
pub(super) fn cell_drawer(app: &App) -> Node<Msg> {
    if app.workbench.overlay_close_confirm {
        return html::div()
            .class("drawer")
            .child(html::strong(
                crate::i18n::tr("丢弃未应用的输入？").into_owned(),
            ))
            .child(html::p(
                crate::i18n::tr("未应用的输入将被丢弃。").into_owned(),
            ))
            .child(
                ui::button(crate::i18n::tr("继续编辑").into_owned())
                    .key("overlay-close-safe")
                    .on_click(Msg::Action("overlay-close-continue".into())),
            )
            .child(
                ui::button(crate::i18n::tr("丢弃").into_owned())
                    .variant(ui::ButtonVariant::Danger)
                    .on_click(Msg::Action("overlay-close-discard".into())),
            )
            .into();
    }
    let mut drawer = html::div().class("drawer").child(
        html::div()
            .class("toolbar")
            .child(html::strong(
                crate::i18n::tr(if app.is_redis() {
                    "单元格详情"
                } else {
                    "单元格编辑"
                })
                .into_owned(),
            ))
            .child(html::span("").class("spacer"))
            .child(button("关闭", "overlay-close")),
    );
    let Some(cell) = app.detail_cell() else {
        return drawer
            .child(
                html::span(crate::i18n::tr("原结果已不可用，请重新选择单元格").into_owned())
                    .class("empty-note"),
            )
            .into();
    };
    let grid = app.grid.as_ref().unwrap();
    let mut identity = Vec::new();
    if let Some(keys) = app
        .schema
        .as_ref()
        .and_then(|schema| schema.get("keys"))
        .and_then(Value::as_array)
        .and_then(|keys| keys.first())
        .and_then(Value::as_array)
    {
        for name in keys.iter().filter_map(Value::as_str) {
            if let Some(column) = grid
                .page
                .result
                .columns
                .iter()
                .position(|column| column.name == name)
                && let Some(value) = grid.value(Cell {
                    row: cell.row,
                    column,
                })
            {
                identity.push(format!("{name} = {}", value_text(value, false)));
            }
        }
    }
    if identity.is_empty() {
        identity.push(crate::i18n::format_message(
            "第 {row} 行",
            &[("row", &(cell.row + 1).to_string())],
        ));
    }
    let mut selector = ui::select()
        .value(cell.column.to_string())
        .open(app.workbench.cell_fields_open)
        .on_toggle(|collapsed| Msg::Action(format!("cell-fields-open:{}", !collapsed)))
        .on_change(|change| {
            Msg::Action(format!(
                "cell-field:{}",
                change.value.as_str().unwrap_or_default()
            ))
        });
    for (index, column) in grid.page.result.columns.iter().enumerate() {
        selector = selector.option(ui::SelectOption::new(
            index.to_string(),
            column.name.clone(),
        ));
    }
    let (connection, resource) = app
        .workbench
        .cell_context
        .as_ref()
        .map(|(connection, resource)| (connection.as_deref(), resource.as_deref()))
        .unwrap_or((None, None));
    let missing = crate::i18n::tr("未记录");
    drawer = drawer
        .child(html::span(crate::i18n::tr("连接").into_owned()).class("label"))
        .child(html::span(connection.unwrap_or(&missing).to_owned()))
        .child(html::span(crate::i18n::tr("目标资源").into_owned()).class("label"))
        .child(html::span(resource.unwrap_or(&missing).to_owned()).class("mono"));
    drawer = drawer
        .child(html::span(crate::i18n::tr("当前行").into_owned()).class("label"))
        .child(html::span(identity.join(" · ")).class("mono"))
        .child(ui::field(crate::i18n::tr("字段").into_owned()).child(selector));
    drawer = drawer.child(button("高级值编辑", "advanced-value-open"));
    let row_error = if app.is_redis() {
        None
    } else {
        app.check_rows_not_deleted([cell.row]).err()
    };
    let reason = if app.profile.is_none() {
        Some(crate::i18n::tr("连接不可用，无法编辑单元格").into_owned())
    } else if !writable(app) {
        Some(crate::i18n::tr("当前连接为只读，无法编辑单元格").into_owned())
    } else if !grid.editable {
        Some(crate::i18n::tr("当前结果不可编辑").into_owned())
    } else if grid.page.result.columns[cell.column].generated {
        Some(crate::i18n::tr("生成列不可编辑").into_owned())
    } else {
        row_error.map(|error| match error {
            crate::i18n::UiMessage::Key("目标行已有待提交变更") => {
                crate::i18n::tr("该行已有待提交的删除，无法编辑").into_owned()
            }
            other => other.render().into_owned(),
        })
    };
    if reason.is_none() {
        drawer = drawer
            .child(field(app, "cell", "值"))
            .child(
                html::div()
                    .class("choice-row")
                    .child(button("加入草稿", "set-cell"))
                    .child(button("设为 NULL", "set-null")),
            )
            .child(button("设置 JSON", "set-json"))
            .child(
                html::span(crate::i18n::tr("长值可在侧栏编辑，变更先进入草稿。").into_owned())
                    .class("empty-note"),
            );
    } else {
        let text = if app.is_redis() {
            grid.value(cell)
                .map(|value| value_text(value, true).into_owned())
                .unwrap_or_default()
        } else {
            app.field("cell").to_owned()
        };
        drawer = drawer.child(html::pre(text).class("preview"));
        if let Some(reason) = reason {
            drawer = drawer.child(html::p(reason).class("muted"));
        }
        let mut actions = html::div().class("choice-row");
        for (label, action) in [
            ("加入草稿", "set-cell"),
            ("设为 NULL", "set-null"),
            ("设置 JSON", "set-json"),
        ] {
            actions = actions.child(
                ui::button(crate::i18n::tr(label).into_owned())
                    .disabled(true)
                    .on_click(Msg::Action(action.into())),
            );
        }
        drawer = drawer.child(actions);
    }
    drawer.into()
}
