use super::*;

#[test]
fn archery_preview_focuses_first_invalid_name_in_display_order_and_preserves_selection() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("archery-name-validation".into());
    app.archery.open = true;
    app.workbench.overlay = Some("profile".into());
    app.archery.server = Some(json!({"id":Uuid::new_v4(),"revision":1}));
    app.archery.instances = vec![
        json!({"id":2,"name":"second","provider":"mysql"}),
        json!({"id":1,"name":"first","provider":"mysql"}),
        json!({"id":3,"name":"missing","provider":"mysql"}),
    ];
    for index in 0..3 {
        app.action(&format!("archery-instance:{index}"), &backend);
    }
    app.archery.instances.pop();
    for selection in app.archery.selected.values_mut() {
        selection.databases = Some(vec!["shop".into(), "audit".into()]);
        selection.selected.insert("shop".into());
        selection.default = Some("shop".into());
    }
    let fields: Vec<_> = [2, 1, 3]
        .iter()
        .map(|id| {
            format!(
                "archery_profile_name_{}",
                app.archery.selected[id].profile_id
            )
        })
        .collect();
    for field in &fields {
        app.field_set(field, "  ");
    }
    for (id, field) in [2, 1, 3].iter().zip(&fields) {
        app.action("archery-preview", &backend);
        assert!(!app.archery.preview);
        assert!(app.archery.expanded.contains(id));
        let error = app.workbench.field_error.as_ref().unwrap();
        assert_eq!(&error.field, field);
        assert_eq!(error.message.render(), crate::i18n::tr("请填写连接名称"));
        assert_eq!(
            app.workbench.focus_request.as_deref(),
            Some(format!("input-{field}").as_str())
        );
        assert_eq!(app.field(field), "  ");
        assert_eq!(app.archery.selected[id].default.as_deref(), Some("shop"));
        assert!(app.archery.selected[id].selected.contains("shop"));
        assert!(requests.try_recv().is_err());
        app.field_set(field, format!("connection-{id}"));
    }
    app.action("archery-preview", &backend);
    assert!(app.archery.preview);
    assert!(app.workbench.field_error.is_none());
    assert!(requests.try_recv().is_err());
}

#[test]
fn archery_wizard_authenticates_and_preserves_batch_after_partial_save() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("wizard".into());
    app.archery_action("archery-open", &backend);
    let list = requests.try_recv().unwrap();
    app.archery_response(
        Response {
            tag: list.tag,
            result: Ok(json!({"servers":[]})),
        },
        &backend,
    );
    app.field_set("archery_name", "Platform");
    app.field_set("archery_url", "https://example.test/");
    app.field_set("archery_user", "alice");
    app.field_set("password", "secret");
    app.archery_action("archery-login", &backend);
    let login = requests.try_recv().unwrap();
    assert_eq!(login.method, "archery.auth.begin");
    assert_eq!(login.params["server"]["username"], "alice");
    assert_eq!(login.params["password"], "secret");
    let server = login.params["server"].clone();
    assert!(Uuid::parse_str(server["id"].as_str().unwrap()).is_ok());
    app.archery_response(
        Response {
            tag: login.tag,
            result: Ok(json!({"auth_id":"auth","state":"challenge_required","auth_generation":1,"controller_token":Uuid::new_v4()})),
        },
        &backend,
    );
    app.field_set("archery_otp", "123456");
    app.archery_action("archery-verify", &backend);
    let verify = requests.try_recv().unwrap();
    assert_eq!(verify.method, "archery.auth.verify");
    assert_eq!(verify.params["otp"], "123456");
    assert_eq!(app.field("archery_otp"), "");
    app.archery_response(
        Response {
            tag: verify.tag,
            result: Ok(json!({"auth_id":"auth","state":"ready"})),
        },
        &backend,
    );
    let instances = requests.try_recv().unwrap();
    assert_eq!(instances.method, "archery.instances");
    app.archery_response(
        Response {
            tag: instances.tag,
            result: Ok(json!({"instances":[
                {"id":1,"name":"mysql","provider":"mysql"},
                {"id":2,"name":"redis","provider":"redis"}
            ]})),
        },
        &backend,
    );
    app.archery_action("archery-instance:0", &backend);
    app.archery_action("archery-load:1", &backend);
    let discovery = requests.try_recv().unwrap();
    assert_eq!(discovery.params["instance_id"], 1);
    app.archery_response(
        Response {
            tag: discovery.tag,
            result: Err(AppError::new("denied", "discovery failed")),
        },
        &backend,
    );
    app.archery_action("archery-preview", &backend);
    assert!(!app.archery.preview);
    app.archery_action("archery-load:1", &backend);
    let discovery = requests.try_recv().unwrap();
    app.archery_response(
        Response {
            tag: discovery.tag,
            result: Ok(json!({"databases":["shop","audit"],"discovery":"platform_reported"})),
        },
        &backend,
    );
    app.archery_action("archery-db:1:0", &backend);
    app.archery_action("archery-db:1:1", &backend);
    app.archery_action("archery-default:1:0", &backend);
    app.archery_action("archery-instance:1", &backend);
    app.archery_action("archery-load:2", &backend);
    let discovery = requests.try_recv().unwrap();
    app.archery_response(
        Response {
            tag: discovery.tag,
            result: Ok(json!({"databases":["0","3"],"discovery":"platform_reported"})),
        },
        &backend,
    );
    app.archery_action("archery-db:2:1", &backend);
    app.archery_action("archery-preview", &backend);
    assert!(!app.archery.preview);
    app.archery_action("archery-default:2:1", &backend);
    app.archery_action("archery-preview", &backend);
    assert!(app.archery.preview);
    let first_id = app.archery.selected[&1].profile_id;
    let second_id = app.archery.selected[&2].profile_id;
    app.archery_action("archery-save", &backend);
    let save = requests.try_recv().unwrap();
    assert_eq!(save.method, "archery.server.save");
    assert_eq!(save.params["server"]["id"], server["id"]);
    let mut saved_server = server;
    saved_server["revision"] = json!(1);
    app.archery_response(
        Response {
            tag: save.tag,
            result: Ok(saved_server.clone()),
        },
        &backend,
    );
    let batch = requests.try_recv().unwrap();
    assert_eq!(batch.method, "archery.profile.apply_batch");
    assert_eq!(batch.params["expected_server_revision"], 1);
    let items = batch.params["desired_profiles"].clone();
    assert_eq!(items[0]["id"], json!(first_id));
    assert_eq!(
        items[0]["connection"]["databases"],
        json!(["audit", "shop"])
    );
    assert_eq!(items[0]["connection"]["default_database"], "shop");
    assert_eq!(items[1]["id"], json!(second_id));
    assert_eq!(items[1]["connection"]["databases"], json!(["3"]));
    app.archery_response(
        Response {
            tag: batch.tag,
            result: Err(AppError::new("transport", "reply lost")),
        },
        &backend,
    );
    assert!(app.archery.saved && app.archery.open && app.archery.preview);
    let reconcile = requests.try_recv().unwrap();
    assert_eq!(reconcile.method, "profile.list");
    app.archery_response(
        Response {
            tag: reconcile.tag,
            result: Err(AppError::new("transport", "reconciliation failed")),
        },
        &backend,
    );
    app.archery_action("archery-save", &backend);
    assert!(requests.try_recv().is_err());
    app.archery_action("archery-reconcile", &backend);
    let reconcile = requests.try_recv().unwrap();
    assert_eq!(reconcile.method, "profile.list");
    app.archery_response(
        Response {
            tag: reconcile.tag,
            result: Ok(json!({"profiles":[]})),
        },
        &backend,
    );
    assert!(app.archery.recovery.is_some());
    assert!(requests.try_recv().is_err());
}

#[test]
fn archery_cancel_discovery_preserves_draft_and_refreshes_again() {
    for method in ["archery.instances", "archery.databases"] {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = App::new("archery-cancel".into());
        let server_id = Uuid::new_v4();
        let profile_id = Uuid::new_v4();
        let profile: Profile = serde_json::from_value(json!({"id":profile_id,"name":"saved","provider":"mysql","access_mode":"read_only","connection":{"kind":"archery","server_id":server_id,"instance_id":42,"instance_name":"remote","databases":["shop","audit"],"default_database":"shop"}})).unwrap();
        app.profiles.push(profile);
        app.archery_open_server(server_id, 42, &backend);
        let list = requests.try_recv().unwrap();
        app.archery_response(Response { tag: list.tag, result: Ok(json!({"servers":[{"id":server_id,"name":"platform","base_url":"https://example.test/","username":"alice","http_tls":{},"totp_mode":"manual","revision":1}]})) }, &backend);
        app.field_set(&format!("archery_profile_name_{profile_id}"), "draft name");
        app.archery_action("archery-db:42:1", &backend);
        let retry = if method == "archery.instances" {
            "archery-instances"
        } else {
            "archery-load:42"
        };
        app.archery_action(retry, &backend);
        let login = requests.try_recv().unwrap();
        app.archery_response(Response { tag: login.tag, result: Ok(json!({"auth_id":"first","state":"ready","auth_generation":5,"controller_token":"controller"})) }, &backend);
        let pending = requests.try_recv().unwrap();
        assert_eq!(pending.method, method);
        app.archery.discovery_queue.push(99);
        app.archery_action("archery-cancel-auth", &backend);
        let cancel = requests.try_recv().unwrap();
        assert_eq!(cancel.method, "archery.operation.cancel");
        assert_eq!(
            cancel.params["operation_id"],
            pending.params["operation_id"]
        );
        let close = requests.try_recv().unwrap();
        assert_eq!(close.method, "archery.auth.close");
        assert_eq!(close.params["auth_id"], "first");
        assert!(app.archery.pending.is_none());
        assert!(app.archery.loading_instance.is_none());
        assert!(app.archery.discovery_queue.is_empty());
        assert_eq!(app.archery.baseline.len(), 1);
        assert_eq!(
            app.field(&format!("archery_profile_name_{profile_id}")),
            "draft name"
        );
        assert_eq!(
            app.archery.selected[&42]
                .selected
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["shop"]
        );
        assert_eq!(app.archery.selected[&42].default.as_deref(), Some("shop"));
        app.archery_response(
            Response {
                tag: pending.tag,
                result: Ok(json!({"instances":[],"databases":[]})),
            },
            &backend,
        );
        app.archery_action(retry, &backend);
        let login = requests.try_recv().unwrap();
        assert_eq!(login.method, "archery.auth.begin");
        app.archery_response(
            Response {
                tag: login.tag,
                result: Ok(json!({"auth_id":"second","state":"ready","auth_generation":1})),
            },
            &backend,
        );
        let refresh = requests.try_recv().unwrap();
        assert_eq!(refresh.method, method);
        app.archery_response(Response { tag: refresh.tag, result: Ok(json!({"instances":[{"id":42,"name":"remote","provider":"mysql"}],"databases":["shop","audit"]})) }, &backend);
        assert_eq!(app.archery.selected[&42].default.as_deref(), Some("shop"));
        assert!(app.archery.selected[&42].selected.contains("shop"));
        assert!(!app.archery.selected[&42].selected.contains("audit"));
    }
}

#[test]
fn archery_wizard_ignores_responses_from_an_old_round() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("wizard-round".into());
    app.archery_action("archery-open", &backend);
    let old = requests.try_recv().unwrap();
    app.archery_action("archery-open", &backend);
    let current = requests.try_recv().unwrap();
    app.archery_response(
        Response {
            tag: old.tag,
            result: Ok(json!({"servers":[{"name":"stale"}]})),
        },
        &backend,
    );
    assert!(app.archery.servers.is_empty());
    assert!(app.archery.pending.is_some());
    app.archery_response(
        Response {
            tag: current.tag,
            result: Ok(json!({"servers":[{"name":"current"}]})),
        },
        &backend,
    );
    assert_eq!(app.archery.servers[0]["name"], "current");
}

#[test]
fn saved_archery_login_without_password_routes_success_and_error() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("saved-archery".into());
    app.archery_action("archery-open", &backend);
    let list = requests.try_recv().unwrap();
    let server_id = Uuid::new_v4();
    app.routed_response(Response {
        tag: list.tag,
        result: Ok(json!({"servers":[{"id":server_id,"name":"saved","base_url":"https://example.test/","username":"alice","http_tls":{},"revision":1,"credential_mode":"unknown"}]})),
    }, &backend);
    app.archery_action("archery-server:0", &backend);
    app.archery_action("archery-instances", &backend);
    let login = requests.try_recv().unwrap();
    assert_eq!(login.method, "archery.auth.begin");
    assert_eq!(login.params["server"]["id"], json!(server_id));
    assert!(login.params["password"].is_null());
    app.routed_response(
        Response {
            tag: login.tag,
            result: Err(AppError::new("auth_required", "请输入 Archery 密码")),
        },
        &backend,
    );
    assert!(app.archery.pending.is_none());
    assert!(
        app.archery
            .error
            .as_deref()
            .unwrap()
            .contains("请输入 Archery 密码")
    );
    app.archery_action("archery-login", &backend);
    assert!(requests.try_recv().is_err());
    app.field_set("password", "replacement fixture password");
    app.archery_action("archery-login", &backend);
    let login = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: login.tag,
            result: Ok(json!({"auth_id":Uuid::new_v4(),"server_id":server_id,"state":"ready"})),
        },
        &backend,
    );
    assert_eq!(app.archery.state, "ready");
    assert_eq!(requests.try_recv().unwrap().method, "archery.instances");
}

#[test]
fn archery_management_restores_unchecked_saved_instance_and_can_remove_all() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("archery-manage".into());
    let server_id = Uuid::new_v4();
    let profile_id = Uuid::new_v4();
    let profile: Profile = serde_json::from_value(json!({"id":profile_id,"name":"custom","provider":"mysql","access_mode":"read_only","connection":{"kind":"archery","server_id":server_id,"instance_id":42,"instance_name":"remote","databases":["shop"],"default_database":"shop"}})).unwrap();
    app.profiles.push(profile.clone());
    app.archery_open_server(server_id, 42, &backend);
    let list = requests.try_recv().unwrap();
    app.archery_response(Response { tag:list.tag, result:Ok(json!({"servers":[{"id":server_id,"name":"platform","base_url":"https://example.test/","username":"alice","http_tls":{},"totp_mode":"manual","revision":1}]})) }, &backend);
    assert_eq!(app.archery.selected[&42].profile_id, profile_id);
    assert!(app.archery.expanded.contains(&42));
    app.field_set(
        &format!("archery_profile_name_{profile_id}"),
        "unsaved name",
    );
    app.archery_action("archery-new", &backend);
    assert!(app.archery.server_only);
    app.field_set("archery_name", "another service");
    app.archery_action("overlay-close", &backend);
    assert!(app.archery.server_only);
    assert_eq!(app.field("archery_name"), "another service");
    app.archery_action("archery-discard-confirm", &backend);
    assert!(!app.archery.server_only);
    assert_eq!(
        app.field(&format!("archery_profile_name_{profile_id}")),
        "unsaved name"
    );
    assert_eq!(app.archery.selected[&42].profile_id, profile_id);
    assert_eq!(app.archery.server.as_ref().unwrap()["id"], json!(server_id));
    app.archery_action("archery-manage", &backend);
    app.field_set("archery_name", "renamed platform");
    app.archery_action("archery-save-server", &backend);
    let save = requests.try_recv().unwrap();
    assert_eq!(save.method, "archery.server.save");
    app.archery_response(Response { tag:save.tag, result:Ok(json!({"id":server_id,"name":"renamed platform","base_url":"https://example.test/","username":"alice","http_tls":{},"totp_mode":"manual","revision":2})) }, &backend);
    assert_eq!(app.archery.server.as_ref().unwrap()["revision"], 2);
    assert_eq!(
        app.field(&format!("archery_profile_name_{profile_id}")),
        "unsaved name"
    );
    assert_eq!(app.archery.selected[&42].profile_id, profile_id);
    app.archery_action("archery-instance:0", &backend);
    assert!(app.archery.selected.is_empty());
    app.archery_action("archery-instance:0", &backend);
    assert_eq!(app.archery.selected[&42].profile_id, profile_id);
    assert_eq!(
        app.field(&format!("archery_profile_name_{profile_id}")),
        "custom"
    );
    app.archery_action("archery-instance:0", &backend);
    app.archery_action("archery-preview", &backend);
    assert!(app.archery.preview);
    app.archery_action("archery-save", &backend);
    let batch = requests.try_recv().unwrap();
    assert_eq!(batch.method, "archery.profile.apply_batch");
    assert_eq!(batch.params["expected_profiles"], json!([profile]));
    assert_eq!(batch.params["desired_profiles"], json!([]));
}

#[test]
fn credential_storage_policy_changes_require_replacement_and_preserve_user_choice() {
    for mode in ["session", "keyring"] {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = App::new("credential-policy".into());
        let id = Uuid::new_v4();
        app.archery_manage_server(Some(id), &backend);
        let list = requests.try_recv().unwrap();
        app.archery_response(
            Response {
                tag: list.tag,
                result: Ok(json!({"servers":[{
                    "id":id,"name":"fixture","base_url":"https://example.test/","username":"alice",
                    "http_tls":{},"revision":1,"totp_mode":"manual","totp_configured":true,
                    "credential_mode":mode,"totp_credential_mode":mode
                }]})),
            },
            &backend,
        );
        let persisted = mode == "keyring";
        assert_eq!(app.archery.save_secret, persisted);
        assert_eq!(app.archery.save_totp, persisted);
        app.archery_action("archery-toggle-secret", &backend);
        app.archery_action("archery-toggle-totp", &backend);
        app.archery_action("archery-save-server", &backend);
        assert!(requests.try_recv().is_err());
        assert_eq!(
            app.workbench.field_error.as_ref().unwrap().field,
            "archery_totp_uri"
        );
        app.field_set(
            "archery_totp_uri",
            "otpauth://totp/Fixture?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ",
        );
        app.archery_action("archery-save-server", &backend);
        assert!(requests.try_recv().is_err());
        assert_eq!(
            app.workbench.field_error.as_ref().unwrap().field,
            "password"
        );
        app.archery_action("archery-login", &backend);
        assert!(requests.try_recv().is_err());
        app.field_set("password", "fixture replacement");
        app.archery_action("archery-login", &backend);
        let login = requests.try_recv().unwrap();
        app.archery_response(
            Response {
                tag: login.tag,
                result: Ok(json!({
                    "auth_id":Uuid::new_v4(),"server_id":id,"state":"ready","auth_generation":1
                })),
            },
            &backend,
        );
        let save = requests.try_recv().unwrap();
        assert_eq!(save.method, "archery.server.save");
        assert_eq!(save.params["password_update"]["persist"], !persisted);
        assert_eq!(save.params["totp_update"]["persist"], !persisted);
    }
}

#[test]
fn escape_returns_from_server_form_and_cancels_pending_authentication() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("escape-archery".into());
    app.profile_form(None);
    app.archery_action("archery-open", &backend);
    let list = requests.try_recv().unwrap();
    app.archery_response(
        Response {
            tag: list.tag,
            result: Ok(json!({"servers":[]})),
        },
        &backend,
    );
    app.archery_action("archery-new", &backend);
    assert!(app.archery.server_only);
    app.field_set("archery_name", "unsaved child");
    app.workbench_key(&tern_sdk::Key::named("escape"), &backend);
    assert!(app.archery.server_only);
    assert_eq!(app.field("archery_name"), "unsaved child");
    app.archery_action("archery-discard-cancel", &backend);
    assert_eq!(app.field("archery_name"), "unsaved child");
    app.workbench_key(&tern_sdk::Key::named("escape"), &backend);
    app.archery_action("archery-discard-confirm", &backend);
    assert!(app.archery.open && !app.archery.server_only);
    assert_eq!(app.workbench.overlay.as_deref(), Some("profile"));
    assert_eq!(app.field("archery_name"), "");

    app.archery_action("archery-new", &backend);
    app.field_set("archery_name", "fixture");
    app.field_set("archery_url", "https://example.test/");
    app.field_set("archery_user", "alice");
    app.field_set("password", "fixture password");
    app.archery_action("archery-auth-open", &backend);
    app.archery_action("archery-login", &backend);
    let login = requests.try_recv().unwrap();
    assert_eq!(login.method, "archery.auth.begin");
    app.workbench_key(&tern_sdk::Key::named("escape"), &backend);
    let cancel = requests.try_recv().unwrap();
    assert_eq!(cancel.method, "archery.operation.cancel");
    assert_eq!(cancel.params["operation_id"], login.params["operation_id"]);
    assert!(!app.archery.auth_dialog && app.archery.pending.is_none());
    assert_eq!(app.workbench.overlay.as_deref(), Some("profile"));
    app.archery_response(
        Response {
            tag: login.tag,
            result: Err(AppError::new("auth_required", "expired")),
        },
        &backend,
    );
    assert!(!app.archery.auth_dialog);
    assert_eq!(app.workbench.overlay.as_deref(), Some("profile"));
}

#[test]
fn archery_save_locks_editing_and_escape_until_failure_unlocks_the_form() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("save-lock".into());
    let id = Uuid::new_v4();
    app.archery_manage_server(Some(id), &backend);
    let list = requests.try_recv().unwrap();
    app.archery_response(
        Response {
            tag: list.tag,
            result: Ok(json!({"servers":[{
                "id":id,"name":"fixture","base_url":"https://example.test/","username":"alice",
                "http_tls":{},"revision":1,"totp_mode":"manual","totp_configured":false,
                "credential_mode":"unknown","totp_credential_mode":"unknown"
            }]})),
        },
        &backend,
    );
    app.archery.state = "ready".into();
    app.archery.auth_id = Some(Uuid::new_v4().to_string());
    app.field_set("archery_name", "submitted");
    app.archery_action("archery-save-server", &backend);
    let save = requests.try_recv().unwrap();
    assert_eq!(save.method, "archery.server.save");
    app.dispatch_editor_message(Msg::Edit("archery_name".into(), 9, 9, "!".into(), 10, 9));
    app.focus = "archery_name".into();
    app.input_key(&tern_sdk::Key::named("backspace"));
    assert_eq!(app.field("archery_name"), "submitted");
    app.workbench_key(&tern_sdk::Key::named("escape"), &backend);
    assert_eq!(app.workbench.overlay.as_deref(), Some("profile"));
    assert!(app.archery.pending.is_some());
    app.archery_response(
        Response {
            tag: save.tag,
            result: Err(AppError::new("conflict", "changed")),
        },
        &backend,
    );
    assert!(!app.connection_form_busy());
    app.dispatch_editor_message(Msg::Edit("archery_name".into(), 9, 9, "!".into(), 10, 9));
    assert_eq!(app.field("archery_name"), "submitted!");
}

#[test]
fn preview_type_selection_rejects_keyboard_switching() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("preview-types".into());
    app.workbench.overlay = Some("profile".into());
    app.archery.open = true;
    app.archery.preview = true;
    app.focus = "provider-archery".into();
    assert!(app.workbench_key(&tern_sdk::Key::named("left"), &backend));
    assert!(app.archery.open && app.archery.preview);
    assert_eq!(app.focus, "provider-archery");
    assert!(requests.try_recv().is_err());
}

#[test]
fn archery_default_keyboard_rejects_disappeared_instances() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("archery-default-focus".into());
    app.archery.open = true;
    app.workbench.overlay = Some("profile".into());
    app.archery.instances = vec![json!({"id":1,"name":"one","provider":"mysql"})];
    app.archery_action("archery-instance:0", &backend);
    let selection = app.archery.selected.get_mut(&1).unwrap();
    selection.databases = Some(vec!["missing".into(), "shop".into(), "audit".into()]);
    selection.selected = ["missing".into(), "shop".into(), "audit".into()]
        .into_iter()
        .collect();
    selection.available_databases = Some(["shop".into(), "audit".into()].into_iter().collect());
    selection.default = Some("missing".into());

    app.archery_action("archery-default:1:1", &backend);
    assert_eq!(app.focus, "archery-default-1-1");
    assert_eq!(app.workbench.focus_request.as_deref(), Some(""));
    assert!(app.workbench_key(&tern_sdk::Key::named("right"), &backend));
    assert_eq!(app.archery.selected[&1].default.as_deref(), Some("audit"));
    assert_eq!(app.workbench.focus_request.as_deref(), Some(""));

    app.archery.instances.clear();
    app.archery_action("archery-db:1:1", &backend);
    app.archery_action("archery-default:1:1", &backend);
    assert!(app.workbench_key(&tern_sdk::Key::named("left"), &backend));
    assert!(app.archery.selected[&1].selected.contains("shop"));
    assert_eq!(app.archery.selected[&1].default.as_deref(), Some("audit"));

    app.archery.instances = vec![json!({"id":1,"missing":true})];
    app.archery_action("archery-default:1:1", &backend);
    assert!(app.workbench_key(&tern_sdk::Key::named("left"), &backend));
    assert_eq!(app.archery.selected[&1].default.as_deref(), Some("audit"));
}

#[test]
fn archery_http_rejects_ca_expansion() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("archery-ca".into());
    app.archery.open = true;
    app.field_set("archery_url", "http://example.test");
    app.archery_action("archery-server-details", &backend);
    assert!(!app.archery.server_details);
    app.field_set("archery_url", "https://example.test");
    app.archery_action("archery-server-details", &backend);
    assert!(app.archery.server_details);
}

#[test]
fn modal_tabs_keep_host_text_focus_cleared_and_approval_can_reopen() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("modal-tab-focus".into());
    app.workbench.overlay = Some("settings".into());
    app.focus = "settings-tab-general".into();
    assert!(app.workbench_key(&tern_sdk::Key::named("right"), &backend));
    assert_eq!(app.focus, "settings-tab-editing");
    assert_eq!(app.workbench.focus_request.as_deref(), Some(""));
    app.workbench.overlay = None;
    app.action("approval-toggle", &backend);
    app.action("approval-close", &backend);
    assert_eq!(app.focus, "approval-open");
    assert!(app.workbench_key(&tern_sdk::Key::named("enter"), &backend));
    assert!(app.approvals.open);
}
