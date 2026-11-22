use datapier_core::{
    execution::migration::inspect_input,
    model::{ProviderKind, transfer::TransferInput},
};
use serde_json::json;
use std::fs;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[test]
fn csv_discovery_decodes_headers_and_names_headerless_positions() {
    let path = std::env::temp_dir().join(format!("datapier-inspect-{}", Uuid::new_v4()));
    let bytes: Vec<u8> = "姓名;编号\n张三;7\n"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    fs::write(&path, bytes).unwrap();
    let mut input: TransferInput = serde_json::from_value(json!({
        "path": path,
        "format": "csv",
        "csv": {"encoding":"utf-16le", "delimiter":";", "header":true}
    }))
    .unwrap();
    let cancel = CancellationToken::new();
    let result = inspect_input(&input, ProviderKind::Mysql, &cancel).unwrap();
    assert_eq!(result["columns"][0]["name"], "姓名");
    assert_eq!(result["columns"][1]["name"], "编号");
    input.csv.header = false;
    let result = inspect_input(&input, ProviderKind::Mysql, &cancel).unwrap();
    assert_eq!(result["columns"][0]["name"], "column_1");
    assert_eq!(result["columns"][1]["name"], "column_2");
    assert!(!result.to_string().contains("张三"));
    fs::remove_file(path).unwrap();
}

#[test]
fn sql_discovery_distinguishes_databases_and_merges_declarations() {
    let path = std::env::temp_dir().join(format!("datapier-inspect-{}", Uuid::new_v4()));
    fs::write(&path, "USE first_db; CREATE TABLE items (id INT); INSERT INTO items (id) VALUES (1); USE second_db; CREATE TABLE items (code INT);").unwrap();
    let input: TransferInput =
        serde_json::from_value(json!({"path": path, "format":"sql"})).unwrap();
    let result = inspect_input(&input, ProviderKind::Mysql, &CancellationToken::new()).unwrap();
    let objects = result["objects"].as_array().unwrap();
    let tables: Vec<_> = objects
        .iter()
        .filter(|object| object["kind"] == "table")
        .collect();
    assert_eq!(tables.len(), 2);
    assert_eq!(tables[0]["database"], "first_db");
    assert_eq!(tables[0]["columns"][0]["name"], "id");
    assert_eq!(tables[1]["database"], "second_db");
    assert_eq!(tables[1]["columns"][0]["name"], "code");
    fs::remove_file(path).unwrap();
}

#[test]
fn sql_indexes_use_their_table_scope() {
    let path = std::env::temp_dir().join(format!("datapier-inspect-{}", Uuid::new_v4()));
    fs::write(&path, "USE inventory; CREATE TABLE first_table (id INT); CREATE INDEX shared_idx ON first_table (id); CREATE INDEX shared_idx ON second_table (id); DROP INDEX shared_idx ON second_table;").unwrap();
    let input: TransferInput =
        serde_json::from_value(json!({"path": path, "format":"sql"})).unwrap();
    let result = inspect_input(&input, ProviderKind::Mysql, &CancellationToken::new()).unwrap();
    let objects = result["objects"].as_array().unwrap();
    assert_eq!(objects.len(), 2);
    assert_eq!(objects[0]["database"], "inventory");
    assert_eq!(objects[0]["name"], "first_table");
    assert_eq!(objects[0]["columns"][0]["type_name"], "");
    assert_eq!(objects[1]["database"], "inventory");
    assert_eq!(objects[1]["name"], "second_table");
    assert_eq!(objects[1]["kind"], "table");
    fs::remove_file(path).unwrap();
}
