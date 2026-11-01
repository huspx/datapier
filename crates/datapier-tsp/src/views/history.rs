use super::*;

pub(super) fn history_panel(app: &App) -> Node<Msg> {
    let mut list = html::div().class("history-list");
    let needle = app.field("history_filter").to_lowercase();
    let mut count = 0;
    for (i, item) in app.workbench.history.iter().enumerate() {
        let text = item.get("text").and_then(Value::as_str).unwrap_or("");
        let state = task_state_text(item.get("state").and_then(Value::as_str).unwrap_or(""));
        let time = timestamp_text(item.get("created_at"));
        if !needle.is_empty()
            && !text.to_lowercase().contains(&needle)
            && !state.to_lowercase().contains(&needle)
            && !time.contains(&needle)
        {
            continue;
        }
        count += 1;
        list = list.child(
            html::button("")
                .class(if app.workbench.history_selected == Some(i) {
                    "history-item active"
                } else {
                    "history-item"
                })
                .child(
                    html::span(
                        text.lines()
                            .next()
                            .unwrap_or("")
                            .chars()
                            .take(160)
                            .collect::<String>(),
                    )
                    .class("history-summary"),
                )
                .child(html::span(format!("{time} · {state}")).class("history-meta"))
                .on_click(Msg::Action(format!("history-select:{i}"))),
        );
    }
    if count == 0 {
        list = list
            .child(html::p(crate::i18n::tr("没有匹配的历史记录").into_owned()).class("empty-note"));
    }
    let mut preview = html::div().class("history-preview").child(html::strong(
        crate::i18n::tr(if app.is_redis() {
            "命令预览"
        } else {
            "SQL 预览"
        })
        .into_owned(),
    ));
    if let Some(item) = app
        .workbench
        .history_selected
        .and_then(|i| app.workbench.history.get(i))
    {
        preview = preview
            .child(
                html::span(format!(
                    "{} · {}",
                    timestamp_text(item.get("created_at")),
                    task_state_text(item.get("state").and_then(Value::as_str).unwrap_or(""))
                ))
                .class("history-preview-meta"),
            )
            .child(
                html::pre(item.get("text").and_then(Value::as_str).unwrap_or("")).class("preview"),
            );
    } else {
        preview = preview.child(
            html::p(crate::i18n::tr("选择一条记录查看完整文本").into_owned()).class("empty-note"),
        );
    }
    html::div()
        .class("history-panel")
        .child(field(app, "history_filter", "搜索历史"))
        .child(
            html::span(crate::i18n::format_message(
                "{count} 条记录",
                &[("count", &count.to_string())],
            ))
            .class("history-count"),
        )
        .child(
            html::div()
                .class("history-layout")
                .child(list)
                .child(preview),
        )
        .into()
}
