//! 连接上下文与工作区标签。
use super::*;

pub(super) fn header(app: &App) -> Node<Msg> {
    let redis = app.is_redis();
    let mut context = html::div().class("context");
    if let Some(profile) = &app.profile {
        context = context.child(connection_provider_icon(app, profile));
    }
    context = context.child(
        html::strong(
            app.profile
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| crate::i18n::tr("未选择连接").into_owned()),
        )
        .class("context-title"),
    );
    if !redis {
        context = context.child(
            html::span(format!(
                " / {}",
                app.database
                    .as_deref()
                    .map(str::to_owned)
                    .unwrap_or_else(|| crate::i18n::tr("未选择数据库").into_owned())
            ))
            .class("breadcrumb"),
        );
    }
    if redis && app.profile.is_some() {
        context = context.child(
            html::div()
                .class("db-picker")
                .child(redis_db_picker(app, false)),
        );
    }
    if let Some(profile) = &app.profile {
        context = context
            .child(html::span(if redis { "Redis" } else { "MySQL" }).class("tag"))
            .child(
                html::span(
                    crate::i18n::tr(
                        if profile.access_mode == datapier_core::model::AccessMode::ReadWrite {
                            "读写"
                        } else {
                            "只读"
                        },
                    )
                    .into_owned(),
                )
                .class("tag"),
            );
    }
    context.into()
}

pub(super) fn tabs(app: &App) -> Node<Msg> {
    let mut tab_strip = html::div().class("tab-strip").key("tab-strip");
    for (i, t) in app.tabs.iter().enumerate() {
        let left = format!("tab-close-left:{}", t.id);
        let right = format!("tab-close-right:{}", t.id);
        let all = format!("tab-close-all:{}", t.id);
        let target = if i == app.active {
            app.target.as_ref()
        } else {
            t.runtime.target.as_ref().or(t.browse.target.as_ref())
        };
        let view = if i == app.active {
            app.view.as_str()
        } else {
            t.runtime.view.as_str()
        };
        let icon = if view == "query" || target.is_none() {
            "terminal"
        } else if target.is_some_and(|target| target.kind == "table") {
            "table"
        } else if target.is_some_and(|target| target.kind == "view") {
            "eye"
        } else {
            "layers"
        };
        let title = html::button("")
            .class("tab-title")
            .child(ui::icon(icon))
            .child(html::span(t.title.clone()))
            .actions(
                ui::Actions::new()
                    .menu(ui::MenuEntry::header(
                        crate::i18n::tr("标签操作").into_owned(),
                    ))
                    .menu(
                        ui::MenuItem::new(&left, crate::i18n::tr("关闭左侧标签").into_owned())
                            .icon("arrow-left")
                            .disabled(i == 0),
                    )
                    .menu(
                        ui::MenuItem::new(&right, crate::i18n::tr("关闭右侧标签").into_owned())
                            .icon("arrow-right")
                            .disabled(i + 1 == app.tabs.len()),
                    )
                    .menu(ui::MenuEntry::Separator)
                    .menu(
                        ui::MenuItem::new(&all, crate::i18n::tr("关闭全部标签").into_owned())
                            .icon("x"),
                    ),
            )
            .on_action(left.clone(), Msg::Action(left))
            .on_action(right.clone(), Msg::Action(right))
            .on_action(all.clone(), Msg::Action(all))
            .on_click(Msg::Tab(i));
        let close = flat_action("x", "关闭标签", &format!("close-tab:{}", t.id)).class("tab-close");
        let tab = html::div()
            .class(if i == app.active { "tab active" } else { "tab" })
            .key(format!("tab{i}"));
        tab_strip = tab_strip.child(if cfg!(target_os = "macos") {
            tab.child(close).child(title)
        } else {
            tab.child(title).child(close)
        });
    }
    let tabs = html::div()
        .class("tabs")
        .key("tabs")
        .child(tab_strip)
        .child(icon_button("plus", "新建查询", "view-query"));
    tabs.into()
}
