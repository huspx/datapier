use super::*;

fn visible_rows(app: &App) -> Vec<u64> {
    let Some(grid) = app.grid.as_ref() else {
        return Vec::new();
    };
    let needle = app.field("row_search").to_lowercase();
    let redis = app.is_redis();
    grid.page
        .rows
        .iter()
        .enumerate()
        .filter_map(|(i, row)| {
            let row_id = grid.page.start + i as u64;
            (needle.is_empty()
                || row.iter().enumerate().any(|(col, value)| {
                    let value = grid
                        .value(Cell {
                            row: row_id,
                            column: col,
                        })
                        .unwrap_or(value);
                    value_text(value, redis).to_lowercase().contains(&needle)
                }))
            .then_some(row_id)
        })
        .collect()
}

impl App {
    /// 同步显示筛选与行选区，供筛选、结果及单元格变更后调用。
    pub(crate) fn sync_grid_visible_rows(&mut self) {
        let rows = visible_rows(self);
        if let Some(grid) = self.grid.as_mut() {
            grid.set_visible_rows(rows);
        }
    }
}

// Tern 自动附加原生 Copy；它只读取展示文本，完整选区由应用动作复制。
fn row_menu(app: &App, cell: Cell, redis: bool, editable: bool, include_cell: bool) -> ui::Actions {
    let mut actions = ui::Actions::new();
    if redis {
        actions = actions.menu(
            ui::MenuItem::new(
                "grid-open-key",
                crate::i18n::tr("打开当前 key").into_owned(),
            )
            .icon("eye"),
        );
    }
    if include_cell {
        actions = actions.menu(
            ui::MenuItem::new(
                "grid-copy-cells",
                crate::i18n::tr("复制选中单元格").into_owned(),
            )
            .icon("copy"),
        );
    }
    actions = actions.menu(
        ui::MenuItem::new("grid-copy-rows", crate::i18n::tr("复制选中行").into_owned())
            .icon("copy"),
    );
    if app.can_copy_insert() {
        actions = actions
            .menu(
                ui::MenuItem::new(
                    "grid-copy-insert",
                    crate::i18n::tr("复制 INSERT").into_owned(),
                )
                .icon("copy"),
            )
            .menu(
                ui::MenuItem::new(
                    "grid-copy-insert-without-auto-primary",
                    crate::i18n::tr("复制 INSERT（不带自增主键）").into_owned(),
                )
                .icon("copy"),
            );
    }
    if writable(app)
        && ((redis && app.browse_result && app.edit_token.is_some())
            || (!app.is_redis() && editable && app.can_copy_insert()))
    {
        actions = actions.menu(ui::MenuEntry::Separator).menu(
            ui::MenuItem::new(
                "grid-delete-rows",
                crate::i18n::format_message(
                    "删除 {count} 行",
                    &[("count", &app.grid_menu_row_count(cell).to_string())],
                ),
            )
            .icon("trash")
            .danger(true),
        );
    }
    actions
}

pub(super) fn grid(app: &App) -> Option<Node<Msg>> {
    let grid = app.grid.as_ref()?;
    let cols = grid.visible_columns();
    let redis = app.is_redis();
    let redis_rows = app.redis_composite_grid();
    let mut wrapper = html::div().class("gridscroll").key("grid");
    let mut header = html::div().class("gridrow header").key("header");
    header = header.child(html::span("#").class("rownum"));
    for &col in &cols {
        let column = &grid.page.result.columns[col];
        let name = if redis {
            match column.name.as_str() {
                "field" => "Field",
                "value" => "Value",
                "index" => "Index",
                "member" => "Member",
                "score" => "Score",
                name => name,
            }
        } else {
            column.name.as_str()
        };
        let mut cell = html::span("")
            .class(format!("cell col{col}"))
            .child(html::span(name));
        cell = cell
            .actions(
                ui::Actions::new().menu(
                    ui::MenuItem::new(
                        "grid-copy-column",
                        crate::i18n::tr("复制字段名").into_owned(),
                    )
                    .icon("copy"),
                ),
            )
            .on_action("grid-copy-column", Msg::CopyText(column.name.clone()));
        if !redis {
            cell = cell.child(html::span(column.type_name.clone()).class("column-type"));
        }
        header = header.child(cell);
    }
    wrapper = wrapper.child(header);
    for row_id in visible_rows(app) {
        let row = &grid.page.rows[(row_id - grid.page.start) as usize];
        let mut line = html::div().class("gridrow").key(format!("r{row_id}"));
        let first = cols.first().copied().unwrap_or(0);
        let row_cell = Cell {
            row: row_id,
            column: first,
        };
        let row_button = html::button((row_id + 1).to_string())
            .class(if grid.is_row_selected(row_id) {
                "rownum selected"
            } else {
                "rownum"
            })
            .actions(row_menu(app, row_cell, redis_rows, grid.editable, false))
            .on_click(Msg::Row(row_id, first))
            .on_action(
                "grid-open-key",
                Msg::Action("redis-open-current-key".into()),
            )
            .on_action(
                "grid-copy-rows",
                Msg::GridMenu(
                    row_cell,
                    if redis_rows {
                        menus::GridMenuAction::CopyRedisRows
                    } else {
                        menus::GridMenuAction::CopyRows
                    },
                ),
            )
            .on_action(
                "grid-copy-insert",
                Msg::GridMenu(row_cell, menus::GridMenuAction::CopyInsert),
            )
            .on_action(
                "grid-copy-insert-without-auto-primary",
                Msg::GridMenu(
                    row_cell,
                    menus::GridMenuAction::CopyInsertWithoutAutoPrimary,
                ),
            )
            .on_action(
                "grid-delete-rows",
                Msg::GridMenu(row_cell, menus::GridMenuAction::DeleteRows),
            );
        line = line.child(row_button);
        for &col in &cols {
            let cell = Cell {
                row: row_id,
                column: col,
            };
            let value = grid.value(cell).or_else(|| row.get(col));
            let text = value.map(|v| cell_preview(app, v)).unwrap_or_default();
            let selected = grid.is_selected(cell);
            let mut class = format!("cell col{col}");
            if selected {
                class.push_str(" selected");
            }
            if grid.edits().contains_key(&cell) {
                class.push_str(" edited");
            }
            if matches!(value, Some(datapier_core::model::Value::Null)) {
                class.push_str(" null");
            }
            if matches!(
                value,
                Some(
                    datapier_core::model::Value::Integer(_)
                        | datapier_core::model::Value::Unsigned(_)
                        | datapier_core::model::Value::Decimal(_)
                        | datapier_core::model::Value::Float(_)
                )
            ) {
                class.push_str(" numeric");
            }
            if app.workbench.inline == Some(cell) && app.large.is_none() {
                let ed = &app.workbench.cell_editor;
                let mut editor = html::div()
                    .class(format!("{class} inline-editing"))
                    .key(format!("c{col}"))
                    .child(
                        ui::input()
                            .variant(ui::InputVariant::Field)
                            .key("cell-editor")
                            .text(ed.text.clone())
                            .cursor(ed.cursor)
                            .anchor(ed.anchor)
                            .on_focus(Msg::Focus("cell".into()))
                            .on_undo(Msg::Undo("cell".into()))
                            .on_edit(|e| {
                                Msg::Edit(
                                    "cell".into(),
                                    e.from,
                                    e.to,
                                    e.text.clone(),
                                    e.cursor,
                                    e.len,
                                )
                            }),
                    );
                if let Some(error) = app
                    .workbench
                    .field_error
                    .as_ref()
                    .filter(|error| error.field == "cell")
                {
                    editor = editor.child(
                        html::span(error.message.render().into_owned()).class("grid-inline-error"),
                    );
                }
                line = line.child(editor);
            } else {
                let node = html::button(text)
                    .class(class)
                    .key(format!("c{col}"))
                    .actions(row_menu(app, cell, redis_rows, grid.editable, true).click("click"))
                    .on_action_with("click", move |event| {
                        Msg::Cell(row_id, col, event.mods.iter().any(|m| m == "shift"))
                    })
                    .on_dblclick(Msg::CellOpen(row_id, col))
                    .on_action(
                        "grid-open-key",
                        Msg::Action("redis-open-current-key".into()),
                    )
                    .on_action(
                        "grid-copy-cells",
                        Msg::GridMenu(cell, menus::GridMenuAction::CopyCells),
                    )
                    .on_action(
                        "grid-copy-rows",
                        Msg::GridMenu(
                            cell,
                            if redis_rows {
                                menus::GridMenuAction::CopyRedisRows
                            } else {
                                menus::GridMenuAction::CopyRows
                            },
                        ),
                    )
                    .on_action(
                        "grid-copy-insert",
                        Msg::GridMenu(cell, menus::GridMenuAction::CopyInsert),
                    )
                    .on_action(
                        "grid-copy-insert-without-auto-primary",
                        Msg::GridMenu(cell, menus::GridMenuAction::CopyInsertWithoutAutoPrimary),
                    )
                    .on_action(
                        "grid-delete-rows",
                        Msg::GridMenu(cell, menus::GridMenuAction::DeleteRows),
                    );
                line = line.child(node);
            }
        }
        wrapper = wrapper.child(line);
    }
    Some(wrapper.into())
}

#[cfg(test)]
#[path = "../../tests/unit/views/grid_view.rs"]
mod tests;
