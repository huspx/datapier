use super::*;

#[test]
fn preview_handle_survives_until_png_is_complete_then_is_retired() {
    let mut ui = ValueUi {
        revision: 7,
        mode: Mode::Image,
        ..ValueUi::default()
    };
    let request = ui
        .image_response(
            &json!({"preview_id":"derived","revision":7,"mime":"image/png","total_bytes":5}),
        )
        .unwrap();
    assert_eq!(request.1["value_session"], "derived");
    let next = ui.read_response(&json!({"value_session":"derived","revision":7,"offset":0,"data_b64":STANDARD.encode(b"abc")})).unwrap();
    assert!(next.is_some());
    assert!(ui.take_preview_png().is_none());
    assert!(ui.retired_previews.is_empty());
    ui.read_response(&json!({"value_session":"derived","revision":7,"offset":3,"data_b64":STANDARD.encode(b"de")})).unwrap();
    assert_eq!(ui.take_preview_png().unwrap(), b"abcde");
    assert_eq!(ui.retired_previews, vec![("derived".into(), 7)]);
    ui.image_response(
        &json!({"preview_id":"second","revision":7,"mime":"image/png","total_bytes":1}),
    )
    .unwrap();
    ui.invalidate_preview();
    assert_eq!(
        ui.retired_previews,
        vec![("derived".into(), 7), ("second".into(), 7)]
    );
}

#[test]
fn small_pages_reassemble_text_without_losing_bytes() {
    let mut ui = ValueUi {
        max_text_bytes: 8192,
        ..ValueUi::default()
    };
    let data = "中🙂abc".repeat(400).into_bytes();
    let first = 517;
    let mut request = ui.summary_response(&json!({"value_session":"session","revision":1,"total_bytes":data.len(),"writable":true,"kind":"text","is_null":false,"offset":0,"data_b64":STANDARD.encode(&data[..first])})).unwrap();
    while let Some((method, params)) = request {
        assert_eq!(method, "value.read");
        let offset = params["offset"].as_u64().unwrap() as usize;
        let end = (offset + 517).min(data.len());
        request = ui.read_response(&json!({"value_session":"session","revision":1,"offset":offset,"data_b64":STANDARD.encode(&data[offset..end])})).unwrap();
    }
    assert_eq!(ui.original_window, data);
}

fn value_app(backend: &crate::Backend) -> crate::App {
    use datapier_core::model::{Column, ResultMeta, ResultPage, Value as DataValue};
    let mut app = crate::App::new("advanced-value-test".into());
    let session = uuid::Uuid::new_v4();
    let result = uuid::Uuid::new_v4();
    app.session = Some(json!({"session_id": session}));
    app.edit_token = Some(json!(uuid::Uuid::new_v4()));
    let mut grid = crate::GridState::new(
        ResultPage {
            result: ResultMeta {
                id: result,
                execution_id: uuid::Uuid::new_v4(),
                columns: vec![Column::new("name", "text")],
                kind: "rows".into(),
                row_count: 1,
                complete: true,
                summary: Value::Null,
            },
            rows: vec![vec![DataValue::Text("before".into())]],
            start: 0,
            next: None,
        },
        true,
    );
    grid.focus = Some(crate::Cell { row: 0, column: 0 });
    app.grid = Some(grid);
    app.advanced_value_open(
        backend,
        json!({"session_id":session,"target":null,"result_id":result,"row_no":0,"column":"name"}),
    );
    let (nonce, _) = app.advanced_value.request.clone().unwrap();
    let id = uuid::Uuid::new_v4().to_string();
    app.advanced_value_response(
        &format!("advanced-value/{nonce}"),
        &Ok(json!({
            "value_session":id,"revision":1,"total_bytes":6,"writable":true,
            "kind":"text","is_null":false,"offset":0,"data_b64":STANDARD.encode(b"before")
        })),
        backend,
    );
    app
}

fn apply_value(app: &mut crate::App, backend: &crate::Backend) {
    app.advanced_value.editor = Editor::new("after");
    assert!(app.advanced_value.dirty());
    app.advanced_value_action("advanced-value-patch", backend);
    let (nonce, _) = app.advanced_value.request.clone().unwrap();
    let id = app.advanced_value.session.clone().unwrap();
    app.advanced_value_response(
        &format!("advanced-value/{nonce}"),
        &Ok(json!({
            "value_session":id,"revision":2,"total_bytes":5,"writable":true,
            "kind":"text","is_null":false,"offset":0,"data_b64":STANDARD.encode(b"after")
        })),
        backend,
    );
    assert!(!app.advanced_value.dirty());
    assert!(app.advanced_value.applied);
}

#[test]
fn applied_value_requires_discard_until_successful_stage() {
    let (backend, _requests, _responses) = crate::Backend::scripted();
    let mut app = value_app(&backend);
    apply_value(&mut app, &backend);
    app.action("overlay-close", &backend);
    assert!(app.advanced_value.confirm_close);
    app.advanced_value_action("advanced-value-continue", &backend);
    app.advanced_value_action("advanced-value-stage", &backend);
    let (nonce, _) = app.advanced_value.request.clone().unwrap();
    let source = app.advanced_value.source.clone().unwrap();
    let id = app.advanced_value.session.clone().unwrap();
    app.advanced_value_response(
        &format!("advanced-value/{nonce}"),
        &Ok(json!({
            "value_session":id,"revision":2,"result_id":source["result_id"],
            "row_no":0,"column":"name","value":datapier_core::model::Value::Text("after".into())
        })),
        &backend,
    );
    assert!(app.advanced_value.staged);
    assert!(!app.advanced_value.can_stage());
    app.advanced_value_action("advanced-value-close", &backend);
    assert!(app.workbench.overlay.is_none());
}

#[test]
fn failed_external_cleanup_keeps_frozen_confirmation_and_error() {
    let (backend, _requests, _responses) = crate::Backend::scripted();
    let mut app = value_app(&backend);
    app.advanced_files.file_session = Some("file-session".into());
    app.advanced_files.file_revision = 3;
    app.advanced_files.external_path = Some("/tmp/value-edit".into());
    app.advanced_value_action("advanced-external-clean", &backend);
    let target = app.advanced_files.confirm_clean.clone();
    app.advanced_value_action("advanced-external-clean-confirm", &backend);
    let (nonce, _) = app.advanced_value.request.clone().unwrap();
    app.advanced_value_response(
        &format!("advanced-value/{nonce}"),
        &Err(crate::AppError::invalid("cleanup failed")),
        &backend,
    );
    assert_eq!(app.advanced_files.confirm_clean, target);
    assert!(
        app.advanced_files
            .error
            .as_deref()
            .unwrap()
            .contains("cleanup failed")
    );
    assert!(app.advanced_value.error.is_none());
    app.advanced_value_action("advanced-external-clean-cancel", &backend);
    assert!(app.advanced_files.confirm_clean.is_none());
    assert_eq!(
        app.advanced_files.file_session.as_deref(),
        Some("file-session")
    );
}

#[test]
fn selecting_mode_establishes_keyboard_focus() {
    let (backend, _requests, _responses) = crate::Backend::scripted();
    let mut app = value_app(&backend);
    app.focus = "advanced-value".into();
    app.advanced_value_action("advanced-value-mode:hex", &backend);
    assert_eq!(app.focus, "advanced-value-tab-hex");
    assert_eq!(app.workbench.focus_request.as_deref(), Some(""));
}
