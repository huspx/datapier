use super::*;

fn waiting_message(state: &crate::connections::archery::Wizard) -> Option<&'static str> {
    match state.pending.as_ref()?.1.as_str() {
        "archery.auth.begin" | "archery.auth.verify" => Some("正在登录或验证…"),
        "archery.instances" => Some("正在加载实例…"),
        "archery.databases" => Some("正在加载数据库…"),
        _ => None,
    }
}

pub(super) fn auth_dialog(app: &App) -> (html::El<Msg>, html::El<Msg>) {
    let state = &app.archery;
    if state.discard_action.is_some() {
        return wizard(app);
    }
    let busy = state.pending.is_some();
    let observing = state.auth_id.is_some()
        && state.controller_token.is_none()
        && matches!(
            state.state.as_str(),
            "authenticating" | "challenge_required"
        );
    let mut body = html::div().class("modal-body");
    if !app.field("archery_name").is_empty() || !app.field("archery_user").is_empty() {
        body = body.child(
            html::span(format!(
                "{} · {}",
                app.field("archery_name"),
                app.field("archery_user")
            ))
            .class("muted"),
        );
    }
    if observing {
        body = body.child(html::span(
            crate::i18n::tr("正在等待其他窗口完成认证").into_owned(),
        ));
    } else if let Some(message) = waiting_message(state) {
        body = body.child(html::span(crate::i18n::tr(message).into_owned()));
    } else if state.state == "challenge_required" {
        body = body.child(field(app, "archery_otp", "验证码"));
    } else {
        body = body.child(field(app, "password", "密码"));
    }
    if let Some(error) = &state.error {
        body = body.child(html::span(error.clone()).class("connection-error"));
    }
    let mut footer = html::div()
        .class("modal-footer")
        .child(html::span("").class("spacer"))
        .child(button("返回", "archery-auth-back"));
    if waiting_message(state).is_some() {
        footer = footer.child(button("停止等待", "archery-cancel-auth"));
    }
    if observing {
        return (body, footer);
    }
    if state.state == "challenge_required" {
        if state.controller_token.is_some() {
            footer = footer.child(
                ui::button(crate::i18n::tr("验证").into_owned())
                    .variant(ui::ButtonVariant::Primary)
                    .disabled(busy)
                    .on_click(Msg::Action("archery-verify".into())),
            );
        }
    } else {
        footer = footer.child(
            ui::button(crate::i18n::tr("登录").into_owned())
                .variant(ui::ButtonVariant::Primary)
                .disabled(busy)
                .on_click(Msg::Action("archery-login".into())),
        );
    }
    (body, footer)
}

pub(super) fn wizard(app: &App) -> (html::El<Msg>, html::El<Msg>) {
    let state = &app.archery;
    if state.discard_action.is_some() {
        let body = html::div().class("modal-body").child(html::span(
            crate::i18n::tr("未保存的更改将被丢弃，是否继续？").into_owned(),
        ));
        let footer = html::div()
            .class("modal-footer")
            .child(html::span("").class("spacer"))
            .child(button("继续编辑", "archery-discard-cancel"))
            .child(
                ui::button(crate::i18n::tr("丢弃更改").into_owned())
                    .variant(ui::ButtonVariant::Danger)
                    .on_click(Msg::Action("archery-discard-confirm".into())),
            );
        return (body, footer);
    }
    let busy = state.pending.is_some();
    let locked = state.pending.as_ref().is_some_and(|(_, method)| {
        !matches!(
            method.as_str(),
            "archery.auth.begin"
                | "archery.auth.verify"
                | "archery.instances"
                | "archery.databases"
        )
    });
    let mut panel = html::div().class("modal-body");
    let mut form = html::div().class("profile-form");
    if !state.server_only && !app.connection_form_is_editing() {
        form = form.child(super::profile_form::connection_type_cards(app, true));
    }
    if !state.server_only {
        form = form.child(
            html::div()
                .class("archery-steps")
                .child(
                    html::span(format!("1. {}", crate::i18n::tr("连接设置"))).class(
                        if state.preview {
                            "muted"
                        } else {
                            "archery-step-current"
                        },
                    ),
                )
                .child(html::span(format!("2. {}", crate::i18n::tr("预览"))).class(
                    if state.preview {
                        "archery-step-current"
                    } else {
                        "muted"
                    },
                )),
        );
    }
    if !state.server_only && !state.preview {
        let selected = state
            .server
            .as_ref()
            .and_then(|current| {
                state
                    .servers
                    .iter()
                    .position(|server| server["id"] == current["id"])
            })
            .map(|index| index.to_string())
            .unwrap_or_default();
        let mut select = ui::select()
            .aria(crate::i18n::tr("服务器").into_owned())
            .key("input-archery-server")
            .value(selected)
            .disabled(locked || app.connection_form_is_editing())
            .open(app.workbench.generic_select.as_deref() == Some("archery-server"))
            .on_toggle(|collapsed| {
                Msg::Action(format!("select-open:archery-server:{}", !collapsed))
            })
            .option(ui::SelectOption::new(
                "",
                crate::i18n::tr("请选择服务器").into_owned(),
            ));
        for (index, server) in state.servers.iter().enumerate() {
            select = select.option(ui::SelectOption::new(
                index.to_string(),
                format!(
                    "{} · {}",
                    server["name"].as_str().unwrap_or(""),
                    server["username"].as_str().unwrap_or("")
                ),
            ));
        }
        form =
            form.child(
                html::div()
                    .class("archery-server-select")
                    .child(ui::field(crate::i18n::tr("服务器").into_owned()).child(
                        select.on_change(|change| {
                            Msg::Action(format!(
                                "archery-server:{}",
                                change.value.as_str().unwrap_or_default()
                            ))
                        }),
                    ))
                    .child(icon_button("plus", "新建服务", "archery-new"))
                    .child(page_button(
                        "edit",
                        "编辑服务",
                        "archery-manage",
                        !locked && state.saved,
                    )),
            );
    }
    if !state.server_only
        && state.preview
        && let Some(server) = &state.server
    {
        form = form.child(
            ui::field(crate::i18n::tr("服务器").into_owned()).child(
                html::span(format!(
                    "{} · {}",
                    server["name"].as_str().unwrap_or(""),
                    server["username"].as_str().unwrap_or("")
                ))
                .class("archery-server-summary"),
            ),
        );
    }
    if !state.server_only
        && state.auth_id.is_some()
        && state.controller_token.is_none()
        && matches!(
            state.state.as_str(),
            "authenticating" | "challenge_required"
        )
    {
        form = form.child(
            html::span(crate::i18n::tr("正在等待其他窗口完成认证").into_owned()).class("muted"),
        );
    }
    if let Some(message) = waiting_message(state) {
        form = form.child(html::span(crate::i18n::tr(message).into_owned()).class("muted"));
        form = form.child(button("停止等待", "archery-cancel-auth"));
    }
    if state.server_only {
        form = form.child(field(app, "archery_name", "服务名称"));
        if state.saved {
            for (label, name) in [("服务地址", "archery_url"), ("用户名", "archery_user")] {
                form = form.child(
                    ui::field(crate::i18n::tr(label).into_owned()).child(
                        ui::input()
                            .variant(ui::InputVariant::Field)
                            .readonly(true)
                            .key(format!("readonly-{name}"))
                            .text(app.field(name)),
                    ),
                );
            }
            if let Some(server) = &state.server {
                form = form.child(
                    html::span(crate::i18n::format_message(
                        "关联 {count} 个连接",
                        &[(
                            "count",
                            &server["connection_count"]
                                .as_u64()
                                .unwrap_or(state.baseline.len() as u64)
                                .to_string(),
                        )],
                    ))
                    .class("muted"),
                );
            }
            form = form.child(button("重新登录或修改密码", "archery-relogin"));
        } else {
            form = form
                .child(field(app, "archery_url", "服务地址"))
                .child(field(app, "archery_user", "用户名"));
        }
        form = form.child(
            ui::switch(crate::i18n::tr("保存密码").into_owned())
                .on(state.save_secret)
                .disabled(busy)
                .on_change(|_| Msg::Action("archery-toggle-secret".into())),
        );
        let ca = std::path::Path::new(app.field("archery_ca"))
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(&crate::i18n::tr("默认信任链"))
            .to_owned();
        let https = app
            .field("archery_url")
            .trim()
            .get(..8)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"));
        form = form
            .child(html::span(format!("CA: {ca}")).class("muted"))
            .child(
                ui::button(crate::i18n::tr("CA 设置").into_owned())
                    .disabled(!https || busy)
                    .on_click(Msg::Action("archery-server-details".into())),
            );
        if !https {
            form = form
                .child(html::span(crate::i18n::tr("CA 仅用于 HTTPS").into_owned()).class("muted"));
        }
        if state.server_details && https {
            let editor = &app.fields["archery_ca"];
            form = form.child(
                ui::field(crate::i18n::tr("CA 证书路径").into_owned()).child(
                    ui::input()
                        .variant(ui::InputVariant::Field)
                        .aria(crate::i18n::tr("CA 证书路径").into_owned())
                        .readonly(!https || busy)
                        .key("input-archery_ca")
                        .text(editor.text.clone())
                        .cursor(editor.cursor)
                        .anchor(editor.anchor)
                        .on_focus(Msg::Focus("archery_ca".into()))
                        .on_undo(Msg::Undo("archery_ca".into()))
                        .on_edit(|e| {
                            Msg::Edit(
                                "archery_ca".into(),
                                e.from,
                                e.to,
                                e.text.clone(),
                                e.cursor,
                                e.len,
                            )
                        }),
                ),
            );
            form = form.child(
                ui::button(crate::i18n::tr("清除 CA").into_owned())
                    .disabled(!https || busy || app.field("archery_ca").is_empty())
                    .on_click(Msg::Action("archery-clear-ca".into())),
            );
        }
        form = form
            .child(
                html::span(format!(
                    "TOTP: {} · {}",
                    crate::i18n::tr(if state.totp_automatic {
                        "自动生成验证码"
                    } else {
                        "手动输入验证码"
                    }),
                    crate::i18n::tr(
                        if state
                            .server
                            .as_ref()
                            .is_some_and(|server| server["totp_configured"] == true)
                        {
                            "已配置"
                        } else {
                            "未配置"
                        }
                    )
                ))
                .class("muted"),
            )
            .child(button("TOTP 设置", "archery-totp-details"));
        if state.totp_details {
            form = form
                .child(field(app, "archery_totp_uri", "TOTP 链接"))
                .child(
                    ui::switch(crate::i18n::tr("自动生成验证码").into_owned())
                        .on(state.totp_automatic)
                        .disabled(busy)
                        .on_change(|_| Msg::Action("archery-toggle-totp-mode".into())),
                )
                .child(
                    ui::switch(crate::i18n::tr("保存 TOTP 链接").into_owned())
                        .on(state.save_totp)
                        .disabled(busy)
                        .on_change(|_| Msg::Action("archery-toggle-totp".into())),
                );
            if state.saved {
                form = form.child(
                    ui::switch(crate::i18n::tr("清除 TOTP 链接").into_owned())
                        .on(state.clear_totp)
                        .disabled(busy)
                        .on_change(|_| Msg::Action("archery-clear-totp".into())),
                );
            }
        }
        if state.saved {
            form = form.child(
                ui::switch(crate::i18n::tr("清除密码").into_owned())
                    .on(state.clear_password)
                    .disabled(busy)
                    .on_change(|_| Msg::Action("archery-clear-password".into())),
            );
        }
        if state.saved && state.manage {
            if state.delete_confirm {
                form = form.child(html::span(crate::i18n::format_message(
                    "删除服务器及 {count} 个连接？",
                    &[("count", &state.baseline.len().to_string())],
                )));
                form = form.child(
                    html::span(
                        crate::i18n::tr("仅删除本地配置，不删除远端实例、数据库或数据")
                            .into_owned(),
                    )
                    .class("muted"),
                );
                for profile in &state.baseline {
                    form = form.child(html::span(format!(
                        "{} · {}",
                        profile.name,
                        profile.archery().unwrap().instance_name
                    )));
                }
            } else {
                form = form.child(button("删除服务", "archery-confirm-delete"));
            }
        }
    }
    {
        if !state.server_only && state.preview {
            if state.saved {
                form = form.child(
                    html::span(crate::i18n::tr("服务已保存；连接尚未保存").into_owned())
                        .class("muted"),
                );
            }
            if let Some(server) = &state.server {
                let desired = app.archery_desired_profiles(server);
                for (label, rows) in [
                    (
                        "新增",
                        desired
                            .iter()
                            .filter(|p| !state.baseline.iter().any(|old| p["id"] == json!(old.id)))
                            .cloned()
                            .collect::<Vec<_>>(),
                    ),
                    (
                        "修改",
                        desired
                            .iter()
                            .filter(|p| {
                                state
                                    .baseline
                                    .iter()
                                    .any(|old| p["id"] == json!(old.id) && **p != json!(old))
                            })
                            .cloned()
                            .collect::<Vec<_>>(),
                    ),
                    (
                        "删除",
                        state
                            .baseline
                            .iter()
                            .filter(|old| !desired.iter().any(|p| p["id"] == json!(old.id)))
                            .map(|p| json!(p))
                            .collect::<Vec<_>>(),
                    ),
                ] {
                    if !rows.is_empty() {
                        let mut group =
                            html::div()
                                .class("archery-group")
                                .child(html::strong(format!(
                                    "{} ({})",
                                    crate::i18n::tr(label),
                                    rows.len()
                                )));
                        for row in rows {
                            let mut entry = html::div().class(if label == "删除" {
                                "archery-preview-delete"
                            } else {
                                "archery-preview-row"
                            });
                            if label == "修改"
                                && let Some(old) =
                                    state.baseline.iter().find(|old| row["id"] == json!(old.id))
                            {
                                let connection = old.archery().unwrap();
                                entry = entry.child(html::span(format!(
                                    "{}: {} · {} · {}: {}",
                                    crate::i18n::tr("原值"),
                                    old.name,
                                    connection.databases.join(", "),
                                    crate::i18n::tr("默认目标"),
                                    connection.default_database
                                )));
                            }
                            let prefix = if label == "修改" {
                                format!("{}: ", crate::i18n::tr("新值"))
                            } else {
                                String::new()
                            };
                            entry = entry.child(html::span(format!(
                                "{}{} · {} · {} · {}: {}",
                                prefix,
                                row["name"].as_str().unwrap_or(""),
                                row["connection"]["instance_name"].as_str().unwrap_or(""),
                                row["connection"]["databases"]
                                    .as_array()
                                    .map(|dbs| dbs
                                        .iter()
                                        .filter_map(Value::as_str)
                                        .collect::<Vec<_>>()
                                        .join(", "))
                                    .unwrap_or_default(),
                                crate::i18n::tr("默认目标"),
                                row["connection"]["default_database"].as_str().unwrap_or("")
                            )));
                            if label == "删除" {
                                entry = entry.child(html::span(
                                    crate::i18n::tr("仅删除本地配置，不删除远端实例、数据库或数据")
                                        .into_owned(),
                                ));
                            }
                            group = group.child(entry);
                        }
                        form = form.child(group);
                    }
                }
            }
        } else if !state.server_only {
            form = form.child(
                html::div()
                    .class("archery-section-heading")
                    .child(html::strong(
                        crate::i18n::tr("显示在资源树中的实例与数据库").into_owned(),
                    ))
                    .child(page_button(
                        "repeat",
                        "刷新实例",
                        "archery-instances",
                        !busy,
                    )),
            );
            let instances = state
                .instances
                .iter()
                .enumerate()
                .map(|(index, instance)| (Some(index), instance))
                .chain(state.selected.iter().filter_map(|(id, selection)| {
                    (!state
                        .instances
                        .iter()
                        .any(|instance| instance["id"].as_i64() == Some(*id)))
                    .then_some((None, &selection.instance))
                }));
            for (index, instance) in instances {
                let name = instance["name"].as_str().unwrap_or("");
                let id = instance["id"].as_i64().unwrap_or_default();
                let selection = state.selected.get(&id);
                let added = state
                    .baseline
                    .iter()
                    .any(|p| p.archery().is_some_and(|a| a.instance_id == id));
                let missing = index.is_none() || instance["missing"].as_bool() == Some(true);
                let mut heading = html::div().class("archery-instance-heading").child(
                    html::label()
                        .class("database-choice")
                        .child(
                            html::checkbox("instance", id.to_string())
                                .checked(selection.is_some())
                                .disabled(busy)
                                .on_change(move |_| {
                                    Msg::Action(match index {
                                        Some(index) => format!("archery-instance:{index}"),
                                        None => format!("archery-remove:{id}"),
                                    })
                                }),
                        )
                        .child(html::span(format!(
                            "{} · {}{}",
                            instance["provider"].as_str().unwrap_or(""),
                            name,
                            if added && selection.is_none() {
                                format!(" · {}", crate::i18n::tr("待删除本地连接；勾回可恢复原值"))
                            } else if missing {
                                format!(" · {}", crate::i18n::tr("平台未发现"))
                            } else if added {
                                format!(" · {}", crate::i18n::tr("已添加"))
                            } else {
                                String::new()
                            }
                        ))),
                );
                if selection.is_some() {
                    heading = heading.child(
                        ui::button(
                            crate::i18n::tr(if state.expanded.contains(&id) {
                                "收起"
                            } else {
                                "展开"
                            })
                            .into_owned(),
                        )
                        .disabled(busy)
                        .on_click(Msg::Action(format!("archery-expand:{id}"))),
                    );
                }
                let mut group = html::div()
                    .key(format!("archery-instance-{id}"))
                    .class("archery-group")
                    .child(heading);
                if let Some(selection) = selection.filter(|_| state.expanded.contains(&id)) {
                    let mut details = html::div()
                        .class("archery-instance-details")
                        .key(format!("archery-database-group-{id}"))
                        .attr("tabindex", 0)
                        .attr("role", "group")
                        .attr("aria-label", crate::i18n::tr("数据库选择").into_owned())
                        .child(field(
                            app,
                            &format!("archery_profile_name_{}", selection.profile_id),
                            "连接名称",
                        ))
                        .child(
                            ui::button(crate::i18n::tr("移除实例").into_owned())
                                .disabled(busy)
                                .on_click(Msg::Action(format!("archery-remove:{id}"))),
                        );
                    if let Some(error) = &selection.error {
                        details =
                            details.child(html::span(error.clone()).class("connection-error"));
                    }
                    if let Some(databases) = &selection.databases {
                        details = details.child(page_button(
                            "repeat",
                            "刷新数据库",
                            &format!("archery-load:{id}"),
                            !busy && !missing,
                        ));
                        details = details.child(
                            html::span(
                                crate::i18n::tr(if selection.available_databases.is_some() {
                                    "数据库列表由平台报告"
                                } else {
                                    "显示已保存的数据库；刷新可核对平台可见性"
                                })
                                .into_owned(),
                            )
                            .class("muted"),
                        );
                        let mut choices = html::div()
                            .attr("role", "radiogroup")
                            .attr("aria-label", crate::i18n::tr("默认目标").into_owned());
                        let entry = databases
                            .iter()
                            .position(|db| {
                                selection.selected.contains(db)
                                    && selection
                                        .available_databases
                                        .as_ref()
                                        .is_none_or(|available| available.contains(db))
                                    && selection.default.as_ref() == Some(db)
                            })
                            .or_else(|| {
                                databases.iter().position(|db| {
                                    selection.selected.contains(db)
                                        && selection
                                            .available_databases
                                            .as_ref()
                                            .is_none_or(|available| available.contains(db))
                                })
                            });
                        for (index, db) in databases.iter().enumerate() {
                            let unavailable = missing
                                || selection
                                    .available_databases
                                    .as_ref()
                                    .is_some_and(|available| !available.contains(db));
                            let selected = selection.selected.contains(db);
                            let mut row = html::div()
                                .class("archery-database-row")
                                .child(
                                    html::label()
                                        .class("database-choice")
                                        .child(
                                            html::checkbox("database", db.clone())
                                                .checked(selected)
                                                .disabled(busy || unavailable)
                                                .on_change(move |_| {
                                                    Msg::Action(format!("archery-db:{id}:{index}"))
                                                }),
                                        )
                                        .child(html::span(db.clone())),
                                )
                                .child(
                                    html::button(crate::i18n::tr("默认目标").into_owned())
                                        .key(format!("archery-default-{id}-{index}"))
                                        .attr(
                                            "aria-label",
                                            format!("{} · {}", crate::i18n::tr("默认目标"), db),
                                        )
                                        .attr("role", "radio")
                                        .attr(
                                            "aria-checked",
                                            selection.default.as_ref() == Some(db),
                                        )
                                        .attr("tabindex", if entry == Some(index) { 0 } else { -1 })
                                        .disabled(busy || unavailable || !selected)
                                        .on_click(Msg::Action(format!(
                                            "archery-default:{id}:{index}"
                                        ))),
                                );
                            if unavailable {
                                row = row.child(
                                    html::span(
                                        crate::i18n::tr("平台未发现，已保留原选择").into_owned(),
                                    )
                                    .class("muted"),
                                );
                            } else if !selected {
                                row = row.child(
                                    html::span(crate::i18n::tr("请先选择此数据库").into_owned())
                                        .class("muted"),
                                );
                            } else if selection.default.as_ref() == Some(db) {
                                row = row.child(
                                    html::span(crate::i18n::tr("当前默认目标").into_owned())
                                        .class("muted"),
                                );
                            }
                            choices = choices.child(row);
                        }
                        details = details.child(choices);
                    } else {
                        details = details.child(page_button(
                            "repeat",
                            "加载数据库",
                            &format!("archery-load:{id}"),
                            !busy && !missing,
                        ));
                    }
                    group = group.child(details);
                }
                form = form.child(group);
            }
        }
    }
    if state.recovery.is_some() {
        form = form.child(button("核对保存结果", "archery-reconcile"));
    }
    if let Some(error) = &state.error {
        form = form.child(html::span(error.clone()).class("connection-error"));
    }
    panel = panel.child(form);
    let mut footer = html::div()
        .class("modal-footer")
        .child(html::span("").class("spacer"));
    if state.preview && !state.server_only {
        footer = footer.child(button("返回修改", "archery-back")).child(
            ui::button(crate::i18n::tr("保存全部连接").into_owned())
                .variant(ui::ButtonVariant::Primary)
                .disabled(busy)
                .on_click(Msg::Action("archery-save".into())),
        );
    } else {
        footer = footer.child(
            ui::button(crate::i18n::tr("关闭").into_owned())
                .on_click(Msg::Action("overlay-close".into())),
        );
        if state.delete_confirm {
            footer = footer.child(
                ui::button(crate::i18n::format_message(
                    "删除服务器及 {count} 个连接",
                    &[("count", &state.baseline.len().to_string())],
                ))
                .variant(ui::ButtonVariant::Danger)
                .role("archery-server-delete-confirm")
                .disabled(busy)
                .on_click(Msg::Action("archery-delete-server".into())),
            );
        } else if state.server_only {
            footer = footer.child(
                ui::button(crate::i18n::tr("保存服务").into_owned())
                    .variant(ui::ButtonVariant::Primary)
                    .disabled(busy)
                    .on_click(Msg::Action("archery-save-server".into())),
            );
        } else {
            footer = footer.child(
                ui::button(crate::i18n::tr("预览连接").into_owned())
                    .variant(ui::ButtonVariant::Primary)
                    .disabled(busy)
                    .on_click(Msg::Action("archery-preview".into())),
            );
        }
    }
    (panel, footer)
}
