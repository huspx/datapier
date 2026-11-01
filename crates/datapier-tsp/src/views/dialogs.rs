use super::*;
mod diagnostics;
use diagnostics::{diagnostic_content, diagnostic_task, diagnostic_text};
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
    let title = crate::i18n::tr(match kind {
        "profile" => {
            if app.connection_form_is_editing() {
                "连接设置"
            } else {
                "新建连接"
            }
        }
        "diagnostics" => app.detail_context.kind,
        "transfer" => "导入 / 导出",
        "import" => "导入",
        "export" => "导出",
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
        "resource-drop" => "确认删除资源",
        "shortcuts" => "快捷键帮助",
        "preview" => "变更预览",
        "file" => "查询文件",
        "history" => "查询历史",
        _ => "设置",
    })
    .into_owned();
    let close = if kind == "profile" && app.connection_form_busy() {
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
        "history" => {
            panel = panel.child(history_panel(app));
            let label = crate::i18n::tr(if app.is_redis() {
                "打开到 CLI"
            } else {
                "打开到查询"
            })
            .into_owned();
            let mut open = ui::button(label)
                .variant(ui::ButtonVariant::Primary)
                .disabled(app.workbench.history_selected.is_none());
            if let Some(index) = app.workbench.history_selected {
                open = open.on_click(Msg::Action(format!("history-open:{index}")));
            }
            footer = footer.child(open);
        }
        "shortcuts" => panel = panel.child(shortcut_help()),
        "redis-delete-rows" => {
            panel = panel.child(html::p(crate::i18n::format_message(
                "将 {count} 个选中条目加入删除草稿，提交后才会写入数据库。",
                &[("count", &app.redis_row_delete_count().to_string())],
            )));
            if let Some(target) = &app.target {
                panel = panel.child(
                    html::pre(workbench::redis_key_text(
                        &target.name,
                        target.key_b64.as_deref(),
                    ))
                    .class("preview"),
                );
            }
            footer = html::div()
                .class("modal-footer")
                .child(html::span("").class("spacer"))
                .child(button("取消", "redis-row-delete-cancel"))
                .child(button("加入草稿", "redis-row-delete-confirm"));
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
        "profile-delete" => {
            if let Some(confirmation) = app.navigation_delete_confirmation() {
                panel = panel
                    .child(html::p(confirmation.name.clone()))
                    .child(html::p(
                        crate::i18n::tr("关闭当前工作区的相关会话并删除连接配置；数据库数据保留。")
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
        "transfer" | "import" | "export" => {
            let configured = app.field("mapping") != "{}"
                || app.field("null_marker") != "\\N"
                || app.field("backslash_escape") != "true"
                || app.field("binary_columns") != "[]";
            let mut form = html::div()
                .class("transfer-form")
                .child(field(app, "path", "文件路径"))
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
                for (name, label) in [
                    ("mapping", "列映射 JSON"),
                    ("null_marker", "NULL 标记"),
                    ("backslash_escape", "反斜线转义"),
                    ("binary_columns", "二进制列 JSON"),
                ] {
                    advanced = advanced.child(field(app, name, label));
                }
                form = form.child(advanced);
            }
            panel = panel.child(form);
            if kind != "export"
                && ((writable(app)
                    && (app.is_redis() || app.target.as_ref().is_some_and(|t| t.kind == "table")))
                    || (app.is_redis()
                        && app.target.is_none()
                        && app.profile.as_ref().is_some_and(|p| {
                            p.access_mode == datapier_core::model::AccessMode::ReadWrite
                        })))
            {
                footer = footer
                    .child(button("预览导入", "preview-import"))
                    .child(button("确认导入", "import"));
            }
            if kind != "import" {
                footer = footer.child(button("导出完整结果", "export"));
            }
        }
        "file" => {
            panel = panel.child(field(app, "path", "文件路径"));
            footer = footer
                .child(button("打开", "file-open"))
                .child(button("保存", "file-save"));
        }
        "columns" => {
            panel = panel
                .child(field(app, "column", "列序号"))
                .child(field(app, "width", "宽度"));
            footer = footer
                .child(button("设置宽度", "column-width"))
                .child(button("隐藏 / 显示", "column-hide"))
                .child(button("冻结 / 解冻", "column-freeze"));
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
            panel = panel.child(html::pre(app.field("cell")).class("preview"));
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
                    panel = panel.child(diagnostic_content("内容", &value, 0, &mut 120));
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
            let value = match app.settings.language {
                datapier_core::model::LanguagePreference::System => "system",
                datapier_core::model::LanguagePreference::ZhCn => "zh-CN",
                datapier_core::model::LanguagePreference::En => "en",
            };
            let language = ui::select()
                .value(value)
                .option(ui::SelectOption::new(
                    "system",
                    crate::i18n::tr("跟随系统").into_owned(),
                ))
                .option(ui::SelectOption::new(
                    "zh-CN",
                    crate::i18n::tr("简体中文").into_owned(),
                ))
                .option(ui::SelectOption::new("en", "English"))
                .open(app.workbench.generic_select.as_deref() == Some("settings-language"))
                .on_toggle(|collapsed| {
                    Msg::Action(format!("select-open:settings-language:{}", !collapsed))
                })
                .on_change(|change| {
                    Msg::Action(format!(
                        "language:{}",
                        change.value.as_str().unwrap_or_default()
                    ))
                });
            panel = panel.child(ui::field(crate::i18n::tr("语言").into_owned()).child(language));
        }
        "redis" if writable(app) => {
            let op = app.field("operation");
            let mut form = html::div().class(if op == "zset_set" {
                "form-grid redis-form-grid redis-zset-form"
            } else {
                "form-grid redis-form-grid"
            });
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
                    let mut choices = html::div().class("choice-row wide");
                    for (policy, label, action) in [
                        (
                            workbench::RedisTtlPolicy::Keep,
                            "保留现有 TTL",
                            "redis-ttl:keep",
                        ),
                        (
                            workbench::RedisTtlPolicy::Set,
                            "设置新 TTL",
                            "redis-ttl:set",
                        ),
                        (
                            workbench::RedisTtlPolicy::Persist,
                            "移除期限",
                            "redis-ttl:persist",
                        ),
                    ] {
                        choices = choices.child(
                            ui::button(crate::i18n::tr(label).into_owned())
                                .on(app.workbench.ttl_policy == policy)
                                .key(action)
                                .on_click(Msg::Action(action.into())),
                        );
                    }
                    form = form.child(choices);
                    if app.workbench.ttl_policy == workbench::RedisTtlPolicy::Set {
                        form = form.child(field(app, "redis_ttl", "期限（毫秒）"));
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
    let picker_open = app.navigation.form_redis_db_menu
        || app.workbench.cell_fields_open
        || app.workbench.sql_forms.open_select.is_some()
        || app.workbench.generic_select.is_some();
    // SDK 把 slot 移到 body 之后；正文显式命名，焦点路径不依赖构建顺序。
    let overlay: Node<Msg> = ui::overlay()
        .modal(true)
        .size(ui::OverlaySize::Lg)
        .head(title)
        .end(close)
        .foot(Node::from(footer))
        .child(panel.key("body"))
        .into();
    // class 仅属于 el；用外层限定菜单溢出，不改原生 overlay 的视觉样式。
    html::div()
        .class(format!(
            "modal-layer {}{}",
            app.viewport_class(),
            if picker_open {
                " modal-picker-open"
            } else {
                ""
            }
        ))
        .key("modal")
        .child(overlay)
        .into()
}
