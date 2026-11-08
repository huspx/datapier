use super::*;

#[test]
fn transfer_close_keeps_input_until_explicit_discard() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("modal-close".into());
    app.action("overlay:export", &backend);
    app.field_set("path", "/tmp/report.csv");
    app.action("overlay-close", &backend);
    assert!(app.workbench.overlay_close_confirm);
    assert_eq!(app.workbench.overlay.as_deref(), Some("export"));
    assert_eq!(app.field("path"), "/tmp/report.csv");
    app.action("overlay-close-continue", &backend);
    assert!(!app.workbench.overlay_close_confirm);
    assert_eq!(app.field("path"), "/tmp/report.csv");
    app.action("overlay-close", &backend);
    app.action("overlay-close-discard", &backend);
    assert!(app.workbench.overlay.is_none());
    assert_eq!(app.field("path"), "");
}

#[test]
fn closing_unchanged_form_needs_no_confirmation() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("modal-clean".into());
    app.action("overlay:file", &backend);
    app.field_set("path", "changed");
    app.field_set("path", "");
    app.action("overlay-close", &backend);
    assert!(app.workbench.overlay.is_none());
    assert!(!app.workbench.overlay_close_confirm);
}

#[test]
fn file_request_prevents_close_and_later_allows_discard() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("modal-busy".into());
    app.action("overlay:file", &backend);
    app.field_set("path", "/tmp/query.sql");
    app.file_open_pending = Some((Uuid::new_v4(), "/tmp/query.sql".into(), 0));
    app.action("overlay-close", &backend);
    assert_eq!(app.workbench.overlay.as_deref(), Some("file"));
    assert!(!app.workbench.overlay_close_confirm);
    app.file_open_pending = None;
    app.action("overlay-close", &backend);
    assert!(app.workbench.overlay_close_confirm);
    app.action("overlay-close-discard", &backend);
    assert!(app.workbench.overlay.is_none());
}

#[test]
fn redis_ttl_policy_change_is_protected_without_text_edit() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("modal-ttl".into());
    app.workbench.overlay = Some("redis".into());
    app.track_modal_draft();
    app.workbench.ttl_policy = RedisTtlPolicy::Persist;
    app.action("overlay-close", &backend);
    assert!(app.workbench.overlay_close_confirm);
    app.action("overlay-close-discard", &backend);
    assert!(app.workbench.ttl_policy == RedisTtlPolicy::Keep);
    assert!(app.workbench.overlay.is_none());
}

fn cell_app(value: DataValue) -> App {
    use datapier_core::model::{Column, ResultMeta, ResultPage};
    let mut app = App::new("modal-cell".into());
    let mut grid = GridState::new(
        ResultPage {
            result: ResultMeta {
                id: Uuid::new_v4(),
                execution_id: Uuid::new_v4(),
                columns: vec![Column::new("name", "text")],
                kind: "rows".into(),
                row_count: 1,
                complete: true,
                summary: Value::Null,
            },
            rows: vec![vec![value]],
            start: 0,
            next: None,
        },
        true,
    );
    grid.focus = Some(Cell { row: 0, column: 0 });
    app.grid = Some(grid);
    app
}

#[test]
fn advanced_editor_transition_keeps_unapplied_cell_input_until_discard() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = cell_app(DataValue::Text("original".into()));
    app.open_cell_at(Cell { row: 0, column: 0 }, &backend);
    app.field_set("cell", "unapplied input");
    app.action("advanced-value-open", &backend);
    assert_eq!(app.workbench.overlay.as_deref(), Some("cell"));
    assert!(app.workbench.overlay_close_confirm);
    assert!(requests.try_recv().is_err());
    app.action("overlay-close-continue", &backend);
    assert_eq!(app.field("cell"), "unapplied input");
    assert_eq!(app.workbench.overlay.as_deref(), Some("cell"));
}

#[test]
fn replacing_cell_drawer_requires_explicit_discard_and_resumes_requested_action() {
    for (action, overlay) in [
        ("history", "history"),
        ("app-settings", "settings"),
        ("overlay:diagnostics", "diagnostics"),
    ] {
        let (backend, _, _) = Backend::scripted();
        let mut app = cell_app(DataValue::Text("original".into()));
        app.open_cell_at(Cell { row: 0, column: 0 }, &backend);
        app.field_set("cell", "unapplied input");
        app.action(action, &backend);
        assert_eq!(app.workbench.overlay.as_deref(), Some("cell"));
        assert!(app.workbench.overlay_close_confirm);
        app.action("overlay-close-continue", &backend);
        assert_eq!(app.field("cell"), "unapplied input");
        assert_eq!(app.workbench.overlay.as_deref(), Some("cell"));
        app.action(action, &backend);
        app.action("overlay-close-discard", &backend);
        assert_eq!(app.workbench.overlay.as_deref(), Some(overlay));
        assert_eq!(app.field("cell"), "original");
    }
}

#[test]
fn late_large_value_caches_data_without_replacing_a_new_form() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let full = DataValue::Text("full value".into());
    let bytes = serde_json::to_vec(&full).unwrap();
    let id = Uuid::new_v4();
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = cell_app(DataValue::Large {
        id,
        original_type: "text".into(),
        bytes: bytes.len() as u64,
        preview: "full".into(),
    });
    app.open_cell_at(Cell { row: 0, column: 0 }, &backend);
    let _request = requests.try_recv().unwrap();
    app.action("overlay:row", &backend);
    app.field_set("row_values", "unapplied new row");
    app.large_response(
        &format!("large.value/{id}/0"),
        json!({
            "data_b64":STANDARD.encode(&bytes), "offset":0, "next":null, "total_bytes":bytes.len()
        }),
        &backend,
    );
    assert_eq!(app.workbench.overlay.as_deref(), Some("row"));
    assert_eq!(app.field("row_values"), "unapplied new row");
    assert_eq!(
        app.grid.as_ref().unwrap().value(Cell { row: 0, column: 0 }),
        Some(&full)
    );
}

#[tokio::test]
async fn switching_cell_drawer_to_large_field_opens_the_completed_value() {
    use datapier_core::{Store, model::Column};
    let root = std::env::temp_dir().join(format!("datapier-cell-chunks-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let task = store
        .create_task(Uuid::new_v4(), "test", "execution.run", json!({}), false)
        .await
        .unwrap();
    let result = store
        .start_result(
            task.id,
            vec![Column::new("small", "text"), Column::new("large", "text")],
            "rows",
        )
        .await
        .unwrap();
    let text = "完整 value ".repeat(2048);
    store
        .append_rows(
            result,
            vec![vec![
                DataValue::Text("original".into()),
                DataValue::Text(text.clone()),
            ]],
        )
        .await
        .unwrap();
    store.finish_result(result, json!({})).await.unwrap();
    let page = store.result_page(result, 0, 1, 262144).await.unwrap();
    assert!(matches!(page.rows[0][1], DataValue::Large { .. }));
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("cell-wire-contract".into());
    app.grid = Some(GridState::new(page, true));
    app.open_cell_at(Cell { row: 0, column: 0 }, &backend);
    app.action("cell-field:1", &backend);
    while let Ok(request) = requests.try_recv() {
        let id = serde_json::from_value(request.params["value_id"].clone()).unwrap();
        let offset = request.params["offset"].as_u64().unwrap();
        let chunk = store.large_value(id, offset, 257).await.unwrap();
        app.large_response(&format!("large.value/{id}/{offset}"), chunk, &backend);
    }
    assert_eq!(app.workbench.overlay.as_deref(), Some("cell"));
    assert_eq!(app.detail_cell(), Some(Cell { row: 0, column: 1 }));
    assert_eq!(app.field("cell"), text);
    store.pool.close().await;
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}
