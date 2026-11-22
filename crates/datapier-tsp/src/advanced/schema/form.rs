//! 结构定义字段、类型参数与递归表单控件。
use super::{
    COLUMN_TYPE_CHOICES, Editor, parse_parameter_text, template, type_parameter_text,
    type_parameters, view::row_action,
};
use crate::{Msg, i18n::tr};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use tern_sdk::ui::{self, Node, html};

fn field_label(key: &str) -> &str {
    match key {
        "columns" => "字段",
        "indexes" => "索引",
        "constraints" => "约束",
        "name" => "名称",
        "native_type" => "类型",
        "@length" => "长度",
        "nullable" => "允许 NULL",
        "default" => "默认值",
        "value" => "值",
        "on_update" => "更新时",
        "auto_increment" => "自增",
        "generated" => "生成列",
        "expression" => "表达式",
        "stored" => "存储生成列",
        "charset" => "字符集",
        "collation" => "排序规则",
        "visible" => "可见",
        "comment" => "注释",
        "extra" => "其他属性",
        "kind" => "种类",
        "parts" => "索引列",
        "column" => "字段",
        "prefix_length" => "前缀长度",
        "descending" => "降序",
        "method" => "索引方法",
        "options" => "选项",
        "referenced_database" => "引用数据库",
        "referenced_table" => "引用表",
        "referenced_columns" => "引用字段",
        "on_delete" => "删除时",
        "enforced" => "启用约束",
        "engine" => "存储引擎",
        "select" => "SELECT 正文",
        "algorithm" => "ALGORITHM",
        "definer" => "DEFINER",
        "security" => "SQL SECURITY",
        "check_option" => "CHECK OPTION",
        "character_set_client" => "创建字符集",
        "collation_connection" => "创建排序规则",
        "database" => "数据库",
        "table" => "表",
        "timing" => "时机",
        "event" => "事件",
        "body" => "SQL 正文",
        "sql_mode" => "SQL MODE",
        "order" => "触发顺序",
        "follows" => "跟随",
        "trigger" => "参照触发器",
        _ => key,
    }
}

fn schema_choices(path: &str) -> &'static [(&'static str, &'static str)] {
    if path.ends_with("/default/kind") {
        &[
            ("unspecified", "未指定"),
            ("null", "NULL"),
            ("literal", "字面量"),
            ("expression", "表达式"),
        ]
    } else if path.contains("/indexes/") && path.ends_with("/kind") {
        &[
            ("unique", "唯一索引"),
            ("normal", "普通索引"),
            ("fulltext", "全文索引"),
            ("spatial", "空间索引"),
        ]
    } else if path == "/timing" {
        &[("BEFORE", "之前"), ("AFTER", "之后")]
    } else if path == "/event" {
        &[("INSERT", "插入"), ("UPDATE", "更新"), ("DELETE", "删除")]
    } else if path == "/security" {
        &[("DEFINER", "定义者"), ("INVOKER", "调用者")]
    } else if path == "/algorithm" {
        &[
            ("UNDEFINED", "未指定"),
            ("MERGE", "合并"),
            ("TEMPTABLE", "临时表"),
        ]
    } else if path == "/check_option" {
        &[("LOCAL", "本地"), ("CASCADED", "级联")]
    } else if path.contains("/constraints/")
        && (path.ends_with("/on_update") || path.ends_with("/on_delete"))
    {
        &[
            ("RESTRICT", "限制"),
            ("CASCADE", "级联"),
            ("SET NULL", "设为 NULL"),
            ("NO ACTION", "不操作"),
        ]
    } else {
        &[]
    }
}

#[derive(Clone, Copy)]
pub(super) struct SchemaChoices<'a> {
    pub(super) open_select: Option<&'a str>,
    pub(super) root: Option<&'a Value>,
}

pub(super) fn render_value(
    value: &Value,
    path: &str,
    editors: &mut BTreeMap<String, Editor>,
    disabled: bool,
    table: bool,
    selected_column: Option<&str>,
    choices: SchemaChoices<'_>,
) -> Node<Msg> {
    let SchemaChoices { open_select, root } = choices;
    let mut container = html::div().class("advanced-fields");
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if key == "id" {
                    continue;
                }
                let child = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
                let locked_identity = path.is_empty()
                    && map.get("kind").and_then(Value::as_str) == Some("trigger")
                    && matches!(key.as_str(), "name" | "database");
                let creation_context = path.is_empty()
                    && matches!(
                        map.get("kind").and_then(Value::as_str),
                        Some("view" | "trigger")
                    )
                    && matches!(
                        key.as_str(),
                        "sql_mode" | "character_set_client" | "collation_connection"
                    );
                if let Value::Bool(on) = value {
                    let action = format!("schema-op:toggle:{child}");
                    container = container.child(
                        html::div().class("schema-design-boolean").child(
                            ui::switch(tr(field_label(key)).into_owned())
                                .on(*on)
                                .disabled(disabled || locked_identity)
                                .on_change(move |_| Msg::Action(action.clone())),
                        ),
                    );
                    continue;
                }
                let mut field = html::div()
                    .class(if path.is_empty() && table {
                        "advanced-field schema-design-section"
                    } else {
                        "advanced-field"
                    })
                    .child(html::strong(tr(field_label(key)).into_owned()))
                    .child(render_value(
                        value,
                        &child,
                        editors,
                        disabled || locked_identity || creation_context,
                        table,
                        selected_column,
                        SchemaChoices { open_select, root },
                    ));
                if !value.is_null()
                    && (matches!(
                        key.as_str(),
                        "generated"
                            | "charset"
                            | "collation"
                            | "method"
                            | "prefix_length"
                            | "definer"
                            | "check_option"
                            | "order"
                    ) || (key == "on_update" && path.starts_with("/columns/"))
                        || (matches!(key.as_str(), "column" | "expression")
                            && path.contains("/parts/")))
                {
                    field = field.child(
                        ui::button(tr("清除").into_owned())
                            .disabled(disabled || creation_context)
                            .on_click(Msg::Action(format!("schema-op:set:{child}:null"))),
                    );
                }
                container = container.child(field);
            }
        }
        Value::Array(items) => {
            if path == "/columns" && table {
                let selected = selected_column
                    .and_then(|id| {
                        items
                            .iter()
                            .position(|item| item.get("id").and_then(Value::as_str) == Some(id))
                    })
                    .or_else(|| (!items.is_empty()).then_some(0));
                let mut list = html::div().class("schema-design-columns");
                let mut header =
                    html::div().class("schema-design-column-row schema-design-column-header");
                for label in [
                    "名称",
                    "类型",
                    "长度",
                    "允许 NULL",
                    "默认值",
                    "约束",
                    "注释",
                    "操作",
                ] {
                    header = header.child(html::strong(tr(label).into_owned()));
                }
                list = list.child(header);
                for (index, item) in items.iter().enumerate() {
                    let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                    let mut primary = false;
                    let mut foreign = false;
                    for index in root
                        .and_then(|v| v.get("indexes"))
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if !name.is_empty()
                            && index
                                .get("parts")
                                .and_then(Value::as_array)
                                .is_some_and(|parts| {
                                    parts.iter().any(|part| {
                                        part.get("column").and_then(Value::as_str) == Some(name)
                                    })
                                })
                            && index.get("kind").and_then(Value::as_str) == Some("primary")
                        {
                            primary = true;
                        }
                    }
                    for constraint in root
                        .and_then(|v| v.get("constraints"))
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if !name.is_empty()
                            && constraint.get("kind").and_then(Value::as_str) == Some("foreign_key")
                            && constraint
                                .get("columns")
                                .and_then(Value::as_array)
                                .is_some_and(|columns| {
                                    columns.iter().any(|column| column.as_str() == Some(name))
                                })
                        {
                            foreign = true;
                        }
                    }
                    let mut row = html::div()
                        .class(if selected == Some(index) {
                            "schema-design-column-row selected"
                        } else {
                            "schema-design-column-row"
                        })
                        .key(id.to_owned())
                        .on_click(Msg::Action(format!("schema-op:column:{id}:")));
                    if let Some(value) = item.get("name") {
                        row = row.child(render_value(
                            value,
                            &format!("/columns/{index}/name"),
                            editors,
                            disabled,
                            table,
                            selected_column,
                            SchemaChoices { open_select, root },
                        ));
                    }
                    let type_path = format!("/columns/{index}/native_type");
                    let native_type = item
                        .get("native_type")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let params = type_parameters(native_type);
                    let family = native_type
                        .trim_start()
                        .split(|c: char| !c.is_ascii_alphabetic())
                        .next()
                        .unwrap_or("");
                    let current = COLUMN_TYPE_CHOICES
                        .iter()
                        .find(|(choice, _)| family.eq_ignore_ascii_case(choice))
                        .map_or(native_type, |(choice, _)| *choice);
                    let toggle_path = type_path.clone();
                    let change_path = type_path.clone();
                    let mut type_select = ui::select()
                        .aria(tr("类型").into_owned())
                        .value(current)
                        .open(open_select == Some(type_path.as_str()))
                        .disabled(disabled);
                    if !COLUMN_TYPE_CHOICES
                        .iter()
                        .any(|(choice, _)| *choice == current)
                    {
                        type_select = type_select.option(ui::SelectOption::new(current, current));
                    }
                    for (choice, _) in COLUMN_TYPE_CHOICES {
                        let label = if *choice == current {
                            params.as_ref().map_or_else(
                                || native_type.to_owned(),
                                |params| format!("{}{}", params.name, params.suffix),
                            )
                        } else {
                            (*choice).to_owned()
                        };
                        type_select = type_select.option(ui::SelectOption::new(*choice, label));
                    }
                    row = row.child(
                        type_select
                            .on_toggle(move |collapsed| {
                                Msg::Action(format!("schema-op:open:{toggle_path}:{}", !collapsed))
                            })
                            .on_change(move |change| {
                                Msg::Action(format!(
                                    "schema-op:column-type:{change_path}:{}",
                                    change.value.as_str().unwrap_or_default()
                                ))
                            }),
                    );
                    let mut length_cell = html::div();
                    if let Some(params) = params {
                        if params.fractional_seconds {
                            length_cell =
                                length_cell.attr("title", tr("小数秒精度（0–6）").into_owned());
                        }
                        let length_path = format!("{type_path}/@length");
                        length_cell = length_cell.child(render_value(
                            &Value::String(type_parameter_text(&params)),
                            &length_path,
                            editors,
                            disabled,
                            table,
                            selected_column,
                            SchemaChoices { open_select, root },
                        ));
                        if editors.contains_key(&format!("{type_path}/@parameters"))
                            && let Some(editor) = editors.get(&length_path)
                            && let Err(error) = parse_parameter_text(&params, &editor.text)
                        {
                            length_cell =
                                length_cell.child(html::p(tr(error).into_owned()).class("notice"));
                        }
                    }
                    row = row.child(length_cell);
                    let nullable_path = format!("/columns/{index}/nullable");
                    let change_path = nullable_path.clone();
                    let nullable = html::checkbox(format!("schema-nullable-{id}"), "true")
                        .class("schema-design-nullable")
                        .aria(tr("允许 NULL").into_owned())
                        .checked(item.get("nullable") == Some(&Value::Bool(true)))
                        .disabled(disabled || primary)
                        .on_change(move |change| {
                            Msg::Action(format!(
                                "schema-op:choice:{change_path}:{}",
                                change.checked == Some(true)
                            ))
                        });
                    row = row.child(nullable);
                    let mut default_cell = html::div().class("schema-design-default");
                    if let Some(default) = item.get("default") {
                        if let Some(kind) = default.get("kind") {
                            default_cell = default_cell.child(render_value(
                                kind,
                                &format!("/columns/{index}/default/kind"),
                                editors,
                                disabled,
                                table,
                                selected_column,
                                SchemaChoices { open_select, root },
                            ));
                        }
                        if let Some(value) = default.get("value") {
                            default_cell = default_cell.child(render_value(
                                value,
                                &format!("/columns/{index}/default/value"),
                                editors,
                                disabled,
                                table,
                                selected_column,
                                SchemaChoices { open_select, root },
                            ));
                        }
                    }
                    let pk_label = tr(if primary {
                        "取消主键"
                    } else {
                        "设为主键"
                    })
                    .into_owned();
                    let mut badges = html::div().class("schema-design-column-constraints");
                    if primary {
                        badges = badges.child(
                            html::span("")
                                .attr("role", "img")
                                .aria(tr("主键").into_owned())
                                .title(tr("主键").into_owned())
                                .child(ui::icon("key")),
                        );
                    }
                    if foreign {
                        badges = badges.child(
                            html::span("")
                                .attr("role", "img")
                                .aria(tr("外键").into_owned())
                                .title(tr("外键").into_owned())
                                .child(ui::icon("link")),
                        );
                    }
                    if !primary && !foreign {
                        badges = badges.child(html::span("—"));
                    }
                    row = row.child(default_cell).child(badges);
                    if let Some(value) = item.get("comment") {
                        row = row.child(render_value(
                            value,
                            &format!("/columns/{index}/comment"),
                            editors,
                            disabled,
                            table,
                            selected_column,
                            SchemaChoices { open_select, root },
                        ));
                    }
                    let mut actions = html::div().class("schema-design-row-actions").child(
                        ui::button("")
                            .icon("key")
                            .aria(pk_label.clone())
                            .title(pk_label)
                            .variant(if primary {
                                ui::ButtonVariant::Primary
                            } else {
                                ui::ButtonVariant::Default
                            })
                            .disabled(disabled || name.trim().is_empty())
                            .on_click(Msg::Action(format!("schema-op:primary:{id}:"))),
                    );
                    for (label, icon, command, unavailable) in [
                        ("上移字段", "chev-up", "up", index == 0),
                        ("下移字段", "chev", "down", index + 1 == items.len()),
                        ("删除字段", "trash", "remove", false),
                    ] {
                        actions = actions.child(row_action(
                            label,
                            icon,
                            format!("schema-op:{command}:/columns:{index}"),
                            disabled || unavailable,
                        ));
                    }
                    row = row.child(actions);
                    list = list.child(row);
                }
                container =
                    container
                        .child(list)
                        .child(html::div().class("advanced-actions").child(row_action(
                            "新增字段",
                            "plus",
                            "schema-op:add:/columns:column".into(),
                            disabled,
                        )));
                if let Some(index) = selected {
                    let mut properties = items[index].as_object().cloned().unwrap_or_default();
                    for key in ["name", "native_type", "nullable", "default", "comment"] {
                        properties.remove(key);
                    }
                    let details = html::div()
                        .class("schema-design-column-details")
                        .child(html::strong(tr("选中字段扩展属性").into_owned()))
                        .child(html::span(
                            items[index]
                                .get("name")
                                .and_then(Value::as_str)
                                .filter(|name| !name.is_empty())
                                .unwrap_or("—")
                                .to_owned(),
                        ))
                        .child(render_value(
                            &Value::Object(properties),
                            &format!("/columns/{index}"),
                            editors,
                            disabled,
                            table,
                            selected_column,
                            SchemaChoices { open_select, root },
                        ));
                    container = container.child(details);
                }
                return container.into();
            }
            for (index, item) in items.iter().enumerate() {
                let child = format!("{path}/{index}");
                let row_key = item
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| child.clone());
                let mut row =
                    html::div()
                        .class("advanced-section")
                        .key(row_key)
                        .child(render_value(
                            item,
                            &child,
                            editors,
                            disabled,
                            table,
                            selected_column,
                            SchemaChoices { open_select, root },
                        ));
                for (label, action, enabled) in [
                    ("删除", "remove", true),
                    ("前移", "up", index > 0),
                    ("后移", "down", index + 1 < items.len()),
                ] {
                    row = row.child(
                        ui::button(tr(label).into_owned())
                            .disabled(disabled || !enabled)
                            .on_click(Msg::Action(format!("schema-op:{action}:{path}:{index}"))),
                    );
                }
                container = container.child(row);
            }
            let kinds: &[(&str, &str)] = if path.ends_with("/constraints") {
                &[("外键", "foreign_key"), ("CHECK", "check")]
            } else {
                &[(
                    "新增",
                    if path == "/columns" && table {
                        "column"
                    } else {
                        "item"
                    },
                )]
            };
            for (label, kind) in kinds {
                if template(path, kind).is_some() {
                    container = container.child(row_action(
                        label,
                        "plus",
                        format!("schema-op:add:{path}:{kind}"),
                        disabled,
                    ));
                }
            }
        }
        Value::Bool(on) => {
            container = container.child(
                ui::switch(tr(field_label(path.rsplit('/').next().unwrap_or(""))).into_owned())
                    .on(*on)
                    .disabled(disabled)
                    .on_change({
                        let path = path.to_owned();
                        move |_| Msg::Action(format!("schema-op:toggle:{path}"))
                    }),
            );
        }
        Value::String(text) if !schema_choices(path).is_empty() => {
            let label = tr(if path.ends_with("/default/kind") {
                "默认值种类"
            } else {
                field_label(path.rsplit('/').next().unwrap_or(""))
            })
            .into_owned();
            let toggle_path = path.to_owned();
            let change_path = path.to_owned();
            let mut select = ui::select()
                .aria(label)
                .value(text.clone())
                .disabled(disabled)
                .open(open_select == Some(path))
                .on_toggle(move |collapsed| {
                    Msg::Action(format!("schema-op:open:{toggle_path}:{}", !collapsed))
                });
            let choices = schema_choices(path);
            if !choices.iter().any(|(value, _)| *value == text.as_str()) {
                select = select.option(ui::SelectOption::new(text.clone(), text.clone()));
            }
            for (value, label) in choices {
                select = select.option(ui::SelectOption::new(*value, tr(label).into_owned()));
            }
            container = container.child(select.on_change(move |change| {
                Msg::Action(format!(
                    "schema-op:choice:{change_path}:{}",
                    change.value.as_str().unwrap_or_default()
                ))
            }));
        }
        Value::String(text) if path.ends_with("/kind") => {
            container = container.child(html::span(text.clone()));
        }
        Value::String(text) => {
            let editor = editors
                .entry(path.to_owned())
                .or_insert_with(|| Editor::new(text));
            let name = format!("schema:{path}");
            let key = path.rsplit('/').next().unwrap_or("");
            let label = tr(if key == "value" && path.contains("/default/") {
                "默认值"
            } else {
                field_label(key)
            })
            .into_owned();
            let long = matches!(key, "select" | "body" | "expression");
            if long {
                let edit_name = name.clone();
                let undo_name = name.clone();
                container = container.child(
                    ui::editor()
                        .key(name.clone())
                        .aria(label)
                        .readonly(disabled)
                        .text(editor.text.clone())
                        .cursor(editor.cursor)
                        .anchor(editor.anchor)
                        .max_lines(16u32)
                        .on_focus(Msg::Focus(name))
                        .on_undo(Msg::Undo(undo_name))
                        .on_edit(move |e| {
                            Msg::Edit(
                                edit_name.clone(),
                                e.from,
                                e.to,
                                e.text.clone(),
                                e.cursor,
                                e.len,
                            )
                        }),
                );
            } else {
                let edit_name = name.clone();
                let undo_name = name.clone();
                container = container.child(
                    ui::input()
                        .key(name.clone())
                        .aria(label)
                        .variant(ui::InputVariant::Field)
                        .readonly(disabled)
                        .text(editor.text.clone())
                        .cursor(editor.cursor)
                        .anchor(editor.anchor)
                        .on_focus(Msg::Focus(name))
                        .on_undo(Msg::Undo(undo_name))
                        .on_edit(move |e| {
                            Msg::Edit(
                                edit_name.clone(),
                                e.from,
                                e.to,
                                e.text.clone(),
                                e.cursor,
                                e.len,
                            )
                        }),
                );
            }
        }
        Value::Null => {
            let replacement = if path.ends_with("/generated") {
                json!({"expression":"","stored":false})
            } else if path.ends_with("/order") {
                json!({"follows":true,"trigger":""})
            } else if path.ends_with("/prefix_length") {
                json!(1)
            } else {
                json!("")
            };
            container = container.child(
                ui::button(tr("设置").into_owned())
                    .disabled(disabled)
                    .on_click(Msg::Action(format!("schema-op:set:{path}:{replacement}"))),
            );
        }
        Value::Number(number) => {
            return render_value(
                &Value::String(number.to_string()),
                path,
                editors,
                disabled,
                table,
                selected_column,
                SchemaChoices { open_select, root },
            );
        }
    }
    container.into()
}
