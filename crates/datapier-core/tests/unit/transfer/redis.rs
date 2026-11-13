use super::*;

#[test]
fn redis_export_rejects_incompatible_rows() {
    assert!(
        redis_csv_row(
            "aw==",
            "string",
            &[Value::Bytes("YQ==".into()), Value::Bytes("Yg==".into())]
        )
        .is_err()
    );
    assert!(redis_csv_row("aw==", "string", &[Value::Bytes("YQ==".into())]).is_ok());
}

#[test]
fn hash_csv_preserves_absolute_field_expiry_and_rejects_missing_expiry() {
    let row = vec![
        Value::Bytes("Zg==".into()),
        Value::Bytes("dg==".into()),
        Value::Integer("1900000000000".into()),
    ];
    let record = csv::StringRecord::from(redis_csv_row("aw==", "hash", &row).unwrap());
    let key = KeyImport {
        token: Json::Null,
        key: "aw==".into(),
        kind: "hash".into(),
        batch: Vec::new(),
        rows: 0,
        next_index: 0,
    };
    assert_eq!(csv_item(&record, &key).unwrap(), row);
    assert!(redis_csv_row("aw==", "hash", &row[..2]).is_err());
    let old = csv::StringRecord::from(vec![
        "1", "item", "aw==", "hash", "", "Zg==", "", "", "", "dg==", "",
    ]);
    assert!(csv_item(&old, &key).is_err());
}
