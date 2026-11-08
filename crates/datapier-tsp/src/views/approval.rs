use super::*;
use crate::approval::ApprovalTab;

fn text(key: &str) -> String {
    crate::i18n::tr(key).into_owned()
}

fn status(value: &str) -> String {
    match value {
        "pending" | "approval_required" => text("待审批"),
        "approved" => text("已批准"),
        "rejected" => text("已拒绝"),
        "expired" => text("已过期"),
        "invalidated" => text("已失效"),
        "dispatching" => text("正在执行"),
        "disconnected" => text("待认证"),
        "challenge_required" => text("等待验证码"),
        "authenticating" => text("认证中"),
        "auth_failed" => text("认证失败"),
        "ready" => text("已认证"),
        "" => text("未知"),
        "queued" | "running" | "cancelling" | "succeeded" | "failed" | "cancelled"
        | "wait_stopped" | "outcome_unknown" => task_state_text(value),
        _ => text("未知"),
    }
}

fn operation(method: &str) -> String {
    text(match method {
        "resource.browse" => "浏览资源",
        "execution.start" | "execution.run" => "执行命令",
        "archery.sql_check" => "检查上线 SQL",
        "change.submit" => "提交变更",
        "transfer.import" => "导入",
        "transfer.export" => "导出",
        _ => "执行操作",
    })
}

fn connection(app: &App, value: &Value) -> String {
    let context = value
        .get("context")
        .or_else(|| value.pointer("/request/proof/context"))
        .unwrap_or(&Value::Null);
    let name = context["connection"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            value["profile_id"]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok())
                .and_then(|id| app.profiles.iter().find(|profile| profile.id == id))
                .map(|profile| profile.name.as_str())
        })
        .unwrap_or("");
    let database = context["database"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            value
                .pointer("/request/params/database")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
        });
    let name = if name.is_empty() {
        text("未记录")
    } else {
        name.to_owned()
    };
    if let Some(db) = database {
        format!("{name} / {db}")
    } else if let Some(db) = context["redis_db"].as_u64() {
        format!("{name} / DB {db}")
    } else {
        name
    }
}

fn fact(label: &str, value: String) -> html::El<Msg> {
    html::div()
        .class("task-fact")
        .child(html::span(text(label)).class("muted"))
        .child(html::span(value).class("task-fact-value"))
}

fn clean_request(value: &mut Value) {
    match value {
        Value::Object(fields) => {
            fields.retain(|key, _| {
                let key = key.to_ascii_lowercase();
                !key.contains("password")
                    && !key.contains("secret")
                    && !key.contains("token")
                    && key != "_access_proof"
                    && key != "proof"
                    && key != "otp"
            });
            for value in fields.values_mut() {
                clean_request(value);
            }
        }
        Value::Array(items) => {
            for item in items {
                clean_request(item);
            }
        }
        _ => {}
    }
}

pub(super) fn panel(app: &App) -> Node<Msg> {
    let state = &app.approvals;
    let approval_count = state
        .items
        .iter()
        .filter(|item| item["state"].as_str() == Some("pending"))
        .count();
    let mut tabs = html::div().class("mcp-tabs").attr("role", "tablist");
    for (tab, label, action) in [
        (
            ApprovalTab::Approvals,
            format!(
                "{} ({})",
                text("待审批"),
                if state.list_loaded {
                    approval_count.to_string()
                } else {
                    text("未知")
                }
            ),
            "approval-tab-approvals",
        ),
        (
            ApprovalTab::Authentication,
            format!(
                "{} ({})",
                text("待认证"),
                if state.list_loaded {
                    state.authentication.len().to_string()
                } else {
                    text("未知")
                }
            ),
            "approval-tab-authentication",
        ),
        (ApprovalTab::Tasks, text("Agent 任务"), "approval-tab-tasks"),
    ] {
        tabs = tabs.child(
            html::button(label)
                .key(action)
                .class(match (state.tab == tab, app.focus == action) {
                    (true, true) => "mcp-tab active focused",
                    (true, false) => "mcp-tab active",
                    (false, true) => "mcp-tab focused",
                    (false, false) => "mcp-tab",
                })
                .attr("role", "tab")
                .attr("aria-selected", state.tab == tab)
                .on_click(Msg::Action(action.into())),
        );
    }
    let mut body = html::div().class("modal-body mcp-body").child(tabs);
    if (state.tab == ApprovalTab::Tasks
        && state.observe_list_loaded
        && state.observe_list_pending.is_some())
        || (state.tab != ApprovalTab::Tasks && state.list_loaded && state.list_pending.is_some())
    {
        body = body.child(html::p(text("正在刷新列表")).class("muted"));
    }
    let footer = html::div()
        .class("modal-footer")
        .child(html::span("").class("spacer"))
        .child(button("关闭", "approval-close"));
    match state.tab {
        ApprovalTab::Approvals => {
            if let Some(error) = &state.error {
                body = body.child(html::p(text(error)).class("form-submit-error"));
            }
            if let Some(error) = &state.list_error {
                body = body
                    .child(html::p(text(error)).class("form-submit-error"))
                    .child(button("重试列表", "approval-refresh"));
            }
            if state.items.is_empty() && state.list_error.is_none() {
                if state.list_pending.is_some() || !state.list_loaded {
                    body = body.child(html::p(text("正在加载审批")).class("empty-note"));
                } else {
                    body = body.child(html::p(text("没有待处理的审批")).class("empty-note"));
                }
            }
            let mut list = html::div().class("mcp-list");
            for item in &state.items {
                let Some(id) = item["approval_id"].as_str() else {
                    continue;
                };
                let task = state
                    .observed_task
                    .as_ref()
                    .filter(|task| task["id"] == item["execution_id"]);
                let detail = state
                    .detail
                    .as_ref()
                    .filter(|detail| detail["approval_id"].as_str() == Some(id));
                let method = detail
                    .and_then(|value| value.pointer("/request/method").and_then(Value::as_str))
                    .or_else(|| task.and_then(|task| task["method"].as_str()))
                    .unwrap_or("");
                let title = if method.is_empty() {
                    item["profile_id"]
                        .as_str()
                        .and_then(|id| Uuid::parse_str(id).ok())
                        .and_then(|id| app.profiles.iter().find(|profile| profile.id == id))
                        .map(|profile| {
                            text(match profile.provider {
                                datapier_core::model::ProviderKind::Mysql => "MySQL 命令",
                                datapier_core::model::ProviderKind::Redis => "Redis 命令",
                            })
                        })
                        .unwrap_or_else(|| text("审批请求"))
                } else {
                    operation(method)
                };
                let subtitle = task
                    .map(|task| connection(app, task))
                    .unwrap_or_else(|| connection(app, detail.unwrap_or(item)));
                let selected = state.selected.as_deref() == Some(id);
                let can_decide = selected
                    && state.detail_pending.is_none()
                    && state.detail_error.is_none()
                    && state.decide_pending.is_none()
                    && detail.is_some_and(|record| {
                        record["state"].as_str() == Some("pending")
                            && record["can_decide"].as_bool() == Some(true)
                    });
                list = list.child(
                    html::div()
                        .class(if selected {
                            "mcp-approval-row active"
                        } else {
                            "mcp-approval-row"
                        })
                        .child(
                            html::button("")
                                .class("mcp-row")
                                .attr("aria-pressed", selected)
                                .on_click(Msg::Action(format!("approval-select:{id}")))
                                .child(
                                    html::div()
                                        .class("mcp-row-content")
                                        .child(
                                            html::div()
                                                .class("mcp-row-title")
                                                .child(html::strong(title))
                                                .child(
                                                    html::span(status(
                                                        item["state"].as_str().unwrap_or(""),
                                                    ))
                                                    .class("mcp-state"),
                                                ),
                                        )
                                        .child(html::span(subtitle).class("mcp-subtitle")),
                                ),
                        )
                        .child(
                            html::div()
                                .class("mcp-row-actions")
                                .child(page_button(
                                    "x",
                                    "拒绝",
                                    &format!("approval-decide:{id}:reject"),
                                    can_decide,
                                ))
                                .child(page_button(
                                    "check",
                                    "批准",
                                    &format!("approval-decide:{id}:approve"),
                                    can_decide,
                                )),
                        ),
                );
            }
            body = body.child(list);
            if let Some(error) = &state.detail_error {
                body = body
                    .child(html::p(text(error)).class("form-submit-error"))
                    .child(button("重新读取详情", "approval-detail-retry"));
            }
            if state.detail_pending.is_some() {
                body = body.child(html::p(text("正在加载审批详情")).class("empty-note"));
            } else if let Some(record) = &state.detail {
                let request = &record["request"];
                let method = request["method"].as_str().unwrap_or("");
                let state_text = status(record["state"].as_str().unwrap_or(""));
                body = body.child(
                    html::div()
                        .class("mcp-detail")
                        .child(
                            html::div()
                                .class("task-summary")
                                .child(html::strong(operation(method)))
                                .child(html::span("").class("spacer"))
                                .child(html::span(state_text.clone())),
                        )
                        .child(
                            html::div()
                                .class("task-facts")
                                .child(fact("连接 / 数据库", connection(app, record)))
                                .child(fact("状态", state_text))
                                .child(fact("到期时间", timestamp_text(record.get("expires_at")))),
                        ),
                );
                if record["state"].as_str() == Some("pending")
                    && record["can_decide"].as_bool() != Some(true)
                {
                    body = body.child(html::p(text("仅观察：请在控制窗口处理审批")).class("muted"));
                }
                if let Some(command) = request.pointer("/params/text").and_then(Value::as_str) {
                    body = body.child(
                        html::div()
                            .class("task-section")
                            .child(html::strong(text("执行语句")))
                            .child(html::pre(command.to_owned()).class("task-statement mcp-code")),
                    );
                }
                if let Some(parameters) = request
                    .pointer("/params/parameters")
                    .and_then(Value::as_array)
                    .filter(|p| !p.is_empty())
                {
                    let mut rows = html::div().class("mcp-parameters");
                    for (index, parameter) in parameters.iter().enumerate() {
                        let value = match parameter {
                            Value::String(value) => value.clone(),
                            _ => parameter.to_string(),
                        };
                        rows = rows.child(
                            html::div()
                                .class("mcp-parameter")
                                .child(html::span((index + 1).to_string()))
                                .child(html::pre(value).class("mcp-parameter-value")),
                        );
                    }
                    body = body.child(
                        html::div()
                            .class("task-section")
                            .child(html::strong(text("绑定参数")))
                            .child(rows),
                    );
                }
                body = body.child(
                    html::div()
                        .class("mcp-tools")
                        .child(
                            html::button(text("完整诊断"))
                                .class("mcp-diagnostics-toggle")
                                .attr("aria-expanded", state.diagnostics_open)
                                .on_click(Msg::Action("approval-diagnostics-toggle".into())),
                        )
                        .child(button("查看执行", "approval-observe")),
                );
                if state.diagnostics_open {
                    let mut diagnostic = record.clone();
                    clean_request(&mut diagnostic);
                    body = body.child(
                        html::pre(serde_json::to_string_pretty(&diagnostic).unwrap_or_default())
                            .class("task-statement mcp-code"),
                    );
                }
            } else if state.selected.is_none() && !state.items.is_empty() {
                body = body.child(html::p(text("选择一项查看详情")).class("empty-note"));
            }
        }
        ApprovalTab::Authentication => {
            if let Some(error) = &state.error {
                body = body.child(html::p(text(error)).class("form-submit-error"));
            }
            if let Some(error) = &state.list_error {
                body = body
                    .child(html::p(text(error)).class("form-submit-error"))
                    .child(button("重试列表", "approval-refresh"));
            }
            if state.authentication.is_empty() && state.list_error.is_none() {
                if state.list_pending.is_some() || !state.list_loaded {
                    body = body.child(html::p(text("加载中…")).class("empty-note"));
                } else {
                    body = body.child(html::p(text("没有待认证的连接")).class("empty-note"));
                }
            }
            let mut list = html::div().class("mcp-list");
            for item in &state.authentication {
                let Some(id) = item["request_id"].as_str() else {
                    continue;
                };
                let allowed = item["can_authenticate"].as_bool() == Some(true);
                let name = item["connection_name"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| text("未记录"));
                list = list.child(
                    html::div()
                        .class("mcp-auth-row")
                        .child(
                            html::div()
                                .class("mcp-row-content")
                                .child(html::strong(name))
                                .child(
                                    html::span(status(item["state"].as_str().unwrap_or("")))
                                        .class("mcp-subtitle"),
                                )
                                .child(
                                    html::span(text(if allowed {
                                        "当前控制窗口"
                                    } else {
                                        "仅观察：请在控制窗口认证"
                                    }))
                                    .class("mcp-subtitle"),
                                ),
                        )
                        .child(
                            ui::button(text("认证"))
                                .disabled(!allowed)
                                .on_click(Msg::Action(format!("approval-authenticate:{id}"))),
                        ),
                );
            }
            body = body.child(list);
        }
        ApprovalTab::Tasks => {
            body = body.child(
                html::div()
                    .class("mcp-tools")
                    .child(button("刷新任务", "approval-refresh")),
            );
            if let Some(error) = &state.observe_error {
                body = body.child(html::p(text(error)).class("form-submit-error"));
                if state.observed_task_id.is_some() {
                    body = body.child(button("重新读取详情", "approval-observe-refresh"));
                }
            }
            if let Some(error) = &state.observe_list_error {
                body = body.child(html::p(text(error)).class("form-submit-error"));
            }
            if state.observed_tasks.is_empty() && state.observe_list_error.is_none() {
                if state.observe_list_pending.is_some() || !state.observe_list_loaded {
                    body = body.child(html::p(text("加载中…")).class("empty-note"));
                } else {
                    body = body.child(html::p(text("没有可观察的任务")).class("empty-note"));
                }
            }
            let mut list = html::div().class("mcp-list");
            for task in &state.observed_tasks {
                let Some(id) = task["id"].as_str() else {
                    continue;
                };
                let details = state
                    .observed_task
                    .as_ref()
                    .filter(|value| value["id"].as_str() == Some(id))
                    .unwrap_or(task);
                list = list.child(
                    html::button("")
                        .class(if state.observed_task_id.as_deref() == Some(id) {
                            "mcp-row active"
                        } else {
                            "mcp-row"
                        })
                        .attr(
                            "aria-pressed",
                            state.observed_task_id.as_deref() == Some(id),
                        )
                        .on_click(Msg::Action(format!("approval-observe-task:{id}")))
                        .child(
                            html::div()
                                .class("mcp-row-content")
                                .child(
                                    html::div()
                                        .class("mcp-row-title")
                                        .child(html::strong(
                                            if details["method"].as_str().is_some() {
                                                operation(details["method"].as_str().unwrap_or(""))
                                            } else {
                                                text("Agent 任务")
                                            },
                                        ))
                                        .child(
                                            html::span(status(
                                                task["state"].as_str().unwrap_or(""),
                                            ))
                                            .class("mcp-state"),
                                        ),
                                )
                                .child(
                                    html::span(if details["context"].is_object() {
                                        connection(app, details)
                                    } else {
                                        timestamp_text(task.get("created_at"))
                                    })
                                    .class("mcp-subtitle"),
                                ),
                        ),
                );
            }
            body = body.child(list);
            if state.observe_task_pending.is_some() {
                body = body.child(html::p(text("加载中…")).class("empty-note"));
            }
            if let Some(task) = &state.observed_task {
                body = body.child(
                    html::div()
                        .class("mcp-detail")
                        .child(
                            html::div()
                                .class("task-summary")
                                .child(html::strong(operation(
                                    task["method"].as_str().unwrap_or(""),
                                )))
                                .child(html::span("").class("spacer"))
                                .child(html::span(status(task["state"].as_str().unwrap_or("")))),
                        )
                        .child(
                            html::div()
                                .class("task-facts")
                                .child(fact("连接 / 数据库", connection(app, task)))
                                .child(fact("创建时间", timestamp_text(task.get("created_at")))),
                        )
                        .child(button("刷新任务", "approval-observe-refresh")),
                );
                if let Some(error) = task.get("error").filter(|v| !v.is_null()) {
                    body = body.child(
                        html::p(
                            error
                                .get("message")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .unwrap_or_else(|| error.to_string()),
                        )
                        .class("form-submit-error"),
                    );
                }
                if let Some(results) = task["results"].as_array() {
                    let mut actions = html::div().class("mcp-result-actions");
                    for (index, result) in results.iter().enumerate() {
                        if let Some(id) = result.as_str() {
                            actions = actions.child(
                                ui::button(crate::i18n::format_message(
                                    "结果 {number}",
                                    &[("number", &(index + 1).to_string())],
                                ))
                                .on_click(Msg::Action(format!("approval-observe-result:{id}"))),
                            );
                        }
                    }
                    body = body.child(actions);
                }
            } else if state.observed_task_id.is_none() && !state.observed_tasks.is_empty() {
                body = body.child(html::p(text("选择一项查看详情")).class("empty-note"));
            }
            if let Some(task) = &state.observed_task {
                if let Some(result) = task["result_summaries"].as_array().and_then(|items| {
                    items
                        .iter()
                        .find(|item| item["id"].as_str() == state.observed_result.as_deref())
                }) {
                    let message = if result["kind"].as_str() == Some("columns_unknown") {
                        "结果定义未知"
                    } else if result["complete"].as_bool() == Some(true) {
                        "读取完成"
                    } else {
                        "尚未读取完成"
                    };
                    body = body.child(html::p(text(message)).class("mcp-subtitle"));
                } else if task["results"]
                    .as_array()
                    .is_some_and(|items| items.is_empty())
                    && (task["summary"]["result"].is_object()
                        || task["summary"]["results"]
                            .as_array()
                            .is_some_and(|items| !items.is_empty()))
                {
                    body =
                        body.child(html::p(text("结果元数据未记录或已释放")).class("mcp-subtitle"));
                }
            }
            if let Some(page) = &state.observed_page {
                // 观察者只读已保留数据，不请求 Agent 的下一批或重放其查询。
                body = body.child(html::p(text("仅显示当前已返回的数据")).class("mcp-subtitle"));
                let mut grid = html::div().class("gridscroll mcp-grid");
                let mut header = html::div().class("gridrow header");
                for column in &page.result.columns {
                    header = header.child(html::span(column.name.clone()).class("cli-cell"));
                }
                grid = grid.child(header);
                for row in &page.rows {
                    let mut line = html::div().class("gridrow");
                    for value in row {
                        line = line.child(
                            html::span(value_text(value, false).into_owned()).class("cli-cell"),
                        );
                    }
                    grid = grid.child(line);
                }
                body = body.child(grid).child(super::pager::render(
                    app,
                    super::pager::Pager {
                        key: "approval-observe-page-size",
                        size_action: "approval-observe-size",
                        previous_action: "approval-observe-prev",
                        next_action: "approval-observe-next",
                        size: state
                            .observed_size
                            .unwrap_or(app.settings.paging.sql_rows as u64)
                            .max(1),
                        page: state.observed_history.len() as u64 + 1,
                        total: Some(page.result.row_count),
                        can_previous: !state.observed_history.is_empty(),
                        can_next: page.next.is_some_and(|next| next < page.result.row_count),
                        loading: state.observe_page_pending.is_some(),
                        limited: page.result.summary["resource_limited"]
                            .as_bool()
                            .unwrap_or(false),
                        count_action: None,
                    },
                ));
            }
        }
    }
    let overlay: Node<Msg> = ui::overlay()
        .modal(true)
        .size(ui::OverlaySize::Lg)
        .head(text("MCP 待办"))
        .end(icon_button("x", "关闭对话框", "approval-close"))
        .foot(Node::from(footer))
        .child(body.key("body"))
        .into();
    html::div()
        .class(format!("modal-layer {}", app.viewport_class()))
        .key("mcp-modal")
        .child(overlay)
        .into()
}

pub(super) fn control(app: &App) -> Node<Msg> {
    let count = app
        .approvals
        .items
        .iter()
        .filter(|item| item["state"].as_str() == Some("pending"))
        .count()
        + app.approvals.authentication.len();
    flat_action("list", "MCP 待办", "approval-toggle")
        .key("approval-open")
        .class(if app.focus == "approval-open" {
            "status-action focused"
        } else {
            "status-action"
        })
        .attr("aria-expanded", app.approvals.open)
        .child(html::span(format!(
            " ({})",
            if app.approvals.list_loaded {
                count.to_string()
            } else {
                text("未知")
            }
        )))
        .into()
}
