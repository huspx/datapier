use super::*;

fn app(sql: &str) -> App {
    let mut app = App::new("completion-test".into());
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.workbench.sql_mode = Some(editor::MysqlMode::default());
    app.database = Some("test".into());
    app.focus = "query".into();
    app.tabs[0].query = editor::Editor::new(sql);
    app
}

#[test]
fn acceptance_replaces_full_token_once_and_undo_restores_it() {
    let mut app = app("SELect");
    app.tabs[0].query.set_selection(3, 3);
    assert!(app.completion_update(true));
    let snapshot = app.workbench.query_ui.completion.current.as_ref().unwrap();
    let id = snapshot.id;
    let index = snapshot
        .candidates
        .iter()
        .position(|item| item.name == "SELECT")
        .unwrap();
    app.completion_accept(id, index);
    assert_eq!(app.tabs[0].query.text, "SELECT");
    assert!(!app.completion_update(false));
    assert!(app.workbench.query_ui.completion.current.is_none());
    assert!(app.tabs[0].query.undo());
    assert_eq!(app.tabs[0].query.text, "SELect");
}

#[test]
fn stale_candidate_cannot_edit_new_revision_or_new_list() {
    let mut app = app("SEL");
    app.completion_update(true);
    let old_id = app
        .workbench
        .query_ui
        .completion
        .current
        .as_ref()
        .unwrap()
        .id;
    app.tabs[0].query.edit(0..3, "FRO");
    app.changed_field("query");
    app.completion_update(true);
    app.completion_accept(old_id, 0);
    assert_eq!(app.tabs[0].query.text, "FRO");
    assert!(app.workbench.query_ui.completion.current.is_some());
}

#[test]
fn late_catalog_response_cannot_cancel_new_pending_request() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app("SELECT * FROM us");
    app.completion_update(true);
    app.completion_poll(&backend);
    let first = requests.try_recv().unwrap();
    app.tabs[0].query.edit(14..16, "ord");
    app.changed_field("query");
    app.completion_update(true);
    app.completion_poll(&backend);
    let mut second = requests.try_recv().unwrap();
    if second.method == "completion.unsubscribe" {
        second = requests.try_recv().unwrap();
    }
    app.routed_response(Response { tag: first.tag, result: Ok(json!({"request_id": first.params["request_id"], "context_generation":first.params["context_generation"], "items":[], "completeness":"complete"})) }, &backend);
    assert_eq!(
        app.workbench
            .query_ui
            .completion
            .pending
            .as_ref()
            .unwrap()
            .0
            .to_string(),
        second.params["request_id"].as_str().unwrap()
    );
}

#[test]
fn qualifier_uses_confirmed_table_name_case_rules() {
    let analysis = analysis::analyze(
        "SELECT u. FROM users U",
        9,
        editor::SqlContext::Session(editor::MysqlMode::default()),
        Some("test"),
    )
    .unwrap();
    assert!(keys(&analysis, Some("test"), None, false).is_empty());
    assert!(keys(&analysis, Some("test"), Some(0), false).is_empty());
    assert_eq!(keys(&analysis, Some("test"), Some(1), false).len(), 1);
}

#[test]
fn functions_use_supported_server_versions() {
    assert!(
        keyword_candidates("COU", None)
            .iter()
            .any(|item| item.name == "COUNT")
    );
    assert!(
        keyword_candidates("COU", Some("8.0.36"))
            .iter()
            .any(|item| item.name == "COUNT")
    );
    assert!(
        !keyword_candidates("COU", Some("5.6.51"))
            .iter()
            .any(|item| item.name == "COUNT")
    );
}

#[test]
fn archery_multi_table_expression_never_requests_all_table_structures() {
    let sql = "SELECT na FROM users u JOIN orders o ON o.user_id = u.id";
    let analysis = analysis::analyze(sql, 9, editor::SqlContext::Stateless, Some("test")).unwrap();
    assert_eq!(keys(&analysis, Some("test"), None, false).len(), 2);
    assert!(keys(&analysis, Some("test"), None, true).is_empty());
    let sql = "SELECT u.na FROM users u JOIN orders o ON o.user_id = u.id";
    let analysis = analysis::analyze(sql, 11, editor::SqlContext::Stateless, Some("test")).unwrap();
    let targets = keys(&analysis, Some("test"), None, true);
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].relation.as_deref(), Some("users"));
}

#[test]
fn archery_single_table_fields_remain_available() {
    let sql = "SELECT na FROM users u";
    let analysis = analysis::analyze(sql, 9, editor::SqlContext::Stateless, Some("test")).unwrap();
    let targets = keys(&analysis, Some("test"), None, true);
    assert_eq!(targets.len(), 1);
    assert_eq!(targets[0].relation.as_deref(), Some("users"));
}

#[test]
fn catalog_pages_are_reachable_and_acceptance_is_one_undo() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app("SELECT * FROM tbl_");
    app.completion_update(true);
    app.completion_poll(&backend);
    let first = requests.try_recv().unwrap();
    let items: Vec<_> = (0..200)
        .map(|index| json!({"name": format!("tbl_{index:03}"), "kind":"table"}))
        .collect();
    app.routed_response(Response {
        tag: first.tag,
        result: Ok(json!({"request_id": first.params["request_id"], "context_generation":first.params["context_generation"], "catalog_generation":1, "items":items, "completeness":"partial", "next_cursor":"next-page"})),
    }, &backend);

    for _ in 0..150 {
        assert!(app.completion_key(&tern_sdk::Key::named("down")));
    }
    let snapshot = app.workbench.query_ui.completion.current.as_ref().unwrap();
    assert_eq!(snapshot.candidates.len(), 50);
    assert_eq!(snapshot.candidates[snapshot.selected].name, "tbl_150");
    for _ in 0..50 {
        assert!(app.completion_key(&tern_sdk::Key::named("down")));
    }
    app.completion_poll(&backend);
    let next = requests.try_recv().unwrap();
    assert_eq!(next.params["cursor"], "next-page");
    app.routed_response(Response {
        tag: next.tag,
        result: Ok(json!({"request_id":next.params["request_id"], "context_generation":next.params["context_generation"], "catalog_generation":1, "items":[{"name":"tbl_200","kind":"table"}], "completeness":"complete", "next_cursor":null})),
    }, &backend);
    assert!(app.completion_key(&tern_sdk::Key::named("enter")));
    assert_eq!(app.tabs[0].query.text, "SELECT * FROM `tbl_200`");
    assert!(app.tabs[0].query.undo());
    assert_eq!(app.tabs[0].query.text, "SELECT * FROM tbl_");
    app.completion_update(true);
    app.completion_poll(&backend);
    let reopened = requests.try_recv().unwrap();
    assert_eq!(reopened.params["scope"], "relations");
    assert!(reopened.params["cursor"].is_null());
    app.routed_response(Response {
        tag: reopened.tag,
        result: Ok(json!({"request_id":reopened.params["request_id"],"context_generation":reopened.params["context_generation"],"catalog_generation":1,"items":[{"name":"tbl_000","kind":"table"}],"completeness":"complete"})),
    }, &backend);
    assert!(app.completion_key(&tern_sdk::Key::named("enter")));
    assert_eq!(app.tabs[0].query.text, "SELECT * FROM `tbl_000`");
}

#[test]
fn late_continuation_cannot_pollute_new_context() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app("SELECT * FROM tbl_");
    app.completion_update(true);
    app.completion_poll(&backend);
    let first = requests.try_recv().unwrap();
    app.routed_response(Response {
        tag: first.tag,
        result: Ok(json!({"request_id":first.params["request_id"], "context_generation":first.params["context_generation"], "catalog_generation":1, "items":[{"name":"tbl_000","kind":"table"}], "completeness":"partial", "next_cursor":"next-page"})),
    }, &backend);
    assert!(app.completion_key(&tern_sdk::Key::named("down")));
    app.completion_poll(&backend);
    let continuation = requests.try_recv().unwrap();
    assert_eq!(continuation.params["cursor"], "next-page");
    app.tabs[0].query.edit(14..18, "new_");
    app.changed_field("query");
    app.completion_update(true);
    app.completion_poll(&backend);
    let mut current = requests.try_recv().unwrap();
    if current.method == "completion.unsubscribe" {
        current = requests.try_recv().unwrap();
    }
    app.routed_response(Response {
        tag: continuation.tag,
        result: Ok(json!({"request_id":continuation.params["request_id"], "context_generation":continuation.params["context_generation"], "catalog_generation":1, "items":[{"name":"tbl_stale","kind":"table"}], "completeness":"complete"})),
    }, &backend);
    assert_eq!(
        app.workbench
            .query_ui
            .completion
            .pending
            .as_ref()
            .unwrap()
            .0
            .to_string(),
        current.params["request_id"].as_str().unwrap()
    );
    app.routed_response(Response {
        tag: current.tag,
        result: Ok(json!({"request_id":current.params["request_id"], "context_generation":current.params["context_generation"], "catalog_generation":1, "items":[{"name":"new_table","kind":"table"}], "completeness":"complete"})),
    }, &backend);
    let snapshot = app.workbench.query_ui.completion.current.as_ref().unwrap();
    assert!(
        snapshot
            .candidates
            .iter()
            .any(|item| item.name == "new_table")
    );
    assert!(
        !snapshot
            .candidates
            .iter()
            .any(|item| item.name == "tbl_stale")
    );
}

#[test]
fn expired_catalog_is_refreshed_before_keyboard_navigation() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app("SELECT * FROM test.tbl_");
    app.completion_update(true);
    app.completion_poll(&backend);
    let first = requests.try_recv().unwrap();
    app.routed_response(Response {
        tag: first.tag,
        result: Ok(json!({"request_id":first.params["request_id"],"context_generation":first.params["context_generation"],"catalog_generation":1,"items":[{"name":"tbl_old","kind":"table"}],"completeness":"partial","next_cursor":"expired-cursor"})),
    }, &backend);
    app.workbench.query_ui.completion.pages[0].loaded = Instant::now() - CACHE_TTL;

    assert!(app.completion_key(&tern_sdk::Key::named("down")));
    app.completion_poll(&backend);
    let refreshed = requests.try_recv().unwrap();
    assert!(refreshed.params["cursor"].is_null());
    app.routed_response(Response {
        tag: refreshed.tag,
        result: Ok(json!({"request_id":refreshed.params["request_id"],"context_generation":refreshed.params["context_generation"],"catalog_generation":2,"items":[{"name":"tbl_fresh","kind":"table"}],"completeness":"complete"})),
    }, &backend);
    assert!(app.completion_key(&tern_sdk::Key::named("enter")));
    assert_eq!(app.tabs[0].query.text, "SELECT * FROM test.`tbl_fresh`");
}
