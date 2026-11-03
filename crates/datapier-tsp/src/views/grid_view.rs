use super::*;

/// 结果范围属于缓存结果本身，不能随当前连接或 tab 切换而改变。
pub(super) fn result_provenance(result: &datapier_core::model::ResultMeta) -> Option<Node<Msg>> {
    let summary = &result.summary;
    if summary.get("origin")?.get("source")?.as_str()? != "archery" {
        return None;
    }
    let mut details = html::div().class("result-provenance").child(
        html::span(
            crate::i18n::tr(if result.complete {
                "平台本次返回结果已缓存"
            } else {
                "平台本次返回结果缓存未完成"
            })
            .into_owned(),
        )
        .class("muted"),
    );
    let origin = &summary["origin"];
    let coverage = &summary["coverage"];
    let fidelity = &summary["fidelity"];
    let missing = crate::i18n::tr("未提供").into_owned();
    let instance = origin
        .get("instance_id")
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_i64().map(|id| id.to_string()))
        })
        .unwrap_or_else(|| missing.clone());
    let database = origin
        .get("database")
        .and_then(Value::as_str)
        .unwrap_or(&missing);
    let number = |field: &str| {
        coverage
            .get(field)
            .and_then(Value::as_u64)
            .map(|value| value.to_string())
            .unwrap_or_else(|| missing.clone())
    };
    let scope = crate::i18n::tr(
        match coverage.get("original_query").and_then(Value::as_str) {
            Some("limited") => "原查询结果受限",
            _ => "未提供",
        },
    );
    let kind = crate::i18n::tr(match fidelity.get("kind").and_then(Value::as_str) {
        Some("platform_text") => "平台文本",
        Some("platform_values") => "平台值",
        _ => "未提供",
    });
    let masking = crate::i18n::tr(match summary.get("masking").and_then(Value::as_str) {
        Some("masked") => "已脱敏",
        Some("not_supported") => "平台不支持脱敏",
        _ => "未提供",
    });
    // 缓存数量和平台值不能证明原查询总量、原始类型或原始字节。
    for (label, value) in [
        ("来源", "Archery".to_owned()),
        ("实例", instance),
        ("数据库", database.to_owned()),
        ("请求上限", number("requested_limit")),
        ("已返回行数", number("returned_rows")),
        ("原查询总行数", missing.clone()),
        ("原始类型", missing.clone()),
        ("原始字节", missing.clone()),
        ("覆盖范围", scope.into_owned()),
        ("保真度", kind.into_owned()),
        ("脱敏状态", masking.into_owned()),
    ] {
        details = details.child(
            html::div()
                .class("task-fact")
                .child(html::span(crate::i18n::tr(label).into_owned()).class("muted"))
                .child(html::span(value).class("task-fact-value")),
        );
    }
    if let Some(lossy) = fidelity.get("lossy").and_then(Value::as_array) {
        let losses: Vec<_> = lossy
            .iter()
            .filter_map(Value::as_str)
            .map(|loss| {
                crate::i18n::tr(match loss {
                    "binary" => "二进制",
                    "bit" => "位类型",
                    "encoding" => "编码",
                    "masking" => "脱敏",
                    other => other,
                })
                .into_owned()
            })
            .collect();
        if !losses.is_empty() {
            details = details.child(
                html::div()
                    .class("task-fact")
                    .child(html::span(crate::i18n::tr("类型损失").into_owned()).class("muted"))
                    .child(html::span(losses.join(" · ")).class("task-fact-value")),
            );
        }
    }
    if summary.get("check_state").and_then(Value::as_str) == Some("completed") {
        let verdict = crate::i18n::tr(match summary.get("verdict").and_then(Value::as_str) {
            Some("pass") => "通过",
            Some("warning") => "有警告",
            Some("error") => "有错误",
            _ => "结论未知",
        });
        let warnings = summary
            .get("warning_count")
            .and_then(Value::as_u64)
            .map(|count| count.to_string())
            .unwrap_or_else(|| missing.clone());
        let errors = summary
            .get("error_count")
            .and_then(Value::as_u64)
            .map(|count| count.to_string())
            .unwrap_or_else(|| missing.clone());
        let check = crate::i18n::format_message(
            "检查完成 · {verdict} · 警告 {warnings} · 错误 {errors}",
            &[
                ("verdict", &verdict),
                ("warnings", &warnings),
                ("errors", &errors),
            ],
        );
        details = details.child(html::div().child(html::span(check)).class("muted"));
    }
    Some(details.into())
}

pub(super) fn result_info(
    app: &App,
    result: &datapier_core::model::ResultMeta,
) -> Option<Node<Msg>> {
    let details = result_provenance(result)?;
    let id = result.id.to_string();
    let label = crate::i18n::tr("结果说明").into_owned();
    let mut container = html::div()
        .class(if app.focus == "result_info" {
            "result-info-anchor focused"
        } else {
            "result-info-anchor"
        })
        .child(
            ui::button("")
                .key("result-info")
                .variant(ui::ButtonVariant::Quiet)
                .size(ui::ButtonSize::Md)
                .icon("info")
                .title(label.clone())
                .aria(label.clone())
                .on_focus(Msg::Focus("result_info".into()))
                .on_click(Msg::Action(format!("result-info-toggle:{id}"))),
        );
    if app.workbench.query_ui.result_info.as_deref() == Some(id.as_str()) {
        container = container.child(
            html::div()
                .class("result-info-popup")
                .attr("role", "dialog")
                .attr("aria-label", label.clone())
                .on_click(Msg::Action("result-info-keep".into()))
                .child(
                    html::div()
                        .class("result-info-head")
                        .child(html::strong(label))
                        .child(
                            ui::button("")
                                .variant(ui::ButtonVariant::Quiet)
                                .size(ui::ButtonSize::Md)
                                .icon("x")
                                .title(crate::i18n::tr("关闭结果说明").into_owned())
                                .aria(crate::i18n::tr("关闭结果说明").into_owned())
                                .on_click(Msg::Action("result-info-close".into())),
                        ),
                )
                .child(details),
        );
    }
    Some(container.into())
}

pub(super) fn archery_pagination(app: &App, grid: &GridState, cli: bool) -> Node<Msg> {
    let id = grid.page.result.id.to_string();
    let state = app.workbench.query_ui.pagination.get(&id);
    let size = state.map_or(20, |state| state.page_size);
    let key = if cli {
        "cli-result-size"
    } else {
        "result-size"
    };
    let kind = if cli { "cli" } else { "grid" };
    let mut select = ui::select()
        .key(key)
        .value(size.to_string())
        .open(app.workbench.generic_select.as_deref() == Some(key))
        .on_toggle(move |collapsed| Msg::Action(format!("select-open:{key}:{}", !collapsed)))
        .on_change(move |change| {
            Msg::Action(format!(
                "result-size:{kind}:{}",
                change.value.as_str().unwrap_or_default()
            ))
        });
    for size in [20, 50, 100] {
        select = select.option(ui::SelectOption::new(size.to_string(), size.to_string()));
    }
    let starts = state.map(|state| state.starts.as_slice()).unwrap_or(&[]);
    let page = starts.iter().position(|start| *start == grid.page.start);
    let indicator = page
        .map(|page| {
            crate::i18n::format_message(
                "第 {page} / {total} 页",
                &[
                    ("page", &(page + 1).to_string()),
                    ("total", &starts.len().to_string()),
                ],
            )
        })
        .unwrap_or_else(|| "…".into());
    let mut controls = html::div()
        .class("result-pagination")
        .child(html::span(crate::i18n::tr("每页").into_owned()))
        .child(select)
        .child(page_button(
            "arrow-left",
            "上一页",
            if cli { "cli-previous" } else { "previous" },
            page.is_some_and(|page| page > 0),
        ))
        .child(html::span(indicator).class("page-indicator"))
        .child(page_button(
            "arrow-right",
            "下一页",
            if cli { "cli-next" } else { "next" },
            page.is_some_and(|page| page + 1 < starts.len()),
        ));
    if state.is_some_and(|state| state.complete == Some(false)) {
        controls = controls.child(
            html::span(crate::i18n::tr("平台本次返回结果缓存未完成").into_owned()).class("muted"),
        );
    }
    let coverage = &grid.page.result.summary["coverage"];
    if let (Some(rows), Some(limit)) = (
        coverage.get("returned_rows").and_then(Value::as_u64),
        coverage.get("requested_limit").and_then(Value::as_u64),
    ) && rows >= limit
    {
        controls = controls.child(
            html::span(crate::i18n::tr("已达上限").into_owned())
                .class("muted")
                .attr(
                    "title",
                    crate::i18n::tr("可调大请求上限后重新执行；平台不保证返回更多").into_owned(),
                ),
        );
    }
    controls.into()
}

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

pub(super) fn footer(app: &App, grid: &GridState) -> Node<Msg> {
    let redis = app.is_redis();
    let mut footer = html::div().class("table-footer").child(
        html::span(crate::i18n::format_message(
            if grid.page.result.summary["origin"]["source"].as_str() == Some("archery") {
                "本页 {page} 行 · 已返回 {total} 行"
            } else {
                "本页 {page} 行 · 共 {total} 行"
            },
            &[
                ("page", &grid.page.rows.len().to_string()),
                ("total", &grid.page.result.row_count.to_string()),
            ],
        ))
        .class("muted"),
    );
    if grid.selected_row_count() > 0 {
        footer = footer.child(
            html::span(crate::i18n::format_message(
                "已选 {count} 行",
                &[("count", &grid.selected_row_count().to_string())],
            ))
            .class("selected-count"),
        );
    }
    footer = footer.child(html::span("").class("spacer"));
    if app.browse_result
        && (!redis
            || app
                .target
                .as_ref()
                .is_some_and(|target| target.kind != "string"))
    {
        footer = footer.child(
            html::span(crate::i18n::format_message(
                if redis {
                    "单击选行 · Shift 连选 · {modifier} 增选"
                } else if writable(app) && grid.editable {
                    "行号选行 · Shift 连选 · {modifier} 增选 · 双击单元格编辑"
                } else {
                    "行号选行 · Shift 连选 · {modifier} 增选"
                },
                &[("modifier", crate::i18n::modifier_label())],
            ))
            .class("selection-hint muted"),
        );
    }
    if grid.page.result.summary["origin"]["source"].as_str() == Some("archery") {
        footer = footer.child(grid_view::archery_pagination(app, grid, false));
    } else {
        if !redis && !app.browse_result {
            footer = footer
                .child(html::span(crate::i18n::tr("查询结果只读").into_owned()).class("muted"));
        }
        footer = footer
            .child(page_button(
                "arrow-left",
                "上一页",
                "previous",
                grid.page.start > 0,
            ))
            .child(
                html::span(crate::i18n::format_message(
                    "第 {number} 页",
                    &[("number", &(app.page_history.len() + 1).to_string())],
                ))
                .class("page-indicator"),
            )
            .child(page_button(
                "arrow-right",
                "下一页",
                "next",
                grid.page.start + (grid.page.rows.len() as u64) < grid.page.result.row_count,
            ));
    }
    footer = footer.child(
        ui::button("")
            .variant(ui::ButtonVariant::Quiet)
            .size(ui::ButtonSize::Md)
            .icon("copy")
            .title(crate::i18n::tr("复制选区").into_owned())
            .prop("aria-label", crate::i18n::tr("复制选区").into_owned())
            .disabled(grid.focus.is_none())
            .on_click(Msg::CopyGrid),
    );
    if !redis
        || app
            .target
            .as_ref()
            .is_some_and(|target| target.kind != "string")
    {
        footer =
            footer.child(flat_action("info", "单元格详情", "overlay:cell").class("status-action"));
    }
    footer.into()
}
