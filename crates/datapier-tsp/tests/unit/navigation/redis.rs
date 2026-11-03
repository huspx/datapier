use super::*;

#[test]
fn redis_switch_persists_database_for_originating_background_tab() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, mut profile, old_session) = redis_app();
    assert!(app.navigation_action("redis-db:3", &backend));
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "connection.open");
    assert_eq!(request.params["redis_db"], 3);

    app.action("new-tab", &backend);
    app.dirty = false;
    profile.direct_mut().unwrap().redis_db = 3;
    let new_session = Uuid::new_v4();
    assert!(app.navigation_response(
        &Response {
            tag: request.tag,
            result: Ok(json!({"session_id": new_session, "profile": profile, "redis_db":3})),
        },
        &backend
    ));

    assert_eq!(app.active, 1);
    assert_eq!(app.tabs[0].browse.redis_db, Some(3));
    assert!(app.dirty);
    let close = requests.try_recv().unwrap();
    assert_eq!(close.method, "connection.close");
    assert_eq!(close.params["session_id"], json!(old_session));
    let list = requests.try_recv().unwrap();
    assert_eq!(list.method, "resource.list");
    assert_eq!(list.params["session_id"], json!(new_session));
    app.switch_tab(0);
    assert_eq!(app.profile.as_ref().unwrap().redis_db(), 3);
    assert_eq!(app.session_id(), Some(json!(new_session)));
}

#[test]
fn redis_database_switch_keeps_cli_history_and_new_command_target() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, mut profile, old_session) = redis_app();
    app.workbench.cli = true;
    app.tabs[0].browse.redis_db = Some(0);
    app.field_set("cli_text", "PING");
    app.workbench_action("cli-run", &backend);
    let start = requests.try_recv().unwrap();
    assert_eq!(start.params["database"], "0");
    app.workbench_response("cli.start", json!({"task_id":"first"}), &backend);
    let result_id = Uuid::new_v4();
    app.workbench_response(
        "execution.get",
        json!({"state":"succeeded","results":[result_id]}),
        &backend,
    );
    let _page = requests.try_recv().unwrap();
    let record_id = app.workbench.cli_history[0].id;
    assert_eq!(app.workbench.cli_history[0].database, "0");

    app.navigation_action("redis-db:1", &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.method, "connection.open");
    assert_eq!(open.params["redis_db"], 1);
    profile.direct_mut().unwrap().redis_db = 1;
    let new_session = Uuid::new_v4();
    app.navigation_response(
        &Response {
            tag: open.tag,
            result: Ok(json!({"session_id":new_session,"profile":profile,"redis_db":1})),
        },
        &backend,
    );
    assert_eq!(
        requests.try_recv().unwrap().params["session_id"],
        json!(old_session)
    );
    assert_eq!(requests.try_recv().unwrap().method, "resource.list");
    assert_eq!(app.tabs[0].browse.redis_db, Some(1));

    app.workbench_action(&format!("cli-record:{record_id}:0"), &backend);
    assert_eq!(requests.try_recv().unwrap().method, "result.page");
    assert_eq!(app.tabs[0].browse.redis_db, Some(1));
    app.field_set("cli_text", "GET x");
    app.workbench_action("cli-run", &backend);
    let start = requests.try_recv().unwrap();
    assert_eq!(start.method, "execution.start");
    assert_eq!(start.params["session_id"], json!(new_session));
    assert_eq!(start.params["database"], "1");
    assert_eq!(app.workbench.cli_history[0].database, "0");
    assert_eq!(app.workbench.cli_history[1].database, "1");
}

#[test]
fn rejected_redis_switch_keeps_original_database_and_session() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, old_session) = redis_app();
    app.tabs[0].browse.redis_db = Some(profile.redis_db());
    assert!(app.navigation_action("redis-db:3", &backend));
    let request = requests.try_recv().unwrap();
    let rejected_session = Uuid::new_v4();
    assert!(app.navigation_response(
        &Response {
            tag: request.tag,
            result: Ok(json!({"session_id": rejected_session, "profile": profile})),
        },
        &backend
    ));

    assert_eq!(app.tabs[0].browse.redis_db, Some(0));
    assert_eq!(app.session_id(), Some(json!(old_session)));
    assert!(!app.dirty);
    assert!(!app.operation_pending);
    let close = requests.try_recv().unwrap();
    assert_eq!(close.method, "connection.close");
    assert_eq!(close.params["session_id"], json!(rejected_session));
}
