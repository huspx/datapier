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

#[test]
fn provenance_distinguishes_cached_rows_from_unknown_original_data() {
    let result = ResultMeta {
        id: Uuid::new_v4(),
        execution_id: Uuid::new_v4(),
        columns: vec![],
        kind: "rows".into(),
        row_count: 99,
        complete: true,
        summary: json!({
            "origin": {"source": "archery", "instance_id": 7, "database": "frozen_db"},
            "coverage": {"requested_limit": 500, "returned_rows": 42, "original_query": "limited"},
            "fidelity": {"kind": "platform_values", "lossy": ["binary", "encoding"]},
            "masking": "unknown"
        }),
    };
    let node = result_provenance(&result).unwrap();
    let fact = |label: &str| {
        let label = crate::i18n::tr(label);
        node.child_nodes()
            .iter()
            .find_map(|row| {
                let children = row.child_nodes();
                (children.len() == 2
                    && children[0].props().get("text").and_then(Value::as_str)
                        == Some(label.as_ref()))
                .then(|| children[1].props()["text"].as_str().unwrap().to_owned())
            })
            .unwrap()
    };
    assert_eq!(fact("实例"), "7");
    assert_eq!(fact("数据库"), "frozen_db");
    assert_eq!(fact("请求上限"), "500");
    assert_eq!(fact("已返回行数"), "42");
    for label in ["原查询总行数", "原始类型", "原始字节", "脱敏状态"] {
        assert_eq!(fact(label), crate::i18n::tr("未提供"));
    }
    assert_eq!(fact("覆盖范围"), crate::i18n::tr("原查询结果受限"));
    assert!(fact("类型损失").contains(crate::i18n::tr("二进制").as_ref()));
}
