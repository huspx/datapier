use super::*;

#[path = "connections/archery_open.rs"]
mod archery_open;
#[path = "connections/archery_wizard.rs"]
mod archery_wizard;

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
fn connection_edit_keeps_mcp_policy_and_creation_uses_global_default() {
    use datapier_core::model::ApprovalPolicy;

    let mut app = App::new("mcp-policy-precedence".into());
    app.profile_form(None);
    let mut existing = app.form_profile().unwrap();
    existing.approval_policy = ApprovalPolicy::Off;
    app.settings.approval.default_policy = ApprovalPolicy::AllWrites;

    app.profile_form(Some(existing));
    app.field_set("name", "renamed");
    app.field_set("host", "changed-host");
    assert_eq!(
        app.form_profile().unwrap().approval_policy,
        ApprovalPolicy::Off
    );

    app.profile_form(None);
    app.field_set("provider", "redis");
    assert_eq!(
        app.form_profile().unwrap().approval_policy,
        ApprovalPolicy::AllWrites
    );
}

#[test]
fn mysql_options_roundtrip_and_validate_without_losing_input() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("mysql-options".into());
    app.profile_form(None);
    assert!(!app.workbench.profile_timeout_details);
    app.field_set("name", "mysql");
    assert_eq!(
        app.form_profile()
            .unwrap()
            .direct()
            .unwrap()
            .mysql
            .timezone
            .as_deref(),
        Some("+00:00")
    );
    app.field_set("mysql_timezone", "");
    app.field_set("mysql_connect_timeout_seconds", "12");
    app.field_set("mysql_query_timeout_seconds", "86401");
    app.action("test-profile", &backend);
    assert_eq!(
        app.workbench.field_error.as_ref().unwrap().field,
        "mysql_query_timeout_seconds"
    );
    assert!(app.workbench.profile_timeout_details);
    assert_eq!(app.field("mysql_connect_timeout_seconds"), "12");
    assert!(requests.try_recv().is_err());
    app.field_set("mysql_query_timeout_seconds", "86400");
    let profile = app.form_profile().unwrap();
    let mysql = &profile.direct().unwrap().mysql;
    assert_eq!(mysql.timezone, None);
    assert_eq!(mysql.connect_timeout_seconds, Some(12));
    assert_eq!(mysql.query_timeout_seconds, Some(86400));
    app.profile_form(Some(profile));
    assert_eq!(app.field("mysql_timezone"), "");
    assert_eq!(app.field("mysql_connect_timeout_seconds"), "12");
    app.action("test-profile", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(
        request.params["profile"]["connection"]["mysql"]["timezone"],
        Value::Null
    );
    assert_eq!(
        request.params["profile"]["connection"]["mysql"]["query_timeout_seconds"],
        86400
    );
}

#[test]
fn inactive_redis_fields_do_not_block_mysql_submission() {
    for invalid in ["0", "abc", "4294967296"] {
        for action in ["save-profile", "test-profile", "profile-databases-load"] {
            let (backend, mut requests, _) = Backend::scripted();
            let mut app = App::new("inactive-redis".into());
            app.profile_form(None);
            app.field_set("name", "mysql");
            app.field_set("provider", "redis");
            app.field_set("redis_databases_override", invalid);
            app.field_set("provider", "mysql");
            app.action(action, &backend);
            assert!(requests.try_recv().is_ok(), "{action}: {invalid}");
            assert_eq!(app.field("redis_databases_override"), invalid);
            app.field_set("provider", "redis");
            assert!(!app.validate_profile_form());
            assert_eq!(
                app.workbench.field_error.as_ref().unwrap().field,
                "redis_databases_override"
            );
        }
    }
}

#[test]
fn saving_mysql_options_preserves_other_connection_settings() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("mysql-options-save".into());
    app.profile_form(None);
    app.field_set("name", "primary");
    app.field_set("mysql_timezone", "Asia/Shanghai");
    app.field_set("mysql_connect_timeout_seconds", "25");
    app.field_set("access", "read_write");
    app.field_set("tls", "true");
    app.action("save-profile", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "profile.save");
    assert_eq!(
        request.params["profile"]["connection"]["mysql"]["timezone"],
        "Asia/Shanghai"
    );
    assert_eq!(
        request.params["profile"]["connection"]["mysql"]["connect_timeout_seconds"],
        25
    );
    assert_eq!(
        request.params["profile"]["connection"]["mysql"]["query_timeout_seconds"],
        Value::Null
    );
    assert_eq!(
        request.params["profile"]["connection"]["tls"]["enabled"],
        true
    );
    assert_eq!(request.params["profile"]["access_mode"], "read_write");
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
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "profile.test");
    assert_eq!(request.params["profile"]["connection"]["kind"], "direct");
    assert_eq!(
        request.params["profile"]["connection"]["tls"]["client_cert_path"],
        "client.pem"
    );
    assert!(request.params["profile"].get("host").is_none());
}

#[test]
fn new_form_after_editing_resets_credentials_tls_scope_and_undo() {
    let (mut app, mut profile, session) = connected_app();
    profile.name = "previous".into();
    profile.provider = ProviderKind::Redis;
    let direct = profile.direct_mut().unwrap();
    direct.host = "previous-host".into();
    direct.port = 6379;
    direct.username = "previous-user".into();
    direct.database = Some("previous-db".into());
    direct.redis_db = 15;
    direct.visible_databases = Some(vec!["previous-db".into()]);
    direct.tls.enabled = true;
    direct.tls.ca_path = Some("previous-ca".into());
    direct.tls.client_cert_path = Some("previous-cert".into());
    direct.tls.client_key_path = Some("previous-key".into());
    profile.access_mode = AccessMode::ReadWrite;
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
    let direct = fresh.direct().unwrap();
    assert_eq!((direct.host.as_str(), direct.port), ("127.0.0.1", 3306));
    assert_eq!(fresh.access_mode, AccessMode::ReadOnly);
    assert!(
        fresh.name.is_empty() && direct.username.is_empty() && app.field("password").is_empty()
    );
    assert!(direct.database.is_none() && direct.visible_databases.is_none());
    assert_eq!(direct.redis_db, 0);
    assert!(!direct.tls.enabled);
    assert!(
        direct.tls.ca_path.is_none()
            && direct.tls.client_cert_path.is_none()
            && direct.tls.client_key_path.is_none()
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
    assert_eq!(
        app.profile.as_ref().unwrap().direct().unwrap().host,
        "updated-host"
    );
    assert_eq!(
        app.tabs[1]
            .runtime
            .profile
            .as_ref()
            .unwrap()
            .direct()
            .unwrap()
            .host,
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
    assert_eq!(
        app.profile.as_ref().unwrap().direct().unwrap().host,
        "127.0.0.1"
    );
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
