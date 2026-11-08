use super::*;

pub(super) fn history_panel(app: &App) -> Node<Msg> {
    let history = &app.workbench.archery_history;
    let mut list = html::div().class("history-list");
    if history.items.is_empty() && history.pending.is_some() {
        list =
            list.child(html::p(crate::i18n::tr("正在加载历史").into_owned()).class("empty-note"));
    } else if let Some(error) = &history.error {
        list = list.child(html::p(error.clone()).class("form-submit-error"));
        if history.auth_required {
            list = list.child(button("重新登录", "archery-history-reauth"));
        }
    } else if history.items.is_empty() {
        list = list
            .child(html::p(crate::i18n::tr("没有匹配的历史记录").into_owned()).class("empty-note"));
    } else {
        for (index, item) in history.items.iter().enumerate() {
            let text = crate::application::archery_history_text(item).unwrap_or("");
            let metadata = if history.visible {
                ["instance_name", "db_name", "database"]
                    .iter()
                    .filter_map(|key| item.get(*key).and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join(" · ")
            } else {
                format!(
                    "{} · {}",
                    timestamp_text(item.get("created_at")),
                    task_state_text(item.get("state").and_then(Value::as_str).unwrap_or(""))
                )
            };
            list = list.child(
                html::button("")
                    .class(if history.selected == Some(index) {
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
                    .child(html::span(metadata).class("history-meta"))
                    .on_click(Msg::Action(format!("history-select:{index}"))),
            );
        }
    }
    let mut preview = html::div().class("history-preview").child(html::strong(
        crate::i18n::tr(if app.is_redis() {
            "命令预览"
        } else {
            "SQL 预览"
        })
        .into_owned(),
    ));
    if history.text_pending.is_some() {
        preview = preview
            .child(html::p(crate::i18n::tr("正在加载完整文本").into_owned()).class("empty-note"));
    } else if let Some(error) = &history.text_error {
        preview = preview.child(html::p(error.clone()).class("form-submit-error"));
    } else if let Some(text) = app.history_selected_text() {
        preview = preview.child(html::pre(text).class("preview"));
    } else {
        preview = preview.child(
            html::p(crate::i18n::tr("选择一条记录查看完整文本").into_owned()).class("empty-note"),
        );
    }
    html::div()
        .class("history-panel")
        .child(html::strong(
            crate::i18n::tr(if history.visible {
                "平台历史：当前账号可见"
            } else {
                "本地历史：当前工作区"
            })
            .into_owned(),
        ))
        .child(
            html::div()
                .class("history-search")
                .child(field(app, "history_filter", "搜索历史"))
                .child(button("查询", "history-search")),
        )
        .child(
            html::div()
                .class("history-layout")
                .child(list)
                .child(preview),
        )
        .child(history_pagination(app))
        .into()
}

fn history_pagination(app: &App) -> Node<Msg> {
    let history = &app.workbench.archery_history;
    let loading = history.pending.is_some();
    html::div()
        .class("table-footer history-pagination")
        .child(html::span(crate::i18n::format_message(
            "本页 {count} 条",
            &[("count", &history.items.len().to_string())],
        )))
        .child(html::span("").class("spacer"))
        .child(super::pager::render(
            app,
            super::pager::Pager {
                key: "history-page-size",
                size_action: "history-size",
                previous_action: "history-prev",
                next_action: "history-next",
                size: history.limit.max(1),
                page: history.offset / history.limit.max(1) + 1,
                total: Some(history.total),
                can_previous: history.offset != 0,
                can_next: history.offset.saturating_add(history.limit) < history.total,
                loading,
                limited: false,
                count_action: None,
            },
        ))
        .into()
}

pub(super) fn history_footer(app: &App) -> html::El<Msg> {
    let reason = if app.workbench.archery_history.text_pending.is_some() {
        Some("正在加载完整文本")
    } else if app.history_selected_text().is_none() {
        Some("请先选择包含完整文本的记录")
    } else if app.tabs.get(app.active).is_none() {
        Some("请先连接数据库")
    } else if app.workbench.inline.is_some()
        || app.has_live_drafts()
        || !app.tabs[app.active].pending.is_null()
    {
        Some("请先提交或丢弃当前草稿")
    } else if app.task.is_some() || app.operation_pending || app.workbench.cli_running {
        Some("请等待或取消当前任务")
    } else {
        None
    };
    let mut footer = html::div().class("modal-footer");
    if let Some(reason) = reason {
        footer = footer.child(html::span(crate::i18n::tr(reason).into_owned()).class("muted"));
    }
    footer
        .child(html::span("").class("spacer"))
        .child(button("关闭", "overlay-close"))
        .child(
            ui::button(crate::i18n::tr("打开为草稿").into_owned())
                .variant(ui::ButtonVariant::Primary)
                .disabled(reason.is_some())
                .on_click(Msg::Action("history-open".into())),
        )
}
