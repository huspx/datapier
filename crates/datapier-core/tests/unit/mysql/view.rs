use super::*;

#[test]
fn preserves_direct_aliases_and_marks_expressions_unwritable() {
    let (target, columns) = projection("SELECT t.id AS renamed, t.name, upper(t.name) AS display FROM db.items AS t WHERE t.id > 0", "db").unwrap();
    assert_eq!(target.database.as_deref(), Some("db"));
    assert_eq!(target.name, "items");
    assert_eq!(columns, vec![Some("id".into()), Some("name".into()), None]);
}

#[test]
fn rejects_join_and_subquery_in_view_proof() {
    assert!(projection("SELECT a.id FROM a JOIN b ON a.id = b.id", "db").is_err());
    assert!(projection("SELECT a.id FROM a WHERE EXISTS (SELECT 1 FROM b)", "db").is_err());
}

#[test]
fn structural_fingerprint_ignores_only_table_counter() {
    let mut schema = TableSchema {
        target: ResourceTarget { database: Some("db".into()), name: "t".into(), key_b64: None, kind: "table".into() },
        columns: vec![crate::model::Column::new("id", "int")],
        keys: vec![vec!["id".into()]],
        ddl: "CREATE TABLE `t` (`id` int NOT NULL AUTO_INCREMENT, PRIMARY KEY (`id`)) ENGINE=InnoDB AUTO_INCREMENT=3".into(),
        engine: "InnoDB".into(),
        details: json!({"indexes":[]}),
    };
    let baseline = catalog::schema_fingerprint(&schema).unwrap();
    schema.ddl = schema.ddl.replace("AUTO_INCREMENT=3", "AUTO_INCREMENT=4");
    assert_eq!(baseline, catalog::schema_fingerprint(&schema).unwrap());
    schema.keys.clear();
    assert_ne!(baseline, catalog::schema_fingerprint(&schema).unwrap());
    schema.keys.push(vec!["id".into()]);
    schema.ddl = schema.ddl.replace("int NOT NULL", "bigint NOT NULL");
    assert_ne!(baseline, catalog::schema_fingerprint(&schema).unwrap());
}
