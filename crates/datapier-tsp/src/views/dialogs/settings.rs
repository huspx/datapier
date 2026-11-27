use super::*;
fn settings_section(title: &str) -> html::El<Msg> {
    let title = crate::i18n::tr(title).into_owned();
    html::div()
        .class("settings-section")
        .attr("role", "group")
        .attr("aria-label", title.clone())
        .child(html::strong(title).class("settings-section-title"))
}

// 存储分组保持不变；显示时把超时独立成组、平台请求并入读取批量。
fn settings_display_group<'a>(group: &'a str, key: &str) -> &'a str {
    match group {
        "archery" => "paging",
        "limits" if key.ends_with("seconds") => "timeouts",
        _ => group,
    }
}

pub(super) fn render(app: &App, mut panel: html::El<Msg>) -> (html::El<Msg>, html::El<Msg>) {
    let mut footer;
    let current_tab = app.field("settings-tab");
    let mcp = current_tab == "mcp";
    let mut tabs = html::div()
        .class("mcp-tabs settings-tabs")
        .attr("role", "tablist")
        .attr("aria-label", crate::i18n::tr("设置").into_owned());
    for (tab, label) in [
        ("general", "常规"),
        ("updates", "更新"),
        ("editing", "编辑与文件"),
        ("archery", "Archery 服务器"),
        ("mcp", "MCP"),
    ] {
        let selected = current_tab == tab;
        let key = format!("settings-tab-{tab}");
        let focused = app.focus == key;
        tabs = tabs.child(
            html::button(crate::i18n::tr(label).into_owned())
                .key(key.clone())
                .class(match (selected, focused) {
                    (true, true) => "mcp-tab active focused",
                    (true, false) => "mcp-tab active",
                    (false, true) => "mcp-tab focused",
                    (false, false) => "mcp-tab",
                })
                .attr("role", "tab")
                .attr("aria-selected", selected)
                .on_focus(Msg::Focus(key))
                .on_click(Msg::Action(format!("settings-tab:{tab}"))),
        );
    }
    panel = panel.child(tabs);
    let settings_ready = app.settings_loaded && app.field("settings-reload") != "true";
    let mut settings_layout = html::div().class("settings-grid");
    if settings_ready && current_tab != "archery" {
        let status = if app.native_ui.settings_reloading() {
            "加载中…"
        } else if app.settings_save_token.is_some() || app.native_ui.settings_busy() {
            "全局设置保存中…"
        } else if !app.settings_error.is_empty() || app.native_ui.policy_error.is_some() {
            "全局设置保存失败"
        } else if app.settings_global_form_dirty() || app.native_ui.settings_dirty() {
            "全局设置有未保存修改"
        } else {
            "全局设置已保存"
        };
        panel = panel.child(html::p(crate::i18n::tr(status).into_owned()).class("muted"));
    }
    if current_tab != "archery" && !settings_ready && app.settings_error.is_empty() {
        panel = panel.child(html::p(crate::i18n::tr("加载中…").into_owned()).class("empty-note"));
    }
    if settings_ready && current_tab == "general" {
        let value = app.field("settings-language-draft");
        let language = ui::select()
            .aria(crate::i18n::tr("语言").into_owned())
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
                    "settings-language:{}",
                    change.value.as_str().unwrap_or_default()
                ))
            });
        panel = panel.child(
            html::div().class("settings-language").child(
                ui::field(crate::i18n::tr("语言").into_owned())
                    .variant(ui::FieldVariant::Row)
                    .child(language),
            ),
        );
    }
    if settings_ready && current_tab == "updates" {
        settings_layout = settings_layout.child(crate::native_ui::update_settings(app));
    }
    if settings_ready && current_tab == "editing" {
        let mut panel = settings_section("编辑限制");
        for (name, label) in [
            ("editing_plan_items", "计划条目上限"),
            ("editing_plan_bytes", "计划字节上限"),
            ("editing_value_bytes", "单值字节上限"),
            ("editing_text_bytes", "文本编辑字节上限"),
            ("editing_preview_rows", "预览每页条目数"),
        ] {
            panel = panel.child(field(app, name, label));
        }
        panel = panel.child(
            html::p(
                crate::i18n::tr("均为正整数；字节上限以字节计，文本编辑上限不能超过单值上限。")
                    .into_owned(),
            )
            .class("muted"),
        );
        settings_layout = settings_layout.child(panel);
        let mut panel = settings_section("外部编辑器");
        panel = panel.child(choices(
            app,
            "external_mode",
            "外部编辑器模式",
            &[
                ("disabled", "不配置"),
                ("gui", "图形界面"),
                ("terminal", "终端"),
            ],
        ));
        if app.field("external_mode") != "disabled" {
            panel = panel.child(field(app, "external_program", "编辑器程序"));
            panel = panel.child(field(app, "external_args", "编辑器参数，每行一项"));
            panel = panel.child(
                html::span(
                    crate::i18n::tr(
                        "参数中必须恰好包含一个 {file}；程序和参数不会通过 shell 解析。",
                    )
                    .into_owned(),
                )
                .class("empty-note"),
            );
        }
        settings_layout = settings_layout.child(panel);
        let mut panel =
            settings_section("遗留编辑文件").class("settings-section settings-section-wide");
        panel = panel.child(
            html::span(
                crate::i18n::tr("领取文件只用于管理本地文件，不恢复数据库编辑权限。").into_owned(),
            )
            .class("empty-note"),
        );
        let busy = app.editing_files.pending.is_some();
        if busy {
            panel = panel
                .child(html::p(crate::i18n::tr("正在处理编辑文件…").into_owned()).class("muted"));
        }
        if let Some(error) = &app.editing_files.error {
            panel = panel.child(
                html::p(error.clone())
                    .class("form-submit-error")
                    .attr("role", "alert"),
            );
        } else if app.editing_files.loaded && app.editing_files.items.is_empty() && !busy {
            panel = panel
                .child(html::p(crate::i18n::tr("暂无编辑文件").into_owned()).class("empty-note"));
        }
        let mut files = html::div().class("mcp-list");
        for item in &app.editing_files.items {
            let Some(id) = item["id"].as_str() else {
                continue;
            };
            let selected = app.editing_files.selected.as_deref() == Some(id);
            let purpose = crate::i18n::tr(match item["purpose"].as_str() {
                Some("external") => "外部编辑文件",
                Some("draft") => "草稿文件",
                _ => "未知用途",
            });
            let state = crate::i18n::tr(match item["state"].as_str() {
                Some("preparing") => "准备中",
                Some("ready") => "就绪",
                Some("opened") => "已打开",
                Some("returned") => "已回填",
                Some("orphaned") => "已遗留",
                Some("cleanup_pending") => "待清理",
                _ => "未知状态",
            });
            let active = crate::i18n::tr(if app.editing_files.has_claim(item) {
                "已领取"
            } else {
                match item["active"].as_bool() {
                    Some(true) => "使用中",
                    Some(false) => "未使用",
                    None => "未知状态",
                }
            });
            let referenced = crate::i18n::tr(match item["referenced"].as_bool() {
                Some(true) => "草稿引用中",
                Some(false) => "无草稿引用",
                None => "未知状态",
            });
            let format = crate::i18n::tr(match item["format"].as_str() {
                Some("txt") => "文本",
                Some("json") => "JSON",
                Some("xml") => "XML",
                Some("hex") => "Hex",
                Some("bin") => "二进制",
                Some("intent") => "编辑意图",
                _ => "未知",
            });
            let workspace = item["workspace_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(std::borrow::Cow::Borrowed)
                .unwrap_or_else(|| crate::i18n::tr("未记录"));
            files = files.child(
                html::button("")
                    .class(if selected {
                        "mcp-row active"
                    } else {
                        "mcp-row"
                    })
                    .attr("aria-pressed", selected)
                    .attr("disabled", busy)
                    .on_click(Msg::Action(format!("editing-files-select:{id}")))
                    .child(
                        html::div()
                            .class("mcp-row-content")
                            .child(html::strong(format!("{purpose} · {format}")))
                            .child(
                                html::div()
                                    .class("mcp-row-title")
                                    .child(html::span(state.into_owned()).class("mcp-state"))
                                    .child(html::span(active.into_owned()).class("mcp-state"))
                                    .child(html::span(referenced.into_owned()).class("mcp-state")),
                            )
                            .child(
                                html::span(crate::i18n::format_message(
                                    "工作区：{workspace}",
                                    &[("workspace", &workspace)],
                                ))
                                .class("mcp-subtitle"),
                            )
                            .child(
                                html::span(format!(
                                    "{}: {}",
                                    crate::i18n::tr("创建时间（毫秒）"),
                                    item["created_at_ms"]
                                        .as_i64()
                                        .map(|v| v.to_string())
                                        .unwrap_or_else(|| crate::i18n::tr("未记录").into_owned())
                                ))
                                .class("mcp-subtitle"),
                            ),
                    ),
            );
        }
        panel = panel.child(files);
        let mut file_actions = html::div().class("settings-file-actions").child(
            ui::button(crate::i18n::tr("刷新文件列表").into_owned())
                .disabled(busy)
                .on_click(Msg::Action("editing-files-refresh".into())),
        );
        if app.editing_files.offset > 0 {
            file_actions = file_actions.child(
                ui::button(crate::i18n::tr("上一页").into_owned())
                    .disabled(busy)
                    .on_click(Msg::Action("editing-files-prev".into())),
            );
        }
        if app.editing_files.next.is_some() {
            file_actions = file_actions.child(
                ui::button(crate::i18n::tr("下一页").into_owned())
                    .disabled(busy)
                    .on_click(Msg::Action("editing-files-next".into())),
            );
        }
        panel = panel.child(file_actions);
        if let Some(item) = app
            .editing_files
            .items
            .iter()
            .find(|item| item["id"].as_str() == app.editing_files.selected.as_deref())
        {
            let claimed = app.editing_files.has_claim(item);
            let can_claim = crate::editing_files::FileManagement::can_claim(item);
            let can_export = claimed
                && item["state"] != "preparing"
                && !app.field("editing_file_path").trim().is_empty();
            let can_cleanup = claimed && item["referenced"] == false;
            for (label, key) in [
                ("登记相对路径", "relative_dir"),
                ("工作区", "workspace_id"),
                ("文件状态", "state"),
                ("领取版本", "revision"),
                ("创建时间（毫秒）", "created_at_ms"),
                ("更新时间（毫秒）", "updated_at_ms"),
            ] {
                let value = item
                    .get(key)
                    .filter(|value| !value.is_null())
                    .map(|value| {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string())
                    })
                    .unwrap_or_else(|| crate::i18n::tr("未记录").into_owned());
                panel = panel.child(
                    ui::field(crate::i18n::tr(label).into_owned())
                        .variant(ui::FieldVariant::Row)
                        .child(html::span(value)),
                );
            }
            panel = panel.child(
                html::p(crate::i18n::tr("文件大小未记录；登记路径为相对路径。").into_owned())
                    .class("muted"),
            );
            if let Some(target) = item["target"].as_object() {
                for (key, label) in [
                    ("database", "数据库"),
                    ("table", "表"),
                    ("column", "列"),
                    ("key", "Key"),
                ] {
                    if let Some(value) = target.get(key).and_then(Value::as_str) {
                        panel = panel.child(
                            ui::field(crate::i18n::tr(label).into_owned())
                                .variant(ui::FieldVariant::Row)
                                .child(html::span(value.to_owned())),
                        );
                    }
                }
            }
            panel = panel.child(
                ui::tree().item(
                    ui::TreeItem::new(
                        "editing-file-diagnostic",
                        crate::i18n::tr("完整诊断").into_owned(),
                    )
                    .open(false)
                    .child(ui::TreeItem::new(
                        "editing-file-diagnostic-value",
                        diagnostic_text(item),
                    )),
                ),
            );
            panel = panel.child(
                html::p(
                    crate::i18n::tr("领取文件并填写路径后可导出；使用中或草稿引用的文件不能领取。")
                        .into_owned(),
                )
                .class("muted"),
            );
            panel = panel.child(
                html::div()
                    .class("settings-file-actions")
                    .child(
                        ui::button(crate::i18n::tr("领取文件").into_owned())
                            .disabled(busy || !can_claim)
                            .on_click(Msg::Action("editing-files-claim".into())),
                    )
                    .child(
                        ui::button(crate::i18n::tr("导出文件").into_owned())
                            .disabled(busy || !can_export)
                            .on_click(Msg::Action("editing-files-export".into())),
                    ),
            );
            panel = panel.child(controls::path_field(
                app,
                "editing_file_path",
                "导出路径",
                &[("save", "", "浏览…")],
            ));
            if app.editing_files.confirm_cleanup {
                if let Some(frozen) = &app.editing_files.cleanup_target {
                    let path = frozen["relative_dir"].as_str().unwrap_or("");
                    panel = panel.child(html::p(crate::i18n::format_message("确认永久清理文件：{path}。文件大小未记录；当前无草稿引用。此操作不可恢复。", &[("path", path)])));
                }
                panel = panel.child(
                    html::div()
                        .class("settings-file-actions")
                        .child(
                            ui::button(crate::i18n::tr("取消").into_owned())
                                .disabled(busy)
                                .on_click(Msg::Action("editing-files-cleanup-cancel".into())),
                        )
                        .child(
                            ui::button(crate::i18n::tr("确认清理").into_owned())
                                .variant(ui::ButtonVariant::Danger)
                                .disabled(busy || !can_cleanup)
                                .on_click(Msg::Action("editing-files-cleanup-confirm".into())),
                        ),
                );
            } else {
                panel = panel.child(
                    ui::button(crate::i18n::tr("清理文件").into_owned())
                        .disabled(busy || !can_cleanup)
                        .on_click(Msg::Action("editing-files-cleanup".into())),
                );
            }
        }
        settings_layout = settings_layout.child(panel);
    }
    if settings_ready && (current_tab == "general" || mcp) {
        for (section, title) in [
            ("paging", "分页"),
            ("timeouts", "超时设置"),
            ("limits", "资源限制"),
            ("history", "历史"),
            ("mcp", "MCP"),
            ("notifications", "通知"),
        ] {
            let has_fields = application::SETTINGS_FIELDS
                .iter()
                .chain(application::SETTINGS_SWITCHES)
                .any(|(group, key, _)| {
                    settings_display_group(group, key) == section
                        && application::settings_is_mcp(group, key) == mcp
                });
            if !has_fields {
                continue;
            }
            let mut panel = settings_section(title);
            for &(group, key, label) in application::SETTINGS_SWITCHES {
                if settings_display_group(group, key) != section
                    || application::settings_is_mcp(group, key) != mcp
                {
                    continue;
                }
                let field_name = format!("settings-{group}-{key}");
                let action = format!("settings-toggle:{group}:{key}");
                panel = panel.child(
                    ui::switch(crate::i18n::tr(label).into_owned())
                        .on(app.field(&field_name) == "true")
                        .on_change(move |_| Msg::Action(action.clone())),
                );
            }
            for &(group, key, label) in application::SETTINGS_FIELDS {
                if settings_display_group(group, key) != section
                    || application::settings_is_mcp(group, key) != mcp
                {
                    continue;
                }
                let note = if group == "history" {
                    "留空表示不限制；填写时为正整数。"
                } else if key == "response_bytes" {
                    "范围：1024–1048576 字节。"
                } else if key == "value_preview_bytes" {
                    "单位：字节；为正数且不超过响应字节上限。"
                } else if key == "result_memory_bytes_per_client" {
                    "单位：字节；不小于响应字节上限。"
                } else if (group == "paging" && key != "redis_key_batch")
                    || key == "mcp_response_rows"
                {
                    "范围：1–1000 条。"
                } else if key.ends_with("seconds") {
                    "单位：秒；必须大于零。"
                } else {
                    "必须大于零。"
                };
                panel = panel.child(controls::field_with_hint(
                    app,
                    &format!("settings-{group}-{key}"),
                    label,
                    note,
                ));
            }
            settings_layout = settings_layout.child(panel);
        }
    }
    if settings_ready && mcp {
        let mut policies = ui::select()
            .aria(crate::i18n::tr("新连接默认审批策略").into_owned())
            .value(app.field("settings-approval-default_policy"))
            .open(app.workbench.generic_select.as_deref() == Some("settings-default-policy"))
            .on_toggle(|collapsed| {
                Msg::Action(format!(
                    "select-open:settings-default-policy:{}",
                    !collapsed
                ))
            })
            .on_change(|change| {
                Msg::Action(format!(
                    "settings-policy:{}",
                    change.value.as_str().unwrap_or_default()
                ))
            });
        for (value, label) in [
            ("dangerous", "危险操作审批"),
            ("all_writes", "所有写入审批"),
        ] {
            policies = policies.option(ui::SelectOption::new(
                value,
                crate::i18n::tr(label).into_owned(),
            ));
        }
        settings_layout = settings_layout.child(
            settings_section("审批")
                .child(field(
                    app,
                    "settings-approval-ttl_seconds",
                    "审批有效期（秒）",
                ))
                .child(
                    ui::field(crate::i18n::tr("新连接默认审批策略").into_owned())
                        .variant(ui::FieldVariant::Row)
                        .child(policies),
                ),
        );
    }
    if current_tab != "archery" && !app.settings_error.is_empty() {
        panel = panel.child(
            html::p(app.settings_error.render().into_owned())
                .class("form-submit-error")
                .attr("role", "alert"),
        );
    }
    if current_tab != "archery"
        && current_tab != "updates"
        && let Some(error) = &app.native_ui.policy_error
    {
        panel = panel.child(
            html::p(error.clone())
                .class("form-submit-error")
                .attr("role", "alert"),
        );
    }
    if settings_ready && mcp {
        let mut rules = html::div()
            .class("settings-section settings-section-wide")
            .child(
                html::strong(crate::i18n::tr("MCP 连接权限").into_owned())
                    .class("settings-section-title"),
            );
        let busy = !app.field("mcp-rule-pending").is_empty();
        let selected_profile = Uuid::parse_str(app.field("mcp-rule-profile")).ok();
        let mut selector = ui::select()
            .aria(crate::i18n::tr("连接").into_owned())
            .option(
                ui::SelectOption::new("", crate::i18n::tr("请选择连接").into_owned())
                    .disabled(true),
            )
            .key("settings-mcp-profile")
            .value(app.field("mcp-rule-profile").to_owned())
            .disabled(app.profiles.is_empty())
            .open(app.workbench.generic_select.as_deref() == Some("settings-mcp-profile"))
            .on_toggle(|collapsed| {
                Msg::Action(format!("select-open:settings-mcp-profile:{}", !collapsed))
            })
            .on_change(|change| {
                Msg::Action(format!(
                    "mcp-rules-load:{}",
                    change.value.as_str().unwrap_or_default()
                ))
            });
        for profile in &app.profiles {
            let provider = match profile.provider {
                datapier_core::model::ProviderKind::Mysql => "MySQL",
                datapier_core::model::ProviderKind::Redis => "Redis",
            };
            let source = if profile.is_archery() {
                "Archery".to_owned()
            } else {
                crate::i18n::tr("直连").into_owned()
            };
            selector = selector.option(ui::SelectOption::new(
                profile.id.to_string(),
                format!("{} · {} · {}", profile.name, provider, source),
            ));
        }
        rules = rules.child(
            ui::field(crate::i18n::tr("连接").into_owned())
                .variant(ui::FieldVariant::Row)
                .child(selector),
        );
        rules = rules.child(page_button(
            "repeat",
            "重新加载连接权限和策略",
            "mcp-rules-reload",
            !busy,
        ));
        if app.field("mcp-rule-reload-confirm") == "true" {
            rules = rules
                .child(
                    html::p(
                        crate::i18n::tr("重新加载将丢弃此连接未保存的权限和审批策略，是否继续？")
                            .into_owned(),
                    )
                    .class("form-submit-error"),
                )
                .child(button("丢弃并重新加载", "mcp-rules-reload-confirm"))
                .child(button("取消", "mcp-rules-reload-cancel"));
        }
        if busy {
            rules =
                rules.child(html::p(crate::i18n::tr("正在处理权限…").into_owned()).class("muted"));
        }
        if !app.field("mcp-rule-error").is_empty() {
            rules = rules.child(
                html::p(app.field("mcp-rule-error").to_owned())
                    .class("form-submit-error")
                    .attr("role", "alert"),
            );
            if !app.field("mcp-rule-retry").is_empty() {
                rules = rules.child(button("重试", app.field("mcp-rule-retry")));
            }
        }
        if !app.field("mcp-rule-data").is_empty() {
            let mut profile_policies = ui::select()
                .aria(crate::i18n::tr("连接审批策略").into_owned())
                .value(app.field("mcp-rule-policy"))
                .disabled(busy)
                .open(app.workbench.generic_select.as_deref() == Some("mcp-profile-policy"))
                .on_toggle(|collapsed| {
                    Msg::Action(format!("select-open:mcp-profile-policy:{}", !collapsed))
                })
                .on_change(|change| {
                    Msg::Action(format!(
                        "mcp-policy-set:{}",
                        change.value.as_str().unwrap_or_default()
                    ))
                });
            for (policy, label) in [
                ("off", "关闭审批"),
                ("dangerous", "危险操作审批"),
                ("all_writes", "所有写入审批"),
            ] {
                profile_policies = profile_policies.option(ui::SelectOption::new(
                    policy,
                    crate::i18n::tr(label).into_owned(),
                ));
            }
            rules = rules.child(
                ui::field(crate::i18n::tr("连接审批策略").into_owned())
                    .variant(ui::FieldVariant::Row)
                    .child(profile_policies),
            );
            rules = rules
                .child(
                    html::p(
                        crate::i18n::tr(if app.mcp_policy_dirty() {
                            "连接审批策略有未保存修改"
                        } else {
                            "连接审批策略已保存"
                        })
                        .into_owned(),
                    )
                    .class("muted"),
                )
                .child(page_button(
                    "check",
                    "保存连接审批策略",
                    "mcp-policy-save",
                    !busy && app.mcp_policy_dirty(),
                ));
            rules = rules.child(
                html::p(
                    crate::i18n::tr(if busy {
                        "连接权限处理中…"
                    } else if app.field("mcp-rule-data") != app.field("mcp-rule-baseline") {
                        "连接权限有未保存修改"
                    } else {
                        "连接权限已保存"
                    })
                    .into_owned(),
                )
                .class("muted"),
            );
            if app.field("settings-mcp-fine_grained_access") == "true" {
                if let Ok(data) = serde_json::from_str::<Value>(app.field("mcp-rule-data"))
                    && let Some(items) = data["rules"].as_array()
                {
                    for item in items {
                        let key = item["scope_key"].as_str().unwrap_or("");
                        let scope = if item["scope_kind"] == "instance" {
                            if key == "*" {
                                crate::i18n::tr("Redis 整个实例").into_owned()
                            } else {
                                format!("{} ({key})", crate::i18n::tr("Redis 实例范围"))
                            }
                        } else {
                            key.to_owned()
                        };
                        let level = match item["level"].as_str().unwrap_or("") {
                            "disabled" => crate::i18n::tr("禁用").into_owned(),
                            "read_only" => crate::i18n::tr("只读").into_owned(),
                            "read_write" => crate::i18n::tr("读写").into_owned(),
                            _ => crate::i18n::tr("未知").into_owned(),
                        };
                        rules = rules.child(
                            ui::field(scope)
                                .variant(ui::FieldVariant::Row)
                                .child(html::span(level)),
                        );
                    }
                }
                if app
                    .profiles
                    .iter()
                    .find(|profile| Some(profile.id) == selected_profile)
                    .is_some_and(|profile| {
                        profile.provider == datapier_core::model::ProviderKind::Mysql
                    })
                {
                    rules = rules.child(field(app, "mcp-rule-scope", "MySQL 数据库名称"));
                }
                let scope = app.field("mcp-rule-scope").trim();
                let selected_level = serde_json::from_str::<Value>(app.field("mcp-rule-data"))
                    .ok()
                    .and_then(|data| {
                        data["rules"].as_array().and_then(|items| {
                            items
                                .iter()
                                .find(|item| {
                                    item["scope_key"].as_str()
                                        == Some(if scope.is_empty() { "*" } else { scope })
                                })
                                .and_then(|item| item["level"].as_str())
                                .map(str::to_owned)
                        })
                    })
                    .unwrap_or_default();
                let mut levels = ui::select()
                    .aria(crate::i18n::tr("访问权限").into_owned())
                    .value(selected_level)
                    .disabled(busy)
                    .open(app.workbench.generic_select.as_deref() == Some("mcp-rule-level"))
                    .on_toggle(|collapsed| {
                        Msg::Action(format!("select-open:mcp-rule-level:{}", !collapsed))
                    })
                    .on_change(|change| {
                        Msg::Action(format!(
                            "mcp-rules-level:{}",
                            change.value.as_str().unwrap_or_default()
                        ))
                    })
                    .option(
                        ui::SelectOption::new("", crate::i18n::tr("请选择").into_owned())
                            .disabled(true),
                    );
                for (level, label) in [
                    ("disabled", "禁用"),
                    ("read_only", "只读"),
                    ("read_write", "读写"),
                ] {
                    levels = levels.option(ui::SelectOption::new(
                        level,
                        crate::i18n::tr(label).into_owned(),
                    ));
                }
                rules = rules.child(
                    ui::field(crate::i18n::tr("访问权限").into_owned())
                        .variant(ui::FieldVariant::Row)
                        .child(levels),
                );
                rules = rules.child(
                    ui::button(crate::i18n::tr("保存 MCP 权限").into_owned())
                        .variant(ui::ButtonVariant::Primary)
                        .disabled(
                            busy || app.field("mcp-rule-data") == app.field("mcp-rule-baseline"),
                        )
                        .on_click(Msg::Action("mcp-rules-save".into())),
                );
            }
        }
        settings_layout = settings_layout.child(rules);
    }
    if current_tab == "archery" {
        let mut servers = html::div()
            .class("settings-section settings-section-wide archery-server-list")
            .child(
                html::div()
                    .class("archery-section-heading")
                    .child(html::strong(crate::i18n::tr("Archery 服务器").into_owned()))
                    .child(icon_button("plus", "新建服务", "server-new")),
            );
        for server in &app.archery.servers {
            let Some(id) = server["id"].as_str() else {
                continue;
            };
            let count = uuid::Uuid::parse_str(id)
                .ok()
                .map(|id| {
                    app.profiles
                        .iter()
                        .filter(|profile| {
                            profile
                                .archery()
                                .is_some_and(|source| source.server_id == id)
                        })
                        .count()
                })
                .unwrap_or(0);
            servers = servers.child(
                html::div()
                    .class("archery-server-row")
                    .child(
                        html::div()
                            .class("archery-server-info")
                            .child(
                                ui::field(crate::i18n::tr("服务名称").into_owned())
                                    .variant(ui::FieldVariant::Row)
                                    .child(html::span(
                                        server["name"].as_str().unwrap_or("").to_owned(),
                                    )),
                            )
                            .child(
                                ui::field(crate::i18n::tr("服务地址").into_owned())
                                    .variant(ui::FieldVariant::Row)
                                    .child(html::span(
                                        server["base_url"].as_str().unwrap_or("").to_owned(),
                                    )),
                            )
                            .child(
                                ui::field(crate::i18n::tr("用户名").into_owned())
                                    .variant(ui::FieldVariant::Row)
                                    .child(html::span(
                                        server["username"].as_str().unwrap_or("").to_owned(),
                                    )),
                            )
                            .child(
                                ui::field(crate::i18n::tr("关联连接").into_owned())
                                    .variant(ui::FieldVariant::Row)
                                    .child(html::span(count.to_string())),
                            ),
                    )
                    .child(icon_button(
                        "edit",
                        "编辑服务",
                        &format!("server-edit:{id}"),
                    ))
                    .child(icon_button(
                        "trash",
                        "删除服务",
                        &format!("server-delete:{id}"),
                    )),
            );
        }
        settings_layout = settings_layout.child(servers);
    }
    panel = panel.child(settings_layout);
    footer = html::div().class("modal-footer");
    if current_tab != "archery" {
        footer = footer.child(
            ui::button(crate::i18n::tr("重新加载设置").into_owned())
                .disabled(
                    app.settings_save_token.is_some()
                        || app.native_ui.request.is_some()
                        || app.field("settings-reload") == "true",
                )
                .on_click(Msg::Action("settings-reload".into())),
        );
    }
    footer = footer.child(html::span("").class("spacer"));
    if current_tab == "archery" {
        footer = footer.child(button("关闭", "overlay-close"));
    }
    if current_tab != "archery" {
        footer = footer.child(
            ui::button(crate::i18n::tr("保存全局设置").into_owned())
                .variant(ui::ButtonVariant::Primary)
                .disabled(
                    !settings_ready
                        || app.settings_save_token.is_some()
                        || app.native_ui.settings_busy()
                        || (app.native_ui.settings_dirty() && app.native_ui.request.is_some()),
                )
                .on_click(Msg::Action("settings-save".into())),
        );
    }
    (panel, footer)
}
