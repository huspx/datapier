use super::*;

#[test]
fn language_saves_are_serialized_and_coalesce_to_latest_selection() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.application_action("language:en", &backend);
    let first = requests.try_recv().unwrap();
    app.application_action("language:zh-CN", &backend);
    app.application_action("language:system", &backend);
    assert!(requests.try_recv().is_err());
    app.settings_response(
        Response {
            tag: first.tag,
            result: Ok(first.params["settings"].clone()),
        },
        &backend,
    );
    let latest = requests.try_recv().unwrap();
    assert_eq!(latest.method, "settings.set");
    assert_eq!(latest.params["settings"]["language"], "system");
    assert_eq!(app.settings.language, LanguagePreference::System);
    app.settings_response(
        Response {
            tag: latest.tag,
            result: Ok(latest.params["settings"].clone()),
        },
        &backend,
    );
    assert!(!app.settings_unsaved);
    assert!(app.settings_save_token.is_none());
    assert!(requests.try_recv().is_err());
}

#[test]
fn clean_settings_refresh_updates_form_and_revision_together() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("window-a".into());
    let original = app.settings.clone();
    app.settings_response(
        Response {
            tag: "settings.get/0".into(),
            result: Ok(serde_json::to_value(&original).unwrap()),
        },
        &backend,
    );

    let mut other_window = original.clone();
    other_window.revision += 1;
    other_window.paging.mysql_table_rows = 137;
    app.settings_response(
        Response {
            tag: "settings.get/0".into(),
            result: Ok(serde_json::to_value(&other_window).unwrap()),
        },
        &backend,
    );
    assert_eq!(app.field("settings-paging-mysql_table_rows"), "137");
    assert!(!app.settings_form_dirty());
    app.field_set("settings-paging-sql_rows", "123");
    app.application_action("settings-save", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(
        request.params["settings"]["revision"],
        json!(other_window.revision)
    );
    assert_eq!(
        request.params["settings"]["paging"]["mysql_table_rows"],
        json!(137)
    );
    assert_eq!(request.params["settings"]["paging"]["sql_rows"], json!(123));
}

#[test]
fn dirty_closed_settings_keep_revision_and_conflict_after_remote_refresh() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("window-a".into());
    let original = app.settings.clone();
    app.settings_response(
        Response {
            tag: "settings.get/0".into(),
            result: Ok(serde_json::to_value(&original).unwrap()),
        },
        &backend,
    );
    app.field_set("settings-paging-sql_rows", "123");
    app.workbench.overlay = None;

    let mut other_window = original.clone();
    other_window.revision += 1;
    other_window.paging.mysql_table_rows = 137;
    app.settings_response(
        Response {
            tag: "settings.get/0".into(),
            result: Ok(serde_json::to_value(&other_window).unwrap()),
        },
        &backend,
    );
    assert_eq!(app.settings.revision, original.revision);
    assert_eq!(app.field("settings-paging-sql_rows"), "123");
    assert!(app.settings_form_dirty());

    app.application_action("settings-save", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(
        request.params["settings"]["revision"],
        json!(original.revision)
    );
    app.settings_response(
        Response {
            tag: request.tag,
            result: Err(AppError::new("revision_conflict", "conflict")),
        },
        &backend,
    );
    assert!(app.field("settings-error").contains("conflict"));
    assert_eq!(app.field("settings-paging-sql_rows"), "123");
    assert!(app.settings_form_dirty());
}

#[test]
fn failed_settings_write_keeps_selection_until_explicit_reload() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.application_action("language:en", &backend);
    let first = requests.try_recv().unwrap();
    app.application_action("language:zh-CN", &backend);
    app.settings_response(
        Response {
            tag: first.tag,
            result: Err(AppError::new("revision_conflict", "save failed")),
        },
        &backend,
    );
    assert!(app.settings_unsaved);
    assert!(app.settings_save_token.is_none());
    assert_eq!(app.settings.language, LanguagePreference::ZhCn);
    assert!(format!("{:?}", app.status).contains("save failed"));
    assert!(requests.try_recv().is_err());

    let mut saved = app.settings.clone();
    saved.revision += 1;
    saved.language = LanguagePreference::En;
    app.application_action("settings-reload", &backend);
    let reload = requests.try_recv().unwrap();
    app.settings_response(
        Response {
            tag: reload.tag,
            result: Ok(serde_json::to_value(&saved).unwrap()),
        },
        &backend,
    );
    assert!(!app.settings_unsaved);
    assert_eq!(app.settings.language, LanguagePreference::En);
    assert!(requests.try_recv().is_err());
    app.application_action("language:zh-CN", &backend);
    let latest = requests.try_recv().unwrap();
    assert_eq!(latest.params["settings"]["revision"], json!(saved.revision));
    assert_eq!(latest.params["settings"]["language"], "zh-CN");
}

#[test]
fn settings_tabs_keep_drafts_and_footer_saves_all_fields() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("settings".into());
    let saved = serde_json::to_value(&app.settings).unwrap();
    app.settings_response(
        Response {
            tag: "settings.get/0".into(),
            result: Ok(saved),
        },
        &backend,
    );
    app.field_set("editing_plan_items", "234");
    app.field_set("external_mode", "gui");
    app.field_set("external_program", "editor");
    app.field_set("external_args", "--wait\n{file}");
    app.application_action("settings-language:en", &backend);
    app.application_action("settings-tab:mcp", &backend);
    app.application_action("settings-toggle:notifications:approval", &backend);
    app.field_set("mcp-rule-profile", "selected");
    app.field_set("mcp-rule-error", "permission failed");
    app.application_action("settings-tab:general", &backend);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.field("mcp-rule-profile"), "selected");
    assert_eq!(app.field("mcp-rule-error"), "permission failed");
    app.application_action("settings-save", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "settings.set");
    assert_eq!(request.params["settings"]["editing"]["max_plan_items"], 234);
    assert_eq!(request.params["settings"]["language"], "en");
    assert_eq!(
        request.params["settings"]["external_editor"]["args"],
        json!(["--wait", "{file}"])
    );
    assert_eq!(
        request.params["settings"]["notifications"]["approval"],
        json!(app.field("settings-notifications-approval") == "true")
    );
    app.application_action("settings-reload", &backend);
    app.application_action("settings-save", &backend);
    assert!(requests.try_recv().is_err());
    app.settings_response(
        Response {
            tag: request.tag,
            result: Err(AppError::new("revision_conflict", "conflict")),
        },
        &backend,
    );
    app.application_action("settings-tab:mcp", &backend);
    assert_eq!(app.field("editing_plan_items"), "234");
    assert!(app.field("settings-error").contains("conflict"));
}

#[test]
fn settings_invalid_editor_and_failed_reload_preserve_drafts() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("settings".into());
    let saved = serde_json::to_value(&app.settings).unwrap();
    app.settings_response(
        Response {
            tag: "settings.get/0".into(),
            result: Ok(saved),
        },
        &backend,
    );
    app.field_set("external_mode", "gui");
    app.field_set("external_program", "editor");
    app.field_set("external_args", "--wait\n{file}\n{file}");
    app.application_action("settings-save", &backend);
    assert!(requests.try_recv().is_err());
    assert_eq!(
        app.workbench
            .field_error
            .as_ref()
            .map(|error| error.field.as_str()),
        Some("external_args")
    );
    assert_eq!(app.field("external_args"), "--wait\n{file}\n{file}");
    app.application_action("settings-reload", &backend);
    assert_eq!(app.field("settings-reload-confirm"), "true");
    assert!(requests.try_recv().is_err());
    app.application_action("settings-reload-confirm", &backend);
    let request = requests.try_recv().unwrap();
    app.settings_response(
        Response {
            tag: request.tag,
            result: Err(AppError::new("unavailable", "reload failed")),
        },
        &backend,
    );
    assert_eq!(app.field("external_args"), "--wait\n{file}\n{file}");
    assert!(app.field("settings-error").contains("reload failed"));
    app.application_action("settings-reload", &backend);
    assert!(requests.try_recv().is_err());
    app.application_action("settings-reload-confirm", &backend);
    let request = requests.try_recv().unwrap();
    let persisted = serde_json::to_value(&app.settings).unwrap();
    app.settings_response(
        Response {
            tag: request.tag,
            result: Ok(persisted),
        },
        &backend,
    );
    assert_eq!(app.field("external_mode"), "disabled");
    assert_eq!(app.field("external_args"), "");
    assert!(app.field("settings-error").is_empty());
}

#[test]
fn history_sizes_use_saved_default_and_all_pager_options() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("history-sizes".into());
    app.settings.paging.history_rows = 200;
    app.open_history(&backend);
    assert_eq!(requests.try_recv().unwrap().params["limit"], 200);
    for size in [20, 50, 100, 137, 200, 500, 1000] {
        app.application_action(&format!("history-size:{size}"), &backend);
        let request = requests.try_recv().unwrap();
        assert_eq!(request.params["limit"], size);
    }
    app.workbench.overlay = None;
    app.settings.paging.history_rows = 50;
    app.open_history(&backend);
    assert_eq!(app.workbench.archery_history.limit, 1000);
    assert!(requests.try_recv().is_err());
}

#[test]
fn closed_history_response_is_cached_for_its_own_tab() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("history-late".into());
    app.open_history(&backend);
    let request = requests.try_recv().unwrap();
    app.action("overlay-close", &backend);
    app.create_query_tab();
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"items":[{"text":"saved"}],"total":1,"offset":0,"limit":20})),
        },
        &backend,
    );
    assert_eq!(app.active, 1);
    app.switch_tab(0);
    assert!(app.workbench.archery_history.pending.is_none());
    app.open_history(&backend);
    assert_eq!(app.history_selected_text(), Some("saved"));
    assert!(requests.try_recv().is_err());
}

#[test]
fn history_search_and_pagination_use_only_applied_condition() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("history".into());
    app.open_history(&backend);
    let first = requests.try_recv().unwrap();
    assert_eq!(first.method, "history.list");
    assert_eq!(first.params["search"], "");
    assert_eq!(first.params["limit"], 20);
    app.field_set("history_filter", "needle");
    assert!(requests.try_recv().is_err());
    let first_token = first.tag.strip_prefix("history.page/").unwrap();
    app.history_response(
        first_token,
        &Ok(json!({"items":[{"text":"old"}],"total":100,"offset":0,"limit":20})),
        &backend,
    );
    app.application_action("history-search", &backend);
    let search = requests.try_recv().unwrap();
    assert_eq!(search.params["search"], "needle");
    let token = search.tag.strip_prefix("history.page/").unwrap();
    app.history_response(
        token,
        &Ok(json!({"items":[{"text":"found"}],"total":100,"offset":0,"limit":20})),
        &backend,
    );
    app.field_set("history_filter", "draft");
    app.application_action("history-next", &backend);
    let page = requests.try_recv().unwrap();
    assert_eq!(page.params["search"], "needle");
    assert_eq!(page.params["offset"], 20);
    app.application_action("history-size:50", &backend);
    let size = requests.try_recv().unwrap();
    assert_eq!(size.params["search"], "needle");
    assert_eq!(size.params["offset"], 0);
    assert_eq!(size.params["limit"], 50);
    app.history_response(
        page.tag.strip_prefix("history.page/").unwrap(),
        &Ok(json!({"items":[{"text":"stale"}],"total":100,"offset":20,"limit":20})),
        &backend,
    );
    assert!(app.workbench.archery_history.items.is_empty());
}

#[test]
fn platform_history_ignores_previous_session_and_exposes_auth_error() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _, _) = archery_task_app(datapier_core::model::ProviderKind::Mysql);
    app.open_history(&backend);
    let first = requests.try_recv().unwrap();
    let token = first.tag.strip_prefix("history.page/").unwrap();
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.history_response(
        token,
        &Ok(json!({"rows":[{"sql":"stale"}],"total":1,"offset":0,"limit":20})),
        &backend,
    );
    assert!(app.workbench.archery_history.items.is_empty());
    app.application_action("history-search", &backend);
    assert_eq!(
        requests.try_recv().unwrap().method,
        "archery.operation.cancel"
    );
    let request = requests.try_recv().unwrap();
    let token = request.tag.strip_prefix("history.page/").unwrap();
    app.history_response(
        token,
        &Err(AppError::new("auth_required", "login expired")),
        &backend,
    );
    assert!(app.workbench.archery_history.auth_required);
    assert!(
        app.workbench
            .archery_history
            .error
            .as_ref()
            .unwrap()
            .contains("login expired")
    );
}

#[test]
fn platform_history_reads_archery_query_log_sql() {
    let row = json!({"instance_name":"datapier-mysql", "db_name":"fixture", "sqllog":"SELECT * FROM items limit 1000;"});
    assert_eq!(
        archery_history_text(&row),
        Some("SELECT * FROM items limit 1000;")
    );
}

#[test]
fn truncated_history_loads_full_text_and_ignores_late_selection() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("history-full".into());
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    app.open_history(&backend);
    let page = requests.try_recv().unwrap();
    app.history_response(
        page.tag.strip_prefix("history.page/").unwrap(),
        &Ok(json!({
            "items":[{"id":first,"text":"SELECT first","text_truncated":true},
                     {"id":second,"text":"SELECT second","text_truncated":true}],
            "total":2,"offset":0,"limit":20
        })),
        &backend,
    );
    let initial = requests.try_recv().unwrap();
    assert!(app.history_selected_text().is_none());
    app.application_action("history-open", &backend);
    assert_eq!(app.workbench.overlay.as_deref(), Some("history"));
    app.application_action("history-select:1", &backend);
    let selected = requests.try_recv().unwrap();
    app.history_text_response(
        initial.tag.strip_prefix("history.text/").unwrap(),
        &Ok(json!({"id":first,"text":"SELECT first complete"})),
    );
    assert!(app.history_selected_text().is_none());
    let complete = format!("SELECT second {}", "文".repeat(2048));
    app.history_text_response(
        selected.tag.strip_prefix("history.text/").unwrap(),
        &Ok(json!({"id":second,"text":complete})),
    );
    assert_eq!(app.history_selected_text(), Some(complete.as_str()));
    app.application_action("history-open", &backend);
    assert_eq!(app.tabs[app.active].query.text, complete);
    assert!(app.workbench.overlay.is_none());
}

fn archery_task_app(provider: datapier_core::model::ProviderKind) -> (App, Uuid, String) {
    let server_id = Uuid::new_v4();
    let mut app = App::new("task-auth".into());
    app.profile = Some(serde_json::from_value(json!({
        "id":Uuid::new_v4(), "name":"Archery", "provider":provider, "access_mode":"read_only",
        "connection":{"kind":"archery","server_id":server_id,"instance_id":7,"instance_name":"fixture",
            "databases":["0"],"default_database":"0"}
    })).unwrap());
    app.profiles = app.profile.iter().cloned().collect();
    app.tabs[app.active].profile_id = app.profile.as_ref().map(|profile| profile.id);
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.database = Some("0".into());
    app.tabs[app.active].query = Editor::new("SELECT 1");
    app.field_set("cli_text", "GET 'kept key'");
    let task_id = Uuid::new_v4().to_string();
    app.task = Some(task_id.clone());
    app.workbench.cli_running = provider == datapier_core::model::ProviderKind::Redis;
    (app, server_id, task_id)
}

#[test]
fn sql_task_auth_required_opens_relogin_without_replaying_query() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, server_id, task_id) = archery_task_app(datapier_core::model::ProviderKind::Mysql);
    let session = app.session_id();
    app.response(Response { tag:"execution.get".into(), result:Ok(json!({
        "id":task_id,"state":"failed","method":"execution.start","results":[],
        "error":{"code":"auth_required","message":"expired","details":{"server_id":server_id}}
    })) }, &backend);
    assert!(app.task.is_none());
    assert!(app.archery.open);
    assert_eq!(app.archery.reauth_server, Some(server_id));
    assert_eq!(app.session_id(), session);
    assert_eq!(app.tabs[app.active].query.text, "SELECT 1");
    assert_eq!(requests.try_recv().unwrap().method, "archery.server.list");
    let operation = Uuid::new_v4();
    app.archery.pending = Some((operation, "archery.auth.begin".into()));
    let tag = format!(
        "archery-ui/{}/{}/archery.auth.begin",
        app.archery.epoch, operation
    );
    app.archery_response(
        Response {
            tag,
            result: Ok(json!({"auth_id":"auth-id","state":"ready","auth_generation":1})),
        },
        &backend,
    );
    let release = requests.try_recv().unwrap();
    assert_eq!(release.method, "archery.auth.close");
    app.routed_response(
        Response {
            tag: release.tag,
            result: Ok(json!({"closed":true})),
        },
        &backend,
    );
    assert_eq!(app.tabs[app.active].query.text, "SELECT 1");
    assert_eq!(app.session_id(), session);
    assert!(requests.try_recv().is_err());
}

#[test]
fn redis_task_auth_required_stops_cli_and_preserves_command_for_relogin() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, server_id, task_id) = archery_task_app(datapier_core::model::ProviderKind::Redis);
    let session = app.session_id();
    app.response(Response { tag:"execution.get".into(), result:Ok(json!({
        "id":task_id,"state":"failed","method":"execution.start","results":[],
        "error":{"code":"auth_required","message":"expired","details":{"server_id":server_id}}
    })) }, &backend);
    assert!(app.task.is_none());
    assert!(!app.workbench.cli_running);
    assert!(app.archery.open);
    assert_eq!(app.archery.reauth_server, Some(server_id));
    assert_eq!(app.session_id(), session);
    assert_eq!(app.field("cli_text"), "GET 'kept key'");
    assert_eq!(requests.try_recv().unwrap().method, "archery.server.list");
    assert!(requests.try_recv().is_err());
}

#[test]
fn task_permission_denied_does_not_request_reauthentication() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _, task_id) = archery_task_app(datapier_core::model::ProviderKind::Mysql);
    app.response(
        Response {
            tag: "execution.get".into(),
            result: Ok(json!({
                "id":task_id,"state":"failed","method":"execution.start","results":[],
                "error":{"code":"permission_denied","message":"query denied","details":{}}
            })),
        },
        &backend,
    );
    assert!(app.task.is_none());
    assert!(!app.archery.open);
    assert_eq!(app.tabs[app.active].query.text, "SELECT 1");
    assert!(requests.try_recv().is_err());
}

#[test]
fn sql_wait_stopped_is_terminal_and_keeps_remote_state_unknown() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _, task_id) = archery_task_app(datapier_core::model::ProviderKind::Mysql);
    app.response(
        Response {
            tag: "execution.get".into(),
            result: Ok(json!({
                "id":task_id,"state":"wait_stopped","method":"execution.start","results":[],
                "summary":{"remote_completion":"unknown"},
                "error":{"code":"wait_stopped","message":"local wait ended","details":{}}
            })),
        },
        &backend,
    );
    assert!(app.task.is_none());
    assert!(!app.operation_pending);
    assert!(!app.archery.open);
    let details: Value = serde_json::from_str(&app.detail).unwrap();
    assert_eq!(details["state"], "wait_stopped");
    assert_eq!(details["summary"]["remote_completion"], "unknown");
    assert_eq!(app.tabs[app.active].query.text, "SELECT 1");
    assert!(requests.try_recv().is_err());
}

#[test]
fn platform_history_opens_unexecuted_draft_in_current_profile_and_database() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _, _) = archery_task_app(datapier_core::model::ProviderKind::Mysql);
    app.task = None;
    if let datapier_core::model::ConnectionConfig::Archery(source) =
        &mut app.profile.as_mut().unwrap().connection
    {
        source.databases.push("1".into());
    }
    app.profiles[0] = app.profile.as_ref().unwrap().clone();
    app.database = Some("1".into());
    app.view = "data".into();
    app.browse_result = true;
    let target = ResourceTarget {
        database: Some("1".into()),
        name: "items".into(),
        kind: "table".into(),
        key_b64: None,
    };
    app.target = Some(target.clone());
    let profile_id = app.profile.as_ref().unwrap().id;
    let session = app.session_id();
    app.workbench.archery_history.items = vec![json!({
        "db_name":"different_db","instance_name":"different_instance","sqllog":"SELECT historical FROM items"
    })];
    app.workbench.archery_history.selected = Some(0);
    app.application_action("history-open", &backend);
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.view, "query");
    assert_eq!(
        app.tabs[app.active].query.text,
        "SELECT historical FROM items"
    );
    assert_eq!(app.profile.as_ref().unwrap().id, profile_id);
    assert_eq!(app.database.as_deref(), Some("1"));
    assert_eq!(app.tabs[0].query.text, "SELECT 1");
    let previous_target = app.tabs[0].browse.target.as_ref().unwrap();
    assert_eq!(previous_target.database, target.database);
    assert_eq!(previous_target.name, target.name);
    assert_eq!(previous_target.kind, target.kind);
    assert_eq!(previous_target.key_b64, target.key_b64);
    assert_eq!(
        app.tabs[0]
            .runtime
            .session
            .as_ref()
            .and_then(|value| value.get("session_id"))
            .cloned(),
        session
    );
    while let Ok(request) = requests.try_recv() {
        assert_eq!(request.method, "connection.open");
        assert_eq!(request.params["profile_id"], json!(profile_id));
        assert_eq!(request.params["database"], "1");
    }
}

#[test]
fn platform_history_keeps_pending_drafts_and_running_tasks_intact() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _, task_id) = archery_task_app(datapier_core::model::ProviderKind::Mysql);
    app.workbench.overlay = Some("history".into());
    app.workbench.archery_history.items = vec![json!({"sqllog":"SELECT historical"})];
    app.workbench.archery_history.selected = Some(0);
    app.application_action("history-open", &backend);
    assert_eq!(app.task.as_deref(), Some(task_id.as_str()));
    assert_eq!(app.tabs[app.active].query.text, "SELECT 1");
    assert_eq!(app.workbench.overlay.as_deref(), Some("history"));
    app.task = None;
    let draft = json!({"rows":[{"operation":"update","before":{"id":1},"values":{"name":"kept"}}]});
    app.tabs[app.active].pending = draft.clone();
    app.application_action("history-open", &backend);
    assert_eq!(app.tabs[app.active].pending, draft);
    assert_eq!(app.tabs[app.active].query.text, "SELECT 1");
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.workbench.overlay.as_deref(), Some("history"));
    assert!(requests.try_recv().is_err());
}

#[test]
fn redis_history_preserves_input_until_task_finishes() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _, _) = archery_task_app(datapier_core::model::ProviderKind::Redis);
    app.workbench.overlay = Some("history".into());
    app.workbench.archery_history.items = vec![json!({"sqllog":"INFO"})];
    app.workbench.archery_history.selected = Some(0);
    app.application_action("history-open", &backend);
    assert_eq!(app.field("cli_text"), "GET 'kept key'");
    assert_eq!(app.workbench.overlay.as_deref(), Some("history"));
    app.task = None;
    app.workbench.cli_running = false;
    app.application_action("history-open", &backend);
    assert_eq!(app.field("cli_text"), "INFO");
    assert!(app.workbench.cli);
    assert!(app.workbench.overlay.is_none());
    assert!(requests.try_recv().is_err());
}
