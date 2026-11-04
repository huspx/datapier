use super::*;

#[test]
fn redis_export_rejects_changed_type_and_incompatible_rows() {
    assert!(validate_redis_browse_type("string", &json!({"type":"hash"})).is_err());
    assert!(validate_redis_browse_type("string", &json!({})).is_err());
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
