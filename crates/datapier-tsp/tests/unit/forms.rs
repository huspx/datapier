use super::*;

#[test]
fn filter_value_control_tracks_operator_and_uses_visible_label() {
    fn value_input(node: &Node<Msg>) -> Option<&Node<Msg>> {
        if node.kind() == "input"
            && node.props().get("aria").and_then(Value::as_str)
                == Some(
                    crate::i18n::format_message(
                        "{label}（必填）",
                        &[("label", crate::i18n::tr("值").as_ref())],
                    )
                    .as_str(),
                )
        {
            return Some(node);
        }
        node.child_nodes().iter().find_map(value_input)
    }

    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("filter-accessibility".into());
    app.schema = Some(json!({"columns":[Column::new("name", "text")]}));
    app.init_sql_form("filters");
    app.sql_form_action("sql-filter-select:0", &backend);
    let (body, _) = crate::forms::filter_form(&app);
    assert!(value_input(&body).is_some());
    app.sql_form_action("sql-op-select:8", &backend);
    let (body, _) = crate::forms::filter_form(&app);
    assert!(value_input(&body).is_none());
    app.sql_form_action("sql-op-select:0", &backend);
    let (body, _) = crate::forms::filter_form(&app);
    assert!(value_input(&body).is_some());
}

#[test]
fn advanced_filter_accepts_empty_arrays_and_null_operator_without_value() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("filter-validation".into());
    app.schema = Some(json!({"columns":[Column::new("name", "text")]}));
    app.init_sql_form("filters");
    app.field_set("filter", r#"[{"column":"name","op":"is_null"}]"#);
    app.field_set("sort", "[]");
    app.workbench.overlay = Some("filters".into());
    app.sql_form_action("sql-filter-apply-advanced", &backend);
    assert!(app.workbench.overlay.is_none());
    app.field_set("filter", r#"[{"column":"missing","op":"eq","value":""}]"#);
    app.workbench.overlay = Some("filters".into());
    app.sql_form_action("sql-filter-apply-advanced", &backend);
    assert_eq!(app.workbench.overlay.as_deref(), Some("filters"));
    assert_eq!(app.workbench.field_error.as_ref().unwrap().field, "filter");
}

#[test]
fn typed_insert_values_follow_mysql_write_rules() {
    let cases = [
        ("year", DataValue::Integer("2024".into()), true),
        ("year", DataValue::Integer("0".into()), true),
        ("year", DataValue::Integer("1900".into()), false),
        ("tinyint", DataValue::Bool(true), true),
        ("int unsigned", DataValue::Integer("42".into()), true),
        ("tinyint", DataValue::Integer("128".into()), false),
        ("decimal(4,2)", DataValue::Decimal("1.234".into()), false),
    ];
    for (kind, value, valid) in cases {
        let column = Column::new("value", kind);
        let values = BTreeMap::from([("value".into(), value)]);
        assert_eq!(validate_row(&values, &[column]).is_ok(), valid, "{kind}");
    }
}

#[test]
fn mysql_form_errors_keep_column_type_and_length_when_rendered() {
    for (kind, value, expected) in [
        (
            "varchar(2)",
            DataValue::Text("中文文".into()),
            "列 {name} 超过字符长度 {limit}",
        ),
        (
            "tinyint unsigned",
            DataValue::Unsigned("256".into()),
            "列 {name} ({type})：数值无效、超出范围或无法按目标精度和标度无损表示",
        ),
        (
            "decimal(4,2)",
            DataValue::Decimal("1.234".into()),
            "列 {name} ({type})：数值无效、超出范围或无法按目标精度和标度无损表示",
        ),
    ] {
        let column = Column::new("商品", kind);
        let values = BTreeMap::from([("商品".into(), value)]);
        let message = validate_row(&values, &[column])
            .unwrap_err()
            .render()
            .into_owned();
        let reason = crate::i18n::format_message(
            expected,
            &[("name", "商品"), ("type", kind), ("limit", "2")],
        );
        assert!(message.contains(&reason), "{message}");
    }
}

#[test]
fn database_diagnostics_remain_untranslated_even_with_validation_details() {
    let column = Column::new("amount", "decimal(4,2)");
    let mut error = AppError::invalid("database diagnostic")
        .with_details(json!({"validation":"numeric_range"}));
    error.database_code = Some("1234".into());
    let message = form_validation_error("列 {name}: {error}", &column, error);
    assert!(message.render().contains("database diagnostic"));
}

#[test]
fn insert_requires_schema_values_but_accepts_empty_text_and_null() {
    let (backend, mut requests, _responses) = Backend::scripted();
    let mut app = App::new("insert-validation".into());
    app.profile_form(None);
    app.field_set("access", "read_write");
    app.profile = Some(app.form_profile().unwrap());
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.target = Some(ResourceTarget {
        database: Some("fixture".into()),
        name: "rows".into(),
        kind: "table".into(),
        key_b64: None,
    });
    app.browse_result = true;
    app.grid = Some(GridState::new(serde_json::from_value(json!({
        "result":{"id":Uuid::new_v4(),"execution_id":Uuid::new_v4(),"columns":[],"kind":"rows","row_count":0,"complete":true,"summary":{}},
        "rows":[],"start":0,"next":null
    })).unwrap(), true));
    let mut required = Column::new("required", "text");
    required.nullable = false;
    app.schema = Some(
        json!({"columns":[required, Column::new("optional", "text")],"engine":"InnoDB","keys":[["required"]]}),
    );
    app.edit_token = Some(json!("token"));
    app.init_sql_form("row");
    app.dirty = false;
    app.sql_form_action("sql-row-add", &backend);
    assert!(app.pending_rows.is_empty());
    assert!(!app.dirty);
    assert_eq!(
        app.workbench.field_error.as_ref().unwrap().field,
        "sql-form-row-0"
    );
    app.workbench.sql_forms.row[0].mode = RowMode::Value;
    app.action("sql-row-add", &backend);
    assert!(app.pending_rows.is_empty());
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "workspace.draft_scope");
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"revision":request.params["revision"]})),
        },
        &backend,
    );
    assert_eq!(
        app.pending_rows[0].values.get("required"),
        Some(&DataValue::Text(String::new()))
    );
    app.pending_rows.clear();
    app.field_set("row_values", "{}");
    app.action("insert-row", &backend);
    assert!(app.pending_rows.is_empty());
    assert_eq!(
        app.workbench.field_error.as_ref().unwrap().field,
        "row_values"
    );
}

#[test]
fn form_text_preserves_empty_null_and_backslashes() {
    let column = Column::new("name", "varchar(100)");
    assert_eq!(
        grid::decode_form_value("", false, &column).unwrap(),
        DataValue::Text("".into())
    );
    assert_eq!(
        grid::decode_form_value("\\N", false, &column).unwrap(),
        DataValue::Text("\\N".into())
    );
    assert_eq!(
        grid::decode_form_value("", true, &column).unwrap(),
        DataValue::Null
    );
    let mut required = column;
    required.nullable = false;
    assert!(grid::decode_form_value("", true, &required).is_err());
}

#[test]
fn form_text_uses_schema_types_without_rounding() {
    assert_eq!(
        grid::decode_form_value(
            "12345678901234567890.0123",
            false,
            &Column::new("amount", "decimal(30,4) unsigned")
        )
        .unwrap(),
        DataValue::Decimal("12345678901234567890.0123".into())
    );
    assert_eq!(
        grid::decode_form_value("AP8=", false, &Column::new("flags", "bit(16)")).unwrap(),
        DataValue::Bytes("AP8=".into())
    );
    assert_eq!(
        grid::decode_form_value("12:34:56.123", false, &Column::new("time", "time(3)")).unwrap(),
        DataValue::Time("12:34:56.123".into())
    );
    assert!(grid::decode_form_value("not base64", false, &Column::new("bytes", "blob")).is_err());
    assert!(grid::decode_form_value("NaN", false, &Column::new("number", "double")).is_err());
}

#[test]
fn row_modes_preserve_empty_null_and_omitted_values_across_json_switches() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.schema = Some(
        json!({"columns":[Column::new("empty", "text"),Column::new("omitted", "text"),Column::new("nullable", "text")]}),
    );
    app.init_sql_form("row");
    app.workbench.sql_forms.row[0].mode = RowMode::Value;
    app.workbench.sql_forms.row[2].mode = RowMode::Null;
    app.sql_form_action("sql-row-advanced", &backend);
    let values: BTreeMap<String, DataValue> =
        serde_json::from_str(app.field("row_values")).unwrap();
    assert_eq!(values.get("empty"), Some(&DataValue::Text("".into())));
    assert_eq!(values.get("nullable"), Some(&DataValue::Null));
    assert!(!values.contains_key("omitted"));

    app.field_set(
        "row_values",
        json!({"empty":DataValue::Text("\\N".into()),"nullable":DataValue::Null}).to_string(),
    );
    app.sql_form_action("sql-row-advanced", &backend);
    let values = app.workbench.sql_forms.row_values().unwrap();
    assert_eq!(values.get("empty"), Some(&DataValue::Text("\\N".into())));
    assert_eq!(values.get("nullable"), Some(&DataValue::Null));
    assert!(!values.contains_key("omitted"));
}

#[test]
fn regression_cursor_keys_preserve_default_and_null_row_modes() {
    let mut app = App::new("test".into());
    app.schema =
        Some(json!({"columns":[Column::new("omitted", "text"),Column::new("nullable", "text")]}));
    app.init_sql_form("row");
    app.workbench.sql_forms.row[1].mode = RowMode::Null;
    for index in 0..2 {
        app.focus = format!("sql-form-row-{index}");
        for name in ["left", "right", "home", "end", "backspace", "delete"] {
            app.input_key(&tern_sdk::Key::named(name));
        }
        let mut select = tern_sdk::Key::named("a");
        select.ctrl = true;
        app.input_key(&select);
    }
    let values = app.workbench.sql_forms.row_values().unwrap();
    assert!(!values.contains_key("omitted"));
    assert_eq!(values.get("nullable"), Some(&DataValue::Null));
    let mut typed = tern_sdk::Key::named("x");
    typed.text = Some("x".into());
    app.input_key(&typed);
    assert_eq!(
        app.workbench
            .sql_forms
            .row_values()
            .unwrap()
            .get("nullable"),
        Some(&DataValue::Text("x".into()))
    );
}

#[test]
fn regression_like_patterns_keep_text_for_non_text_columns() {
    for kind in ["bigint", "json"] {
        for op in ["like", "not_like"] {
            let state = SqlForms {
                columns: vec![Column::new("value", kind)],
                filter_column: Some(0),
                filter_op: OPERATORS.iter().position(|(name, _)| *name == op).unwrap(),
                filter_value: Editor::new("12%_enabled"),
                ..SqlForms::default()
            };
            let (filters, _) = state.filter_values().unwrap();
            assert_eq!(
                filters[0]["value"],
                json!(DataValue::Text("12%_enabled".into()))
            );
        }
    }
}

#[test]
fn multiple_advanced_filters_are_not_silently_replaced_by_a_single_condition() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.schema = Some(json!({"columns":[Column::new("id", "bigint")]}));
    let filters = json!([{"column":"id","op":"gte","value":{"type":"integer","value":"5"}},{"column":"id","op":"lte","value":{"type":"integer","value":"10"}}]);
    app.field_set("filter", filters.to_string());
    app.init_sql_form("filters");
    assert!(app.workbench.sql_forms.filter_advanced);
    app.sql_form_action("sql-filter-advanced", &backend);
    assert!(app.workbench.sql_forms.filter_advanced);
    assert_eq!(
        serde_json::from_str::<Value>(app.field("filter")).unwrap(),
        filters
    );
}
