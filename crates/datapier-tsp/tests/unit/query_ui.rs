use super::*;

fn query_app(sql: &str) -> App {
    let mut app = App::new("query-plan-test".into());
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.workbench.sql_mode = Some(editor::MysqlMode::default());
    app.tabs[0].query = editor::Editor::new(sql);
    app
}

#[test]
fn query_info_updates_only_its_own_tab_and_rejects_stale_generation() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = query_app("USE two");
    app.database = Some("one".into());
    app.tabs[0].browse.database = app.database.clone();
    app.refresh_query_mode(&backend);
    let info = requests.try_recv().unwrap();
    app.create_query_tab();
    app.database = Some("other".into());
    app.tabs[1].browse.database = app.database.clone();
    app.routed_response(
        Response {
            tag: info.tag.clone(),
            result: Ok(json!({"database":"two", "sql_mode":""})),
        },
        &backend,
    );
    assert_eq!(app.database.as_deref(), Some("other"));
    app.switch_tab(0);
    assert_eq!(app.database.as_deref(), Some("two"));
    assert_eq!(app.tabs[0].browse.database.as_deref(), Some("two"));
    let generation = app.generation;
    app.routed_response(
        Response {
            tag: info.tag,
            result: Ok(json!({"database":"stale", "sql_mode":""})),
        },
        &backend,
    );
    assert_eq!(app.database.as_deref(), Some("two"));
    assert_eq!(app.generation, generation);
}

#[test]
fn archery_request_limit_uses_setting_until_explicitly_selected() {
    let (backend, _, _) = Backend::scripted();
    let mut app = query_app("SELECT 1");
    app.settings.archery.request_rows = 500;
    app.profile = Some(
        serde_json::from_value(json!({
            "id": Uuid::new_v4(), "name":"Archery", "provider":"mysql", "access_mode":"read_only",
            "connection":{"kind":"archery", "server_id":Uuid::new_v4(), "instance_id":1,
                "instance_name":"mysql", "databases":["one"], "default_database":"one"}
        }))
        .unwrap(),
    );
    assert_eq!(app.archery_request_limit(), 500);
    app.workbench_action("archery-request-limit:100", &backend);
    app.settings.archery.request_rows = 1000;
    assert_eq!(app.archery_request_limit(), 100);
    app.create_query_tab();
    assert_eq!(app.archery_request_limit(), 1000);
    app.switch_tab(0);
    assert_eq!(app.archery_request_limit(), 100);
}

#[test]
fn plan_executes_selection_before_current_statement() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = query_app("SELECT '中文😀;'; SELECT 2");
    let end = "SELECT '中文😀;';".encode_utf16().count();
    assert!(app.tabs[0].query.set_selection(0, end));
    assert!(app.query_action("query-plan", &backend));
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "execution.start");
    assert_eq!(
        request.params["text"],
        "EXPLAIN FORMAT=JSON SELECT '中文😀;';"
    );
}

#[test]
fn plan_executes_cursor_statement_with_quoted_and_commented_semicolons() {
    let (backend, mut requests, _) = Backend::scripted();
    let sql = "SELECT 1; /* 中文; 😀 */ SELECT 'a;b'";
    let mut app = query_app(sql);
    let cursor = sql.encode_utf16().count();
    assert!(app.tabs[0].query.set_selection(cursor, cursor));
    assert!(app.query_action("query-plan", &backend));
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "execution.start");
    assert_eq!(
        request.params["text"],
        "EXPLAIN FORMAT=JSON /* 中文; 😀 */ SELECT 'a;b'"
    );
}

#[test]
fn plan_rejects_unsafe_targets_without_execution() {
    for sql in [
        "",
        "-- comment only",
        "SELECT 'unfinished",
        "SELECT 1; SELECT 2",
        "CREATE TABLE t (id int)",
        "/*! SELECT 1 */",
    ] {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = query_app(sql);
        let end = sql.encode_utf16().count();
        assert!(app.tabs[0].query.set_selection(0, end));
        assert!(app.query_action("query-plan", &backend));
        assert!(
            requests.try_recv().is_err(),
            "unexpected execution for {sql:?}"
        );
    }
}

#[test]
fn plan_refuses_invalid_utf16_mode_session_and_running_task() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = query_app("SELECT '😀'");
    app.tabs[0].query.anchor = 0;
    app.tabs[0].query.cursor = "SELECT '".encode_utf16().count() + 1;
    assert!(app.query_action("query-plan", &backend));
    assert!(requests.try_recv().is_err());
    app.tabs[0].query = editor::Editor::new("SELECT 1");
    app.workbench.sql_mode = None;
    assert!(app.query_action("query-plan", &backend));
    assert!(requests.try_recv().is_err());
    app.workbench.sql_mode = Some(editor::MysqlMode::default());
    app.session = None;
    assert!(app.query_action("query-plan", &backend));
    assert!(requests.try_recv().is_err());
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.operation_pending = true;
    assert!(app.query_action("query-plan", &backend));
    assert!(requests.try_recv().is_err());
}

#[test]
fn context_format_undo_restores_reverse_selection() {
    let mut query = editor::Editor::new("select '😀' from t");
    let end = query.text.encode_utf16().count();
    assert!(restore_menu_selection(
        &mut query,
        end,
        0,
        "select '😀' from t"
    ));
    let formatted =
        editor::format_range(&query.text, query.selection(), editor::MysqlMode::default()).unwrap();
    assert!(query.edit(query.selection(), &formatted));
    assert!(query.undo());
    assert_eq!((query.anchor, query.cursor), (end, 0));
    assert_eq!(query.text, "select '😀' from t");
}

#[test]
fn archery_plan_uses_stateless_context_and_selected_database() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = query_app("SELECT '😀'; SELECT 2");
    app.profile = Some(
        serde_json::from_value(json!({
            "id": Uuid::new_v4(), "name": "Archery", "provider": "mysql",
            "access_mode": "read_only", "connection": {
                "kind": "archery", "server_id": Uuid::new_v4(), "instance_id": 1,
                "instance_name": "mysql", "databases": ["one", "two"], "default_database": "one"
            }
        }))
        .unwrap(),
    );
    app.database = Some("two".into());
    app.workbench.sql_mode = None;
    assert!(app.query_action("query-plan", &backend));
    let request = requests.try_recv().unwrap();
    assert_eq!(request.params["text"], "EXPLAIN FORMAT=JSON SELECT 2");
    assert_eq!(request.params["database"], "two");
}

#[test]
fn late_page_cannot_replace_current_page_or_another_tab() {
    use datapier_core::model::{Column, ResultMeta, Value as DataValue};

    let (backend, mut requests, _) = Backend::scripted();
    let mut app = query_app("SELECT 1");
    let id = Uuid::new_v4();
    app.result_id = Some(id.to_string());
    let result = ResultMeta {
        id,
        execution_id: Uuid::new_v4(),
        columns: vec![Column::new("value", "text")],
        kind: "rows".into(),
        row_count: 2,
        complete: true,
        summary: json!({"retrieval_mode":"bounded_response"}),
    };
    let page = |start, text: &str| ResultPage {
        result: result.clone(),
        rows: vec![vec![DataValue::Text(text.into())]],
        start,
        next: None,
    };

    app.page(&backend, 0);
    let old = requests.try_recv().unwrap();
    app.page(&backend, 1);
    let current = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: current.tag,
            result: Ok(serde_json::to_value(page(1, "current")).unwrap()),
        },
        &backend,
    );
    app.routed_response(
        Response {
            tag: old.tag,
            result: Ok(serde_json::to_value(page(0, "late")).unwrap()),
        },
        &backend,
    );
    assert_eq!(app.grid.as_ref().unwrap().page.start, 1);
    assert_eq!(
        app.grid.as_ref().unwrap().page.rows[0][0],
        DataValue::Text("current".into())
    );

    app.page(&backend, 0);
    let pending = requests.try_recv().unwrap();
    app.create_query_tab();
    app.routed_response(
        Response {
            tag: pending.tag,
            result: Ok(serde_json::to_value(page(0, "first tab")).unwrap()),
        },
        &backend,
    );
    assert_eq!(app.active, 1);
    assert!(app.grid.is_none());
    app.switch_tab(0);
    assert_eq!(
        app.grid.as_ref().unwrap().page.rows[0][0],
        DataValue::Text("first tab".into())
    );
}

#[test]
fn result_info_closing_restores_focus_only_when_requested() {
    let mut app = query_app("SELECT 1");
    app.workbench.query_ui.result_info = Some(Uuid::new_v4().to_string());
    app.close_result_info(false);
    assert!(app.workbench.query_ui.result_info.is_none());
    assert!(app.workbench.focus_request.is_none());
    app.workbench.query_ui.result_info = Some(Uuid::new_v4().to_string());
    app.close_result_info(true);
    assert_eq!(app.workbench.focus_request.as_deref(), Some("result-info"));
}

#[test]
fn request_limit_changes_only_while_idle() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = query_app("SELECT 1");
    app.profile = Some(serde_json::from_value(json!({
        "id": Uuid::new_v4(), "name": "Archery", "provider": "mysql", "access_mode": "read_only",
        "connection": { "kind": "archery", "server_id": Uuid::new_v4(), "instance_id": 1,
            "instance_name": "mysql", "databases": ["one"], "default_database": "one" }
    })).unwrap());
    app.workbench_action("archery-request-limit:500", &backend);
    assert_eq!(app.workbench.query_ui.request_limit, 500);
    app.task = Some(Uuid::new_v4().to_string());
    app.workbench_action("archery-request-limit:1000", &backend);
    assert_eq!(app.workbench.query_ui.request_limit, 500);
    app.task = None;
    app.workbench_action("archery-request-limit:123", &backend);
    assert_eq!(app.workbench.query_ui.request_limit, 500);
    app.database = Some("one".into());
    assert!(app.query_action("query-plan", &backend));
    let request = requests.try_recv().unwrap();
    assert_eq!(request.params["limit"], 500);
}
