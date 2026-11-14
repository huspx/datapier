use super::*;

fn view_schema() -> TableSchema {
    let mut id = Column::new("id", "int");
    id.nullable = false;
    let name = Column::new("name", "varchar(40)");
    TableSchema {
        target: ResourceTarget {
            database: Some("db".into()),
            name: "v".into(),
            key_b64: None,
            kind: "view".into(),
        },
        columns: vec![id, name],
        keys: vec![vec!["id".into()]],
        ddl: String::new(),
        engine: "InnoDB".into(),
        details: json!({"view_mapping":{"insert":true,"update":true,"delete":true,"required_insert_columns":["name"]}}),
    }
}

#[test]
fn view_update_checks_complete_before_in_the_write() {
    let schema = view_schema();
    let key = Value::Integer("1".into());
    let change = RowChange {
        operation: "update".into(),
        key: [("id".into(), key.clone())].into_iter().collect(),
        before: [
            ("id".into(), key),
            ("name".into(), Value::Text("old".into())),
        ]
        .into_iter()
        .collect(),
        values: [("name".into(), Value::Text("new".into()))]
            .into_iter()
            .collect(),
    };
    validate(&schema, &change).unwrap();
    let plan = plan_change(&schema, &change, true).unwrap();
    assert!(plan.check.is_none());
    let write = plan.write.unwrap();
    assert!(write.text.starts_with("UPDATE `db`.`v` SET "));
    assert!(write.text.contains("`id`"));
    assert!(write.text.contains("`name`"));
    assert_eq!(write.parameters.len(), 3);
}

#[test]
fn view_insert_rejects_missing_required_column() {
    let schema = view_schema();
    let change = RowChange {
        operation: "insert".into(),
        key: BTreeMap::new(),
        before: BTreeMap::new(),
        values: [("id".into(), Value::Integer("1".into()))]
            .into_iter()
            .collect(),
    };
    assert!(validate(&schema, &change).is_err());
}
