use super::*;
pub(super) fn string_value(app: &App) -> Node<Msg> {
    let mut panel = html::div()
        .class("string-value key-value")
        .child(html::strong("Value"));
    if let Some(grid) = &app.grid
        && let Some(value) = grid.value(Cell {
            row: grid.page.start,
            column: 0,
        })
    {
        let bytes = match value {
            datapier_core::model::Value::Bytes(encoded) => {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .ok()
                    .map(std::borrow::Cow::Owned)
            }
            datapier_core::model::Value::Text(text) => {
                Some(std::borrow::Cow::Borrowed(text.as_bytes()))
            }
            _ => None,
        };
        let text = match bytes.as_deref() {
            Some(bytes) if app.workbench.value_format == "hex" => {
                use std::fmt::Write as _;
                let mut text = String::with_capacity(bytes.len().saturating_mul(3));
                for (i, byte) in bytes.iter().enumerate() {
                    if i > 0 {
                        text.push(' ');
                    }
                    let _ = write!(text, "{byte:02X}");
                }
                text
            }
            Some(bytes) if app.workbench.value_format == "json" => {
                match serde_json::from_slice::<Value>(bytes) {
                    Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_default(),
                    Err(error) => {
                        panel = panel.child(
                            html::span(crate::i18n::format_message(
                                "JSON 解析失败：{error}",
                                &[("error", &error.to_string())],
                            ))
                            .class("empty-note"),
                        );
                        value_text(value, true).into_owned()
                    }
                }
            }
            _ => value_text(value, true).into_owned(),
        };
        panel = panel.child(html::pre(text).class("value-content"));
    }
    panel.into()
}
fn cli_context_preview(text: &str) -> std::borrow::Cow<'_, str> {
    let first = text.lines().next().unwrap_or_default();
    if let Some((end, _)) = first.char_indices().nth(32) {
        std::borrow::Cow::Owned(format!("{}…", &first[..end]))
    } else if first.len() != text.len() {
        std::borrow::Cow::Owned(format!("{first}…"))
    } else {
        std::borrow::Cow::Borrowed(first)
    }
}

pub(super) fn cli_panel(app: &App) -> Node<Msg> {
    let mut head = html::div()
        .class("cli-head")
        .child(html::strong("CLI"))
        .child(
            html::span(crate::i18n::format_message(
                "当前输入：{instance} · DB {database}",
                &[
                    (
                        "instance",
                        &cli_context_preview(
                            app.profile
                                .as_ref()
                                .map_or("", |profile| profile.name.as_str()),
                        ),
                    ),
                    (
                        "database",
                        &app.workbench
                            .cli_database
                            .unwrap_or_else(|| {
                                app.tabs[app.active].browse.redis_db.or_else(|| {
                                    app.profile.as_ref().map(|profile| profile.redis_db())
                                })
                            })
                            .map(|db| db.to_string())
                            .unwrap_or_else(|| crate::i18n::tr("未知").into_owned()),
                    ),
                ],
            ))
            .class("muted")
            .prop(
                "title",
                app.profile
                    .as_ref()
                    .map_or("", |profile| profile.name.as_str()),
            ),
        )
        .child(icon_button("history", "历史", "history"));
    head = head.child(page_button(
        "copy",
        "复制执行记录",
        "cli-copy",
        !app.workbench.cli_history.is_empty()
            && !app.workbench.cli_running
            && !app.operation_pending,
    ));
    if app
        .profile
        .as_ref()
        .is_some_and(|profile| profile.is_archery())
    {
        head = head.child(crate::query_ui::request_limit_picker(app));
    }
    if app
        .profile
        .as_ref()
        .is_some_and(|profile| !profile.is_archery())
    {
        let label = crate::i18n::tr("调整 CLI 高度").into_owned();
        head = head.child(
            ui::button("↕")
                .variant(ui::ButtonVariant::Quiet)
                .size(ui::ButtonSize::Sm)
                .key("cli-height")
                .title(label.clone())
                .aria(label)
                .on_focus(Msg::Focus("cli_height".into()))
                .on_click(Msg::Focus("cli_height".into())),
        );
    }
    head = head.child(html::span("").class("spacer"));
    if app.workbench.cli_running {
        head = head.child(html::span(crate::i18n::tr("执行中…").into_owned()).class("muted"));
    }
    head = head.child(page_button(
        "stop",
        if app.profile.as_ref().is_some_and(|p| p.is_archery()) {
            "停止等待"
        } else {
            "取消"
        },
        "cancel",
        app.workbench.cli_running || app.task.is_some(),
    ));
    if !app.profile.as_ref().is_some_and(|p| p.is_archery()) {
        head = head.child(icon_button("chev", "收起", "cli-toggle"));
    }
    let mut result_head = html::div().class("cli-head");
    if let Some(record) = app.workbench.cli_selected.and_then(|id| {
        app.workbench
            .cli_history
            .iter()
            .find(|record| record.id == id)
    }) && app.workbench.cli_result_id.is_some()
    {
        let elapsed = record
            .elapsed
            .map(|value| format!(" · {} ms", value.as_millis()))
            .unwrap_or_default();
        let database = app
            .workbench
            .cli_result_id
            .as_ref()
            .and_then(|id| app.workbench.cli_result_databases.get(id))
            .map(u32::to_string)
            .unwrap_or_else(|| record.database.clone());
        result_head = result_head.child(
            html::span(crate::i18n::format_message(
                "定位结果：{command} · {instance} · DB {database}{elapsed}",
                &[
                    ("command", cli_context_preview(&record.command).as_ref()),
                    ("instance", cli_context_preview(&record.instance).as_ref()),
                    ("database", &database),
                    ("elapsed", &elapsed),
                ],
            ))
            .class("muted")
            .prop("title", record.command.clone()),
        );
    }
    if let Some(grid) = &app.workbench.cli_grid {
        result_head = result_head.child(
            html::span(crate::i18n::format_message(
                "本页 {count} 行",
                &[("count", &grid.page.rows.len().to_string())],
            ))
            .class("muted"),
        );
    }
    result_head = result_head.child(html::span("").class("spacer"));
    let info = app
        .workbench
        .cli_grid
        .as_ref()
        .and_then(|grid| super::grid_view::result_info(app, &grid.page.result));
    result_head = result_head.child(page_button(
        "download",
        "导出",
        "cli-export",
        app.workbench.cli_result_id.is_some(),
    ));
    if let Some(grid) = &app.workbench.cli_grid {
        result_head = result_head.child(super::grid_view::result_pagination(app, grid, true));
    }
    if let Some(info) = info {
        result_head = result_head.child(info);
    }
    let mut header = html::div().class("cli-header-wrap").child(head);
    if app.workbench.cli_result_id.is_some() {
        header = header.child(result_head);
    }
    let panel = html::div()
        .class(if app.profile.as_ref().is_some_and(|p| p.is_archery()) {
            "cli-panel cli-full"
        } else {
            "cli-panel"
        })
        .child(header);
    let mut body = html::div().class("cli-body").key("cli-body");
    for record in &app.workbench.cli_history {
        let mut entry = html::div()
            .class("cli-entry")
            .key(format!("cli-entry-{}", record.id))
            .child(html::pre(record.command.clone()).class("cli-command"))
            .child(
                html::span(format!(
                    "{} · DB {} · {}{}",
                    record.instance,
                    record.database,
                    record.state.render(),
                    record
                        .elapsed
                        .map(|elapsed| format!(" · {} ms", elapsed.as_millis()))
                        .unwrap_or_default()
                ))
                .class("muted"),
            );
        if !record.error.is_empty() {
            entry = entry.child(html::span(record.error.render().into_owned()).class("empty-note"));
        }
        for (result_index, grid) in record.result_grids.iter().enumerate() {
            let grid = if app.workbench.cli_selected == Some(record.id)
                && record.results.get(result_index).map(String::as_str)
                    == app.workbench.cli_result_id.as_deref()
            {
                app.workbench.cli_grid.as_ref()
            } else {
                grid.as_ref()
            };
            let Some(grid) = grid else { continue };
            let action = format!("cli-record:{}:{result_index}", record.id);
            let mut result = html::div()
                .class("cli-results")
                .key(format!("cli-result-{}-{result_index}", record.id))
                .child(
                    ui::button(crate::i18n::tr("定位执行记录").into_owned())
                        .variant(ui::ButtonVariant::Quiet)
                        .aria(format!(
                            "{} · {}",
                            crate::i18n::tr("定位执行记录"),
                            record.command
                        ))
                        .on_click(Msg::Action(action)),
                );
            if let Some(db) = app
                .workbench
                .cli_result_databases
                .get(&grid.page.result.id.to_string())
            {
                result = result.child(html::span(format!("DB {db}")).class("muted"));
            }
            let mut header = html::div().class("gridrow header");
            for col in &grid.page.result.columns {
                header = header.child(html::span(col.name.clone()).class("cli-cell"));
            }
            result = result.child(header);
            if grid.page.rows.is_empty() {
                result = result.child(
                    html::span(crate::i18n::tr("没有结果").into_owned()).class("empty-note"),
                );
            }
            for (i, row) in grid.page.rows.iter().enumerate() {
                let row_id = grid.page.start + i as u64;
                let mut line = html::div().class("gridrow");
                for (col, value) in row.iter().enumerate() {
                    let cell = Cell {
                        row: row_id,
                        column: col,
                    };
                    let value = grid.value(cell).unwrap_or(value);
                    let label = match value {
                        datapier_core::model::Value::Large { bytes, .. } => {
                            crate::i18n::format_message(
                                "大值（{bytes} 字节），点击读取",
                                &[("bytes", &bytes.to_string())],
                            )
                        }
                        _ => {
                            let text = cell_preview(app, value);
                            if text.is_empty() { "\"\"".into() } else { text }
                        }
                    };
                    if app.workbench.cli_selected == Some(record.id)
                        && app
                            .workbench
                            .cli_grid
                            .as_ref()
                            .is_some_and(|current| current.page.result.id == grid.page.result.id)
                    {
                        line = line.child(
                            html::button(label)
                                .class("cli-cell")
                                .on_click(Msg::Action(format!("cli-open:{row_id}:{col}"))),
                        );
                    } else {
                        line = line.child(html::span(label).class("cli-cell"));
                    }
                }
                result = result.child(line);
            }
            entry = entry.child(result);
        }
        body = body.child(entry);
    }
    panel
        .child(body)
        .child(
            html::div()
                .class("cli-prompt")
                .child(html::span(">"))
                .child(field(app, "cli_text", ""))
                .child(page_button(
                    "play",
                    "执行",
                    "cli-run",
                    !app.workbench.cli_running && !app.operation_pending,
                ))
                .child(if app.workbench.cli_running || app.operation_pending {
                    html::span(crate::i18n::tr("执行中，暂不能修改输入或再次执行").into_owned())
                        .class("muted")
                } else {
                    html::span("")
                }),
        )
        .into()
}

pub(super) fn resource_controls(app: &App) -> (Node<Msg>, Node<Msg>) {
    let target = app.target.as_ref().unwrap();
    let can_edit = writable(app) && app.browse_result && app.edit_token.is_some();
    let ttl = app
        .schema
        .as_ref()
        .and_then(|schema| schema.get("ttl_ms"))
        .and_then(Value::as_i64);
    let kind = app
        .schema
        .as_ref()
        .and_then(|schema| schema.get("type"))
        .and_then(Value::as_str)
        .unwrap_or(&target.kind);
    let type_label = match kind {
        "string" => "String",
        "hash" => "Hash",
        "list" => "List",
        "set" => "Set",
        "zset" => "ZSet",
        "none" => "不存在",
        _ => kind,
    };
    let type_label = if kind == "none" {
        crate::i18n::tr("不存在").into_owned()
    } else {
        type_label.to_owned()
    };
    let mut metadata = html::div().class("key-meta");
    if let Some(length) = app
        .schema
        .as_ref()
        .and_then(|schema| schema.get("length"))
        .and_then(Value::as_u64)
    {
        metadata = metadata.child(html::span(format!(
            "{length} {}",
            crate::i18n::tr(match kind {
                "string" => "字节",
                "hash" => "字段",
                _ => "项",
            })
        )));
    }
    metadata = metadata.child(html::span(format!("TTL {}", ttl_label(ttl))));
    metadata = metadata.child(page_button(
        "edit",
        "编辑 TTL",
        "redis-op:set_ttl",
        writable(app),
    ));
    let mut heading = html::div()
        .class("key-heading")
        .child(html::span(type_label).class("tag"))
        .child(html::strong(target.name.clone()));
    heading = heading.child(page_button(
        "edit",
        "编辑 key 名称",
        "redis-op:rename",
        writable(app),
    ));
    let copy_label = crate::i18n::tr("复制完整 key").into_owned();
    heading = heading.child(
        ui::button("")
            .variant(ui::ButtonVariant::Quiet)
            .size(ui::ButtonSize::Sm)
            .icon("copy")
            .key("copy-key")
            .title(copy_label.clone())
            .aria(copy_label)
            .on_click(Msg::CopyText(workbench::redis_key_text(
                &target.name,
                target.key_b64.as_deref(),
            ))),
    );
    heading = heading.child(page_button(
        "trash",
        "删除 key",
        "redis-op:delete_key",
        writable(app),
    ));
    let title = html::div()
        .class("key-title")
        .child(heading)
        .child(metadata);
    let mut tools = html::div().class("key-actions");
    if app.target.as_ref().is_some_and(|t| t.kind == "string") {
        let mut formats = ui::tabs()
            .variant(ui::TabsVariant::Underline)
            .active(app.workbench.value_format.clone());
        for (value, label) in [("text", "Text"), ("json", "JSON"), ("hex", "HEX")] {
            formats = formats.tab(value, label);
        }
        tools = tools.child(formats.on_select(|value| Msg::Action(format!("format:{value}"))));
        if app.grid.as_ref().is_some_and(|grid| {
            matches!(
                grid.value(Cell {
                    row: grid.page.start,
                    column: 0
                }),
                Some(datapier_core::model::Value::Large { .. })
            )
        }) {
            tools = tools.child(flat_action("info", "读取完整值", "redis-view-string"));
        }
    } else {
        tools = tools.child(
            html::span(match kind {
                "hash" => "Fields",
                "list" => "Items",
                "set" | "zset" => "Members",
                _ => "Value",
            })
            .class("muted"),
        );
    }
    tools = tools.child(html::span("").class("spacer"));
    if matches!(kind, "string" | "hash" | "list" | "set" | "zset") && app.grid.is_some() {
        tools = tools.child(button("高级值编辑", "redis-advanced-value-open"));
    }
    tools = tools.child(page_button("edit", "编辑 value", "redis-edit", can_edit));
    {
        let operations: &[(&str, &str, &str)] = match kind {
            "hash" => &[("plus", "设置 Hash 字段", "hash_set")],
            "list" => &[("plus", "插入 List 元素", "list_insert")],
            "set" => &[("plus", "添加 Set 成员", "set_add")],
            "zset" => &[("plus", "设置 ZSet 成员", "zset_set")],
            _ => &[],
        };
        for (icon, label, operation) in operations {
            tools = tools.child(page_button(
                icon,
                label,
                &format!("redis-op:{operation}"),
                writable(app),
            ));
        }
        if matches!(kind, "hash" | "list" | "set" | "zset") {
            let label = crate::i18n::tr("删除选中条目").into_owned();
            tools = tools.child(
                ui::button("")
                    .variant(ui::ButtonVariant::Quiet)
                    .size(ui::ButtonSize::Md)
                    .icon("trash")
                    .role("redis-delete")
                    .key("redis-row-delete")
                    .title(label.clone())
                    .aria(label)
                    .disabled(!writable(app) || !app.redis_row_delete_enabled())
                    .on_click(Msg::Action("redis-row-delete".into())),
            );
        }
    }
    if app.grid.is_some() {
        tools = tools.child(icon_button("columns", "列配置", "overlay:columns"));
    }
    tools = tools.child(
        ui::button(crate::i18n::tr("批量替换").into_owned())
            .disabled(!writable(app))
            .on_click(Msg::Action("replace-open".into())),
    );
    tools = tools.child(page_button(
        "arrow-up",
        "导入",
        "overlay:import",
        import_writable(app),
    ));
    if !writable(app) {
        tools = tools
            .child(html::span(crate::i18n::tr("只读连接不能修改数据").into_owned()).class("muted"));
    }
    if writable(app) && !can_edit {
        tools = tools.child(
            html::span(crate::i18n::tr("当前 Redis 资源没有可用的编辑基线").into_owned())
                .class("muted"),
        );
    }
    tools = tools.child(icon_button("download", "导出", "overlay:export"));
    let value_copy_label = crate::i18n::tr("复制原始 value（不含草稿）").into_owned();
    tools = tools.child(
        ui::button("")
            .variant(ui::ButtonVariant::Quiet)
            .size(ui::ButtonSize::Sm)
            .icon("copy")
            .key("copy-value")
            .title(value_copy_label.clone())
            .aria(value_copy_label)
            .on_click(Msg::CopyRedisValue),
    );
    (title.into(), tools.into())
}
