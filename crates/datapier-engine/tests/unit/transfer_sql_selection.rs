//! Engine SQL 恢复对象选择联动回归。
use super::*;
use datapier_core::model::{ResourceTarget, transfer::TransferPlanItem};

fn object_item(id: &str, name: &str, action: &str) -> TransferPlanItem {
    let object = ResourceTarget {
        database: Some("db".into()),
        name: name.into(),
        key_b64: None,
        kind: "table".into(),
    };
    TransferPlanItem {
        id: id.into(),
        phase: "restore".into(),
        action: action.into(),
        source: Some(object.clone()),
        target: Some(object),
        sql: None,
        destructive: false,
        selected: true,
        precondition: None,
        notes: vec![],
    }
}

#[test]
fn selection_tracks_whole_object_and_database_context() {
    let mut items = vec![
        object_item("a", "a", "object_scope"),
        object_item("a:create", "a", "create_table"),
        object_item("a:data", "a", "insert"),
        object_item("b", "b", "object_scope"),
        object_item("b:data", "b", "insert"),
    ];
    let mut database = object_item("db", "db", "create_database");
    database.phase = "database".into();
    database.source = None;
    items.push(database);
    assert!(items.iter().all(|item| item.selected));
    let apply = |items: &mut Vec<TransferPlanItem>, id: &str, value| {
        let states = sql_object_selection(items, vec![(id.into(), value)], true).unwrap();
        for (item, (_, selected)) in items.iter_mut().zip(states) {
            item.selected = selected;
        }
    };
    apply(&mut items, "a", false);
    assert!(items[..3].iter().all(|item| !item.selected));
    assert!(items[3..].iter().all(|item| item.selected));
    apply(&mut items, "a", true);
    assert!(items.iter().all(|item| item.selected));
    apply(&mut items, "a", false);
    apply(&mut items, "b", false);
    assert!(items.iter().all(|item| !item.selected));
    assert!(sql_object_selection(&items, vec![("db".into(), true)], true).is_err());
}
