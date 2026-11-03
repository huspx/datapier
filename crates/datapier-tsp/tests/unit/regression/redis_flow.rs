use super::*;

#[test]
fn cli_command_and_late_failure_stay_with_originating_tab() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("cli-tabs".into());
    app.profile = Some(ui_profile("redis", "first"));
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.field_set("cli_text", "GET first");
    app.action("cli-run", &backend);
    let start = requests.try_recv().unwrap();
    app.field_set("cli_text", "GET edited");
    second_tab(&mut app);
    app.switch_tab(1);
    app.profile = Some(ui_profile("redis", "second"));
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.field_set("cli_text", "PING");
    app.action("cli-run", &backend);
    requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: start.tag,
            result: Err(AppError::new("failure", "first failure")),
        },
        &backend,
    );
    assert_eq!(app.workbench.cli_command.as_deref(), Some("PING"));
    assert!(app.workbench.cli_error.is_empty());
    app.switch_tab(0);
    assert_eq!(app.field("cli_text"), "GET edited");
    assert_eq!(app.workbench.cli_command.as_deref(), Some("GET first"));
    assert!(app.workbench.cli_error.render().contains("first failure"));
}
#[test]
fn redis_cli_enter_runs_after_switching_from_mysql() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("cross-provider".into());
    let mysql = ui_profile("mysql", "mysql");
    let redis = ui_profile("redis", "redis");
    app.profiles = vec![mysql.clone(), redis.clone()];
    app.profile = Some(mysql.clone());
    app.tabs[0].profile_id = Some(mysql.id);
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.select_profile(1, &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.method, "connection.open");
    let session = Uuid::new_v4();
    app.routed_response(
        Response {
            tag: open.tag,
            result: Ok(json!({"session_id": session, "profile": redis})),
        },
        &backend,
    );
    while requests.try_recv().is_ok() {}
    assert_eq!(app.view, "query");
    app.action("cli-toggle", &backend);
    app.dispatch_editor_message(Msg::Edit("cli_text".into(), 0, 0, "PING".into(), 4, 0));
    assert!(app.workbench_key(&tern_sdk::Key::named("enter"), &backend));
    let execution = requests.try_recv().unwrap();
    assert_eq!(execution.method, "execution.start");
    assert_eq!(execution.params["text"], "PING");
    assert_eq!(execution.params["session_id"], json!(session));
}

#[test]
fn cli_text_selection_and_visibility_survive_workspace_reopen() {
    let mut app = App::new("test".into());
    second_tab(&mut app);
    app.field_set("cli_text", "PING\nECHO 中文😀");
    let end = app.field("cli_text").encode_utf16().count();
    app.editor("cli_text").unwrap().set_selection(5, end);
    app.workbench.cli = true;
    app.switch_tab(1);
    app.field_set("cli_text", "GET user:1042");
    let saved: Workspace =
        serde_json::from_value(serde_json::to_value(app.workspace_snapshot()).unwrap()).unwrap();
    let mut reopened = App::new("test".into());
    reopened.tabs = saved.tabs;
    reopened.active = saved.active;
    reopened.restore_browse();
    assert_eq!(reopened.field("cli_text"), "GET user:1042");
    reopened.switch_tab(0);
    assert_eq!(reopened.field("cli_text"), "PING\nECHO 中文😀");
    assert_eq!(
        reopened.tabs[0].cli_query.selected_text(),
        Some("ECHO 中文😀")
    );
    assert!(reopened.workbench.cli);
}

#[test]
fn redis_workspace_migrates_the_previous_command_editor_without_losing_text() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("test".into());
    let profile = ui_profile("redis", "legacy");
    app.tabs[0].profile_id = Some(profile.id);
    app.tabs[0].query = Editor::new("PING\nECHO 保存😀");
    app.response(
        Response {
            tag: "profile.list".into(),
            result: Ok(json!({"profiles":[profile]})),
        },
        &backend,
    );
    assert_eq!(app.field("cli_text"), "PING\nECHO 保存😀");
    assert!(app.tabs[0].query.text.is_empty());
    assert!(app.tabs[0].browse.cli_visible);
}
