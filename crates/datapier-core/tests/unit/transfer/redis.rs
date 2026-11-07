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
