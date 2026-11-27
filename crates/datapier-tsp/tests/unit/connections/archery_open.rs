use super::*;

fn archery_open_for_auth_test(backend: &Backend) -> (App, Value, String) {
    let mut app = App::new("archery-open-auth".into());
    let server = Uuid::new_v4();
    let profile: Profile = serde_json::from_value(json!({"id":Uuid::new_v4(),"name":"fixture","provider":"mysql","access_mode":"read_only","connection":{"kind":"archery","server_id":server,"instance_id":7,"instance_name":"fixture","databases":["db_a","db_b"],"default_database":"db_a"}})).unwrap();
    app.profile = Some(profile.clone());
    app.profiles.push(profile.clone());
    app.tabs[0].profile_id = Some(profile.id);
    app.database = Some("db_b".into());
    app.action("open", backend);
    let params = json!({"profile_id":profile.id,"database":"db_b","redis_db":null,"secret":null});
    let tag = format!("{}:{}:connection.open", app.tabs[0].id, app.generation);
    (app, params, tag)
}

fn start_saved_auth(app: &mut App, backend: &Backend, list_tag: String) {
    let server_id = app.profile.as_ref().unwrap().archery().unwrap().server_id;
    app.routed_response(
        Response {
            tag: list_tag,
            result: Ok(json!({"servers":[{"id":server_id,"name":"fixture","base_url":"https://example.test/","username":"fixture","http_tls":{},"revision":1,"credential_mode":"keyring","totp_mode":"manual"}]})),
        },
        backend,
    );
}

fn archery_ready_response(app: &mut App, backend: &Backend, auth_id: &str) -> String {
    let operation = Uuid::new_v4();
    app.archery.pending = Some((operation, "archery.auth.begin".into()));
    let tag = format!(
        "archery-ui/{}/{}/archery.auth.begin",
        app.archery.epoch, operation
    );
    app.archery_response(
        Response {
            tag: tag.clone(),
            result: Ok(json!({"auth_id":auth_id,"state":"ready","auth_generation":1})),
        },
        backend,
    );
    tag
}

#[test]
fn first_archery_open_resumes_once_and_release_does_not_replace_status() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, params, tag) = archery_open_for_auth_test(&backend);
    let original = requests.try_recv().unwrap();
    assert_eq!(original.params, params);
    app.routed_response(
        Response {
            tag,
            result: Err(AppError::new("auth_required", "login")),
        },
        &backend,
    );
    let list = requests.try_recv().unwrap();
    assert_eq!(list.method, "archery.server.list");
    start_saved_auth(&mut app, &backend, list.tag);
    let login = requests.try_recv().unwrap();
    assert_eq!(login.method, "archery.auth.begin");
    assert!(login.params["password"].is_null());
    assert_eq!(login.params["candidate"], false);
    assert!(login.params["operation_id"].is_string());
    assert!(app.workbench.connection_error.is_some());
    archery_ready_response(&mut app, &backend, "auth-id");
    let release = requests.try_recv().unwrap();
    assert_eq!(release.method, "archery.auth.close");
    let resumed = requests.try_recv().unwrap();
    assert_eq!(resumed.method, "connection.open");
    assert_eq!(resumed.params, params);
    assert!(app.workbench.connection_error.is_none());
    let status = app.status.render().into_owned();
    let detail = app.detail.clone();
    app.routed_response(
        Response {
            tag: release.tag,
            result: Ok(json!({"closed":true})),
        },
        &backend,
    );
    assert_eq!(app.status.render().as_ref(), status);
    assert_eq!(app.detail, detail);
    assert!(requests.try_recv().is_err());
    app.routed_response(
        Response {
            tag: resumed.tag,
            result: Err(AppError::new("auth_required", "expired again")),
        },
        &backend,
    );
    assert_eq!(requests.try_recv().unwrap().method, "archery.server.list");
    archery_ready_response(&mut app, &backend, "second-auth");
    assert!(app.workbench.connection_error.is_none());
    assert!(
        std::iter::from_fn(|| requests.try_recv().ok())
            .all(|request| request.method != "connection.open")
    );
}

#[test]
fn cancelled_or_changed_target_never_resumes_archery_open() {
    for cancel in [true, false] {
        let (backend, mut requests, _) = Backend::scripted();
        let (mut app, _, tag) = archery_open_for_auth_test(&backend);
        requests.try_recv().unwrap();
        app.routed_response(
            Response {
                tag,
                result: Err(AppError::new("auth_required", "login")),
            },
            &backend,
        );
        let list = requests.try_recv().unwrap();
        start_saved_auth(&mut app, &backend, list.tag);
        let login = requests.try_recv().unwrap();
        if cancel {
            let late = login.tag;
            app.archery_action("archery-auth-back", &backend);
            assert!(app.archery.open_continuation.is_none());
            app.archery_response(
                Response {
                    tag: late,
                    result: Ok(json!({"auth_id":"late","state":"ready","auth_generation":1})),
                },
                &backend,
            );
        } else {
            app.database = Some("db_a".into());
            archery_ready_response(&mut app, &backend, "auth-id");
        }
        assert!(
            std::iter::from_fn(|| requests.try_recv().ok())
                .all(|request| request.method != "connection.open")
        );
    }
}

#[test]
fn background_direct_open_cannot_steal_archery_continuation() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, direct, _) = connected_app();
    app.session = None;
    app.action("open", &backend);
    let first = requests.try_recv().unwrap();
    app.create_query_tab();
    let server = Uuid::new_v4();
    let archery: Profile = serde_json::from_value(json!({"id":Uuid::new_v4(),"name":"archery","provider":"mysql","access_mode":"read_only","connection":{"kind":"archery","server_id":server,"instance_id":7,"instance_name":"fixture","databases":["db_a"],"default_database":"db_a"}})).unwrap();
    app.profile = Some(archery.clone());
    app.profiles.push(archery.clone());
    app.tabs[app.active].profile_id = Some(archery.id);
    app.action("open", &backend);
    let second = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: first.tag,
            result: Ok(json!({"profile":direct,"session_id":Uuid::new_v4()})),
        },
        &backend,
    );
    while requests.try_recv().is_ok() {}
    app.routed_response(
        Response {
            tag: second.tag,
            result: Err(AppError::new("auth_required", "login")),
        },
        &backend,
    );
    let list = requests.try_recv().unwrap();
    assert_eq!(list.method, "archery.server.list");
    start_saved_auth(&mut app, &backend, list.tag);
    assert_eq!(requests.try_recv().unwrap().method, "archery.auth.begin");
    archery_ready_response(&mut app, &backend, "auth-id");
    let requests: Vec<_> = std::iter::from_fn(|| requests.try_recv().ok()).collect();
    let opened: Vec<_> = requests
        .iter()
        .filter(|request| request.method == "connection.open")
        .collect();
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].params["profile_id"], json!(archery.id));
}

#[test]
fn cold_login_errors_remain_visible_and_manual_login_can_continue() {
    for code in [
        "auth_required",
        "credentials",
        "credentials_unavailable",
        "auth_failed",
        "transport",
    ] {
        let (backend, mut requests, _) = Backend::scripted();
        let (mut app, _, tag) = archery_open_for_auth_test(&backend);
        requests.try_recv().unwrap();
        app.routed_response(
            Response {
                tag,
                result: Err(AppError::new("auth_required", "login")),
            },
            &backend,
        );
        let list = requests.try_recv().unwrap();
        start_saved_auth(&mut app, &backend, list.tag);
        let login = requests.try_recv().unwrap();
        app.routed_response(
            Response {
                tag: login.tag,
                result: Err(AppError::new(code, "specific failure")),
            },
            &backend,
        );
        assert!(
            app.archery
                .error
                .as_deref()
                .unwrap()
                .contains("specific failure")
        );
        assert!(app.archery.auth_dialog);
        assert!(requests.try_recv().is_err());
        app.field_set("password", "replacement fixture password");
        app.archery_action("archery-login", &backend);
        assert_eq!(requests.try_recv().unwrap().method, "archery.auth.begin");
    }
}

#[test]
fn cold_login_challenge_only_shows_otp_and_resumes_after_verify() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, params, tag) = archery_open_for_auth_test(&backend);
    requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag,
            result: Err(AppError::new("auth_required", "login")),
        },
        &backend,
    );
    let list = requests.try_recv().unwrap();
    start_saved_auth(&mut app, &backend, list.tag);
    let login = requests.try_recv().unwrap();
    app.routed_response(Response { tag: login.tag, result: Ok(json!({"auth_id":"fixture","state":"challenge_required","controller_token":"token","auth_generation":1})) }, &backend);
    assert_eq!(app.archery.state, "challenge_required");
    assert_eq!(
        app.workbench.focus_request.as_deref(),
        Some("input-archery_otp")
    );
    app.field_set("archery_otp", "123456");
    app.archery_action("archery-verify", &backend);
    let verify = requests.try_recv().unwrap();
    assert_eq!(verify.method, "archery.auth.verify");
    app.routed_response(
        Response {
            tag: verify.tag,
            result: Ok(json!({"auth_id":"fixture","state":"ready","auth_generation":2})),
        },
        &backend,
    );
    assert_eq!(requests.try_recv().unwrap().method, "archery.auth.close");
    let resumed = requests.try_recv().unwrap();
    assert_eq!(resumed.method, "connection.open");
    assert_eq!(resumed.params, params);
    assert!(requests.try_recv().is_err());
}

#[test]
fn background_auth_required_does_not_open_a_dialog() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _, tag) = archery_open_for_auth_test(&backend);
    requests.try_recv().unwrap();
    app.create_query_tab();
    app.routed_response(
        Response {
            tag,
            result: Err(AppError::new("auth_required", "login")),
        },
        &backend,
    );
    assert!(!app.archery.open);
    assert!(requests.try_recv().is_err());
}
