use super::*;
use datapier_core::model::{Column, ResultMeta, ResultPage, Value as DataValue};
use serde_json::json;
use uuid::Uuid;

#[test]
fn filtered_row_selection_freezes_only_visible_copy_and_delete_targets() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.profile_form(None);
    app.field_set("provider", "redis");
    app.field_set("access", "read_write");
    app.profile = Some(app.form_profile().unwrap());
    app.workbench.overlay = None;
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.target = Some(ResourceTarget {
        database: None,
        name: "list".into(),
        key_b64: None,
        kind: "list".into(),
    });
    app.edit_token = Some(json!(Uuid::new_v4()));
    app.browse_result = true;
    app.schema = Some(json!({"type": "list"}));
    let rows: Vec<_> = ["match zero", "hidden", "match two"]
        .into_iter()
        .enumerate()
        .map(|(i, text)| {
            vec![
                DataValue::Integer(i.to_string()),
                DataValue::Text(text.into()),
            ]
        })
        .collect();
    let page = ResultPage {
        result: ResultMeta {
            id: Uuid::new_v4(),
            execution_id: Uuid::new_v4(),
            columns: vec![
                Column::new("index", "integer"),
                Column::new("value", "text"),
            ],
            kind: "redis".into(),
            row_count: 3,
            complete: true,
            summary: serde_json::Value::Null,
        },
        rows: rows.clone(),
        start: 0,
        next: None,
    };
    app.grid = Some(GridState::new(page, false));
    app.field_set("row_search", "match");
    let grid = app.grid.as_mut().unwrap();
    grid.select_row(0, 0, false, false).unwrap();
    grid.select_row(2, 0, true, false).unwrap();
    assert_eq!(grid.selected_rows(), vec![0, 2]);

    let expected = json!([rows[0], rows[2]]);
    let mut clipboard = crate::clipboard::Clipboard::default();
    clipboard.copy_redis_selected_rows(&mut app, &backend);
    let copy = requests.try_recv().unwrap();
    assert_eq!(copy.params["rows"], expected);

    app.request_redis_row_delete();
    assert_eq!(app.redis_row_delete_count(), 2);
    app.confirm_redis_row_delete(&backend);
    let delete = requests.try_recv().unwrap();
    assert_eq!(delete.params["rows"], expected);

    app.cancel_redis_row_delete();
    app.request_redis_row_delete();
    assert_eq!(app.redis_row_delete_count(), 2);
    app.field_set("row_search", "zero");
    assert_eq!(app.grid.as_ref().unwrap().selected_rows(), vec![0]);
    app.confirm_redis_row_delete(&backend);
    assert!(requests.try_recv().is_err());
    assert!(app.redis_changes.is_empty());
}
