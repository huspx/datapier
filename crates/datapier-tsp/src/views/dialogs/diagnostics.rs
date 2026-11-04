use super::*;

// 详情可能包含大批导入样本或任意嵌套数据；摘要限定节点与文本长度，完整原文仍可展开和复制。
fn diagnostic_label(key: &str) -> std::borrow::Cow<'_, str> {
    crate::i18n::tr(match key {
        "target" => "目标资源",
        "database" => "数据库",
        "name" => "名称",
        "kind" => "类型",
        "provider" => "数据库类型",
        "method" => "操作",
        "state" => "状态",
        "id" => "标识",
        "error" => "错误",
        "errors" => "错误",
        "code" => "错误代码",
        "message" => "消息",
        "database_code" => "数据库错误代码",
        "details" => "详细信息",
        "summary" => "结果摘要",
        "affected_rows" => "影响行数",
        "row_count" => "行数",
        "elapsed_ms" => "耗时（毫秒）",
        "duration_ms" => "耗时（毫秒）",
        "length" => "长度",
        "ttl_ms" => "过期时间（毫秒）",
        "native_type" | "type_name" => "原生类型",
        "type" => "类型",
        "nullable" => "允许 NULL",
        "primary_key" => "主键",
        "auto_increment" => "自增",
        "generated" => "生成列",
        "comment" => "注释",
        "columns" => "列",
        "rows" => "行",
        "keys" => "键",
        "ddl" => "DDL",
        "engine" => "引擎",
        "format" => "格式",
        "sample_complete" => "样本完整",
        "operation" => "操作",
        "key" => "键",
        "before" => "原值",
        "values" => "新值",
        "items" => "条目",
        "results" => "结果",
        "status" => "状态",
        "schema" => "结构",
        "mapping" => "映射",
        "complete" => "完整",
        "sent" => "已发送",
        "may_write" => "可能写入",
        "workspace_id" => "工作区",
        "owner" => "所属者",
        "token" => "令牌",
        "result_id" => "结果标识",
        _ => key,
    })
}

pub(super) fn diagnostic_text(value: &Value) -> String {
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    if text.chars().count() > 512 {
        format!("{}…", text.chars().take(512).collect::<String>())
    } else {
        text
    }
}
fn diagnostic_scalar(key: &str, value: &Value) -> String {
    if key == "ttl_ms" {
        match value.as_i64() {
            Some(-1) => return crate::i18n::tr("永久").into_owned(),
            Some(-2) => return crate::i18n::tr("不存在").into_owned(),
            _ => {}
        }
    }
    if let Some(boolean) = value.as_bool() {
        return crate::i18n::tr(if boolean { "是" } else { "否" }).into_owned();
    }
    diagnostic_text(value)
}

pub(super) fn diagnostic_content(
    label: &str,
    value: &Value,
    depth: usize,
    budget: &mut usize,
) -> Node<Msg> {
    let mut section = html::div()
        .class("diagnostic-section")
        .child(html::strong(diagnostic_label(label).into_owned()).class("section-title"));
    if depth >= 6 || *budget == 0 {
        return section.into();
    }
    match value {
        Value::Object(fields) => {
            // 先展示标识、状态等事实，避免大量行或列耗尽展示预算。
            let priority = [
                "id",
                "method",
                "operation",
                "state",
                "status",
                "target",
                "summary",
                "error",
            ];
            let ordered = priority
                .iter()
                .filter_map(|key| fields.get_key_value(*key))
                .chain(fields.iter().filter(|(key, child)| {
                    !priority.contains(&key.as_str()) && !child.is_object() && !child.is_array()
                }))
                .chain(fields.iter().filter(|(key, child)| {
                    !priority.contains(&key.as_str()) && (child.is_object() || child.is_array())
                }));
            for (key, child) in ordered {
                if *budget == 0 {
                    break;
                }
                *budget -= 1;
                if child.is_object() || child.is_array() {
                    section = section.child(diagnostic_content(key, child, depth + 1, budget));
                } else {
                    section = section.child(
                        html::div()
                            .class("diagnostic-row")
                            .child(html::span(diagnostic_label(key).into_owned()).class("muted"))
                            .child(html::span(diagnostic_scalar(key, child))),
                    );
                }
            }
        }
        Value::Array(values) => {
            for (index, child) in values.iter().enumerate().take(20) {
                if *budget == 0 {
                    break;
                }
                *budget -= 1;
                if child.is_object() || child.is_array() {
                    section = section.child(diagnostic_content(
                        &(index + 1).to_string(),
                        child,
                        depth + 1,
                        budget,
                    ));
                } else {
                    section = section.child(
                        html::div()
                            .class("diagnostic-row")
                            .child(html::span((index + 1).to_string()).class("muted"))
                            .child(html::span(diagnostic_scalar("", child))),
                    );
                }
            }
        }
        _ => section = section.child(html::span(diagnostic_scalar(label, value))),
    }
    section.into()
}

fn diagnostic_fact(label: &str, text: String) -> Node<Msg> {
    html::div()
        .class("task-fact")
        .child(html::span(crate::i18n::tr(label).into_owned()).class("muted"))
        .child(html::span(text).class("task-fact-value"))
        .into()
}

fn task_result_type(method: &str, result: &Value) -> String {
    let kind = result.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind == "redis"
        && let Some(kind) = result.get("redis_type").and_then(Value::as_str)
    {
        return crate::i18n::format_message("{type} 数据", &[("type", kind)]);
    }
    crate::i18n::tr(match kind {
        "rows" if method == "resource.browse" => "表数据",
        "rows" => "查询结果",
        "redis" => "Redis 数据",
        "redis_command" => "命令回复",
        "columns_unknown" => "未知结果",
        "" => "未记录",
        _ => kind,
    })
    .into_owned()
}

pub(super) fn diagnostic_task(app: &App, value: &Value) -> Node<Msg> {
    let method = value.get("method").and_then(Value::as_str).unwrap_or("");
    let context = value.get("context").filter(|context| context.is_object());
    let operation = crate::i18n::tr(match method {
        "resource.browse"
            if context
                .and_then(|c| c.get("provider"))
                .and_then(Value::as_str)
                == Some("mysql") =>
        {
            "浏览表数据"
        }
        "resource.browse" => "浏览资源",
        "execution.start" | "execution.run" => "执行查询",
        "change.submit" => "提交变更",
        "transfer.import" => "导入",
        "archery.sql_check" => "检查上线 SQL",
        "transfer.export" => "导出",
        _ => method,
    });
    let state = value.get("state").and_then(Value::as_str).unwrap_or("");
    let missing = crate::i18n::tr("未记录");
    let loading = app.workbench.diagnostic_task_request.is_some();
    let connection = context
        .map(|context| {
            let name = context
                .get("connection")
                .and_then(Value::as_str)
                .unwrap_or("");
            if context.get("provider").and_then(Value::as_str) == Some("redis") {
                context
                    .get("redis_db")
                    .and_then(Value::as_u64)
                    .map_or_else(|| name.to_owned(), |db| format!("{name} / DB {db}"))
            } else {
                context
                    .get("database")
                    .and_then(Value::as_str)
                    .filter(|db| !db.is_empty())
                    .map_or_else(|| name.to_owned(), |db| format!("{name} / {db}"))
            }
        })
        .unwrap_or_else(|| {
            crate::i18n::tr(if loading { "加载中…" } else { "未记录" }).into_owned()
        });
    let resource = context
        .map(|context| {
            let targets = context.get("targets").and_then(Value::as_array);
            let names = targets
                .into_iter()
                .flatten()
                .filter_map(|target| {
                    let name = target.get("name")?.as_str()?;
                    Some(
                        if context.get("provider").and_then(Value::as_str) == Some("mysql") {
                            target
                                .get("database")
                                .and_then(Value::as_str)
                                .filter(|db| !db.is_empty())
                                .map_or_else(|| name.to_owned(), |db| format!("{db}.{name}"))
                        } else {
                            name.to_owned()
                        },
                    )
                })
                .collect::<Vec<_>>();
            if names.is_empty() {
                "—".into()
            } else {
                names.join("\n")
            }
        })
        .unwrap_or_else(|| missing.to_string());
    let results: &[Value] = value
        .get("result_summaries")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let summary = value.get("summary");
    let recorded_results = summary
        .and_then(|s| s.get("results"))
        .and_then(Value::as_array)
        .is_some_and(|r| !r.is_empty())
        || summary.and_then(|s| s.get("result")).is_some();
    let result_text = if results.len() == 1 {
        let result = &results[0];
        if result.get("kind").and_then(Value::as_str) == Some("columns_unknown") {
            crate::i18n::tr("结果定义未知").into_owned()
        } else {
            let rows = result
                .get("row_count")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .to_string();
            match result.get("column_count").and_then(Value::as_u64) {
                Some(columns) => crate::i18n::format_message(
                    if result.get("complete").and_then(Value::as_bool) == Some(true) {
                        "{rows} 行 · {columns} 列"
                    } else {
                        "已读取 {rows} 行 · {columns} 列"
                    },
                    &[("rows", &rows), ("columns", &columns.to_string())],
                ),
                None => crate::i18n::format_message("{rows} 行 · 列数未记录", &[("rows", &rows)]),
            }
        }
    } else if !results.is_empty() {
        crate::i18n::format_message("{count} 个结果集", &[("count", &results.len().to_string())])
    } else if recorded_results {
        crate::i18n::tr("结果元数据未记录或已释放").into_owned()
    } else if let Some(count) = summary
        .and_then(|s| s.get("affected_rows"))
        .and_then(Value::as_u64)
    {
        crate::i18n::format_message("影响 {count} 行", &[("count", &count.to_string())])
    } else if let Some(count) = summary
        .and_then(|s| s.get("affected"))
        .and_then(Value::as_u64)
    {
        crate::i18n::format_message("影响 {count} 项", &[("count", &count.to_string())])
    } else {
        missing.to_string()
    };
    let elapsed = value
        .get("elapsed_ms")
        .and_then(Value::as_u64)
        .map_or_else(|| missing.to_string(), |ms| format!("{ms} ms"));
    let started = if state == "queued" && value.get("started_at").is_none_or(Value::is_null) {
        crate::i18n::tr("未开始").into_owned()
    } else {
        timestamp_text(value.get("started_at"))
    };
    let id = value.get("id").and_then(Value::as_str).unwrap_or("");
    let facts = html::div()
        .class("task-facts")
        .child(diagnostic_fact(
            "来源",
            context
                .and_then(|context| context.get("source"))
                .and_then(Value::as_str)
                .unwrap_or(&missing)
                .to_owned(),
        ))
        .child(diagnostic_fact("连接 / 数据库", connection))
        .child(diagnostic_fact("资源", resource))
        .child(diagnostic_fact("耗时", elapsed))
        .child(diagnostic_fact("结果", result_text))
        .child(diagnostic_fact("开始时间", started))
        .child(
            html::div()
                .class("task-fact")
                .child(html::span(crate::i18n::tr("任务 ID").into_owned()).class("muted"))
                .child(
                    html::span(if id.is_empty() {
                        missing.to_string()
                    } else {
                        id.to_owned()
                    })
                    .class("task-fact-value")
                    .prop("title", id),
                ),
        );
    let mut section = html::div()
        .class("task-details")
        .child(
            html::div()
                .class("task-summary")
                .child(ui::icon("info"))
                .child(html::strong(operation.into_owned()))
                .child(html::span("").class("spacer"))
                .child(
                    html::span(task_state_text(state))
                        .class(format!("task-state task-state-{state}")),
                ),
        )
        .child(facts);
    if let Some(error) = value.get("error").filter(|error| !error.is_null()) {
        section = section.child(
            html::div()
                .class("task-error")
                .child(html::strong(crate::i18n::tr("错误").into_owned()))
                .child(html::p(
                    error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                )),
        );
        if value.get("may_write").and_then(Value::as_bool) == Some(true)
            && value.get("sent").and_then(Value::as_bool) == Some(true)
        {
            section = section.child(
                html::p(
                    crate::i18n::tr("部分操作可能已执行，请核对完整诊断中的执行与回滚状态。")
                        .into_owned(),
                )
                .class("task-warning"),
            );
        }
    }
    if !results.is_empty() {
        let mut rows = html::div().class("task-result-table").child(
            html::div()
                .class("task-result-row task-result-header")
                .child(html::span(crate::i18n::tr("结果").into_owned()))
                .child(html::span(crate::i18n::tr("类型").into_owned()))
                .child(html::span(crate::i18n::tr("行数").into_owned()))
                .child(html::span(crate::i18n::tr("状态").into_owned())),
        );
        for (index, result) in results.iter().enumerate() {
            let unknown = result.get("kind").and_then(Value::as_str) == Some("columns_unknown");
            rows = rows.child(
                html::div()
                    .class("task-result-row")
                    .child(html::span(crate::i18n::format_message(
                        "结果 {number}",
                        &[("number", &(index + 1).to_string())],
                    )))
                    .child(html::span(task_result_type(method, result)))
                    .child(html::span(if unknown {
                        missing.to_string()
                    } else {
                        result
                            .get("row_count")
                            .map_or_else(|| missing.to_string(), Value::to_string)
                    }))
                    .child(html::span(
                        crate::i18n::tr(if unknown {
                            "结果定义未知"
                        } else if result.get("complete").and_then(Value::as_bool) == Some(true) {
                            "读取完成"
                        } else {
                            "尚未读取完成"
                        })
                        .into_owned(),
                    )),
            );
        }
        section = section.child(
            html::div()
                .class("task-section")
                .child(html::strong(crate::i18n::tr("结果集").into_owned()))
                .child(rows),
        );
    }
    let mut execution = html::div()
        .class("task-section")
        .child(html::strong(crate::i18n::tr("执行语句").into_owned()));
    if let Some(text) = value
        .get("text")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        execution = execution.child(html::pre(text.to_owned()).class("task-statement"));
    } else {
        execution = execution.child(
            html::p(crate::i18n::tr(if loading { "加载中…" } else { "未记录" }).into_owned())
                .class("muted"),
        );
    }
    if value
        .get("execution_text_truncated")
        .and_then(Value::as_bool)
        == Some(true)
    {
        execution = execution.child(
            html::p(crate::i18n::tr("执行文本已截断，当前记录只包含前段。").into_owned())
                .class("task-warning"),
        );
    }
    // 逐语句结构和原始字段统一留在完整诊断，避免默认区重复 SQL 与摘要。
    section.child(execution).into()
}
