use super::*;

fn connected_app() -> (App, Profile, Value) {
    let mut app = App::new("profile-reconnect-test".into());
    app.profile_form(None);
    app.field_set("name", "primary");
    let profile = app.form_profile().unwrap();
    app.profiles.push(profile.clone());
    app.profile = Some(profile.clone());
    app.tabs[0].profile_id = Some(profile.id);
    app.tabs[0].query = Editor::new("SELECT '中文😀'");
    let session = json!({"session_id":Uuid::new_v4()});
    app.session = Some(session.clone());
    app.workbench.overlay = None;
    (app, profile, session)
}
#[test]
fn all_profile_submissions_reject_invalid_fields_without_requests() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("validation".into());
    app.profile_form(None);
    for action in ["save-profile", "test-profile", "profile-databases-load"] {
        app.action(action, &backend);
        assert_eq!(app.workbench.field_error.as_ref().unwrap().field, "name");
        assert!(requests.try_recv().is_err());
    }
    app.field_set("name", "valid");
    app.field_set("port", "0");
    app.action("test-profile", &backend);
    assert_eq!(app.workbench.field_error.as_ref().unwrap().field, "port");
    app.field_set("port", "3306");
    app.field_set("tls", "true");
    app.field_set("cert", "client.pem");
    app.action("test-profile", &backend);
    assert_eq!(app.workbench.field_error.as_ref().unwrap().field, "key");
    assert_eq!(app.field("cert"), "client.pem");
    assert!(requests.try_recv().is_err());
    app.field_set("key", "client.key");
    app.action("test-profile", &backend);
    assert_eq!(requests.try_recv().unwrap().method, "profile.test");
}

#[test]
fn new_form_after_editing_resets_credentials_tls_scope_and_undo() {
    let (mut app, mut profile, session) = connected_app();
    profile.name = "previous".into();
    profile.provider = ProviderKind::Redis;
    profile.host = "previous-host".into();
    profile.port = 6379;
    profile.username = "previous-user".into();
    profile.database = Some("previous-db".into());
    profile.redis_db = 15;
    profile.visible_databases = Some(vec!["previous-db".into()]);
    profile.access_mode = AccessMode::ReadWrite;
    profile.tls.enabled = true;
    profile.tls.ca_path = Some("previous-ca".into());
    profile.tls.client_cert_path = Some("previous-cert".into());
    profile.tls.client_key_path = Some("previous-key".into());
    app.profile_form(Some(profile.clone()));
    app.field_set("password", "temporary-password");
    app.fields.get_mut("name").unwrap().edit(0..8, "changed");
    app.navigation.form.loaded = true;
    app.navigation.form.candidates = vec!["previous-db".into()];
    app.field_set("database_search", "previous");
    app.navigation.form_redis_db_menu = true;
    app.workbench.profile_error = Some("previous failure".into());
    app.profile_form(None);
    let fresh = app.form_profile().unwrap();
    assert_ne!(fresh.id, profile.id);
    assert_eq!(fresh.provider, ProviderKind::Mysql);
    assert_eq!((fresh.host.as_str(), fresh.port), ("127.0.0.1", 3306));
    assert_eq!(fresh.access_mode, AccessMode::ReadOnly);
    assert!(fresh.name.is_empty() && fresh.username.is_empty() && app.field("password").is_empty());
    assert!(fresh.database.is_none() && fresh.visible_databases.is_none());
    assert_eq!(fresh.redis_db, 0);
    assert!(!fresh.tls.enabled);
    assert!(
        fresh.tls.ca_path.is_none()
            && fresh.tls.client_cert_path.is_none()
            && fresh.tls.client_key_path.is_none()
    );
    assert!(!app.fields.get_mut("name").unwrap().undo());
    assert!(!app.navigation.form.loaded && app.navigation.form.candidates.is_empty());
    assert!(app.navigation.form.selected.is_empty() && app.field("database_search").is_empty());
    assert!(!app.navigation.form_redis_db_menu && !app.workbench.profile_tls_details);
    assert!(app.workbench.profile_error.is_none());
    assert_eq!(app.session, Some(session));
}

#[test]
fn editing_save_reconnects_its_tabs_and_preserves_other_profile_and_queries() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, first_session) = connected_app();
    let mut sibling = app.tabs[0].clone();
    sibling.id = Uuid::new_v4();
    sibling.query = Editor::new("SELECT 2");
    sibling.runtime.profile = Some(profile.clone());
    let second_session = json!({"session_id":Uuid::new_v4()});
    sibling.runtime.session = Some(second_session.clone());
    app.tabs.push(sibling);
    let mut other = profile.clone();
    other.id = Uuid::new_v4();
    other.name = "unrelated".into();
    app.profiles.push(other.clone());
    let mut unrelated = app.tabs[0].clone();
    unrelated.id = Uuid::new_v4();
    unrelated.profile_id = Some(other.id);
    unrelated.query = Editor::new("SELECT 'untouched'");
    unrelated.runtime.profile = Some(other);
    let unrelated_session = json!({"session_id":Uuid::new_v4()});
    unrelated.runtime.session = Some(unrelated_session.clone());
    app.tabs.push(unrelated);
    app.profile_form(Some(profile));
    app.field_set("host", "updated-host");
    app.action("save-profile", &backend);
    let save = requests.try_recv().unwrap();
    assert_eq!(save.method, "profile.save");
    let saved = save.params["profile"].clone();
    app.routed_response(
        Response {
            tag: save.tag,
            result: Ok(json!({"profile":saved})),
        },
        &backend,
    );
    let replies: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
    let closed: Vec<_> = replies
        .iter()
        .filter(|r| r.method == "connection.close")
        .map(|r| r.params["session_id"].clone())
        .collect();
    assert_eq!(
        closed,
        vec![
            first_session["session_id"].clone(),
            second_session["session_id"].clone()
        ]
    );
    let opened: Vec<_> = replies
        .iter()
        .filter(|r| r.method == "connection.open")
        .collect();
    assert_eq!(opened.len(), 2);
    assert!(
        opened
            .iter()
            .all(|r| r.params["profile_id"] == saved["id"] && r.params["secret"].is_null())
    );
    assert_eq!(app.active, 0);
    assert_eq!(app.profile.as_ref().unwrap().host, "updated-host");
    assert_eq!(
        app.tabs[1].runtime.profile.as_ref().unwrap().host,
        "updated-host"
    );
    assert_eq!(app.tabs[0].query.text, "SELECT '中文😀'");
    assert_eq!(app.tabs[1].query.text, "SELECT 2");
    assert_eq!(app.tabs[2].query.text, "SELECT 'untouched'");
    assert_eq!(app.tabs[2].runtime.session, Some(unrelated_session));
    assert!(app.workbench.overlay.is_none());
}

#[test]
fn editing_save_refuses_background_work_without_persisting_or_reconnecting() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, session) = connected_app();
    let mut sibling = app.tabs[0].clone();
    sibling.id = Uuid::new_v4();
    sibling.runtime.task = Some(Uuid::new_v4().to_string());
    app.tabs.push(sibling);
    app.profile_form(Some(profile));
    app.field_set("host", "new-host");
    app.action("save-profile", &backend);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.session, Some(session));
    assert_eq!(app.profile.as_ref().unwrap().host, "127.0.0.1");
    assert_eq!(app.field("host"), "new-host");
    assert_eq!(app.workbench.overlay.as_deref(), Some("profile"));
    assert!(app.workbench.profile_error.is_some());
    app.tabs[1].runtime.task = None;
    app.tabs[0].pending = json!({"restored":"draft"});
    app.action("save-profile", &backend);
    assert!(requests.try_recv().is_err());
}

#[test]
fn pending_save_keeps_form_stable_and_failure_retains_input() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, session) = connected_app();
    app.profile_form(Some(profile));
    app.field_set("host", "edited-host");
    let form = app.form_id;
    app.action("save-profile", &backend);
    let save = requests.try_recv().unwrap();
    app.action("save-profile", &backend);
    app.action("overlay-close", &backend);
    app.workbench_key(&tern_sdk::Key::named("escape"), &backend);
    app.profile_form(None);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.form_id, form);
    assert_eq!(app.workbench.overlay.as_deref(), Some("profile"));
    app.routed_response(
        Response {
            tag: save.tag,
            result: Err(AppError::new("credentials", "save rejected")),
        },
        &backend,
    );
    assert!(!app.connection_form_busy());
    assert_eq!(app.field("host"), "edited-host");
    assert_eq!(app.session, Some(session));
    assert!(
        app.workbench
            .profile_error
            .as_ref()
            .unwrap()
            .contains("save rejected")
    );
}

#[test]
fn editing_closes_owned_sessions_before_form_and_keeps_other_connection() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, _) = connected_app();
    let mut sibling = app.tabs[0].clone();
    sibling.id = Uuid::new_v4();
    sibling.runtime.session = Some(json!({"session_id":Uuid::new_v4()}));
    sibling.runtime.profile = Some(profile.clone());
    app.tabs.push(sibling);
    let mut other = app.tabs[0].clone();
    other.id = Uuid::new_v4();
    other.profile_id = Some(Uuid::new_v4());
    other.runtime.session = Some(json!({"session_id":Uuid::new_v4()}));
    let other_id = other.id;
    app.tabs.push(other);
    app.action(&format!("profile-settings:{}", profile.id), &backend);
    assert!(app.workbench.overlay.is_none());
    for _ in 0..2 {
        let close = requests.try_recv().unwrap();
        assert_eq!(close.method, "connection.close");
        app.routed_response(
            Response {
                tag: close.tag,
                result: Ok(json!({})),
            },
            &backend,
        );
    }
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.tabs[0].id, other_id);
    assert_eq!(app.workbench.overlay.as_deref(), Some("profile"));
    assert_eq!(app.form_id, Some(profile.id));
    assert!(!app.connection_form_has_tabs());
}

#[test]
fn reconnect_invalidates_old_baselines_in_both_access_directions() {
    for access in [AccessMode::ReadOnly, AccessMode::ReadWrite] {
        let (backend, mut requests, _) = Backend::scripted();
        let (mut app, mut profile, _) = connected_app();
        profile.access_mode = if access == AccessMode::ReadOnly {
            AccessMode::ReadWrite
        } else {
            AccessMode::ReadOnly
        };
        app.profile = Some(profile.clone());
        app.edit_token = Some(json!("old-active-token"));
        app.schema = Some(json!({"keys":[["id"]]}));
        let mut sibling = app.tabs[0].clone();
        sibling.id = Uuid::new_v4();
        sibling.runtime.profile = Some(profile.clone());
        sibling.runtime.session = Some(json!({"session_id":Uuid::new_v4()}));
        sibling.runtime.edit_token = Some(json!("old-background-token"));
        sibling.runtime.schema = Some(json!({"keys":[["id"]]}));
        app.tabs.push(sibling);
        profile.access_mode = access;
        app.form_editing = true;
        app.profile_saved(json!({"profile":profile}), &backend);
        assert!(app.edit_token.is_none() && app.schema.is_none());
        assert!(app.tabs[1].runtime.edit_token.is_none() && app.tabs[1].runtime.schema.is_none());
        assert_eq!(app.profile.as_ref().unwrap().access_mode, access);
        assert_eq!(
            app.tabs[1].runtime.profile.as_ref().unwrap().access_mode,
            access
        );
        let calls: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
        assert_eq!(
            calls
                .iter()
                .filter(|r| r.method == "connection.open")
                .count(),
            2
        );
        assert_eq!(app.tabs[0].query.text, "SELECT '中文😀'");
    }
}
