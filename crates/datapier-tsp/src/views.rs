use super::*;

fn button(label: &str, action: &str) -> Node<Msg> {
    html::button(label)
        .class("button")
        .key(action.to_owned())
        .on_click(Msg::Action(action.into()))
        .into()
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
fn field(app: &App, name: &str, label: &str) -> Node<Msg> {
    let ed = if name == "cli_text" {
        &app.tabs[app.active].cli_query
    } else {
        &app.fields[name]
    };
    let display = if name == "password" {
        "•".repeat(ed.text.encode_utf16().count())
    } else {
        ed.text.clone()
    };
    let f = name.to_string();
    let u = f.clone();
    let n = f.clone();
    if name == "cell" || name == "redis_value" || name == "cli_text" {
        html::div()
            .class("field")
            .key(name.to_string())
            .child(html::span(label).class("label"))
            .child(
                ui::editor()
                    .key(format!("input-{name}"))
                    .text(display)
                    .cursor(ed.cursor)
                    .anchor(ed.anchor)
                    .max_lines(16u32)
                    .on_focus(Msg::Focus(f))
                    .on_undo(Msg::Undo(u))
                    .on_edit(move |e| {
                        Msg::Edit(n.clone(), e.from, e.to, e.text.clone(), e.cursor, e.len)
                    }),
            )
            .into()
    } else {
        html::div()
            .class("field")
            .key(name.to_string())
            .child(html::span(label).class("label"))
            .child(
                ui::input()
                    .key(format!("input-{name}"))
                    .text(display)
                    .cursor(ed.cursor)
                    .anchor(ed.anchor)
                    .on_focus(Msg::Focus(f))
                    .on_undo(Msg::Undo(u))
                    .on_edit(move |e| {
                        Msg::Edit(n.clone(), e.from, e.to, e.text.clone(), e.cursor, e.len)
                    }),
            )
            .into()
    }
}
fn query(app: &App) -> Node<Msg> {
    let e = &app.tabs[app.active].query;
    ui::editor()
        .key("query-editor")
        .text(e.text.clone())
        .cursor(e.cursor)
        .anchor(e.anchor)
        .mode("insert")
        .lang("sql")
        .max_lines(12u32)
        .on_focus(Msg::Focus("query".into()))
        .on_undo(Msg::Undo("query".into()))
        .on_edit(|e| {
            Msg::Edit(
                "query".into(),
                e.from,
                e.to,
                e.text.clone(),
                e.cursor,
                e.len,
            )
        })
        .into()
}
fn completions(app: &App) -> Node<Msg> {
    let query = &app.tabs[app.active].query;
    let mut row = html::div().class("completion");
    if let Some(byte) = editor::utf16_to_byte(&query.text, query.cursor) {
        let before = &query.text[..byte];
        let start = before
            .char_indices()
            .rev()
            .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
            .map_or(0, |(i, c)| i + c.len_utf8());
        let prefix = &before[start..];
        if !prefix.is_empty() {
            let catalog: Vec<String> = app.resources.iter().map(|r| r.name.clone()).collect();
            let from = before[..start].encode_utf16().count();
            for item in editor::completions(prefix, &catalog).into_iter().take(8) {
                row = row.child(
                    html::button(item.clone())
                        .class("button")
                        .on_click(Msg::Complete(item, from, query.cursor)),
                );
            }
        }
    }
    row.into()
}
fn resource_panel(app: &App) -> Node<Msg> {
    let compact = app.navigation_compact();
    let mut panel = html::div()
        .class(if compact {
            "sidebar compact"
        } else {
            "sidebar"
        })
        .key("sidebar")
        .child(html::strong(if compact { "D" } else { "D  Datapier" }).class("brand"))
        .child(button(
            if compact { "+" } else { "+ 新建连接" },
            "new-profile",
        ));
    for (i, p) in app.profiles.iter().enumerate() {
        let active = app
            .profile
            .as_ref()
            .is_some_and(|selected| selected.id == p.id);
        let provider = if p.provider == datapier_core::model::ProviderKind::Redis {
            "RED"
        } else {
            "SQL"
        };
        let mut card = html::button("")
            .class(if active {
                "connection active"
            } else {
                "connection"
            })
            .key(format!("p{}", p.id))
            .prop(
                "title",
                format!("{} · {} · {}:{}", p.name, provider, p.host, p.port),
            )
            .child(html::span(provider).class("connection-icon"));
        if !compact {
            card = card.child(
                html::div()
                    .class("connection-info")
                    .child(html::strong(p.name.clone()))
                    .child(
                        html::span(format!("{} · {}:{}", provider, p.host, p.port)).class("muted"),
                    ),
            );
        }
        panel = panel.child(card.on_click(Msg::Profile(i)));
    }
    if !compact {
        panel = panel.child(
            html::div()
                .class("toolbar")
                .child(button("连接", "open"))
                .child(button("设置", "edit-profile"))
                .child(button("删除", "delete-profile")),
        );
        if app.session.is_some() && !app.is_redis() {
            let ed = &app.fields["object_filter"];
            panel = panel.child(
                html::div().class("object-search").child(
                    ui::input()
                        .key("input-object_filter")
                        .text(ed.text.clone())
                        .cursor(ed.cursor)
                        .anchor(ed.anchor)
                        .on_focus(Msg::Focus("object_filter".into()))
                        .on_undo(Msg::Undo("object_filter".into()))
                        .on_edit(|e| {
                            Msg::Edit(
                                "object_filter".into(),
                                e.from,
                                e.to,
                                e.text.clone(),
                                e.cursor,
                                e.len,
                            )
                        }),
                ),
            );
            if let Some(db) = &app.database {
                panel = panel.child(html::strong(format!("数据库  {db}")).class("group-title"));
            }
            let needle = app.field("object_filter").to_lowercase();
            for kind in ["database", "table", "view"] {
                let mut group = html::div().class("resource-group");
                let mut count = 0;
                let mut total = 0;
                for (i, r) in app.resources.iter().enumerate() {
                    if r.kind != kind
                        || (kind != "database" && r.database.as_deref() != app.database.as_deref())
                    {
                        continue;
                    }
                    total += 1;
                    if kind != "database" && !r.name.to_lowercase().contains(&needle) {
                        continue;
                    }
                    count += 1;
                    let active = app.target.as_ref().is_some_and(|t| {
                        t.name == r.name && t.database == r.database && t.kind == r.kind
                    });
                    group = group.child(
                        html::button(r.name.clone())
                            .class(if active {
                                "resource active"
                            } else {
                                "resource"
                            })
                            .key(format!("r{i}"))
                            .on_click(Msg::Resource(i)),
                    );
                }
                if kind == "database" && total == 0 {
                    continue;
                }
                let title = match kind {
                    "database" => "数据库",
                    "table" => "表",
                    _ => "视图",
                };
                if kind != "database" && count == 0 {
                    group = group.child(html::span("无匹配对象").class("empty-note"));
                }
                panel = panel
                    .child(
                        html::strong(if kind == "database" {
                            title.to_string()
                        } else {
                            format!("{title}  {count}/{total}")
                        })
                        .class("group-title"),
                    )
                    .child(group);
            }
            panel = panel.child(button("刷新资源", "list"));
        }
    }
    panel.into()
}
fn grid(app: &App) -> Option<Node<Msg>> {
    let grid = app.grid.as_ref()?;
    let cols = grid.visible_columns();
    let mut wrapper = html::div().class("gridscroll").key("grid");
    let mut header = html::div().class("gridrow header").key("header");
    header = header.child(html::span("#").class("rownum"));
    for &col in &cols {
        header = header
            .child(html::span(&grid.page.result.columns[col].name).class(format!("cell col{col}")));
    }
    wrapper = wrapper.child(header);
    let needle = app.field("row_search").to_lowercase();
    for (i, row) in grid.page.rows.iter().enumerate() {
        let row_id = grid.page.start + i as u64;
        if !needle.is_empty()
            && !row.iter().enumerate().any(|(col, value)| {
                let value = grid
                    .value(Cell {
                        row: row_id,
                        column: col,
                    })
                    .unwrap_or(value);
                value_text(value, app.is_redis())
                    .to_lowercase()
                    .contains(&needle)
            })
        {
            continue;
        }
        let mut line = html::div().class("gridrow").key(format!("r{row_id}"));
        let first = cols.first().copied().unwrap_or(0);
        line = line.child(
            html::button((row_id + 1).to_string())
                .class("rownum")
                .on_click(Msg::Row(row_id, first)),
        );
        for &col in &cols {
            let cell = Cell {
                row: row_id,
                column: col,
            };
            let value = grid.value(cell).or_else(|| row.get(col));
            let text = value.map(|v| cell_preview(app, v)).unwrap_or_default();
            let selected = grid.is_selected(cell);
            let class = if selected {
                format!("cell col{col} selected")
            } else {
                format!("cell col{col}")
            };
            if app.workbench.inline == Some(cell) && app.large.is_none() {
                let ed = &app.fields["cell"];
                line = line.child(
                    html::div().class(class).key(format!("c{col}")).child(
                        ui::input()
                            .key("cell-editor")
                            .text(ed.text.clone())
                            .cursor(ed.cursor)
                            .anchor(ed.anchor)
                            .on_focus(Msg::Focus("cell".into()))
                            .on_undo(Msg::Undo("cell".into()))
                            .on_edit(|e| {
                                Msg::Edit(
                                    "cell".into(),
                                    e.from,
                                    e.to,
                                    e.text.clone(),
                                    e.cursor,
                                    e.len,
                                )
                            }),
                    ),
                );
            } else {
                line = line.child(
                    html::button(text)
                        .class(class)
                        .key(format!("c{col}"))
                        .prop("actions", json!({"click":"click"}))
                        .on_action_with("click", move |event| {
                            Msg::Cell(row_id, col, event.mods.iter().any(|m| m == "shift"))
                        })
                        .on_dblclick(Msg::CellOpen(row_id, col)),
                );
            }
        }
        wrapper = wrapper.child(line);
    }
    Some(wrapper.into())
}
pub fn stylesheet(app: &App) -> String {
    let mut css = include_str!("workbench.css").to_string();
    if let Some(g) = &app.grid {
        for (i, s) in g.settings.iter().enumerate() {
            let sticky = g
                .frozen_offset(i)
                .map(|x| format!("position:sticky; left:{}px; z-index:2;", x + 48))
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

fn history_panel(app: &App) -> Node<Msg> {
    let mut list = html::div().class("history-list");
    let needle = app.field("history_filter").to_lowercase();
    for (i, item) in app.workbench.history.iter().enumerate() {
        let text = item.get("text").and_then(Value::as_str).unwrap_or("");
        let state = item.get("state").and_then(Value::as_str).unwrap_or("");
        let time = item
            .get("created_at")
            .map(Value::to_string)
            .unwrap_or_default();
        if !format!("{text} {state} {time}")
            .to_lowercase()
            .contains(&needle)
        {
            continue;
        }
        list = list.child(
            html::button(format!(
                "{}  ·  {}  ·  {}",
                text.replace('\n', " "),
                time,
                state
            ))
            .class(if app.workbench.history_selected == Some(i) {
                "history-item active"
            } else {
                "history-item"
            })
            .on_click(Msg::Action(format!("history-select:{i}"))),
        );
    }
    let mut preview = html::div()
        .class("detail-panel")
        .child(html::strong("SQL 预览"));
    if let Some(i) = app.workbench.history_selected
        && let Some(item) = app.workbench.history.get(i)
    {
        preview = preview
            .child(button("打开到查询", &format!("history-open:{i}")))
            .child(
                html::pre(item.get("text").and_then(Value::as_str).unwrap_or("")).class("preview"),
            );
    }
    html::div()
        .class("body")
        .child(field(app, "history_filter", "搜索历史"))
        .child(html::div().class("split").child(list).child(preview))
        .into()
}

fn choices(app: &App, field_name: &str, label: &str, options: &[(&str, &str)]) -> Node<Msg> {
    let mut row = html::div()
        .class("choice-row")
        .child(html::span(label).class("label"));
    for &(value, title) in options {
        row = row.child(
            html::button(title)
                .class(if app.field(field_name) == value {
                    "button active"
                } else {
                    "button"
                })
                .on_click(Msg::Action(format!("field:{field_name}:{value}"))),
        );
    }
    row.into()
}

fn ttl_label(ttl: Option<i64>) -> String {
    match ttl {
        Some(-1) => "永久".into(),
        Some(-2) => "不存在".into(),
        Some(ms) if ms >= 0 => format!("{} 秒", ms / 1000),
        _ => "未读取".into(),
    }
}

fn schema_panel(app: &App) -> Node<Msg> {
    let mut panel = html::div().class("schema-panel");
    if let Some(schema) = &app.schema {
        panel = panel.child(html::strong("字段").class("section-title"));
        if let Some(columns) = schema.get("columns").and_then(Value::as_array) {
            for col in columns {
                let name = col.get("name").and_then(Value::as_str).unwrap_or("");
                let ty = col.get("type_name").and_then(Value::as_str).unwrap_or("");
                let nullable = if col.get("nullable").and_then(Value::as_bool) == Some(false) {
                    "NOT NULL"
                } else {
                    "可空"
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
                let default = col
                    .get("default_value")
                    .and_then(Value::as_str)
                    .unwrap_or("—");
                panel = panel.child(
                    html::div()
                        .class("schema-row")
                        .child(html::strong(name))
                        .child(html::span(ty))
                        .child(html::span(nullable))
                        .child(html::span(key))
                        .child(html::span(generated))
                        .child(html::span(format!("默认 {default}"))),
                );
            }
        }
        for (key, title) in [("indexes", "索引"), ("constraints", "约束")] {
            panel = panel.child(html::strong(title).class("section-title"));
            if let Some(items) = schema
                .get("details")
                .and_then(|v| v.get(key))
                .and_then(Value::as_array)
            {
                if items.is_empty() {
                    panel = panel.child(html::span("无").class("empty-note"));
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
                            if item.get("unique").and_then(Value::as_bool) == Some(true) {
                                "唯一"
                            } else {
                                "非唯一"
                            },
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
        panel = panel.child(html::span("正在读取结构或尚未选择对象").class("empty-note"));
    }
    panel.into()
}

fn draft_field(value: &Value) -> String {
    if let Some(kind) = value.get("type").and_then(Value::as_str) {
        let data = value.get("value").and_then(Value::as_str).unwrap_or("");
        return match kind {
            "bytes" => {
                value_text(&datapier_core::model::Value::Bytes(data.to_owned()), true).into_owned()
            }
            "text" => data.to_owned(),
            _ => "无法显示".into(),
        };
    }
    match value {
        Value::Null => "永久".into(),
        Value::String(s) => s.clone(),
        _ => value.to_string(),
    }
}
fn redis_draft_before(before: &Value) -> String {
    let Some(fields) = before.as_object() else {
        return "未载入".into();
    };
    [
        ("field", "字段"),
        ("index", "索引"),
        ("member", "成员"),
        ("value", "值"),
        ("score", "分数"),
    ]
    .iter()
    .filter_map(|(key, label)| {
        fields
            .get(*key)
            .map(|value| format!("{label}：{}", draft_field(value)))
    })
    .collect::<Vec<_>>()
    .join(" · ")
}
fn redis_draft_after(change: &Value) -> String {
    let op = change
        .get("operation")
        .and_then(Value::as_str)
        .unwrap_or("");
    let label = match op {
        "set_string" | "hash_set" | "list_set" => "设置",
        "list_insert" | "set_add" => "添加",
        "hash_delete" | "list_delete" | "set_remove" | "zset_remove" | "delete_key" => "删除",
        "zset_set" => "设置成员",
        "rename" => "重命名",
        "set_ttl" => "设置 TTL",
        _ => "变更",
    };
    let mut parts = vec![label.to_owned()];
    for (key, name) in [
        ("field", "字段"),
        ("index", "索引"),
        ("member", "成员"),
        ("value", "值"),
        ("score", "分数"),
        ("ttl_ms", "TTL 毫秒"),
    ] {
        if let Some(value) = change.get(key) {
            parts.push(format!("{name}：{}", draft_field(value)));
        }
    }
    if let Some(encoded) = change.get("new_key_b64").and_then(Value::as_str) {
        let value = datapier_core::model::Value::Bytes(encoded.to_owned());
        parts.push(format!("新 key：{}", value_text(&value, true)));
    }
    parts.join(" · ")
}

fn draft_panel(app: &App) -> Node<Msg> {
    let mut panel = html::div().class("draft-list");
    if let Some(grid) = &app.grid {
        for (cell, after) in grid.edits() {
            let name = grid
                .page
                .result
                .columns
                .get(cell.column)
                .map(|c| c.name.as_str())
                .unwrap_or("");
            let before = grid
                .original(*cell)
                .map(|v| cell_preview(app, v))
                .unwrap_or_default();
            panel = panel.child(
                html::div()
                    .class("draft-item")
                    .child(html::strong(format!("第 {} 行 · {}", cell.row + 1, name)))
                    .child(html::span(format!("原值：{before}")))
                    .child(html::span(format!("新值：{}", cell_preview(app, after)))),
            );
        }
    }
    for row in &app.pending_rows {
        let before = row
            .before
            .iter()
            .map(|(k, v)| format!("{k}: {}", cell_preview(app, v)))
            .collect::<Vec<_>>()
            .join(" · ");
        let after = row
            .values
            .iter()
            .map(|(k, v)| format!("{k}: {}", cell_preview(app, v)))
            .collect::<Vec<_>>()
            .join(" · ");
        panel = panel.child(
            html::div()
                .class("draft-item")
                .child(html::strong(row.operation.clone()))
                .child(html::span(format!("原值：{before}")))
                .child(html::span(format!("新值：{after}"))),
        );
    }
    for (index, item) in app.redis_changes.iter().enumerate() {
        if let Some(preview) = app
            .workbench
            .redis_previews
            .iter()
            .find(|preview| preview.start == index)
        {
            let after = &app.redis_changes[preview.end - 1];
            panel = panel.child(
                html::div()
                    .class("draft-item")
                    .child(html::strong(
                        app.target
                            .as_ref()
                            .map(|target| target.name.as_str())
                            .unwrap_or("Redis"),
                    ))
                    .child(html::pre(format!(
                        "原值：{}",
                        redis_draft_before(&preview.before)
                    )))
                    .child(html::pre(format!("新值：{}", redis_draft_after(after)))),
            );
        } else if !app
            .workbench
            .redis_previews
            .iter()
            .any(|preview| preview.start < index && index < preview.end)
        {
            panel = panel.child(
                html::div()
                    .class("draft-item")
                    .child(html::strong(
                        app.target
                            .as_ref()
                            .map(|target| target.name.as_str())
                            .unwrap_or("Redis"),
                    ))
                    .child(html::span("原值：未载入"))
                    .child(html::span("恢复的操作；原基线已失效，请重新确认"))
                    .child(html::pre(format!("新值：{}", redis_draft_after(item)))),
            );
        }
    }
    if let Some(paste) = &app.paste_preview {
        for edit in &paste.edits {
            panel = panel.child(
                html::div()
                    .class("draft-item")
                    .child(html::strong(format!(
                        "粘贴 · 第 {} 行，第 {} 列",
                        edit.cell.row + 1,
                        edit.cell.column + 1
                    )))
                    .child(html::span(format!(
                        "原值：{}",
                        cell_preview(app, &edit.before)
                    )))
                    .child(html::span(format!(
                        "新值：{}",
                        cell_preview(app, &edit.after)
                    ))),
            );
        }
    }
    panel.into()
}

fn redis_browser(app: &App) -> Node<Msg> {
    let mut list = html::div()
        .class("key-browser")
        .child(field(app, "pattern", "SCAN 匹配"))
        .child(choices(
            app,
            "type",
            "类型",
            &[
                ("", "全部"),
                ("string", "String"),
                ("hash", "Hash"),
                ("list", "List"),
                ("set", "Set"),
                ("zset", "ZSet"),
            ],
        ))
        .child(
            html::div()
                .class("toolbar")
                .child(button("刷新", "list"))
                .child(button("更多", "more"))
                .child(button(
                    if app.workbench.tree {
                        "列表"
                    } else {
                        "命名空间"
                    },
                    "tree-toggle",
                )),
        )
        .child(
            html::span(format!(
                "已加载 {} 个 key · {}",
                app.resources.len(),
                if app.workbench.scan_dirty {
                    "条件已更改，请刷新"
                } else if app.scan_token.is_some() {
                    "扫描中"
                } else if app.cursor.is_some() {
                    "可加载更多"
                } else {
                    "扫描完成"
                }
            ))
            .class("muted"),
        );
    let page_count = app
        .resources
        .len()
        .div_ceil(workbench::KEY_PAGE_SIZE)
        .max(1);
    let page = app.workbench.key_page.min(page_count - 1);
    list = list.child(
        html::div()
            .class("toolbar")
            .child(button("上一页", "keys-previous"))
            .child(html::span(format!("第 {} / {} 页", page + 1, page_count)).class("muted"))
            .child(button("下一页", "keys-next")),
    );
    let start = page * workbench::KEY_PAGE_SIZE;
    let end = (start + workbench::KEY_PAGE_SIZE).min(app.resources.len());
    let mut indices: Vec<usize> = (start..end).collect();
    if app.workbench.tree {
        indices.sort_by(|&a, &b| app.resources[a].name.cmp(&app.resources[b].name));
    }
    let mut namespace = String::new();
    for i in indices {
        let r = &app.resources[i];
        if app.workbench.tree {
            let next = r
                .name
                .split_once(':')
                .map(|(prefix, _)| prefix)
                .unwrap_or("其他");
            if next != namespace {
                namespace = next.to_string();
                list = list.child(html::strong(namespace.clone()).class("group-title"));
            }
        }
        let active = app
            .target
            .as_ref()
            .is_some_and(|t| t.key_b64 == r.key_b64 && t.name == r.name);
        let ttl = ttl_label(r.data.get("ttl_ms").and_then(Value::as_i64));
        list = list.child(
            html::button("")
                .class(if active {
                    "resource key-row active"
                } else {
                    "resource key-row"
                })
                .key(format!("r{i}"))
                .child(html::span(r.kind.clone()).class("key-type"))
                .child(html::span(r.name.clone()).class("key-name"))
                .child(html::span(ttl).class("key-ttl"))
                .on_click(Msg::Resource(i)),
        );
    }
    list.into()
}

fn overlay(app: &App, kind: &str) -> Node<Msg> {
    let mut panel = html::div().class("modal").child(
        html::div()
            .class("toolbar")
            .child(html::strong(match kind {
                "profile" => "连接设置",
                "transfer" => "导入 / 导出",
                "cell" => "单元格",
                "cli-value" => "CLI 单元格",
                "redis" => "Redis 操作",
                "columns" => "列配置",
                "filters" => "过滤与排序",
                "row" => "行操作",
                "preview" => "变更预览",
                "file" => "查询文件",
                _ => "设置",
            }))
            .child(button(
                "关闭",
                if app.view == "profile" {
                    "view-query"
                } else {
                    "overlay-close"
                },
            )),
    );
    match kind {
        "profile" => {
            panel = panel
                .child(field(app, "name", "名称"))
                .child(choices(
                    app,
                    "provider",
                    "类型",
                    &[("mysql", "MySQL"), ("redis", "Redis")],
                ))
                .child(field(app, "host", "主机"))
                .child(field(app, "port", "端口"))
                .child(field(app, "user", "用户名"));
            if app.field("provider") == "mysql" {
                panel = panel.child(field(app, "database", "数据库"));
            } else {
                panel = panel.child(field(app, "redis_db", "Redis DB"));
            }
            panel = panel
                .child(field(app, "password", "密码"))
                .child(choices(
                    app,
                    "access",
                    "权限",
                    &[("read_only", "只读"), ("read_write", "读写")],
                ))
                .child(choices(
                    app,
                    "tls",
                    "TLS",
                    &[("false", "关闭"), ("true", "启用")],
                ));
            if app.field("tls") == "true" {
                for (name, label) in [("ca", "CA 路径"), ("cert", "证书路径"), ("key", "私钥路径")]
                {
                    panel = panel.child(field(app, name, label));
                }
            }
            panel = panel
                .child(button("测试连接", "test-profile"))
                .child(button("保存连接", "save-profile"));
        }
        "transfer" => {
            for (name, label) in [
                ("path", "文件路径"),
                ("format", "格式 csv/json"),
                ("mapping", "列映射 JSON"),
                ("null_marker", "NULL 标记"),
                ("backslash_escape", "反斜线转义"),
                ("binary_columns", "二进制列 JSON"),
            ] {
                panel = panel.child(field(app, name, label));
            }
            if (writable(app)
                && (app.is_redis() || app.target.as_ref().is_some_and(|t| t.kind == "table")))
                || (app.is_redis()
                    && app.target.is_none()
                    && app.profile.as_ref().is_some_and(|p| {
                        p.access_mode == datapier_core::model::AccessMode::ReadWrite
                    }))
            {
                panel = panel
                    .child(button("预览导入", "preview-import"))
                    .child(button("确认导入", "import"));
            }
            panel = panel.child(button("导出完整结果", "export"));
        }
        "file" => {
            panel = panel
                .child(field(app, "path", "文件路径"))
                .child(button("打开", "file-open"))
                .child(button("保存", "file-save"));
        }
        "columns" => {
            panel = panel
                .child(field(app, "column", "列序号"))
                .child(field(app, "width", "宽度"))
                .child(button("设置宽度", "column-width"))
                .child(button("隐藏 / 显示", "column-hide"))
                .child(button("冻结 / 解冻", "column-freeze"));
        }
        "filters" => {
            panel = panel
                .child(field(app, "filter", "过滤 JSON"))
                .child(field(app, "sort", "排序 JSON"))
                .child(button("应用并刷新", "browse"));
        }
        "row" => {
            if writable(app) {
                panel = panel
                    .child(field(app, "row_values", "新增行 JSON"))
                    .child(button("新增行草稿", "insert-row"))
                    .child(button("删除选中行", "delete-row"))
                    .child(button("撤销行草稿", "undo-row"));
            }
        }
        "cell" => {
            panel = panel.child(html::pre(app.field("cell")).class("preview"));
            if writable(app) && !app.is_redis() {
                panel = panel
                    .child(field(app, "cell", "单元格值"))
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
                        "读取中…"
                    } else if !app.workbench.cli_error.is_empty() {
                        app.workbench.cli_error.as_str()
                    } else {
                        "尚未读取完整值"
                    };
                    panel = panel.child(html::span(message).class("muted"));
                } else {
                    panel = panel
                        .child(html::pre(value_text(value, true).into_owned()).class("preview"));
                }
            }
        }
        "preview" => {
            panel = panel.child(draft_panel(app));
            if app.paste_preview.is_some() && writable(app) {
                panel = panel.child(button("确认粘贴", "paste-confirm"));
            }
            if writable(app) {
                panel = panel.child(button(
                    "提交变更",
                    if app.is_redis() {
                        "redis-submit"
                    } else {
                        "submit"
                    },
                ));
            }
        }
        "diagnostics" => {
            panel = panel.child(html::pre(app.detail.clone()).class("preview"));
        }
        "redis" if writable(app) => {
            let kind = app.target.as_ref().map(|t| t.kind.as_str()).unwrap_or("");
            let ops: &[(&str, &str)] = match kind {
                "string" => &[("设置值", "set_string")],
                "hash" => &[("设置字段", "hash_set"), ("删除字段", "hash_delete")],
                "list" => &[
                    ("设置元素", "list_set"),
                    ("插入元素", "list_insert"),
                    ("删除元素", "list_delete"),
                ],
                "set" => &[("添加成员", "set_add"), ("删除成员", "set_remove")],
                "zset" => &[("设置成员", "zset_set"), ("删除成员", "zset_remove")],
                _ => &[],
            };
            for (label, op) in ops.iter().copied().chain([
                ("重命名", "rename"),
                ("删除 key", "delete_key"),
                ("设置 TTL", "set_ttl"),
            ]) {
                panel = panel.child(button(label, &format!("redis-op:{op}")));
            }
            let op = app.field("operation");
            panel = panel.child(html::span(format!("当前操作：{op}")).class("muted"));
            match op {
                "set_string" => panel = panel.child(field(app, "redis_value", "Value")),
                "hash_set" | "hash_delete" => {
                    panel = panel.child(field(app, "redis_field", "Field"));
                    if op == "hash_set" {
                        panel = panel.child(field(app, "redis_value", "Value"));
                    }
                }
                "list_set" | "list_insert" | "list_delete" => {
                    panel = panel.child(html::span(format!("Index: {}", app.field("redis_index"))));
                    if op != "list_delete" {
                        panel = panel.child(field(app, "redis_value", "Value"));
                    }
                }
                "set_add" | "set_remove" => {
                    panel = panel.child(field(app, "redis_member", "Member"))
                }
                "zset_set" | "zset_remove" => {
                    panel = panel.child(field(app, "redis_member", "Member"));
                    if op == "zset_set" {
                        panel = panel.child(field(app, "redis_score", "Score"));
                    }
                }
                "rename" => panel = panel.child(field(app, "redis_key", "新 key")),
                "set_ttl" => panel = panel.child(field(app, "redis_ttl", "TTL 毫秒（空为永久）")),
                _ => {}
            }
            panel = panel.child(button("加入变更预览", "redis-stage"));
        }
        _ => {}
    }
    panel.into()
}

fn string_value(app: &App) -> Node<Msg> {
    let mut panel = html::div()
        .class("string-value")
        .child(html::strong("Value"));
    if let Some(grid) = &app.grid
        && let Some(value) = grid.value(Cell {
            row: grid.page.start,
            column: 0,
        })
    {
        let text = if let datapier_core::model::Value::Bytes(encoded) = value {
            use base64::Engine as _;
            match base64::engine::general_purpose::STANDARD.decode(encoded) {
                Ok(bytes) if app.workbench.value_format == "hex" => {
                    use std::fmt::Write as _;
                    let mut text = String::with_capacity(bytes.len().saturating_mul(3));
                    for (i, byte) in bytes.iter().enumerate() {
                        if i > 0 {
                            text.push(' ');
                        }
                        let _ = write!(text, "{byte:02X}");
                    }
                    text
                }
                Ok(bytes) if app.workbench.value_format == "json" => {
                    match serde_json::from_slice::<Value>(&bytes) {
                        Ok(value) => serde_json::to_string_pretty(&value).unwrap_or_default(),
                        Err(error) => format!("JSON 解析失败：{error}"),
                    }
                }
                _ => value_text(value, true).into_owned(),
            }
        } else {
            value_text(value, true).into_owned()
        };
        panel = panel.child(html::pre(text).class("value-content"));
    }
    panel.into()
}

fn cli_panel(app: &App) -> Node<Msg> {
    let mut panel = html::div().class("cli-panel").child(
        html::div()
            .class("toolbar")
            .child(html::strong("CLI"))
            .child(button("执行", "cli-run"))
            .child(button("取消", "cancel"))
            .child(button("上一页", "cli-previous"))
            .child(button("下一页", "cli-next"))
            .child(button("收起", "cli-toggle")),
    );
    panel = panel.child(field(app, "cli_text", "命令"));
    if app.workbench.cli_running {
        panel = panel.child(html::span("执行中…").class("muted"));
    }
    if !app.workbench.cli_error.is_empty() {
        panel = panel.child(html::span(app.workbench.cli_error.clone()).class("empty-note"));
    }
    for (i, _) in app.workbench.cli_results.iter().enumerate() {
        panel = panel.child(button(
            &format!("结果 {}", i + 1),
            &format!("cli-result:{i}"),
        ));
    }
    if let Some(grid) = &app.workbench.cli_grid {
        let mut result = html::div().class("cli-results");
        let mut header = html::div().class("gridrow header");
        for col in &grid.page.result.columns {
            header = header.child(html::span(col.name.clone()).class("cli-cell"));
        }
        result = result.child(header);
        for (i, row) in grid.page.rows.iter().enumerate() {
            let row_id = grid.page.start + i as u64;
            let mut line = html::div().class("gridrow");
            for (col, value) in row.iter().enumerate() {
                let cell = Cell {
                    row: row_id,
                    column: col,
                };
                let value = grid.value(cell).unwrap_or(value);
                let label = match value {
                    datapier_core::model::Value::Large { bytes, .. } => {
                        format!("大值（{bytes} 字节），点击读取")
                    }
                    _ => cell_preview(app, value),
                };
                line = line.child(
                    html::button(label)
                        .class("cli-cell")
                        .on_click(Msg::Action(format!("cli-open:{row_id}:{col}"))),
                );
            }
            result = result.child(line);
        }
        panel = panel.child(result);
    }
    panel.into()
}

pub fn render(app: &App) -> View<Msg> {
    let redis = app.is_redis();
    let mut context = html::div()
        .class("context")
        .child(button(
            if app.navigation_compact() {
                "›"
            } else {
                "‹"
            },
            "navigation-toggle",
        ))
        .child(html::strong(
            app.profile
                .as_ref()
                .map(|p| p.name.as_str())
                .unwrap_or("未选择连接"),
        ))
        .child(
            html::span(if redis {
                format!(" / DB {}", app.profile.as_ref().map_or(0, |p| p.redis_db))
            } else {
                format!(" / {}", app.database.as_deref().unwrap_or("未选择数据库"))
            })
            .class("muted"),
        )
        .child(
            html::span(if app.session.is_some() {
                "已连接"
            } else {
                "未连接"
            })
            .class("muted"),
        )
        .child(html::span("").class("spacer"))
        .child(button(
            if app.workbench.dark {
                "浅色"
            } else {
                "深色"
            },
            "theme-toggle",
        ))
        .child(button("连接设置", "edit-profile"));
    if app.session.is_some() && !redis {
        context = context.child(button("数据库", "databases"));
    }
    let mut tab_strip = html::div().class("tab-strip").key("tab-strip");
    for (i, t) in app.tabs.iter().enumerate() {
        tab_strip = tab_strip.child(
            html::button(t.title.clone())
                .class(if i == app.active {
                    "button active"
                } else {
                    "button"
                })
                .key(format!("tab{i}"))
                .on_click(Msg::Tab(i)),
        );
    }
    let tabs = html::div()
        .class("tabs")
        .child(tab_strip)
        .child(button("+ 查询", "view-query"))
        .child(button("关闭标签", "close-tab"))
        .child(button("历史", "history"));
    let mut content = html::div()
        .class("content")
        .key("content")
        .child(context)
        .child(tabs);
    if app.save_blocked {
        content = content.child(
            html::div()
                .class("draftbar")
                .child(html::span("工作区保存冲突"))
                .child(button("另存工作区副本", "workspace-copy")),
        );
    }
    if app.view == "history" {
        content = content.child(history_panel(app));
    } else if app.view == "profile" {
        content = content.child(overlay(app, "profile"));
    } else {
        let mut body = html::div().class("detail-panel");
        if !redis && (app.target.is_none() || (app.view == "query" && !app.browse_result)) {
            let mut tools = html::div().class("toolbar");
            for (label, action) in [
                ("执行", "run"),
                ("执行选区", "selection"),
                ("当前语句", "current"),
                ("取消", "cancel"),
                ("查询文件", "overlay:file"),
            ] {
                tools = tools.child(button(label, action));
            }
            body = body.child(tools).child(query(app)).child(completions(app));
        } else if redis && app.target.is_none() {
            let mut empty = html::div()
                .class("empty-state")
                .child(html::strong("选择一个 key"))
                .child(html::span("从左侧已加载的 key 中选择，查看类型和值。"));
            if app
                .profile
                .as_ref()
                .is_some_and(|p| p.access_mode == datapier_core::model::AccessMode::ReadWrite)
            {
                empty = empty.child(button("导入数据库", "overlay:transfer"));
            }
            body = body.child(empty);
        } else if redis {
            let title = app
                .target
                .as_ref()
                .map(|t| format!("{}  ·  {}", t.name, t.kind))
                .unwrap_or_default();
            let mut tools = html::div().class("toolbar").child(html::strong(title));
            if let Some(schema) = &app.schema {
                tools = tools.child(
                    html::span(format!(
                        "TTL {}",
                        ttl_label(schema.get("ttl_ms").and_then(Value::as_i64))
                    ))
                    .class("muted"),
                );
            }
            tools = tools.child(html::span("").class("spacer"));
            if app.target.as_ref().is_some_and(|t| t.kind == "string") {
                for (label, action) in [
                    ("Text", "format:text"),
                    ("JSON", "format:json"),
                    ("HEX", "format:hex"),
                ] {
                    tools = tools.child(button(label, action));
                }
            }
            if writable(app) {
                tools = tools
                    .child(button("操作 key", "overlay:redis"))
                    .child(button("编辑选中项", "redis-edit"));
            }
            body = body.child(tools);
        } else {
            let mut nav = html::div().class("subnav");
            for (label, action) in [
                ("数据", "section:data"),
                ("结构", "section:structure"),
                ("DDL", "section:ddl"),
            ] {
                nav = nav.child(button(label, action));
            }
            body = body.child(nav);
        }
        if !redis && app.target.is_some() && app.browse_result && app.workbench.section != "data" {
            if app.workbench.section == "ddl" {
                body = body.child(
                    html::pre(
                        app.schema
                            .as_ref()
                            .and_then(|s| s.get("ddl"))
                            .and_then(Value::as_str)
                            .unwrap_or(""),
                    )
                    .class("preview"),
                );
            } else {
                body = body.child(schema_panel(app));
            }
        } else if redis && app.target.is_none() {
        } else {
            let mut tools = html::div()
                .class("toolbar")
                .child(button("上一页", "previous"))
                .child(button("下一页", "next"));
            if app.grid.is_some() {
                tools = tools
                    .child(button("列配置", "overlay:columns"))
                    .child(button("读取单元格", "open-cell"))
                    .child(button("单元格详情", "overlay:cell"))
                    .child(field(app, "row_search", "筛选本页"));
            }
            if !redis && app.target.is_some() && app.browse_result {
                tools = tools.child(button("过滤", "overlay:filters"));
            }
            if writable(app) && !redis && app.grid.as_ref().is_some_and(|g| g.editable) {
                tools = tools
                    .child(button("编辑单元格", "inline-begin"))
                    .child(button("行操作", "overlay:row"));
            }
            tools = tools.child(button("导入 / 导出", "overlay:transfer"));
            body = body.child(tools);
            for (i, id) in app.results.iter().enumerate() {
                body = body.child(
                    html::button(format!("结果 {}", i + 1))
                        .class("button")
                        .key(format!("result{id}"))
                        .on_click(Msg::ResultSet(id.clone())),
                );
            }
            if redis && app.target.as_ref().is_some_and(|t| t.kind == "string") {
                body = body.child(string_value(app));
            } else if let Some(table) = grid(app) {
                body = body.child(table);
            }
        }
        let changes = app.pending_rows.len()
            + app.redis_changes.len()
            + app.grid.as_ref().map_or(0, |g| g.edits().len());
        if changes > 0 {
            let mut bar = html::div()
                .class("draftbar")
                .child(html::span(format!("{changes} 项待提交变更")))
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
                    .child(html::span("已保存的编辑意图"))
                    .child(button("恢复编辑意图", "restore-pending"))
                    .child(button("丢弃", "discard")),
            );
        }
        if redis && app.workbench.cli {
            content = content
                .child(
                    html::div()
                        .class("split")
                        .child(redis_browser(app))
                        .child(body),
                )
                .child(cli_panel(app));
        } else {
            content = if redis {
                content.child(
                    html::div()
                        .class("split")
                        .child(redis_browser(app))
                        .child(body),
                )
            } else {
                content.child(body)
            };
        }
    }
    let mut status = html::div()
        .class(if app.workbench.dark {
            "status"
        } else {
            "status light"
        })
        .child(html::span(app.status.clone()));
    if !app.detail.is_empty() {
        status = status.child(button("详情", "overlay:diagnostics"));
    }
    status = status.child(html::span("").class("spacer"));
    if redis {
        status = status.child(button("CLI", "cli-toggle"));
    }
    let layout = html::div()
        .class(if app.workbench.dark {
            "layout"
        } else {
            "layout light"
        })
        .key("layout")
        .child(resource_panel(app))
        .child(content);
    let layer = app
        .workbench
        .overlay
        .as_deref()
        .map(|kind| {
            vec![Node::from(
                html::div()
                    .class(if app.workbench.dark {
                        "overlay-layer"
                    } else {
                        "overlay-layer light"
                    })
                    .child(overlay(app, kind)),
            )]
        })
        .unwrap_or_default();
    View::new()
        .main(vec![Node::from(layout)])
        .dock(vec![Node::from(status)])
        .layer(layer)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD;
    use datapier_core::model::Value as V;

    #[test]
    fn cli_reply_preserves_unicode_binary_and_large_integer() {
        let reply = V::Json(json!([
            {"type":"bytes","value":STANDARD.encode("中文😀")},
            {"type":"bytes","value":STANDARD.encode([0xff, 0x00])},
            {"type":"integer","value":"9223372036854775807"},
            {"type":"missing"}
        ]));
        assert_eq!(
            value_text(&reply, true),
            "1) 中文😀\n2) base64:/wA=\n3) 9223372036854775807\n4) (nil)"
        );
    }
}
