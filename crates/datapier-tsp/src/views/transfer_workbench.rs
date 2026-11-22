use super::*;

fn input(app: &App, key: &str, label: &str) -> Node<Msg> {
    let field = field(app, &app.transfer_key(key), label);
    if [
        "columns",
        "mapping",
        "types",
        "definer-map",
        "output",
        "report-path",
    ]
    .iter()
    .any(|part| key.contains(part))
        || key
            .strip_prefix("file-")
            .is_some_and(|index| index.parse::<usize>().is_ok())
    {
        html::div().class("workbench-wide").child(field).into()
    } else {
        field
    }
}
fn select(app: &App, key: &str, label: &str, values: &[(&str, &str)]) -> Node<Msg> {
    if values.len() == 2
        && values.iter().any(|(value, _)| *value == "true")
        && values.iter().any(|(value, _)| *value == "false")
    {
        let field = app.transfer_key(key);
        let mut wrapper = ui::field(crate::i18n::tr(label).into_owned());
        if let Some(error) = app
            .workbench
            .field_error
            .as_ref()
            .filter(|error| error.field == field)
        {
            wrapper = wrapper.error(error.message.render().into_owned());
        }
        let switch = ui::switch(crate::i18n::tr(label).into_owned())
            .key(format!("input-{field}"))
            .on(app.field(&field) == "true")
            .disabled(
                app.workbench.transfer.pending.is_some()
                    || app.workbench.transfer.queued_prepare.is_some(),
            )
            .on_change(move |change| {
                Msg::Action(format!(
                    "field:{field}:{}",
                    if change.value.as_bool().unwrap_or(false) {
                        "true"
                    } else {
                        "false"
                    }
                ))
            });
        return wrapper.child(switch).into();
    }
    choices(app, &app.transfer_key(key), label, values)
}
fn connection(app: &App, key: &str, label: &str) -> Node<Msg> {
    let mut options = vec![("", "请选择连接")];
    let ids: Vec<_> = app.profiles.iter().map(|p| p.id.to_string()).collect();
    for (profile, id) in app.profiles.iter().zip(&ids) {
        let redis = profile.provider == datapier_core::model::ProviderKind::Redis;
        if !profile.is_archery() && redis == (app.field(&app.transfer_key("provider")) == "redis") {
            options.push((id.as_str(), profile.name.as_str()));
        }
    }
    select(app, key, label, &options)
}
fn database_select(app: &App, side: &str, redis: bool) -> Node<Msg> {
    let key = format!("{side}-{}", if redis { "redis-db" } else { "database" });
    let label = match (side, redis) {
        ("source", false) => "来源数据库",
        ("source", true) => "来源 Redis DB",
        (_, false) => "目标数据库",
        _ => "目标 Redis DB",
    };
    let entry = app
        .workbench
        .transfer
        .catalog
        .get(&format!("{side}:databases"));
    let names: Vec<&str> = entry
        .and_then(|entry| entry.value.as_ref())
        .and_then(|value| value["databases"].as_array())
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    let mut options = vec![("", "请选择数据库")];
    options.extend(names.iter().map(|name| (*name, *name)));
    let current = app.field(&app.transfer_key(&key));
    if !current.is_empty() && !names.contains(&current) {
        options.push((current, current));
    }
    let mut wrapper = html::div()
        .class("transfer-catalog-field")
        .child(select(app, &key, label, &options));
    if let Some(entry) = entry {
        if entry.token.is_some() {
            wrapper = wrapper.child(html::span(crate::i18n::tr("正在加载数据库…").into_owned()));
        }
        if let Some(error) = &entry.error {
            wrapper = wrapper
                .child(html::span(error.clone()).class("field-error"))
                .child(icon_button(
                    "repeat",
                    "重试加载数据库",
                    &format!("transfer-catalog-retry:{side}:databases"),
                ));
        }
    }
    wrapper.into()
}
fn object_select(
    app: &App,
    key: &str,
    label: &str,
    side: &str,
    kind: &str,
    allow_new: bool,
) -> Node<Msg> {
    let index = key.rsplit('-').next().unwrap_or("0");
    let field_side = if side == "destination" {
        "target"
    } else {
        side
    };
    let override_key = format!("object-{field_side}-db-{index}");
    let selected = if key.starts_with("file-") {
        ""
    } else {
        app.field(&app.transfer_key(&override_key))
    };
    let database = if !selected.is_empty() {
        selected
    } else if side == "source" && app.field(&app.transfer_key("mode")) == "sql_restore" {
        ""
    } else {
        app.field(&app.transfer_key(&format!("{side}-database")))
    };
    let catalog_key = format!("{side}:objects:{kind}:{database}");
    let entry = app.workbench.transfer.catalog.get(&catalog_key);
    let resources = entry
        .and_then(|entry| entry.value.as_ref())
        .and_then(|value| value["resources"].as_array());
    let mut names: Vec<&str> = resources
        .into_iter()
        .flatten()
        .filter(|item| item["kind"].as_str() == Some(kind))
        .filter_map(|item| item["name"].as_str())
        .collect();
    if side == "source" && app.field(&app.transfer_key("mode")) == "sql_restore" {
        names = (0..app.workbench.transfer.files)
            .filter_map(|i| {
                app.workbench
                    .transfer
                    .catalog
                    .get(&format!("input:{i}"))
                    .filter(|entry| {
                        app.transfer_input_params(i)
                            .ok()
                            .is_some_and(|params| params == entry.signature)
                    })
            })
            .filter_map(|entry| entry.value.as_ref())
            .flat_map(|value| value["objects"].as_array().into_iter().flatten())
            .filter(|item| {
                item["kind"].as_str() == Some(kind)
                    && (database.is_empty() || item["database"].as_str() == Some(database))
            })
            .filter_map(|item| item["name"].as_str())
            .collect();
    }
    let current = app.field(&app.transfer_key(key));
    if !current.is_empty() && !names.contains(&current) {
        names.push(current);
    }
    let mut options = vec![("", "请选择对象")];
    options.extend(names.iter().map(|name| (*name, *name)));
    if allow_new {
        options.push(("__transfer_new__", "新建…"));
    }
    let control = select(app, key, label, &options);
    let mut wrapper = html::div().class("transfer-catalog-field").child(control);
    if let Some(entry) = entry {
        if entry.token.is_some() {
            wrapper = wrapper.child(html::span(crate::i18n::tr("正在加载对象…").into_owned()));
        }
        if let Some(error) = &entry.error {
            wrapper = wrapper
                .child(html::span(error.clone()).class("field-error"))
                .child(icon_button(
                    "repeat",
                    "重试加载对象",
                    &format!("transfer-objects-load:{side}:{kind}:{database}"),
                ));
        }
        if entry
            .value
            .as_ref()
            .is_some_and(|page| page["complete"] == false && page["cursor"].is_string())
        {
            wrapper = wrapper.child(button(
                "加载更多对象",
                &format!("transfer-objects-more:{side}:{kind}:{database}"),
            ));
        }
    }
    if entry.is_none()
        && !(side == "source" && app.field(&app.transfer_key("mode")) == "sql_restore")
    {
        wrapper = wrapper.child(button(
            "读取对象",
            &format!("transfer-objects-load:{side}:{kind}:{database}"),
        ));
    }
    wrapper.into()
}
fn object_database_select(app: &App, index: usize, side: &str) -> Node<Msg> {
    if app.workbench.transfer.project_mode {
        return html::span(crate::i18n::tr("数据库由项目绑定确定").into_owned())
            .class("muted")
            .into();
    }
    let field_side = if side == "destination" {
        "target"
    } else {
        side
    };
    let key = format!("object-{field_side}-db-{index}");
    let label = if side == "source" {
        "来源对象数据库"
    } else {
        "目标对象数据库"
    };
    let mut names: Vec<&str> = app
        .workbench
        .transfer
        .catalog
        .get(&format!("{side}:databases"))
        .and_then(|entry| entry.value.as_ref())
        .and_then(|value| value["databases"].as_array())
        .map(|values| values.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if side == "source" && app.field(&app.transfer_key("mode")) == "sql_restore" {
        names = (0..app.workbench.transfer.files)
            .filter_map(|i| {
                app.workbench
                    .transfer
                    .catalog
                    .get(&format!("input:{i}"))
                    .filter(|entry| {
                        app.transfer_input_params(i)
                            .ok()
                            .is_some_and(|params| params == entry.signature)
                    })
            })
            .filter_map(|entry| entry.value.as_ref())
            .flat_map(|value| value["objects"].as_array().into_iter().flatten())
            .filter_map(|object| object["database"].as_str())
            .collect();
    }
    names.sort_unstable();
    names.dedup();
    let mut values = vec![("", "使用所选数据库")];
    values.extend(names.iter().map(|name| (*name, *name)));
    let current = app.field(&app.transfer_key(&key));
    if !current.is_empty() && !names.contains(&current) {
        values.push((current, current));
    }
    select(app, &key, label, &values)
}
fn column_mapping(app: &App, group: &str, index: usize) -> Node<Msg> {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let source = app.field(&app.transfer_key(&format!("object-source-{index}")));
    let target = app.field(&app.transfer_key(&format!(
        "{}-target-{index}",
        if group == "file" { "file" } else { "object" }
    )));
    let source_override = app.field(&app.transfer_key(&format!("object-source-db-{index}")));
    let target_override = if group == "file" {
        ""
    } else {
        app.field(&app.transfer_key(&format!("object-target-db-{index}")))
    };
    let source_db = if !source_override.is_empty() {
        source_override
    } else if app.field(&app.transfer_key("mode")) == "sql_restore" {
        ""
    } else {
        app.field(&app.transfer_key("source-database"))
    };
    let target_db = if target_override.is_empty() {
        app.field(&app.transfer_key("destination-database"))
    } else {
        target_override
    };
    let source_columns = if group == "file" {
        app.workbench
            .transfer
            .catalog
            .get(&format!("input:{index}"))
            .filter(|entry| {
                app.transfer_input_params(index)
                    .ok()
                    .is_some_and(|params| params == entry.signature)
            })
            .and_then(|entry| entry.value.as_ref())
            .and_then(|value| value["columns"].as_array())
    } else if app.field(&app.transfer_key("mode")) == "sql_restore" {
        (0..app.workbench.transfer.files)
            .filter_map(|i| {
                app.workbench
                    .transfer
                    .catalog
                    .get(&format!("input:{i}"))
                    .filter(|entry| {
                        app.transfer_input_params(i)
                            .ok()
                            .is_some_and(|params| params == entry.signature)
                    })
            })
            .filter_map(|entry| entry.value.as_ref())
            .flat_map(|value| value["objects"].as_array().into_iter().flatten())
            .find(|object| {
                object["name"] == source
                    && (source_db.is_empty() || object["database"] == source_db)
            })
            .and_then(|object| object["columns"].as_array())
    } else {
        app.workbench
            .transfer
            .catalog
            .get(&format!("source:columns:{source_db}:{source}"))
            .and_then(|entry| entry.value.as_ref())
            .and_then(|value| value["columns"].as_array())
    };
    let target_columns = app
        .workbench
        .transfer
        .catalog
        .get(&format!("destination:columns:{target_db}:{target}"))
        .and_then(|entry| entry.value.as_ref())
        .and_then(|value| value["columns"].as_array());
    let mapping = crate::transfer_workbench::parse_mapping(
        app.field(&app.transfer_key(&format!("{group}-mapping-{index}"))),
    )
    .unwrap_or_default();
    let mut result = html::div()
        .class("transfer-mapping")
        .child(html::strong(crate::i18n::tr("字段映射").into_owned()));
    for (side, name, database) in [
        ("source", source, source_db),
        ("destination", target, target_db),
    ] {
        if side == "source"
            && (group == "file" || app.field(&app.transfer_key("mode")) == "sql_restore")
        {
            continue;
        }
        if name.is_empty() {
            continue;
        }
        let entry = app
            .workbench
            .transfer
            .catalog
            .get(&format!("{side}:columns:{database}:{name}"));
        if let Some(error) = entry.and_then(|entry| entry.error.as_ref()) {
            result = result.child(
                html::div()
                    .class("transfer-actions")
                    .child(html::span(error.clone()).class("field-error"))
                    .child(icon_button(
                        "repeat",
                        "重试加载字段",
                        &format!("transfer-columns-retry:{side}:{group}:{index}"),
                    )),
            );
        } else if entry.is_none() {
            result = result.child(html::div().class("transfer-actions").child(button(
                "读取字段",
                &format!("transfer-columns-retry:{side}:{group}:{index}"),
            )));
        }
    }
    if source_columns.is_none() {
        return result
            .child(html::span(crate::i18n::tr("请先读取来源字段").into_owned()).class("muted"))
            .into();
    }
    for column in source_columns
        .into_iter()
        .flatten()
        .filter(|column| group != "object" || column["generated"] != true)
    {
        let Some(name) = column["name"].as_str() else {
            continue;
        };
        let encoded = URL_SAFE_NO_PAD.encode(name);
        let select_key = format!("transfer-map-{group}-{index}-{encoded}");
        let open_key = select_key.clone();
        let action_key = format!("transfer-map:{group}:{index}:{encoded}:");
        let selected = match mapping.get(name) {
            Some(target) if target == name => "".to_owned(),
            Some(target) => target.clone(),
            None if group == "object" && !mapping.is_empty() => "\0".to_owned(),
            None => String::new(),
        };
        let mut control = ui::select()
            .key(select_key.clone())
            .aria(name.to_owned())
            .value(selected)
            .open(app.workbench.generic_select.as_deref() == Some(select_key.as_str()))
            .on_toggle(move |collapsed| {
                Msg::Action(format!("select-open:{open_key}:{}", !collapsed))
            })
            .option(ui::SelectOption::new(
                "",
                crate::i18n::tr("默认同名").into_owned(),
            ));
        if group == "object" {
            control = control.option(ui::SelectOption::new(
                "\0",
                crate::i18n::tr("不复制此字段").into_owned(),
            ));
        }
        for target in target_columns
            .into_iter()
            .flatten()
            .filter(|column| column["generated"] != true)
            .filter_map(|column| column["name"].as_str())
        {
            control = control.option(ui::SelectOption::new(target, target));
        }
        result = result.child(
            ui::field(name.to_owned()).child(control.on_change(move |change| {
                let target = change.value.as_str().unwrap_or_default();
                Msg::Action(format!("{action_key}{}", URL_SAFE_NO_PAD.encode(target)))
            })),
        );
    }
    result.into()
}
fn redis_key_selection(app: &App) -> Node<Msg> {
    let entry = app.workbench.transfer.catalog.get("source:objects");
    let mut list = html::div()
        .class("transfer-key-list")
        .child(html::strong(crate::i18n::tr("选择来源 key").into_owned()));
    if let Some(entry) = entry {
        if let Some(error) = &entry.error {
            list = list.child(
                html::div()
                    .class("transfer-actions")
                    .child(html::span(error.clone()).class("field-error"))
                    .child(icon_button(
                        "repeat",
                        "重试加载 key",
                        "transfer-catalog-retry:source:objects",
                    )),
            );
        }
        if let Some(resources) = entry
            .value
            .as_ref()
            .and_then(|value| value["resources"].as_array())
        {
            for item in resources {
                let Some(key) = item["key_b64"].as_str() else {
                    continue;
                };
                let name = item["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .unwrap_or("空 key");
                let checked = app
                    .workbench
                    .transfer
                    .redis_keys
                    .as_ref()
                    .is_some_and(|keys| keys.contains(key));
                let action = format!("transfer-redis-key:{key}");
                list = list.child(
                    html::label()
                        .child(
                            html::checkbox("transfer-redis-key", name.to_owned())
                                .checked(checked)
                                .on_change(move |_| Msg::Action(action.clone())),
                        )
                        .child(html::span(name.to_owned())),
                );
            }
        }
        if entry.token.is_some() {
            list = list.child(html::span(crate::i18n::tr("正在加载 key…").into_owned()));
        }
        if entry
            .value
            .as_ref()
            .is_some_and(|value| value["complete"] == false && value["cursor"].is_string())
        {
            list = list.child(button("加载更多", "transfer-catalog-more:source"));
        }
    } else {
        list = list.child(button("读取 key", "transfer-catalog-retry:source:objects"));
    }
    list.into()
}
fn filter_column_select(app: &App, index: usize, filter: usize) -> Node<Msg> {
    let object = app.field(&app.transfer_key(&format!("object-source-{index}")));
    let override_db = app.field(&app.transfer_key(&format!("object-source-db-{index}")));
    let database = if override_db.is_empty() {
        app.field(&app.transfer_key("source-database"))
    } else {
        override_db
    };
    let entry = app
        .workbench
        .transfer
        .catalog
        .get(&format!("source:columns:{database}:{object}"));
    let names: Vec<&str> = entry
        .and_then(|entry| entry.value.as_ref())
        .and_then(|value| value["columns"].as_array())
        .map(|columns| {
            columns
                .iter()
                .filter_map(|column| column["name"].as_str())
                .collect()
        })
        .unwrap_or_default();
    let key = format!("object-filter-column-{index}-{filter}");
    let mut options = vec![("", "请选择字段")];
    options.extend(names.iter().map(|name| (*name, *name)));
    let current = app.field(&app.transfer_key(&key));
    if !current.is_empty() && !names.contains(&current) {
        options.push((current, current));
    }
    select(app, &key, "过滤字段", &options)
}

fn detail_node(value: &Value, key: &str) -> Node<Msg> {
    let mut node = html::div().class("workbench-detail");
    if let Some(object) = value["object"].as_str() {
        node = node.child(html::strong(object));
    }
    if let (Some(columns), Some(rows)) = (value["columns"].as_array(), value["rows"].as_array()) {
        let mut table = html::div().class("workbench-sample").attr("role", "table");
        let mut header = html::div().class("gridrow header").attr("role", "row");
        for column in columns {
            header = header.child(
                html::div()
                    .class("cell")
                    .attr("role", "columnheader")
                    .child(html::span(column["name"].as_str().unwrap_or(""))),
            );
        }
        table = table.child(header);
        for row in rows {
            let mut line = html::div().class("gridrow").attr("role", "row");
            if let Some(cells) = row.as_array() {
                for cell in cells {
                    let text =
                        <datapier_core::model::Value as serde::Deserialize>::deserialize(cell)
                            .map(|value| value_text(&value, true).into_owned())
                            .unwrap_or_else(|_| cell.to_string());
                    line = line.child(
                        html::div()
                            .class("cell")
                            .attr("role", "cell")
                            .child(html::span(text)),
                    );
                }
            }
            table = table.child(line);
        }
        node = node.child(table);
    } else {
        let mut facts = html::div().class("advanced-facts");
        for (field, label) in [
            ("state", "状态"),
            ("complete", "完整性"),
            ("item_count", "计划项数"),
            ("sample_count", "样本数"),
            ("diagnostic_count", "诊断数"),
            ("read_rows", "读取行数"),
            ("read_keys", "读取 key 数"),
            ("written_rows", "写入行数"),
            ("written_keys", "写入 key 数"),
            ("failed", "失败数量"),
            ("skipped", "跳过数量"),
            ("rolled_back", "回滚数量"),
            ("outcome_unknown", "待确认数量"),
            ("processed_bytes", "已处理字节"),
            ("total_bytes", "总字节"),
            ("first_record", "首条记录"),
            ("last_record", "末条记录"),
            ("record", "记录号"),
            ("byte_offset", "字节偏移"),
            ("message", "消息"),
        ] {
            let Some(item) = value.get(field).or_else(|| value["progress"].get(field)) else {
                continue;
            };
            let text = if field == "state" {
                crate::advanced::status(item.as_str().unwrap_or("unknown"))
            } else if let Some(text) = item.as_str() {
                text.to_owned()
            } else if let Some(on) = item.as_bool() {
                crate::i18n::tr(if on { "是" } else { "否" }).into_owned()
            } else if item.is_number() {
                item.to_string()
            } else {
                continue;
            };
            facts = facts.child(crate::advanced::fact(label, text));
        }
        node = node.child(facts);
    }
    if let Some(sql) = value["sql"].as_str() {
        node = node.child(html::pre(sql).class("preview"));
    }
    if let Some(notes) = value["notes"].as_array() {
        for note in notes.iter().filter_map(Value::as_str) {
            node = node.child(html::p(note).class("notice"));
        }
    }
    node.child(crate::advanced::diagnostic(key, value)).into()
}

fn item_title(item: &Value) -> String {
    let object = item["target"]["name"]
        .as_str()
        .or_else(|| item["source"]["name"].as_str())
        .or_else(|| item["object"].as_str())
        .unwrap_or("");
    let action = item["action"].as_str().unwrap_or("");
    format!(
        "{} · {}",
        crate::i18n::tr(if action == "object_scope" {
            "恢复整个对象及数据"
        } else if item["phase"] == "schema" {
            "结构变更"
        } else {
            "数据处理"
        }),
        object
    )
}

pub(super) fn panel(app: &App) -> Node<Msg> {
    let stage = if app.workbench.transfer.stage.is_empty() {
        "source"
    } else {
        app.workbench.transfer.stage.as_str()
    };
    let mode = app.field(&app.transfer_key("mode"));
    let redis = app.field(&app.transfer_key("provider")) == "redis";
    let picker_open =
        app.workbench.generic_select.as_deref().is_some_and(|key| {
            key.starts_with("field-transfer-") || key.starts_with("transfer-map-")
        });
    let mut root = html::div()
        .class(if picker_open {
            "body detail-panel transfer-panel transfer-picker-open"
        } else {
            "body detail-panel transfer-panel"
        })
        .key("transfer-workbench")
        .attr("role", "region")
        .attr("aria-label", crate::i18n::tr("数据交换与迁移").into_owned());
    root = root.child(html::strong(crate::i18n::tr("数据交换与迁移").into_owned()));
    let endpoint_summary = |side: &str| {
        let key = if app.workbench.transfer.project_mode {
            format!("{side}-alias")
        } else {
            format!("{side}-profile")
        };
        let id = app.field(&app.transfer_key(&key));
        let name = if app.workbench.transfer.project_mode {
            id.to_owned()
        } else {
            app.profiles
                .iter()
                .find(|profile| profile.id.to_string() == id)
                .map(|profile| profile.name.clone())
                .unwrap_or_else(|| id.to_owned())
        };
        let db = app.field(&app.transfer_key(&format!(
            "{side}-{}",
            if redis { "redis-db" } else { "database" }
        )));
        if db.is_empty() {
            name
        } else {
            format!("{name} / {db}")
        }
    };
    let source = endpoint_summary("source");
    let destination = endpoint_summary("destination");
    let file_summary = if matches!(mode, "import" | "sql_restore") {
        (0..app.workbench.transfer.files)
            .map(|i| app.field(&app.transfer_key(&format!("file-{i}"))))
            .filter(|path| !path.is_empty())
            .collect::<Vec<_>>()
            .join(" · ")
    } else if matches!(mode, "export" | "sql_dump") {
        app.field(&app.transfer_key("output")).to_owned()
    } else {
        String::new()
    };
    let mut summary = html::div()
        .class("transfer-summary advanced-facts")
        .child(crate::advanced::fact(
            "来源",
            if source.is_empty() {
                crate::i18n::tr("未选择连接").into_owned()
            } else {
                source
            },
        ))
        .child(crate::advanced::fact(
            "目标",
            if destination.is_empty() {
                crate::i18n::tr("未选择连接").into_owned()
            } else {
                destination
            },
        ));
    if mode != "copy" {
        summary = summary.child(crate::advanced::fact(
            "文件路径",
            if file_summary.is_empty() {
                crate::i18n::tr("未选择文件").into_owned()
            } else {
                file_summary
            },
        ));
    }
    root = root.child(summary);
    let mut tabs = ui::tabs().key("transfer-tabs").active(stage.to_owned());
    for (id, title) in [
        ("source", "来源与目标"),
        ("objects", "对象与映射"),
        ("policy", "执行策略"),
        ("preview", "准备预览"),
        ("report", "执行报告"),
    ] {
        tabs = tabs.tab(id, crate::i18n::tr(title).into_owned());
    }
    root = root.child(
        html::div()
            .class("transfer-tabs")
            .child(tabs.on_select(|value| Msg::Action(format!("transfer-stage:{value}")))),
    );
    let mut body = html::div().class("transfer-body");
    if stage == "source" {
        body = body.child(select(
            app,
            "mode",
            "操作",
            &[
                ("import", "导入数据"),
                ("export", "导出数据"),
                ("copy", "迁移到…"),
                ("sql_dump", "导出 SQL"),
                ("sql_restore", "恢复 SQL"),
            ],
        ));
        body = body.child(select(
            app,
            "provider",
            "数据库类型",
            &[("mysql", "MySQL"), ("redis", "Redis")],
        ));
        let aliases: Vec<&str> = app
            .workbench
            .project
            .project
            .as_ref()
            .and_then(|project| project["manifest"]["connections"].as_array())
            .map(|connections| {
                connections
                    .iter()
                    .filter_map(|connection| connection["alias"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        let mut alias_options = vec![("", "请选择连接")];
        alias_options.extend(aliases.iter().map(|alias| (*alias, *alias)));
        body = body.child(html::strong(crate::i18n::tr("来源与目标").into_owned()));
        if app.workbench.transfer.project_mode {
            if matches!(mode, "copy" | "export" | "sql_dump") {
                body = body.child(select(
                    app,
                    "source-alias",
                    "来源逻辑连接别名",
                    &alias_options,
                ));
            }
            if matches!(mode, "copy" | "import" | "sql_restore") {
                body = body.child(select(
                    app,
                    "destination-alias",
                    "目标逻辑连接别名",
                    &alias_options,
                ));
            }
            body = body.child(html::span(
                "使用项目绑定组解析连接与路径 / Project bindings resolve connections and paths",
            ));
        } else {
            if matches!(mode, "copy" | "export" | "sql_dump") {
                body = body.child(connection(app, "source-profile", "来源连接"));
                body = body.child(database_select(app, "source", redis));
            }
            if matches!(mode, "copy" | "import" | "sql_restore") {
                body = body.child(connection(app, "destination-profile", "目标连接"));
                body = body.child(database_select(app, "destination", redis));
            }
        }
        if mode == "export" {
            body = body.child(select(
                app,
                "format",
                "文件格式",
                &[("csv", "CSV"), ("json", "JSON")],
            ));
        }
        if matches!(mode, "import" | "sql_restore") {
            for i in 0..app.workbench.transfer.files {
                let mut file = html::div().class("transfer-group");
                file = file
                    .child(html::strong(format!(
                        "{} {}",
                        crate::i18n::tr("输入文件"),
                        i + 1
                    )))
                    .child(input(app, &format!("file-{i}"), "文件路径"));
                if mode == "import" {
                    file = file
                        .child(select(
                            app,
                            &format!("file-format-{i}"),
                            "文件格式",
                            &[("csv", "CSV"), ("json", "JSON")],
                        ))
                        .child(input(app, &format!("file-encoding-{i}"), "CSV 编码"))
                        .child(input(app, &format!("file-delimiter-{i}"), "CSV 分隔符"))
                        .child(select(
                            app,
                            &format!("file-header-{i}"),
                            "CSV 包含表头",
                            &[("true", "是"), ("false", "否")],
                        ))
                        .child(input(
                            app,
                            &format!("file-columns-{i}"),
                            "CSV 列顺序（JSON 字符串数组）",
                        ))
                        .child(input(
                            app,
                            &format!("file-null-marker-{i}"),
                            "CSV NULL 标记",
                        ))
                        .child(select(
                            app,
                            &format!("file-backslash-{i}"),
                            "CSV 反斜线转义",
                            &[("true", "是"), ("false", "否")],
                        ))
                        .child(input(
                            app,
                            &format!("file-binary-columns-{i}"),
                            "CSV 二进制列（JSON 字符串数组）",
                        ))
                        .child(select(
                            app,
                            &format!("file-sync-{i}"),
                            "同步已有表结构",
                            &[("false", "否"), ("true", "是")],
                        ));
                    file = file
                        .child(object_select(
                            app,
                            &format!("file-target-{i}"),
                            "目标表",
                            "destination",
                            "table",
                            true,
                        ))
                        .child(column_mapping(app, "file", i))
                        .child(input(
                            app,
                            &format!("file-types-{i}"),
                            "目标类型（字段=类型或 JSON 数组）",
                        ))
                        .child(input(
                            app,
                            &format!("file-range-{i}"),
                            "记录范围（起始-结束，逗号分隔）",
                        ));
                    if let Ok(columns) = crate::transfer_workbench::parse_types(
                        app.field(&app.transfer_key(&format!("file-types-{i}"))),
                    ) {
                        if !columns.is_empty() {
                            file = file.child(html::span(
                                crate::i18n::tr("文件元数据可能不完整，生成的结构仅供确认。")
                                    .into_owned(),
                            ));
                        }
                        for column in columns {
                            let Some(name) = column["name"].as_str() else {
                                continue;
                            };
                            let kind =
                                crate::transfer_workbench::column_default_key(i, name, "kind");
                            let value =
                                crate::transfer_workbench::column_default_key(i, name, "value");
                            file = file.child(html::strong(name.to_owned())).child(select(
                                app,
                                &kind,
                                "列默认值类型",
                                &[
                                    ("", "自动（按文件元数据）"),
                                    ("unspecified", "不指定默认值"),
                                    ("null", "NULL"),
                                    ("literal", "字面值"),
                                    ("expression", "SQL 表达式"),
                                ],
                            ));
                            if matches!(
                                app.field(&app.transfer_key(&kind)),
                                "literal" | "expression"
                            ) {
                                file = file.child(input(app, &value, "默认值内容"));
                            }
                        }
                    }
                }
                let entry = app.workbench.transfer.catalog.get(&format!("input:{i}"));
                let current = app.transfer_input_params(i).ok();
                let valid = entry.is_some_and(|entry| current.as_ref() == Some(&entry.signature));
                let mut inspect = html::div().class("transfer-actions").child(button(
                    "读取文件对象与字段",
                    &format!("transfer-input-inspect:{i}"),
                ));
                if let Some(entry) = entry {
                    if entry.token.is_some() && valid {
                        inspect = inspect
                            .child(html::span(crate::i18n::tr("正在读取文件…").into_owned()));
                    } else if let Some(error) = &entry.error {
                        inspect = inspect.child(html::span(error.clone()).class("field-error"));
                    } else if entry.value.is_some() && valid {
                        inspect = inspect.child(html::span(
                            crate::i18n::tr("已读取文件对象与字段").into_owned(),
                        ));
                    }
                }
                file = file.child(inspect);
                body = body.child(file);
            }
            body = body.child(
                html::div()
                    .class("transfer-actions")
                    .child(icon_button("plus", "添加文件", "transfer-ui-file-add"))
                    .child(icon_button(
                        "trash",
                        "移除末尾文件",
                        "transfer-ui-file-remove",
                    )),
            );
        }
        if matches!(mode, "export" | "sql_dump") {
            body = body.child(input(app, "output", "输出文件路径"));
        }
    }
    if stage == "objects" {
        body = body.child(html::strong(
            crate::i18n::tr(if mode == "sql_restore" {
                "对象名称映射（留空保留原名；预览中选择恢复范围）"
            } else {
                "对象与映射"
            })
            .into_owned(),
        ));
        if !redis && matches!(mode, "copy" | "export" | "sql_dump" | "sql_restore") {
            for i in 0..app.workbench.transfer.objects {
                let mut object = html::div().class("transfer-group");
                let kind = app.field(&app.transfer_key(&format!("object-kind-{i}")));
                object = object
                    .child(html::strong(format!(
                        "{} {}",
                        crate::i18n::tr(if mode == "sql_restore" {
                            "名称映射（留空保留原名）"
                        } else {
                            "对象（留空表示全部）"
                        }),
                        i + 1
                    )))
                    .child(select(
                        app,
                        &format!("object-kind-{i}"),
                        "对象类型",
                        &[
                            ("table", "表"),
                            ("view", "视图"),
                            ("trigger", "触发器"),
                            ("procedure", "过程"),
                            ("function", "函数"),
                            ("event", "事件"),
                        ],
                    ))
                    .child(object_database_select(app, i, "source"))
                    .child(object_select(
                        app,
                        &format!("object-source-{i}"),
                        "来源对象",
                        "source",
                        kind,
                        false,
                    ))
                    .child(object_database_select(app, i, "destination"))
                    .child(object_select(
                        app,
                        &format!("object-target-{i}"),
                        "目标对象",
                        "destination",
                        kind,
                        true,
                    ))
                    .child(column_mapping(app, "object", i))
                    .child(input(
                        app,
                        &format!("object-types-{i}"),
                        "目标类型（字段=类型或 JSON 数组）",
                    ))
                    .child(select(
                        app,
                        &format!("object-create-{i}"),
                        "创建此对象",
                        &[("false", "否"), ("true", "是")],
                    ))
                    .child(select(
                        app,
                        &format!("object-sync-{i}"),
                        "同步此对象结构",
                        &[("false", "否"), ("true", "是")],
                    ));
                for j in 0..app
                    .workbench
                    .transfer
                    .object_filters
                    .get(i)
                    .copied()
                    .unwrap_or(1)
                {
                    let filter = html::div()
                        .class("transfer-filter")
                        .key(format!("transfer-filter-{i}-{j}"))
                        .child(filter_column_select(app, i, j))
                        .child(select(
                            app,
                            &format!("object-filter-op-{i}-{j}"),
                            "过滤条件",
                            &[
                                ("eq", "等于"),
                                ("ne", "不等于"),
                                ("lt", "小于"),
                                ("le", "小于或等于"),
                                ("gt", "大于"),
                                ("ge", "大于或等于"),
                                ("like", "LIKE"),
                                ("is_null", "为空"),
                                ("is_not_null", "非空"),
                            ],
                        ))
                        .child(select(
                            app,
                            &format!("object-filter-type-{i}-{j}"),
                            "过滤值类型",
                            &[
                                ("text", "文本"),
                                ("integer", "整数"),
                                ("unsigned", "无符号整数"),
                                ("decimal", "精确小数"),
                                ("float", "浮点"),
                                ("bool", "布尔"),
                                ("bytes", "Base64"),
                                ("date", "日期"),
                                ("time", "时间"),
                                ("date_time", "日期时间"),
                                ("json", "JSON"),
                                ("null", "NULL"),
                            ],
                        ))
                        .child(input(
                            app,
                            &format!("object-filter-value-{i}-{j}"),
                            "过滤值",
                        ))
                        .child(icon_button(
                            "trash",
                            "移除过滤条件",
                            &format!("transfer-ui-filter-remove:{i}:{j}"),
                        ));
                    object = object.child(filter);
                }
                object = object.child(html::div().class("transfer-actions").child(icon_button(
                    "plus",
                    "添加过滤条件",
                    &format!("transfer-ui-filter-add:{i}"),
                )));
                body = body.child(object);
            }
            body = body.child(
                html::div()
                    .class("transfer-actions")
                    .child(icon_button("plus", "添加对象", "transfer-ui-object-add"))
                    .child(icon_button(
                        "trash",
                        "移除末尾对象",
                        "transfer-ui-object-remove",
                    )),
            );
        }
        if redis {
            body = body
                .child(select(
                    app,
                    "redis-selection",
                    "Redis key 选择方式",
                    &[("pattern", "按匹配模式"), ("keys", "指定 key")],
                ))
                .child(
                    if app.field(&app.transfer_key("redis-selection")) == "keys" {
                        redis_key_selection(app)
                    } else {
                        input(app, "pattern", "Redis key 匹配模式")
                    },
                )
                .child(input(app, "source-prefix", "来源前缀 Base64"))
                .child(input(app, "target-prefix", "目标前缀 Base64"))
                .child(html::span(
                    crate::i18n::tr("保留源绝对到期时间；已过期跳过，永久 key 保持永久。")
                        .into_owned(),
                ));
        }
    }
    if stage == "policy" {
        body = body.child(html::strong(crate::i18n::tr("执行策略").into_owned()));
        if !redis && matches!(mode, "copy" | "sql_dump" | "sql_restore") {
            body = body
                .child(select(
                    app,
                    "definer-mode",
                    "DEFINER 策略",
                    &[
                        ("current_user", "目标当前用户"),
                        ("preserve", "保留原账号"),
                        ("map", "显式映射"),
                    ],
                ))
                .child(input(
                    app,
                    "definer-map",
                    "DEFINER 映射（源=目标或 JSON 对象）",
                ))
                .child(select(
                    app,
                    "triggers-confirmed",
                    "确认已有触发器影响",
                    &[("false", "否"), ("true", "是")],
                ));
        }
        body = body
            .child(select(
                app,
                "conflict",
                "冲突策略",
                &[("insert", "插入"), ("skip", "跳过"), ("overwrite", "覆盖")],
            ))
            .child(select(
                app,
                "conversion",
                "转换错误",
                &[("stop", "停止"), ("skip", "跳过")],
            ))
            .child(select(
                app,
                "include-schema",
                "包含结构",
                &[("true", "是"), ("false", "否")],
            ))
            .child(select(
                app,
                "include-data",
                "包含数据",
                &[("true", "是"), ("false", "否")],
            ))
            .child(select(
                app,
                "create-missing",
                "创建缺失对象",
                &[("false", "否"), ("true", "是")],
            ));
        if app.workbench.transfer.project_mode {
            body = body
                .child(input(app, "preset-name", "共享迁移名称"))
                .child(input(app, "project-path", "项目内配置路径"))
                .child(button("保存到项目草稿", "transfer-ui-project-save"))
                .child(button("返回项目", "transfer-ui-project-return"));
        } else {
            body = body
                .child(input(app, "preset-name", "预设名称"))
                .child(button("保存预设", "transfer-ui-preset-save"))
                .child(button("查看预设", "transfer-ui-preset-list"))
                .child(button("删除选中预设", "transfer-ui-preset-delete"));
        }
        for (i, preset) in app.workbench.transfer.presets.iter().enumerate() {
            body = body.child(button(
                preset["name"].as_str().unwrap_or(""),
                &format!("transfer-ui-preset-load:{i}"),
            ));
        }
        if app.workbench.transfer.preset_offset > 0 {
            body = body.child(button("上一页", "transfer-ui-preset-prev"));
        }
        if app.workbench.transfer.preset_next {
            body = body.child(button("下一页", "transfer-ui-preset-next"));
        }
    }
    if stage == "preview" {
        if app.workbench.transfer.pending.is_some()
            || app.workbench.transfer.queued_prepare.is_some()
        {
            body =
                body.child(html::p(crate::i18n::tr("准备中，请稍候").into_owned()).class("muted"));
        } else if app.workbench.transfer.plan.is_none()
            && app.workbench.transfer.obsolete_plan.is_none()
        {
            body = body.child(html::p(crate::i18n::tr("尚未准备").into_owned()).class("muted"));
        }
        if let Some(plan) = &app.workbench.transfer.plan {
            body = body
                .child(html::strong(
                    crate::i18n::tr("准备结果、结构差异和样本").into_owned(),
                ))
                .child(detail_node(plan, "transfer-plan-diagnostic"));
            if let Some(progress) = plan.get("preparation_progress") {
                let phase = match progress["phase"].as_str().unwrap_or("") {
                    "connecting" => "连接与准备",
                    "indexing" => "建立文件指纹索引",
                    "preflight" => "预检与生成计划",
                    "sql_preflight" => "SQL 解析与预检",
                    _ => "准备中",
                };
                body = body.child(html::span(format!(
                    "{}: {}",
                    crate::i18n::tr("准备阶段"),
                    crate::i18n::tr(phase)
                )));
                if let Some(index) = progress["input_index"].as_u64() {
                    body = body.child(html::span(format!(
                        "{}: {} / {}",
                        crate::i18n::tr("当前输入文件"),
                        index,
                        progress["input_count"]
                    )));
                }
                if progress["phase"] == "indexing"
                    && let (Some(indexed), Some(total)) = (
                        progress["indexed_bytes"].as_u64(),
                        progress["input_bytes"].as_u64(),
                    )
                {
                    body = body.child(html::span(format!(
                        "{}: {} / {}",
                        crate::i18n::tr("已索引字节"),
                        indexed,
                        total
                    )));
                }
            }
            let mut sections = ui::tabs()
                .key("transfer-preview-sections")
                .active(app.workbench.transfer.plan_section.clone());
            for (id, title) in [
                ("items", "结构与对象"),
                ("samples", "数据样本"),
                ("diagnostics", "诊断"),
            ] {
                sections = sections.tab(id, crate::i18n::tr(title).into_owned());
            }
            body = body
                .child(button("刷新准备状态", "transfer-ui-refresh-plan"))
                .child(
                    sections.on_select(|value| Msg::Action(format!("transfer-ui-section:{value}"))),
                );
            if app.workbench.transfer.chunk_offset > 0 {
                body = body
                    .child(html::span(
                        crate::i18n::tr("正在读取完整内容…").into_owned(),
                    ))
                    .child(button("继续读取完整内容", "transfer-ui-page-read"));
            }
            if let Some(page) = &app.workbench.transfer.plan_page {
                body = body.child(html::span(format!(
                    "{} {} / {}",
                    crate::i18n::tr("位置"),
                    page["offset"],
                    page["total"]
                )));
                if let Some(items) = page["items"].as_array() {
                    for (i, item) in items.iter().enumerate() {
                        body = body.child(detail_node(item, &format!("transfer-plan-item-{i}")));
                        if app.workbench.transfer.plan_section == "items" {
                            if (mode == "sql_restore" && item["action"] == "object_scope")
                                || (mode != "sql_restore" && item["phase"] == "schema")
                            {
                                body = body.child(
                                    html::label()
                                        .child(
                                            html::checkbox("transfer-item", item_title(item))
                                                .checked(item["selected"] == true)
                                                .on_change(move |_| {
                                                    Msg::Action(format!(
                                                        "transfer-ui-page-item:{i}"
                                                    ))
                                                }),
                                        )
                                        .child(html::span(item_title(item))),
                                );
                            }
                            if item["destructive"] == true
                                && let Some(id) = item["id"].as_str()
                            {
                                let confirmed = app.workbench.transfer.confirmations.contains(id);
                                let action = format!("transfer-ui-confirm:{id}");
                                body = body.child(
                                    html::label()
                                        .child(
                                            html::checkbox("transfer-confirm", id.to_owned())
                                                .checked(confirmed)
                                                .on_change(move |_| Msg::Action(action.clone())),
                                        )
                                        .child(html::span(format!(
                                            "{} · {}",
                                            crate::i18n::tr("确认破坏性变更"),
                                            item_title(item)
                                        ))),
                                );
                            }
                        }
                    }
                }
                if !app.workbench.transfer.plan_pages.is_empty() {
                    body = body.child(button("上一页", "transfer-ui-page-prev"));
                }
                if page["next_offset"].as_u64().is_some() {
                    body = body.child(button("下一页", "transfer-ui-page-next"));
                }
            } else if let Some(items) = plan.get("items").and_then(Value::as_array) {
                for (i, item) in items.iter().enumerate() {
                    body = body.child(detail_node(item, &format!("transfer-plan-item-{i}")));
                    if (mode == "sql_restore" && item["action"] == "object_scope")
                        || (mode != "sql_restore" && item["phase"] == "schema")
                    {
                        body = body.child(
                            html::label()
                                .child(
                                    html::checkbox("transfer-item", item_title(item))
                                        .checked(item["selected"] == true)
                                        .on_change(move |_| {
                                            Msg::Action(format!("transfer-ui-item:{i}"))
                                        }),
                                )
                                .child(html::span(item_title(item))),
                        );
                    }
                    if item["destructive"] == true
                        && let Some(id) = item["id"].as_str()
                    {
                        let confirmed = app.workbench.transfer.confirmations.contains(id);
                        let action = format!("transfer-ui-confirm:{id}");
                        body = body.child(
                            html::label()
                                .child(
                                    html::checkbox("transfer-confirm", id.to_owned())
                                        .checked(confirmed)
                                        .on_change(move |_| Msg::Action(action.clone())),
                                )
                                .child(html::span(format!(
                                    "{} · {}",
                                    crate::i18n::tr("确认破坏性变更"),
                                    item_title(item)
                                ))),
                        );
                    }
                }
            }
        }
    }
    if stage == "report" {
        if let Some(run) = &app.workbench.transfer.run {
            body = body
                .child(html::strong(crate::i18n::tr("执行报告").into_owned()))
                .child(detail_node(run, "transfer-run-diagnostic"))
                .child(button("查看逐项结果", "transfer-ui-run-items"));
            if app
                .workbench
                .transfer
                .run_spec
                .as_ref()
                .is_some_and(|s| s["provider"] == "redis")
            {
                let run_mode = app
                    .workbench
                    .transfer
                    .run_spec
                    .as_ref()
                    .and_then(|s| s["mode"].as_str())
                    .unwrap_or("");
                let explanation = if run_mode == "copy" {
                    "已处理字节为逻辑内容字节（含 ZSet score，不含 Base64 和列表索引）；当前对象为源 key SHA-256 摘要。 / Processed bytes are logical payload bytes (including ZSet scores, excluding Base64 and list indexes); current object is the source key SHA-256 digest."
                } else {
                    "已处理字节为文件实际字节；当前对象为源 key SHA-256 摘要。 / Processed bytes are actual file bytes; current object is the source key SHA-256 digest."
                };
                body = body.child(html::span(explanation));
            }
            body = body
                .child(input(app, "report-path", "报告输出路径"))
                .child(select(
                    app,
                    "report-format",
                    "报告格式",
                    &[("json", "JSON"), ("csv", "CSV")],
                ))
                .child(select(
                    app,
                    "report-input",
                    "报告包含失败输入",
                    &[("false", "否"), ("true", "是")],
                ))
                .child(button("导出报告", "transfer-ui-report"))
                .child(html::span(
                    crate::i18n::tr("再次执行使用当前条件创建新快照，不包含待确认项。")
                        .into_owned(),
                ))
                .child(select(
                    app,
                    "retry-failed",
                    "续作失败项",
                    &[("true", "是"), ("false", "否")],
                ))
                .child(select(
                    app,
                    "retry-rolled-back",
                    "续作已回滚项",
                    &[("true", "是"), ("false", "否")],
                ))
                .child(select(
                    app,
                    "retry-not-sent",
                    "续作未发送项",
                    &[("true", "是"), ("false", "否")],
                ));
        }
        if let Some(page) = &app.workbench.transfer.items {
            body = body.child(html::strong(crate::i18n::tr("逐项结果").into_owned()));
            if let Some(items) = page["items"].as_array() {
                for (i, item) in items.iter().enumerate() {
                    body = body.child(detail_node(item, &format!("transfer-run-item-{i}")));
                }
            }
            if !app.workbench.transfer.run_pages.is_empty() {
                body = body.child(button("上一页", "transfer-ui-items-prev"));
            }
            if page["next"].as_u64().is_some() {
                body = body.child(button("下一页", "transfer-ui-items-next"));
            }
        }
    }
    root = root.child(body);
    let mut footer = html::div().class("transfer-footer");
    if stage == "report" {
        footer = footer.child(button("刷新执行状态", "transfer-ui-refresh-run"));
        if app.workbench.transfer.run.as_ref().is_some_and(|run| {
            matches!(
                run["state"].as_str(),
                Some("queued" | "running" | "cancelling")
            )
        }) {
            footer = footer.child(button("取消执行（已提交部分保留）", "transfer-ui-cancel"));
        }
        if app.workbench.transfer.run.is_some() {
            footer = footer
                .child(html::span("").class("spacer"))
                .child(button("再次准备失败范围", "transfer-ui-retry"));
        }
        return root.child(footer).into();
    }
    let ready = stage == "preview"
        && app.workbench.transfer.plan.as_ref().is_some_and(|p| {
            p.get("digest").and_then(Value::as_str).is_some()
                && p.get("spec").is_some_and(Value::is_object)
        });
    let (label, action) = if ready {
        ("确认执行", "transfer-ui-submit")
    } else {
        ("准备并预览", "transfer-ui-prepare")
    };
    if stage == "preview"
        && (app.workbench.transfer.plan.is_some() || app.workbench.transfer.obsolete_plan.is_some())
    {
        footer = footer.child(button("丢弃计划", "transfer-ui-discard"));
    }
    footer = footer.child(html::span("").class("spacer"));
    footer = footer.child(
        ui::button(crate::i18n::tr(label).into_owned())
            .variant(ui::ButtonVariant::Primary)
            .disabled(
                app.workbench.transfer.pending.is_some()
                    || app.workbench.transfer.queued_prepare.is_some()
                    || app.workbench.transfer.cleanup_wait,
            )
            .key(action)
            .on_click(Msg::Action(action.into())),
    );
    root = root.child(footer);
    if app.workbench.transfer.new_target.is_some() {
        let content = html::div()
            .class("transfer-group")
            .child(input(app, "new-target-name", "目标对象名称"))
            .child(input(
                app,
                "new-target-types",
                "目标类型（字段=类型或 JSON 数组）",
            ));
        let footer = html::div()
            .class("transfer-actions")
            .child(button("取消", "transfer-new-target-cancel"))
            .child(button("确认", "transfer-new-target-save"));
        let overlay: Node<Msg> = ui::overlay()
            .modal(true)
            .head(crate::i18n::tr("新建目标对象").into_owned())
            .end(icon_button("x", "关闭对话框", "transfer-new-target-cancel"))
            .foot(Node::from(footer))
            .child(content)
            .into();
        root = root.child(
            html::div()
                .class(format!("modal-layer {}", app.viewport_class()))
                .child(overlay),
        );
    }
    root.into()
}
