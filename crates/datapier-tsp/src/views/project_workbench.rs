use super::*;
fn input(app: &App, name: &str, label: &str) -> Node<Msg> {
    let field_name = app.project_key(name);
    let browse: &[(&str, &str, &str)] = match name {
        "path" => &[("directory", "", "浏览…"), ("parent", "", "选择父目录")],
        "local-root" => &[("directory", "", "浏览…")],
        "destination" => &[("parent", "", "浏览…")],
        "archive" if app.workbench.project.project.is_none() => &[("file", "zip", "浏览…")],
        "archive" | "artifact-path" => &[("save", "", "浏览…")],
        _ => &[],
    };
    let field = if browse.is_empty() {
        field(app, &field_name, label)
    } else {
        controls::path_field(app, &field_name, label, browse)
    };
    if name == "query-text" {
        html::div().class("workbench-wide").child(field).into()
    } else {
        field
    }
}
fn text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => "—".into(),
    }
}
fn line(label: &str, value: &serde_json::Value) -> Node<Msg> {
    crate::advanced::fact(
        label,
        if matches!(label, "状态" | "资源状态") {
            crate::advanced::status(value.as_str().unwrap_or("unknown"))
        } else {
            text(value)
        },
    )
}
fn changes(items: &serde_json::Value, recovery: bool) -> Node<Msg> {
    let mut body = html::div();
    if let Some(items) = items.as_array() {
        for (i, item) in items.iter().enumerate() {
            let mut row = html::div()
                .class("project-file-group")
                .child(html::strong(format!(
                    "{} {}: {}",
                    crate::i18n::tr("文件"),
                    i + 1,
                    text(&item["path"])
                )))
                .child(crate::advanced::diagnostic(
                    &format!("project-file-change-{i}"),
                    item,
                ));
            if recovery {
                let status = match item["status"].as_str() {
                    Some("old") => "当前为旧版",
                    Some("new") => "当前为新版",
                    Some("conflict") => "内容冲突",
                    Some("staged") => "待发布",
                    _ => "未知",
                };
                row = row
                    .child(line("当前摘要", &item["current_digest"]))
                    .child(html::span(format!(
                        "{}: {}",
                        crate::i18n::tr("文件状态"),
                        crate::i18n::tr(status)
                    )));
            }
            if !item["old_bytes"].is_null() {
                row = row.child(line("旧大小（字节）", &item["old_bytes"]));
            }
            if !item["new_bytes"].is_null() {
                row = row.child(line("新大小（字节）", &item["new_bytes"]));
            }
            if !recovery {
                if let (Some(before), Some(after)) = (
                    item["comparison"]["before"].as_str(),
                    item["comparison"]["after"].as_str(),
                ) {
                    row = row.child(crate::advanced::text_diff(
                        before,
                        after,
                        item["path"].as_str().unwrap_or_default(),
                    ));
                    if item["comparison"]["complete"] != true {
                        row = row.child(
                            html::span(crate::i18n::tr("仅显示有界前缀").into_owned())
                                .class("notice"),
                        );
                    }
                } else if item["path"]
                    .as_str()
                    .is_some_and(|path| path.to_ascii_lowercase().ends_with(".sql"))
                {
                    row = row.child(
                        html::span(crate::i18n::tr("比较内容超出预览额度").into_owned())
                            .class("notice"),
                    );
                }
            }
            body = body.child(row);
        }
    }
    body.into()
}
fn document_choices(app: &App) -> Node<Msg> {
    let state = &app.workbench.project;
    let mut body = html::div();
    if let Some(root) = &state.document_root {
        body = body
            .child(html::strong(crate::i18n::tr("项目说明文件").into_owned()))
            .child(html::span(root.clone()));
        if state.document_files.is_empty() {
            body = body.child(html::span(
                crate::i18n::tr("目录中没有说明文件").into_owned(),
            ));
        }
        for (index, name) in state.document_files.iter().enumerate() {
            let mut row = html::div().class("project-local-row").child(
                flat_action("file", name, &format!("project-local-document:{index}"))
                    .class("workbench-entry"),
            );
            if crate::project_workbench::is_project_markdown(std::path::Path::new(name)) {
                row = row
                    .child(local_icon_button(
                        app,
                        "并排预览 Markdown",
                        "columns",
                        &format!("project-local-document-beside:{index}"),
                    ))
                    .child(local_icon_button(
                        app,
                        "临时覆盖预览 Markdown",
                        "file",
                        &format!("project-local-document-over:{index}"),
                    ));
            }
            body = body.child(row);
        }
        let mut actions = html::div()
            .class("toolbar")
            .child(button("预览说明文件", "project-local-document-open"));
        if crate::project_workbench::is_project_markdown(std::path::Path::new(
            app.field(&app.project_key("document-path")),
        )) {
            actions = actions
                .child(button("并排预览 Markdown", "project-local-document-beside"))
                .child(button(
                    "临时覆盖预览 Markdown",
                    "project-local-document-over",
                ));
        }
        body = body
            .child(input(app, "document-path", "项目内文件相对路径"))
            .child(actions);
    }
    body.into()
}

fn local_button(app: &App, label: &str, action: &str) -> Node<Msg> {
    ui::button(crate::i18n::tr(label).into_owned())
        .key(if action == "overlay-close-continue" {
            "overlay-close-safe".to_owned()
        } else {
            action.to_owned()
        })
        .disabled(app.workbench.project.is_pending())
        .on_click(Msg::Action(action.to_owned()))
        .into()
}
fn local_icon_button(app: &App, label: &str, icon: &str, action: &str) -> Node<Msg> {
    let label = crate::i18n::tr(label).into_owned();
    ui::button("")
        .variant(ui::ButtonVariant::Quiet)
        .icon(icon)
        .title(label.clone())
        .aria(label)
        .disabled(app.workbench.project.is_pending())
        .on_click(Msg::Action(action.to_owned()))
        .into()
}
fn local_copy_root(path: &str) -> Node<Msg> {
    let label = crate::i18n::tr("复制").into_owned();
    ui::button("")
        .variant(ui::ButtonVariant::Quiet)
        .icon("copy")
        .title(label.clone())
        .aria(label)
        .on_click(Msg::CopyText(path.to_owned()))
        .into()
}

fn local_panel(app: &App) -> Node<Msg> {
    let state = &app.workbench.project;
    let mut list = html::div().class("project-local-list").child(
        html::div()
            .class("project-local-heading")
            .child(html::strong(crate::i18n::tr("项目").into_owned()))
            .child(
                html::div()
                    .class("project-local-actions")
                    .child(local_icon_button(
                        app,
                        "新建项目",
                        "plus",
                        "project-local-new",
                    ))
                    .child(local_icon_button(
                        app,
                        "刷新项目列表",
                        "refresh",
                        "project-local-refresh",
                    )),
            ),
    );
    if state.local_loaded && state.local_projects.is_empty() {
        list = list.child(html::span(crate::i18n::tr("尚无本机项目").into_owned()).class("muted"));
    }
    for (index, project) in state.local_projects.iter().enumerate() {
        let selected = state
            .local_project
            .as_ref()
            .is_some_and(|current| current["id"] == project["id"]);
        list = list.child(
            html::button(project["name"].as_str().unwrap_or("").to_owned())
                .key(format!("project-local-select:{index}"))
                .class(if selected {
                    "project-local-item active"
                } else {
                    "project-local-item"
                })
                .attr("disabled", state.is_pending())
                .on_click(Msg::Action(format!("project-local-select:{index}"))),
        );
    }
    let mut content = html::div().class("project-local-content");
    if let Some(project) = state.local_project.as_ref() {
        let mut roots = html::div().class("project-local-roots");
        if let Some(items) = project["roots"].as_array() {
            if items.is_empty() {
                roots = roots.child(
                    html::span(match crate::i18n::resolved() {
                        crate::i18n::Language::ZhCn => "未关联仓库",
                        crate::i18n::Language::En => "No repository associated",
                    })
                    .class("muted"),
                );
            }
            for root in items {
                let path = root["path"].as_str().unwrap_or("");
                roots = roots.child(
                    html::div()
                        .class("project-local-root")
                        .child(ui::icon("folder"))
                        .child(html::span(path.to_owned()))
                        .child(local_copy_root(path)),
                );
            }
        }
        content = content.child(
            html::div()
                .class("project-local-title")
                .child(
                    html::div()
                        .class("project-local-project-info")
                        .child(html::strong(
                            project["name"].as_str().unwrap_or("").to_owned(),
                        ))
                        .child(roots),
                )
                .child(local_icon_button(
                    app,
                    "项目设置",
                    "gear",
                    "project-local-settings",
                )),
        );
        let mut tabs = ui::tabs().key("project-environments").active(
            state
                .local_environment
                .map(|i| i.to_string())
                .unwrap_or_default(),
        );
        if let Some(environments) = project["environments"].as_array() {
            for (index, environment) in environments.iter().enumerate() {
                let mut title = environment["name"].as_str().unwrap_or("").to_owned();
                if project["default_environment_id"] == environment["id"] {
                    title.push_str(&format!(" · {}", crate::i18n::tr("Agent 默认环境")));
                }
                tabs = tabs.tab(index.to_string(), title);
            }
        }
        content =
            content.child(
                html::div()
                    .class("project-local-heading project-local-tabs")
                    .child(tabs.on_select(|value| {
                        Msg::Action(format!("project-local-environment:{value}"))
                    }))
                    .child(local_icon_button(
                        app,
                        "新建环境",
                        "plus",
                        "project-local-environment-new",
                    )),
            );
        if let Some(environment) = state
            .local_environment
            .and_then(|i| project["environments"].get(i))
        {
            let mut actions = html::div()
                .class("project-local-actions")
                .child(local_icon_button(
                    app,
                    "编辑环境",
                    "edit",
                    "project-local-environment-edit",
                ))
                .child(local_icon_button(
                    app,
                    "移除环境",
                    "trash",
                    "project-local-environment-delete",
                ));
            actions = if project["default_environment_id"] == environment["id"] {
                actions.child(local_icon_button(
                    app,
                    "取消默认环境",
                    "star",
                    "project-local-clear-default",
                ))
            } else {
                actions.child(local_icon_button(
                    app,
                    "设为 Agent 默认环境",
                    "star",
                    "project-local-default",
                ))
            };
            content = content
                .child(
                    html::div()
                        .class("project-local-heading")
                        .child(
                            html::div().child(
                                html::p(
                                    environment["description"]
                                        .as_str()
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or("—")
                                        .to_owned(),
                                )
                                .class("muted"),
                            ),
                        )
                        .child(actions),
                )
                .child(
                    html::div()
                        .class("project-local-heading")
                        .child(html::strong(format!(
                            "{} ({})",
                            crate::i18n::tr("已关联数据库"),
                            environment["resources"].as_array().map_or(0, Vec::len)
                        )))
                        .child(local_icon_button(
                            app,
                            "关联数据库",
                            "plus",
                            "project-local-resource-new",
                        )),
                );
            if let Some(resources) = environment["resources"].as_array() {
                if resources.is_empty() {
                    content = content.child(
                        html::p(crate::i18n::tr("尚未关联数据库").into_owned()).class("muted"),
                    );
                }
                if !resources.is_empty() {
                    content = content.child(
                        html::div()
                            .class("project-local-row project-local-columns")
                            .child(
                                html::span(crate::i18n::tr("实例").into_owned())
                                    .class("project-local-instance"),
                            )
                            .child(
                                html::span(crate::i18n::tr("数据库").into_owned())
                                    .class("project-local-database"),
                            )
                            .child(
                                html::span(crate::i18n::tr("资源用途").into_owned())
                                    .class("project-local-purpose"),
                            )
                            .child(
                                html::span(crate::i18n::tr("操作").into_owned())
                                    .class("project-local-column-actions"),
                            ),
                    );
                }
                for (index, resource) in resources.iter().enumerate() {
                    let profile = resource["profile_id"]
                        .as_str()
                        .and_then(|id| app.profiles.iter().find(|p| p.id.to_string() == id));
                    let connection = profile.map(|p| p.name.as_str()).unwrap_or("");
                    let database = resource["database"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| resource["redis_db"].as_u64().map(|db| format!("DB {db}")))
                        .unwrap_or_else(|| crate::i18n::tr("使用连接默认值").into_owned());
                    let mut instance = html::div()
                        .class("project-local-instance")
                        .child(ui::icon("database"))
                        .child(html::span(connection.to_owned()));
                    if resource["binding_status"] == "invalid" {
                        instance = instance.child(
                            html::span(crate::i18n::tr("连接引用失效").into_owned()).class("muted"),
                        );
                    }
                    content = content.child(
                        html::div()
                            .class("project-local-row")
                            .child(instance)
                            .child(html::span(database).class("project-local-database"))
                            .child(
                                html::span(
                                    resource["description"]
                                        .as_str()
                                        .filter(|s| !s.is_empty())
                                        .unwrap_or("—")
                                        .to_owned(),
                                )
                                .class("project-local-purpose muted"),
                            )
                            .child(
                                html::div()
                                    .class("project-local-actions project-local-column-actions")
                                    .child(local_icon_button(
                                        app,
                                        "编辑",
                                        "edit",
                                        &format!("project-local-resource:{index}"),
                                    ))
                                    .child(local_icon_button(
                                        app,
                                        "移除关联",
                                        "trash",
                                        &format!("project-local-resource-delete:{index}"),
                                    )),
                            ),
                    );
                }
            }
        }
    } else {
        content =
            content.child(html::p(crate::i18n::tr("请选择或新建项目").into_owned()).class("muted"));
    }
    if let Some(error) = &state.local_error {
        content = content.child(html::p(error.render().into_owned()).class("notice"));
    }
    let status = app.status.render();
    if !status.is_empty() {
        content = content.child(html::p(status.into_owned()).class("notice"));
    }
    html::div()
        .class("project-local-layout")
        .child(list)
        .child(content)
        .into()
}

pub(super) fn local_dialog(app: &App) -> Node<Msg> {
    let state = &app.workbench.project;
    let form = state.local_form.as_str();
    let title = match form {
        "project-new" => "新建项目",
        "settings" => "项目设置",
        "root-add" | "root-cancel-confirm" => "添加仓库目录",
        "environment-new" => "新建环境",
        "environment-edit" => "编辑环境",
        _ => "关联数据库",
    };
    let mut panel = html::div().class("modal-body project-local-dialog");
    let mut save = None;
    if app.workbench.overlay_close_confirm {
        panel = panel.child(html::p(
            crate::i18n::tr("关闭将丢弃未应用的输入。继续编辑或丢弃？").into_owned(),
        ));
    } else {
        match form {
            "project-new" => {
                panel = panel
                    .child(input(app, "local-name", "项目名称"))
                    .child(input(app, "local-description", "项目说明"));
                save = Some("project-local-create");
            }
            "settings" => {
                panel = panel
                    .child(input(app, "local-name", "项目名称"))
                    .child(input(app, "local-description", "项目说明"));
                for (index, root) in state.local_roots.iter().enumerate() {
                    panel = panel.child(
                        html::div()
                            .class("project-local-row")
                            .child(html::span(root["path"].as_str().unwrap_or("").to_owned()))
                            .child(local_icon_button(
                                app,
                                "移除仓库关联",
                                "trash",
                                &format!("project-local-root-delete:{index}"),
                            ))
                            .child(local_icon_button(
                                app,
                                "查看说明文件",
                                "file",
                                &format!("project-local-documents:{index}"),
                            )),
                    );
                }
                panel = panel
                    .child(
                        html::div()
                            .class("project-local-actions")
                            .child(local_icon_button(
                                app,
                                "添加仓库目录",
                                "plus",
                                "project-local-root-open",
                            ))
                            .child(local_icon_button(
                                app,
                                "删除本机项目",
                                "trash",
                                "project-local-delete",
                            )),
                    )
                    .child(document_choices(app));
                if state.local_delete_confirm {
                    panel = panel
                        .child(html::p(
                            crate::i18n::tr("仅删除本机项目关联，不删除仓库或连接。").into_owned(),
                        ))
                        .child(local_button(
                            app,
                            "确认删除本机项目",
                            "project-local-delete-confirm",
                        ))
                        .child(local_button(app, "取消", "project-local-delete-cancel"));
                }
                save = Some("project-local-save");
            }
            "root-add" => {
                panel = panel.child(input(app, "local-root", "仓库目录"));
                save = Some("project-local-root-add");
            }
            "root-cancel-confirm" => {
                panel = panel.child(html::p(
                    crate::i18n::tr("关闭将丢弃未应用的输入。继续编辑或丢弃？").into_owned(),
                ));
            }
            "environment-new" | "environment-edit" => {
                panel = panel
                    .child(input(app, "local-environment-name", "环境名称"))
                    .child(choices(
                        app,
                        &app.project_key("local-environment-kind"),
                        "环境类型",
                        &[
                            ("dev", "开发"),
                            ("test", "测试"),
                            ("staging", "预发布"),
                            ("production", "生产"),
                            ("custom", "自定义"),
                        ],
                    ))
                    .child(input(app, "local-environment-description", "环境用途"));
                if app.field(&app.project_key("local-environment-kind")) == "custom" {
                    panel = panel.child(input(app, "local-environment-type", "自定义环境类型"));
                }
                save = Some(if form == "environment-new" {
                    "project-local-environment-add"
                } else {
                    "project-local-environment-save"
                });
            }
            "resource" => {
                if state.local_resource_edit.is_none() {
                    let mut projects = ui::select()
                        .aria(crate::i18n::tr("项目").into_owned())
                        .value(
                            state
                                .local_project
                                .as_ref()
                                .and_then(|p| p["id"].as_str())
                                .unwrap_or("")
                                .to_owned(),
                        )
                        .disabled(state.is_pending())
                        .open(app.workbench.generic_select.as_deref() == Some("project-target"))
                        .on_toggle(|collapsed| {
                            Msg::Action(format!("select-open:project-target:{}", !collapsed))
                        });
                    for project in &state.local_projects {
                        projects = projects.option(ui::SelectOption::new(
                            project["id"].as_str().unwrap_or(""),
                            project["name"].as_str().unwrap_or(""),
                        ));
                    }
                    panel = panel.child(ui::field(crate::i18n::tr("项目").into_owned()).child(
                        projects.on_change(|change| {
                            Msg::Action(format!(
                                "project-local-target:{}",
                                change.value.as_str().unwrap_or_default()
                            ))
                        }),
                    ));
                    if let Some(project) = &state.local_project {
                        let mut tabs = ui::tabs().key("project-resource-environments").active(
                            state
                                .local_environment
                                .map(|i| i.to_string())
                                .unwrap_or_default(),
                        );
                        if let Some(environments) = project["environments"].as_array() {
                            for (index, environment) in environments.iter().enumerate() {
                                tabs = tabs.tab(
                                    index.to_string(),
                                    environment["name"].as_str().unwrap_or("").to_owned(),
                                );
                            }
                        }
                        panel = panel.child(tabs.on_select(|value| {
                            Msg::Action(format!("project-local-environment:{value}"))
                        }));
                    }
                }
                if state.local_environment.is_none() {
                    panel = panel.child(html::p(
                        crate::i18n::tr("请先新建环境，再关联数据库。").into_owned(),
                    ));
                }
                if app.profiles.is_empty() {
                    panel = panel.child(html::p(
                        crate::i18n::tr("请先在侧栏新建连接，再关联数据库。").into_owned(),
                    ));
                } else {
                    let ids: Vec<_> = app.profiles.iter().map(|p| p.id.to_string()).collect();
                    let mut options = vec![("", "请选择连接")];
                    for (profile, id) in app.profiles.iter().zip(&ids) {
                        options.push((id.as_str(), profile.name.as_str()));
                    }
                    panel = panel.child(choices(
                        app,
                        &app.project_key("local-profile"),
                        "连接",
                        &options,
                    ));
                    if let Some(profile) = app
                        .profiles
                        .iter()
                        .find(|p| p.id.to_string() == app.field(&app.project_key("local-profile")))
                    {
                        let redis = profile.provider == datapier_core::model::ProviderKind::Redis;
                        let (field, label) = if redis {
                            ("local-redis-db", "Redis DB")
                        } else {
                            ("local-database", "MySQL 数据库")
                        };
                        let discovery = &state.local_database_discovery;
                        if discovery.token.is_some() {
                            panel = panel.child(
                                html::p(crate::i18n::tr("加载中…").into_owned()).class("muted"),
                            );
                        } else if let Some(error) = &discovery.error {
                            panel = panel.child(
                                html::div()
                                    .class("project-local-feedback")
                                    .child(
                                        html::p(error.clone())
                                            .class("notice")
                                            .attr("role", "alert"),
                                    )
                                    .child(local_icon_button(
                                        app,
                                        "重试",
                                        "refresh",
                                        "project-local-database-retry",
                                    )),
                            );
                        } else if discovery.loaded && discovery.profile_id == profile.id.to_string()
                        {
                            if discovery.databases.is_empty() {
                                panel = panel.child(
                                    html::p(match crate::i18n::resolved() {
                                        crate::i18n::Language::ZhCn => "没有可访问的数据库",
                                        crate::i18n::Language::En => "No accessible databases",
                                    })
                                    .class("muted"),
                                );
                            }
                            let mut choices_list = vec![("", "请选择数据库")];
                            for db in &discovery.databases {
                                choices_list.push((db.as_str(), db.as_str()));
                            }
                            panel = panel.child(choices(
                                app,
                                &app.project_key(field),
                                label,
                                &choices_list,
                            ));
                            let selected = app.field(&app.project_key(field));
                            if !selected.is_empty()
                                && !discovery.databases.iter().any(|db| db == selected)
                            {
                                panel = panel.child(
                                    html::p(crate::i18n::tr("连接引用失效").into_owned())
                                        .class("notice")
                                        .attr("role", "alert"),
                                );
                            } else if state.local_environment.is_some() && !selected.is_empty() {
                                save = Some("project-local-resource-save");
                            }
                        }
                        panel = panel.child(input(app, "local-resource-description", "资源用途"));
                    } else if !app.field(&app.project_key("local-profile")).is_empty() {
                        panel = panel.child(
                            html::p(crate::i18n::tr("连接引用失效").into_owned())
                                .class("notice")
                                .attr("role", "alert"),
                        );
                    }
                }
            }
            _ => {}
        }
        let status = app.status.render();
        if state.is_pending() {
            panel = panel.child(html::p(crate::i18n::tr("处理中").into_owned()).class("muted"));
        }
        if !status.is_empty() {
            panel = panel.child(
                html::p(status.into_owned())
                    .class("notice")
                    .attr("role", "alert"),
            );
        }
    }
    let mut footer = html::div()
        .class("modal-footer")
        .child(html::span("").class("spacer"));
    if app.workbench.overlay_close_confirm {
        footer = footer
            .child(local_button(app, "继续编辑", "overlay-close-continue"))
            .child(local_button(app, "丢弃", "overlay-close-discard"));
    } else if form == "root-cancel-confirm" {
        footer = footer
            .child(local_button(app, "继续编辑", "project-local-root-continue"))
            .child(local_button(app, "丢弃", "project-local-root-discard"));
    } else {
        footer = footer.child(local_button(
            app,
            "取消",
            if form == "root-add" {
                "project-local-root-cancel"
            } else {
                "overlay-close"
            },
        ));
        if let Some(action) = save {
            footer = footer.child(local_button(app, "保存", action));
        }
    }
    let overlay: Node<Msg> = ui::overlay()
        .modal(true)
        .size(ui::OverlaySize::Lg)
        .head(crate::i18n::tr(title).into_owned())
        .end(
            ui::button("")
                .variant(ui::ButtonVariant::Quiet)
                .icon("x")
                .disabled(state.is_pending())
                .title(crate::i18n::tr("关闭对话框").into_owned())
                .aria(crate::i18n::tr("关闭对话框").into_owned())
                .on_click(Msg::Action("overlay-close".into())),
        )
        .foot(Node::from(footer))
        .child(panel.key("body"))
        .into();
    html::div()
        .class(format!(
            "modal-layer {}{}",
            app.viewport_class(),
            if app.workbench.generic_select.is_some() {
                " modal-picker-open"
            } else {
                ""
            }
        ))
        .key("modal")
        .child(overlay)
        .into()
}
pub(super) fn panel(app: &App) -> Node<Msg> {
    let section = if app.workbench.project.section.is_empty() {
        "local"
    } else {
        app.workbench.project.section.as_str()
    };
    let project = app.workbench.project.project.as_ref().or(app
        .workbench
        .project
        .restored
        .as_ref());
    let mut root = html::div()
        .class("body detail-panel project-panel")
        .key("project-workbench")
        .attr("role", "region")
        .attr("aria-label", crate::i18n::tr("项目").into_owned())
        .child(html::strong(crate::i18n::tr("项目").into_owned()));
    let top = ui::tabs()
        .key("project-entry-tabs")
        .active(if section == "local" {
            "local"
        } else {
            "overview"
        })
        .tab("local", crate::i18n::tr("MCP 项目").into_owned())
        .tab("overview", crate::i18n::tr("项目文件").into_owned())
        .on_select(|value| Msg::Action(format!("project-ui-section:{value}")));
    root = root.child(top);
    if section == "local" {
        return root.child(local_panel(app)).into();
    }
    let mut summary = html::div().class("project-summary advanced-facts");
    if let Some(project) = project {
        summary = summary
            .child(crate::advanced::fact(
                "名称",
                text(&project["manifest"]["name"]),
            ))
            .child(crate::advanced::fact("项目目录", text(&project["root"])));
    } else {
        summary = summary.child(html::span(crate::i18n::tr("未打开项目").into_owned()));
    }
    if app.workbench.project.unsaved {
        summary = summary
            .child(html::span(crate::i18n::tr("共享内容待保存").into_owned()).class("notice"));
    }
    root = root.child(summary);
    if app.workbench.project.external.is_some() || app.workbench.project.needs_baseline_check {
        root = root.child(html::div().class("project-error").child(html::span(
            crate::i18n::tr("请在保存分区核对外部版本并确认基线。").into_owned(),
        )));
    }
    let mut tabs = ui::tabs().key("project-tabs").active(section.to_owned());
    for (id, title) in [
        ("overview", "概览"),
        ("bindings", "项目文件连接映射"),
        ("queries", "查询"),
        ("transfers", "迁移配置"),
        ("save", "保存差异"),
        ("recovery", "恢复"),
    ] {
        tabs = tabs.tab(id, crate::i18n::tr(title).into_owned());
    }
    root = root.child(
        html::div()
            .class("project-tabs")
            .child(tabs.on_select(|value| Msg::Action(format!("project-ui-section:{value}")))),
    );
    let mut body = html::div().class("project-body");
    if section == "overview" && app.workbench.project.project.is_none() {
        body = body
            .child(
                html::div()
                    .class("project-file-group")
                    .child(html::strong(crate::i18n::tr("打开项目").into_owned()))
                    .child(input(app, "path", "项目目录"))
                    .child(html::span(crate::i18n::tr("新建项目时，项目目录必须不存在；请选择已有的父目录并输入新的目录名。").into_owned()).class("muted"))
                    .child(input(app, "name", "新项目名称"))
                    .child(
                        html::div()
                            .class("project-local-actions")
                            .child(button("打开项目", "project-ui-open"))
                            .child(button("新建项目", "project-ui-create")),
                    ),
            )
            .child(
                html::div()
                    .class("project-file-group")
                    .child(html::strong(crate::i18n::tr("导入项目归档").into_owned()))
                    .child(input(app, "archive", "项目归档路径"))
                    .child(input(app, "destination", "导入目标目录"))
                    .child(button("导入项目归档", "project-ui-import")),
            );
        if app.workbench.project.restored.is_some() {
            body = body.child(html::span(
                crate::i18n::tr("已恢复本地草稿；请重新打开原项目目录，不会自动连接或执行。")
                    .into_owned(),
            ));
        }
    }
    if section == "recovery" {
        body = body.child(button("查看遗留资源", "project-ui-artifact-list"));
        for (i, artifact) in app.workbench.project.artifacts.iter().enumerate() {
            body = body.child(
                flat_action(
                    "file",
                    &format!(
                        "{} · {} · {}",
                        text(&artifact["kind"]),
                        text(&artifact["identity"]["path"]),
                        crate::advanced::status(artifact["state"].as_str().unwrap_or("unknown"))
                    ),
                    &format!("project-ui-artifact-select:{i}"),
                )
                .class("workbench-entry"),
            );
        }
        if !app.workbench.project.artifact_history.is_empty() {
            body = body.child(button("上一页", "project-ui-artifact-previous"));
        }
        if app.workbench.project.artifact_next.is_some() {
            body = body.child(button("下一页", "project-ui-artifact-next"));
        }
        if let Some(artifact) = &app.workbench.project.artifact {
            body = body
                .child(html::strong(crate::i18n::tr("资源详情").into_owned()))
                .child(line("资源类型", &artifact["kind"]))
                .child(line("资源状态", &artifact["state"]))
                .child(line("路径", &artifact["identity"]["path"]))
                .child(line("大小（字节）", &artifact["identity"]["size"]))
                .child(crate::advanced::diagnostic(
                    "project-artifact-diagnostic",
                    artifact,
                ))
                .child(button("查看资源详情", "project-ui-artifact-read"));
            if !artifact["observed_identity"].is_null() {
                body = body
                    .child(html::strong(crate::i18n::tr("当前未完成文件").into_owned()))
                    .child(line(
                        "当前大小（字节）",
                        &artifact["observed_identity"]["size"],
                    ))
                    .child(line(
                        "当前 SHA-256 摘要",
                        &artifact["observed_identity"]["sha256"],
                    ));
            }
            body = body
                .child(button(
                    if artifact["observed_identity"].is_null() {
                        "领取遗留资源"
                    } else {
                        "领取当前未完成文件"
                    },
                    "project-ui-artifact-claim",
                ))
                .child(input(app, "artifact-path", "遗留资源导出路径"));
            if artifact["kind"] == "project_stage" && artifact["identity"]["kind"] == "save" {
                body = body
                    .child(input(app, "artifact-index", "恢复文件序号（从 1 开始）"))
                    .child(choices(
                        app,
                        &app.project_key("artifact-side"),
                        "导出版本",
                        &[("old", "旧版"), ("new", "新版")],
                    ));
            }
            body = body
                .child(button("导出遗留资源", "project-ui-artifact-export"))
                .child(button("清理遗留资源", "project-ui-artifact-cleanup"));
            if app.workbench.project.cleanup_confirm {
                body = body.child(
                    html::div()
                        .class("project-file-group")
                        .child(html::strong(text(&artifact["identity"]["path"])))
                        .child(html::span(
                            crate::i18n::tr(
                                "请先导出所需恢复副本并完成手工合并；清理后这些副本不可恢复。",
                            )
                            .into_owned(),
                        ))
                        .child(
                            html::div()
                                .class("project-local-actions")
                                .child(
                                    ui::button(crate::i18n::tr("确认清理恢复副本").into_owned())
                                        .variant(ui::ButtonVariant::Danger)
                                        .on_click(Msg::Action(
                                            "project-ui-artifact-cleanup-confirm".into(),
                                        )),
                                )
                                .child(button("取消", "project-ui-artifact-cleanup-cancel")),
                        ),
                );
            }
            if artifact["kind"] == "project_stage" {
                body = body.child(button("查看项目恢复冲突", "project-ui-recovery-read"));
            }
        }
        if let Some(recovery) = &app.workbench.project.recovery {
            body = body
                .child(html::strong(crate::i18n::tr("项目恢复").into_owned()))
                .child(line("项目目录", &recovery["root"]))
                .child(line("资源状态", &recovery["state"]));
            if !recovery["observed_directory_identity"].is_null() {
                body = body.child(crate::advanced::diagnostic(
                    "project-recovery-identity",
                    &recovery["observed_directory_identity"],
                ));
            }
            if !recovery["journal_sha256"].is_null() {
                body = body.child(line("日志 SHA-256", &recovery["journal_sha256"]));
            }
            if recovery["state"] == "preparing"
                && let Some(files) = recovery["files"].as_array()
            {
                for file in files {
                    body = body.child(html::span(format!("目录项: {}", text(file))));
                }
            }
            body = body
                .child(changes(&recovery["files"], true))
                .child(html::span(
                    crate::i18n::tr("外部第三种内容只能导出或人工合并后重新保存。").into_owned(),
                ));
            if recovery["state"] != "preparing" {
                body = body
                    .child(button("预览继续恢复", "project-ui-recovery-continue"))
                    .child(button("预览回退恢复", "project-ui-recovery-rollback"));
            }
        }
        if let Some(preview) = &app.workbench.project.recovery_preview {
            body = body.child(html::strong(crate::i18n::tr("恢复步骤预览").into_owned()));
            body = body.child(html::span(
                crate::i18n::tr(if app.workbench.project.recovery_action == "continue" {
                    "继续发布"
                } else {
                    "回退发布"
                })
                .into_owned(),
            ));
            body = body
                .child(changes(&preview["changes"], false))
                .child(button("确认恢复操作", "project-ui-recovery-commit"));
        }
    }
    if let Some(project) = &app.workbench.project.project {
        if section == "overview" {
            body = body.child(
                html::div()
                    .class("project-file-group")
                    .child(html::strong(crate::i18n::tr("导出项目归档").into_owned()))
                    .child(input(app, "archive", "项目归档路径"))
                    .child(button("导出项目归档", "project-ui-export")),
            );
            body = body
                .child(button("查看说明文件", "project-local-documents-git"))
                .child(document_choices(app));
        }
        if section == "overview" {
            body = body.child(button("应用项目布局", "project-layout-apply"));
        }
        if section == "save" {
            body = body.child(button("查看外部项目版本", "project-ui-external-read"));
            if let Some(external) = &app.workbench.project.external {
                body = body
                    .child(html::strong(crate::i18n::tr("外部项目清单").into_owned()))
                    .child(crate::advanced::diagnostic(
                        "project-external-manifest",
                        &external["manifest"],
                    ))
                    .child(input(app, "external-path", "外部文件项目内路径"))
                    .child(button("查看外部文件", "project-ui-external-file"))
                    .child(html::pre(app.workbench.project.external_text.clone()).class("preview"));
                if let Some(baseline) = &app.workbench.project.baseline_manifest {
                    for (section, index, name) in crate::project_workbench::external_changes(
                        baseline,
                        external,
                        &project["digests"],
                    ) {
                        let decided = app
                            .workbench
                            .project
                            .change_decisions
                            .contains(&format!("{section}:{index}"));
                        let kind = match section.as_str() {
                            "connections" => "连接",
                            "queries" => "查询",
                            "transfers" => "迁移配置",
                            _ => "项目",
                        };
                        let mut row =
                            html::div()
                                .class("project-file-group")
                                .child(html::span(format!(
                                    "{} · {} · {} · {}{}",
                                    if crate::i18n::resolved() == crate::i18n::Language::ZhCn {
                                        "外部新增或修改"
                                    } else {
                                        "Externally added or changed"
                                    },
                                    crate::i18n::tr(kind),
                                    name,
                                    external["manifest"][section.as_str()][index]["path"]
                                        .as_str()
                                        .unwrap_or("—"),
                                    if decided { " ✓" } else { "" }
                                )));
                        if !decided {
                            row = row.child(
                                html::div()
                                    .class("project-local-actions")
                                    .child(button(
                                        "合并外部条目和正文",
                                        &format!(
                                            "project-ui-change-decision:{section}:{index}:merge"
                                        ),
                                    ))
                                    .child(button(
                                        "保留本地条目和正文",
                                        &format!(
                                            "project-ui-change-decision:{section}:{index}:keep"
                                        ),
                                    )),
                            );
                        }
                        body = body.child(row);
                    }
                }
                if app.workbench.project.external_next.is_some() {
                    body = body.child(button("继续读取外部查询", "project-ui-external-next"));
                }
                if let Some(baseline) = &app.workbench.project.baseline_manifest {
                    for (section, index, name) in crate::project_workbench::external_deletions(
                        baseline,
                        external,
                        &project["digests"],
                    ) {
                        let decided = app
                            .workbench
                            .project
                            .deletion_decisions
                            .contains(&format!("{section}:{index}"));
                        let kind = match section.as_str() {
                            "connections" => "连接",
                            "queries" => "查询",
                            "transfers" => "迁移配置",
                            _ => "项目",
                        };
                        let mut row =
                            html::div()
                                .class("project-file-group")
                                .child(html::span(format!(
                                    "{} · {} · {} · {}{}",
                                    if crate::i18n::resolved() == crate::i18n::Language::ZhCn {
                                        "外部已删除"
                                    } else {
                                        "Externally deleted"
                                    },
                                    crate::i18n::tr(kind),
                                    name,
                                    baseline[section.as_str()][index]["path"]
                                        .as_str()
                                        .unwrap_or("—"),
                                    if decided { " ✓" } else { "" }
                                )));
                        if !decided {
                            row = row.child(
                                html::div()
                                    .class("project-local-actions")
                                    .child(button(
                                        "合并删除",
                                        &format!(
                                            "project-ui-delete-decision:{section}:{index}:delete"
                                        ),
                                    ))
                                    .child(button(
                                        "保留本地条目",
                                        &format!(
                                            "project-ui-delete-decision:{section}:{index}:keep"
                                        ),
                                    )),
                            );
                        }
                        body = body.child(row);
                    }
                }
                body = body
                    .child(html::span(
                        crate::i18n::tr("逐项决定新增、修改和删除；合并正文前须完整读取对应外部文件。接受新基线后仍需显式保存当前草稿。")
                            .into_owned(),
                    ))
                    .child(button(
                        "确认接受外部版本为新基线",
                        "project-ui-external-accept",
                    ));
            }
        }
        if app.workbench.project.close_confirm {
            body = body
                .child(html::span(
                    crate::i18n::tr("关闭将放弃未保存的项目编辑和保存预览。").into_owned(),
                ))
                .child(button("确认关闭项目", "project-ui-close-confirm"))
                .child(button("取消", "project-ui-close-cancel"));
        }
        if section == "bindings" {
            body = body
                .child(input(app, "group", "本机绑定组"))
                .child(button("读取绑定", "project-ui-bindings"));
            if let Some(bindings) = &app.workbench.project.bindings {
                body = body.child(html::span(if bindings["confirmed"] == true {
                    "本机绑定已确认"
                } else {
                    "本机绑定未确认"
                }));
                if bindings["confirmed"] != true {
                    body = body.child(button("确认当前本机绑定", "project-ui-bindings-confirm"));
                }
            }
            body = body.child(html::strong(
                crate::i18n::tr("逻辑连接与本机绑定").into_owned(),
            ));
            body = body.child(local_icon_button(
                app,
                "添加逻辑连接",
                "plus",
                "project-ui-connection-new",
            ));
            if app.workbench.project.git_connection_new {
                body = body.child(
                    html::div()
                        .class("project-file-group")
                        .child(input(app, "logical-alias", "新增逻辑连接别名"))
                        .child(choices(
                            app,
                            &app.project_key("logical-provider"),
                            "数据库类型",
                            &[("mysql", "MySQL"), ("redis", "Redis")],
                        ))
                        .child(choices(
                            app,
                            &app.project_key("logical-source"),
                            "连接来源",
                            &[("direct", "直连"), ("archery", "Archery")],
                        ))
                        .child(button("添加逻辑连接", "project-ui-connection-add")),
                );
            }
            body = body.child(html::span(
                crate::i18n::tr("新增逻辑连接须先保存项目，再保存本机绑定。").into_owned(),
            ));
            if app.workbench.project.bindings.is_some() {
                body = body.child(html::span(
                    crate::i18n::tr("仅显示当前页已读取的绑定").into_owned(),
                ));
            }
            if let Some(connections) = project["manifest"]["connections"].as_array() {
                for (i, connection) in connections.iter().enumerate() {
                    let alias = connection["alias"].as_str().unwrap_or("");
                    let binding = app
                        .workbench
                        .project
                        .bindings
                        .as_ref()
                        .and_then(|v| v["bindings"].as_array())
                        .and_then(|items| {
                            items
                                .iter()
                                .find(|v| v["connection_alias"].as_str() == Some(alias))
                        });
                    let local = binding
                        .and_then(|b| b["profile_id"].as_str())
                        .and_then(|id| app.profiles.iter().find(|p| p.id.to_string() == id))
                        .map(|p| p.name.as_str())
                        .map(str::to_owned)
                        .unwrap_or_else(|| {
                            if binding.is_some() {
                                crate::i18n::tr("连接引用失效").into_owned()
                            } else {
                                "—".to_owned()
                            }
                        });
                    let target = binding
                        .and_then(|b| {
                            b["overrides"]["database"]
                                .as_str()
                                .map(str::to_owned)
                                .or_else(|| {
                                    b["overrides"]["redis_db"]
                                        .as_u64()
                                        .map(|db| format!("DB {db}"))
                                })
                        })
                        .unwrap_or_else(|| "—".to_owned());
                    body = body.child(
                        flat_action(
                            "database",
                            &format!(
                                "{alias} · {} · {local} · {target}",
                                text(&connection["provider"])
                            ),
                            &format!("project-ui-alias:{i}"),
                        )
                        .class("workbench-entry"),
                    );
                }
            }
            if !app.workbench.project.binding_history.is_empty() {
                body = body.child(button("上一页", "project-ui-bindings-previous"));
            }
            if app
                .workbench
                .project
                .bindings
                .as_ref()
                .and_then(|b| b["next"].as_u64())
                .is_some()
            {
                body = body.child(button("继续读取绑定", "project-ui-bindings-next"));
            }
            let ids: Vec<_> = app.profiles.iter().map(|p| p.id.to_string()).collect();
            let mut options = vec![("", "请选择连接")];
            for (profile, id) in app.profiles.iter().zip(&ids) {
                options.push((id.as_str(), profile.name.as_str()));
            }
            if app.workbench.project.git_binding_edit {
                body = body.child(
                    html::div()
                        .class("project-file-group")
                        .child(input(app, "alias", "连接别名"))
                        .child(choices(
                            app,
                            &app.project_key("profile"),
                            "本机连接",
                            &options,
                        ))
                        .child(
                            if app
                                .profiles
                                .iter()
                                .find(|p| {
                                    p.id.to_string() == app.field(&app.project_key("profile"))
                                })
                                .is_some_and(|p| {
                                    p.provider == datapier_core::model::ProviderKind::Redis
                                })
                            {
                                input(app, "redis-db", "本机 Redis DB")
                            } else {
                                input(app, "database", "本机数据库")
                            },
                        )
                        .child(input(app, "path-parameter", "路径参数名"))
                        .child(input(app, "local-path", "本机路径覆盖")),
                );
            }
        }
        if section == "queries" {
            body = body.child(
                html::div()
                    .class("project-local-heading")
                    .child(html::strong(crate::i18n::tr("项目查询").into_owned()))
                    .child(local_icon_button(
                        app,
                        "添加或更新查询",
                        "plus",
                        "project-ui-query-new",
                    )),
            );
            if let Some(queries) = project["manifest"]["queries"].as_array() {
                for (i, query) in queries.iter().enumerate() {
                    let mut row = html::div().class("project-local-row").child(
                        flat_action(
                            "file",
                            &format!("{} · {}", text(&query["name"]), text(&query["path"])),
                            &format!("project-ui-query:{i}"),
                        )
                        .class("workbench-entry"),
                    );
                    if let Some(path) = query["path"].as_str() {
                        row = row.child(local_icon_button(
                            app,
                            "预览文件",
                            "file",
                            &format!("project-local-glimpse:{path}"),
                        ));
                    }
                    body = body.child(row);
                }
            }
        }
        if section == "transfers" {
            body = body.child(
                html::div()
                    .class("project-local-heading")
                    .child(html::strong(crate::i18n::tr("项目迁移预设").into_owned()))
                    .child(local_icon_button(
                        app,
                        "新增共享迁移配置",
                        "plus",
                        "project-ui-transfer-new",
                    )),
            );
            if app.workbench.project.file_next.is_some() {
                body = body
                    .child(html::span(
                        crate::i18n::tr("正在分块读取项目文件。").into_owned(),
                    ))
                    .child(button("继续读取项目文件", "project-ui-file-next"));
            }
            if let Some(transfers) = project["manifest"]["transfers"].as_array() {
                for (i, transfer) in transfers.iter().enumerate() {
                    body = body.child(
                        flat_action(
                            "repeat",
                            &format!("{} · {}", text(&transfer["name"]), text(&transfer["path"])),
                            &format!("project-ui-transfer:{i}"),
                        )
                        .class("workbench-entry"),
                    );
                }
            }
        }
        if section == "queries" && app.workbench.project.git_query_edit {
            if app.workbench.project.file_next.is_some() {
                body = body.child(button("继续读取查询", "project-ui-file-next"));
            }
            body = body.child(button("重新读取查询", "project-ui-file-reload"));
            if app.workbench.project.reload_confirm {
                body = body
                    .child(html::span(
                        crate::i18n::tr("重新读取将替换当前查询正文；读取失败时保留正文。")
                            .into_owned(),
                    ))
                    .child(button("确认重新读取", "project-ui-file-reload-confirm"))
                    .child(button("取消", "project-ui-file-reload-cancel"));
            }
            body = body
                .child(html::strong(crate::i18n::tr("添加或更新查询").into_owned()))
                .child(input(app, "query-name", "查询名称"))
                .child(input(app, "query-path", "项目内查询路径"))
                .child(input(app, "query-alias", "查询连接别名"))
                .child(input(app, "query-text", "查询正文"));
            body = body.child(
                html::span(
                    crate::i18n::tr("查询修改保留在本机草稿，预览并确认保存后写入项目文件。")
                        .into_owned(),
                )
                .class("muted"),
            );
        }
        if section == "save"
            && let Some(preview) = &app.workbench.project.preview
        {
            body = body.child(html::strong(crate::i18n::tr("保存差异").into_owned()));
            body = body
                .child(changes(&preview["changes"], false))
                .child(html::span(
                    crate::i18n::tr("查询正文可能包含敏感内容，请在保存前核对。").into_owned(),
                ));
        }
    }
    if let Some(result) = &app.workbench.project.result {
        body = body
            .child(html::strong(crate::i18n::tr("操作结果").into_owned()))
            .child(line("状态", &result["state"]));
        if !result["path"].is_null() {
            body = body.child(line("路径", &result["path"]));
        }
        if !result["destination"].is_null() {
            body = body.child(line("导入目标目录", &result["destination"]));
        }
    }
    root = root.child(body);
    let mut actions = html::div().class("project-actions");
    if app.workbench.project.project.is_some() {
        actions = actions.child(button("关闭项目", "project-ui-close"));
        if app.workbench.project.preview.is_some()
            || app.workbench.project.obsolete_preview.is_some()
        {
            actions = actions.child(button("丢弃保存预览", "project-ui-save-discard"));
        }
        actions = actions.child(html::span("").class("spacer"));
        let action = if section == "bindings" {
            Some(("保存绑定", "project-ui-bindings-save"))
        } else if section == "save" && app.workbench.project.preview.is_some() {
            Some(("确认保存项目", "project-ui-save-commit"))
        } else if matches!(section, "save" | "queries") || app.workbench.project.unsaved {
            Some(("预览项目保存差异", "project-ui-save-prepare"))
        } else {
            None
        };
        if let Some((label, action)) = action {
            actions = actions.child(
                ui::button(crate::i18n::tr(label).into_owned())
                    .key(action)
                    .variant(ui::ButtonVariant::Primary)
                    .disabled(
                        app.workbench.project.is_pending()
                            || app.saving
                            || app.workbench.project.close_confirm
                            || (section == "bindings"
                                && (app.workbench.project.bindings.is_none()
                                    || !app.workbench.project.git_binding_edit))
                            || (section != "bindings"
                                && (app.workbench.project.needs_baseline_check
                                    || app.workbench.project.external.is_some())),
                    )
                    .on_click(Msg::Action(action.into())),
            );
        }
    }
    root.child(actions).into()
}
