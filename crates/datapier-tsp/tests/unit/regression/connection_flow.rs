use super::*;

#[test]
fn provider_keyboard_preserves_form_and_refuses_changes_during_save() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("provider".into());
    app.profile_form(None);
    app.field_set("name", "保留名称");
    app.field_set("port", "3307");
    app.workbench_action("field:provider:mysql", &backend);
    assert_eq!(app.field("port"), "3307");
    app.focus = "provider-mysql".into();
    assert!(app.workbench_key(&tern_sdk::Key::named("right"), &backend));
    assert_eq!(app.field("provider"), "redis");
    assert_eq!(app.field("port"), "6379");
    assert_eq!(app.field("name"), "保留名称");
    app.reconcile_modal_focus();
    assert!(app.workbench_key(&tern_sdk::Key::named("left"), &backend));
    assert_eq!(app.field("provider"), "mysql");
    assert_eq!(app.field("port"), "3306");
    app.profile_saves.insert(app.form_id.unwrap());
    app.reconcile_modal_focus();
    assert!(app.workbench_key(&tern_sdk::Key::named("right"), &backend));
    assert_eq!(app.field("provider"), "mysql");
    assert_eq!(app.field("name"), "保留名称");
}

#[test]
fn connection_failure_can_retry_and_late_failure_stays_with_its_tab() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    let profile = ui_profile("mysql", "first");
    app.profile = Some(profile.clone());
    app.action("open", &backend);
    let first = requests.try_recv().unwrap();
    assert!(app.workbench.connecting);
    second_tab(&mut app);
    app.switch_tab(1);
    app.status = i18n::UiMessage::raw("active tab");
    app.routed_response(
        Response {
            tag: first.tag,
            result: Err(AppError::new("connection_failed", "offline")),
        },
        &backend,
    );
    assert!(app.workbench.connection_error.is_none());
    assert_eq!(app.status.render(), "active tab");
    app.switch_tab(0);
    assert!(!app.workbench.connecting);
    assert!(
        app.workbench
            .connection_error
            .as_deref()
            .unwrap()
            .contains("offline")
    );
    app.action("open", &backend);
    let retry = requests.try_recv().unwrap();
    assert_eq!(retry.method, "connection.open");
    assert!(app.workbench.connecting);
    app.routed_response(
        Response {
            tag: retry.tag,
            result: Ok(json!({"session_id": Uuid::new_v4(), "profile": profile})),
        },
        &backend,
    );
    assert!(!app.workbench.connecting);
    assert!(app.workbench.connection_error.is_none());
}

#[test]
fn connection_enqueue_failure_leaves_retry_available() {
    let (backend, requests, _) = Backend::scripted();
    drop(requests);
    let mut app = App::new("test".into());
    app.profile = Some(ui_profile("mysql", "first"));
    app.action("open", &backend);
    assert!(!app.workbench.connecting && !app.operation_pending);
    assert!(app.workbench.connection_error.is_some());
    let (backend, mut requests, _) = Backend::scripted();
    app.action("open", &backend);
    assert_eq!(requests.try_recv().unwrap().method, "connection.open");
    assert!(app.workbench.connecting);
}

#[test]
fn saving_another_profile_keeps_disconnected_resource_context_until_selection() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    let old = ui_profile("redis", "old");
    let mut next = ui_profile("mysql", "next");
    next.direct_mut().unwrap().database = Some("commerce".into());
    let target = ResourceTarget {
        database: Some("15".into()),
        name: "old:key".into(),
        key_b64: Some(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b"old:key",
        )),
        kind: "hash".into(),
    };
    app.profile = Some(old.clone());
    app.tabs[0].profile_id = Some(old.id);
    app.target = Some(target.clone());
    app.database = Some("15".into());
    app.profiles = vec![old.clone(), next.clone()];
    app.response(
        Response {
            tag: "profile.save".into(),
            result: Ok(json!({"profile": next})),
        },
        &backend,
    );
    assert_eq!(app.profile.as_ref().unwrap().id, old.id);
    assert_eq!(app.target.as_ref().unwrap().name, target.name);
    assert_eq!(app.database.as_deref(), Some("15"));

    app.select_profile(1, &backend);
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.profile.as_ref().unwrap().id, next.id);
    assert!(app.target.is_none());
    assert!(app.workbench.queued_target.is_none());
    let mut opened = None;
    while let Ok(request) = requests.try_recv() {
        if request.method == "connection.open" {
            opened = Some(request.params["profile_id"].clone());
        }
    }
    assert_eq!(opened, Some(json!(next.id)));
    app.switch_tab(0);
    assert_eq!(app.profile.as_ref().unwrap().id, old.id);
    assert_eq!(app.target.as_ref().unwrap().name, "old:key");
    assert_eq!(app.database.as_deref(), Some("15"));
}

#[test]
fn switching_disconnected_profile_preserves_sql_and_cli_drafts() {
    for provider in ["mysql", "redis"] {
        let (backend, _, _) = Backend::scripted();
        let mut app = App::new("restored-draft".into());
        let old = ui_profile(provider, "old");
        let next = ui_profile(
            if provider == "mysql" {
                "redis"
            } else {
                "mysql"
            },
            "next",
        );
        app.profile = Some(old.clone());
        app.tabs[0].profile_id = Some(old.id);
        let draft = if provider == "mysql" {
            "SELECT '草稿😀';"
        } else {
            "GET 草稿😀"
        };
        if provider == "mysql" {
            app.tabs[0].query = Editor::new(draft);
        } else {
            app.tabs[0].cli_query = Editor::new(draft);
        }
        app.profiles = vec![old.clone(), next.clone()];
        app.select_profile(1, &backend);
        assert_eq!(app.tabs[0].profile_id, Some(old.id));
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.profile.as_ref().unwrap().id, next.id);
        app.switch_tab(0);
        assert_eq!(app.profile.as_ref().unwrap().id, old.id);
        assert_eq!(
            if provider == "mysql" {
                &app.tabs[0].query.text
            } else {
                &app.tabs[0].cli_query.text
            },
            draft
        );
    }
}

#[test]
fn profile_error_preserves_fields_for_retry() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.action("new-profile", &backend);
    app.field_set("name", "saved name");
    app.action("test-profile", &backend);
    let request = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: request.tag,
            result: Err(AppError::new("connection_failed", "offline")),
        },
        &backend,
    );
    assert!(app.workbench.profile_error.is_some());
    assert_eq!(app.field("name"), "saved name");
    app.action("test-profile", &backend);
    let retry = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: retry.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(app.workbench.profile_error.is_none());
}

#[test]
fn profile_form_identity_is_stable_and_independent_of_selected_connection() {
    let mut app = App::new("test".into());
    app.profile_form(None);
    let selected = app.form_profile().unwrap();
    app.profile = Some(selected.clone());

    app.profile_form(Some(selected.clone()));
    assert_eq!(app.form_profile().unwrap().id, selected.id);
    app.field_set("name", "edited");
    assert_eq!(app.form_profile().unwrap().id, selected.id);

    app.profile_form(None);
    let new_id = app.form_profile().unwrap().id;
    assert_ne!(new_id, selected.id);
    app.field_set("name", "new");
    assert_eq!(app.form_profile().unwrap().id, new_id);
    assert_eq!(app.profile.as_ref().unwrap().id, selected.id);
}

#[test]
fn saving_new_profile_keeps_existing_session_associated_with_its_profile() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.profile_form(None);
    app.field_set("name", "connection A");
    let profile_a = app.form_profile().unwrap();
    let session = json!({"session_id": Uuid::new_v4()});
    app.profile = Some(profile_a.clone());
    app.tabs[0].profile_id = Some(profile_a.id);
    app.session = Some(session.clone());

    app.action("new-profile", &backend);
    app.field_set("name", "connection B");
    app.action("save-profile", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "profile.save");
    let profile_b: Profile = serde_json::from_value(request.params["profile"].clone()).unwrap();
    assert_ne!(profile_a.id, profile_b.id);
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"profile": profile_b})),
        },
        &backend,
    );

    assert_eq!(app.profile.as_ref().unwrap().id, profile_a.id);
    assert_eq!(app.tabs[0].profile_id, Some(profile_a.id));
    assert_eq!(app.session, Some(session));
}

#[test]
fn profile_save_uses_the_profile_id_returned_by_the_backend() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.action("new-profile", &backend);
    app.field_set("name", "connection");
    app.action("save-profile", &backend);
    let request = requests.try_recv().unwrap();
    let mut returned: Profile = serde_json::from_value(request.params["profile"].clone()).unwrap();
    returned.id = Uuid::new_v4();
    let returned_id = returned.id;
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"profile": returned})),
        },
        &backend,
    );
    assert_eq!(app.profile.as_ref().unwrap().id, returned_id);
    assert_eq!(app.tabs[0].profile_id, Some(returned_id));
}

#[test]
fn late_connection_reply_does_not_replace_a_newly_selected_connection() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("test".into());
    let first = ui_profile("mysql", "first");
    let second = ui_profile("redis", "second");
    app.profiles = vec![first.clone(), second.clone()];
    app.profile = Some(first.clone());
    app.tabs[0].profile_id = Some(first.id);
    app.operation_pending = true;
    let first_tab = app.tabs[0].id;
    app.select_profile(1, &backend);
    assert_eq!(app.tabs.len(), 2);
    app.routed_response(
        Response {
            tag: format!("{first_tab}:0:connection.open"),
            result: Ok(json!({"session_id":Uuid::new_v4(),"profile":first})),
        },
        &backend,
    );
    assert_eq!(app.active, 1);
    assert_eq!(app.profile.as_ref().unwrap().id, second.id);
    assert!(app.tabs[0].runtime.session.is_some());
}

#[test]
fn restored_tab_focus_follows_profile_arrival_order() {
    let (backend, _, _) = Backend::scripted();
    for profiles_first in [false, true] {
        let profile = ui_profile("redis", "redis");
        let mut saved = App::new("saved".into());
        saved.profile = Some(profile.clone());
        saved.tabs[0].profile_id = Some(profile.id);
        saved.tabs[0].cli_query = Editor::new("GET");
        saved.workbench.cli = true;
        let state = serde_json::to_value(saved.workspace_snapshot()).unwrap();
        let mut app = App::new("restored".into());
        let profiles = || Response {
            tag: "profile.list".into(),
            result: Ok(json!({"profiles":[profile.clone()]})),
        };
        if profiles_first {
            app.routed_response(profiles(), &backend);
        }
        app.routed_response(
            Response {
                tag: "workspace.load".into(),
                result: Ok(json!({"revision":1,"state":state})),
            },
            &backend,
        );
        if !profiles_first {
            app.routed_response(profiles(), &backend);
        }
        assert_eq!(app.focus, "cli_text");
        let mut key = tern_sdk::Key::named("x");
        key.text = Some(" x".into());
        app.input_key(&key);
        assert_eq!(app.tabs[0].cli_query.text, "GET x");
        assert!(app.tabs[0].query.text.is_empty());
    }

    let mut saved = App::new("saved".into());
    saved.target = Some(ResourceTarget {
        database: Some("db".into()),
        name: "t".into(),
        key_b64: None,
        kind: "table".into(),
    });
    let state = serde_json::to_value(saved.workspace_snapshot()).unwrap();
    let mut app = App::new("restored".into());
    app.routed_response(
        Response {
            tag: "workspace.load".into(),
            result: Ok(json!({"revision":1,"state":state})),
        },
        &backend,
    );
    assert_eq!(app.focus, "grid");

    let mut interacting = App::new("interacting".into());
    interacting.focus = "pattern".into();
    interacting.workspace_interacted = true;
    let state = serde_json::to_value(saved.workspace_snapshot()).unwrap();
    interacting.routed_response(
        Response {
            tag: "workspace.load".into(),
            result: Ok(json!({"revision":1,"state":state})),
        },
        &backend,
    );
    assert_eq!(interacting.focus, "pattern");
}

#[test]
fn tls_recovery_does_not_reopen_settings_after_a_transport_failure() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("tls-recovery".into());
    app.profile = Some(ui_profile("mysql", "secure"));
    app.action("open", &backend);
    let open = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: open.tag,
            result: Err(AppError::new("tls_required", "3159")),
        },
        &backend,
    );
    app.action("connection-enable-tls", &backend);
    assert_eq!(app.workbench.overlay.as_deref(), Some("profile"));
    assert_eq!(app.field("tls"), "true");
    app.action("overlay-close", &backend);
    assert!(app.workbench.overlay_close_confirm);
    app.action("overlay-close-discard", &backend);
    drop(requests);
    app.action("open", &backend);
    assert!(app.workbench.connection_error.is_some());
    app.action("connection-enable-tls", &backend);
    assert!(app.workbench.overlay.is_none());
}

#[test]
fn escape_dismisses_redis_db_picker_before_its_connection_form() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("db-picker-escape".into());
    let escape = tern_sdk::Key::named("escape");
    app.navigation.redis_db_menu = true;
    assert!(app.workbench_key(&escape, &backend));
    assert!(!app.navigation.redis_db_menu);
    app.workbench.overlay = Some("profile".into());
    app.navigation.form_redis_db_menu = true;
    assert!(app.workbench_key(&escape, &backend));
    assert!(!app.navigation.form_redis_db_menu);
    assert_eq!(app.workbench.overlay.as_deref(), Some("profile"));
    assert!(app.workbench_key(&escape, &backend));
    assert!(app.workbench.overlay.is_none());
}
