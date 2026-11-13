use super::*;

#[test]
fn closing_connection_preserves_profile_and_rejects_pending_drafts() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.tabs[0].pending = json!({"edit":"draft"});
    app.navigation_action(&format!("profile-close:{}", profile.id), &backend);
    assert!(requests.try_recv().is_err());
    app.tabs[0].pending = Value::Null;
    app.navigation_action(&format!("profile-close:{}", profile.id), &backend);
    let close = requests.try_recv().unwrap();
    assert_eq!(close.method, "connection.close");
    app.tabs_close_response(
        &Response {
            tag: close.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(app.tabs.is_empty());
    assert!(app.session.is_none());
    assert_eq!(app.profiles[0].id, profile.id);
    assert!(requests.try_recv().is_err());
}

#[test]
fn deleting_profile_from_empty_workspace_uses_real_action_route() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.tabs.clear();
    app.session = None;
    app.action(&format!("profile-delete-request:{}", profile.id), &backend);
    let token = app.navigation_delete_confirmation().unwrap().token;
    app.action(&confirm_action(token), &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "profile.delete");
    app.navigation_response(
        &Response {
            tag: request.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(app.profiles.is_empty());
    assert!(app.tabs.is_empty());
    assert!(app.workbench.overlay.is_none());
}

#[test]
fn selecting_disconnected_profile_opens_a_new_tab() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, first) = mysql_drop_app();
    let mut second = first.clone();
    second.id = Uuid::new_v4();
    app.profiles.push(second.clone());
    let original = app.tabs[0].id;
    app.select_profile(1, &backend);
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.tabs[0].id, original);
    assert_eq!(app.tabs[0].profile_id, Some(first.id));
    assert_eq!(app.tabs[app.active].profile_id, Some(second.id));
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "connection.open");
    assert_eq!(request.params["profile_id"], json!(second.id));
}

#[test]
fn closing_profile_removes_only_its_tabs() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.create_query_tab();
    app.session = Some(json!({"session_id":Uuid::new_v4(),"profile":profile}));
    app.create_query_tab();
    let mut other = profile.clone();
    other.id = Uuid::new_v4();
    app.profiles.push(other.clone());
    app.profile = Some(other.clone());
    app.tabs[app.active].profile_id = Some(other.id);
    let other_session = Uuid::new_v4();
    app.session = Some(json!({"session_id":other_session,"profile":other}));
    app.action(&format!("profile-close:{}", profile.id), &backend);
    for _ in 0..2 {
        let request = requests.try_recv().unwrap();
        assert_eq!(request.method, "connection.close");
        app.tabs_close_response(
            &Response {
                tag: request.tag,
                result: Ok(json!({})),
            },
            &backend,
        );
    }
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.tabs[0].profile_id, Some(other.id));
    assert_eq!(app.session_id(), Some(json!(other_session)));
    assert!(app.profiles.iter().any(|p| p.id == profile.id));
}

#[test]
fn deletion_closes_all_profile_tabs_before_removing_configuration() {
    for provider in [
        datapier_core::model::ProviderKind::Mysql,
        datapier_core::model::ProviderKind::Redis,
    ] {
        let (backend, mut requests, _) = Backend::scripted();
        let (mut app, mut profile, _) = redis_app();
        profile.provider = provider;
        app.profile = Some(profile.clone());
        app.profiles[0] = profile.clone();
        app.create_query_tab();
        app.session = Some(json!({"session_id":Uuid::new_v4(),"profile":profile}));
        app.create_query_tab();
        let mut other = profile.clone();
        other.id = Uuid::new_v4();
        app.profiles.push(other.clone());
        app.profile = Some(other.clone());
        app.tabs[app.active].profile_id = Some(other.id);
        let other_session = Uuid::new_v4();
        app.session = Some(json!({"session_id":other_session,"profile":other}));
        app.action(&format!("profile-delete-request:{}", profile.id), &backend);
        let token = app.navigation_delete_confirmation().unwrap().token;
        app.action(&confirm_action(token), &backend);
        for _ in 0..2 {
            let close = requests.try_recv().unwrap();
            assert_eq!(close.method, "connection.close");
            assert_ne!(close.params["session_id"], json!(other_session));
            app.tabs_close_response(
                &Response {
                    tag: close.tag,
                    result: Ok(json!({})),
                },
                &backend,
            );
        }
        assert_eq!(app.tabs.len(), 1);
        assert_eq!(app.tabs[0].profile_id, Some(other.id));
        let delete = requests.try_recv().unwrap();
        assert_eq!(delete.method, "profile.delete");
        app.navigation_response(
            &Response {
                tag: delete.tag,
                result: Err(AppError::new("busy", "other window")),
            },
            &backend,
        );
        assert!(app.profiles.iter().any(|p| p.id == profile.id));
        assert_eq!(app.session_id(), Some(json!(other_session)));
        assert!(!app.navigation_delete_confirmation().unwrap().busy);
    }
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
    app.tabs_close_response(
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
fn deletion_checks_background_tab_and_originating_tab() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, _) = redis_app();
    app.action("new-tab", &backend);
    app.navigation_action(&format!("profile-delete-request:{}", profile.id), &backend);
    let token = app.navigation_delete_confirmation().unwrap().token;
    app.tabs[0].pending = json!({"row":"draft"});
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
    app.tabs_close_response(
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
    app.tabs_close_response(
        &Response {
            tag: close.tag,
            result: Err(AppError::new("not_found", "session already closed")),
        },
        &backend,
    );
    assert_eq!(app.tabs.len(), 1);
    let close = requests.try_recv().unwrap();
    assert_eq!(close.params["session_id"], json!(first));
    app.tabs_close_response(
        &Response {
            tag: close.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(app.tabs.is_empty());
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

#[test]
fn drafts_appearing_during_close_are_kept_and_abort_profile_deletion() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, _) = redis_app();
    app.navigation_action(&format!("profile-delete-request:{}", profile.id), &backend);
    let token = app.navigation_delete_confirmation().unwrap().token;
    app.navigation_action(&confirm_action(token), &backend);
    let close = requests.try_recv().unwrap();
    app.tabs[0].pending = json!({"row":"new draft"});
    app.tabs_close_response(
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
fn advanced_work_blocks_profile_deletion_in_active_and_background_tabs() {
    for background in [false, true] {
        for kind in ["schema", "schema_draft", "value", "replace"] {
            let (backend, mut requests, _) = Backend::scripted();
            let (mut app, profile, _) = redis_app();
            let target = ResourceTarget {
                database: Some("db".into()),
                name: "v".into(),
                key_b64: None,
                kind: "view".into(),
            };
            match kind {
                "schema" | "schema_draft" => {
                    let mut state = crate::advanced::schema::SchemaEditor::new(
                        json!("session"),
                        target,
                        "1".into(),
                        None,
                    );
                    state.dirty = true;
                    if kind == "schema_draft" {
                        app.tabs[0].schema_draft = state.workspace_draft();
                    } else {
                        app.advanced_schema = Some(state);
                    }
                }
                "value" => {
                    app.advanced_value.session = Some("value-session".into());
                    app.advanced_value.editor.text = "待提交😀".into();
                    app.advanced_value.applied = true;
                    app.workbench.overlay = Some("advanced-value".into());
                }
                _ => app.advanced_replace.open(
                    json!("session"),
                    "workspace".into(),
                    crate::advanced::replace::ReplaceTarget::Redis {
                        targets: vec![json!({"name":"key"})],
                    },
                ),
            }
            if background {
                app.create_query_tab();
            }
            let overlay = app.workbench.overlay.clone();
            app.navigation_action(&format!("profile-delete-request:{}", profile.id), &backend);
            assert!(
                app.navigation_delete_confirmation().is_none(),
                "{kind}, background={background}"
            );
            assert_eq!(app.workbench.overlay, overlay);
            assert!(requests.try_recv().is_err());
            if kind == "value" {
                let value = if background {
                    &app.tabs[0].runtime.advanced_value
                } else {
                    &app.advanced_value
                };
                assert_eq!(value.session.as_deref(), Some("value-session"));
                assert_eq!(value.editor.text, "待提交😀");
            }
        }
    }
}

#[test]
fn replace_pending_blocks_all_overlay_close_actions() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _, _) = redis_app();
    app.advanced_replace.open(
        json!("session"),
        "workspace".into(),
        crate::advanced::replace::ReplaceTarget::Redis {
            targets: vec![json!({"name":"key"})],
        },
    );
    app.advanced_replace.roles.push("value".into());
    let route = format!("{}:{}", app.tabs[0].id, app.generation);
    app.advanced_replace
        .action("replace:prepare", &backend, &route);
    assert_eq!(requests.try_recv().unwrap().method, "replace.prepare");
    app.workbench.overlay = Some("replace".into());
    assert!(app.advanced_replace.busy());
    app.action("overlay-close", &backend);
    assert_eq!(app.workbench.overlay.as_deref(), Some("replace"));
    assert!(app.workbench_key(&tern_sdk::Key::named("escape"), &backend));
    assert_eq!(app.workbench.overlay.as_deref(), Some("replace"));
    assert!(app.advanced_replace.busy());
}
