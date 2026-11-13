use super::*;

#[test]
fn drop_requires_live_context_write_access_and_clean_background_tabs() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.profiles[0].access_mode = datapier_core::model::AccessMode::ReadOnly;
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    assert!(app.navigation_drop_confirmation().is_none());
    app.profiles[0] = profile.clone();
    app.action("new-tab", &backend);
    app.tabs[0].pending = json!({"draft":true});
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    assert!(app.navigation_drop_confirmation().is_none());
    app.tabs[0].pending = Value::Null;
    app.session = Some(json!({"session_id":Uuid::new_v4(),"profile":profile}));
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    let token = app.navigation_drop_confirmation().unwrap().token;
    assert!(requests.try_recv().is_err());
    app.navigation_action(&format!("resource-drop-cancel:{token}"), &backend);
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    assert!(requests.try_recv().is_err());
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    let token = app.navigation_drop_confirmation().unwrap().token;
    app.generation += 1;
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    assert!(requests.try_recv().is_err());
}

#[test]
fn drop_task_quotes_identifiers_and_handles_terminal_states() {
    for state in ["succeeded", "failed", "outcome_unknown", "wait_stopped"] {
        let (backend, mut requests, _) = Backend::scripted();
        let (mut app, profile) = mysql_drop_app();
        app.target = Some(ResourceTarget {
            database: Some("db`;x".into()),
            name: "t`; DROP TABLE y".into(),
            kind: "table".into(),
            key_b64: None,
        });
        app.tabs[0].query = Editor::new("preserve me");
        app.navigation_action("navigation-drop-table:db`;x:0", &backend);
        let token = app.navigation_drop_confirmation().unwrap().token;
        app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
        let open = requests.try_recv().unwrap();
        assert_eq!(open.method, "connection.open");
        let ddl_session = Uuid::new_v4();
        assert_ne!(app.session_id(), Some(json!(ddl_session)));
        app.navigation_response(
            &Response {
                tag: open.tag,
                result: Ok(json!({"session_id":ddl_session,"profile":profile})),
            },
            &backend,
        );
        let start = requests.try_recv().unwrap();
        assert_eq!(start.method, "execution.start");
        assert_eq!(start.params["session_id"], json!(ddl_session));
        assert_eq!(
            start.params["text"],
            "DROP TABLE `db``;x`.`t``; DROP TABLE y`"
        );
        assert_eq!(start.params["mysql_write"], true);
        assert!(app.operation_pending);
        app.navigation_response(
            &Response {
                tag: start.tag,
                result: Ok(json!({"task_id":Uuid::new_v4()})),
            },
            &backend,
        );
        let poll = requests.try_recv().unwrap();
        assert_eq!(poll.method, "execution.get");
        app.navigation_response(
            &Response {
                tag: poll.tag,
                result: Ok(json!({"state":state,"error":"detail"})),
            },
            &backend,
        );
        let close = requests.try_recv().unwrap();
        assert_eq!(close.method, "connection.close");
        assert_eq!(close.params["session_id"], json!(ddl_session));
        assert!(app.operation_pending);
        app.navigation_response(
            &Response {
                tag: close.tag,
                result: Ok(json!({})),
            },
            &backend,
        );
        assert!(!app.operation_pending);
        assert_eq!(app.tabs[0].query.text, "preserve me");
        if state == "failed" {
            assert_eq!(
                app.navigation.databases[&profile.id].nodes["db`;x"]
                    .resources
                    .len(),
                1
            );
            assert!(requests.try_recv().is_err());
        } else {
            assert!(
                app.navigation.databases[&profile.id].nodes["db`;x"]
                    .resources
                    .is_empty()
            );
            let refresh = requests.try_recv().unwrap();
            assert_eq!(refresh.method, "resource.list");
            assert_eq!(refresh.params["database"], "db`;x");
        }
    }
}
#[test]
fn dropping_database_invalidates_its_tree_and_refreshes_catalog() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    let token = app.navigation_drop_confirmation().unwrap().token;
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    let open = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: open.tag,
            result: Ok(json!({"session_id":Uuid::new_v4(),"profile":profile})),
        },
        &backend,
    );
    let start = requests.try_recv().unwrap();
    assert_eq!(start.params["text"], "DROP DATABASE `db``;x`");
    app.navigation_response(
        &Response {
            tag: start.tag,
            result: Ok(json!({"task_id":Uuid::new_v4()})),
        },
        &backend,
    );
    let poll = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: poll.tag,
            result: Ok(json!({"state":"succeeded"})),
        },
        &backend,
    );
    let close = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: close.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(
        !app.navigation.databases[&profile.id]
            .nodes
            .contains_key("db`;x")
    );
    let refresh = requests.try_recv().unwrap();
    assert_eq!(refresh.method, "resource.list");
    assert!(refresh.params["database"].is_null());
}
#[test]
fn successful_drop_invalidates_background_view_without_losing_new_draft() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.action("new-tab", &backend);
    app.target = Some(ResourceTarget {
        database: Some("db`;x".into()),
        name: "t`; DROP TABLE y".into(),
        kind: "table".into(),
        key_b64: None,
    });
    app.schema = Some(json!({"columns":[]}));
    app.switch_tab(0);
    app.navigation_action("navigation-drop-table:db`;x:0", &backend);
    let token = app.navigation_drop_confirmation().unwrap().token;
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    let open = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: open.tag,
            result: Ok(json!({"session_id":Uuid::new_v4(),"profile":profile})),
        },
        &backend,
    );
    let start = requests.try_recv().unwrap();
    app.tabs[1].pending = json!({"row":"preserve"});
    app.navigation_response(
        &Response {
            tag: start.tag,
            result: Ok(json!({"task_id":Uuid::new_v4()})),
        },
        &backend,
    );
    let poll = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: poll.tag,
            result: Ok(json!({"state":"succeeded"})),
        },
        &backend,
    );
    let close = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: close.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert_eq!(app.tabs[1].pending["row"], "preserve");
    assert!(app.tabs[1].runtime.schema.is_none());
}
#[test]
fn drop_stop_cancels_the_dedicated_task_and_waits_for_close() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    let original = app.session_id().unwrap();
    app.form_id = Some(Uuid::new_v4());
    app.field_set("password", "other-profile-secret");
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    let token = app.navigation_drop_confirmation().unwrap().token;
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.method, "connection.open");
    assert!(open.params["secret"].is_null());
    let isolated = Uuid::new_v4();
    app.navigation_response(
        &Response {
            tag: open.tag,
            result: Ok(json!({"session_id":isolated,"profile":profile})),
        },
        &backend,
    );
    let start = requests.try_recv().unwrap();
    assert_eq!(start.params["session_id"], json!(isolated));
    app.navigation_action(&format!("resource-drop-stop:{token}"), &backend);
    assert!(app.navigation_drop_confirmation().unwrap().cancel_requested);
    assert!(requests.try_recv().is_err());
    let task = Uuid::new_v4();
    app.navigation_response(
        &Response {
            tag: start.tag,
            result: Ok(json!({"task_id":task})),
        },
        &backend,
    );
    let cancel = requests.try_recv().unwrap();
    assert_eq!(cancel.method, "execution.cancel");
    assert_eq!(cancel.params["task_id"], json!(task));
    assert!(app.navigation_drop_confirmation().unwrap().task_started);
    let poll = requests.try_recv().unwrap();
    assert_eq!(poll.method, "execution.get");
    app.navigation_response(
        &Response {
            tag: poll.tag,
            result: Ok(json!({"state":"outcome_unknown"})),
        },
        &backend,
    );
    let close = requests.try_recv().unwrap();
    assert_eq!(close.method, "connection.close");
    assert_eq!(close.params["session_id"], json!(isolated));
    assert!(app.operation_pending);
    app.navigation_response(
        &Response {
            tag: close.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(!app.operation_pending);
    assert_eq!(app.session_id(), Some(original));
}

#[test]
fn stopping_before_open_closes_session_without_submitting_drop() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    let token = app.navigation_drop_confirmation().unwrap().token;
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    let open = requests.try_recv().unwrap();
    app.navigation_action(&format!("resource-drop-stop:{token}"), &backend);
    let isolated = Uuid::new_v4();
    app.navigation_response(
        &Response {
            tag: open.tag,
            result: Ok(json!({"session_id":isolated,"profile":profile})),
        },
        &backend,
    );
    let close = requests.try_recv().unwrap();
    assert_eq!(close.method, "connection.close");
    app.navigation_response(
        &Response {
            tag: close.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(requests.try_recv().is_err());
}
#[test]
fn drop_session_close_failure_keeps_operation_pending_until_retry() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    let token = app.navigation_drop_confirmation().unwrap().token;
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    let open = requests.try_recv().unwrap();
    app.navigation_action(&format!("resource-drop-stop:{token}"), &backend);
    let isolated = Uuid::new_v4();
    app.navigation_response(
        &Response {
            tag: open.tag,
            result: Ok(json!({"session_id":isolated,"profile":profile})),
        },
        &backend,
    );
    let close = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: close.tag,
            result: Err(AppError::new("busy", "still closing")),
        },
        &backend,
    );
    assert!(app.operation_pending);
    app.navigation.drop_operation.as_mut().unwrap().last_poll = Instant::now();
    assert!(!app.navigation_poll_drop(&backend));
    app.navigation.drop_operation.as_mut().unwrap().last_poll =
        Instant::now() - Duration::from_secs(1);
    assert!(app.navigation_poll_drop(&backend));
    let retry = requests.try_recv().unwrap();
    assert_eq!(retry.method, "connection.close");
    assert_eq!(retry.params["session_id"], json!(isolated));
    app.navigation_response(
        &Response {
            tag: retry.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(!app.operation_pending);
}
#[test]
fn drop_rejects_changed_connection_identity_at_every_stage() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.profiles[0].direct_mut().unwrap().host = "different-server".into();
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    assert!(app.navigation_drop_confirmation().is_none());
    assert!(requests.try_recv().is_err());

    app.profiles[0] = profile.clone();
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    let token = app.navigation_drop_confirmation().unwrap().token;
    app.profiles[0].direct_mut().unwrap().port += 1;
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    assert!(requests.try_recv().is_err());

    app.profiles[0] = profile.clone();
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.method, "connection.open");
    app.profiles[0].direct_mut().unwrap().host = "changed-while-opening".into();
    let isolated = Uuid::new_v4();
    app.navigation_response(
        &Response {
            tag: open.tag,
            result: Ok(json!({"session_id":isolated,"profile":profile})),
        },
        &backend,
    );
    let close = requests.try_recv().unwrap();
    assert_eq!(close.method, "connection.close");
    assert_eq!(close.params["session_id"], json!(isolated));
    app.navigation_response(
        &Response {
            tag: close.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(requests.try_recv().is_err());
}

#[test]
fn drop_allows_non_connection_profile_metadata_changes() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _) = mysql_drop_app();
    app.profiles[0].name = "renamed".into();
    app.profiles[0].direct_mut().unwrap().visible_databases = Some(vec!["db`;x".into()]);
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    let token = app.navigation_drop_confirmation().unwrap().token;
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    assert_eq!(requests.try_recv().unwrap().method, "connection.open");
}

#[test]
fn drop_confirmation_rechecks_background_advanced_draft() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _) = mysql_drop_app();
    app.create_query_tab();
    app.session = Some(json!({"session_id":Uuid::new_v4(),"profile":app.profile}));
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    let token = app.navigation_drop_confirmation().unwrap().token;
    app.tabs[0].runtime.advanced_value.session = Some("value-session".into());
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    assert!(requests.try_recv().is_err());
    assert!(app.navigation_drop_confirmation().unwrap().error.is_some());
}
