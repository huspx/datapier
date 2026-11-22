use super::*;

fn receive(app: &mut App, backend: &Backend, ok: bool, value: Value, error: Option<&str>) {
    let id = app.native_ui.request.as_ref().unwrap()["id"].clone();
    let payload = json!({"id":id,"ok":ok,"value":value,"error":error});
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&payload).unwrap());
    assert!(app.native_ui_action(&format!("datapier-plugin-reply:{encoded}"), backend));
}

#[test]
fn saving_update_policy_retains_prepared_version_and_status() {
    let mut app = App::new("native-update-test".into());
    let (backend, _, _) = Backend::scripted();
    app.native_ui.status = json!({"state":"ready","target_version":"0.5.1","prepared":true});
    app.native_ui.saved = Some(UpdatePolicy {
        auto_check: true,
        auto_download: false,
        auto_apply: false,
    });
    app.native_ui.draft = Some(UpdatePolicy {
        auto_check: false,
        auto_download: false,
        auto_apply: false,
    });
    let settings = serde_json::to_value(&app.settings).unwrap();
    app.settings_response(
        Response {
            tag: format!("settings.get/{}", app.settings_epoch),
            result: Ok(settings),
        },
        &backend,
    );
    app.application_action("settings-save", &backend);
    assert_eq!(
        app.native_ui.request.as_ref().unwrap()["action"],
        "configure"
    );
    receive(
        &mut app,
        &backend,
        true,
        json!({"settings":{"auto_check":false,"auto_download":false,"auto_apply":false}}),
        None,
    );
    assert_eq!(app.native_ui.status["state"], "ready");
    assert_eq!(app.native_ui.status["target_version"], "0.5.1");
    assert!(!app.native_ui.settings_dirty());
    assert!(!app.native_ui.saved.unwrap().auto_check);
}

#[test]
fn failed_global_save_keeps_update_policy_as_draft() {
    let mut app = App::new("native-update-combined-save".into());
    let (backend, mut requests, _) = Backend::scripted();
    let settings = serde_json::to_value(&app.settings).unwrap();
    app.settings_response(
        Response {
            tag: format!("settings.get/{}", app.settings_epoch),
            result: Ok(settings),
        },
        &backend,
    );
    let initial = UpdatePolicy {
        auto_check: true,
        auto_download: false,
        auto_apply: false,
    };
    app.native_ui.saved = Some(initial);
    app.native_ui.draft = Some(UpdatePolicy {
        auto_check: false,
        ..initial
    });
    app.application_action("settings-language:en", &backend);
    app.application_action("settings-save", &backend);
    let written = requests.try_recv().unwrap();
    assert!(app.native_ui.request.is_none());
    app.settings_response(
        Response {
            tag: written.tag,
            result: Err(AppError::new("storage", "disk full")),
        },
        &backend,
    );
    assert!(app.native_ui.request.is_none());
    assert!(app.native_ui.settings_dirty());
    assert!(app.native_ui.saved.unwrap().auto_check);
}

#[test]
fn failed_update_keeps_diagnostic_and_failed_status() {
    let mut app = App::new("native-update-failure-test".into());
    let (backend, _, _) = Backend::scripted();
    app.native_update_request("check");
    receive(
        &mut app,
        &backend,
        false,
        json!({"state":"failed","message":"摘要不匹配"}),
        Some("摘要不匹配"),
    );
    assert_eq!(app.native_ui.status["state"], "failed");
    assert_eq!(app.native_ui.error.as_deref(), Some("摘要不匹配"));
    assert!(app.native_ui.request.is_none());
}

#[test]
fn failed_status_capability_controls_auto_apply_and_policy_changes() {
    fn switch<'a>(node: &'a Node<Msg>, label: &str) -> Option<&'a Node<Msg>> {
        if node.kind() == "switch"
            && node.props().get("label").and_then(Value::as_str) == Some(label)
        {
            return Some(node);
        }
        node.child_nodes()
            .iter()
            .find_map(|child| switch(child, label))
    }

    for capability in [Some(true), Some(false), None] {
        let mut app = App::new("update-capability".into());
        let (backend, _, _) = Backend::scripted();
        let policy = UpdatePolicy {
            auto_check: true,
            auto_download: true,
            auto_apply: false,
        };
        app.native_ui.saved = Some(policy);
        app.native_ui.draft = Some(policy);
        app.native_ui.status = json!({"state":"ready","automatic_install_supported":true});
        app.native_update_request("check");
        let mut failed = json!({"state":"failed","message":"检查失败"});
        if let Some(supported) = capability {
            failed["automatic_install_supported"] = json!(supported);
        }
        receive(&mut app, &backend, false, failed, Some("检查失败"));

        let panel = update_settings(&app);
        let label = i18n::tr("退出后自动应用");
        let control = switch(&panel, label.as_ref()).unwrap();
        assert_eq!(
            control.props().get("disabled"),
            Some(&json!(capability != Some(true)))
        );

        assert!(app.native_ui_action("native-update-toggle:auto_apply", &backend));
        assert_eq!(
            app.native_ui.draft.unwrap().auto_apply,
            capability == Some(true)
        );
        assert!(app.native_ui_action("native-update-toggle:auto_check", &backend));
        let snapshot = serde_json::to_value(&app.settings).unwrap();
        app.settings_response(
            Response {
                tag: format!("settings.get/{}", app.settings_epoch),
                result: Ok(snapshot),
            },
            &backend,
        );
        assert!(app.application_action("settings-save", &backend));
        let settings = &app.native_ui.request.as_ref().unwrap()["settings"];
        assert_eq!(settings["auto_apply"], capability == Some(true));
        assert_eq!(settings["auto_check"], false);
        assert_eq!(settings["auto_download"], true);
    }
}

#[test]
fn install_action_requires_explicit_support_and_prepared_update() {
    fn button<'a>(node: &'a Node<Msg>, label: &str) -> Option<&'a Node<Msg>> {
        if node.kind() == "button"
            && node.props().get("text").and_then(Value::as_str) == Some(label)
        {
            return Some(node);
        }
        node.child_nodes()
            .iter()
            .find_map(|child| button(child, label))
    }

    for capability in [Some(true), Some(false), None] {
        let mut app = App::new("update-install-capability".into());
        let (backend, _, _) = Backend::scripted();
        app.native_ui.status = json!({"state":"ready","prepared":true});
        if let Some(supported) = capability {
            app.native_ui.status["automatic_install_supported"] = json!(supported);
        }
        let panel = update_settings(&app);
        let label = i18n::tr("退出后安装更新");
        let control = button(&panel, label.as_ref()).unwrap();
        assert_eq!(
            control.props().get("disabled"),
            Some(&json!(capability != Some(true)))
        );
        assert!(app.native_ui_action("native-update:apply", &backend));
        assert_eq!(app.native_ui.request.is_some(), capability == Some(true));
    }
}
