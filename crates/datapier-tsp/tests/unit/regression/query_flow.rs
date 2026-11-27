use super::*;

#[test]
fn sql_text_changes_clear_query_results_without_discarding_table_data() {
    let mut app = App::new("test".into());
    app.grid = Some(editable_grid());
    let id = app.grid.as_ref().unwrap().page.result.id.to_string();
    app.result_id = Some(id.clone());
    app.results.push(id);
    app.tabs[0].query = Editor::new("SELECT 1");
    app.tabs[0].query.set_selection(0, 0);
    app.focus = "query".into();
    app.input_key(&tern_sdk::Key::named("right"));
    assert_eq!(app.tabs[0].query.cursor, 1);
    assert!(app.grid.is_some());
    let mut typed = tern_sdk::Key::named("x");
    typed.text = Some("x".into());
    app.input_key(&typed);
    assert!(app.grid.is_none());
    assert!(app.result_id.is_none() && app.results.is_empty());

    app.grid = Some(editable_grid());
    app.browse_result = true;
    app.changed_field("query");
    assert_eq!(
        app.grid.as_ref().unwrap().value(Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("before".into()))
    );
}

#[test]
fn native_sql_edit_checks_utf16_length_and_preserves_results_for_cursor_moves() {
    let mut app = App::new("native-query".into());
    app.focus = "query".into();
    app.tabs[0].query = Editor::new("中😀文");
    app.grid = Some(editable_grid());
    app.dispatch_editor_message(Msg::Edit("query".into(), 0, 0, String::new(), 3, 4));
    assert_eq!(app.tabs[0].query.cursor, 3);
    assert!(app.grid.is_some());
    app.dispatch_editor_message(Msg::Edit("query".into(), 1, 3, "x".into(), 2, 3));
    assert_eq!(app.tabs[0].query.text, "中😀文");
    app.dispatch_editor_message(Msg::Edit("query".into(), 1, 3, "好".into(), 2, 4));
    assert_eq!(app.tabs[0].query.text, "中好文");
    assert!(app.grid.is_none());
}

#[test]
fn sql_node_identity_rejects_same_length_stale_events_and_preserves_focus() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("native-revision".into());
    app.focus = "query".into();
    app.tabs[0].query = Editor::new("中😀");
    let old = app.sql_editor_name(0);
    let old_key = app.sql_editor_key(0, false);
    app.dispatch_guarded_editor_message(Msg::Edit(old.clone(), 0, 1, "文".into(), 1, 3), &backend);
    assert_eq!(app.tabs[0].query.text, "文😀");
    assert_ne!(old_key, app.sql_editor_key(0, false));
    assert_eq!(app.take_focus_request(), Some(app.sql_editor_key(0, false)));
    app.dispatch_guarded_editor_message(Msg::Edit(old.clone(), 0, 1, "错".into(), 1, 3), &backend);
    assert_eq!(app.tabs[0].query.text, "文😀");
    assert!(app.resolve_sql_editor(old, true).is_none());
    let current = app.sql_editor_name(0);
    app.dispatch_guarded_editor_message(Msg::Edit(current, 0, 0, String::new(), 3, 3), &backend);
    assert_eq!(app.tabs[0].query.cursor, 3);
    let before = app.sql_editor_key(0, false);
    let mut typed = tern_sdk::Key::named("x");
    typed.text = Some("x".into());
    app.input_key(&typed);
    assert_ne!(before, app.sql_editor_key(0, false));
    assert_eq!(app.take_focus_request(), Some(app.sql_editor_key(0, false)));
}

#[test]
fn current_statement_uses_text_and_cursor_at_request_time() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("current".into());
    app.profile = Some(ui_profile("mysql", "mysql"));
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.tabs[0].query = Editor::new("SELECT 1; SELECT 2;");
    app.tabs[0].query.set_selection(2, 2);
    app.action("current", &backend);
    let request = requests.try_recv().unwrap();
    app.tabs[0].query = Editor::new("SELECT 3; SELECT 4;");
    app.changed_field("query");
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"sql_mode":""})),
        },
        &backend,
    );
    let execution = requests.try_recv().unwrap();
    assert_eq!(execution.method, "execution.start");
    assert_eq!(execution.params["text"], "SELECT 1");
}

#[test]
fn file_open_ignores_stale_responses_and_preserves_intervening_edits() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("file".into());
    app.tabs[0].query = Editor::new("original");
    app.field_set("path", "query.sql");
    app.action("file-open", &backend);
    let first = requests.try_recv().unwrap();
    app.action("file-open", &backend);
    let second = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: first.tag,
            result: Ok(json!({"text":"stale"})),
        },
        &backend,
    );
    assert_eq!(app.tabs[0].query.text, "original");
    app.tabs[0].query = Editor::new("edited");
    app.changed_field("query");
    app.routed_response(
        Response {
            tag: second.tag,
            result: Ok(json!({"text":"file"})),
        },
        &backend,
    );
    assert_eq!(app.tabs[0].query.text, "edited");
}

#[test]
fn opening_query_file_invalidates_query_results_but_preserves_browse_grid() {
    for browsing in [false, true] {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = App::new("file-results".into());
        app.grid = Some(editable_grid());
        app.browse_result = browsing;
        app.result_id = Some("old".into());
        app.results.push("old".into());
        app.page_token = Some(Uuid::new_v4());
        app.field_set("path", "query.sql");
        app.action("file-open", &backend);
        let request = requests.try_recv().unwrap();
        app.routed_response(
            Response {
                tag: request.tag,
                result: Ok(json!({"text":"SELECT 2"})),
            },
            &backend,
        );
        assert_eq!(app.tabs[0].query.text, "SELECT 2");
        if browsing {
            assert!(app.grid.is_some());
            assert!(app.page_token.is_some());
        } else {
            assert!(app.grid.is_none());
            assert!(app.results.is_empty() && app.result_id.is_none() && app.page_token.is_none());
        }
    }
}
