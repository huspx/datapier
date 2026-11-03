use super::*;

#[test]
fn generated_and_explicitly_excluded_columns_are_projected_consistently() {
    let p = mapped_columns(&names(), &json!({"column_mapping":{"text":null}}), &table()).unwrap();
    assert_eq!(p.indices, vec![0, 2]);
    assert_eq!(
        p.columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["id", "year"]
    );
    let csv = csv::StringRecord::from(vec!["7", "14", "2026", "ignored"]);
    let expected = vec![Value::Integer("7".into()), Value::Unsigned("2026".into())];
    assert_eq!(
        p.csv_row(&csv, &names(), "\\N", true, &HashSet::new())
            .unwrap(),
        expected
    );
    assert_eq!(
        p.project(vec![
            Value::Integer("7".into()),
            Value::Integer("14".into()),
            Value::Unsigned("2026".into()),
            Value::Text("ignored".into())
        ])
        .unwrap(),
        expected
    );
    assert!(
        mapped_columns(
            &names(),
            &json!({"column_mapping":{"id":"doubled"}}),
            &table()
        )
        .is_err()
    );
    crate::providers::mysql::validate_import_value(&expected[1], &p.columns[1]).unwrap();
}

#[test]
fn csv_distinguishes_null_and_escaped_literal() {
    let p = mapped_columns(&names(), &json!({}), &table()).unwrap();
    let row = csv::StringRecord::from(vec!["1", "2", "2026", "\\N"]);
    assert_eq!(
        p.csv_row(&row, &names(), "\\N", true, &HashSet::new())
            .unwrap()[2],
        Value::Null
    );
    let row = csv::StringRecord::from(vec!["1", "2", "2026", "\\\\N"]);
    assert_eq!(
        p.csv_row(&row, &names(), "\\N", true, &HashSet::new())
            .unwrap()[2],
        Value::Text("\\N".into())
    );
}

#[test]
fn unordered_json_fields_preserve_typed_rows() {
    let root = std::env::temp_dir().join(format!("datapier-transfer-test-{}", Uuid::new_v4()));
    fs::create_dir(&root).unwrap();
    let input = root.join("input.json");
    fs::write(&input, r#"{"rows":[[{"type":"unsigned","value":"2026"}]],"ignored":{"nested":[1,2]},"columns":[{"name":"year","type_name":"year"}],"version":1,"format":"datapier"}"#).unwrap();
    let doc = json_document(
        File::open(input).unwrap(),
        &root,
        "rows",
        &CancellationToken::new(),
    )
    .unwrap();
    assert_eq!(doc.columns.as_ref().unwrap()[0].name, "year");
    let mut reader = doc.data.reader().unwrap();
    reader.expect(b'[').unwrap();
    let mut first = true;
    assert!(reader.array_next(&mut first).unwrap());
    assert_eq!(
        reader.value::<Vec<Value>>().unwrap(),
        vec![Value::Unsigned("2026".into())]
    );
    assert!(!reader.array_next(&mut first).unwrap());
    reader.end_document().unwrap();
    drop(reader);
    drop(doc);
    fs::remove_dir_all(root).unwrap();
}
