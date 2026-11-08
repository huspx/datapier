use super::*;
use datapier_core::model::AppError;
use datapier_core::model::schema::TableDefinition;

fn editor() -> SchemaEditor {
    let table = TableDefinition {
        columns: vec![],
        indexes: vec![],
        constraints: vec![ConstraintDefinition::ForeignKey {
            id: "fk-1".into(),
            name: "fk".into(),
            columns: vec![],
            referenced_database: String::new(),
            referenced_table: "parent".into(),
            referenced_columns: vec![],
            on_update: "RESTRICT".into(),
            on_delete: "RESTRICT".into(),
        }],
        engine: String::new(),
        charset: None,
        collation: None,
        comment: String::new(),
        options: vec![],
    };
    SchemaEditor::new(
        json!("session-1"),
        ResourceTarget {
            database: Some("db".into()),
            name: "child".into(),
            key_b64: None,
            kind: "table".into(),
        },
        String::new(),
        Some(SchemaDefinition::Table(table)),
    )
}

fn token(request: &SchemaRequest) -> &str {
    request.tag.strip_prefix("schema-ref/").unwrap()
}

fn page(names: &[&str], cursor: Option<&str>, complete: bool) -> Value {
    let resources: Vec<_> = names
        .iter()
        .map(|name| json!({"name": name, "kind": "table"}))
        .collect();
    json!({"resources": resources, "cursor": cursor, "complete": complete})
}

fn table(editor: &mut SchemaEditor) -> &mut TableDefinition {
    match editor.definition.as_mut().unwrap() {
        SchemaDefinition::Table(table) => table,
        _ => panic!("expected table"),
    }
}

#[test]
fn loads_all_pages_before_marking_tables_complete() {
    let editor = editor();
    let mut state = ReferenceState::default();
    let first = state.load(&editor, "fk-1", "tables").unwrap();
    assert_eq!(first.method, "resource.list");
    assert_eq!(first.params["database"], "db");
    let next = state
        .response(
            &editor,
            token(&first),
            &Ok(page(&["a", "b"], Some("next"), false)),
        )
        .unwrap();
    assert_eq!(next.params["cursor"], "next");
    assert!(state.entries["fk-1"].tables_loading);
    assert!(!state.entries["fk-1"].tables_complete);
    assert!(
        state
            .response(&editor, token(&next), &Ok(page(&["c"], None, true)))
            .is_none()
    );
    let entry = &state.entries["fk-1"];
    assert_eq!(entry.tables, ["a", "b", "c"]);
    assert!(entry.tables_complete);
    assert!(!entry.tables_loading);
}

#[test]
fn late_responses_cannot_cross_session_target_or_deleted_foreign_key() {
    for changed in 0..3 {
        let mut editor = editor();
        let mut state = ReferenceState::default();
        let old = state.load(&editor, "fk-1", "tables").unwrap();
        match changed {
            0 => editor.session = json!("session-2"),
            1 => match &mut table(&mut editor).constraints[0] {
                ConstraintDefinition::ForeignKey {
                    referenced_table, ..
                } => *referenced_table = "other".into(),
                _ => unreachable!(),
            },
            _ => table(&mut editor).constraints.clear(),
        }
        state.sync(&editor);
        let new = if changed != 2 {
            state.load(&editor, "fk-1", "tables")
        } else {
            None
        };
        assert!(
            state
                .response(&editor, token(&old), &Ok(page(&["stale"], None, true)))
                .is_none()
        );
        if let Some(new) = new {
            assert!(state.entries["fk-1"].tables.is_empty());
            assert!(state.entries["fk-1"].tables_loading);
            state.response(&editor, token(&new), &Ok(page(&["fresh"], None, true)));
            assert_eq!(state.entries["fk-1"].tables, ["fresh"]);
        } else {
            assert!(state.entries.is_empty());
        }
    }
}

#[test]
fn describes_columns_and_ignores_self_reference() {
    let mut editor = editor();
    let mut state = ReferenceState::default();
    let request = state.load(&editor, "fk-1", "columns").unwrap();
    assert_eq!(request.method, "resource.describe");
    assert_eq!(request.params["target"]["database"], "db");
    assert_eq!(request.params["target"]["name"], "parent");
    let result = json!({"target":{"database":"db","name":"parent","kind":"table"},
            "columns":[{"name":"id","type_name":"int"},{"name":"code","type_name":"text"}],
            "keys":[],"ddl":"","engine":"","details":{}});
    state.response(&editor, token(&request), &Ok(result));
    assert_eq!(state.entries["fk-1"].columns, ["id", "code"]);
    assert!(state.entries["fk-1"].columns_complete);
    if let ConstraintDefinition::ForeignKey {
        referenced_table, ..
    } = &mut table(&mut editor).constraints[0]
    {
        *referenced_table = "child".into();
    }
    assert!(state.load(&editor, "fk-1", "columns").is_none());
    assert!(state.entries.is_empty());
}

#[test]
fn errors_are_visible_and_retry_starts_again() {
    let editor = editor();
    let mut state = ReferenceState::default();
    let request = state.load(&editor, "fk-1", "tables").unwrap();
    let next = state
        .response(
            &editor,
            token(&request),
            &Ok(page(&["partial"], Some("next"), false)),
        )
        .unwrap();
    state.response(
        &editor,
        token(&next),
        &Err(AppError::invalid("network error")),
    );
    let entry = &state.entries["fk-1"];
    assert!(!entry.tables_loading && !entry.tables_complete);
    assert!(
        entry
            .tables_error
            .as_deref()
            .unwrap()
            .contains("network error")
    );
    let retry = state.load(&editor, "fk-1", "tables").unwrap();
    assert!(retry.params["cursor"].is_null());
    assert!(state.entries["fk-1"].tables.is_empty());
    assert!(state.entries["fk-1"].tables_error.is_none());
    state.response(&editor, token(&retry), &Ok(page(&["complete"], None, true)));
    assert_eq!(state.entries["fk-1"].tables, ["complete"]);
    assert!(state.entries["fk-1"].tables_complete);
}
