use super::*;
mod diagnostics;
mod settings;
use diagnostics::{diagnostic_detail, diagnostic_task, diagnostic_text};
fn paste_preview_panel(app: &App) -> Node<Msg> {
    let mut panel = html::div().class("draft-list");
    if let Some(paste) = &app.paste_preview {
        for edit in &paste.edits {
            panel = panel.child(
                html::div()
                    .class("draft-item")
                    .child(html::strong(crate::i18n::format_message(
                        "粘贴 · 第 {row} 行，第 {column} 列",
                        &[
                            ("row", &(edit.cell.row + 1).to_string()),
                            ("column", &(edit.cell.column + 1).to_string()),
                        ],
                    )))
                    .child(html::span(crate::i18n::format_message(
                        "原值：{value}",
                        &[("value", &cell_preview(app, &edit.before))],
                    )))
                    .child(html::span(crate::i18n::format_message(
                        "新值：{value}",
                        &[("value", &cell_preview(app, &edit.after))],
                    ))),
            );
        }
    }
    panel.into()
}

fn preview_role(statement: &Value) -> std::borrow::Cow<'_, str> {
    let role = statement.get("role").and_then(Value::as_str).unwrap_or("");
    crate::i18n::tr(match role {
        "check" => "校验",
        "write" => "写入",
        "ttl" => "过期时间",
        _ => role,
    })
}

fn change_preview_panel(preview: &Value) -> Node<Msg> {
    let provider = preview
        .get("provider")
        .and_then(Value::as_str)
        .unwrap_or("");
    let transactional = preview
        .get("transactional")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut panel = html::div().class("change-preview");
    let boundary = match (provider, transactional) {
        ("redis", true) => "Redis 事务：MULTI / EXEC；单条命令错误不会回滚其他命令。",
        (_, true) => "事务执行：开始事务，执行以下语句，然后提交。",
        _ => "非事务执行。",
    };
    panel = panel.child(
        html::div()
            .class("notice")
            .child(html::span(crate::i18n::tr(boundary).into_owned())),
    );
    panel = panel.child(
        html::p(crate::i18n::tr("预览尚未执行；提交时将重新校验。").into_owned()).class("muted"),
    );
    if let Some(notes) = preview.get("notes").and_then(Value::as_array) {
        for note in notes.iter().filter_map(Value::as_str) {
            panel = panel.child(html::p(crate::i18n::tr(note).into_owned()).class("muted"));
        }
    }
    if let Some(items) = preview.get("items").and_then(Value::as_array) {
        let mut statement_number = 0usize;
        // 完整原值校验可能有很多参数；整页先列实际语句，避免把后续 DML 推出首屏。
        for item in items {
            let operation = item.get("operation").and_then(Value::as_str).unwrap_or("");
            let heading = match item.get("index").and_then(Value::as_u64) {
                Some(index) => format!("#{} · {}", index + 1, operation),
                None => operation.to_owned(),
            };
            let mut entry = html::div().class("draft-item").child(html::strong(heading));
            if let Some(statements) = item.get("statements").and_then(Value::as_array) {
                for statement in statements {
                    statement_number += 1;
                    let role = preview_role(statement);
                    entry = entry.child(html::strong(crate::i18n::format_message(
                        "语句 {number} · {role}",
                        &[
                            ("number", &statement_number.to_string()),
                            ("role", role.as_ref()),
                        ],
                    )));
                    let text = statement.get("text").and_then(Value::as_str).unwrap_or("");
                    if provider == "mysql" {
                        entry = entry.child(ui::code(text.to_owned()).lang("sql"));
                    } else {
                        entry = entry.child(html::pre(text.to_owned()).class("preview"));
                    }
                }
            }
            panel = panel.child(entry);
        }
        statement_number = 0;
        for item in items {
            if let Some(statements) = item.get("statements").and_then(Value::as_array) {
                for statement in statements {
                    statement_number += 1;
                    if let Some(parameters) = statement.get("parameters").and_then(Value::as_array)
                        && !parameters.is_empty()
                    {
                        let mut table = html::div()
                            .class("preview-parameters")
                            .child(html::strong(crate::i18n::format_message(
                                "语句 {number} · {role} · 参数",
                                &[
                                    ("number", &statement_number.to_string()),
                                    ("role", preview_role(statement).as_ref()),
                                ],
                            )))
                            .child(
                                html::div()
                                    .class("preview-parameter-row preview-parameter-header")
                                    .child(html::span("#"))
                                    .child(html::span(crate::i18n::tr("类型").into_owned()))
                                    .child(html::span(crate::i18n::tr("字节").into_owned()))
                                    .child(html::span(crate::i18n::tr("值").into_owned())),
                            );
                        for (index, parameter) in parameters.iter().enumerate() {
                            let kind = parameter.get("kind").and_then(Value::as_str).unwrap_or("");
                            let display = parameter
                                .get("display")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let bytes = parameter
                                .get("byte_len")
                                .and_then(Value::as_u64)
                                .map(|length| length.to_string())
                                .unwrap_or_else(|| crate::i18n::tr("未知").into_owned());
                            let mut status = crate::i18n::tr(
                                if parameter.get("complete").and_then(Value::as_bool) == Some(true)
                                {
                                    "完整"
                                } else {
                                    "不完整"
                                },
                            )
                            .into_owned();
                            if parameter.get("dynamic").and_then(Value::as_bool) == Some(true) {
                                status.push_str(" · ");
                                status.push_str(&crate::i18n::tr("动态生成"));
                            }
                            table = table.child(
                                html::div()
                                    .class("preview-parameter-row")
                                    .child(html::span((index + 1).to_string()))
                                    .child(html::span(kind.to_owned()))
                                    .child(html::span(bytes))
                                    .child(
                                        html::div()
                                            .class("preview-parameter-value")
                                            .child(html::span(status).class("muted"))
                                            .child(
                                                html::pre(display.to_owned())
                                                    .class("preview-parameter-text"),
                                            ),
                                    ),
                            );
                        }
                        panel = panel.child(table);
                    }
                }
            }
        }
    }
    panel.into()
}

pub(crate) fn overlay(app: &App, kind: &str) -> Node<Msg> {
    if kind == "project-local" {
        return super::project_workbench::local_dialog(app);
    }
    let title = crate::i18n::tr(match kind {
        "profile" => {
            if app.archery.server_only {
                if app.archery.saved {
                    "编辑服务"
                } else {
                    "新建服务"
                }
            } else if app.connection_form_is_editing() {
                "连接设置"
            } else {
                "新建连接"
            }
        }
        "archery-auth" => {
            if app.archery.state == "challenge_required" {
                "Archery 验证码认证"
            } else {
                "Archery 密码认证"
            }
        }
        "diagnostics" => app.detail_context.kind,
        "transfer" => "导入 / 导出",
        "import" => "导入",
        "export" => "导出",
        "replace" => "批量替换",
        "advanced-value" => "高级值编辑",
        "cell" => "单元格",
        "cli-value" => "CLI 单元格",
        "redis" => match app.field("operation") {
            "set_string" => "编辑 String",
            "hash_set" => "设置 Hash 字段",
            "hash_delete" => "删除 Hash 字段",
            "list_set" => "编辑 List 元素",
            "list_insert" => "插入 List 元素",
            "list_delete" => "删除 List 元素",
            "set_add" => "添加 Set 成员",
            "set_remove" => "删除 Set 成员",
            "zset_set" => "设置 ZSet 成员",
            "zset_remove" => "删除 ZSet 成员",
            "rename" => "重命名 key",
            "delete_key" => "删除 key",
            "set_ttl" => "设置过期时间",
            _ => "Redis 操作",
        },
        "columns" => "列配置",
        "filters" => "过滤与排序",
        "row" => "行操作",
        "delete-rows" => "删除选中行",
        "redis-delete-rows" => "删除选中条目",
        "redis-key-delete" => "删除选中 key",
        "profile-delete" => "确认删除连接",
        "database-create" => "新建数据库",
        "resource-drop" => "确认删除资源",
        "shortcuts" => "快捷键帮助",
        "preview" => "变更预览",
        "file" => "查询文件",
        "history" => "查询历史",
        _ => "设置",
    })
    .into_owned();
    let close = if (kind == "profile" && app.connection_form_busy())
        || (kind == "replace" && app.advanced_replace.busy())
    {
        ui::button("")
            .variant(ui::ButtonVariant::Quiet)
            .icon("x")
            .disabled(true)
            .title(crate::i18n::tr("关闭对话框").into_owned())
            .aria(crate::i18n::tr("关闭对话框").into_owned())
            .into()
    } else {
        icon_button("x", "关闭对话框", "overlay-close")
    };
    let mut panel = html::div().class("modal-body");
    if let Some(error) = &app.workbench.field_error {
        panel = panel.child(
            html::p(error.message.render().into_owned())
                .class("form-submit-error")
                .attr("role", "alert"),
        );
    }
    let mut footer = html::div()
        .class("modal-footer")
        .child(html::span("").class("spacer"))
        .child(button("关闭", "overlay-close"));
    match kind {
        "replace" => {
            panel = panel.child(crate::advanced::replace::view(&app.advanced_replace));
            footer = crate::advanced::replace::footer(&app.advanced_replace);
        }
        "advanced-value" => {
            if !app.advanced_files.confirm_clean.is_some() {
                panel = panel.child(crate::advanced::value::view(&app.advanced_value));
            }
            if !app.advanced_value.confirm_close {
                panel = panel.child(crate::advanced::files::view(
                    &app.advanced_files,
                    app.advanced_value.writable,
                    app.advanced_value.session.is_some(),
                    app.advanced_value.dirty(),
                ));
            }
            footer = if app.advanced_files.confirm_clean.is_some() {
                html::div().class("modal-footer")
            } else {
                crate::advanced::value::footer(&app.advanced_value)
            };
        }
        "history" => {
            panel = panel.child(history_panel(app));
            footer = history_footer(app);
        }
        "shortcuts" => panel = panel.child(shortcut_help()),
        "redis-delete-rows" => {
            panel = panel.child(html::p(crate::i18n::format_message(
                "将 {count} 个选中条目加入删除草稿，提交后才会写入数据库。",
                &[("count", &app.redis_row_delete_count().to_string())],
            )));
            if let Some((target, rows)) = app.redis_row_delete_targets() {
                panel = panel.child(
                    html::pre(workbench::redis_key_text(
                        &target.name,
                        target.key_b64.as_deref(),
                    ))
                    .class("preview"),
                );
                let resolved = app.redis_row_delete_resolved();
                let mut needs_load = false;
                for (position, (index, row)) in rows.iter().enumerate() {
                    if let Some(identity) = resolved
                        .and_then(|rows| rows.get(position))
                        .and_then(|row| row.first())
                        .or_else(|| row.first())
                    {
                        let large = matches!(identity, datapier_core::model::Value::Large { .. });
                        needs_load |= large;
                        let text = value_text(identity, true).into_owned();
                        let mut entry = html::div()
                            .class("key-delete-target")
                            .child(
                                html::span(format!("{} {}", crate::i18n::tr("条目"), index + 1))
                                    .class("muted"),
                            )
                            .child(html::pre(text.clone()).class("preview"));
                        if !large {
                            entry = entry.child(
                                ui::button(crate::i18n::tr("复制完整身份").into_owned())
                                    .on_click(Msg::CopyText(text)),
                            );
                        }
                        panel = panel.child(entry);
                    }
                }
                if needs_load {
                    panel = panel
                        .child(
                            html::p(
                                crate::i18n::tr("部分条目身份尚未完整读取；可读取后查看和复制。")
                                    .into_owned(),
                            )
                            .class("muted"),
                        )
                        .child(
                            ui::button(crate::i18n::tr("读取完整身份").into_owned())
                                .disabled(app.redis_row_delete_resolution_pending())
                                .on_click(Msg::Action("redis-row-delete-load-identities".into())),
                        );
                }
                if let Some(error) = app.redis_row_delete_error() {
                    panel = panel.child(html::p(error.to_owned()).class("form-submit-error"));
                }
            }
            footer = html::div()
                .class("modal-footer")
                .child(html::span("").class("spacer"))
                .child(button("取消", "redis-row-delete-cancel"))
                .child(
                    ui::button(crate::i18n::tr("加入草稿").into_owned())
                        .variant(ui::ButtonVariant::Danger)
                        .disabled(app.redis_row_delete_resolution_pending())
                        .on_click(Msg::Action("redis-row-delete-confirm".into())),
                );
        }
        "redis-key-delete" => {
            let targets = app.redis_key_delete_targets();
            panel = panel.child(html::p(crate::i18n::format_message(
                "将 {count} 个选中 key 加入删除草稿，提交后才会写入数据库。",
                &[("count", &targets.len().to_string())],
            )));
            for target in targets {
                panel = panel.child(
                    html::div()
                        .class("key-delete-target")
                        .child(
                            html::span(format!("DB {}", target.database.as_deref().unwrap_or("0")))
                                .class("muted"),
                        )
                        .child(html::span(workbench::redis_key_text(
                            &target.name,
                            target.key_b64.as_deref(),
                        ))),
                );
            }
            footer = html::div()
                .class("modal-footer")
                .child(html::span("").class("spacer"))
                .child(button("取消", "redis-key-delete-cancel"))
                .child(button("加入草稿", "redis-key-delete-confirm"));
        }
        "resource-drop" => {
            if let Some(drop) = app.navigation_drop_confirmation() {
                panel = panel
                    .child(html::strong(if drop.kind == "database" {
                        drop.name.clone()
                    } else {
                        format!("{}.{}", drop.database, drop.name)
                    }))
                    .child(html::p(
                        crate::i18n::tr(if drop.kind == "database" {
                            "将永久删除该数据库及其中全部表和数据。此操作不能撤销。"
                        } else {
                            "将永久删除该表及其全部数据。此操作不能撤销。"
                        })
                        .into_owned(),
                    ));
                if let Some(error) = &drop.error {
                    panel = panel.child(
                        html::p(crate::i18n::tr(error).into_owned()).class("connection-error"),
                    );
                }
                footer = html::div()
                    .class("modal-footer")
                    .child(html::span("").class("spacer"));
                if drop.busy {
                    footer = footer
                        .child(html::span(crate::i18n::tr("正在删除资源…").into_owned()))
                        .child(button(
                            "停止等待",
                            &format!("resource-drop-stop:{}", drop.token),
                        ));
                } else {
                    footer = footer
                        .child(button(
                            "取消",
                            &format!("resource-drop-cancel:{}", drop.token),
                        ))
                        .child(button(
                            if drop.kind == "database" {
                                "删除数据库"
                            } else {
                                "删除表"
                            },
                            &format!("resource-drop-confirm:{}", drop.token),
                        ));
                }
            }
        }
        "database-create" => {
            if let Some(form) = app.navigation_database_create() {
                let locked = form.unknown
                    || form.task.is_some()
                    || (form.busy && form.preview_input.is_none());
                let current = app.database_create_input();
                let preview_current = form.plan_input.is_some() && form.plan_input == current;
                if locked {
                    let name = form
                        .plan
                        .as_ref()
                        .and_then(|plan| plan.pointer("/target/name"))
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| app.field("database-create-name"));
                    panel = panel.child(html::p(format!(
                        "{}：{}",
                        crate::i18n::tr("数据库名称"),
                        name
                    )));
                } else {
                    panel = panel.child(field(app, "database-create-name", "数据库名称"));
                }
                if let Some((default_charset, server_collation, rows)) =
                    form.options.as_ref().and_then(|options| {
                        Some((
                            options["charset"].as_str()?,
                            options["collation"].as_str()?,
                            options["collations"].as_array()?,
                        ))
                    })
                {
                    let mut names = std::collections::BTreeSet::new();
                    for row in rows {
                        if let Some(name) = row["charset"].as_str() {
                            names.insert(name);
                        }
                    }
                    let mut charset = ui::select()
                        .aria(crate::i18n::tr("数据库字符集").into_owned())
                        .value(form.charset.clone())
                        .disabled(locked)
                        .open(form.open_select.as_deref() == Some("charset"))
                        .on_toggle(|collapsed| {
                            Msg::Action(format!(
                                "database-create-select-open:{}",
                                if collapsed { "" } else { "charset" }
                            ))
                        })
                        .option(ui::SelectOption::new(
                            "",
                            format!("{} ({default_charset})", crate::i18n::tr("服务器默认")),
                        ));
                    for name in names {
                        charset = charset.option(ui::SelectOption::new(name, name));
                    }
                    panel = panel
                        .child(html::strong(crate::i18n::tr("数据库字符集").into_owned()))
                        .child(charset.on_change(|change| {
                            Msg::Action(format!(
                                "database-create-charset:{}",
                                change.value.as_str().unwrap_or_default()
                            ))
                        }));
                    let selected_charset = if form.charset.is_empty() {
                        default_charset
                    } else {
                        &form.charset
                    };
                    let default_collation = if form.charset.is_empty() {
                        Some(server_collation)
                    } else {
                        rows.iter()
                            .find(|row| {
                                row["charset"] == selected_charset && row["default"] == true
                            })
                            .and_then(|row| row["name"].as_str())
                    };
                    let mut collation = ui::select()
                        .aria(crate::i18n::tr("排序规则").into_owned())
                        .value(form.collation.clone())
                        .disabled(locked)
                        .open(form.open_select.as_deref() == Some("collation"))
                        .on_toggle(|collapsed| {
                            Msg::Action(format!(
                                "database-create-select-open:{}",
                                if collapsed { "" } else { "collation" }
                            ))
                        });
                    if let Some(default_collation) = default_collation {
                        collation = collation.option(ui::SelectOption::new(
                            "",
                            format!("{} ({default_collation})", crate::i18n::tr("字符集默认")),
                        ));
                    }
                    for row in rows {
                        if row["charset"] == selected_charset
                            && let Some(name) = row["name"].as_str()
                        {
                            collation = collation
                                .option(ui::SelectOption::new(name.to_owned(), name.to_owned()));
                        }
                    }
                    panel = panel
                        .child(html::strong(crate::i18n::tr("排序规则").into_owned()))
                        .child(collation.on_change(|change| {
                            Msg::Action(format!(
                                "database-create-collation:{}",
                                change.value.as_str().unwrap_or_default()
                            ))
                        }));
                } else if form.busy {
                    panel =
                        panel.child(html::p(crate::i18n::tr("正在读取数据库选项…").into_owned()));
                }
                if preview_current && let Some(sql) = &form.sql {
                    panel = panel.child(ui::code(sql.clone()).lang("sql"));
                }
                if form.options.is_some()
                    && !preview_current
                    && !form.unknown
                    && form.error.is_none()
                    && !app.field("database-create-name").trim().is_empty()
                {
                    panel = panel.child(
                        html::p(crate::i18n::tr("正在生成 SQL 预览…").into_owned()).class("muted"),
                    );
                }
                if let Some(error) = &form.error {
                    panel = panel.child(
                        html::p(crate::i18n::tr(error).into_owned()).class("connection-error"),
                    );
                }
                footer = html::div()
                    .class("modal-footer")
                    .child(html::span("").class("spacer"));
                if !locked {
                    footer = footer.child(
                        ui::button(crate::i18n::tr("取消").into_owned())
                            .disabled(form.busy)
                            .on_click(Msg::Action("overlay-close".into())),
                    );
                    footer = footer.child(
                        ui::button(crate::i18n::tr("执行").into_owned())
                            .disabled(form.busy || !preview_current || form.sql.is_none())
                            .on_click(Msg::Action("database-create-submit".into())),
                    );
                } else if form.unknown && !form.busy && form.task.is_none() {
                    footer = footer.child(button("取消", "overlay-close"));
                } else {
                    footer = footer.child(html::span(crate::i18n::tr("操作进行中…").into_owned()));
                }
            }
        }
        "profile-delete" => {
            if let Some(confirmation) = app.navigation_delete_confirmation() {
                panel = panel
                    .child(html::p(confirmation.name.clone()))
                    .child(html::p(
                        crate::i18n::tr(
                            "关闭当前工作区的相关标签和会话并删除连接配置；数据库数据保留。",
                        )
                        .into_owned(),
                    ));
                if let Some(error) = &confirmation.error {
                    panel = panel.child(
                        html::p(crate::i18n::tr(error).into_owned()).class("connection-error"),
                    );
                }
                footer = html::div()
                    .class("modal-footer")
                    .child(html::span("").class("spacer"));
                if confirmation.busy {
                    footer = footer.child(html::span(
                        crate::i18n::tr("正在删除连接，请等待").into_owned(),
                    ));
                } else {
                    footer = footer
                        .child(button(
                            "取消",
                            &format!("profile-delete-cancel:{}", confirmation.token),
                        ))
                        .child(button(
                            "删除连接",
                            &format!("{}{}:{}", "profile-delete-", "confirm", confirmation.token),
                        ));
                }
            }
        }
        "profile" => {
            (panel, footer) = profile_form::profile_form(app);
        }
        "archery-auth" => {
            (panel, footer) = archery::auth_dialog(app);
        }
        "transfer" | "import" | "export" => {
            let archery = app
                .profile
                .as_ref()
                .is_some_and(|profile| profile.is_archery());
            let result = if app.workbench.cli_export || (archery && app.is_redis()) {
                app.workbench
                    .cli_grid
                    .as_ref()
                    .map(|grid| &grid.page.result)
            } else {
                app.grid.as_ref().map(|grid| &grid.page.result)
            };
            let archery_result = result.is_some_and(|result| {
                result
                    .summary
                    .get("origin")
                    .and_then(|origin| origin.get("source"))
                    .and_then(Value::as_str)
                    == Some("archery")
            });
            let configured = app.field("mapping") != "{}"
                || app.field("null_marker") != "\\N"
                || app.field("backslash_escape") != "true"
                || app.field("binary_columns") != "[]";
            let mut form = html::div()
                .class("transfer-form")
                .child(controls::path_field(
                    app,
                    "path",
                    "文件路径",
                    match kind {
                        "export" => &[("save", "", "浏览…")],
                        "import" => &[("file", "csv,json", "浏览…")],
                        _ => &[("file", "csv,json", "浏览…"), ("save", "", "选择保存位置")],
                    },
                ))
                .child(choices(
                    app,
                    "format",
                    "文件格式",
                    &[("csv", "CSV"), ("json", "JSON")],
                ))
                .child(
                    ui::button(
                        crate::i18n::tr(if app.workbench.transfer_advanced {
                            "▾ 高级选项"
                        } else if configured {
                            "▸ 高级选项（含自定义配置）"
                        } else {
                            "▸ 高级选项"
                        })
                        .into_owned(),
                    )
                    .variant(ui::ButtonVariant::Quiet)
                    .on_click(Msg::Action("transfer-advanced-toggle".into())),
                );
            if app.workbench.transfer_advanced {
                let mut advanced = html::div().class("transfer-advanced");
                advanced = advanced.child(field(app, "mapping", "列映射 JSON"));
                advanced = advanced.child(field(app, "null_marker", "NULL 标记"));
                advanced = advanced.child(
                    ui::switch(crate::i18n::tr("反斜线转义").into_owned())
                        .on(app.field("backslash_escape") == "true")
                        .on_change({
                            let next = app.field("backslash_escape") != "true";
                            move |_| Msg::Action(format!("field:backslash_escape:{next}"))
                        }),
                );
                advanced = advanced.child(field(app, "binary_columns", "二进制列 JSON"));
                advanced = advanced.child(
                    html::p(
                        crate::i18n::tr("选项修改保存在表单草稿中，执行导入或导出时生效。")
                            .into_owned(),
                    )
                    .class("muted"),
                );
                form = form.child(advanced);
            }
            panel = panel.child(form);
            if archery_result && app.field("format") == "csv" {
                panel = panel.child(
                    html::p(crate::i18n::tr("CSV 不包含来源元数据").into_owned()).class("muted"),
                );
            }
            if kind != "import" && !archery && !archery_result {
                panel = panel.child(html::p(crate::i18n::tr("安全只读源会重新查询，结果可能与屏幕不同；其他结果仅导出仍保留的数据。").into_owned()).class("muted"));
            }
            if kind != "export" && import_writable(app) {
                footer = footer
                    .child(button("预览导入", "preview-import"))
                    .child(button("确认导入", "import"));
            } else if !archery
                && kind != "export"
                && (app.is_redis()
                    || app
                        .target
                        .as_ref()
                        .is_some_and(|target| target.kind == "table"))
            {
                panel = panel.child(
                    html::p(
                        crate::i18n::tr("当前连接只读或编辑基线不可用，无法导入。").into_owned(),
                    )
                    .class("muted"),
                );
                footer = footer
                    .child(ui::button(crate::i18n::tr("预览导入").into_owned()).disabled(true))
                    .child(ui::button(crate::i18n::tr("确认导入").into_owned()).disabled(true));
            }
            if kind != "import" && (!archery || result.is_some_and(|result| result.complete)) {
                footer = footer.child(
                    ui::button(
                        crate::i18n::tr(if archery_result || archery {
                            "导出本次返回结果"
                        } else {
                            "导出"
                        })
                        .into_owned(),
                    )
                    .key("export")
                    .variant(if kind == "export" {
                        ui::ButtonVariant::Primary
                    } else {
                        ui::ButtonVariant::Default
                    })
                    .on_click(Msg::Action("export".into())),
                );
            } else if kind != "import" && archery {
                panel = panel.child(
                    html::p(crate::i18n::tr("结果尚未完整返回，暂不能导出。").into_owned())
                        .class("muted"),
                );
                footer = footer.child(
                    ui::button(crate::i18n::tr("导出本次返回结果").into_owned()).disabled(true),
                );
            }
        }
        "file" => {
            panel = panel.child(controls::path_field(
                app,
                "path",
                "文件路径",
                &[("file", "sql", "浏览…"), ("save", "", "选择保存位置")],
            ));
            if let Some(base) = &app.query_base {
                panel = panel.child(
                    html::p(i18n::format_message(
                        "相对路径基于 {directory}",
                        &[("directory", &base.to_string_lossy())],
                    ))
                    .class("muted"),
                );
            }
            footer = footer
                .child(button("打开", "file-open"))
                .child(button("保存", "file-save"));
        }
        "columns" => {
            if let Some(grid) = &app.grid {
                let mut selector = ui::select()
                    .aria(crate::i18n::tr("列").into_owned())
                    .value(app.field("column").to_owned())
                    .open(app.workbench.generic_select.as_deref() == Some("column-picker"))
                    .on_toggle(|collapsed| {
                        Msg::Action(format!("select-open:column-picker:{}", !collapsed))
                    })
                    .on_change(|change| {
                        Msg::Action(format!(
                            "field:column:{}",
                            change.value.as_str().unwrap_or_default()
                        ))
                    });
                for (index, column) in grid.page.result.columns.iter().enumerate() {
                    selector = selector.option(ui::SelectOption::new(
                        index.to_string(),
                        column.name.clone(),
                    ));
                    if let Some(setting) = grid.settings.get(index) {
                        panel = panel.child(
                            html::div()
                                .class("choice-row")
                                .child(
                                    html::label()
                                        .child(
                                            html::checkbox("visible-column", index.to_string())
                                                .checked(!setting.hidden)
                                                .on_change(move |_| {
                                                    Msg::Action(format!(
                                                        "column-select-hide:{index}"
                                                    ))
                                                }),
                                        )
                                        .child(html::span(column.name.clone())),
                                )
                                .child(
                                    html::label()
                                        .child(
                                            html::checkbox("frozen-column", index.to_string())
                                                .checked(setting.frozen)
                                                .on_change(move |_| {
                                                    Msg::Action(format!(
                                                        "column-select-freeze:{index}"
                                                    ))
                                                }),
                                        )
                                        .child(html::span(crate::i18n::tr("冻结").into_owned())),
                                ),
                        );
                    }
                }
                panel = panel
                    .child(ui::field(crate::i18n::tr("设置宽度的列").into_owned()).child(selector))
                    .child(field(app, "width", "宽度（像素）"))
                    .child(
                        html::p(
                            crate::i18n::tr("设置宽度时必填：1–65535 像素。显示和冻结立即应用。")
                                .into_owned(),
                        )
                        .class("muted"),
                    );
                footer = footer.child(button("设置宽度", "column-width"));
            }
        }
        "filters" => {
            let (form, action) = crate::forms::filter_form(app);
            panel = panel.child(form);
            footer = footer.child(action);
        }
        "delete-rows" => {
            let count = app.delete_rows_count().to_string();
            let (database, table) = app.delete_rows_target().unwrap_or(("", ""));
            panel = panel.child(html::p(crate::i18n::format_message(
                "确认删除 {count} 行？目标：{database}.{table}。确认后只加入待提交变更。",
                &[("count", &count), ("database", database), ("table", table)],
            )));
            footer = html::div()
                .class("modal-footer")
                .child(html::span("").class("spacer"))
                .child(button("取消", "overlay-close"))
                .child(
                    ui::button(crate::i18n::format_message(
                        "删除 {count} 行",
                        &[("count", &count)],
                    ))
                    .variant(ui::ButtonVariant::Danger)
                    .icon("trash")
                    .key("delete-rows-confirm")
                    .on_click(Msg::Action("delete-rows-confirm".into())),
                );
        }
        "row" => {
            if writable(app) {
                let (form, action) = crate::forms::row_form(app);
                panel = panel.child(form);
                footer = footer.child(action);
            }
        }
        "cell" => {
            let text = if app.is_redis() {
                app.detail_cell()
                    .and_then(|cell| app.grid.as_ref()?.value(cell))
                    .map(|value| value_text(value, true).into_owned())
                    .unwrap_or_default()
            } else {
                app.field("cell").to_owned()
            };
            panel = panel.child(html::pre(text).class("preview"));
            if writable(app) && !app.is_redis() {
                panel = panel.child(field(app, "cell", "单元格值"));
                footer = footer
                    .child(button("设置文本", "set-cell"))
                    .child(button("设置 JSON", "set-json"))
                    .child(button("设置 NULL", "set-null"));
            }
        }
        "cli-value" => {
            if let Some(cell) = app.workbench.cli_cell
                && let Some(grid) = &app.workbench.cli_grid
                && let Some(value) = grid.value(cell)
            {
                if matches!(value, datapier_core::model::Value::Large { .. }) {
                    let message = if app.workbench.cli_large.is_some() {
                        crate::i18n::tr("读取中…").into_owned()
                    } else if !app.workbench.cli_error.is_empty() {
                        app.workbench.cli_error.render().into_owned()
                    } else {
                        crate::i18n::tr("尚未读取完整值").into_owned()
                    };
                    panel = panel.child(html::span(message).class("muted"));
                } else {
                    panel = panel
                        .child(html::pre(value_text(value, true).into_owned()).class("preview"));
                    footer = footer.child(
                        ui::button(crate::i18n::tr("复制").into_owned())
                            .icon("copy")
                            .on_click(Msg::CopyText(value_text(value, true).into_owned())),
                    );
                }
            }
        }
        "preview" => {
            if app.paste_preview.is_some() {
                panel = panel.child(paste_preview_panel(app));
                if writable(app) {
                    footer = footer.child(button("确认粘贴", "paste-confirm"));
                }
            } else if app.change_preview_loading() {
                panel = panel.child(
                    html::p(crate::i18n::tr("正在生成变更预览…").into_owned()).class("muted"),
                );
            } else if let Some(error) = app.change_preview_error() {
                panel = panel.child(html::p(error.to_owned()).class("connection-error"));
            } else if let Some(preview) = app.change_preview_value() {
                panel = panel.child(change_preview_panel(preview));
                let offset = preview.get("offset").and_then(Value::as_u64).unwrap_or(0);
                let total = preview
                    .get("total_changes")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                panel = panel.child(
                    html::p(crate::i18n::format_message(
                        "草稿总数：{total} · 当前偏移：{offset}",
                        &[
                            ("total", &total.to_string()),
                            ("offset", &offset.to_string()),
                        ],
                    ))
                    .class("muted"),
                );
                if offset > 0 {
                    footer = footer.child(button("上一页", "change-preview-prev"));
                }
                if preview.get("next_offset").and_then(Value::as_u64).is_some() {
                    footer = footer.child(button("下一页", "change-preview-next"));
                }
                if writable(app) {
                    footer = footer.child(button(
                        "提交变更",
                        if app.is_redis() {
                            "redis-submit"
                        } else {
                            "submit"
                        },
                    ));
                }
            } else {
                panel = panel
                    .child(html::p(crate::i18n::tr("变更预览不可用").into_owned()).class("muted"));
            }
        }
        "diagnostics" => {
            let task = app.detail_context.kind == "任务详情";
            for (label, value) in [
                ("连接", &app.detail_context.connection),
                ("目标资源", &app.detail_context.resource),
            ] {
                if task {
                    continue;
                }
                if let Some(value) = value {
                    panel = panel.child(
                        html::div()
                            .class("diagnostic-row")
                            .child(html::span(crate::i18n::tr(label).into_owned()).class("muted"))
                            .child(html::span(value.clone())),
                    );
                }
            }
            let recovery = app
                .detail
                .strip_prefix("恢复的编辑意图（基线已失效，请刷新并重新确认）：");
            if recovery.is_some() {
                panel = panel.child(html::div().class("notice").child(html::span(
                    crate::i18n::tr("恢复的编辑意图（基线已失效，请刷新并重新确认）").into_owned(),
                )));
            }
            if let Ok(value) = serde_json::from_str::<Value>(recovery.unwrap_or(&app.detail)) {
                if app.detail_context.kind == "任务详情"
                    && value.get("method").and_then(Value::as_str).is_some()
                    && value.get("state").and_then(Value::as_str).is_some()
                {
                    panel = panel.child(diagnostic_task(app, &value));
                } else {
                    panel = panel.child(diagnostic_detail(app.detail_context.kind, &value));
                }
            } else {
                panel = panel.child(
                    html::pre(diagnostic_text(&Value::String(app.detail.clone()))).class("preview"),
                );
            }
            if !task {
                panel = panel.child(
                    html::p(
                        crate::i18n::tr("内容较长时仅显示部分；完整内容见下方诊断或复制。")
                            .into_owned(),
                    )
                    .class("muted"),
                );
            }
            panel = panel.child(
                ui::tree().item(
                    ui::TreeItem::new(
                        "complete-diagnostic",
                        crate::i18n::tr("完整诊断").into_owned(),
                    )
                    .open(false)
                    .child(ui::TreeItem::new(
                        "complete-diagnostic-value",
                        app.detail.clone(),
                    )),
                ),
            );
            footer = footer.child(
                ui::button(crate::i18n::tr("复制完整诊断").into_owned())
                    .icon("copy")
                    .on_click(Msg::CopyText(app.detail.clone())),
            );
        }
        "settings" => {
            (panel, footer) = settings::render(app, panel);
        }
        "redis" if writable(app) => {
            let op = app.field("operation");
            let mut form = html::div().class("form-grid redis-form-grid");
            if let Some(target) = &app.target {
                let key = target
                    .key_b64
                    .as_deref()
                    .map(redis_bytes_text)
                    .unwrap_or_else(|| std::borrow::Cow::Borrowed(target.name.as_str()));
                form = form.child(
                    html::div()
                        .class("form-field wide redis-key-context")
                        .child(html::span(crate::i18n::tr("当前 key").into_owned()).class("label"))
                        .child(html::pre(key.into_owned()).class("preview")),
                );
            }
            match op {
                "set_string" => form = form.child(field(app, "redis_value", "Value")),
                "hash_set" | "hash_delete" => {
                    form = form.child(field(app, "redis_field", "Field"));
                    if op == "hash_set" {
                        form = form.child(field(app, "redis_value", "Value"));
                    }
                }
                "list_set" | "list_insert" | "list_delete" => {
                    form = form.child(
                        html::div()
                            .class("form-field")
                            .child(
                                html::span(crate::i18n::tr("Index（当前元素位置）").into_owned())
                                    .class("label"),
                            )
                            .child(html::span(app.field("redis_index").to_owned())),
                    );
                    if op != "list_delete" {
                        form = form.child(field(app, "redis_value", "Value"));
                    }
                }
                "set_add" | "set_remove" => {
                    form = form.child(field(app, "redis_member", "Member"));
                }
                "zset_set" | "zset_remove" => {
                    form = form.child(field(app, "redis_member", "Member"));
                    if op == "zset_set" {
                        form = form.child(field(app, "redis_score", "Score"));
                    }
                }
                "rename" => form = form.child(field(app, "redis_key", "新 key")),
                "set_ttl" => {
                    let ttl = app
                        .schema
                        .as_ref()
                        .and_then(|schema| schema.get("ttl_ms"))
                        .and_then(Value::as_i64);
                    form = form.child(
                        html::div()
                            .class("form-field wide")
                            .child(
                                html::span(crate::i18n::tr("当前 TTL").into_owned()).class("label"),
                            )
                            .child(html::span(ttl_label(ttl))),
                    );
                    let selected = match app.workbench.ttl_policy {
                        workbench::RedisTtlPolicy::Keep => "keep",
                        workbench::RedisTtlPolicy::Set => "set",
                        workbench::RedisTtlPolicy::Persist => "persist",
                    };
                    let mut select = ui::select()
                        .aria(crate::i18n::tr("过期策略").into_owned())
                        .value(selected)
                        .open(app.workbench.generic_select.as_deref() == Some("redis-ttl"))
                        .on_toggle(|collapsed| {
                            Msg::Action(format!("select-open:redis-ttl:{}", !collapsed))
                        })
                        .on_change(|change| {
                            Msg::Action(format!(
                                "redis-ttl:{}",
                                change.value.as_str().unwrap_or_default()
                            ))
                        });
                    for (value, label) in [
                        ("keep", "保留现有 TTL"),
                        ("set", "设置新 TTL"),
                        ("persist", "移除期限"),
                    ] {
                        select = select.option(ui::SelectOption::new(
                            value,
                            crate::i18n::tr(label).into_owned(),
                        ));
                    }
                    form = form
                        .child(ui::field(crate::i18n::tr("过期策略").into_owned()).child(select));
                    if app.workbench.ttl_policy == workbench::RedisTtlPolicy::Set {
                        form = form.child(field(app, "redis_ttl", "期限（毫秒）"));
                        form = form.child(
                            html::p(
                                crate::i18n::tr("请输入非负整数毫秒数；0 会立即使 key 过期。")
                                    .into_owned(),
                            )
                            .class("muted"),
                        );
                    }
                }
                _ => {}
            }
            let note = match op {
                "set_ttl" => "保留现有 TTL 不产生修改；设置或移除期限先加入草稿，显式提交后生效。",
                "rename" => {
                    "重命名先加入草稿，再在变更预览中提交；新 key 可使用 base64: 前缀表示原始字节。"
                }
                "delete_key" => "删除先加入草稿，再在变更预览中提交。请确认上方完整 key。",
                "list_set" => {
                    "Index 用于定位当前元素；修改 Value 不改变元素位置。编辑先加入草稿，默认保留 TTL。"
                }
                "list_insert" | "list_delete" => {
                    "Index 用于定位选中的元素；操作先加入草稿，默认保留 TTL。"
                }
                _ => {
                    "操作先加入草稿，再在变更预览中提交；默认保留 TTL。字节值可使用 base64: 前缀。"
                }
            };
            panel = panel.child(form).child(
                html::div()
                    .class("notice")
                    .child(html::span(crate::i18n::tr(note).into_owned())),
            );
            if !matches!(
                op,
                "set_ttl"
                    | "delete_key"
                    | "hash_delete"
                    | "list_delete"
                    | "set_remove"
                    | "zset_remove"
            ) {
                panel = panel.child(
                    html::p(
                        crate::i18n::tr(
                            "Key、Field、Member 和 Value 允许为空；base64: 可表示空字节。",
                        )
                        .into_owned(),
                    )
                    .class("muted"),
                );
            }
            footer = footer.child(
                if matches!(
                    op,
                    "delete_key" | "hash_delete" | "list_delete" | "set_remove" | "zset_remove"
                ) {
                    ui::button(crate::i18n::tr("加入草稿").into_owned())
                        .variant(ui::ButtonVariant::Danger)
                        .icon("trash")
                        .on_click(Msg::Action("redis-stage".into()))
                        .into()
                } else {
                    button("加入草稿", "redis-stage")
                },
            );
        }
        _ => {}
    }
    if kind == "settings" && app.field("settings-reload-confirm") == "true" {
        panel = html::div().class("modal-body").child(html::p(
            crate::i18n::tr("重新加载将丢弃未保存的设置。继续编辑或丢弃并重新加载？").into_owned(),
        ));
        footer = html::div()
            .class("modal-footer")
            .child(html::span("").class("spacer"))
            .child(button("继续编辑", "settings-reload-cancel"))
            .child(
                ui::button(crate::i18n::tr("丢弃并重新加载").into_owned())
                    .variant(ui::ButtonVariant::Danger)
                    .on_click(Msg::Action("settings-reload-confirm".into())),
            );
    }
    if app.workbench.overlay_close_confirm {
        panel = html::div().class("modal-body").child(html::p(
            crate::i18n::tr("关闭将丢弃未应用的输入。继续编辑或丢弃？").into_owned(),
        ));
        footer = html::div()
            .class("modal-footer")
            .child(html::span("").class("spacer"))
            .child(
                ui::button(crate::i18n::tr("继续编辑").into_owned())
                    .key("overlay-close-safe")
                    .on_click(Msg::Action("overlay-close-continue".into())),
            )
            .child(
                ui::button(crate::i18n::tr("丢弃").into_owned())
                    .variant(ui::ButtonVariant::Danger)
                    .on_click(Msg::Action("overlay-close-discard".into())),
            );
    }
    let picker_open = app.workbench.cell_fields_open
        || app.workbench.sql_forms.open_select.is_some()
        || app.workbench.generic_select.is_some();
    // SDK 把 slot 移到 body 之后；正文显式命名，焦点路径不依赖构建顺序。
    let overlay: Node<Msg> = ui::overlay()
        .modal(true)
        .size(if kind == "settings" {
            ui::OverlaySize::Full
        } else {
            ui::OverlaySize::Lg
        })
        .head(title)
        .end(close)
        .foot(Node::from(footer))
        .child(panel.key("body"))
        .into();
    // class 仅属于 el；用外层限定菜单溢出，不改原生 overlay 的视觉样式。
    html::div()
        .class(format!(
            "modal-layer {}{}{}",
            app.viewport_class(),
            if picker_open {
                " modal-picker-open"
            } else {
                ""
            },
            if kind == "settings" {
                " settings-dialog"
            } else {
                ""
            }
        ))
        .key("modal")
        .child(overlay)
        .into()
}
