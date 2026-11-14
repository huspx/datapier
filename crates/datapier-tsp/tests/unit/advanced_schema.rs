use super::*;

fn editor() -> SchemaEditor {
    let target = ResourceTarget {
        database: Some("db".into()),
        name: "items".into(),
        key_b64: None,
        kind: "table".into(),
    };
    let mut editor = SchemaEditor::new(json!("session"), target, "1".into(), None);
    editor.summary = Some(
        json!({"id":"plan","revision":1,"plan_digest":"digest","complete":true,"submittable":true,"total_items":2}),
    );
    editor
}

#[test]
fn query_requires_all_preview_pages_and_keeps_sql_order() {
    let mut editor = editor();
    let first = editor.preview_page(0).unwrap();
    let page = json!({"id":"plan","revision":1,"plan_digest":"digest","complete":true,"submittable":true,"total_items":2,"offset":0,"next":1,"steps":[{"sql":"DROP TRIGGER t","warnings":["first warning"]}]});
    assert!(editor.response(&first.tag, &Ok(page)));
    assert!(editor.query_sql().is_none());
    let second = editor.preview_page(1).unwrap();
    let page = json!({"id":"plan","revision":1,"plan_digest":"digest","complete":true,"submittable":true,"total_items":2,"offset":1,"next":null,"steps":[{"sql":"CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET @x = 1;"}]});
    assert!(editor.response(&second.tag, &Ok(page)));
    assert_eq!(
        editor.preview.as_ref().unwrap()["steps"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        editor.preview.as_ref().unwrap()["steps"][0]["warnings"][0],
        "first warning"
    );
    assert_eq!(
        editor.query_sql().as_deref(),
        Some("DROP TRIGGER t;\n\nCREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET @x = 1;")
    );
}

#[test]
fn schema_invalidation_keeps_intent_and_rejects_late_preview() {
    let mut editor = editor();
    editor.dirty = true;
    editor.draft.operations.push(SchemaOperation::Set {
        path: "/comment".into(),
        value: json!("new"),
    });
    let request = editor.preview_page(0).unwrap();
    editor.invalidate_schema();
    assert!(editor.restore_confirmation_required);
    assert_eq!(editor.draft.operations.len(), 1);
    assert!(editor.summary.is_none());
    assert!(!editor.response(&request.tag, &Ok(json!({}))));
}

fn parameter_editor(native_type: &str) -> SchemaEditor {
    let definition: SchemaDefinition = serde_json::from_value(json!({
        "kind":"table", "columns":[{"id":Uuid::new_v4(),"name":"value","native_type":native_type,
            "nullable":true,"default":{"kind":"unspecified"},"on_update":null,"auto_increment":false,
            "generated":null,"charset":null,"collation":null,"visible":true,"comment":"","extra":[]}],
        "indexes":[],"constraints":[],"engine":"InnoDB","charset":null,"collation":null,"comment":"","options":[]
    })).unwrap();
    let mut editor = editor();
    editor.summary = None;
    editor.draft.creating = true;
    editor.initial_definition = Some(definition.clone());
    editor.definition = Some(definition);
    let _ = editor.view();
    editor
}

fn replace_parameter(editor: &mut SchemaEditor, parameter: &str, value: &str) {
    let path = format!("/columns/0/native_type/@{parameter}");
    let len = editor
        .editors
        .borrow()
        .get(&path)
        .unwrap()
        .text
        .encode_utf16()
        .count();
    assert!(editor.edit(
        &format!("schema:{path}"),
        0,
        len,
        value,
        value.encode_utf16().count(),
        len
    ));
}

#[test]
fn type_parameters_update_draft_and_plan_with_suffix() {
    let mut editor = parameter_editor("varchar(64) CHARACTER SET utf8mb4");
    replace_parameter(&mut editor, "length", "128");
    let plan = editor.plan().unwrap();
    assert_eq!(
        plan.params["intent"]["definition"]["columns"][0]["native_type"],
        "varchar(128) CHARACTER SET utf8mb4"
    );
    let mut restored = serde_json::to_value(editor.initial_definition.as_ref().unwrap()).unwrap();
    let draft: SchemaDraft =
        serde_json::from_value(serde_json::to_value(&editor.draft).unwrap()).unwrap();
    for operation in &draft.operations {
        apply(&mut restored, operation).unwrap();
    }
    assert_eq!(
        restored["columns"][0]["native_type"],
        "varchar(128) CHARACTER SET utf8mb4"
    );

    let mut editor = parameter_editor("DECIMAL(12,2) UNSIGNED ZEROFILL");
    replace_parameter(&mut editor, "length", "18, 4");
    assert_eq!(
        editor.plan().unwrap().params["intent"]["definition"]["columns"][0]["native_type"],
        "DECIMAL(18,4) UNSIGNED ZEROFILL"
    );
}

#[test]
fn invalid_type_parameters_block_plan_until_corrected() {
    let mut editor = parameter_editor("DECIMAL(12,2) UNSIGNED");
    replace_parameter(&mut editor, "length", "12,31");
    assert!(editor.plan().is_none());
    assert_eq!(
        editor.editors.borrow()["/columns/0/native_type/@length"].text,
        "12,31"
    );
    replace_parameter(&mut editor, "length", "12,13");
    assert!(editor.plan().is_none());
    replace_parameter(&mut editor, "length", "18,13");
    assert!(editor.plan().is_some());
}

#[test]
fn temporal_precision_preserves_invalid_input_and_updates_plan() {
    for native_type in ["DATETIME", "TIMESTAMP(3)", "TIME(6)"] {
        for precision in ["0", "3", "6"] {
            let mut editor = parameter_editor(native_type);
            for invalid in ["7", "-1", "1.5", "3,2"] {
                replace_parameter(&mut editor, "length", invalid);
                assert!(editor.plan().is_none(), "{native_type}: {invalid}");
                assert_eq!(
                    editor.editors.borrow()["/columns/0/native_type/@length"].text,
                    invalid
                );
            }
            replace_parameter(&mut editor, "length", precision);
            let family = native_type.split('(').next().unwrap();
            let expected = if precision == "0" {
                family.to_owned()
            } else {
                format!("{family}({precision})")
            };
            assert_eq!(
                editor.plan().unwrap().params["intent"]["definition"]["columns"][0]["native_type"],
                expected
            );
        }
    }
}

#[test]
fn keyboard_fallback_edits_empty_schema_fields_and_respects_freeze() {
    let mut state = parameter_editor("VARCHAR(8)");
    state.action("schema-op:add:/constraints:foreign_key");
    state.action("schema-op:section:foreign_keys:");
    let _ = state.view();
    let mut app = crate::App::new("schema-keyboard".into());
    app.advanced_schema = Some(state);
    app.focus = "schema:/constraints/0/name".into();
    let mut paste = tern_sdk::Key::named("paste");
    paste.text = Some("外键😀".into());
    app.input_key(&paste);
    assert_eq!(
        serde_json::to_value(
            app.advanced_schema
                .as_ref()
                .unwrap()
                .definition
                .as_ref()
                .unwrap()
        )
        .unwrap()["constraints"][0]["name"],
        "外键😀"
    );
    app.input_key(&tern_sdk::Key::named("backspace"));
    assert_eq!(
        serde_json::to_value(
            app.advanced_schema
                .as_ref()
                .unwrap()
                .definition
                .as_ref()
                .unwrap()
        )
        .unwrap()["constraints"][0]["name"],
        "外键"
    );
    app.advanced_schema.as_mut().unwrap().summary = Some(json!({"id":"frozen"}));
    app.input_key(&paste);
    assert_eq!(
        serde_json::to_value(
            app.advanced_schema
                .as_ref()
                .unwrap()
                .definition
                .as_ref()
                .unwrap()
        )
        .unwrap()["constraints"][0]["name"],
        "外键"
    );
}

#[test]
fn mysql_type_parameter_boundaries_and_defaults() {
    for (kind, first, second, valid) in [
        ("VARCHAR", "0", "0", true),
        ("VARBINARY", "65535", "0", true),
        ("VARCHAR", "65536", "0", false),
        ("CHAR", "255", "0", true),
        ("BINARY", "256", "0", false),
        ("BIT", "0", "0", false),
        ("BIT", "64", "0", true),
        ("BIT", "65", "0", false),
        ("DATETIME", "0", "0", true),
        ("DATETIME", "6", "0", true),
        ("DATETIME", "7", "0", false),
        ("TIMESTAMP", "3", "0", true),
        ("TIME", "7", "0", false),
        ("DECIMAL", "1", "0", true),
        ("NUMERIC", "65", "30", true),
        ("DECIMAL", "66", "0", false),
        ("DECIMAL", "2", "3", false),
        ("DECIMAL", "65", "31", false),
    ] {
        let params = type_parameters(kind).unwrap();
        assert_eq!(
            type_parameter_error(&params, first, second).is_none(),
            valid,
            "{kind}({first},{second})"
        );
    }
    let params = type_parameters("DECIMAL(12) UNSIGNED").unwrap();
    assert_eq!(
        (params.first, params.second, params.suffix),
        ("12", "0", " UNSIGNED")
    );
    let params = type_parameters("DECIMAL").unwrap();
    assert_eq!((params.first, params.second), ("10", "0"));
    assert!(type_parameters("INT(11) UNSIGNED").is_none());
}

#[test]
fn type_switch_and_parameter_undo_update_draft() {
    let mut editor = parameter_editor("DECIMAL(12,2) UNSIGNED");
    replace_parameter(&mut editor, "length", "18,4");
    assert!(editor.undo("schema:/columns/0/native_type/@length"));
    assert_eq!(
        serde_json::to_value(editor.definition.as_ref().unwrap()).unwrap()["columns"][0]["native_type"],
        "DECIMAL(12,2) UNSIGNED"
    );

    assert!(editor.action("schema-op:column-type:/columns/0/native_type:NUMERIC"));
    assert_eq!(
        serde_json::to_value(editor.definition.as_ref().unwrap()).unwrap()["columns"][0]["native_type"],
        "NUMERIC(12,2) UNSIGNED"
    );
    assert!(editor.action("schema-op:column-type:/columns/0/native_type:TEXT"));
    assert_eq!(
        serde_json::to_value(editor.definition.as_ref().unwrap()).unwrap()["columns"][0]["native_type"],
        "TEXT"
    );
    let _ = editor.view();
    assert!(editor.action("schema-op:column-type:/columns/0/native_type:VARCHAR"));
    let _ = editor.view();
    replace_parameter(&mut editor, "length", "0");
    assert_eq!(
        editor.plan().unwrap().params["intent"]["definition"]["columns"][0]["native_type"],
        "VARCHAR(0)"
    );
}

#[test]
fn null_checkbox_clears_null_default_and_preserves_length() {
    let mut editor = parameter_editor("VARCHAR(64)");
    editor.action("schema-op:choice:/columns/0/default/kind:null");
    let _ = editor.view();
    replace_parameter(&mut editor, "length", "128");
    editor.action("schema-op:choice:/columns/0/nullable:false");
    let plan = editor.plan().unwrap();
    assert_eq!(
        plan.params["intent"]["definition"]["columns"][0]["nullable"],
        false
    );
    assert_eq!(
        plan.params["intent"]["definition"]["columns"][0]["default"]["kind"],
        "unspecified"
    );
    assert_eq!(
        plan.params["intent"]["definition"]["columns"][0]["native_type"],
        "VARCHAR(128)"
    );
}

#[test]
fn plan_selects_preview_and_switching_sections_preserves_frozen_draft() {
    let mut editor = parameter_editor("VARCHAR(64)");
    replace_parameter(&mut editor, "length", "128");
    let draft = serde_json::to_value(&editor.draft).unwrap();
    assert!(editor.action("schema-op:section:ddl_preview:"));
    assert_eq!(editor.section, "ddl_preview");
    assert!(editor.action("schema-op:section:columns:"));
    assert_eq!(serde_json::to_value(&editor.draft).unwrap(), draft);
    assert!(editor.summary.is_none());
    let request = editor.plan().unwrap();
    assert!(editor.response(&request.tag, &Ok(json!({"id":"plan", "revision":1, "plan_digest":"digest", "complete":true, "submittable":true, "total_items":1}))));
    assert_eq!(editor.section, "ddl_preview");
    assert!(editor.query_sql().is_none());
    assert!(editor.action("schema-op:section:columns:"));
    assert!(editor.summary.is_some());
    assert_eq!(serde_json::to_value(&editor.draft).unwrap(), draft);
    assert!(
        editor
            .apply_operation(SchemaOperation::Set {
                path: "/columns/0/name".into(),
                value: json!("other")
            })
            .is_err()
    );
    assert!(editor.action("schema-op:section:ddl_preview:"));
    assert_eq!(editor.section, "ddl_preview");
    assert!(!editor.has_pending_request());
}

#[test]
fn primary_actions_keep_invalid_input_and_preserve_composite_order() {
    let mut editor = parameter_editor("VARCHAR(64)");
    let mut second = template("/columns", "column").unwrap();
    second["name"] = json!("other");
    second["native_type"] = json!("INT");
    editor
        .apply_operation(SchemaOperation::Insert {
            path: "/columns".into(),
            value: second,
        })
        .unwrap();
    let _ = editor.view();
    let root = serde_json::to_value(editor.definition.as_ref().unwrap()).unwrap();
    let first_id = root["columns"][0]["id"].as_str().unwrap();
    let second_id = root["columns"][1]["id"].as_str().unwrap();
    replace_parameter(&mut editor, "length", "65536");
    let operations = editor.draft.operations.len();
    editor.action(&format!("schema-op:primary:{first_id}:"));
    editor.action("schema-op:down:/columns:0");
    assert_eq!(editor.draft.operations.len(), operations);
    assert_eq!(
        editor.editors.borrow()["/columns/0/native_type/@length"].text,
        "65536"
    );
    replace_parameter(&mut editor, "length", "128");
    editor.action(&format!("schema-op:primary:{first_id}:"));
    editor.action(&format!("schema-op:primary:{second_id}:"));
    let root = serde_json::to_value(editor.definition.as_ref().unwrap()).unwrap();
    assert_eq!(root["indexes"].as_array().unwrap().len(), 1);
    assert_eq!(root["indexes"][0]["parts"][0]["column"], "value");
    assert_eq!(root["indexes"][0]["parts"][1]["column"], "other");
    assert_eq!(root["columns"][0]["nullable"], false);
    assert_eq!(root["columns"][1]["nullable"], false);
    editor.action("schema-op:choice:/columns/0/nullable:true");
    assert_eq!(
        serde_json::to_value(editor.definition.as_ref().unwrap()).unwrap()["columns"][0]["nullable"],
        false
    );
    editor.action(&format!("schema-op:primary:{first_id}:"));
    let root = serde_json::to_value(editor.definition.as_ref().unwrap()).unwrap();
    assert_eq!(root["indexes"][0]["parts"][0]["column"], "other");
    editor.action(&format!("schema-op:primary:{second_id}:"));
    assert!(
        serde_json::to_value(editor.definition.as_ref().unwrap()).unwrap()["indexes"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn ordered_index_columns_preserve_part_options() {
    let mut editor = parameter_editor("INT");
    let index = json!({"id":Uuid::new_v4(),"name":"ix","kind":"normal","parts":[{"column":"value","expression":null,"prefix_length":4,"descending":true}],"visible":true,"method":null,"options":[]});
    editor
        .apply_operation(SchemaOperation::Insert {
            path: "/indexes".into(),
            value: index,
        })
        .unwrap();
    editor.action("schema-op:add:/indexes/0/parts:item");
    editor.action("schema-op:index-column:/indexes/0/parts/1/column:value");
    editor.action("schema-op:up:/indexes/0/parts:1");
    let root = serde_json::to_value(editor.definition.as_ref().unwrap()).unwrap();
    assert_eq!(root["indexes"][0]["parts"][1]["prefix_length"], 4);
    assert_eq!(root["indexes"][0]["parts"][1]["descending"], true);
}

#[test]
fn foreign_key_grid_actions_preserve_ordered_column_pairs() {
    let mut editor = parameter_editor("INT");
    editor.action("schema-op:add:/constraints:foreign_key");
    editor.action("schema-op:choice:/constraints/0/referenced_table:parent");
    for (path, first, second) in [
        ("columns", "value", "other"),
        ("referenced_columns", "id", "alt_id"),
    ] {
        editor.action(&format!("schema-op:add:/constraints/0/{path}:item"));
        editor.action(&format!("schema-op:choice:/constraints/0/{path}/0:{first}"));
        editor.action(&format!("schema-op:add:/constraints/0/{path}:item"));
        editor.action(&format!(
            "schema-op:choice:/constraints/0/{path}/1:{second}"
        ));
    }
    let root = serde_json::to_value(editor.definition.as_ref().unwrap()).unwrap();
    assert_eq!(root["constraints"][0]["columns"], json!(["value", "other"]));
    assert_eq!(
        root["constraints"][0]["referenced_columns"],
        json!(["id", "alt_id"])
    );
    assert_eq!(root["constraints"][0]["referenced_table"], "parent");
    let mut restored = serde_json::to_value(editor.initial_definition.as_ref().unwrap()).unwrap();
    for operation in &editor.draft.operations {
        apply(&mut restored, operation).unwrap();
    }
    assert_eq!(restored["constraints"], root["constraints"]);
}
