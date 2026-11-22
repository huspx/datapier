//! 在结构表格中编辑索引、外键和约束。
use super::{
    form::{SchemaChoices, render_value},
    schema_reference,
    view::row_action,
};
use crate::{Editor, Msg, i18n::tr};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tern_sdk::ui::{self, Node, html};

fn candidate_select(
    path: &str,
    label: &str,
    current: &str,
    candidates: &[String],
    disabled: bool,
    open: Option<&str>,
    command: &str,
) -> Node<Msg> {
    let toggle_path = path.to_owned();
    let change_path = path.to_owned();
    let command = command.to_owned();
    let mut select = ui::select()
        .aria(tr(label).into_owned())
        .value(current.to_owned())
        .disabled(disabled)
        .open(open == Some(path))
        .on_toggle(move |collapsed| {
            Msg::Action(format!("schema-op:open:{toggle_path}:{}", !collapsed))
        })
        .option(ui::SelectOption::new("", tr("请选择").into_owned()));
    if !current.is_empty() && !candidates.iter().any(|value| value == current) {
        select = select.option(ui::SelectOption::new(
            current.to_owned(),
            format!("{} · {}", current, tr("当前值未加载或不可见")),
        ));
    }
    for candidate in candidates {
        select = select.option(ui::SelectOption::new(candidate.clone(), candidate.clone()));
    }
    select
        .on_change(move |change| {
            Msg::Action(format!(
                "schema-op:{command}:{change_path}:{}",
                change.value.as_str().unwrap_or_default()
            ))
        })
        .into()
}

fn ordered_columns(
    items: &Value,
    path: &str,
    label: &str,
    candidates: &[String],
    index_parts: bool,
    disabled: bool,
    open: Option<&str>,
) -> Node<Msg> {
    let mut cell = html::div().class("schema-design-ordered-columns");
    let values = items.as_array();
    let count = values.map_or(0, Vec::len);
    for (index, item) in values.into_iter().flatten().enumerate() {
        let value = if index_parts {
            item.get("column").and_then(Value::as_str).unwrap_or("")
        } else {
            item.as_str().unwrap_or("")
        };
        let item_path = format!("{path}/{index}");
        let select_path = if index_parts {
            format!("{item_path}/column")
        } else {
            item_path
        };
        let mut line = html::div()
            .class("schema-design-ordered-column")
            .child(candidate_select(
                &select_path,
                label,
                value,
                candidates,
                disabled,
                open,
                if index_parts {
                    "index-column"
                } else {
                    "choice"
                },
            ));
        if let Some(expression) = item
            .get("expression")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            line = line.child(html::span(expression.to_owned()));
        }
        for (title, icon, action, unavailable) in [
            ("前移", "chev-up", "up", index == 0),
            ("后移", "chev", "down", index + 1 == count),
            ("删除", "trash", "remove", false),
        ] {
            line = line.child(row_action(
                title,
                icon,
                format!("schema-op:{action}:{path}:{index}"),
                disabled || unavailable,
            ));
        }
        cell = cell.child(line);
    }
    cell = cell.child(row_action(
        "添加列",
        "plus",
        format!("schema-op:add:{path}:item"),
        disabled,
    ));
    cell.into()
}

pub(super) struct SchemaGridContext<'a> {
    pub(super) references: &'a schema_reference::ReferenceState,
    pub(super) database: &'a str,
    pub(super) table_name: &'a str,
}

pub(super) fn render_object_grid(
    root: &Value,
    section: &str,
    editors: &mut BTreeMap<String, Editor>,
    disabled: bool,
    selected_id: Option<&str>,
    open_select: Option<&str>,
    context: SchemaGridContext<'_>,
) -> Node<Msg> {
    let SchemaGridContext {
        references,
        database,
        table_name,
    } = context;
    let (path, kind, fields): (&str, &str, &[(&str, &str)]) = match section {
        "indexes" => (
            "/indexes",
            "",
            &[
                ("name", "名称"),
                ("kind", "种类"),
                ("@table", "表"),
                ("parts", "列"),
                ("visible", "可见"),
            ],
        ),
        "foreign_keys" => (
            "/constraints",
            "foreign_key",
            &[
                ("name", "名称"),
                ("columns", "本地列"),
                ("referenced_database", "引用数据库"),
                ("referenced_table", "引用表"),
                ("referenced_columns", "引用列"),
                ("on_update", "更新时"),
                ("on_delete", "删除时"),
            ],
        ),
        _ => (
            "/constraints",
            "check",
            &[
                ("name", "名称"),
                ("expression", "表达式"),
                ("enforced", "启用约束"),
            ],
        ),
    };
    let items = root.pointer(path).and_then(Value::as_array);
    let matching: Vec<_> = items
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, item)| {
            kind.is_empty() || item.get("kind").and_then(Value::as_str) == Some(kind)
        })
        .collect();
    let selected = selected_id
        .and_then(|id| {
            matching
                .iter()
                .position(|(_, item)| item.get("id").and_then(Value::as_str) == Some(id))
        })
        .or_else(|| (!matching.is_empty()).then_some(0));
    let mut grid = html::div().class("schema-design-object-grid");
    let mut header = html::div().class(format!(
        "schema-design-object-row schema-design-object-{section} schema-design-column-header"
    ));
    for (_, label) in fields {
        header = header.child(html::strong(tr(label).into_owned()));
    }
    header = header.child(html::strong(tr("操作").into_owned()));
    grid = grid.child(header);
    let local_columns: Vec<String> = root["columns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|column| {
            column["name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .collect();
    for (position, (index, item)) in matching.iter().enumerate() {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
        let referenced_database = item
            .get("referenced_database")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(database);
        let referenced_table = item
            .get("referenced_table")
            .and_then(Value::as_str)
            .unwrap_or("");
        let entry = references.entries.get(id).filter(|entry| {
            entry.database == referenced_database && entry.table == referenced_table
        });
        let self_reference = referenced_database == database
            && referenced_table == table_name
            && !table_name.is_empty();
        let mut row = html::div()
            .class(format!(
                "schema-design-object-row schema-design-object-{section}{}",
                if selected == Some(position) {
                    " selected"
                } else {
                    ""
                }
            ))
            .key(id.to_owned())
            .on_click(Msg::Action(format!("schema-op:item:{section}:{id}")));
        for (key, label) in fields {
            let field_path = format!("{path}/{index}/{key}");
            let mut cell = html::div().class("schema-design-object-cell");
            if *key == "@table" {
                cell = cell.child(html::span(table_name.to_owned()));
                row = row.child(cell);
                continue;
            }
            if matches!(*key, "parts" | "columns" | "referenced_columns") {
                let candidates = if *key == "referenced_columns" {
                    if self_reference {
                        local_columns.as_slice()
                    } else {
                        entry.map_or(&[][..], |entry| entry.columns.as_slice())
                    }
                } else {
                    local_columns.as_slice()
                };
                let empty = json!([]);
                cell = cell.child(ordered_columns(
                    item.get(*key).unwrap_or(&empty),
                    &field_path,
                    label,
                    candidates,
                    *key == "parts",
                    disabled,
                    open_select,
                ));
                if *key == "referenced_columns" && !self_reference {
                    let loading = entry.is_some_and(|entry| entry.columns_loading);
                    let complete = entry.is_some_and(|entry| entry.columns_complete);
                    if let Some(error) = entry.and_then(|entry| entry.columns_error.as_deref()) {
                        cell = cell.child(html::p(tr(error).into_owned()).class("field-error"));
                    }
                    if loading {
                        cell = cell.child(html::span(tr("正在加载引用列").into_owned()));
                    }
                    if !complete && !loading {
                        cell = cell.child(
                            ui::button(tr("加载引用列").into_owned())
                                .disabled(disabled || referenced_table.is_empty())
                                .on_click(Msg::Action(format!("schema-ref:load:{id}:columns"))),
                        );
                    }
                }
                row = row.child(cell);
                continue;
            }
            if *key == "referenced_table" {
                let mut candidates = entry.map_or_else(Vec::new, |entry| entry.tables.clone());
                if referenced_database == database
                    && !table_name.is_empty()
                    && !candidates.iter().any(|value| value == table_name)
                {
                    candidates.push(table_name.to_owned());
                }
                cell = cell.child(candidate_select(
                    &field_path,
                    label,
                    referenced_table,
                    &candidates,
                    disabled,
                    open_select,
                    "choice",
                ));
                let loading = entry.is_some_and(|entry| entry.tables_loading);
                let complete = entry.is_some_and(|entry| entry.tables_complete);
                if let Some(error) = entry.and_then(|entry| entry.tables_error.as_deref()) {
                    cell = cell.child(html::p(tr(error).into_owned()).class("field-error"));
                }
                if loading {
                    cell = cell.child(html::span(tr("正在加载引用表").into_owned()));
                }
                if !complete && !loading {
                    cell = cell.child(
                        ui::button(tr("加载引用表").into_owned())
                            .disabled(disabled || referenced_database.is_empty())
                            .on_click(Msg::Action(format!("schema-ref:load:{id}:tables"))),
                    );
                }
                row = row.child(cell);
                continue;
            }
            if let Some(value) = item.get(*key) {
                if let Some(on) = value.as_bool() {
                    let toggle_path = field_path.clone();
                    let change_path = field_path.clone();
                    cell = cell.child(
                        ui::select()
                            .aria(tr(label).into_owned())
                            .value(if on { "true" } else { "false" })
                            .open(open_select == Some(field_path.as_str()))
                            .disabled(disabled)
                            .option(ui::SelectOption::new("true", tr("是").into_owned()))
                            .option(ui::SelectOption::new("false", tr("否").into_owned()))
                            .on_toggle(move |collapsed| {
                                Msg::Action(format!("schema-op:open:{toggle_path}:{}", !collapsed))
                            })
                            .on_change(move |change| {
                                Msg::Action(format!(
                                    "schema-op:choice:{change_path}:{}",
                                    change.value.as_str().unwrap_or_default()
                                ))
                            }),
                    );
                } else {
                    cell = cell.child(render_value(
                        value,
                        &field_path,
                        editors,
                        disabled,
                        true,
                        None,
                        SchemaChoices {
                            open_select,
                            root: Some(root),
                        },
                    ));
                }
            }
            if *key == "name" {
                cell = cell.child(
                    ui::button(tr("扩展属性").into_owned())
                        .on_click(Msg::Action(format!("schema-op:item:{section}:{id}"))),
                );
            }
            row = row.child(cell);
        }
        let mut actions = html::div().class("schema-design-row-actions");
        for (label, icon, target) in [
            ("前移", "chev-up", position.checked_sub(1)),
            (
                "后移",
                "chev",
                (position + 1 < matching.len()).then_some(position + 1),
            ),
        ] {
            let destination = target.map(|n| matching[n].0).unwrap_or(*index);
            actions = actions.child(row_action(
                label,
                icon,
                format!("schema-op:move:{path}:{index},{destination}"),
                disabled || target.is_none(),
            ));
        }
        actions = actions.child(row_action(
            "删除",
            "trash",
            format!("schema-op:remove:{path}:{index}"),
            disabled,
        ));
        row = row.child(actions);
        grid = grid.child(row);
    }
    let add_kind = if kind.is_empty() { "item" } else { kind };
    let mut content = html::div().class("advanced-fields").child(grid).child(
        html::div().class("advanced-actions").child(row_action(
            "新增",
            "plus",
            format!("schema-op:add:{path}:{add_kind}"),
            disabled,
        )),
    );
    if let Some(position) = selected {
        let (index, item) = matching[position];
        let mut properties = item.as_object().cloned().unwrap_or_default();
        for (key, _) in fields {
            if *key != "parts" {
                properties.remove(*key);
            }
        }
        properties.remove("kind");
        let mut detail = html::div()
            .class("schema-design-column-details")
            .child(html::strong(tr("选中项扩展属性").into_owned()))
            .child(html::span(
                item.get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("—")
                    .to_owned(),
            ));
        if properties.keys().any(|key| key != "id") {
            detail = detail.child(render_value(
                &Value::Object(properties),
                &format!("{path}/{index}"),
                editors,
                disabled,
                true,
                None,
                SchemaChoices {
                    open_select,
                    root: Some(root),
                },
            ));
        }
        content = content.child(detail);
    }
    content.into()
}
