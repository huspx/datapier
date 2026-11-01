use super::*;
mod grid_view;
use grid_view::grid;
mod sidebar;
use sidebar::resource_panel;
mod redis_browser;
use redis_browser::redis_browser;
mod dialogs;
pub(super) use dialogs::overlay;
mod controls;
mod profile_form;
use controls::{button, choices, field, flat_action, icon_button, page_button};
mod redis_detail;
use redis_detail::{cli_panel, string_value};
mod history;
use history::history_panel;

fn provider_icon(app: &App, redis: bool) -> Node<Msg> {
    if let Some(icons) = &app.icon_blobs {
        html::span("")
            .class("provider-icon")
            .child(
                ui::image()
                    .blob(if redis {
                        icons.redis.clone()
                    } else {
                        icons.mysql.clone()
                    })
                    .alt(if redis { "Redis" } else { "MySQL" })
                    .w(22.0)
                    .h(22.0),
            )
            .into()
    } else {
        html::span("")
            .class("provider-icon")
            .child(ui::icon("database"))
            .into()
    }
}

fn redis_db_picker(app: &App, form: bool) -> Node<Msg> {
    let current = if form {
        app.field("redis_db").parse::<u32>().unwrap_or(0)
    } else {
        app.profile.as_ref().map_or(0, |profile| profile.redis_db)
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
    for db in 0..16 {
        select = select.option(ui::SelectOption::new(db.to_string(), format!("DB {db}")));
    }
    if current >= 16 {
        select = select.option(ui::SelectOption::new(
            current.to_string(),
            format!("DB {current}"),
        ));
    }
    select.into()
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
    let mut css = include_str!("workbench.css").to_string();
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

fn task_state_text(state: &str) -> String {
    crate::i18n::tr(match state {
        "queued" => "排队中",
        "running" => "执行中",
        "cancelling" => "取消中",
        "succeeded" => "已完成",
        "failed" => "失败",
        "cancelled" => "已取消",
        "outcome_unknown" => "结果待确认",
        _ => state,
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

fn schema_panel(app: &App) -> Node<Msg> {
    let mut panel = html::div().class("schema-panel");
    if let Some(schema) = &app.schema {
        panel = panel.child(
            html::div()
                .class("schema-row schema-header schema-columns")
                .child(html::strong(crate::i18n::tr("字段").into_owned()))
                .child(html::span(crate::i18n::tr("类型").into_owned()))
                .child(html::span("NULL"))
                .child(html::span(crate::i18n::tr("约束").into_owned()))
                .child(html::span(crate::i18n::tr("注释").into_owned())),
        );
        if let Some(columns) = schema.get("columns").and_then(Value::as_array) {
            for col in columns {
                let name = col.get("name").and_then(Value::as_str).unwrap_or("");
                let ty = col.get("type_name").and_then(Value::as_str).unwrap_or("");
                let nullable = if col.get("nullable").and_then(Value::as_bool) == Some(false) {
                    "NO"
                } else {
                    "YES"
                };
                let key = if col.get("primary_key").and_then(Value::as_bool) == Some(true) {
                    "主键"
                } else {
                    ""
                };
                let generated = if col.get("generated").and_then(Value::as_bool) == Some(true) {
                    "生成列"
                } else {
                    ""
                };
                let mut constraints = html::span("").class("schema-constraints");
                let mut constrained = false;
                for label in [
                    key,
                    generated,
                    if col.get("auto_increment").and_then(Value::as_bool) == Some(true) {
                        "AUTO_INCREMENT"
                    } else {
                        ""
                    },
                ] {
                    if !label.is_empty() {
                        constrained = true;
                        constraints = constraints.child(
                            html::span(crate::i18n::tr(label).into_owned()).class("schema-key"),
                        );
                    }
                }
                if let Some(default) = col.get("default_value").and_then(Value::as_str) {
                    constrained = true;
                    constraints = constraints.child(html::span(crate::i18n::format_message(
                        "默认 {value}",
                        &[("value", default)],
                    )));
                }
                if !constrained {
                    constraints = constraints.child(html::span("—").class("muted"));
                }
                let comment = col.get("comment").and_then(Value::as_str).unwrap_or("");
                panel = panel.child(
                    html::div()
                        .class("schema-row schema-columns")
                        .child(html::strong(name))
                        .child(html::span(ty))
                        .child(html::span(crate::i18n::tr(nullable).into_owned()))
                        .child(constraints)
                        .child(html::span(comment).class("schema-comment")),
                );
            }
        }
        for (key, title) in [("indexes", "索引"), ("constraints", "约束")] {
            panel = panel
                .child(html::strong(crate::i18n::tr(title).into_owned()).class("section-title"));
            if let Some(items) = schema
                .get("details")
                .and_then(|v| v.get(key))
                .and_then(Value::as_array)
            {
                if items.is_empty() {
                    panel = panel
                        .child(html::span(crate::i18n::tr("无").into_owned()).class("empty-note"));
                }
                for item in items {
                    let part = |name| item.get(name).and_then(Value::as_str).unwrap_or("");
                    let ref_target = [
                        part("referenced_database"),
                        part("referenced_table"),
                        part("referenced_column"),
                    ]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(".");
                    let desc = if key == "indexes" {
                        format!(
                            "{}  {}  {}",
                            part("type"),
                            crate::i18n::tr(
                                if item.get("unique").and_then(Value::as_bool) == Some(true) {
                                    "唯一"
                                } else {
                                    "非唯一"
                                }
                            ),
                            item.get("expression")
                                .and_then(Value::as_str)
                                .or_else(|| item.get("column").and_then(Value::as_str))
                                .unwrap_or("")
                        )
                    } else {
                        format!("{}  {}  {}", part("type"), part("column"), ref_target)
                    };
                    panel = panel.child(
                        html::div()
                            .class("schema-row")
                            .child(html::strong(part("name")))
                            .child(html::span(desc)),
                    );
                }
            }
        }
    } else {
        panel = panel.child(
            html::span(crate::i18n::tr("正在读取结构或尚未选择对象").into_owned())
                .class("empty-note"),
        );
    }
    panel.into()
}

fn cell_drawer(app: &App) -> Node<Msg> {
    let mut drawer = html::div().class("drawer").child(
        html::div()
            .class("toolbar")
            .child(html::strong(crate::i18n::tr("单元格编辑").into_owned()))
            .child(html::span("").class("spacer"))
            .child(button("关闭", "overlay-close")),
    );
    let Some(cell) = app.detail_cell() else {
        return drawer
            .child(
                html::span(crate::i18n::tr("原结果已不可用，请重新选择单元格").into_owned())
                    .class("empty-note"),
            )
            .into();
    };
    let grid = app.grid.as_ref().unwrap();
    let mut identity = Vec::new();
    if let Some(keys) = app
        .schema
        .as_ref()
        .and_then(|schema| schema.get("keys"))
        .and_then(Value::as_array)
        .and_then(|keys| keys.first())
        .and_then(Value::as_array)
    {
        for name in keys.iter().filter_map(Value::as_str) {
            if let Some(column) = grid
                .page
                .result
                .columns
                .iter()
                .position(|column| column.name == name)
                && let Some(value) = grid.value(Cell {
                    row: cell.row,
                    column,
                })
            {
                identity.push(format!("{name} = {}", value_text(value, false)));
            }
        }
    }
    if identity.is_empty() {
        identity.push(crate::i18n::format_message(
            "第 {row} 行",
            &[("row", &(cell.row + 1).to_string())],
        ));
    }
    let mut selector = ui::select()
        .value(cell.column.to_string())
        .open(app.workbench.cell_fields_open)
        .on_toggle(|collapsed| Msg::Action(format!("cell-fields-open:{}", !collapsed)))
        .on_change(|change| {
            Msg::Action(format!(
                "cell-field:{}",
                change.value.as_str().unwrap_or_default()
            ))
        });
    for (index, column) in grid.page.result.columns.iter().enumerate() {
        selector = selector.option(ui::SelectOption::new(
            index.to_string(),
            column.name.clone(),
        ));
    }
    drawer = drawer
        .child(html::span(crate::i18n::tr("当前行").into_owned()).class("label"))
        .child(html::span(identity.join(" · ")).class("mono"))
        .child(ui::field(crate::i18n::tr("字段").into_owned()).child(selector));
    if writable(app) && grid.editable && !grid.page.result.columns[cell.column].generated {
        drawer = drawer
            .child(field(app, "cell", "值"))
            .child(
                html::div()
                    .class("choice-row")
                    .child(button("加入草稿", "set-cell"))
                    .child(button("设为 NULL", "set-null")),
            )
            .child(button("设置 JSON", "set-json"))
            .child(
                html::span(crate::i18n::tr("长值可在侧栏编辑，变更先进入草稿。").into_owned())
                    .class("empty-note"),
            );
    } else {
        drawer = drawer.child(html::pre(app.field("cell")).class("preview"));
    }
    drawer.into()
}

pub fn render(app: &App) -> View<Msg> {
    let redis = app.is_redis();
    let mut context = html::div().class("context");
    if app.profile.is_some() {
        context = context.child(provider_icon(app, redis));
    }
    context = context.child(
        html::strong(
            app.profile
                .as_ref()
                .map(|p| p.name.clone())
                .unwrap_or_else(|| crate::i18n::tr("未选择连接").into_owned()),
        )
        .class("context-title"),
    );
    if !redis {
        context = context.child(
            html::span(format!(
                " / {}",
                app.database
                    .as_deref()
                    .map(str::to_owned)
                    .unwrap_or_else(|| crate::i18n::tr("未选择数据库").into_owned())
            ))
            .class("breadcrumb"),
        );
    }
    if redis && app.profile.is_some() {
        context = context.child(
            html::div()
                .class("db-picker")
                .child(redis_db_picker(app, false)),
        );
    }
    if let Some(profile) = &app.profile {
        context = context
            .child(html::span(if redis { "Redis" } else { "MySQL" }).class("tag"))
            .child(
                html::span(
                    crate::i18n::tr(
                        if profile.access_mode == datapier_core::model::AccessMode::ReadWrite {
                            "读写"
                        } else {
                            "只读"
                        },
                    )
                    .into_owned(),
                )
                .class("tag"),
            );
    }
    let mut tab_strip = html::div().class("tab-strip").key("tab-strip");
    for (i, t) in app.tabs.iter().enumerate() {
        let left = format!("tab-close-left:{}", t.id);
        let right = format!("tab-close-right:{}", t.id);
        let all = format!("tab-close-all:{}", t.id);
        let target = if i == app.active {
            app.target.as_ref()
        } else {
            t.runtime.target.as_ref().or(t.browse.target.as_ref())
        };
        let view = if i == app.active {
            app.view.as_str()
        } else {
            t.runtime.view.as_str()
        };
        let icon = if view == "query" || target.is_none() {
            "terminal"
        } else if target.is_some_and(|target| target.kind == "table") {
            "table"
        } else if target.is_some_and(|target| target.kind == "view") {
            "eye"
        } else {
            "layers"
        };
        let title = html::button("")
            .class("tab-title")
            .child(ui::icon(icon))
            .child(html::span(t.title.clone()))
            .actions(
                ui::Actions::new()
                    .menu(ui::MenuEntry::header(
                        crate::i18n::tr("标签操作").into_owned(),
                    ))
                    .menu(
                        ui::MenuItem::new(&left, crate::i18n::tr("关闭左侧标签").into_owned())
                            .icon("arrow-left")
                            .disabled(i == 0),
                    )
                    .menu(
                        ui::MenuItem::new(&right, crate::i18n::tr("关闭右侧标签").into_owned())
                            .icon("arrow-right")
                            .disabled(i + 1 == app.tabs.len()),
                    )
                    .menu(ui::MenuEntry::Separator)
                    .menu(
                        ui::MenuItem::new(&all, crate::i18n::tr("关闭全部标签").into_owned())
                            .icon("x"),
                    ),
            )
            .on_action(left.clone(), Msg::Action(left))
            .on_action(right.clone(), Msg::Action(right))
            .on_action(all.clone(), Msg::Action(all))
            .on_click(Msg::Tab(i));
        let close = flat_action("x", "关闭标签", &format!("close-tab:{}", t.id)).class("tab-close");
        let tab = html::div()
            .class(if i == app.active { "tab active" } else { "tab" })
            .key(format!("tab{i}"));
        tab_strip = tab_strip.child(if cfg!(target_os = "macos") {
            tab.child(close).child(title)
        } else {
            tab.child(title).child(close)
        });
    }
    let tabs = html::div()
        .class("tabs")
        .key("tabs")
        .child(tab_strip)
        .child(icon_button("plus", "新建查询", "view-query"));
    let mut content = html::div().class("content").key("content").child(context);
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
    content = content.child(tabs);
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
        if !redis && (app.target.is_none() || (app.view == "query" && !app.browse_result)) {
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
            if app
                .profile
                .as_ref()
                .is_some_and(|p| p.access_mode == datapier_core::model::AccessMode::ReadWrite)
            {
                empty = empty.child(button("导入数据库", "overlay:import"));
            }
            body = body.child(empty);
        } else if redis {
            let target = app.target.as_ref().unwrap();
            let ttl = app
                .schema
                .as_ref()
                .and_then(|schema| schema.get("ttl_ms"))
                .and_then(Value::as_i64);
            let kind = app
                .schema
                .as_ref()
                .and_then(|schema| schema.get("type"))
                .and_then(Value::as_str)
                .unwrap_or(&target.kind);
            let type_label = match kind {
                "string" => "String",
                "hash" => "Hash",
                "list" => "List",
                "set" => "Set",
                "zset" => "ZSet",
                "none" => "不存在",
                _ => kind,
            };
            let type_label = if kind == "none" {
                crate::i18n::tr("不存在").into_owned()
            } else {
                type_label.to_owned()
            };
            let mut metadata = html::div().class("key-meta");
            if let Some(length) = app
                .schema
                .as_ref()
                .and_then(|schema| schema.get("length"))
                .and_then(Value::as_u64)
            {
                metadata = metadata.child(html::span(format!(
                    "{length} {}",
                    crate::i18n::tr(match kind {
                        "string" => "字节",
                        "hash" => "字段",
                        _ => "项",
                    })
                )));
            }
            metadata = metadata.child(html::span(format!("TTL {}", ttl_label(ttl))));
            if writable(app) {
                metadata = metadata.child(icon_button("edit", "编辑 TTL", "redis-op:set_ttl"));
            }
            let mut heading = html::div()
                .class("key-heading")
                .child(html::span(type_label).class("tag"))
                .child(html::strong(target.name.clone()));
            if writable(app) {
                heading = heading.child(icon_button("edit", "编辑 key 名称", "redis-op:rename"));
            }
            let copy_label = crate::i18n::tr("复制完整 key").into_owned();
            heading = heading.child(
                ui::button("")
                    .variant(ui::ButtonVariant::Quiet)
                    .size(ui::ButtonSize::Sm)
                    .icon("copy")
                    .key("copy-key")
                    .title(copy_label.clone())
                    .aria(copy_label)
                    .on_click(Msg::CopyText(workbench::redis_key_text(
                        &target.name,
                        target.key_b64.as_deref(),
                    ))),
            );
            if writable(app) {
                let label = crate::i18n::tr("删除 key").into_owned();
                heading = heading.child(
                    ui::button("")
                        .variant(ui::ButtonVariant::Quiet)
                        .size(ui::ButtonSize::Md)
                        .icon("trash")
                        .role("redis-delete")
                        .title(label.clone())
                        .aria(label)
                        .on_click(Msg::Action("redis-op:delete_key".into())),
                );
            }
            let title = html::div()
                .class("key-title")
                .child(heading)
                .child(metadata);
            body = body.child(title);
            let mut tools = html::div().class("key-actions");
            if app.target.as_ref().is_some_and(|t| t.kind == "string") {
                let mut formats = ui::tabs()
                    .variant(ui::TabsVariant::Segmented)
                    .active(app.workbench.value_format.clone());
                for (value, label) in [("text", "Text"), ("json", "JSON"), ("hex", "HEX")] {
                    formats = formats.tab(value, label);
                }
                tools =
                    tools.child(formats.on_select(|value| Msg::Action(format!("format:{value}"))));
            } else {
                tools = tools.child(
                    html::span(match kind {
                        "hash" => "Fields",
                        "list" => "Items",
                        "set" | "zset" => "Members",
                        _ => "Value",
                    })
                    .class("muted"),
                );
            }
            tools = tools.child(html::span("").class("spacer"));
            if writable(app) {
                tools = tools.child(icon_button("edit", "编辑 value", "redis-edit"));
            }
            if writable(app) {
                let operations: &[(&str, &str, &str)] = match kind {
                    "hash" => &[("plus", "设置 Hash 字段", "hash_set")],
                    "list" => &[("plus", "插入 List 元素", "list_insert")],
                    "set" => &[("plus", "添加 Set 成员", "set_add")],
                    "zset" => &[("plus", "设置 ZSet 成员", "zset_set")],
                    _ => &[],
                };
                for (icon, label, operation) in operations {
                    tools = tools.child(icon_button(icon, label, &format!("redis-op:{operation}")));
                }
                if matches!(kind, "hash" | "list" | "set" | "zset") {
                    let label = crate::i18n::tr("删除选中条目").into_owned();
                    tools = tools.child(
                        ui::button("")
                            .variant(ui::ButtonVariant::Quiet)
                            .size(ui::ButtonSize::Md)
                            .icon("trash")
                            .role("redis-delete")
                            .key("redis-row-delete")
                            .title(label.clone())
                            .aria(label)
                            .disabled(!app.redis_row_delete_enabled())
                            .on_click(Msg::Action("redis-row-delete".into())),
                    );
                }
            }
            if app.grid.is_some() {
                tools = tools.child(icon_button("columns", "列配置", "overlay:columns"));
            }
            if writable(app) {
                tools = tools.child(icon_button("arrow-up", "导入", "overlay:import"));
            }
            tools = tools.child(icon_button("download", "导出", "overlay:export"));
            let value_copy_label = crate::i18n::tr("复制原始 value（不含草稿）").into_owned();
            tools = tools.child(
                ui::button("")
                    .variant(ui::ButtonVariant::Quiet)
                    .size(ui::ButtonSize::Sm)
                    .icon("copy")
                    .key("copy-value")
                    .title(value_copy_label.clone())
                    .aria(value_copy_label)
                    .on_click(Msg::CopyRedisValue),
            );
            body = body.child(tools);
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
        if !redis
            && let Some(target) = &app.target
            && app.browse_result
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
                body = body.child(schema_panel(app));
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
                        crate::i18n::tr(if target.kind == "table" {
                            "基表"
                        } else {
                            "视图"
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
                tools = tools
                    .child(row_search(app))
                    .child(icon_button("funnel", "过滤与排序", "overlay:filters"))
                    .child(icon_button("columns", "列配置", "overlay:columns"))
                    .child(html::span("").class("spacer"));
                if writable(app) && app.grid.as_ref().is_some_and(|g| g.editable) {
                    tools = tools.child(icon_button("plus", "新增行", "overlay:row"));
                }
                tools = tools.child(icon_button("download", "导出", "overlay:export"));
                if writable(app)
                    && app
                        .target
                        .as_ref()
                        .is_some_and(|target| target.kind == "table")
                {
                    tools = tools.child(icon_button("arrow-up", "导入", "overlay:import"));
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
                let results = html::div()
                    .class("sql-results-head")
                    .child(result_tabs)
                    .child(row_search(app))
                    .child(icon_button("columns", "列配置", "overlay:columns"))
                    .child(icon_button("download", "导出", "overlay:export"))
                    .child(icon_button("x", "关闭结果", "query-result-close"));
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
                let mut footer = html::div().class("table-footer").child(
                    html::span(crate::i18n::format_message(
                        "本页 {page} 行 · 共 {total} 行",
                        &[
                            ("page", &grid.page.rows.len().to_string()),
                            ("total", &grid.page.result.row_count.to_string()),
                        ],
                    ))
                    .class("muted"),
                );
                if grid.selected_row_count() > 0 {
                    footer = footer.child(
                        html::span(crate::i18n::format_message(
                            "已选 {count} 行",
                            &[("count", &grid.selected_row_count().to_string())],
                        ))
                        .class("selected-count"),
                    );
                }
                footer = footer.child(html::span("").class("spacer"));
                if app.browse_result
                    && (!redis
                        || app
                            .target
                            .as_ref()
                            .is_some_and(|target| target.kind != "string"))
                {
                    footer = footer.child(
                        html::span(crate::i18n::format_message(
                            if redis {
                                "单击选行 · Shift 连选 · {modifier} 增选"
                            } else {
                                "行号选行 · Shift 连选 · {modifier} 增选 · 双击单元格编辑"
                            },
                            &[("modifier", crate::i18n::modifier_label())],
                        ))
                        .class("selection-hint muted"),
                    );
                }
                if !redis && !app.browse_result {
                    footer = footer.child(
                        html::span(crate::i18n::tr("查询结果只读").into_owned()).class("muted"),
                    );
                }
                footer = footer
                    .child(page_button(
                        "arrow-left",
                        "上一页",
                        "previous",
                        grid.page.start > 0,
                    ))
                    .child(
                        html::span(crate::i18n::format_message(
                            "第 {number} 页",
                            &[("number", &(app.page_history.len() + 1).to_string())],
                        ))
                        .class("page-indicator"),
                    )
                    .child(page_button(
                        "arrow-right",
                        "下一页",
                        "next",
                        grid.page.start + (grid.page.rows.len() as u64)
                            < grid.page.result.row_count,
                    ));
                footer = footer.child(
                    ui::button("")
                        .variant(ui::ButtonVariant::Quiet)
                        .size(ui::ButtonSize::Md)
                        .icon("copy")
                        .title(crate::i18n::tr("复制选区").into_owned())
                        .prop("aria-label", crate::i18n::tr("复制选区").into_owned())
                        .disabled(grid.focus.is_none())
                        .on_click(Msg::CopyGrid),
                );
                if !redis {
                    footer = footer.child(
                        flat_action("info", "单元格详情", "overlay:cell").class("status-action"),
                    );
                }
                body = body.child(footer);
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
        if redis && app.workbench.cli {
            content = content
                .child(
                    html::div()
                        .class("body redis-layout")
                        .child(redis_browser(app))
                        .child(body),
                )
                .child(cli_panel(app));
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
    if app.navigation_drop_running() {
        status = status.child(button("停止等待", "cancel"));
    }
    status = status.child(html::span("").class("spacer"));
    if !app.detail.is_empty() {
        status =
            status.child(flat_action("info", "详情", "overlay:diagnostics").class("status-action"));
    }
    if redis && !app.tabs.is_empty() {
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
        .child(resource_panel(app))
        .child(content);
    let layer = app
        .workbench
        .overlay
        .as_deref()
        .filter(|kind| redis || *kind != "cell")
        .map(|kind| vec![overlay(app, kind)])
        .unwrap_or_default();
    View::new().main(vec![Node::from(layout)]).layer(layer)
}

#[cfg(test)]
#[path = "../tests/unit/views.rs"]
mod tests;
