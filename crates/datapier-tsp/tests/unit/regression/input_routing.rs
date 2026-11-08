use super::*;
use crate::approval::ApprovalTab;

#[test]
fn programmatic_focus_routes_typing_to_the_requested_field() {
    let mut app = App::new("test".into());
    app.focus = "query".into();
    app.action("new-profile", &Backend::scripted().0);
    assert_eq!(app.take_focus_request().as_deref(), Some("input-name"));
    let mut typed = tern_sdk::Key::named("x");
    typed.text = Some("x".into());
    app.input_key(&typed);
    assert_eq!(app.field("name"), "x");
    assert!(app.tabs[0].query.text.is_empty());
    app.focus = "grid".into();
    app.workbench.focus_request = Some(String::new());
    assert_eq!(app.take_focus_request().as_deref(), Some(""));
    assert_eq!(app.focus, "grid");
}

#[test]
fn selecting_existing_resource_restores_focus_and_marks_workspace_dirty() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("resource-focus".into());
    let profile = ui_profile("mysql", "mysql");
    app.profile = Some(profile.clone());
    app.tabs[0].profile_id = Some(profile.id);
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.target = Some(ResourceTarget {
        database: Some("db".into()),
        name: "t".into(),
        key_b64: None,
        kind: "table".into(),
    });
    second_tab(&mut app);
    app.switch_tab(1);
    app.profile = Some(profile.clone());
    app.tabs[1].profile_id = Some(profile.id);
    app.resources =
        vec![serde_json::from_value(json!({"name":"t","kind":"table","database":"db"})).unwrap()];
    app.focus = "query".into();
    app.dirty = false;
    app.select_resource(0, &backend);
    assert_eq!(app.active, 0);
    assert_eq!(app.focus, "grid");
    assert_eq!(app.workbench.focus_request.as_deref(), Some("grid"));
    assert!(app.dirty);
}

#[test]
fn modal_dispatch_rejects_hidden_fields_and_accepts_its_own_fields() {
    let mut app = App::new("modal".into());
    app.field_set("pattern", "base");
    app.dispatch_editor_message(Msg::Edit("pattern".into(), 4, 4, "!".into(), 5, 4));
    app.focus = "pattern".into();
    app.application_action("app-settings", &Backend::scripted().0);
    app.reconcile_modal_focus();
    app.focus = "pattern".into();
    let mut key = tern_sdk::Key::named("x");
    key.text = Some("x".into());
    app.input_key(&key);
    app.dispatch_editor_message(Msg::Edit("pattern".into(), 5, 5, "x".into(), 6, 5));
    app.dispatch_editor_message(Msg::Undo("pattern".into()));
    assert_eq!(app.field("pattern"), "base!");

    app.workbench.overlay = Some("file".into());
    app.dispatch_editor_message(Msg::Edit("path".into(), 0, 0, "a".into(), 1, 0));
    assert_eq!(app.field("path"), "a");
    app.dispatch_editor_message(Msg::Undo("path".into()));
    assert_eq!(app.field("path"), "");
    app.workbench.overlay = Some("key-type".into());
    app.focus = "pattern".into();
    app.input_key(&key);
    assert_eq!(app.field("pattern"), "base!x");
}

#[test]
fn settings_and_pending_tabs_accept_keys_after_open_and_pointer_activation() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("modal-tabs".into());
    app.action("app-settings", &backend);
    app.reconcile_modal_focus();
    app.take_focus_request();
    assert!(app.workbench_key(&tern_sdk::Key::named("right"), &backend));
    assert_eq!(app.field("settings-tab"), "editing");
    app.focus = "settings-paging-table_rows".into();
    app.action("settings-tab:general", &backend);
    assert!(app.workbench_key(&tern_sdk::Key::named("end"), &backend));
    assert_eq!(app.field("settings-tab"), "mcp");
    assert!(app.workbench_key(&tern_sdk::Key::named("home"), &backend));
    assert_eq!(app.field("settings-tab"), "general");
    app.action("overlay-close", &backend);
    app.action("approval-toggle", &backend);
    app.action("approval-tab-approvals", &backend);
    assert!(app.workbench_key(&tern_sdk::Key::named("right"), &backend));
    assert!(matches!(app.approvals.tab, ApprovalTab::Authentication));
    assert!(app.workbench_key(&tern_sdk::Key::named("end"), &backend));
    assert!(matches!(app.approvals.tab, ApprovalTab::Tasks));
    app.action("approval-close", &backend);
    app.take_focus_request();
    assert!(app.workbench_key(&tern_sdk::Key::named("enter"), &backend));
    assert!(app.approvals.open);
}
