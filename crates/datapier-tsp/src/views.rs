use super::*;
mod cell;
use cell::cell_drawer;
mod context;
mod icons;
use icons::{archery_icon, connection_provider_icon, provider_icon};
mod schema;
use schema::schema_panel;
mod grid_view;
mod pager;
use grid_view::grid;
mod sidebar;
use sidebar::resource_panel;
mod redis_browser;
use redis_browser::redis_browser;
mod dialogs;
pub(super) use dialogs::overlay;
mod approval;
mod archery;
mod controls;
mod profile_form;
use controls::{button, choices, field, flat_action, icon_button, page_button};
mod redis_detail;
use redis_detail::{cli_panel, string_value};
mod history;
use history::{history_footer, history_panel};

fn redis_db_picker(app: &App, form: bool) -> Node<Msg> {
    let current = if form {
        app.field("redis_db").parse::<u32>().unwrap_or(0)
    } else {
        app.current_redis_db()
    };
    let open = if form {
        app.navigation.form_redis_db_menu
    } else {
        app.navigation.redis_db_menu
    };
    let prefix = if form { "form-redis-db" } else { "redis-db" };
    let mut select = ui::select()
        .value(current.to_string())
        .open(open)
        .disabled(form && app.connection_form_busy())
        .on_toggle(move |collapsed| Msg::Action(format!("{prefix}-menu-open:{}", !collapsed)))
        .on_change(move |change| {
            Msg::Action(format!(
                "{prefix}:{}",
                change.value.as_str().unwrap_or_default()
            ))
        });
    let discovery = if form {
        Some(&app.navigation.form_redis_discovery)
    } else {
        app.profile
            .as_ref()
            .and_then(|profile| app.navigation.redis_discovery.get(&profile.id))
    };
    let databases = if !form {
        app.profile
            .as_ref()
            .and_then(|profile| profile.archery())
            .map(|source| &source.databases)
            .or_else(|| discovery.map(|state| &state.databases))
    } else {
        discovery.map(|state| &state.databases)
    };
    let mut has_current = false;
    if let Some(databases) = databases {
        for database in databases {
            has_current |= database.parse::<u32>() == Ok(current);
            select = select.option(ui::SelectOption::new(
                database.clone(),
                format!("DB {database}"),
            ));
        }
    }
    if !has_current {
        select = select.option(ui::SelectOption::new(
            current.to_string(),
            format!("DB {current}"),
        ));
    }
    let mut picker = html::div();
    if form {
        let editor = &app.fields["redis_db"];
        picker = picker.child(
            ui::input()
                .variant(ui::InputVariant::Field)
                .aria(crate::i18n::tr("默认 DB").into_owned())
                .readonly(app.connection_form_busy())
                .key("input-redis_db")
                .text(editor.text.clone())
                .cursor(editor.cursor)
                .anchor(editor.anchor)
                .on_focus(Msg::Focus("redis_db".into()))
                .on_undo(Msg::Undo("redis_db".into()))
                .on_edit(|e| {
                    Msg::Edit(
                        "redis_db".into(),
                        e.from,
                        e.to,
                        e.text.clone(),
                        e.cursor,
                        e.len,
                    )
                }),
        );
    }
    picker = picker.child(select);
    if let Some(state) = discovery {
        if state.loading {
            picker = picker
                .child(html::span(crate::i18n::tr("正在探测数据库").into_owned()).class("muted"));
        } else if let Some(error) = &state.error {
            picker = picker.child(html::span(error.clone()).class("form-submit-error"));
        }
    }
    picker.into()
}

fn redis_bytes_text(encoded: &str) -> std::borrow::Cow<'_, str> {
    use base64::Engine as _;
    let text = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok());
    std::borrow::Cow::Owned(text.unwrap_or_else(|| format!("base64:{encoded}")))
}

// CLI 的 JSON 是 provider 编码的 Redis reply；只解码展示，不改变缓存或写回类型。
fn redis_reply_text(reply: &serde_json::Value) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    if let Some(kind) = reply.get("type").and_then(serde_json::Value::as_str) {
        match kind {
            "bytes" => {
                if let Some(value) = reply.get("value").and_then(serde_json::Value::as_str) {
                    return redis_bytes_text(value);
                }
            }
            "text" | "integer" | "unsigned" | "decimal" | "float" => {
                if let Some(value) = reply.get("value").and_then(serde_json::Value::as_str) {
                    return Cow::Borrowed(value);
                }
            }
            "missing" => return Cow::Borrowed("(nil)"),
            _ => {}
        }
    }
    if let Some(items) = reply.as_array() {
        if items.is_empty() {
            return Cow::Borrowed("[]");
        }
        use std::fmt::Write as _;
        let mut text = String::new();
        for (index, item) in items.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            write!(&mut text, "{}) {}", index + 1, redis_reply_text(item)).unwrap();
        }
        return Cow::Owned(text);
    }
    Cow::Owned(reply.to_string())
}

fn value_text<'a>(
    value: &'a datapier_core::model::Value,
    redis: bool,
) -> std::borrow::Cow<'a, str> {
    use datapier_core::model::Value as V;
    use std::borrow::Cow;
    match value {
        V::Bytes(encoded) if redis => redis_bytes_text(encoded),
        V::Json(reply) if redis => redis_reply_text(reply),
        V::Integer(s)
        | V::Unsigned(s)
        | V::Decimal(s)
        | V::Float(s)
        | V::Text(s)
        | V::Date(s)
        | V::Time(s)
        | V::DateTime(s) => Cow::Borrowed(s),
        V::Large { preview, .. } => Cow::Owned(format!("{preview}…")),
        _ => Cow::Owned(value.text()),
    }
}
fn cell_preview(app: &App, value: &datapier_core::model::Value) -> String {
    let text = value_text(value, app.is_redis());
    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(256).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    preview
}

fn shortcut_help() -> Node<Msg> {
    let primary = if cfg!(target_os = "macos") {
        "Command"
    } else {
        "Ctrl"
    };
    let groups: &[(&str, &[(&str, &str)])] = &[
        (
            "全局",
            &[("Ctrl+Q", "保存工作区并退出"), ("Escape", "关闭弹窗或下拉")],
        ),
        (
            "SQL 查询",
            &[
                ("{mod}+Enter", "执行选区或当前语句"),
                ("{mod}+Shift+Enter", "执行全部 SQL"),
            ],
        ),
        (
            "文本编辑",
            &[
                ("{mod}+A", "全选"),
                ("{mod}+C / X", "复制 / 剪切"),
                ("{mod}+Z", "撤销"),
                ("Shift+方向键 / Home / End", "扩展文本选区"),
            ],
        ),
        (
            "数据 grid",
            &[
                ("Enter / F2", "编辑单元格"),
                ("方向键 / Home / End", "移动单元格焦点"),
                ("Shift+方向键 / Home / End", "扩展单元格选区"),
                ("Tab / Shift+Tab", "移动到后一个 / 前一个单元格"),
                ("{mod}+Z", "撤销 grid 草稿"),
                ("{mod}+单击行号", "增选或取消选中行"),
                ("Shift+单击行号", "连续选择行"),
            ],
        ),
        (
            "原位编辑",
            &[
                ("Enter", "保留当前编辑为草稿"),
                ("Escape", "取消当前原位输入"),
                ("Tab / Shift+Tab", "保留编辑并移动到相邻单元格"),
            ],
        ),
        ("Redis CLI", &[("Enter", "执行 CLI 命令")]),
    ];
    let mut panel = html::div().class("shortcuts-help");
    for (title, rows) in groups {
        panel = panel.child(html::strong(crate::i18n::tr(title).into_owned()));
        for (keys, label) in *rows {
            panel = panel.child(
                html::div()
                    .class("shortcut-row")
                    .child(
                        html::span(crate::i18n::tr(keys).replace("{mod}", primary))
                            .class("shortcut-keys"),
                    )
                    .child(html::span(crate::i18n::tr(label).into_owned()).class("shortcut-label")),
            );
        }
    }
    panel.into()
}

fn row_search(app: &App) -> Node<Msg> {
    let ed = &app.fields["row_search"];
    html::div()
        .class("row-search")
        .child(
            ui::input()
                .variant(ui::InputVariant::Field)
                .key("input-row_search")
                .text(ed.text.clone())
                .cursor(ed.cursor)
                .anchor(ed.anchor)
                .placeholder(crate::i18n::tr("筛选本页…").into_owned())
                .prop("aria-label", crate::i18n::tr("筛选本页…").into_owned())
                .on_focus(Msg::Focus("row_search".into()))
                .on_undo(Msg::Undo("row_search".into()))
                .on_edit(|e| {
                    Msg::Edit(
                        "row_search".into(),
                        e.from,
                        e.to,
                        e.text.clone(),
                        e.cursor,
                        e.len,
                    )
                }),
        )
        .into()
}

pub fn stylesheet(app: &App) -> String {
    let mut css = concat!(
        include_str!("styles/shell-navigation.css"),
        include_str!("styles/workspace-grid.css"),
        include_str!("styles/redis-cli.css"),
        include_str!("styles/details-forms.css"),
        include_str!("styles/navigation-editor.css"),
        include_str!("styles/native-surfaces.css"),
        include_str!("styles/archery-forms.css"),
        include_str!("styles/advanced-editing.css"),
    )
    .to_string();
    if let Some(g) = &app.grid {
        for (i, s) in g.settings.iter().enumerate() {
            let sticky = g
                .frozen_offset(i)
                .map(|x| format!("position:sticky; left:{}px; z-index:2;", x + 38))
                .unwrap_or_default();
            css.push_str(&format!(".col{i} {{ width:{}px; {sticky} }}", s.width));
        }
    }
    css
}
fn writable(app: &App) -> bool {
    app.profile
        .as_ref()
        .is_some_and(|p| p.access_mode == datapier_core::model::AccessMode::ReadWrite)
        && app.edit_token.is_some()
        && app.browse_result
        && (app.is_redis() || app.grid.as_ref().is_some_and(|g| g.editable))
}

fn timestamp_text(value: Option<&Value>) -> String {
    let seconds = value.and_then(|value| {
        value
            .as_f64()
            .or_else(|| value.as_str()?.parse::<f64>().ok())
    });
    seconds
        .filter(|seconds| seconds.is_finite())
        .and_then(|seconds| {
            let whole = seconds.floor();
            if whole < i64::MIN as f64 || whole > i64::MAX as f64 {
                return None;
            }
            chrono::DateTime::<chrono::Utc>::from_timestamp(
                whole as i64,
                ((seconds - whole) * 1_000_000_000.0).floor() as u32,
            )
        })
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| crate::i18n::tr("未记录").into_owned())
}

pub(super) fn task_state_text(state: &str) -> String {
    crate::i18n::tr(match state {
        "queued" => "排队中",
        "running" => "执行中",
        "cancelling" => "取消中",
        "succeeded" => "已完成",
        "failed" => "失败",
        "cancelled" => "已取消",
        "wait_stopped" => "已结束等待",
        "outcome_unknown" => "结果待确认",
        _ => "未知",
    })
    .into_owned()
}

fn ttl_label(ttl: Option<i64>) -> String {
    match ttl {
        Some(-1) => crate::i18n::tr("永久").into_owned(),
        Some(-2) => crate::i18n::tr("不存在").into_owned(),
        Some(ms) if ms >= 0 => {
            crate::i18n::format_message("{count} 秒", &[("count", &(ms / 1000).to_string())])
        }
        _ => crate::i18n::tr("未读取").into_owned(),
    }
}

fn ttl_short(ttl: Option<i64>) -> String {
    match ttl {
        Some(-1) => "∞".into(),
        Some(-2) => crate::i18n::tr("不存在").into_owned(),
        Some(ms) if ms >= 86_400_000 => format!("≈{}d", ms / 86_400_000),
        Some(ms) if ms >= 3_600_000 => format!("≈{}h", ms / 3_600_000),
        Some(ms) if ms >= 60_000 => format!("≈{}m", ms / 60_000),
        Some(ms) if ms >= 0 => format!("≈{}s", ms / 1000),
        _ => crate::i18n::tr("未知").into_owned(),
    }
}

pub(super) fn render_window(app: &App, nonce: Option<Uuid>) -> View<Msg> {
    let redis = app.is_redis();
    let mut content = html::div()
        .class("content")
        .key("content")
        .child(context::header(app));
    if let Some(error) = &app.workbench.connection_error {
        let mut feedback = html::div()
            .class("connection-feedback")
            .child(html::span(error.clone()).class("connection-message"));
        if app.workbench.connection_tls_required {
            feedback = feedback.child(button("启用 TLS", "connection-enable-tls"));
        }
        content = content.child(
            feedback
                .child(button("重试连接", "open"))
                .child(button("连接设置", "edit-profile")),
        );
    }
    content = content.child(context::tabs(app));
    if app
        .tabs
        .get(app.active)
        .is_some_and(|tab| tab.replace_draft.is_some())
    {
        content = content.child(button("恢复替换草稿", "replace-restore"));
    }
    if !app.tabs.is_empty()
        && app.session_id().is_some()
        && !app.is_redis()
        && app.database.is_some()
    {
        let mut tools = html::div()
            .class("toolbar")
            .child(button("新建表", "schema-create-table"))
            .child(button("新建视图", "schema-create-view"));
        if app
            .target
            .as_ref()
            .is_some_and(|target| target.kind == "table")
        {
            tools = tools.child(button("触发器", "trigger-list"));
            tools = tools.child(button("新建触发器", "schema-create-trigger"));
        }
        content = content.child(tools);
    }
    if let Some(list) = &app.workbench.trigger_list {
        let mut triggers = html::div().class("toolbar").child(html::strong(format!(
            "{} · {}",
            crate::i18n::tr("触发器"),
            list.target.name
        )));
        if let Some(error) = &list.error {
            triggers = triggers.child(html::span(error.clone()).class("field-error"));
        }
        if list.token.is_some() {
            triggers = triggers
                .child(html::span(crate::i18n::tr("正在加载触发器…").into_owned()).class("muted"));
        }
        for (index, resource) in list.resources.iter().enumerate() {
            triggers = triggers.child(button(&resource.name, &format!("trigger-open:{index}")));
        }
        if list.cursor.is_some() {
            triggers = triggers.child(button("下一页", "trigger-list-next"));
        }
        if list.token.is_none() && list.resources.is_empty() && list.error.is_none() {
            triggers = triggers.child(html::span(crate::i18n::tr("没有触发器").into_owned()));
        }
        content = content.child(triggers);
    }
    if app.save_blocked {
        content = content.child(
            html::div()
                .class("draftbar")
                .child(html::span(crate::i18n::tr("工作区保存冲突").into_owned()))
                .child(button("另存工作区副本", "workspace-copy")),
        );
    }
    if app.tabs.is_empty() {
        let mut empty = html::div()
            .class("empty-state")
            .child(html::strong(crate::i18n::tr("没有打开的标签").into_owned()))
            .child(html::span(
                crate::i18n::tr("从侧栏选择连接，或新建查询。").into_owned(),
            ))
            .child(button("新建查询", "view-query"));
        if app.navigation_compact() {
            empty = empty.child(button("选择连接", "navigation-toggle"));
        }
        content = content.child(empty);
    } else {
        let mut body = html::div().class(if redis {
            "detail-panel key-details"
        } else {
            "body detail-panel"
        });
        if app.view == "advanced-schema" {
            if let Some(schema) = &app.advanced_schema {
                body = body.child(schema.view());
            }
        } else if !redis && (app.target.is_none() || (app.view == "query" && !app.browse_result)) {
            body = body
                .child(crate::query_ui::toolbar(app))
                .child(crate::query_ui::panel(app));
        } else if redis && app.target.is_none() {
            let mut empty = html::div()
                .class("empty-state")
                .child(html::strong(crate::i18n::tr("选择一个 key").into_owned()))
                .child(html::span(
                    crate::i18n::tr("从左侧已加载的 key 中选择，查看类型和值。").into_owned(),
                ));
            empty = empty.child(
                ui::button(crate::i18n::tr("导入数据库").into_owned())
                    .disabled(!writable(app))
                    .on_click(Msg::Action("overlay:import".into())),
            );
            if !writable(app) {
                empty = empty.child(
                    html::span(crate::i18n::tr("只读连接不能修改数据").into_owned()).class("muted"),
                );
            }
            body = body.child(empty);
        } else if redis {
            let (title, tools) = redis_detail::resource_controls(app);
            body = body.child(title).child(tools);
        } else {
            let nav = ui::tabs()
                .active(app.workbench.section.clone())
                .tab("data", crate::i18n::tr("数据").into_owned())
                .tab("structure", crate::i18n::tr("结构").into_owned())
                .tab("ddl", "DDL")
                .on_select(|value| Msg::Action(format!("section:{value}")));
            let resource = app
                .target
                .as_ref()
                .map(
                    |target| match target.database.as_deref().or(app.database.as_deref()) {
                        Some(database) => format!("{database}.{}", target.name),
                        None => target.name.clone(),
                    },
                )
                .unwrap_or_default();
            content = content.child(
                html::div()
                    .class("resource-nav")
                    .child(nav)
                    .child(html::span("").class("spacer"))
                    .child(
                        html::span(resource.clone())
                            .class("resource-identity")
                            .prop("title", resource),
                    ),
            );
        }
        if app.view == "advanced-schema" {
        } else if !redis
            && let Some(target) = &app.target
            && (app.browse_result
                || (app.workbench.section == "structure"
                    && app
                        .advanced_schema
                        .as_ref()
                        .is_some_and(|state| state.embedded)))
            && app.workbench.section != "data"
        {
            if app.workbench.section == "ddl" {
                let ddl = app
                    .schema
                    .as_ref()
                    .and_then(|s| s.get("ddl"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let mut toolbar = html::div()
                    .class("toolbar")
                    .child(html::span(format!("DDL · {}", target.name)).class("muted"))
                    .child(html::span("").class("spacer"));
                if !ddl.is_empty() {
                    toolbar = toolbar.child(
                        ui::button("")
                            .variant(ui::ButtonVariant::Quiet)
                            .size(ui::ButtonSize::Sm)
                            .icon("copy")
                            .title(crate::i18n::tr("复制 DDL").into_owned())
                            .aria(crate::i18n::tr("复制 DDL").into_owned())
                            .on_click(Msg::CopyText(ddl.to_owned())),
                    );
                }
                body = body.child(toolbar).child(
                    html::div()
                        .class("ddl-code")
                        .child(ui::code(ddl).lang("sql").numbers(true)),
                );
            } else {
                if target.kind == "table" {
                    if let Some(schema) = &app.advanced_schema {
                        body = body.child(schema.view());
                    } else {
                        body = body.child(schema_panel(app));
                    }
                } else {
                    body = body.child(schema_panel(app));
                }
            }
        } else if (redis && app.target.is_none())
            || (!redis && !app.browse_result && app.workbench.query_ui.results_hidden)
        {
        } else {
            if !redis
                && let Some(target) = &app.target
                && app.browse_result
            {
                let mut tools = html::div().class("toolbar").child(
                    html::span(
                        crate::i18n::tr(match target.kind.as_str() {
                            "table" => "基表",
                            "view" => "视图",
                            _ => "未知类型",
                        })
                        .into_owned(),
                    )
                    .class("tag"),
                );
                if let Some(columns) = app
                    .schema
                    .as_ref()
                    .and_then(|s| s.get("columns"))
                    .and_then(Value::as_array)
                {
                    let primary = columns
                        .iter()
                        .filter(|column| {
                            column.get("primary_key").and_then(Value::as_bool) == Some(true)
                        })
                        .filter_map(|column| column.get("name").and_then(Value::as_str))
                        .collect::<Vec<_>>();
                    if !primary.is_empty() {
                        tools = tools.child(
                            html::span(crate::i18n::format_message(
                                "主键 {columns}",
                                &[("columns", &primary.join(", "))],
                            ))
                            .class("tag"),
                        );
                    }
                }
                if target.kind == "view" {
                    tools = tools.child(
                        html::span(
                            crate::i18n::tr("视图编辑的并发检查覆盖视图可见的完整原值。")
                                .into_owned(),
                        )
                        .class("muted"),
                    );
                    if let Some(reasons) = app
                        .schema
                        .as_ref()
                        .and_then(|schema| schema.pointer("/details/view_mapping/reasons"))
                        .and_then(Value::as_array)
                    {
                        for reason in reasons.iter().filter_map(Value::as_str) {
                            tools = tools.child(html::span(reason.to_owned()).class("muted"));
                        }
                    }
                }
                tools = tools
                    .child(row_search(app))
                    .child(icon_button("funnel", "过滤与排序", "overlay:filters"))
                    .child(icon_button("columns", "列配置", "overlay:columns"))
                    .child(html::span("").class("spacer"));
                let can_replace = writable(app) && app.workbench_can_operate("update");
                let can_insert = writable(app) && app.workbench_can_operate("insert");
                tools = tools.child(
                    ui::button(crate::i18n::tr("批量替换").into_owned())
                        .disabled(!can_replace)
                        .on_click(Msg::Action("replace-open".into())),
                );
                tools = tools.child(page_button("plus", "新增行", "overlay:row", can_insert));
                tools = tools.child(icon_button("download", "导出", "overlay:export"));
                if target.kind == "table" {
                    tools = tools.child(page_button(
                        "arrow-up",
                        "导入",
                        "overlay:import",
                        writable(app),
                    ));
                }
                if !can_replace || !can_insert {
                    tools = tools.child(
                        html::span(crate::i18n::tr("只读或当前对象权限不允许修改").into_owned())
                            .class("muted"),
                    );
                }
                tools = tools.child(icon_button("repeat", "刷新", "browse"));
                content = content.child(tools);
            } else if !redis && app.workbench.query_ui.executed {
                let mut result_tabs = html::div()
                    .class("result-tabs")
                    .child(html::span(crate::i18n::tr("结果").into_owned()).class("active"));
                if app.results.len() > 1 {
                    for (i, id) in app.results.iter().enumerate() {
                        result_tabs = result_tabs.child(
                            ui::button(crate::i18n::format_message(
                                "结果 {number}",
                                &[("number", &(i + 1).to_string())],
                            ))
                            .variant(ui::ButtonVariant::Quiet)
                            .key(format!("result{id}"))
                            .on_click(Msg::ResultSet(id.clone())),
                        );
                    }
                }
                let mut results = html::div()
                    .class("sql-results-head")
                    .child(result_tabs)
                    .child(row_search(app))
                    .child(icon_button("columns", "列配置", "overlay:columns"))
                    .child(icon_button("download", "导出", "overlay:export"))
                    .child(icon_button("x", "关闭结果", "query-result-close"));
                if let Some(info) = app
                    .grid
                    .as_ref()
                    .and_then(|grid| grid_view::result_info(app, &grid.page.result))
                {
                    results = results.child(info);
                }
                body = body.child(results);
                if app.grid.is_none() {
                    body = body.child(
                        html::div()
                            .class("query-result-status")
                            .child(html::span(app.status.render().into_owned())),
                    );
                }
            }
            if redis && app.target.as_ref().is_some_and(|t| t.kind == "string") {
                body = body.child(string_value(app));
            } else if let Some(table) = grid(app) {
                body = body.child(table);
            }
            if let Some(grid) = &app.grid {
                body = body.child(grid_view::footer(app, grid));
            }
            if redis {
                let hint = match app.target.as_ref().map(|target| target.kind.as_str()) {
                    Some("string") => "文本、JSON、HEX 是同一内容的展示方式，切换不改写字节。",
                    Some("hash") => "Field 唯一，新增与删除进入变更草稿。",
                    Some("list") => "索引从 0 开始；提交时校验编辑基线。",
                    Some("set") => "Member 唯一且无序，列表顺序仅供浏览。",
                    Some("zset") => "Member 唯一，Score 可重复。",
                    _ => "",
                };
                if !hint.is_empty() {
                    body = body.child(
                        html::div()
                            .class("value-hint")
                            .child(html::span(crate::i18n::tr(hint).into_owned())),
                    );
                }
            }
        }
        if app.redis_key_delete_opening() {
            body = body.child(
                html::div()
                    .class("draftbar")
                    .child(html::span(
                        crate::i18n::tr("正在建立 Redis 删除基线…").into_owned(),
                    ))
                    .child(button("取消", "redis-key-delete-cancel")),
            );
        }
        let changes = app.pending_rows.len()
            + app.redis_changes.len()
            + app.grid.as_ref().map_or(0, |g| g.edits().len())
            + app.pending_key_delete_count();
        if changes > 0 {
            let mut bar = html::div()
                .class("draftbar")
                .child(html::span(crate::i18n::format_message(
                    "{count} 项待提交变更",
                    &[("count", &changes.to_string())],
                )))
                .child(button("预览", "overlay:preview"));
            if redis {
                bar = bar.child(button("撤销", "redis-undo"));
                if writable(app) {
                    bar = bar.child(button("提交", "redis-submit"));
                }
            } else {
                bar = bar.child(button("撤销", "undo-grid"));
                if writable(app) {
                    bar = bar.child(button("提交", "submit"));
                }
            }
            body = body.child(bar.child(button("丢弃", "discard")));
        }
        if app.paste_preview.is_some() && writable(app) {
            body = body.child(button("查看粘贴预览", "overlay:preview"));
        }
        if !app.tabs[app.active].pending.is_null() && changes == 0 {
            body = body.child(
                html::div()
                    .class("draftbar")
                    .child(html::span(crate::i18n::tr("已保存的编辑意图").into_owned()))
                    .child(button("恢复编辑意图", "restore-pending"))
                    .child(button("丢弃", "discard")),
            );
        }
        if redis
            && app
                .profile
                .as_ref()
                .is_some_and(|profile| profile.is_archery())
        {
            content = content.child(cli_panel(app));
        } else if redis && app.workbench.cli {
            let mut split = ui::split()
                .key("redis-cli-split")
                .dir(ui::SplitDir::Col)
                .h(ui::Extent::Fraction(1.0))
                .sizes(vec![1.0 - app.cli_share, app.cli_share])
                // 两层上下文和固定输入不能被比例偏好压出 pane；窄屏还需为换行留空间。
                .min(ui::PaneMin::Each(vec![
                    Some(ui::Extent::Lines(8.0)),
                    Some(ui::Extent::Lines(if app.viewport_width <= 760 {
                        16.0
                    } else {
                        12.0
                    })),
                ]))
                .on_change(|change| {
                    Msg::CliResize(
                        change
                            .value
                            .get(1)
                            .and_then(Value::as_f64)
                            .unwrap_or(f64::NAN),
                    )
                })
                .child(
                    html::div()
                        .class("body redis-layout")
                        .child(redis_browser(app))
                        .child(body),
                )
                .child(cli_panel(app));
            if app.focus == "cli_height" {
                split = split.active(0u32);
            }
            content = content.child(html::div().class("redis-cli-layout").child(split));
        } else {
            content = if redis {
                content.child(
                    html::div()
                        .class("body redis-layout")
                        .child(redis_browser(app))
                        .child(body),
                )
            } else if app.workbench.overlay.as_deref() == Some("cell") {
                content.child(
                    html::div()
                        .class("split")
                        .child(body)
                        .child(cell_drawer(app)),
                )
            } else {
                content.child(body)
            };
        }
    }
    let mut status = html::div()
        .class("status")
        .child(html::span(app.status.render().into_owned()));
    if let Some(pending) = &app.workbench.draft_scope_pending {
        status = status
            .child(button("查看未确认输入", "draft-scope-recovery-view"))
            .child(button("放弃未确认输入", "draft-scope-pending-cancel"));
        if pending.request.is_none() {
            status = status.child(button("重试草稿保护", "draft-scope-pending-retry"));
        }
    }
    if !app.workbench.draft_scope_recovery.is_empty() {
        status = status
            .child(button("查看未确认输入", "draft-scope-recovery-view"))
            .child(button("清除未确认输入", "draft-scope-recovery-discard"));
    }
    if app.navigation_drop_running() {
        status = status.child(button("停止等待", "cancel"));
    }
    status = status.child(html::span("").class("spacer"));
    status = status.child(approval::control(app));
    if !app.detail.is_empty() {
        status =
            status.child(flat_action("info", "详情", "overlay:diagnostics").class("status-action"));
    }
    if redis && !app.tabs.is_empty() && !app.profile.as_ref().is_some_and(|p| p.is_archery()) {
        status = status.child(
            flat_action("terminal", "CLI", "cli-toggle")
                .class(if app.workbench.cli {
                    "status-action active"
                } else {
                    "status-action"
                })
                .attr("aria-expanded", app.workbench.cli),
        );
    }
    content = content.child(status);
    let layout = html::div()
        .class(format!(
            "layout {}{}",
            app.viewport_class(),
            if app.viewport_width > 0 && app.viewport_width <= 760 && !app.navigation_compact() {
                " navigation-overlay"
            } else {
                ""
            }
        ))
        .key("layout")
        .attr(
            "data-window-bridge",
            nonce
                .map(|id| format!("datapier-window-bridge:{}", json!({"nonce":id,"data_dir":app.window_data_dir,"editor_handoff":app.advanced_files.terminal_handoff})))
                .unwrap_or_default(),
        )
        .on_click(Msg::Action("result-info-dismiss".into()))
        .child(resource_panel(app))
        .child(content);
    let layer = if app.approvals.open {
        vec![approval::panel(app)]
    } else {
        app.workbench
            .overlay
            .as_deref()
            .filter(|kind| redis || *kind != "cell")
            .map(|kind| vec![overlay(app, kind)])
            .unwrap_or_default()
    };
    View::new().main(vec![Node::from(layout)]).layer(layer)
}

#[cfg(test)]
#[path = "../tests/unit/views.rs"]
mod tests;
