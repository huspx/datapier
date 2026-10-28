use super::*;

fn button(label: &str, action: &str) -> Node<Msg> {
    html::button(label)
        .class("button")
        .on_click(Msg::Action(action.into()))
        .into()
}
fn cell_preview(value: &datapier_core::model::Value) -> String {
    use datapier_core::model::Value as V;
    let text = match value {
        V::Text(s)
        | V::Bytes(s)
        | V::Integer(s)
        | V::Unsigned(s)
        | V::Decimal(s)
        | V::Float(s)
        | V::Date(s)
        | V::Time(s)
        | V::DateTime(s) => s.as_str(),
        V::Large { preview, .. } => preview.as_str(),
        _ => return value.text(),
    };
    let mut chars = text.chars();
    let mut preview: String = chars.by_ref().take(256).collect();
    if chars.next().is_some() {
        preview.push('…');
    }
    if matches!(value, V::Bytes(_)) {
        preview.insert_str(0, "base64:");
    }
    preview
}
fn field(app: &App, name: &str, label: &str) -> Node<Msg> {
    let ed = &app.fields[name];
    let display = if name == "password" {
        "•".repeat(ed.text.encode_utf16().count())
    } else {
        ed.text.clone()
    };
    let f = name.to_string();
    let u = f.clone();
    let n = f.clone();
    if name == "cell" {
        html::div()
            .class("field")
            .key(name.to_string())
            .child(html::span(label).class("label"))
            .child(
                ui::editor()
                    .key("input")
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
                    .key("input")
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
        .key("query")
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
    let mut panel = html::div()
        .class("sidebar")
        .key("sidebar")
        .child(html::strong("连接"));
    panel = panel
        .child(button("新建", "new-profile"))
        .child(button("编辑", "edit-profile"));
    for (i, p) in app.profiles.iter().enumerate() {
        panel = panel.child(
            html::button(format!("{} · {}", p.name, p.host))
                .class("resource")
                .key(format!("p{}", p.id))
                .on_click(Msg::Profile(i)),
        );
    }
    panel = panel
        .child(button("连接", "open"))
        .child(button("删除", "delete-profile"));
    if app.session.is_some() {
        panel = panel
            .child(html::strong("资源"))
            .child(field(app, "pattern", "SCAN pattern"))
            .child(field(app, "type", "类型过滤"));
        panel = panel
            .child(button("刷新", "list"))
            .child(button("下一批", "more"));
        for (i, r) in app.resources.iter().enumerate() {
            panel = panel.child(
                html::button(format!("{}  {}", r.kind, r.name))
                    .class("resource")
                    .key(format!("r{i}"))
                    .on_click(Msg::Resource(i)),
            );
        }
        panel = panel
            .child(button("查看结构", "describe"))
            .child(button("浏览", "browse"))
            .child(button("打开编辑基线", "edit-open"));
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
    for (i, row) in grid.page.rows.iter().enumerate() {
        let row_id = grid.page.start + i as u64;
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
            let value = if matches!(
                row.get(col),
                Some(datapier_core::model::Value::Large { .. })
            ) && !grid.edits().contains_key(&cell)
            {
                row.get(col)
            } else {
                grid.value(cell).or_else(|| row.get(col))
            };
            let text = value.map(cell_preview).unwrap_or_default();
            let selected = grid.is_selected(cell);
            let class = if selected {
                format!("cell col{col} selected")
            } else {
                format!("cell col{col}")
            };
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
        wrapper = wrapper.child(line);
    }
    Some(wrapper.into())
}
pub fn stylesheet(app: &App) -> String {
    let mut css = String::from(
        ".layout { display:flex; gap:8px; height:100%; } .sidebar { width:240px; overflow:auto; flex-shrink:0; } .content { flex:1; min-width:0; overflow:auto; } .resource { display:block; width:100%; text-align:left; padding:3px; } .button { border:1px solid #888; border-radius:4px; padding:4px 7px; margin:2px; } .field { display:flex; gap:6px; padding:3px; } .label { width:120px; } .gridscroll { overflow:auto; max-height:55vh; } .gridrow { display:flex; min-width:max-content; } .header { position:sticky; top:0; background:#333; color:white; z-index:3; } .cell { display:block; flex-shrink:0; overflow:hidden; white-space:nowrap; border:1px solid #777; padding:3px; text-align:left; } .rownum { width:48px; flex-shrink:0; } .selected { background:#426a9e; color:white; } .status { padding:5px; } ",
    );
    if let Some(g) = &app.grid {
        for (i, s) in g.settings.iter().enumerate() {
            let sticky = g
                .frozen_offset(i)
                .map(|x| {
                    format!(
                        "position:sticky; left:{}px; background:#555; z-index:2;",
                        x + 48
                    )
                })
                .unwrap_or_default();
            css.push_str(&format!(".col{i} {{ width:{}px; {sticky} }}", s.width));
        }
    }
    css
}
pub fn render(app: &App) -> View<Msg> {
    let mut content = html::div().class("content").key("content");
    content = content
        .child(button("查询", "view-query"))
        .child(button("编辑", "view-edit"))
        .child(button("导入 / 导出", "view-transfer"));
    if app.save_blocked {
        content = content.child(html::p(
            "工作区保存冲突；原工作区未覆盖。可将当前全部标签与草稿另存为副本。",
        ));
        content = content.child(button("另存工作区副本", "workspace-copy"));
    }
    match app.view.as_str() {
        "profile" => {
            for (name, label) in [
                ("name", "名称"),
                ("provider", "类型 mysql/redis"),
                ("host", "主机"),
                ("port", "端口"),
                ("user", "用户名"),
                ("password", "密码"),
                ("database", "MySQL 数据库"),
                ("redis_db", "Redis DB"),
                ("access", "read_only/read_write"),
                ("tls", "TLS true/false"),
                ("ca", "CA 路径"),
                ("cert", "证书路径"),
                ("key", "私钥路径"),
            ] {
                content = content.child(field(app, name, label));
            }
            content = content
                .child(button("测试连接", "test-profile"))
                .child(button("保存连接", "save-profile"));
        }
        "transfer" => {
            for (name, label) in [
                ("path", "文件路径"),
                ("format", "csv/json"),
                ("mapping", "列映射 JSON"),
                ("null_marker", "NULL 标记"),
                ("backslash_escape", "反斜线转义 true/false"),
                ("binary_columns", "二进制列 JSON 数组"),
            ] {
                content = content.child(field(app, name, label));
            }
            content = content
                .child(button("预览导入", "preview-import"))
                .child(button("确认导入", "import"))
                .child(button("导出完整结果", "export"));
        }
        "edit" => {
            for (name, label) in [
                ("cell", "单元格文本/JSON"),
                ("row_values", "新增行类型化 JSON"),
                ("column", "列序号"),
                ("width", "列宽"),
            ] {
                content = content.child(field(app, name, label));
            }
            for (label, action) in [
                ("文本", "set-cell"),
                ("JSON", "set-json"),
                ("NULL", "set-null"),
                ("新增行草稿", "insert-row"),
                ("删除选中行草稿", "delete-row"),
                ("撤销行草稿", "undo-row"),
                ("确认粘贴预览", "paste-confirm"),
                ("设置列宽", "column-width"),
                ("隐藏/显示列", "column-hide"),
                ("冻结/解冻列", "column-freeze"),
                ("撤销变更", "undo-grid"),
                ("丢弃", "discard"),
                ("提交行变更", "submit"),
            ] {
                content = content.child(button(label, action));
            }
            content = content.child(html::strong("Redis 操作（字节可写为 base64:…）"));
            for (label, op) in [
                ("String 设置", "set_string"),
                ("Hash 设置", "hash_set"),
                ("Hash 删除", "hash_delete"),
                ("List 设置", "list_set"),
                ("List 插入", "list_insert"),
                ("List 删除", "list_delete"),
                ("Set 添加", "set_add"),
                ("Set 删除", "set_remove"),
                ("ZSet 设置", "zset_set"),
                ("ZSet 删除", "zset_remove"),
                ("重命名 key", "rename"),
                ("删除 key", "delete_key"),
                ("设置/移除 TTL", "set_ttl"),
            ] {
                content = content.child(button(label, &format!("redis-op:{op}")));
            }
            for (name, label) in [
                ("operation", "操作"),
                ("redis_value", "值"),
                ("redis_field", "Hash field"),
                ("redis_member", "集合 member"),
                ("redis_index", "List index"),
                ("redis_score", "ZSet score"),
                ("redis_key", "新 key"),
                ("redis_ttl", "TTL 毫秒（空=永久）"),
            ] {
                content = content.child(field(app, name, label));
            }
            content = content
                .child(button("加入变更预览", "redis-stage"))
                .child(button("撤销 Redis 变更", "redis-undo"))
                .child(button("提交 Redis 操作", "redis-submit"));
        }
        _ => {
            let mut tabs = html::div().class("tabs");
            for (i, t) in app.tabs.iter().enumerate() {
                tabs = tabs.child(
                    html::button(t.title.clone())
                        .class("button")
                        .key(format!("tab{i}"))
                        .on_click(Msg::Tab(i)),
                );
            }
            content = content
                .child(tabs)
                .child(button("新建 tab", "new-tab"))
                .child(button("关闭 tab", "close-tab"))
                .child(query(app))
                .child(completions(app));
            for (label, action) in [
                ("执行全文", "run"),
                ("执行选区", "selection"),
                ("执行当前语句", "current"),
                ("取消", "cancel"),
                ("历史", "history"),
                ("打开文件", "file-open"),
                ("保存文件", "file-save"),
            ] {
                content = content.child(button(label, action));
            }
            content = content
                .child(field(app, "path", "查询文件路径"))
                .child(field(app, "filter", "过滤 JSON"))
                .child(field(app, "sort", "排序 JSON"));
        }
    }
    if !app.tabs[app.active].pending.is_null()
        && app.pending_rows.is_empty()
        && app.redis_changes.is_empty()
        && app.grid.as_ref().is_none_or(|g| g.edits().is_empty())
    {
        content = content.child(button("确认恢复编辑意图", "restore-pending"));
    }
    for (i, id) in app.results.iter().enumerate() {
        content = content.child(
            html::button(format!("结果 {}", i + 1))
                .class("button")
                .key(format!("result{id}"))
                .on_click(Msg::ResultSet(id.clone())),
        );
    }
    if let Some(grid) = grid(app) {
        content = content
            .child(button("上一页", "previous"))
            .child(button("下一页", "next"))
            .child(button("打开单元格 / 读取大字段", "open-cell"))
            .child(grid);
    }
    content = content.child(html::pre(app.detail.clone()).class("details"));
    let layout = html::div()
        .class("layout")
        .key("layout")
        .child(resource_panel(app))
        .child(content);
    View::new()
        .main(vec![Node::from(layout)])
        .dock(vec![Node::from(
            html::div().class("status").text(app.status.clone()),
        )])
}
