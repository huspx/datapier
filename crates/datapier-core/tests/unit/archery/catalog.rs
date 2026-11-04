use super::*;

fn table(completeness: &str) -> TableSchema {
    TableSchema {
        target: ResourceTarget {
            database: Some("app".into()),
            name: "order`items".into(),
            key_b64: None,
            kind: "table".into(),
        },
        columns: vec![Column::new("id", "int"), Column::new("name", "varchar(64)")],
        keys: vec![vec!["id".into()]],
        ddl: String::new(),
        engine: String::new(),
        details: json!({"columns_completeness":completeness}),
    }
}

#[test]
fn browse_quotes_names_and_values_and_adds_stable_key() {
    let sql = browse_sql(
        &table("complete"),
        &json!({
            "filters":[{"column":"name","op":"eq","value":"O'Brien"}],
            "sort":[{"column":"name","direction":"desc"}]
        }),
    )
    .unwrap();
    assert_eq!(
        sql,
        "SELECT `id`,`name` FROM `app`.`order``items` WHERE `name` = 'O''Brien' ORDER BY `name` DESC, `id` ASC"
    );
}

#[test]
fn browse_rejects_unconfirmed_structure_and_unsafe_expressions() {
    assert_eq!(
        browse_sql(&table("unknown"), &json!({})).unwrap_err().code,
        "metadata_incomplete"
    );
    assert_eq!(
        browse_sql(&table("incomplete"), &json!({}))
            .unwrap_err()
            .code,
        "metadata_incomplete"
    );
    assert!(
        browse_sql(
            &table("complete"),
            &json!({"filters":[{"column":"name","value":"x\\' OR 1=1"}]})
        )
        .is_err()
    );
    assert!(
        browse_sql(
            &table("complete"),
            &json!({"filters":[{"column":"id) OR 1=1","value":1}]})
        )
        .is_err()
    );
    assert!(
        browse_sql(
            &table("complete"),
            &json!({"sort":[{"column":"id","direction":"desc; DROP TABLE x"}]})
        )
        .is_err()
    );
    assert!(
        browse_sql(
            &table("complete"),
            &json!({"sort":[{"column":"id"},{"column":"id"}]})
        )
        .is_err()
    );
}

#[test]
fn literal_types_remain_bounded_and_do_not_interpret_sql() {
    assert_eq!(literal(&json!("O'Brien")).unwrap(), "'O''Brien'");
    assert_eq!(
        literal(&json!({"type":"bytes","value":"AP8="})).unwrap(),
        "X'00ff'"
    );
    assert!(literal(&json!({"type":"integer","value":"1 OR 1=1"})).is_err());
    assert!(literal(&json!({"type":"float","value":"NaN"})).is_err());
    assert!(literal(&Json::Null).is_err());
    assert!(identifier("bad\0name").is_err());
    assert!(identifier("bad\\name").is_err());
}

#[test]
fn web_envelope_does_not_turn_failures_into_empty_resources() {
    assert_eq!(
        web_data(json!({"status":0,"data":["one"]})).unwrap(),
        json!(["one"])
    );
    assert_eq!(
        web_data(json!({"status":1,"msg":"denied","data":[]}))
            .unwrap_err()
            .code,
        "database"
    );
    assert_eq!(
        web_data(json!({"data":[]})).unwrap_err().code,
        "remote_protocol"
    );
    assert_eq!(
        web_data(json!({"status":0})).unwrap_err().code,
        "remote_protocol"
    );
}

#[test]
fn metadata_completeness_requires_unmasked_count_evidence() {
    assert_eq!(
        metadata_completeness(Some(3), 3, Some(false), Some(false)),
        "complete"
    );
    assert_eq!(
        metadata_completeness(Some(4), 3, Some(false), Some(false)),
        "incomplete"
    );
    assert_eq!(
        metadata_completeness(None, 3, Some(false), Some(false)),
        "unknown"
    );
    assert_eq!(
        metadata_completeness(Some(3), 3, None, Some(false)),
        "unknown"
    );
    assert_eq!(
        metadata_completeness(Some(3), 3, Some(false), Some(true)),
        "unknown"
    );
    assert_eq!(
        metadata_completeness(Some(10_000), 10_000, Some(false), Some(false)),
        "unknown"
    );
}
