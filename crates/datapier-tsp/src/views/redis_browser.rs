use super::*;
fn redis_key_row(
    app: &App,
    i: usize,
    label: String,
    tree_index: Option<(usize, usize)>,
) -> Node<Msg> {
    let r = &app.resources[i];
    let full = crate::workbench::redis_key_text(&r.name, r.key_b64.as_deref());
    let active = app.redis_key_selected(i);
    let identity = format!(
        "{}:{}:{i}:{}",
        app.tabs[app.active].id,
        app.generation,
        r.key_b64.as_deref().unwrap_or("!")
    );
    let ttl = r.data.get("ttl_ms").and_then(Value::as_i64);
    let short_type = match r.kind.as_str() {
        "string" => "STR",
        "hash" => "HASH",
        "list" => "LIST",
        "set" => "SET",
        "zset" => "ZSET",
        _ => r.kind.as_str(),
    };
    let mut actions = ui::Actions::new()
        .menu(ui::MenuEntry::header(full.clone()))
        .menu(ui::MenuItem::new("open", crate::i18n::tr("打开 key").into_owned()).icon("eye"))
        .menu(
            ui::MenuItem::new(
                "redis-key-copy-selected",
                crate::i18n::tr("复制选中 key").into_owned(),
            )
            .icon("copy"),
        );
    actions = actions.menu(
        ui::MenuItem::new("delete", crate::i18n::tr("删除选中 key").into_owned())
            .icon("trash")
            .danger(true)
            .disabled(!writable(app)),
    );
    let mut button = html::button("")
        .class(if active {
            "resource key-row active"
        } else {
            "resource key-row"
        })
        .key(format!("r{i}"))
        .prop(
            "title",
            format!("{} · {} · TTL {}", full, r.kind, ttl_label(ttl)),
        )
        .child(html::span(label).class("key-name"))
        .child(
            html::span("")
                .class("key-aside")
                .child(html::span(short_type).class("key-type"))
                .child(html::span(ttl_short(ttl)).class("key-ttl")),
        )
        .on_click(Msg::Action(format!("redis-key-click:{identity}")))
        .actions(actions)
        .on_action(
            "open",
            Msg::Action(format!("redis-key-menu:{identity}:open")),
        )
        .on_action(
            "redis-key-copy-selected",
            Msg::Action(format!("redis-key-menu:{identity}:copy")),
        )
        .on_action(
            "delete",
            Msg::Action(format!("redis-key-menu:{identity}:delete")),
        );
    if let Some((index, focused)) = tree_index {
        button = button
            .key(format!("input-workspace-tree-{index}"))
            .attr("role", "treeitem")
            .attr("tabindex", if index == focused { 0 } else { -1 })
            .on_focus(Msg::Focus(format!("workspace-tree-{index}")));
    }
    button.into()
}
fn redis_tree_nodes(app: &App, rows: &[crate::redis_tree::Row<'_>]) -> Node<Msg> {
    let mut container = html::div().class("key-tree-nodes").attr("role", "tree");
    let focused = app
        .focus
        .strip_prefix("workspace-tree-")
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|index| *index < rows.len())
        .unwrap_or(0);
    for (position, row) in rows.iter().enumerate() {
        let label = if row.label.is_empty() {
            "∅".to_owned()
        } else {
            crate::redis_tree::display(row.label)
        };
        let node = match &row.kind {
            crate::redis_tree::Kind::Key { index } => {
                redis_key_row(app, *index, label, Some((position, focused)))
            }
            crate::redis_tree::Kind::Directory { prefix, expanded } => {
                let action = format!("redis-tree:{}", crate::redis_tree::encoded(prefix));
                html::button("")
                    .class("resource key-directory")
                    .key(format!("input-workspace-tree-{position}"))
                    .attr("role", "treeitem")
                    .attr("tabindex", if position == focused { 0 } else { -1 })
                    .on_focus(Msg::Focus(format!("workspace-tree-{position}")))
                    .prop("title", crate::redis_tree::display(prefix))
                    .attr("aria-expanded", *expanded)
                    .child(ui::icon(if *expanded { "chev" } else { "chev-r" }))
                    .child(html::span(label).class("key-name"))
                    .on_click(Msg::Action(action))
                    .into()
            }
        };
        container = container.child(
            html::div()
                .class("key-tree-row")
                .child(html::span("").class("key-tree-indent").prop(
                    "min",
                    json!({"w": format!("{}ch", row.depth.saturating_mul(2))}),
                ))
                .child(node),
        );
    }
    container.into()
}
pub(super) fn redis_browser(app: &App) -> Node<Msg> {
    let options = [
        ("all", "全部"),
        ("string", "String"),
        ("hash", "Hash"),
        ("list", "List"),
        ("set", "Set"),
        ("zset", "ZSet"),
    ];
    let mut type_select = ui::select()
        .value(
            if app.field("type").is_empty() {
                "all"
            } else {
                app.field("type")
            }
            .to_owned(),
        )
        .open(app.workbench.key_type_menu)
        .on_toggle(|collapsed| Msg::Action(format!("key-type-menu-open:{}", !collapsed)));
    for (value, label) in options {
        type_select = type_select.option(ui::SelectOption::new(
            value,
            crate::i18n::tr(label).into_owned(),
        ));
    }
    let pattern = &app.fields["pattern"];
    let mut list = html::div()
        .class("key-browser")
        .child(
            html::div()
                .class("key-tools")
                .child(
                    ui::input()
                        .variant(ui::InputVariant::Field)
                        .key("input-pattern")
                        .aria(crate::i18n::tr("Key pattern，例如 user:*").into_owned())
                        .text(pattern.text.clone())
                        .cursor(pattern.cursor)
                        .anchor(pattern.anchor)
                        .placeholder(crate::i18n::tr("Key pattern，例如 user:*").into_owned())
                        .on_focus(Msg::Focus("pattern".into()))
                        .on_undo(Msg::Undo("pattern".into()))
                        .on_edit(|e| {
                            Msg::Edit(
                                "pattern".into(),
                                e.from,
                                e.to,
                                e.text.clone(),
                                e.cursor,
                                e.len,
                            )
                        }),
                )
                .child(
                    html::div()
                        .class("key-tool-row")
                        .child(type_select.on_change(|change| {
                            let value = change.value.as_str().unwrap_or_default();
                            Msg::Action(format!(
                                "key-type:{}",
                                if value == "all" { "" } else { value }
                            ))
                        }))
                        .child(icon_button("repeat", "刷新", "list")),
                ),
        )
        .child(
            html::div()
                .class("key-list-title")
                .child(html::span(if app.selected_redis_key_count() == 0 {
                    crate::i18n::format_message(
                        "{count} 个已加载 key",
                        &[("count", &app.resources.len().to_string())],
                    )
                } else {
                    crate::i18n::format_message(
                        "已加载 {total} · 已选 {selected}",
                        &[
                            ("total", &app.resources.len().to_string()),
                            ("selected", &app.selected_redis_key_count().to_string()),
                        ],
                    )
                }))
                .child(html::span("").class("spacer"))
                .child({
                    let mut modes = html::div().class("key-view-toggle");
                    for (tree, label, icon) in
                        [(false, "列表视图", "list"), (true, "树视图", "tree")]
                    {
                        let mut mode = ui::button("")
                            .variant(ui::ButtonVariant::Quiet)
                            .icon(icon)
                            .size(ui::ButtonSize::Sm)
                            .on(app.workbench.tree == tree)
                            .title(crate::i18n::tr(label).into_owned())
                            .aria(crate::i18n::tr(label).into_owned());
                        if app.workbench.tree != tree {
                            mode = mode.on_click(Msg::Action("tree-toggle".into()));
                        }
                        modes = modes.child(mode);
                    }
                    modes
                }),
        );
    let tree_rows = app.workbench.tree.then(|| {
        if !app.redis_tree_matches_resources() {
            return Vec::new();
        }
        let tree = &app.workbench.redis_tree;
        tree.rows(
            &app.workbench.redis_expansion,
            app.profile.as_ref().map(|profile| {
                (
                    profile.id,
                    app.tabs[app.active]
                        .browse
                        .redis_db
                        .unwrap_or_else(|| profile.redis_db()),
                )
            }),
        )
    });
    let count = tree_rows.as_ref().map_or(app.resources.len(), Vec::len);
    let page_count = count.div_ceil(workbench::KEY_PAGE_SIZE).max(1);
    let page = app.workbench.key_page.min(page_count - 1);
    if page_count > 1 {
        list = list.child(
            html::div()
                .class("key-pagination")
                .child(button("上一页", "keys-previous"))
                .child(
                    html::span(crate::i18n::format_message(
                        "第 {page} / {total} 页",
                        &[
                            ("page", &(page + 1).to_string()),
                            ("total", &page_count.to_string()),
                        ],
                    ))
                    .class("muted"),
                )
                .child(button("下一页", "keys-next")),
        );
    }
    let mut keys = html::div().class("key-list");
    if app.resources.is_empty() {
        let message = if app.workbench.scan_dirty {
            "条件已更改，请刷新"
        } else if app.scan_token.is_some() {
            "扫描中"
        } else if app.cursor.is_some() {
            "可加载更多"
        } else {
            "没有匹配的 key"
        };
        keys = keys.child(html::span(crate::i18n::tr(message).into_owned()).class("empty-note"));
    }
    if !writable(app) {
        keys = keys.child(
            html::span(crate::i18n::tr("只读连接不能删除 key").into_owned()).class("empty-note"),
        );
    }
    if let Some(rows) = tree_rows.as_ref() {
        let start = page * workbench::KEY_PAGE_SIZE;
        let end = (start + workbench::KEY_PAGE_SIZE).min(rows.len());
        keys = keys.child(redis_tree_nodes(app, &rows[start..end]));
    } else {
        let start = page * workbench::KEY_PAGE_SIZE;
        let end = (start + workbench::KEY_PAGE_SIZE).min(app.resources.len());
        for i in start..end {
            let r = &app.resources[i];
            keys = keys.child(redis_key_row(
                app,
                i,
                crate::workbench::redis_key_text(&r.name, r.key_b64.as_deref()),
                None,
            ));
        }
    }
    list.child(keys)
        .child(
            html::div()
                .child(html::span(crate::i18n::format_message(
                    "已加载 {count} 个 key · {state}",
                    &[
                        ("count", &app.resources.len().to_string()),
                        (
                            "state",
                            &crate::i18n::tr(if app.workbench.scan_dirty {
                                "条件已更改，请刷新"
                            } else if app.scan_token.is_some() {
                                "扫描中"
                            } else if app.cursor.is_some() {
                                "可加载更多"
                            } else {
                                "扫描完成"
                            }),
                        ),
                    ],
                )))
                .class("scan-note")
                .prop(
                    "title",
                    crate::i18n::tr("SCAN 增量加载，不保证快照；命名空间只统计已加载项。")
                        .into_owned(),
                )
                .child(page_button(
                    "arrow-right",
                    "继续扫描",
                    "more",
                    app.cursor.is_some() && app.scan_token.is_none() && !app.workbench.scan_dirty,
                )),
        )
        .into()
}
