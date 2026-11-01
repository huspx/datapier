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
        let text = if let datapier_core::model::Value::Bytes(encoded) = value {
            use base64::Engine as _;
            match base64::engine::general_purpose::STANDARD.decode(encoded) {
                Ok(bytes) if app.workbench.value_format == "hex" => {
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
                Ok(bytes) if app.workbench.value_format == "json" => {
                    match serde_json::from_slice::<Value>(&bytes) {
                        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_default(),
                        Err(error) => crate::i18n::format_message(
                            "JSON 解析失败：{error}",
                            &[("error", &error.to_string())],
                        ),
                    }
                }
                _ => value_text(value, true).into_owned(),
            }
        } else {
            value_text(value, true).into_owned()
        };
        panel = panel.child(html::pre(text).class("value-content"));
    }
    panel.into()
}
pub(super) fn cli_panel(app: &App) -> Node<Msg> {
    let mut head = html::div()
        .class("cli-head")
        .child(html::strong("CLI"))
        .child(
            html::span(format!(
                "{} · DB {}",
                app.profile
                    .as_ref()
                    .map_or("", |profile| profile.name.as_str()),
                app.profile.as_ref().map_or(0, |profile| profile.redis_db)
            ))
            .class("muted"),
        )
        .child(icon_button("history", "历史", "cli-history-toggle"));
    for (i, _) in app.workbench.cli_results.iter().enumerate() {
        head = head.child(button(
            &format!("{} {}", crate::i18n::tr("结果"), i + 1),
            &format!("cli-result:{i}"),
        ));
    }
    if let Some(grid) = &app.workbench.cli_grid {
        head = head.child(
            html::span(format!(
                "{}–{} / {}",
                grid.page.start + 1,
                grid.page.start + grid.page.rows.len() as u64,
                grid.page.result.row_count
            ))
            .class("muted"),
        );
    }
    let elapsed = app
        .workbench
        .cli_started_at
        .map(|started| started.elapsed())
        .or(app.workbench.cli_elapsed);
    head = head.child(html::span("").class("spacer"));
    if app.workbench.cli_running {
        head = head.child(html::span(crate::i18n::tr("执行中…").into_owned()).class("muted"));
    }
    if let Some(elapsed) = elapsed {
        head = head.child(html::span(format!("{} ms", elapsed.as_millis())).class("muted"));
    }
    let can_previous = app
        .workbench
        .cli_grid
        .as_ref()
        .is_some_and(|grid| grid.page.start > 0);
    let can_next = app.workbench.cli_grid.as_ref().is_some_and(|grid| {
        grid.page.start + (grid.page.rows.len() as u64) < grid.page.result.row_count
    });
    let panel = html::div().class("cli-panel").child(
        head.child(icon_button("stop", "取消", "cancel"))
            .child(page_button(
                "arrow-left",
                "上一页",
                "cli-previous",
                can_previous,
            ))
            .child(page_button("arrow-right", "下一页", "cli-next", can_next))
            .child(icon_button("chev", "收起", "cli-toggle")),
    );
    let mut body = html::div().class("cli-body");
    if let Some(command) = &app.workbench.cli_command {
        body = body.child(html::pre(command.clone()).class("cli-command"));
    }
    if app.workbench.cli_running {
        body = body.child(html::span(crate::i18n::tr("执行中…").into_owned()).class("muted"));
    }
    if !app.workbench.cli_error.is_empty() {
        body = body
            .child(html::span(app.workbench.cli_error.render().into_owned()).class("empty-note"));
    }
    if app.workbench.cli_history_expanded {
        let mut history = html::div().class("cli-history");
        for (i, item) in app.workbench.history.iter().enumerate() {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                history = history.child(
                    ui::button(text.to_owned())
                        .variant(ui::ButtonVariant::Quiet)
                        .size(ui::ButtonSize::Sm)
                        .on_click(Msg::Action(format!("history-open:{i}"))),
                );
            }
        }
        body = body.child(history);
    }
    if let Some(grid) = &app.workbench.cli_grid {
        let mut result = html::div().class("cli-results");
        let mut header = html::div().class("gridrow header");
        for col in &grid.page.result.columns {
            header = header.child(html::span(col.name.clone()).class("cli-cell"));
        }
        result = result.child(header);
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
                    _ => cell_preview(app, value),
                };
                line = line.child(
                    html::button(label)
                        .class("cli-cell")
                        .on_click(Msg::Action(format!("cli-open:{row_id}:{col}"))),
                );
            }
            result = result.child(line);
        }
        body = body.child(result);
    }
    panel
        .child(body)
        .child(
            html::div()
                .class("cli-prompt")
                .child(html::span(">"))
                .child(field(app, "cli_text", ""))
                .child(icon_button("play", "执行", "cli-run")),
        )
        .into()
}
