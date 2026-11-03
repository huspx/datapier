use super::*;

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
