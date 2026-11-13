use super::*;
fn sidebar_action(icon: &str, label: &str, action: &str, compact: bool) -> Node<Msg> {
    flat_action(icon, label, action)
        .class(if compact {
            "sidebar-action icon-only"
        } else {
            "sidebar-action"
        })
        .into()
}
pub(super) fn resource_panel(app: &App) -> Node<Msg> {
    let compact = app.navigation_compact();
    let toggle_label = crate::i18n::tr(if compact {
        "展开导航"
    } else {
        "收起导航"
    })
    .into_owned();
    let mut brand = html::button("")
        .class("brand")
        .key("navigation-toggle")
        .prop("title", toggle_label.clone())
        .attr("aria-label", toggle_label)
        .on_click(Msg::Action("navigation-toggle".into()));
    brand = if let Some(icons) = &app.icon_blobs {
        brand.child(
            html::span("").class("brand-mark").child(
                ui::image()
                    .blob(icons.brand.clone())
                    .alt("Datapier")
                    .w(36.0)
                    .h(36.0),
            ),
        )
    } else {
        brand.child(
            html::span("")
                .class("brand-mark")
                .child(ui::icon("database")),
        )
    };
    if !compact {
        brand = brand.child(html::strong("Datapier"));
    }
    let shell = html::div()
        .class(if compact {
            "sidebar compact"
        } else {
            "sidebar"
        })
        .key("sidebar")
        .child(brand);
    let mut panel = html::div().class("sidebar-scroll");
    let mut connection_tools = html::div().class("sidebar-section-title");
    if !compact {
        connection_tools = connection_tools
            .child(html::span(crate::i18n::tr("连接").into_owned()))
            .child(html::span("").class("spacer"));
    }
    panel = panel.child(connection_tools.child(icon_button("plus", "新建连接", "new-profile")));
    for (i, p) in app.profiles.iter().enumerate() {
        let active = app
            .profile
            .as_ref()
            .is_some_and(|selected| selected.id == p.id);
        let (state, reason) = app.navigation_connection_state(p.id);
        let connected = state == "已连接";
        let dot_class = match state {
            "已连接" => "connection-dot connected",
            "连接中" => "connection-dot connecting",
            "连接失败" => "connection-dot failed",
            _ => "connection-dot",
        };
        let state = crate::i18n::tr(state).into_owned();
        let (endpoint, source) = match &p.connection {
            datapier_core::model::ConnectionConfig::Direct(direct) => {
                (format!("{}:{}", direct.host, direct.port), None)
            }
            datapier_core::model::ConnectionConfig::Archery(archery) => {
                let server = app.archery.servers.iter().find(|server| {
                    server["id"]
                        .as_str()
                        .and_then(|id| Uuid::parse_str(id).ok())
                        == Some(archery.server_id)
                });
                let address = server
                    .and_then(|server| server["base_url"].as_str())
                    .filter(|address| !address.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| crate::i18n::tr("未提供").into_owned());
                let display = server
                    .and_then(|server| server["name"].as_str())
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&address)
                    .to_owned();
                (display, Some(address))
            }
        };
        let description = if let Some(address) = source {
            format!("{} · Archery · {} · {}", p.name, address, state)
        } else {
            format!("{} · {} · {}", p.name, endpoint, state)
        };
        let description = match reason.as_ref() {
            Some(reason) => format!("{description} · {reason}"),
            None => description,
        };
        let mut card = html::button("")
            .class(if active {
                "connection active"
            } else {
                "connection"
            })
            .key(format!("p{}", p.id))
            .prop("title", description.clone())
            .aria(description)
            .child(
                html::div()
                    .class("connection-icon")
                    .child(connection_provider_icon(app, p))
                    .child(
                        html::span("").class(dot_class).child(
                            html::span(if dot_class == "connection-dot failed" {
                                "!"
                            } else {
                                ""
                            })
                            .class("connection-dot-mark"),
                        ),
                    ),
            );
        if !compact {
            card = card.child(
                html::div()
                    .class("connection-info")
                    .child(
                        html::div()
                            .class("connection-name")
                            .child(html::strong(p.name.clone())),
                    )
                    .child(html::span(endpoint).class("muted")),
            );
        }
        let mut actions = ui::Actions::new()
            .menu(ui::MenuEntry::header(p.name.clone()))
            .menu(
                ui::MenuItem::new("settings", crate::i18n::tr("连接设置").into_owned())
                    .icon("gear"),
            )
            .menu(
                ui::MenuItem::new(
                    "profile-copy-name",
                    crate::i18n::tr("复制连接名称").into_owned(),
                )
                .icon("copy"),
            );
        if p.provider == datapier_core::model::ProviderKind::Mysql
            && matches!(
                &p.connection,
                datapier_core::model::ConnectionConfig::Direct(_)
            )
        {
            let writable = p.ensure_write().is_ok();
            actions = actions.menu(
                ui::MenuItem::new(
                    "profile-create-database",
                    crate::i18n::tr(if !writable {
                        "新建数据库…（只读连接）"
                    } else if !connected {
                        "新建数据库…（请先连接）"
                    } else {
                        "新建数据库…"
                    })
                    .into_owned(),
                )
                .disabled(!writable || !connected),
            );
            actions = actions.menu(ui::MenuItem::new(
                "profile-close",
                crate::i18n::tr("关闭连接").into_owned(),
            ));
        }
        actions = actions.menu(ui::MenuEntry::Separator).menu(
            ui::MenuItem::new("delete", crate::i18n::tr("删除连接").into_owned())
                .icon("trash")
                .danger(true),
        );
        panel = panel.child(
            card.actions(actions)
                .on_action(
                    "settings",
                    Msg::Action(format!("profile-settings:{}", p.id)),
                )
                .on_action("profile-copy-name", Msg::CopyText(p.name.clone()))
                .on_action(
                    "profile-create-database",
                    Msg::Action(format!("profile-create-database:{}", p.id)),
                )
                .on_action(
                    "profile-close",
                    Msg::Action(format!("profile-close:{}", p.id)),
                )
                .on_action(
                    "delete",
                    Msg::Action(format!("profile-delete-request:{}", p.id)),
                )
                .on_click(Msg::Profile(i)),
        );
    }
    if !compact && app.session.is_some() && !app.is_redis() {
        let ed = &app.fields["object_filter"];
        panel = panel.child(
            html::div()
                .class("object-search-row")
                .child(
                    html::div().class("object-search").child(
                        ui::input()
                            .variant(ui::InputVariant::Field)
                            .key("input-object_filter")
                            .aria(crate::i18n::tr("筛选表和视图").into_owned())
                            .text(ed.text.clone())
                            .cursor(ed.cursor)
                            .anchor(ed.anchor)
                            .placeholder(crate::i18n::tr("筛选表和视图").into_owned())
                            .on_focus(Msg::Focus("object_filter".into()))
                            .on_undo(Msg::Undo("object_filter".into()))
                            .on_edit(|e| {
                                Msg::Edit(
                                    "object_filter".into(),
                                    e.from,
                                    e.to,
                                    e.text.clone(),
                                    e.cursor,
                                    e.len,
                                )
                            }),
                    ),
                )
                .child(icon_button("repeat", "刷新资源", "navigation-refresh")),
        );
        let resource_tree = {
            let mut panel = html::div()
                .class("resource-tree")
                .attr("role", "tree")
                .attr("aria-label", crate::i18n::tr("数据库资源").into_owned());
            if let Some(tree) = app
                .profile
                .as_ref()
                .and_then(|p| app.navigation.databases.get(&p.id))
            {
                if tree.loading {
                    panel = panel.child(
                        html::span(crate::i18n::tr("加载中…").into_owned()).class("empty-note"),
                    );
                }
                if let Some(error) = &tree.error {
                    panel = panel.child(html::span(error.clone()).class("connection-error"));
                }
                let needle = app.field("object_filter").to_lowercase();
                let tree_items = app.workspace_tree_items();
                let tree_indices: std::collections::HashMap<_, _> = tree_items
                    .iter()
                    .enumerate()
                    .map(|(index, item)| (item.0.as_str(), index))
                    .collect();
                let focused = app
                    .focus
                    .strip_prefix("workspace-tree-")
                    .and_then(|s| s.parse::<usize>().ok())
                    .filter(|index| *index < tree_items.len())
                    .unwrap_or(0);
                for database in &tree.databases {
                    if app
                        .profile
                        .as_ref()
                        .and_then(|profile| {
                            app.profiles.iter().find(|saved| saved.id == profile.id)
                        })
                        .and_then(crate::navigation::navigation_visible_databases)
                        .is_some_and(|visible| !visible.contains(&database.name))
                    {
                        continue;
                    }
                    let node = tree.nodes.get(&database.name);
                    let db_index = tree_indices
                        .get(format!("navigation-db:{}", database.name).as_str())
                        .copied()
                        .unwrap_or(0);
                    let can_drop = app.profile.as_ref().is_some_and(|profile| {
                        !profile.is_archery()
                            && profile.access_mode == datapier_core::model::AccessMode::ReadWrite
                    });
                    let expanded = node.is_some_and(|node| node.expanded);
                    let mut database_menu = ui::Actions::new()
                        .menu(ui::MenuEntry::header(database.name.clone()))
                        .menu(
                            ui::MenuItem::new(
                                "toggle",
                                crate::i18n::tr("打开 / 折叠").into_owned(),
                            )
                            .icon(if expanded {
                                "chev"
                            } else {
                                "chev-r"
                            }),
                        )
                        .menu(
                            ui::MenuItem::new("refresh", crate::i18n::tr("刷新资源").into_owned())
                                .icon("repeat"),
                        )
                        .menu(
                            ui::MenuItem::new(
                                "database-copy-name",
                                crate::i18n::tr("复制名称").into_owned(),
                            )
                            .icon("copy"),
                        );
                    if !app
                        .profile
                        .as_ref()
                        .is_some_and(|profile| profile.is_archery())
                    {
                        database_menu = database_menu.menu(ui::MenuEntry::Separator).menu(
                            ui::MenuItem::new("delete", crate::i18n::tr("删除数据库").into_owned())
                                .icon("trash")
                                .danger(true)
                                .disabled(!can_drop),
                        );
                    }
                    panel = panel.child(
                        html::button("")
                            .class(
                                if app.profile.as_ref().is_some_and(|profile| {
                                    profile.is_archery()
                                        && profile.provider
                                            == datapier_core::model::ProviderKind::Mysql
                                        && app.database.as_deref() == Some(database.name.as_str())
                                }) {
                                    "resource nav-item database-node active"
                                } else {
                                    "resource nav-item database-node"
                                },
                            )
                            .prop("title", database.name.clone())
                            .attr("aria-label", database.name.clone())
                            .key(format!("input-workspace-tree-{db_index}"))
                            .attr("role", "treeitem")
                            .attr("aria-expanded", expanded)
                            .attr("tabindex", if db_index == focused { 0 } else { -1 })
                            .on_focus(Msg::Focus(format!("workspace-tree-{db_index}")))
                            .child(ui::icon(if expanded { "chev" } else { "chev-r" }))
                            .child(ui::icon("database"))
                            .child(html::span(database.name.clone()))
                            .actions(database_menu)
                            .on_action(
                                "toggle",
                                Msg::Action(format!("navigation-db:{}", database.name)),
                            )
                            .on_action(
                                "refresh",
                                Msg::Action(format!("navigation-db-refresh:{}", database.name)),
                            )
                            .on_action("database-copy-name", Msg::CopyText(database.name.clone()))
                            .on_action(
                                "delete",
                                Msg::Action(format!("navigation-drop-db:{}", database.name)),
                            )
                            .on_click(Msg::Action(format!("navigation-db:{}", database.name))),
                    );
                    if let Some(node) = node.filter(|node| node.expanded) {
                        if node.loading {
                            panel = panel.child(
                                html::span(crate::i18n::tr("加载中…").into_owned())
                                    .class("empty-note"),
                            );
                        }
                        if let Some(error) = &node.error {
                            panel =
                                panel.child(html::span(error.clone()).class("connection-error"));
                        }
                        if !needle.is_empty()
                            && !node.loading
                            && node.error.is_none()
                            && !node
                                .resources
                                .iter()
                                .any(|resource| resource.name.to_lowercase().contains(&needle))
                        {
                            panel = panel.child(
                                html::span(crate::i18n::tr("没有匹配对象").into_owned())
                                    .class("empty-note"),
                            );
                        }
                        for (kind, title) in
                            [("table", "表"), ("view", "视图"), ("unknown", "未知类型")]
                        {
                            let collapsed = node.collapsed_kinds.contains(kind);
                            let mut count = 0;
                            let mut items = html::div().class("resource-group-items");
                            for (index, resource) in node.resources.iter().enumerate() {
                                if resource.kind != kind
                                    || !resource.name.to_lowercase().contains(&needle)
                                {
                                    continue;
                                }
                                count += 1;
                                if collapsed {
                                    continue;
                                }
                                let action =
                                    format!("navigation-resource:{}:{index}", database.name);
                                let item_index =
                                    tree_indices.get(action.as_str()).copied().unwrap_or(0);
                                let selected = app.target.as_ref().is_some_and(|target| {
                                    target.name == resource.name
                                        && target.database == resource.database
                                        && target.kind == resource.kind
                                });
                                let mut menu = ui::Actions::new()
                                    .menu(ui::MenuEntry::header(resource.name.clone()))
                                    .menu(
                                        ui::MenuItem::new(
                                            "open",
                                            crate::i18n::tr("打开").into_owned(),
                                        )
                                        .icon("eye"),
                                    )
                                    .menu(
                                        ui::MenuItem::new(
                                            "structure",
                                            crate::i18n::tr("结构").into_owned(),
                                        )
                                        .icon("table"),
                                    )
                                    .menu(ui::MenuItem::new("ddl", "DDL").icon("code"))
                                    .menu(
                                        ui::MenuItem::new(
                                            "refresh",
                                            crate::i18n::tr("刷新资源").into_owned(),
                                        )
                                        .icon("repeat"),
                                    )
                                    .menu(
                                        ui::MenuItem::new(
                                            "resource-copy-name",
                                            crate::i18n::tr("复制名称").into_owned(),
                                        )
                                        .icon("copy"),
                                    );
                                if resource.kind == "table" {
                                    menu = menu.menu(
                                        ui::MenuItem::new(
                                            "triggers",
                                            crate::i18n::tr("触发器").into_owned(),
                                        )
                                        .icon("code"),
                                    );
                                }
                                if resource.kind == "table"
                                    && !app
                                        .profile
                                        .as_ref()
                                        .is_some_and(|profile| profile.is_archery())
                                {
                                    menu = menu.menu(ui::MenuEntry::Separator).menu(
                                        ui::MenuItem::new(
                                            "delete",
                                            crate::i18n::tr("删除表").into_owned(),
                                        )
                                        .icon("trash")
                                        .danger(true)
                                        .disabled(!can_drop),
                                    );
                                }
                                let resource_button = html::button("")
                                    .class(if selected {
                                        "resource nav-item tree-object active"
                                    } else {
                                        "resource nav-item tree-object"
                                    })
                                    .prop("title", resource.name.clone())
                                    .attr("aria-label", resource.name.clone())
                                    .key(format!("input-workspace-tree-{item_index}"))
                                    .attr("role", "treeitem")
                                    .attr("tabindex", if item_index == focused { 0 } else { -1 })
                                    .on_focus(Msg::Focus(format!("workspace-tree-{item_index}")))
                                    .child(ui::icon(if resource.kind == "view" {
                                        "eye"
                                    } else {
                                        "table"
                                    }))
                                    .child(html::span(resource.name.clone()))
                                    .actions(menu)
                                    .on_action("open", Msg::Action(action.clone()))
                                    .on_action(
                                        "structure",
                                        Msg::Action(format!(
                                            "navigation-resource-section:structure:{}:{index}",
                                            database.name
                                        )),
                                    )
                                    .on_action(
                                        "ddl",
                                        Msg::Action(format!(
                                            "navigation-resource-section:ddl:{}:{index}",
                                            database.name
                                        )),
                                    )
                                    .on_action(
                                        "triggers",
                                        Msg::Action(format!(
                                            "navigation-triggers:{}:{index}",
                                            database.name
                                        )),
                                    )
                                    .on_action(
                                        "refresh",
                                        Msg::Action(format!(
                                            "navigation-db-refresh:{}",
                                            database.name
                                        )),
                                    )
                                    .on_action(
                                        "resource-copy-name",
                                        Msg::CopyText(resource.name.clone()),
                                    )
                                    .on_action(
                                        "delete",
                                        Msg::Action(format!(
                                            "navigation-drop-table:{}:{index}",
                                            database.name
                                        )),
                                    )
                                    .on_click(Msg::Action(action));
                                items = items.child(resource_button);
                            }
                            let mut header = html::button("")
                                .class("resource-group-title")
                                .child(ui::icon(if collapsed { "chev-r" } else { "chev" }))
                                .child(html::span(crate::i18n::tr(title).into_owned()))
                                .child(html::span("").class("spacer"))
                                .child(html::span(count.to_string()).class("tree-count"));
                            if count != 0 {
                                let action = format!("navigation-group:{kind}:{}", database.name);
                                let index = tree_indices.get(action.as_str()).copied().unwrap_or(0);
                                header = header
                                    .key(format!("input-workspace-tree-{index}"))
                                    .attr("role", "treeitem")
                                    .attr("aria-expanded", !collapsed)
                                    .attr("tabindex", if index == focused { 0 } else { -1 })
                                    .on_focus(Msg::Focus(format!("workspace-tree-{index}")));
                            }
                            if count == 0 {
                                header = header.attr("disabled", true);
                            } else {
                                header = header.on_click(Msg::Action(format!(
                                    "navigation-group:{kind}:{}",
                                    database.name
                                )));
                            }
                            panel = panel.child(
                                html::div()
                                    .class("resource-group")
                                    .child(header)
                                    .child(items),
                            );
                        }
                    }
                }
            }
            panel
        };
        panel = panel.child(resource_tree);
        panel = panel
            .child(html::span(crate::i18n::tr("工作区").into_owned()).class("group-title"))
            .child(sidebar_action("terminal", "SQL 查询", "view-query", false))
            .child(sidebar_action("history", "查询历史", "history", false));
    }
    let footer = html::div()
        .class("sidebar-footer")
        .child(sidebar_action(
            "gear",
            "Datapier 设置",
            "app-settings",
            compact,
        ))
        .child(sidebar_action(
            "keyboard",
            "快捷键帮助",
            "overlay:shortcuts",
            compact,
        ))
        .child(sidebar_action("power", "退出", "quit", compact));
    shell.child(panel).child(footer).into()
}
