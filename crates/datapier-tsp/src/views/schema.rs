//! MySQL 字段、索引与约束展示。
use super::*;

fn recorded(value: &str) -> std::borrow::Cow<'_, str> {
    if value.is_empty() {
        crate::i18n::tr("未记录")
    } else {
        std::borrow::Cow::Borrowed(value)
    }
}

fn metadata_part(label: &str, value: &str) -> html::El<Msg> {
    html::div()
        .class("schema-metadata-part")
        .child(html::span(crate::i18n::tr(label).into_owned()).class("muted"))
        .child(html::span(recorded(value).into_owned()).class("schema-metadata-value"))
}

pub(super) fn schema_panel(app: &App) -> Node<Msg> {
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
                let name = col
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .map(std::borrow::Cow::Borrowed)
                    .unwrap_or_else(|| crate::i18n::tr("未记录"));
                let ty = col
                    .get("type_name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .map(std::borrow::Cow::Borrowed)
                    .unwrap_or_else(|| crate::i18n::tr("未知"));
                let nullable = match col.get("nullable").and_then(Value::as_bool) {
                    Some(false) => "NO",
                    Some(true) => "YES",
                    None => "未知",
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
                    let mut details = html::div()
                        .class("schema-metadata-summary")
                        .child(metadata_part("类型", part("type")));
                    if key == "indexes" {
                        details = details
                            .child(metadata_part(
                                "唯一性",
                                &crate::i18n::tr(
                                    match item.get("unique").and_then(Value::as_bool) {
                                        Some(true) => "唯一",
                                        Some(false) => "非唯一",
                                        None => "未知",
                                    },
                                ),
                            ))
                            .child(metadata_part(
                                "字段 / 表达式",
                                item.get("expression")
                                    .and_then(Value::as_str)
                                    .filter(|value| !value.is_empty())
                                    .or_else(|| item.get("column").and_then(Value::as_str))
                                    .unwrap_or(""),
                            ));
                    } else {
                        details = details.child(metadata_part("字段", part("column")));
                        if !ref_target.is_empty() {
                            details = details.child(metadata_part("引用目标", &ref_target));
                        }
                    }
                    panel = panel.child(
                        html::div()
                            .class("schema-metadata-row")
                            .child(
                                html::strong(recorded(part("name"))).class("schema-metadata-name"),
                            )
                            .child(details),
                    );
                }
            } else {
                panel = panel
                    .child(html::span(crate::i18n::tr("未知").into_owned()).class("empty-note"));
            }
        }
    } else {
        panel = panel.child(
            html::span(
                crate::i18n::tr(if app.target.is_some() {
                    "结构尚未加载，请刷新"
                } else {
                    "尚未选择对象"
                })
                .into_owned(),
            )
            .class("empty-note"),
        );
    }
    panel.into()
}
