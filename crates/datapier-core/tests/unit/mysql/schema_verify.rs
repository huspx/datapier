use super::*;
use crate::model::schema::{IndexPart, SchemaDefinition};
use serde_json::json;

fn expected_table() -> TableDefinition {
    serde_json::from_value(json!({
        "columns":[{"id":"parent_id","name":"parent_id","native_type":"INT","nullable":true,
            "default":{"kind":"null"},"on_update":null,"auto_increment":false,"generated":null,
            "charset":null,"collation":null,"visible":true,"comment":"","extra":[]}],
        "indexes":[],
        "constraints":[{"kind":"foreign_key","id":"fk","name":"fk_parent","columns":["parent_id"],
            "referenced_database":"db","referenced_table":"parent","referenced_columns":["id"],
            "on_update":"RESTRICT","on_delete":"RESTRICT"}],
        "engine":"InnoDB","charset":null,"collation":null,"comment":"","options":[]
    }))
    .unwrap()
}

fn implicit_index() -> IndexDefinition {
    IndexDefinition {
        id: "fk_parent".into(),
        name: "fk_parent".into(),
        kind: IndexKind::Normal,
        parts: vec![IndexPart {
            column: Some("parent_id".into()),
            expression: None,
            prefix_length: None,
            descending: false,
        }],
        visible: true,
        method: None,
        options: vec![],
    }
}

fn equal(expected: TableDefinition, actual: TableDefinition) -> bool {
    super::super::equivalent(
        &SchemaDefinition::Table(expected),
        &SchemaDefinition::Table(actual),
        "db",
        None,
    )
}

#[test]
fn readback_accepts_only_the_required_foreign_key_support_index() {
    let expected = expected_table();
    let mut actual = expected.clone();
    actual.indexes.push(implicit_index());
    assert!(equal(expected.clone(), actual.clone()));
    for mismatch in 0..8 {
        let mut altered = actual.clone();
        match mismatch {
            0 => altered.indexes[0].name = "unrequested".into(),
            1 => altered.indexes[0].parts[0].column = Some("other".into()),
            2 => altered.indexes[0].parts[0].prefix_length = Some(4),
            3 => altered.indexes[0].parts[0].descending = true,
            4 => altered.indexes[0].kind = IndexKind::Unique,
            5 => altered.indexes[0].visible = false,
            6 => altered.indexes[0].method = Some("HASH".into()),
            _ => altered.columns[0].nullable = false,
        }
        assert!(!equal(expected.clone(), altered), "mismatch {mismatch}");
    }
}

#[test]
fn existing_support_index_does_not_allow_an_extra_index() {
    let mut expected = expected_table();
    let mut existing = implicit_index();
    existing.name = "explicit_support".into();
    existing.id = existing.name.clone();
    expected.indexes.push(existing);
    let mut actual = expected.clone();
    actual.indexes.push(implicit_index());
    assert!(!equal(expected, actual));
}

#[test]
fn readback_normalizes_mysql_type_aliases_without_erasing_differences() {
    for (requested, returned) in [
        ("BOOLEAN", "tinyint(1)"),
        ("BOOL", "tinyint(1)"),
        ("NUMERIC(12, 2)", "decimal(12,2)"),
        ("DEC(12)", "decimal(12,0)"),
        ("FIXED", "decimal(10,0)"),
        ("INT4 SIGNED", "int"),
        ("INT1 UNSIGNED", "tinyint(3) unsigned"),
        ("DOUBLE PRECISION", "double"),
        ("FLOAT8", "double"),
        ("CHARACTER VARYING (20)", "varchar(20)"),
    ] {
        let mut expected = expected_table();
        let mut actual = expected.clone();
        expected.columns[0].native_type = requested.into();
        actual.columns[0].native_type = returned.into();
        assert!(equal(expected, actual), "{requested} vs {returned}");
    }
    assert_ne!(
        normalized_type("numeric(12,2)"),
        normalized_type("decimal(12,3)")
    );
    assert_ne!(normalized_type("enum('A')"), normalized_type("enum('a')"));
    assert_ne!(normalized_type("int unsigned"), normalized_type("int"));
}
