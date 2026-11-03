use super::*;

fn assert_consumers(ty: &str, value: Value, accepted: bool) {
    let column = Column::new("amount", ty);
    let schema = TableSchema {
        target: ResourceTarget {
            database: Some("db".into()),
            name: "t".into(),
            key_b64: None,
            kind: "table".into(),
        },
        columns: vec![column.clone()],
        keys: Vec::new(),
        ddl: String::new(),
        engine: "InnoDB".into(),
        details: Json::Null,
    };
    let change = RowChange {
        operation: "insert".into(),
        key: BTreeMap::new(),
        before: BTreeMap::new(),
        values: [(column.name.clone(), value.clone())].into_iter().collect(),
    };
    assert_eq!(
        validate_import_value(&value, &column).is_ok(),
        accepted,
        "import {ty}: {value:?}"
    );
    assert_eq!(
        validate(&schema, &change).is_ok(),
        accepted,
        "write {ty}: {value:?}"
    );
}

#[test]
fn decimal_precision_and_scale_are_lossless_for_import_and_write() {
    for (ty, value, accepted) in [
        ("decimal(5,2)", "999.99", true),
        ("decimal(5,2)", "-999.9900", true),
        ("decimal(5,2)", "1.234", false),
        ("decimal(5,2)", "1000", false),
        ("decimal(5,2)", "0.0010", false),
        ("decimal(5,2)", "1e2", true),
        ("decimal(5,2)", "1e-3", false),
        ("decimal(2,2)", "0.99", true),
        ("decimal(2,2)", "1", false),
        ("decimal(5,2) unsigned", "-0.01", false),
        ("decimal(5,2) unsigned", "-0.000", true),
        ("decimal(5,2)", "invalid", false),
        ("decimal", "1", false),
        ("decimal(2,3)", "1", false),
    ] {
        assert_consumers(ty, Value::Decimal(value.into()), accepted);
    }
}

#[test]
fn integer_width_sign_and_year_are_checked_for_import_and_write() {
    for (ty, value, accepted) in [
        ("tinyint", "-128", true),
        ("tinyint", "127", true),
        ("tinyint", "128", false),
        ("tinyint unsigned", "-1", false),
        ("tinyint unsigned", "255", true),
        ("tinyint unsigned", "256", false),
        ("smallint", "-32769", false),
        ("mediumint", "8388607", true),
        ("mediumint", "8388608", false),
        ("int unsigned", "4294967295", true),
        ("int unsigned", "4294967296", false),
        ("bigint", "-9223372036854775808", true),
        ("year", "0", true),
        ("year", "1901", true),
        ("year", "2155", true),
        ("year", "1900", false),
        ("year", "2156", false),
        ("year", "70", false),
    ] {
        assert_consumers(ty, Value::Integer(value.into()), accepted);
    }
    for (ty, value, accepted) in [
        ("bigint unsigned", "18446744073709551615", true),
        ("bigint", "18446744073709551615", false),
        ("int", "2147483647", true),
        ("int", "2147483648", false),
        ("mediumint unsigned", "16777215", true),
        ("mediumint unsigned", "16777216", false),
        ("year", "2026", true),
        ("year", "99", false),
        ("year", "2156", false),
    ] {
        assert_consumers(ty, Value::Unsigned(value.into()), accepted);
    }
}

#[test]
fn bit_width_is_checked_for_import_and_write() {
    for (ty, value, accepted) in [
        ("bit(1)", "1", true),
        ("bit(1)", "2", false),
        ("bit(9)", "511", true),
        ("bit(9)", "512", false),
        ("bit(64)", "18446744073709551615", true),
        ("bit(65)", "1", false),
    ] {
        assert_consumers(ty, Value::Unsigned(value.into()), accepted);
    }
    for (ty, bytes, accepted) in [
        ("bit(1)", vec![1], true),
        ("bit(1)", vec![2], false),
        ("bit(9)", vec![1, 255], true),
        ("bit(9)", vec![2, 0], false),
        ("bit(9)", vec![0, 1, 255], true),
        ("bit(64)", vec![255; 8], true),
        ("bit(64)", vec![1; 9], false),
        ("bit(1)", vec![], false),
    ] {
        assert_consumers(ty, Value::Bytes(STANDARD.encode(bytes)), accepted);
    }
    assert_consumers("bit(8)", Value::Bytes("invalid!".into()), false);
}

#[test]
fn import_validation_exposes_localizable_details() {
    let text = Column::new("title", "varchar(2)");
    let error = validate_import_value(&Value::Text("字字 ".into()), &text).unwrap_err();
    assert_eq!(
        *error.details,
        json!({"validation":"character_length", "column":"title", "type":"varchar(2)", "limit":2})
    );
    let numeric = Column::new("amount", "tinyint");
    let error = validate_import_value(&Value::Integer("128".into()), &numeric).unwrap_err();
    assert_eq!(
        *error.details,
        json!({"validation":"numeric_range", "column":"amount", "type":"tinyint"})
    );
    let error = validate_import_value(&Value::Text("bad".into()), &numeric).unwrap_err();
    assert_eq!(error.details["validation"], "type_mismatch");
    let bit = Column::new("flags", "bit(1)");
    let error = validate_import_value(&Value::Bytes(STANDARD.encode([2])), &bit).unwrap_err();
    assert_eq!(error.details["validation"], "bit_width");
    let mut generated = text;
    generated.generated = true;
    let error = validate_import_value(&Value::Text("ok".into()), &generated).unwrap_err();
    assert_eq!(error.details["validation"], "generated_column");
}
