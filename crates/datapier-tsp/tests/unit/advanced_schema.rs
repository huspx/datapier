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
    let page = json!({"id":"plan","revision":1,"plan_digest":"digest","complete":true,"submittable":true,"total_items":2,"offset":0,"next":1,"steps":[{"sql":"DROP TRIGGER t"}]});
    assert!(editor.response(&first.tag, &Ok(page)));
    assert!(editor.query_sql().is_none());
    let second = editor.preview_page(1).unwrap();
    let page = json!({"id":"plan","revision":1,"plan_digest":"digest","complete":true,"submittable":true,"total_items":2,"offset":1,"next":null,"steps":[{"sql":"CREATE TRIGGER t BEFORE INSERT ON x FOR EACH ROW SET @x = 1;"}]});
    assert!(editor.response(&second.tag, &Ok(page)));
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
