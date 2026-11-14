use super::*;

fn fixture() -> Value {
    json!({
        "columns": [{"id":"a","name":"first","nullable":true}, {"id":"b","name":"second","nullable":true}],
        "indexes": [], "constraints": []
    })
}

fn replay(root: &mut Value, operations: Vec<SchemaOperation>) {
    for op in operations {
        match op {
            SchemaOperation::Set { path, value } => *root.pointer_mut(&path).unwrap() = value,
            SchemaOperation::Insert { path, value } => root
                .pointer_mut(&path)
                .unwrap()
                .as_array_mut()
                .unwrap()
                .push(value),
            SchemaOperation::Remove { path, index } => {
                root.pointer_mut(&path)
                    .unwrap()
                    .as_array_mut()
                    .unwrap()
                    .remove(index);
            }
            SchemaOperation::Move { .. } => panic!("unexpected move"),
        }
    }
}

#[test]
fn composite_primary_preserves_existing_parts_and_options() {
    let mut root = fixture();
    root["indexes"] = json!([{"id":"pk", "name":"PRIMARY", "kind":"primary", "parts":[{"column":"first","expression":null,"prefix_length":8,"descending":true}],"visible":false,"method":"BTREE","options":["custom"]}]);
    let original = root["indexes"][0].clone();
    let ops = toggle_primary(&root, "b").unwrap();
    replay(&mut root, ops);
    assert_eq!(root["columns"][1]["nullable"], false);
    assert_eq!(root["indexes"][0]["parts"][0], original["parts"][0]);
    assert_eq!(root["indexes"][0]["options"], original["options"]);
    let ops = toggle_primary(&root, "b").unwrap();
    replay(&mut root, ops);
    assert_eq!(root["indexes"][0], original);
    let ops = toggle_primary(&root, "a").unwrap();
    replay(&mut root, ops);
    assert!(root["indexes"].as_array().unwrap().is_empty());
    let ops = toggle_primary(&root, "a").unwrap();
    replay(&mut root, ops);
    assert_eq!(root["indexes"][0]["kind"], "primary");
    assert_eq!(root["columns"][0]["nullable"], false);
    let duplicate = root["indexes"][0].clone();
    root["indexes"].as_array_mut().unwrap().push(duplicate);
    assert!(toggle_primary(&root, "a").is_err());
}

#[test]
fn primary_removes_null_default_and_preserves_other_defaults() {
    let mut root = fixture();
    root["columns"][0]["default"] = json!({"kind":"null"});
    root["columns"][1]["default"] = json!({"kind":"expression", "value":"0"});
    let first = toggle_primary(&root, "a").unwrap();
    replay(&mut root, first);
    assert_eq!(root["columns"][0]["nullable"], false);
    assert_eq!(root["columns"][0]["default"]["kind"], "unspecified");
    let second = toggle_primary(&root, "b").unwrap();
    replay(&mut root, second);
    assert_eq!(root["columns"][1]["nullable"], false);
    assert_eq!(
        root["columns"][1]["default"],
        json!({"kind":"expression", "value":"0"})
    );
    let remove = toggle_primary(&root, "a").unwrap();
    replay(&mut root, remove);
    assert_eq!(root["columns"][0]["default"]["kind"], "unspecified");
    assert_eq!(root["columns"][0]["nullable"], false);
}

#[test]
fn rename_updates_local_and_self_references_and_replays() {
    let mut root = fixture();
    root["indexes"] =
        json!([{"kind":"normal","parts":[{"column":"first","prefix_length":4,"descending":true}]}]);
    root["constraints"] = json!([
        {"kind":"foreign_key","columns":["first"],"referenced_database":"db","referenced_table":"t","referenced_columns":["first"]},
        {"kind":"foreign_key","columns":["second"],"referenced_database":"other","referenced_table":"t","referenced_columns":["first"]}
    ]);
    assert!(rename_column(&root, 0, "", Some("db"), "t").is_err());
    let ops = rename_column(&root, 0, "renamed", Some("db"), "t").unwrap();
    replay(&mut root, ops);
    assert_eq!(root["columns"][0]["name"], "renamed");
    assert_eq!(root["indexes"][0]["parts"][0]["column"], "renamed");
    assert_eq!(root["indexes"][0]["parts"][0]["prefix_length"], 4);
    assert_eq!(root["constraints"][0]["columns"][0], "renamed");
    assert_eq!(root["constraints"][0]["referenced_columns"][0], "renamed");
    assert_eq!(root["constraints"][1]["referenced_columns"][0], "first");
}

#[test]
fn empty_names_do_not_capture_unrelated_placeholders() {
    let mut root = fixture();
    root["columns"][0]["name"] = json!("");
    root["indexes"] = json!([{"kind":"normal","parts":[{"column":""}]}]);
    let ops = rename_column(&root, 0, "new", Some("db"), "t").unwrap();
    replay(&mut root, ops);
    assert_eq!(root["indexes"][0]["parts"][0]["column"], "");
}

#[test]
fn remove_rejects_dependencies_and_allows_unreferenced_column() {
    let mut root = fixture();
    root["indexes"] = json!([{"kind":"normal","parts":[{"column":"first"}]}]);
    assert!(remove_column(&root, 0, Some("db"), "t").is_err());
    let ops = remove_column(&root, 1, Some("db"), "t").unwrap();
    replay(&mut root, ops);
    assert_eq!(root["columns"].as_array().unwrap().len(), 1);
    root["indexes"] = json!([]);
    root["constraints"] = json!([{"kind":"foreign_key","columns":["other"],"referenced_database":"db","referenced_table":"t","referenced_columns":["first"]}]);
    assert!(remove_column(&root, 0, Some("db"), "t").is_err());
    root["constraints"][0]["referenced_database"] = json!("other");
    assert!(remove_column(&root, 0, Some("db"), "t").is_ok());
}

#[test]
fn expressions_are_preserved_when_columns_change() {
    let mut root = fixture();
    root["indexes"] =
        json!([{"kind":"normal","parts":[{"column":null,"expression":"lower(first)"}]}]);
    root["constraints"] = json!([{"kind":"check","expression":"first > 0"}]);
    root["columns"][1]["default"] = json!({"kind":"expression","value":"CURRENT_TIMESTAMP"});
    let operations = rename_column(&root, 0, "renamed", Some("db"), "t").unwrap();
    replay(&mut root, operations);
    assert_eq!(root["indexes"][0]["parts"][0]["expression"], "lower(first)");
    assert_eq!(root["constraints"][0]["expression"], "first > 0");
    let operations = remove_column(&root, 0, Some("db"), "t").unwrap();
    replay(&mut root, operations);
    assert_eq!(root["columns"][0]["default"]["value"], "CURRENT_TIMESTAMP");
}
