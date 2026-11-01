use super::*;

fn query_app(sql: &str) -> App {
    let mut app = App::new("query-plan-test".into());
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.workbench.sql_mode = Some(editor::MysqlMode::default());
    app.tabs[0].query = editor::Editor::new(sql);
    app
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
