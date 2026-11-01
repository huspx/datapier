use super::*;

fn redis_app() -> (App, Profile, Uuid) {
    let mut app = App::new("test".into());
    app.profile_form(None);
    app.field_set("name", "redis");
    app.field_set("provider", "redis");
    let profile = app.form_profile().unwrap();
    app.workbench.overlay = None;
    let session = Uuid::new_v4();
    app.profiles.push(profile.clone());
    app.profile = Some(profile.clone());
    app.tabs[0].profile_id = Some(profile.id);
    app.session = Some(json!({"session_id": session, "profile": profile}));
    app.dirty = false;
    (app, profile, session)
}

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
    profile.redis_db = 3;
    let new_session = Uuid::new_v4();
    assert!(app.navigation_response(
        &Response {
            tag: request.tag,
            result: Ok(json!({"session_id": new_session, "profile": profile})),
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
    assert_eq!(app.profile.as_ref().unwrap().redis_db, 3);
    assert_eq!(app.session_id(), Some(json!(new_session)));
}

#[test]
fn rejected_redis_switch_keeps_original_database_and_session() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, old_session) = redis_app();
    app.tabs[0].browse.redis_db = Some(profile.redis_db);
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
fn confirm_action(token: Uuid) -> String {
    format!("{}{}:{}", "profile-delete-", "confirm", token)
}

#[test]
fn profile_deletion_requires_live_confirmation_and_retries_failure() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, current, _) = redis_app();
    let mut other = current.clone();
    other.id = Uuid::new_v4();
    other.name = "other".into();
    app.profiles.push(other.clone());
    app.navigation.databases.entry(current.id).or_default();
    app.navigation.databases.entry(other.id).or_default();
    app.navigation_action(&format!("profile-delete-request:{}", other.id), &backend);
    let first = app.navigation_delete_confirmation().unwrap().token;
    assert!(requests.try_recv().is_err());
    app.navigation_action(&format!("profile-delete-cancel:{first}"), &backend);
    app.navigation_action(&confirm_action(first), &backend);
    assert!(requests.try_recv().is_err());
    app.navigation_action(&format!("profile-delete-request:{}", other.id), &backend);
    let token = app.navigation_delete_confirmation().unwrap().token;
    app.navigation_action(&confirm_action(first), &backend);
    assert!(requests.try_recv().is_err());
    app.navigation_action(&confirm_action(token), &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "profile.delete");
    assert_eq!(request.params["profile_id"], json!(other.id));
    assert_eq!(app.profiles.len(), 2);
    app.navigation_response(
        &Response {
            tag: request.tag,
            result: Err(AppError::new("failed", "failed")),
        },
        &backend,
    );
    assert!(!app.navigation_delete_confirmation().unwrap().busy);
    assert!(
        app.navigation_delete_confirmation()
            .unwrap()
            .error
            .is_some()
    );
    app.navigation_action(&confirm_action(token), &backend);
    let retry = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: retry.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert_eq!(app.profiles.len(), 1);
    assert_eq!(app.profiles[0].id, current.id);
    assert!(app.navigation.databases.contains_key(&current.id));
    assert!(!app.navigation.databases.contains_key(&other.id));
    assert_eq!(app.profile.as_ref().unwrap().id, current.id);
    assert!(app.session.is_some());
}

#[test]
fn profile_deletion_rejects_closed_overlay_changed_profile_and_pending_work() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, _) = redis_app();
    app.navigation_action(&format!("profile-delete-request:{}", profile.id), &backend);
    let token = app.navigation_delete_confirmation().unwrap().token;
    app.workbench.overlay = None;
    app.navigation_action(&confirm_action(token), &backend);
    assert!(requests.try_recv().is_err());
    app.navigation_action(&format!("profile-delete-request:{}", profile.id), &backend);
    let token = app.navigation_delete_confirmation().unwrap().token;
    app.profiles[0].name = "changed".into();
    app.navigation_action(&confirm_action(token), &backend);
    assert!(requests.try_recv().is_err());
    app.profiles[0] = profile.clone();
    app.tabs[0].pending = json!({"edit":"draft"});
    app.navigation_action(&confirm_action(token), &backend);
    assert!(requests.try_recv().is_err());
    app.tabs[0].pending = Value::Null;
    app.task = Some("running".into());
    app.navigation_action(&confirm_action(token), &backend);
    assert!(requests.try_recv().is_err());
    app.task = None;
    app.navigation_action(&confirm_action(token), &backend);
    let close = requests.try_recv().unwrap();
    assert_eq!(close.method, "connection.close");
    assert!(app.navigation_profile_delete_pending(profile.id));
    app.navigation_response(
        &Response {
            tag: close.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(app.session.is_none());
    assert_eq!(requests.try_recv().unwrap().method, "profile.delete");
}

#[test]
fn database_refresh_replaces_only_the_selected_database_and_section_targets_table() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, mut profile, session) = redis_app();
    profile.provider = datapier_core::model::ProviderKind::Mysql;
    app.profile = Some(profile.clone());
    app.profiles[0] = profile.clone();
    let tree = app.navigation.databases.entry(profile.id).or_default();
    tree.nodes.entry("other".into()).or_default().resources =
        vec![serde_json::from_value(json!({"name":"untouched","kind":"table"})).unwrap()];
    tree.nodes.entry("commerce".into()).or_default();
    app.navigation_action("navigation-db-refresh:commerce", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.params["database"], "commerce");
    assert_eq!(request.params["session_id"], json!(session));
    app.navigation_response(&Response { tag: request.tag, result: Ok(json!({"resources":[{"name":"customers","kind":"table"}],"cursor":null,"complete":true})) }, &backend);
    assert_eq!(
        app.navigation.databases[&profile.id].nodes["other"].resources[0].name,
        "untouched"
    );
    app.navigation_action("navigation-resource-section:ddl:commerce:0", &backend);
    assert_eq!(app.target.as_ref().unwrap().name, "customers");
    assert_eq!(
        app.target.as_ref().unwrap().database.as_deref(),
        Some("commerce")
    );
    assert_eq!(app.workbench.section, "ddl");
    let describe = requests.try_recv().unwrap();
    assert_eq!(describe.method, "resource.describe");
    assert_eq!(describe.params["target"]["name"], "customers");
}
#[test]
fn deletion_checks_background_tab_and_originating_tab() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, _) = redis_app();
    app.action("new-tab", &backend);
    app.tabs[0].pending = json!({"row":"draft"});
    app.navigation_action(&format!("profile-delete-request:{}", profile.id), &backend);
    let token = app.navigation_delete_confirmation().unwrap().token;
    app.switch_tab(0);
    app.navigation_action(&confirm_action(token), &backend);
    assert!(requests.try_recv().is_err());
    app.switch_tab(1);
    app.navigation_action(&confirm_action(token), &backend);
    assert!(requests.try_recv().is_err());
    app.tabs[0].pending = Value::Null;
    app.tabs[0].runtime.task = Some("running".into());
    app.navigation_action(&confirm_action(token), &backend);
    assert!(requests.try_recv().is_err());
}

#[test]
fn queued_section_is_applied_only_after_the_matching_target_loads() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, mut profile, _) = redis_app();
    profile.provider = datapier_core::model::ProviderKind::Mysql;
    app.profile = Some(profile.clone());
    app.profiles[0] = profile.clone();
    app.tabs[0].query = Editor::new("select 1");
    app.navigation
        .databases
        .entry(profile.id)
        .or_default()
        .nodes
        .entry("shop".into())
        .or_default()
        .resources = vec![serde_json::from_value(json!({"name":"orders","kind":"table"})).unwrap()];
    app.navigation_action("navigation-resource-section:structure:shop:0", &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.method, "connection.open");
    app.routed_response(
        Response {
            tag: open.tag,
            result: Ok(json!({"session_id":Uuid::new_v4(),"profile":profile})),
        },
        &backend,
    );
    app.navigation_apply_queued_section();
    assert_eq!(app.target.as_ref().unwrap().name, "orders");
    assert_eq!(
        app.target.as_ref().unwrap().database.as_deref(),
        Some("shop")
    );
    assert_eq!(app.workbench.section, "structure");
    let mut described = false;
    while let Ok(request) = requests.try_recv() {
        if request.method == "resource.describe" {
            assert_eq!(request.params["target"]["name"], "orders");
            described = true;
        }
    }
    assert!(described);
}
#[test]
fn profile_delete_waits_for_all_closes_and_retries_failed_close() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, first) = redis_app();
    app.tabs[0].query = Editor::new("keep query");
    app.action("new-tab", &backend);
    let second = Uuid::new_v4();
    app.session = Some(json!({"session_id":second}));
    app.navigation_action(&format!("profile-delete-request:{}", profile.id), &backend);
    let token = app.navigation_delete_confirmation().unwrap().token;
    app.navigation_action(&confirm_action(token), &backend);
    let close = requests.try_recv().unwrap();
    assert_eq!(close.method, "connection.close");
    assert_eq!(close.params["session_id"], json!(second));
    app.navigation_response(
        &Response {
            tag: close.tag,
            result: Err(AppError::new("busy", "close failed")),
        },
        &backend,
    );
    assert!(requests.try_recv().is_err());
    assert!(!app.navigation_delete_confirmation().unwrap().busy);
    assert!(app.session.is_some());
    app.navigation_action(&confirm_action(token), &backend);
    let close = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: close.tag,
            result: Err(AppError::new("not_found", "session already closed")),
        },
        &backend,
    );
    assert!(app.session.is_none());
    let close = requests.try_recv().unwrap();
    assert_eq!(close.params["session_id"], json!(first));
    app.navigation_response(
        &Response {
            tag: close.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(app.tabs[0].runtime.session.is_none());
    assert_eq!(app.tabs[0].query.text, "keep query");
    let delete = requests.try_recv().unwrap();
    assert_eq!(delete.method, "profile.delete");
    app.navigation_response(
        &Response {
            tag: delete.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(app.profiles.is_empty());
    assert!(!app.navigation_profile_delete_pending(profile.id));
}

fn mysql_drop_app() -> (App, Profile) {
    let (mut app, mut profile, _) = redis_app();
    profile.provider = datapier_core::model::ProviderKind::Mysql;
    profile.access_mode = datapier_core::model::AccessMode::ReadWrite;
    app.profile = Some(profile.clone());
    app.profiles[0] = profile.clone();
    app.session.as_mut().unwrap()["profile"] = json!(profile);
    let tree = app.navigation.databases.entry(profile.id).or_default();
    tree.databases =
        vec![serde_json::from_value(json!({"name":"db`;x","kind":"database"})).unwrap()];
    tree.nodes.entry("db`;x".into()).or_default().resources =
        vec![serde_json::from_value(json!({"name":"t`; DROP TABLE y","kind":"table"})).unwrap()];
    (app, profile)
}

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
    for state in ["succeeded", "failed", "outcome_unknown"] {
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
fn drafts_appearing_during_close_are_kept_and_abort_profile_deletion() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, _) = redis_app();
    app.navigation_action(&format!("profile-delete-request:{}", profile.id), &backend);
    let token = app.navigation_delete_confirmation().unwrap().token;
    app.navigation_action(&confirm_action(token), &backend);
    let close = requests.try_recv().unwrap();
    app.tabs[0].pending = json!({"row":"new draft"});
    app.navigation_response(
        &Response {
            tag: close.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert_eq!(app.tabs[0].pending["row"], "new draft");
    assert!(app.session.is_none());
    assert!(requests.try_recv().is_err());
    assert!(!app.navigation_profile_delete_pending(profile.id));
    assert!(!app.navigation_delete_confirmation().unwrap().busy);
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
    app.profiles[0].host = "different-server".into();
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    assert!(app.navigation_drop_confirmation().is_none());
    assert!(requests.try_recv().is_err());

    app.profiles[0] = profile.clone();
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    let token = app.navigation_drop_confirmation().unwrap().token;
    app.profiles[0].port += 1;
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    assert!(requests.try_recv().is_err());

    app.profiles[0] = profile.clone();
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.method, "connection.open");
    app.profiles[0].host = "changed-while-opening".into();
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
    app.profiles[0].visible_databases = Some(vec!["db`;x".into()]);
    app.navigation_action("navigation-drop-db:db`;x", &backend);
    let token = app.navigation_drop_confirmation().unwrap().token;
    app.navigation_action(&format!("resource-drop-confirm:{token}"), &backend);
    assert_eq!(requests.try_recv().unwrap().method, "connection.open");
}
