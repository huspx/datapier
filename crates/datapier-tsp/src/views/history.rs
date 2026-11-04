use super::*;

pub(super) fn history_panel(app: &App) -> Node<Msg> {
    let history = &app.workbench.archery_history;
    let mut list = html::div().class("history-list");
    if history.pending.is_some() {
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
    let mut sizes = ui::select()
        .key("history-page-size")
        .value(history.limit.to_string())
        .disabled(loading)
        .open(app.workbench.generic_select.as_deref() == Some("history-page-size"))
        .on_toggle(|collapsed| Msg::Action(format!("select-open:history-page-size:{}", !collapsed)))
        .on_change(|change| {
            Msg::Action(format!(
                "history-size:{}",
                change.value.as_str().unwrap_or_default()
            ))
        });
    for size in [20_u64, 50, 100] {
        sizes = sizes.option(ui::SelectOption::new(size.to_string(), size.to_string()));
    }
    html::div()
        .class("table-footer history-pagination")
        .child(html::span(crate::i18n::format_message(
            "本页 {count} 条 · 共 {total} 条",
            &[
                ("count", &history.items.len().to_string()),
                ("total", &history.total.to_string()),
            ],
        )))
        .child(html::span("").class("spacer"))
        .child(ui::field(crate::i18n::tr("每页").into_owned()).child(sizes))
        .child(page_button(
            "arrow-left",
            "上一页",
            "history-prev",
            !loading && history.offset != 0,
        ))
        .child(html::span(crate::i18n::format_message(
            "第 {page} / {total} 页",
            &[
                (
                    "page",
                    &(history.offset / history.limit.max(1) + 1).to_string(),
                ),
                (
                    "total",
                    &history
                        .total
                        .div_ceil(history.limit.max(1))
                        .max(1)
                        .to_string(),
                ),
            ],
        )))
        .child(page_button(
            "arrow-right",
            "下一页",
            "history-next",
            !loading && history.offset.saturating_add(history.limit) < history.total,
        ))
        .into()
}

pub(super) fn history_footer(app: &App) -> html::El<Msg> {
    let can_open = app.history_selected_text().is_some();
    html::div()
        .class("modal-footer")
        .child(html::span("").class("spacer"))
        .child(button("关闭", "overlay-close"))
        .child(
            ui::button(crate::i18n::tr("打开为草稿").into_owned())
                .variant(ui::ButtonVariant::Primary)
                .disabled(!can_open)
                .on_click(Msg::Action("history-open".into())),
        )
}
