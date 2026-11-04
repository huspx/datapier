use super::*;

#[path = "transfer/export.rs"]
mod export;
#[path = "transfer/mysql_and_json.rs"]
mod mysql_and_json;
#[path = "transfer/redis.rs"]
mod redis;

fn table() -> TableSchema {
    let mut generated = Column::new("doubled", "int");
    generated.generated = true;
    TableSchema {
        target: ResourceTarget {
            database: None,
            name: "t".into(),
            key_b64: None,
            kind: "table".into(),
        },
        columns: vec![
            Column::new("id", "int"),
            generated,
            Column::new("year", "year"),
            Column::new("text", "varchar"),
        ],
        keys: Vec::new(),
        ddl: String::new(),
        engine: "InnoDB".into(),
        details: Json::Null,
    }
}
fn names() -> Vec<String> {
    ["id", "doubled", "year", "text"]
        .into_iter()
        .map(str::to_owned)
        .collect()
}
