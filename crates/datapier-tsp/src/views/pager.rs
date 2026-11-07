use super::*;

/// 各结果视图共用的分页控件；未知总数不推测页数。
pub(super) struct Pager<'a> {
    pub key: &'static str,
    pub size_action: &'static str,
    pub previous_action: &'static str,
    pub next_action: &'static str,
    pub size: u64,
    pub page: u64,
    pub total: Option<u64>,
    pub can_previous: bool,
    pub can_next: bool,
    pub loading: bool,
    pub limited: bool,
    pub count_action: Option<&'a str>,
}

pub(super) fn render(app: &App, pager: Pager<'_>) -> Node<Msg> {
    let mut sizes = vec![20, 50, 100, 200, 500, 1000];
    if !sizes.contains(&pager.size) {
        sizes.push(pager.size);
        sizes.sort_unstable();
    }
    let key = pager.key;
    let action = pager.size_action;
    let mut select = ui::select()
        .key(key)
        .value(pager.size.to_string())
        .disabled(pager.loading)
        .open(app.workbench.generic_select.as_deref() == Some(key))
        .on_toggle(move |collapsed| Msg::Action(format!("select-open:{key}:{}", !collapsed)))
        .on_change(move |change| {
            Msg::Action(format!(
                "{action}:{}",
                change.value.as_str().unwrap_or_default()
            ))
        });
    for size in sizes {
        select = select.option(ui::SelectOption::new(size.to_string(), size.to_string()));
    }
    let indicator = match pager.total {
        Some(total) => crate::i18n::format_message(
            "第 {page} / {total} 页",
            &[
                ("page", &pager.page.to_string()),
                (
                    "total",
                    &total.div_ceil(pager.size.max(1)).max(1).to_string(),
                ),
            ],
        ),
        None => {
            crate::i18n::format_message("第 {number} 页", &[("number", &pager.page.to_string())])
        }
    };
    let mut controls = html::div()
        .class("result-pagination")
        .child(html::span(crate::i18n::tr("每页").into_owned()))
        .child(select)
        .child(page_button(
            "arrow-left",
            "上一页",
            pager.previous_action,
            !pager.loading && pager.can_previous,
        ))
        .child(html::span(indicator).class("page-indicator"))
        .child(page_button(
            "arrow-right",
            "下一页",
            pager.next_action,
            !pager.loading && pager.can_next,
        ));
    if let Some(total) = pager.total {
        controls = controls.child(
            html::span(crate::i18n::format_message(
                "共 {total} 条",
                &[("total", &total.to_string())],
            ))
            .class("muted"),
        );
    }
    if pager.limited {
        controls = controls
            .child(html::span(crate::i18n::tr("已达到内存上限").into_owned()).class("muted"));
    }
    if let Some(action) = pager.count_action {
        controls = controls.child(page_button(
            "calculator",
            "统计总数",
            action,
            !pager.loading,
        ));
    }
    controls.into()
}
