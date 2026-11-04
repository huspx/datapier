//! 冻结单元格目标的详情与编辑抽屉。
use super::*;
pub(super) fn cell_drawer(app: &App) -> Node<Msg> {
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
    drawer = drawer
        .child(html::span(crate::i18n::tr("当前行").into_owned()).class("label"))
        .child(html::span(identity.join(" · ")).class("mono"))
        .child(ui::field(crate::i18n::tr("字段").into_owned()).child(selector));
    if writable(app) && grid.editable && !grid.page.result.columns[cell.column].generated {
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
    }
    drawer.into()
}
