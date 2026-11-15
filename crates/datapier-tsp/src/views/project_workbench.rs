use super::*;
fn input(app: &App, name: &str, label: &str) -> Node<Msg> {
    let field = field(app, &app.project_key(name), label);
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
            body = body
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
                body = body
                    .child(line("当前摘要", &item["current_digest"]))
                    .child(html::span(format!(
                        "{}: {}",
                        crate::i18n::tr("文件状态"),
                        crate::i18n::tr(status)
                    )));
            }
            if !item["old_bytes"].is_null() {
                body = body.child(line("旧大小（字节）", &item["old_bytes"]));
            }
            if !item["new_bytes"].is_null() {
                body = body.child(line("新大小（字节）", &item["new_bytes"]));
            }
        }
    }
    body.into()
}
pub(super) fn panel(app: &App) -> Node<Msg> {
    let section = if app.workbench.project.section.is_empty() {
        "overview"
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
        root = root.child(
            html::div()
                .class("project-error")
                .child(html::span("请在保存分区核对外部版本并确认基线。")),
        );
    }
    let mut tabs = ui::tabs().key("project-tabs").active(section.to_owned());
    for (id, title) in [
        ("overview", "概览"),
        ("bindings", "本机绑定"),
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
            .child(input(app, "path", "项目目录"))
            .child(input(app, "name", "新项目名称"))
            .child(button("打开项目", "project-ui-open"))
            .child(button("新建项目", "project-ui-create"))
            .child(input(app, "archive", "项目归档路径"))
            .child(input(app, "destination", "导入目标目录"))
            .child(button("导入项目归档", "project-ui-import"));
        if app.workbench.project.restored.is_some() {
            body = body.child(html::span(
                "已恢复本地草稿；请重新打开原项目目录，不会自动连接或执行。",
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
                        "{} · {}",
                        crate::i18n::tr("遗留资源"),
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
                body = body
                    .child(html::span(
                        crate::i18n::tr(
                            "请先导出所需恢复副本并完成手工合并；清理后这些副本不可恢复。",
                        )
                        .into_owned(),
                    ))
                    .child(button(
                        "确认清理恢复副本",
                        "project-ui-artifact-cleanup-confirm",
                    ))
                    .child(button("取消", "project-ui-artifact-cleanup-cancel"));
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
            body = body
                .child(input(app, "archive", "项目归档路径"))
                .child(button("导出项目归档", "project-ui-export"));
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
                        body = body.child(html::span(format!(
                            "外部新增或修改 / Externally added or changed: {section} · {name}{}",
                            if decided { " ✓" } else { "" }
                        )));
                        if !decided {
                            body = body.child(button(
                                "合并外部条目和正文",
                                &format!("project-ui-change-decision:{section}:{index}:merge"),
                            ));
                            body = body.child(button(
                                "保留本地条目和正文",
                                &format!("project-ui-change-decision:{section}:{index}:keep"),
                            ));
                        }
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
                        body = body.child(html::span(format!(
                            "外部已删除 / Externally deleted: {section} · {name}{}",
                            if decided { " ✓" } else { "" }
                        )));
                        if !decided {
                            body = body.child(button(
                                "合并删除",
                                &format!("project-ui-delete-decision:{section}:{index}:delete"),
                            ));
                            body = body.child(button(
                                "保留本地条目",
                                &format!("project-ui-delete-decision:{section}:{index}:keep"),
                            ));
                        }
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
            body = body
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
                .child(button("添加逻辑连接", "project-ui-connection-add"));
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
                        .unwrap_or_else(|| if binding.is_some() { "?" } else { "—" });
                    body = body.child(
                        flat_action(
                            "database",
                            &format!("{alias} · {local}"),
                            &format!("project-ui-alias:{i}"),
                        )
                        .class("workbench-entry"),
                    );
                    if let Some(binding) = binding {
                        body = body
                            .child(line("本机数据库", &binding["overrides"]["database"]))
                            .child(line("本机 Redis DB", &binding["overrides"]["redis_db"]));
                    }
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
            body = body
                .child(input(app, "alias", "连接别名"))
                .child(choices(
                    app,
                    &app.project_key("profile"),
                    "本机连接",
                    &options,
                ))
                .child(input(app, "database", "本机数据库"))
                .child(input(app, "redis-db", "本机 Redis DB"))
                .child(input(app, "path-parameter", "路径参数名"))
                .child(input(app, "local-path", "本机路径覆盖"));
        }
        if section == "queries" {
            body = body.child(html::strong(crate::i18n::tr("项目查询").into_owned()));
            if let Some(queries) = project["manifest"]["queries"].as_array() {
                for (i, query) in queries.iter().enumerate() {
                    body = body.child(
                        flat_action(
                            "file",
                            &format!("{} · {}", text(&query["name"]), text(&query["path"])),
                            &format!("project-ui-query:{i}"),
                        )
                        .class("workbench-entry"),
                    );
                }
            }
        }
        if section == "transfers" {
            body = body.child(html::strong(crate::i18n::tr("项目迁移预设").into_owned()));
            body = body.child(button("新增共享迁移配置", "project-ui-transfer-new"));
            if app.workbench.project.file_next.is_some() {
                body = body
                    .child(html::span("正在分块读取项目文件。"))
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
        if section == "queries" {
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
                .child(html::strong(
                    crate::i18n::tr("将保存的查询正文").into_owned(),
                ))
                .child(
                    html::pre(app.field(&app.project_key("query-text")).to_owned())
                        .class("preview"),
                )
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
                            || (section == "bindings" && app.workbench.project.bindings.is_none())
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
