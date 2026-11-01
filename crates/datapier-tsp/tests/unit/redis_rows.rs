use super::*;
use datapier_core::model::{Column, ResultMeta, ResultPage};

fn list_app() -> App {
    let mut app = App::new("test".into());
    app.profile_form(None);
    app.field_set("provider", "redis");
    app.field_set("access", "read_write");
    app.profile = Some(app.form_profile().unwrap());
    app.workbench.overlay = None;
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.target = Some(ResourceTarget {
        database: None,
        name: "list".into(),
        key_b64: Some(STANDARD.encode("list")),
        kind: "list".into(),
    });
    app.edit_token = Some(json!(Uuid::new_v4()));
    app.browse_result = true;
    app.schema = Some(json!({"type":"list"}));
    let rows = (0..4)
        .map(|i| {
            vec![
                DataValue::Integer(i.to_string()),
                DataValue::Bytes(STANDARD.encode("same")),
            ]
        })
        .collect::<Vec<_>>();
    let page = ResultPage {
        result: ResultMeta {
            id: Uuid::new_v4(),
            execution_id: Uuid::new_v4(),
            columns: vec![
                Column::new("index", "integer"),
                Column::new("value", "bytes"),
            ],
            kind: "redis".into(),
            row_count: 4,
            complete: true,
            summary: Value::Null,
        },
        rows,
        start: 0,
        next: None,
    };
    let mut grid = GridState::new(page, false);
    grid.select_row(1, 0, false, false).unwrap();
    grid.select_row(3, 0, false, true).unwrap();
    app.grid = Some(grid);
    app
}

#[test]
fn pending_delete_requires_frozen_rows_to_remain_visible() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = list_app();
    app.request_redis_row_delete();
    app.grid.as_mut().unwrap().set_visible_rows([0, 1, 2]);
    app.confirm_redis_row_delete(&backend);
    assert!(requests.try_recv().is_err());
    assert!(app.redis_changes.is_empty());

    app.grid.as_mut().unwrap().set_visible_rows([0, 1, 2, 3]);
    app.grid
        .as_mut()
        .unwrap()
        .select_row(3, 0, false, true)
        .unwrap();
    app.request_redis_row_delete();
    app.confirm_redis_row_delete(&backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.grid.as_mut().unwrap().set_visible_rows([0, 1, 2]);
    app.redis_row_delete_response(tag, &Ok(request.params["rows"].clone()));
    assert!(app.redis_changes.is_empty());

    app.grid.as_mut().unwrap().set_visible_rows([0, 1, 2, 3]);
    app.grid
        .as_mut()
        .unwrap()
        .select_row(1, 0, false, false)
        .unwrap();
    app.grid
        .as_mut()
        .unwrap()
        .select_row(3, 0, false, true)
        .unwrap();
    app.request_redis_row_delete();
    app.grid
        .as_mut()
        .unwrap()
        .select_row(0, 0, false, false)
        .unwrap();
    app.confirm_redis_row_delete(&backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.redis_row_delete_response(tag, &Ok(request.params["rows"].clone()));
    assert_eq!(app.redis_changes.len(), 2);
    assert_eq!(app.redis_changes[0]["index"], 3);
    assert_eq!(app.redis_changes[1]["index"], 1);
}

#[test]
fn submit_waits_for_large_row_resolution_and_keeps_both_drafts() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = list_app();
    app.schema = Some(json!({"type":"hash"}));
    app.target.as_mut().unwrap().kind = "hash".into();
    for index in [1, 3] {
        app.grid.as_mut().unwrap().page.rows[index][0] = DataValue::Large {
            id: Uuid::new_v4(),
            original_type: "bytes".into(),
            bytes: 10_000_000,
            preview: "preview".into(),
        };
    }
    let existing = json!({"operation":"set_ttl","ttl_ms":1000});
    app.redis_changes.push(existing.clone());
    app.request_redis_row_delete();
    app.confirm_redis_row_delete(&backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    assert!(app.redis_row_delete_resolution_pending());
    assert!(app.change_input().is_ok());

    app.action("redis-submit", &backend);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.redis_changes, vec![existing.clone()]);

    let mut rows = request.params["rows"].clone();
    rows[0][0] = serde_json::to_value(DataValue::Bytes(STANDARD.encode("field1"))).unwrap();
    rows[1][0] = serde_json::to_value(DataValue::Bytes(STANDARD.encode("field3"))).unwrap();
    assert!(app.redis_row_delete_response(tag, &Ok(rows)));
    assert!(!app.redis_row_delete_resolution_pending());
    assert_eq!(app.redis_changes.len(), 3);
    assert_eq!(app.redis_changes[0], existing);
    assert_eq!(app.redis_changes[1]["operation"], "hash_delete");
    assert_eq!(
        app.redis_changes[1]["field"],
        json!({"type":"bytes","value":STANDARD.encode("field1")})
    );
    assert_eq!(app.redis_changes[2]["operation"], "hash_delete");
    assert_eq!(
        app.redis_changes[2]["field"],
        json!({"type":"bytes","value":STANDARD.encode("field3")})
    );
}

#[test]
fn row_delete_rejects_busy_operations_at_each_stage() {
    for task_active in [false, true] {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = list_app();
        app.redis_changes
            .push(json!({"operation":"list_delete","index":0}));
        if task_active {
            app.task = Some("running".into());
        } else {
            app.operation_pending = true;
        }
        assert!(!app.redis_row_delete_enabled());
        app.request_redis_row_delete();
        assert!(app.workbench.redis_row_pending.is_none());

        app.operation_pending = false;
        app.task = None;
        app.request_redis_row_delete();
        if task_active {
            app.task = Some("running".into());
        } else {
            app.operation_pending = true;
        }
        app.confirm_redis_row_delete(&backend);
        assert!(requests.try_recv().is_err());
        assert_eq!(app.redis_changes.len(), 1);

        app.operation_pending = false;
        app.task = None;
        app.request_redis_row_delete();
        app.confirm_redis_row_delete(&backend);
        let request = requests.try_recv().unwrap();
        let tag = request.tag.splitn(3, ':').nth(2).unwrap();
        assert!(app.redis_row_delete_resolution_pending());
        app.request_redis_row_delete();
        assert!(app.redis_row_delete_resolution_pending());
        if task_active {
            app.task = Some("running".into());
        } else {
            app.operation_pending = true;
        }
        assert!(app.redis_row_delete_response(tag, &Ok(request.params["rows"].clone())));
        assert!(!app.redis_row_delete_resolution_pending());
        assert_eq!(
            app.redis_changes,
            vec![json!({"operation":"list_delete","index":0})]
        );
    }
}

#[test]
fn list_batch_maps_prior_changes_and_undoes_as_one_group() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = list_app();
    app.redis_changes
        .push(json!({"operation":"list_delete","index":0}));
    app.request_redis_row_delete();
    app.confirm_redis_row_delete(&backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    let rows = request.params["rows"].clone();
    assert!(app.redis_row_delete_response(tag, &Ok(rows)));
    assert_eq!(app.redis_changes[1]["index"], 2);
    assert_eq!(app.redis_changes[2]["index"], 0);
    app.workbench_action("redis-undo", &backend);
    assert_eq!(app.redis_changes.len(), 1);
}

#[test]
fn stale_or_invalid_batch_never_stages_partial_changes() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = list_app();
    app.request_redis_row_delete();
    app.confirm_redis_row_delete(&backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.redis_changes_revision += 1;
    app.redis_row_delete_response(tag, &Ok(request.params["rows"].clone()));
    assert!(app.redis_changes.is_empty());
    app.request_redis_row_delete();
    app.confirm_redis_row_delete(&backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    let mut rows = request.params["rows"].clone();
    rows[1][0] = json!({"type":"text","value":"invalid"});
    app.redis_row_delete_response(tag, &Ok(rows));
    assert!(app.redis_changes.is_empty());
    app.profile.as_mut().unwrap().access_mode = AccessMode::ReadOnly;
    assert!(!app.redis_row_delete_enabled());
}
#[test]
fn list_delete_accepts_unresolved_large_values() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = list_app();
    let large = DataValue::Large {
        id: Uuid::new_v4(),
        original_type: "bytes".into(),
        bytes: 10_000_000,
        preview: "preview".into(),
    };
    app.grid.as_mut().unwrap().page.rows[1][1] = large.clone();
    app.grid.as_mut().unwrap().page.rows[3][1] = large;
    app.request_redis_row_delete();
    app.confirm_redis_row_delete(&backend);
    let request = requests.try_recv().unwrap();
    assert!(request.params["kind"].is_null());
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.redis_row_delete_response(tag, &Ok(request.params["rows"].clone()));
    assert_eq!(app.redis_changes.len(), 2);
    assert_eq!(app.redis_changes[0]["index"], 3);
    assert_eq!(app.redis_changes[1]["index"], 1);
    assert!(requests.try_recv().is_err());
}

#[test]
fn existing_identity_removal_is_atomic_and_readdition_restores_presence() {
    for (kind, field, remove, add) in [
        ("hash", "field", "hash_delete", "hash_set"),
        ("set", "member", "set_remove", "set_add"),
        ("zset", "member", "zset_remove", "zset_set"),
    ] {
        let mut app = list_app();
        app.request_redis_row_delete();
        let mut pending = app.workbench.redis_row_pending.take().unwrap();
        pending.schema = json!({"type":kind});
        let bytes = DataValue::Bytes(STANDARD.encode([255, 0]));
        let row = match kind {
            "hash" => vec![bytes.clone(), DataValue::Bytes(STANDARD.encode("v"))],
            "set" => vec![bytes.clone()],
            _ => vec![bytes.clone(), DataValue::Float("1".into())],
        };
        let mut removal = json!({"operation":remove});
        removal[field] = serde_json::to_value(&bytes).unwrap();
        app.redis_changes.push(removal);
        assert!(
            app.redis_row_deletions(&pending, vec![row.clone(), row.clone()])
                .is_err()
        );
        assert_eq!(app.redis_changes.len(), 1);
        let mut addition = json!({"operation":add});
        addition[field] = serde_json::to_value(&bytes).unwrap();
        app.redis_changes.push(addition);
        assert!(app.redis_row_identity_present(kind, &[255, 0]).unwrap());
        // 相同身份在同一批次中也不能重复暂存。
        assert!(
            app.redis_row_deletions(&pending, vec![row.clone(), row])
                .is_err()
        );
    }
}
