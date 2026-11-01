use super::*;
use datapier_core::model::{Column, ResultMeta, RowChange, Value as CellValue};

fn second_tab(app: &mut App) {
    app.tabs.push(Tab {
        title: "second".into(),
        query: Editor::new(""),
        cli_query: Editor::default(),
        profile_id: None,
        id: Uuid::new_v4(),
        layout: Vec::new(),
        browse: BrowseSettings::default(),
        pending: Value::Null,
        pending_live: false,
        runtime: TabRuntime::default(),
    });
}

#[test]
fn provider_keyboard_preserves_form_and_refuses_changes_during_save() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("provider".into());
    app.profile_form(None);
    app.field_set("name", "保留名称");
    app.field_set("port", "3307");
    app.workbench_action("field:provider:mysql", &backend);
    assert_eq!(app.field("port"), "3307");
    app.focus = "provider-mysql".into();
    assert!(app.workbench_key(&tern_sdk::Key::named("right"), &backend));
    assert_eq!(app.field("provider"), "redis");
    assert_eq!(app.field("port"), "6379");
    assert_eq!(app.field("name"), "保留名称");
    app.reconcile_modal_focus();
    assert!(app.workbench_key(&tern_sdk::Key::named("left"), &backend));
    assert_eq!(app.field("provider"), "mysql");
    assert_eq!(app.field("port"), "3306");
    app.profile_saves.insert(app.form_id.unwrap());
    app.reconcile_modal_focus();
    assert!(app.workbench_key(&tern_sdk::Key::named("right"), &backend));
    assert_eq!(app.field("provider"), "mysql");
    assert_eq!(app.field("name"), "保留名称");
}

#[test]
fn narrow_navigation_preserves_wide_preference_across_resize_and_restore() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("navigation".into());
    // 应用按终端列宽估算，此处位于 760px 窄屏边界内。
    let mut caps = tern_sdk::Capabilities {
        cols: 97,
        ..Default::default()
    };
    app.update_viewport(&caps);
    assert!(app.navigation_compact());
    app.action("navigation-toggle", &backend);
    assert!(!app.navigation_compact());
    assert!(app.workspace_snapshot().unwrap().navigation_expanded);
    caps.cols = 98;
    app.update_viewport(&caps);
    assert!(!app.navigation_compact());
    app.action("navigation-toggle", &backend);
    assert!(app.navigation_compact());
    caps.cols = 97;
    app.update_viewport(&caps);
    assert!(app.navigation_compact());
    app.action("navigation-toggle", &backend);
    assert!(!app.navigation_compact());
    let state = serde_json::to_value(app.workspace_snapshot().unwrap()).unwrap();
    let mut restored = App::new("navigation".into());
    restored.update_viewport(&caps);
    restored.response(
        Response {
            tag: "workspace.load".into(),
            result: Ok(json!({"revision":1,"state":state})),
        },
        &backend,
    );
    assert!(restored.navigation_compact());
    caps.cols = 98;
    restored.update_viewport(&caps);
    assert!(restored.navigation_compact());
    restored.action("navigation-toggle", &backend);
    assert!(!restored.navigation_compact());
    caps.cols = 97;
    restored.update_viewport(&caps);
    assert!(restored.navigation_compact());
}

#[test]
fn detail_and_frozen_source_follow_their_tab() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("detail".into());
    app.profile = Some(ui_profile("mysql", "first"));
    app.target = Some(ResourceTarget {
        database: Some("db".into()),
        name: "one".into(),
        kind: "table".into(),
        key_b64: None,
    });
    app.detail = "first result".into();
    app.set_detail_context("资源详情");
    second_tab(&mut app);
    app.switch_tab(1);
    assert!(app.detail.is_empty());
    app.profile = Some(ui_profile("redis", "second"));
    app.detail = "second result".into();
    app.set_detail_context("任务详情");
    app.routed_response(
        Response {
            tag: format!("{}:0:resource.describe", app.tabs[0].id),
            result: Ok(json!({"columns":[]})),
        },
        &backend,
    );
    assert_eq!(app.detail, "second result");
    assert_eq!(app.detail_context.connection.as_deref(), Some("second"));
    app.switch_tab(0);
    assert_eq!(app.detail, json!({"columns":[]}).to_string());
    assert_eq!(app.detail_context.connection.as_deref(), Some("first"));
    assert_eq!(app.detail_context.resource.as_deref(), Some("db.one"));
    app.switch_tab(1);
    assert_eq!(app.detail, "second result");
    assert_eq!(app.detail_context.connection.as_deref(), Some("second"));
    assert_eq!(app.detail_context.kind, "任务详情");
}

#[test]
fn cli_command_and_late_failure_stay_with_originating_tab() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("cli-tabs".into());
    app.profile = Some(ui_profile("redis", "first"));
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.field_set("cli_text", "GET first");
    app.action("cli-run", &backend);
    let start = requests.try_recv().unwrap();
    app.field_set("cli_text", "GET edited");
    second_tab(&mut app);
    app.switch_tab(1);
    app.profile = Some(ui_profile("redis", "second"));
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.field_set("cli_text", "PING");
    app.action("cli-run", &backend);
    requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: start.tag,
            result: Err(AppError::new("failure", "first failure")),
        },
        &backend,
    );
    assert_eq!(app.workbench.cli_command.as_deref(), Some("PING"));
    assert!(app.workbench.cli_error.is_empty());
    app.switch_tab(0);
    assert_eq!(app.field("cli_text"), "GET edited");
    assert_eq!(app.workbench.cli_command.as_deref(), Some("GET first"));
    assert!(app.workbench.cli_error.render().contains("first failure"));
}
#[test]
fn opening_another_redis_key_keeps_browser_filters_and_tree_mode() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("redis-browser".into());
    let profile = ui_profile("redis", "cache");
    app.profile = Some(profile.clone());
    app.tabs[0].profile_id = Some(profile.id);
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.target = Some(ResourceTarget {
        database: None,
        name: "app:a".into(),
        kind: "hash".into(),
        key_b64: None,
    });
    app.resources = serde_json::from_value(json!([{"name":"app:b","kind":"hash"}])).unwrap();
    app.field_set("pattern", "app:*");
    app.field_set("type", "hash");
    app.workbench.tree = true;
    app.select_resource(0, &backend);
    let open = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: open.tag,
            result: Ok(json!({"session_id":Uuid::new_v4(),"profile":profile})),
        },
        &backend,
    );
    assert_eq!(app.field("pattern"), "app:*");
    assert_eq!(app.field("type"), "hash");
    assert!(app.workbench.tree);
    assert_eq!(app.target.as_ref().unwrap().name, "app:b");
}

#[test]
fn redis_cli_enter_runs_after_switching_from_mysql() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("cross-provider".into());
    let mysql = ui_profile("mysql", "mysql");
    let redis = ui_profile("redis", "redis");
    app.profiles = vec![mysql.clone(), redis.clone()];
    app.profile = Some(mysql.clone());
    app.tabs[0].profile_id = Some(mysql.id);
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.select_profile(1, &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.method, "connection.open");
    let session = Uuid::new_v4();
    app.routed_response(
        Response {
            tag: open.tag,
            result: Ok(json!({"session_id": session, "profile": redis})),
        },
        &backend,
    );
    while requests.try_recv().is_ok() {}
    assert_eq!(app.view, "query");
    app.action("cli-toggle", &backend);
    app.dispatch_editor_message(Msg::Edit("cli_text".into(), 0, 0, "PING".into(), 4, 0));
    assert!(app.workbench_key(&tern_sdk::Key::named("enter"), &backend));
    let execution = requests.try_recv().unwrap();
    assert_eq!(execution.method, "execution.start");
    assert_eq!(execution.params["text"], "PING");
    assert_eq!(execution.params["session_id"], json!(session));
}

#[test]
fn connection_failure_can_retry_and_late_failure_stays_with_its_tab() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    let profile = ui_profile("mysql", "first");
    app.profile = Some(profile.clone());
    app.action("open", &backend);
    let first = requests.try_recv().unwrap();
    assert!(app.workbench.connecting);
    second_tab(&mut app);
    app.switch_tab(1);
    app.status = i18n::UiMessage::raw("active tab");
    app.routed_response(
        Response {
            tag: first.tag,
            result: Err(AppError::new("connection_failed", "offline")),
        },
        &backend,
    );
    assert!(app.workbench.connection_error.is_none());
    assert_eq!(app.status.render(), "active tab");
    app.switch_tab(0);
    assert!(!app.workbench.connecting);
    assert!(
        app.workbench
            .connection_error
            .as_deref()
            .unwrap()
            .contains("offline")
    );
    app.action("open", &backend);
    let retry = requests.try_recv().unwrap();
    assert_eq!(retry.method, "connection.open");
    assert!(app.workbench.connecting);
    app.routed_response(
        Response {
            tag: retry.tag,
            result: Ok(json!({"session_id": Uuid::new_v4(), "profile": profile})),
        },
        &backend,
    );
    assert!(!app.workbench.connecting);
    assert!(app.workbench.connection_error.is_none());
}

#[test]
fn connection_enqueue_failure_leaves_retry_available() {
    let (backend, requests, _) = Backend::scripted();
    drop(requests);
    let mut app = App::new("test".into());
    app.profile = Some(ui_profile("mysql", "first"));
    app.action("open", &backend);
    assert!(!app.workbench.connecting && !app.operation_pending);
    assert!(app.workbench.connection_error.is_some());
    let (backend, mut requests, _) = Backend::scripted();
    app.action("open", &backend);
    assert_eq!(requests.try_recv().unwrap().method, "connection.open");
    assert!(app.workbench.connecting);
}

#[test]
fn saving_another_profile_keeps_disconnected_resource_context_until_selection() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    let old = ui_profile("redis", "old");
    let mut next = ui_profile("mysql", "next");
    next.database = Some("commerce".into());
    let target = ResourceTarget {
        database: Some("15".into()),
        name: "old:key".into(),
        key_b64: Some(base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b"old:key",
        )),
        kind: "hash".into(),
    };
    app.profile = Some(old.clone());
    app.tabs[0].profile_id = Some(old.id);
    app.target = Some(target.clone());
    app.database = Some("15".into());
    app.profiles = vec![old.clone(), next.clone()];
    app.response(
        Response {
            tag: "profile.save".into(),
            result: Ok(json!({"profile": next})),
        },
        &backend,
    );
    assert_eq!(app.profile.as_ref().unwrap().id, old.id);
    assert_eq!(app.target.as_ref().unwrap().name, target.name);
    assert_eq!(app.database.as_deref(), Some("15"));

    app.select_profile(1, &backend);
    assert_eq!(app.tabs.len(), 2);
    assert_eq!(app.profile.as_ref().unwrap().id, next.id);
    assert!(app.target.is_none());
    assert!(app.workbench.queued_target.is_none());
    let mut opened = None;
    while let Ok(request) = requests.try_recv() {
        if request.method == "connection.open" {
            opened = Some(request.params["profile_id"].clone());
        }
    }
    assert_eq!(opened, Some(json!(next.id)));
    app.switch_tab(0);
    assert_eq!(app.profile.as_ref().unwrap().id, old.id);
    assert_eq!(app.target.as_ref().unwrap().name, "old:key");
    assert_eq!(app.database.as_deref(), Some("15"));
}

#[test]
fn switching_disconnected_profile_preserves_sql_and_cli_drafts() {
    for provider in ["mysql", "redis"] {
        let (backend, _, _) = Backend::scripted();
        let mut app = App::new("restored-draft".into());
        let old = ui_profile(provider, "old");
        let next = ui_profile(
            if provider == "mysql" {
                "redis"
            } else {
                "mysql"
            },
            "next",
        );
        app.profile = Some(old.clone());
        app.tabs[0].profile_id = Some(old.id);
        let draft = if provider == "mysql" {
            "SELECT '草稿😀';"
        } else {
            "GET 草稿😀"
        };
        if provider == "mysql" {
            app.tabs[0].query = Editor::new(draft);
        } else {
            app.tabs[0].cli_query = Editor::new(draft);
        }
        app.profiles = vec![old.clone(), next.clone()];
        app.select_profile(1, &backend);
        assert_eq!(app.tabs[0].profile_id, Some(old.id));
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.profile.as_ref().unwrap().id, next.id);
        app.switch_tab(0);
        assert_eq!(app.profile.as_ref().unwrap().id, old.id);
        assert_eq!(
            if provider == "mysql" {
                &app.tabs[0].query.text
            } else {
                &app.tabs[0].cli_query.text
            },
            draft
        );
    }
}

#[test]
fn profile_error_preserves_fields_for_retry() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.action("new-profile", &backend);
    app.field_set("name", "saved name");
    app.action("test-profile", &backend);
    let request = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: request.tag,
            result: Err(AppError::new("connection_failed", "offline")),
        },
        &backend,
    );
    assert!(app.workbench.profile_error.is_some());
    assert_eq!(app.field("name"), "saved name");
    app.action("test-profile", &backend);
    let retry = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: retry.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(app.workbench.profile_error.is_none());
}

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
fn switching_sql_and_cli_tabs_routes_new_text_to_the_visible_editor() {
    let mut app = App::new("focus-tabs".into());
    app.tabs[0].query = Editor::new("SELECT");
    second_tab(&mut app);
    app.tabs[1].runtime.profile = Some(ui_profile("redis", "redis"));
    app.tabs[1].cli_query = Editor::new("GET");
    app.tabs[1].browse.cli_visible = true;
    app.switch_tab(1);
    app.focus = "cli_text".into();
    let mut typed = tern_sdk::Key::named("x");
    typed.text = Some(" 中文😀".into());
    app.select_tab(0);
    app.take_focus_request();
    app.input_key(&typed);
    assert_eq!(app.tabs[0].query.text, "SELECT 中文😀");
    assert_eq!(app.tabs[1].cli_query.text, "GET");
    app.select_tab(1);
    app.take_focus_request();
    app.input_key(&typed);
    assert_eq!(app.tabs[1].cli_query.text, "GET 中文😀");
    assert_eq!(app.tabs[0].query.text, "SELECT 中文😀");
}

#[test]
fn quit_keeps_drafts_when_workspace_save_fails() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.workspace_loaded = true;
    app.tabs[0].query = Editor::new("SELECT draft");
    app.dirty = true;
    assert!(app.request_quit(&backend));
    let request = requests.try_recv().unwrap();
    assert!(app.saving);
    app.routed_response(
        Response {
            tag: request.tag,
            result: Err(AppError::new("conflict", "revision conflict")),
        },
        &backend,
    );
    assert!(app.save_blocked && app.dirty && !app.saving);
    assert_eq!(app.tabs[0].query.text, "SELECT draft");
}

fn editable_grid() -> GridState {
    GridState::new(
        ResultPage {
            result: ResultMeta {
                id: Uuid::new_v4(),
                execution_id: Uuid::new_v4(),
                columns: vec![Column::new("name", "text")],
                kind: "rows".into(),
                row_count: 1,
                complete: true,
                summary: Value::Null,
            },
            rows: vec![vec![CellValue::Text("before".into())]],
            start: 0,
            next: None,
        },
        true,
    )
}

#[test]
fn saved_grid_edit_disappears_from_snapshot_after_undo() {
    let mut app = App::new("test".into());
    let mut grid = editable_grid();
    grid.edit(Cell { row: 0, column: 0 }, CellValue::Text("after".into()))
        .unwrap();
    app.grid = Some(grid);

    let saved = app.workspace_snapshot().unwrap();
    assert!(!saved.tabs[0].pending.is_null());
    assert!(app.grid.as_mut().unwrap().undo());
    let undone = app.workspace_snapshot().unwrap();
    assert!(undone.tabs[0].pending.is_null());
}

#[test]
fn sql_text_changes_clear_query_results_without_discarding_table_data() {
    let mut app = App::new("test".into());
    app.grid = Some(editable_grid());
    let id = app.grid.as_ref().unwrap().page.result.id.to_string();
    app.result_id = Some(id.clone());
    app.results.push(id);
    app.tabs[0].query = Editor::new("SELECT 1");
    app.tabs[0].query.set_selection(0, 0);
    app.focus = "query".into();
    app.input_key(&tern_sdk::Key::named("right"));
    assert_eq!(app.tabs[0].query.cursor, 1);
    assert!(app.grid.is_some());
    let mut typed = tern_sdk::Key::named("x");
    typed.text = Some("x".into());
    app.input_key(&typed);
    assert!(app.grid.is_none());
    assert!(app.result_id.is_none() && app.results.is_empty());

    app.grid = Some(editable_grid());
    app.browse_result = true;
    app.changed_field("query");
    assert_eq!(
        app.grid.as_ref().unwrap().value(Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("before".into()))
    );
}

#[test]
fn saved_row_and_redis_changes_disappear_after_undo_on_inactive_tab() {
    let mut app = App::new("test".into());
    second_tab(&mut app);
    app.pending_rows.push(RowChange {
        operation: "insert".into(),
        key: BTreeMap::new(),
        before: BTreeMap::new(),
        values: BTreeMap::new(),
    });
    app.redis_changes.push(json!({"operation": "delete_key"}));
    assert!(!app.workspace_snapshot().unwrap().tabs[0].pending.is_null());
    app.pending_rows.clear();
    app.redis_changes.clear();
    app.switch_tab(1);

    let snapshot = app.workspace_snapshot().unwrap();
    assert!(snapshot.tabs[0].pending.is_null());
    assert!(snapshot.tabs[1].pending.is_null());
}

#[test]
fn restored_pending_survives_an_unrelated_snapshot() {
    let mut original = App::new("test".into());
    original
        .redis_changes
        .push(json!({"operation": "delete_key"}));
    let saved = original.workspace_snapshot().unwrap();
    let persisted = serde_json::to_value(saved).unwrap();
    let loaded: Workspace = serde_json::from_value(persisted).unwrap();
    let pending = loaded.tabs[0].pending.clone();
    assert!(!pending.is_null());

    let mut reopened = App::new("test".into());
    reopened.tabs = loaded.tabs;
    reopened.active = loaded.active;
    assert_eq!(
        reopened.workspace_snapshot().unwrap().tabs[0].pending,
        pending
    );
}

#[test]
fn transfer_previews_and_pending_requests_are_isolated_by_tab() {
    let mut app = App::new("test".into());
    second_tab(&mut app);
    let first_preview = json!({"path": "first.csv"});
    let second_preview = json!({"path": "second.csv"});
    let first_request = (Uuid::new_v4(), json!({"path": "first-new.csv"}));
    let second_request = (Uuid::new_v4(), json!({"path": "second-new.csv"}));
    app.preview = Some(first_preview.clone());
    app.preview_pending = Some(first_request.clone());

    app.switch_tab(1);
    assert!(app.preview.is_none());
    assert!(app.preview_pending.is_none());
    app.preview = Some(second_preview.clone());
    app.preview_pending = Some(second_request.clone());

    app.switch_tab(0);
    assert_eq!(app.preview.as_ref(), Some(&first_preview));
    assert_eq!(app.preview_pending.as_ref(), Some(&first_request));
    app.switch_tab(1);
    assert_eq!(app.preview.as_ref(), Some(&second_preview));
    assert_eq!(app.preview_pending.as_ref(), Some(&second_request));
}

#[test]
fn profile_form_identity_is_stable_and_independent_of_selected_connection() {
    let mut app = App::new("test".into());
    app.profile_form(None);
    let selected = app.form_profile().unwrap();
    app.profile = Some(selected.clone());

    app.profile_form(Some(selected.clone()));
    assert_eq!(app.form_profile().unwrap().id, selected.id);
    app.field_set("name", "edited");
    assert_eq!(app.form_profile().unwrap().id, selected.id);

    app.profile_form(None);
    let new_id = app.form_profile().unwrap().id;
    assert_ne!(new_id, selected.id);
    app.field_set("name", "new");
    assert_eq!(app.form_profile().unwrap().id, new_id);
    assert_eq!(app.profile.as_ref().unwrap().id, selected.id);
}

#[test]
fn browse_settings_and_position_follow_their_tab() {
    let mut app = App::new("test".into());
    second_tab(&mut app);
    app.field_set("pattern", "user:*");
    app.field_set("type", "hash");
    app.field_set("filter", "first filter");
    app.field_set("sort", "first sort");
    app.database = Some("first db".into());
    app.target = Some(ResourceTarget {
        database: app.database.clone(),
        name: "first target".into(),
        key_b64: None,
        kind: "table".into(),
    });
    app.cursor = Some("first cursor".into());
    app.page_history = vec![10, 20];

    app.switch_tab(1);
    assert_eq!(app.field("pattern"), "*");
    assert_eq!(app.field("filter"), "[]");
    assert!(app.database.is_none());
    assert!(app.target.is_none());
    app.field_set("pattern", "order:*");
    app.field_set("type", "string");
    app.field_set("filter", "second filter");
    app.field_set("sort", "second sort");
    app.database = Some("second db".into());
    app.target = Some(ResourceTarget {
        database: app.database.clone(),
        name: "second target".into(),
        key_b64: None,
        kind: "table".into(),
    });
    app.cursor = Some("second cursor".into());
    app.page_history = vec![30];

    app.switch_tab(0);
    assert_eq!(app.field("pattern"), "user:*");
    assert_eq!(app.field("type"), "hash");
    assert_eq!(app.field("filter"), "first filter");
    assert_eq!(app.field("sort"), "first sort");
    assert_eq!(app.database.as_deref(), Some("first db"));
    assert_eq!(
        app.target.as_ref().map(|target| target.name.as_str()),
        Some("first target")
    );
    assert_eq!(app.cursor.as_deref(), Some("first cursor"));
    assert_eq!(app.page_history, vec![10, 20]);
    app.switch_tab(1);
    assert_eq!(app.field("pattern"), "order:*");
    assert_eq!(app.field("type"), "string");
    assert_eq!(app.field("filter"), "second filter");
    assert_eq!(app.field("sort"), "second sort");
    assert_eq!(app.database.as_deref(), Some("second db"));
    assert_eq!(
        app.target.as_ref().map(|target| target.name.as_str()),
        Some("second target")
    );
    assert_eq!(app.cursor.as_deref(), Some("second cursor"));
    assert_eq!(app.page_history, vec![30]);
}

#[test]
fn changing_scan_criteria_invalidates_only_the_current_tab_scan() {
    let mut app = App::new("test".into());
    second_tab(&mut app);
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    app.scan_token = Some(first);
    app.cursor = Some("first".into());
    app.switch_tab(1);
    app.scan_token = Some(second);
    app.cursor = Some("second".into());
    app.changed_field("filter");
    assert_eq!(app.scan_token, Some(second));
    app.changed_field("pattern");
    assert!(app.scan_token.is_none());
    assert!(app.cursor.is_none());
    app.switch_tab(0);
    assert_eq!(app.scan_token, Some(first));
    assert_eq!(app.cursor.as_deref(), Some("first"));
    app.changed_field("type");
    assert!(app.scan_token.is_none());
    assert!(app.cursor.is_none());
}

#[test]
fn paste_previews_and_live_drafts_are_isolated_by_tab() {
    let mut app = App::new("test".into());
    second_tab(&mut app);
    let grid = editable_grid();
    app.paste_preview = Some(grid.preview_paste("first").unwrap());
    app.grid = Some(grid);
    assert!(app.has_live_drafts());

    app.switch_tab(1);
    assert!(app.paste_preview.is_none());
    assert!(!app.has_live_drafts());
    let grid = editable_grid();
    app.paste_preview = Some(grid.preview_paste("second").unwrap());
    app.grid = Some(grid);
    app.switch_tab(0);
    assert_eq!(
        app.paste_preview.as_ref().unwrap().edits[0].after,
        CellValue::Text("first".into())
    );
    assert!(app.has_live_drafts());
    app.switch_tab(1);
    assert_eq!(
        app.paste_preview.as_ref().unwrap().edits[0].after,
        CellValue::Text("second".into())
    );
    assert!(app.has_live_drafts());
}

#[test]
fn live_drafts_detect_each_kind_of_pending_change() {
    let mut app = App::new("test".into());
    assert!(!app.has_live_drafts());
    let mut grid = editable_grid();
    grid.edit(Cell { row: 0, column: 0 }, CellValue::Text("after".into()))
        .unwrap();
    app.grid = Some(grid);
    assert!(app.has_live_drafts());
    app.grid.as_mut().unwrap().undo();
    assert!(!app.has_live_drafts());
    app.pending_rows.push(RowChange {
        operation: "insert".into(),
        key: BTreeMap::new(),
        before: BTreeMap::new(),
        values: BTreeMap::new(),
    });
    assert!(app.has_live_drafts());
    app.pending_rows.clear();
    app.redis_changes.push(json!({"operation": "delete_key"}));
    assert!(app.has_live_drafts());
    app.redis_changes.clear();
    assert!(!app.has_live_drafts());
}

#[test]
fn browse_settings_survive_workspace_serialization() {
    let mut app = App::new("test".into());
    second_tab(&mut app);
    app.field_set("filter", "first filter");
    app.field_set("sort", "first sort");
    app.database = Some("first db".into());
    app.switch_tab(1);
    app.field_set("filter", "second filter");
    app.field_set("sort", "second sort");
    app.database = Some("second db".into());

    let serialized = serde_json::to_value(app.workspace_snapshot().unwrap()).unwrap();
    let saved: Workspace = serde_json::from_value(serialized).unwrap();
    let mut reopened = App::new("test".into());
    reopened.tabs = saved.tabs;
    reopened.active = saved.active;
    reopened.restore_browse();
    assert_eq!(reopened.field("filter"), "second filter");
    assert_eq!(reopened.field("sort"), "second sort");
    assert_eq!(reopened.database.as_deref(), Some("second db"));
    reopened.switch_tab(0);
    assert_eq!(reopened.field("filter"), "first filter");
    assert_eq!(reopened.field("sort"), "first sort");
    assert_eq!(reopened.database.as_deref(), Some("first db"));
}

#[test]
fn saving_new_profile_keeps_existing_session_associated_with_its_profile() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.profile_form(None);
    app.field_set("name", "connection A");
    let profile_a = app.form_profile().unwrap();
    let session = json!({"session_id": Uuid::new_v4()});
    app.profile = Some(profile_a.clone());
    app.tabs[0].profile_id = Some(profile_a.id);
    app.session = Some(session.clone());

    app.action("new-profile", &backend);
    app.field_set("name", "connection B");
    app.action("save-profile", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "profile.save");
    let profile_b: Profile = serde_json::from_value(request.params["profile"].clone()).unwrap();
    assert_ne!(profile_a.id, profile_b.id);
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"profile": profile_b})),
        },
        &backend,
    );

    assert_eq!(app.profile.as_ref().unwrap().id, profile_a.id);
    assert_eq!(app.tabs[0].profile_id, Some(profile_a.id));
    assert_eq!(app.session, Some(session));
}

#[test]
fn profile_save_uses_the_profile_id_returned_by_the_backend() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.action("new-profile", &backend);
    app.field_set("name", "connection");
    app.action("save-profile", &backend);
    let request = requests.try_recv().unwrap();
    let mut returned: Profile = serde_json::from_value(request.params["profile"].clone()).unwrap();
    returned.id = Uuid::new_v4();
    let returned_id = returned.id;
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"profile": returned})),
        },
        &backend,
    );
    assert_eq!(app.profile.as_ref().unwrap().id, returned_id);
    assert_eq!(app.tabs[0].profile_id, Some(returned_id));
}

fn app_with_grid_draft() -> App {
    let mut app = App::new("test".into());
    let mut grid = editable_grid();
    grid.edit(Cell { row: 0, column: 0 }, CellValue::Text("draft".into()))
        .unwrap();
    app.grid = Some(grid);
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.target = Some(ResourceTarget {
        database: None,
        name: "table".into(),
        key_b64: None,
        kind: "table".into(),
    });
    app.tabs[0].query = Editor::new("select 1");
    app
}

#[test]
fn grid_draft_prevents_query_execution_request() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_with_grid_draft();
    app.action("run", &backend);
    assert!(requests.try_recv().is_err());
    assert!(app.has_live_drafts());
}

#[test]
fn grid_draft_prevents_resource_browse_request() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_with_grid_draft();
    app.action("browse", &backend);
    assert!(requests.try_recv().is_err());
    assert!(app.has_live_drafts());
}

#[test]
fn grid_draft_prevents_opening_another_edit_baseline() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_with_grid_draft();
    app.action("edit-open", &backend);
    assert!(requests.try_recv().is_err());
    assert!(app.has_live_drafts());
}

#[test]
fn old_scan_response_does_not_overwrite_new_results() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.action("list", &backend);
    let old = requests.try_recv().unwrap();
    app.field_set("pattern", "new*");
    app.changed_field("pattern");
    app.action("list", &backend);
    let new = requests.try_recv().unwrap();
    assert_ne!(old.tag, new.tag);

    app.routed_response(Response { tag: new.tag, result: Ok(json!({"resources": [{"name": "new", "kind": "table"}], "cursor": "new-cursor", "complete": false})) }, &backend);
    app.routed_response(Response { tag: old.tag, result: Ok(json!({"resources": [{"name": "old", "kind": "table"}], "cursor": "old-cursor", "complete": false})) }, &backend);
    assert_eq!(app.resources.len(), 1);
    assert_eq!(app.resources[0].name, "new");
    assert_eq!(app.cursor.as_deref(), Some("new-cursor"));
}

#[test]
fn successful_submission_retracts_stale_rows_and_retains_column_layout() {
    for operation in [None, Some("rename"), Some("delete_key")] {
        let (backend, _requests, _) = Backend::scripted();
        let mut app = app_with_grid_draft();
        app.grid.as_mut().unwrap().settings[0].width = 240;
        if let Some(operation) = operation {
            app.redis_changes.push(json!({"operation": operation}));
        }
        app.response(
            Response {
                tag: "execution.get".into(),
                result: Ok(json!({
                    "state": "succeeded", "method": "change.submit", "results": [],
                    "summary": {"items": [{"status": "applied"}]}
                })),
            },
            &backend,
        );
        assert!(app.grid.is_none(), "提交后不能重新显示写入前的缓存行");
        assert!(!app.has_live_drafts());
        assert_eq!(app.tabs[0].layout[0].width, 240);
        assert_eq!(app.target.is_none(), operation.is_some());
    }
}

#[test]
fn task_events_refresh_only_the_active_task_and_ignore_stale_responses() {
    let (mut backend, mut requests, responses) = Backend::scripted();
    let mut app = App::new("test".into());
    second_tab(&mut app);
    let first = Uuid::new_v4().to_string();
    let second = Uuid::new_v4().to_string();
    app.task = Some(first.clone());
    app.switch_tab(1);
    app.task = Some(second.clone());
    app.detail = "current data".into();

    for task in [&first, &second] {
        responses.try_send(Response {
            tag: "execution.event".into(),
            result: Ok(json!({"kind":"execution.state", "task_id":task, "payload":{"state":"succeeded"}})),
        }).unwrap();
        app.routed_response(backend.receive().unwrap(), &backend);
    }
    let refresh = requests.try_recv().unwrap();
    assert_eq!(refresh.method, "execution.get");
    assert_eq!(refresh.params["task_id"], second);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.task.as_deref(), Some(second.as_str()));
    assert_eq!(app.detail, "current data");

    app.task = Some(Uuid::new_v4().to_string());
    let current = app.task.clone();
    app.routed_response(
        Response {
            tag: refresh.tag,
            result: Ok(
                json!({"state":"succeeded", "method":"change.submit", "summary":{"items":[]}}),
            ),
        },
        &backend,
    );
    assert_eq!(app.task, current);
    assert_eq!(app.detail, "current data");
    app.request(&backend, "change.submit", "change.submit", json!({}));
    assert!(requests.try_recv().is_err());

    app.switch_tab(0);
    assert_eq!(app.task.as_deref(), Some(first.as_str()));
}

#[test]
fn task_diagnostics_loads_text_only_for_current_detail() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    app.detail_context.kind = "任务详情";
    app.detail = json!({"id":first,"text":null}).to_string();
    app.workbench_action("overlay:diagnostics", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "execution.get");
    assert_eq!(
        request.params,
        json!({"task_id":first.to_string(),"include_text":true})
    );
    app.detail = json!({"id":second,"text":null}).to_string();
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"id":first,"text":"old"})),
        },
        &backend,
    );
    assert_eq!(
        serde_json::from_str::<Value>(&app.detail).unwrap()["id"],
        json!(second)
    );

    app.workbench_action("overlay:diagnostics", &backend);
    let request = requests.try_recv().unwrap();
    app.detail_context.kind = "草稿详情";
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"id":second,"text":"wrong"})),
        },
        &backend,
    );
    assert!(!app.detail.contains("wrong"));

    app.detail_context.kind = "任务详情";
    app.workbench_action("overlay:diagnostics", &backend);
    let request = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"id":second,"text":"complete"})),
        },
        &backend,
    );
    assert_eq!(
        serde_json::from_str::<Value>(&app.detail).unwrap()["text"],
        "complete"
    );
}

#[test]
fn terminal_event_before_start_response_keeps_guard_and_poll_fallback() {
    let (mut backend, mut requests, responses) = Backend::scripted();
    let mut app = App::new("test".into());
    let task = Uuid::new_v4().to_string();
    app.request(&backend, "execution.start", "execution.start", json!({}));
    let start = requests.try_recv().unwrap();
    responses
        .try_send(Response {
            tag: "execution.event".into(),
            result: Ok(
                json!({"kind":"execution.state", "task_id":task, "payload":{"state":"succeeded"}}),
            ),
        })
        .unwrap();
    app.routed_response(backend.receive().unwrap(), &backend);
    assert!(app.operation_pending);
    assert!(requests.try_recv().is_err());
    app.request(&backend, "change.submit", "change.submit", json!({}));
    assert!(requests.try_recv().is_err());

    app.routed_response(
        Response {
            tag: start.tag,
            result: Ok(json!({"task_id":task})),
        },
        &backend,
    );
    assert_eq!(app.task.as_deref(), Some(task.as_str()));
    assert!(app.last_poll.elapsed() >= Duration::from_millis(700));
    app.request(&backend, "change.submit", "change.submit", json!({}));
    assert!(requests.try_recv().is_err());
}

fn ui_profile(provider: &str, name: &str) -> Profile {
    serde_json::from_value(json!({
        "id":Uuid::new_v4(),"name":name,"provider":provider,"host":"127.0.0.1",
        "port":if provider == "mysql" {3306} else {6379},"access_mode":"read_write"
    }))
    .unwrap()
}

fn inline_app() -> App {
    let mut app = app_with_grid_draft();
    app.grid.as_mut().unwrap().discard();
    app.profile = Some(ui_profile("mysql", "inline"));
    app.edit_token = Some(json!(Uuid::new_v4()));
    app.browse_result = true;
    app.schema = Some(json!({"engine":"InnoDB","keys":[["name"]]}));
    app
}

#[test]
fn reconnect_keeps_unconfirmed_text_attached_to_its_original_row() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = inline_app();
    let original_session = app.session_id();
    app.begin_inline(&backend);
    app.field_set("cell", "保持原行😀");
    app.action("open", &backend);
    assert!(app.workbench.inline.is_none());
    assert_eq!(
        app.grid.as_ref().unwrap().value(Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("保持原行😀".into()))
    );
    assert_eq!(app.session_id(), original_session);
    assert!(requests.try_recv().is_err(), "存在草稿时不得关闭并重连会话");
}

#[test]
fn paging_confirms_inline_before_changing_absolute_row_positions() {
    let (backend, _, _) = Backend::scripted();
    let mut app = inline_app();
    let grid = app.grid.as_mut().unwrap();
    grid.page.result.row_count = 2;
    grid.page.next = Some(1);
    app.result_id = Some(grid.page.result.id.to_string());
    let mut next = grid.page.clone();
    next.start = 1;
    next.next = None;
    next.rows = vec![vec![CellValue::Text("另一行".into())]];
    app.begin_inline(&backend);
    app.field_set("cell", "只修改旧行");
    app.action("next", &backend);
    app.response(
        Response {
            tag: "result.page".into(),
            result: Ok(serde_json::to_value(next).unwrap()),
        },
        &backend,
    );
    let grid = app.grid.as_ref().unwrap();
    assert!(app.workbench.inline.is_none());
    assert_eq!(
        grid.edits().get(&Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("只修改旧行".into()))
    );
    assert_eq!(
        grid.value(Cell { row: 1, column: 0 }),
        Some(&CellValue::Text("另一行".into()))
    );
}

#[test]
fn late_connection_reply_does_not_replace_a_newly_selected_connection() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("test".into());
    let first = ui_profile("mysql", "first");
    let second = ui_profile("redis", "second");
    app.profiles = vec![first.clone(), second.clone()];
    app.profile = Some(first.clone());
    app.tabs[0].profile_id = Some(first.id);
    app.operation_pending = true;
    let first_tab = app.tabs[0].id;
    app.select_profile(1, &backend);
    assert_eq!(app.tabs.len(), 2);
    app.routed_response(
        Response {
            tag: format!("{first_tab}:0:connection.open"),
            result: Ok(json!({"session_id":Uuid::new_v4(),"profile":first})),
        },
        &backend,
    );
    assert_eq!(app.active, 1);
    assert_eq!(app.profile.as_ref().unwrap().id, second.id);
    assert!(app.tabs[0].runtime.session.is_some());
}

#[test]
fn cli_text_selection_and_visibility_survive_workspace_reopen() {
    let mut app = App::new("test".into());
    second_tab(&mut app);
    app.field_set("cli_text", "PING\nECHO 中文😀");
    let end = app.field("cli_text").encode_utf16().count();
    app.editor("cli_text").unwrap().set_selection(5, end);
    app.workbench.cli = true;
    app.switch_tab(1);
    app.field_set("cli_text", "GET user:1042");
    let saved: Workspace =
        serde_json::from_value(serde_json::to_value(app.workspace_snapshot().unwrap()).unwrap())
            .unwrap();
    let mut reopened = App::new("test".into());
    reopened.tabs = saved.tabs;
    reopened.active = saved.active;
    reopened.restore_browse();
    assert_eq!(reopened.field("cli_text"), "GET user:1042");
    reopened.switch_tab(0);
    assert_eq!(reopened.field("cli_text"), "PING\nECHO 中文😀");
    assert_eq!(
        reopened.tabs[0].cli_query.selected_text(),
        Some("ECHO 中文😀")
    );
    assert!(reopened.workbench.cli);
}

#[test]
fn redis_workspace_migrates_the_previous_command_editor_without_losing_text() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("test".into());
    let profile = ui_profile("redis", "legacy");
    app.tabs[0].profile_id = Some(profile.id);
    app.tabs[0].query = Editor::new("PING\nECHO 保存😀");
    app.response(
        Response {
            tag: "profile.list".into(),
            result: Ok(json!({"profiles":[profile]})),
        },
        &backend,
    );
    assert_eq!(app.field("cli_text"), "PING\nECHO 保存😀");
    assert!(app.tabs[0].query.text.is_empty());
    assert!(app.tabs[0].browse.cli_visible);
}

#[test]
fn automatic_column_resize_preserves_manual_width_and_frozen_offsets() {
    let mut grid = editable_grid();
    grid.page
        .result
        .columns
        .push(Column::new("description", "text"));
    grid.page.rows[0].push(CellValue::Text("a longer description".into()));
    let mut grid = GridState::new(grid.page, true);
    grid.fit_columns(700, true);
    assert_eq!(
        grid.settings
            .iter()
            .map(|setting| u32::from(setting.width))
            .sum::<u32>(),
        700
    );
    let setting = grid::ColumnSetting {
        width: 180,
        frozen: true,
        ..grid.settings[0].clone()
    };
    grid.set_column(0, setting).unwrap();
    grid.settings[1].frozen = true;
    grid.fit_columns(900, true);
    assert_eq!(grid.settings[0].width, 180);
    assert_eq!(grid.settings[1].width, 480);
    assert_eq!(grid.frozen_offset(1), Some(180));
    grid.fit_columns(100, true);
    assert_eq!(grid.settings[0].width, 180);
    assert!(grid.settings[1].width >= 64);
    assert_eq!(grid.frozen_offset(1), Some(180));
    assert_eq!(
        grid.value(Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("before".into()))
    );
}

#[test]
fn saved_explicit_column_width_survives_viewport_changes() {
    let mut grid = editable_grid();
    let saved: grid::ColumnSetting =
        serde_json::from_value(json!({"width":247,"hidden":false,"frozen":true})).unwrap();
    grid.settings[0] = saved;
    grid.fit_columns(800, true);
    assert_eq!(grid.settings[0].width, 247);
    grid.fit_columns(320, true);
    assert_eq!(grid.settings[0].width, 247);
    assert_eq!(grid.frozen_offset(0), Some(0));
    assert!(grid.select(Cell { row: 0, column: 0 }, false).is_ok());
    assert_eq!(grid.copy_tsv().unwrap(), "before\n");
}

#[test]
fn cell_detail_keeps_its_write_target_when_the_grid_selection_changes() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("test".into());
    let mut grid = editable_grid();
    grid.page
        .rows
        .push(vec![CellValue::Text("another row".into())]);
    grid.page.result.row_count = 2;
    app.grid = Some(grid);
    app.open_cell(&backend);
    app.field_set("cell", "edited in the drawer");
    app.grid
        .as_mut()
        .unwrap()
        .select(Cell { row: 1, column: 0 }, false)
        .unwrap();
    app.action("set-cell", &backend);

    let grid = app.grid.as_ref().unwrap();
    assert_eq!(
        grid.value(Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("edited in the drawer".into()))
    );
    assert_eq!(
        grid.value(Cell { row: 1, column: 0 }),
        Some(&CellValue::Text("another row".into()))
    );
}

#[test]
fn cell_detail_cannot_write_to_a_replacement_result_at_the_same_coordinates() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.grid = Some(editable_grid());
    app.open_cell(&backend);
    app.field_set("cell", "from the previous result");
    app.grid = Some(editable_grid());
    app.action("set-cell", &backend);

    assert_eq!(
        app.grid.as_ref().unwrap().value(Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("before".into()))
    );
}

#[test]
fn cell_detail_input_and_target_follow_their_tab() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("test".into());
    second_tab(&mut app);
    app.grid = Some(editable_grid());
    app.open_cell(&backend);
    app.field_set("cell", "first tab");
    app.switch_tab(1);
    app.grid = Some(editable_grid());
    app.open_cell(&backend);
    app.field_set("cell", "second tab");
    app.switch_tab(0);
    app.action("set-cell", &backend);
    assert_eq!(
        app.grid.as_ref().unwrap().value(Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("first tab".into()))
    );
    app.switch_tab(1);
    app.action("set-cell", &backend);
    assert_eq!(
        app.grid.as_ref().unwrap().value(Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("second tab".into()))
    );
}

#[test]
fn selecting_resource_tab_leaves_history_without_losing_result_or_draft() {
    let mut app = app_with_grid_draft();
    let result = app.grid.as_ref().unwrap().page.result.id;
    app.workbench.overlay = Some("history".into());
    app.select_tab(0);
    assert!(app.workbench.overlay.is_none());
    assert_eq!(app.grid.as_ref().unwrap().page.result.id, result);
    assert!(app.has_live_drafts());

    second_tab(&mut app);
    app.workbench.overlay = Some("history".into());
    app.select_tab(1);
    app.select_tab(0);
    assert_eq!(
        app.grid.as_ref().unwrap().value(Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("draft".into()))
    );
}

#[test]
fn refreshing_database_tree_reloads_expanded_objects_and_ignores_old_catalog() {
    fn page(name: &str, kind: &str) -> Value {
        json!({"resources":[{"name":name,"kind":kind}],"cursor":null,"complete":true})
    }
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("catalog-refresh".into());
    let profile = ui_profile("mysql", "catalog");
    app.profiles = vec![profile.clone()];
    app.profile = Some(profile.clone());
    app.tabs[0].profile_id = Some(profile.id);
    app.session = Some(json!({"session_id":"catalog-session"}));
    app.navigation_load_tree(&backend, false);
    let root = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: root.tag,
            result: Ok(page("commerce", "database")),
        },
        &backend,
    );
    app.navigation_action("navigation-db:commerce", &backend);
    let old = requests.try_recv().unwrap();
    app.navigation_load_tree(&backend, true);
    while let Ok(request) = requests.try_recv() {
        let result = if request.params["database"].as_str() == Some("commerce") {
            page("fresh_table", "table")
        } else {
            page("commerce", "database")
        };
        app.navigation_response(
            &Response {
                tag: request.tag,
                result: Ok(result),
            },
            &backend,
        );
    }
    app.navigation_response(
        &Response {
            tag: old.tag,
            result: Ok(page("stale_table", "table")),
        },
        &backend,
    );
    let node = &app.navigation.databases[&profile.id].nodes["commerce"];
    assert!(node.expanded);
    assert_eq!(
        node.resources
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["fresh_table"]
    );
}

#[test]
fn reopening_disconnected_resource_tab_reconnects_before_loading() {
    for from_tree in [false, true] {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = App::new("reopen-resource".into());
        let profile = ui_profile("mysql", "restored");
        let target = ResourceTarget {
            database: Some("commerce".into()),
            name: "customers".into(),
            key_b64: None,
            kind: "table".into(),
        };
        app.profiles = vec![profile.clone()];
        app.profile = Some(profile.clone());
        app.tabs[0].profile_id = Some(profile.id);
        app.tabs[0].query = Editor::new("SELECT '草稿😀'");
        app.target = Some(target);
        app.database = Some("commerce".into());
        second_tab(&mut app);
        app.switch_tab(1);
        let saved: Workspace = serde_json::from_value(
            serde_json::to_value(app.workspace_snapshot().unwrap()).unwrap(),
        )
        .unwrap();
        app = App::new("reopen-resource".into());
        app.tabs = saved.tabs;
        app.active = saved.active;
        app.profiles = vec![profile.clone()];
        app.restore_browse();
        app.profile = Some(profile.clone());
        app.tabs[1].profile_id = Some(profile.id);
        app.session = Some(json!({"session_id": Uuid::new_v4()}));
        app.resources = vec![
            serde_json::from_value(json!({
                "name":"customers", "kind":"table", "database":"commerce"
            }))
            .unwrap(),
        ];
        if from_tree {
            app.navigation
                .databases
                .entry(profile.id)
                .or_default()
                .nodes
                .entry("commerce".into())
                .or_default()
                .resources = app.resources.clone();
            app.navigation_action("navigation-resource:commerce:0", &backend);
        } else {
            app.select_resource(0, &backend);
        }
        assert_eq!(app.active, 0);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.tabs[0].query.text, "SELECT '草稿😀'");
        let reconnect = requests
            .try_recv()
            .expect("restored resource must reconnect");
        assert_eq!(reconnect.method, "connection.open");
        app.routed_response(
            Response {
                tag: reconnect.tag,
                result: Ok(json!({"session_id":Uuid::new_v4(),"profile":profile})),
            },
            &backend,
        );
        let mut described = false;
        while let Ok(request) = requests.try_recv() {
            if request.method == "resource.describe" {
                assert_eq!(request.params["target"]["database"], "commerce");
                assert_eq!(request.params["target"]["name"], "customers");
                described = true;
            }
        }
        assert!(described, "reconnection must load the original resource");
        assert_eq!(app.tabs[0].query.text, "SELECT '草稿😀'");
    }
}

#[test]
fn workspace_load_keeps_edits_made_while_loading_and_saved_tabs() {
    let (backend, _, _) = Backend::scripted();
    let mut saved = App::new("saved".into());
    saved.tabs[0].query = Editor::new("SELECT saved");
    let state = serde_json::to_value(saved.workspace_snapshot().unwrap()).unwrap();
    let mut app = App::new("loading".into());
    app.tabs[0].query = Editor::new("SELECT local");
    app.changed_field("query");
    app.routed_response(
        Response {
            tag: "workspace.load".into(),
            result: Ok(json!({"revision": 7, "state": state})),
        },
        &backend,
    );
    assert_eq!(app.tabs[app.active].query.text, "SELECT local");
    assert!(app.tabs.iter().any(|tab| tab.query.text == "SELECT saved"));
    assert!(app.dirty && app.workspace_loaded);
    assert_eq!(app.revision, 7);
}

#[test]
fn current_statement_uses_text_and_cursor_at_request_time() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("current".into());
    app.profile = Some(ui_profile("mysql", "mysql"));
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.tabs[0].query = Editor::new("SELECT 1; SELECT 2;");
    app.tabs[0].query.set_selection(2, 2);
    app.action("current", &backend);
    let request = requests.try_recv().unwrap();
    app.tabs[0].query = Editor::new("SELECT 3; SELECT 4;");
    app.changed_field("query");
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"sql_mode":""})),
        },
        &backend,
    );
    let execution = requests.try_recv().unwrap();
    assert_eq!(execution.method, "execution.start");
    assert_eq!(execution.params["text"], "SELECT 1");
}

#[test]
fn file_open_ignores_stale_responses_and_preserves_intervening_edits() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("file".into());
    app.tabs[0].query = Editor::new("original");
    app.field_set("path", "query.sql");
    app.action("file-open", &backend);
    let first = requests.try_recv().unwrap();
    app.action("file-open", &backend);
    let second = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: first.tag,
            result: Ok(json!({"text":"stale"})),
        },
        &backend,
    );
    assert_eq!(app.tabs[0].query.text, "original");
    app.tabs[0].query = Editor::new("edited");
    app.changed_field("query");
    app.routed_response(
        Response {
            tag: second.tag,
            result: Ok(json!({"text":"file"})),
        },
        &backend,
    );
    assert_eq!(app.tabs[0].query.text, "edited");
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
fn redis_new_and_restored_tabs_reconnect_to_their_selected_database() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("redis-db".into());
    let mut profile = ui_profile("redis", "redis");
    profile.redis_db = 5;
    app.profile = Some(profile.clone());
    app.profiles = vec![profile.clone()];
    app.tabs[0].profile_id = Some(profile.id);
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.open_query_tab(&backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "connection.open");
    assert_eq!(request.params["redis_db"], 5);
    let saved: Workspace =
        serde_json::from_value(serde_json::to_value(app.workspace_snapshot().unwrap()).unwrap())
            .unwrap();
    let mut restored = App::new("redis-db".into());
    restored.tabs = saved.tabs;
    restored.active = saved.active;
    profile.redis_db = 0;
    restored.profiles = vec![profile.clone()];
    restored.profile = Some(profile);
    restored.restore_browse();
    restored.action("open", &backend);
    let reconnect = requests.try_recv().unwrap();
    assert_eq!(reconnect.method, "connection.open");
    assert_eq!(reconnect.params["redis_db"], 5);
}

#[test]
fn modal_dispatch_rejects_hidden_fields_and_accepts_its_own_fields() {
    let mut app = App::new("modal".into());
    app.field_set("pattern", "base");
    app.dispatch_editor_message(Msg::Edit("pattern".into(), 4, 4, "!".into(), 5, 4));
    app.focus = "pattern".into();
    app.workbench.overlay = Some("settings".into());
    app.reconcile_modal_focus();
    assert_eq!(app.workbench.focus_request.as_deref(), Some(""));
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
fn restored_tab_focus_follows_profile_arrival_order() {
    let (backend, _, _) = Backend::scripted();
    for profiles_first in [false, true] {
        let profile = ui_profile("redis", "redis");
        let mut saved = App::new("saved".into());
        saved.profile = Some(profile.clone());
        saved.tabs[0].profile_id = Some(profile.id);
        saved.tabs[0].cli_query = Editor::new("GET");
        saved.workbench.cli = true;
        let state = serde_json::to_value(saved.workspace_snapshot().unwrap()).unwrap();
        let mut app = App::new("restored".into());
        let profiles = || Response {
            tag: "profile.list".into(),
            result: Ok(json!({"profiles":[profile.clone()]})),
        };
        if profiles_first {
            app.routed_response(profiles(), &backend);
        }
        app.routed_response(
            Response {
                tag: "workspace.load".into(),
                result: Ok(json!({"revision":1,"state":state})),
            },
            &backend,
        );
        if !profiles_first {
            app.routed_response(profiles(), &backend);
        }
        assert_eq!(app.focus, "cli_text");
        let mut key = tern_sdk::Key::named("x");
        key.text = Some(" x".into());
        app.input_key(&key);
        assert_eq!(app.tabs[0].cli_query.text, "GET x");
        assert!(app.tabs[0].query.text.is_empty());
    }

    let mut saved = App::new("saved".into());
    saved.target = Some(ResourceTarget {
        database: Some("db".into()),
        name: "t".into(),
        key_b64: None,
        kind: "table".into(),
    });
    let state = serde_json::to_value(saved.workspace_snapshot().unwrap()).unwrap();
    let mut app = App::new("restored".into());
    app.routed_response(
        Response {
            tag: "workspace.load".into(),
            result: Ok(json!({"revision":1,"state":state})),
        },
        &backend,
    );
    assert_eq!(app.focus, "grid");

    let mut interacting = App::new("interacting".into());
    interacting.focus = "pattern".into();
    interacting.workspace_interacted = true;
    let state = serde_json::to_value(saved.workspace_snapshot().unwrap()).unwrap();
    interacting.routed_response(
        Response {
            tag: "workspace.load".into(),
            result: Ok(json!({"revision":1,"state":state})),
        },
        &backend,
    );
    assert_eq!(interacting.focus, "pattern");
}

#[test]
fn opening_query_file_invalidates_query_results_but_preserves_browse_grid() {
    for browsing in [false, true] {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = App::new("file-results".into());
        app.grid = Some(editable_grid());
        app.browse_result = browsing;
        app.result_id = Some("old".into());
        app.results.push("old".into());
        app.page_token = Some(Uuid::new_v4());
        app.field_set("path", "query.sql");
        app.action("file-open", &backend);
        let request = requests.try_recv().unwrap();
        app.routed_response(
            Response {
                tag: request.tag,
                result: Ok(json!({"text":"SELECT 2"})),
            },
            &backend,
        );
        assert_eq!(app.tabs[0].query.text, "SELECT 2");
        if browsing {
            assert!(app.grid.is_some());
            assert!(app.page_token.is_some());
        } else {
            assert!(app.grid.is_none());
            assert!(app.results.is_empty() && app.result_id.is_none() && app.page_token.is_none());
        }
    }
}

#[test]
fn tls_recovery_does_not_reopen_settings_after_a_transport_failure() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("tls-recovery".into());
    app.profile = Some(ui_profile("mysql", "secure"));
    app.action("open", &backend);
    let open = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: open.tag,
            result: Err(AppError::new("tls_required", "3159")),
        },
        &backend,
    );
    app.action("connection-enable-tls", &backend);
    assert_eq!(app.workbench.overlay.as_deref(), Some("profile"));
    assert_eq!(app.field("tls"), "true");
    app.action("overlay-close", &backend);
    drop(requests);
    app.action("open", &backend);
    assert!(app.workbench.connection_error.is_some());
    app.action("connection-enable-tls", &backend);
    assert!(app.workbench.overlay.is_none());
}

#[test]
fn escape_dismisses_redis_db_picker_before_its_connection_form() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("db-picker-escape".into());
    let escape = tern_sdk::Key::named("escape");
    app.navigation.redis_db_menu = true;
    assert!(app.workbench_key(&escape, &backend));
    assert!(!app.navigation.redis_db_menu);
    app.workbench.overlay = Some("profile".into());
    app.navigation.form_redis_db_menu = true;
    assert!(app.workbench_key(&escape, &backend));
    assert!(!app.navigation.form_redis_db_menu);
    assert_eq!(app.workbench.overlay.as_deref(), Some("profile"));
    assert!(app.workbench_key(&escape, &backend));
    assert!(app.workbench.overlay.is_none());
}

#[test]
fn change_preview_rejects_changed_drafts_and_pages_real_plan() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("preview".into());
    app.profile_form(None);
    app.field_set("provider", "redis");
    app.profile = Some(app.form_profile().unwrap());
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.target = Some(ResourceTarget {
        database: Some("0".into()),
        name: "key".into(),
        key_b64: None,
        kind: "string".into(),
    });
    app.edit_token = Some(json!(Uuid::new_v4()));
    app.redis_changes = vec![json!({"operation":"delete_key"})];
    app.workbench.overlay = None;
    app.action("overlay:preview", &backend);
    let stale = requests.try_recv().unwrap();
    assert_eq!(stale.method, "change.preview");
    app.redis_changes
        .push(json!({"operation":"set_ttl","ttl_ms":42}));
    app.routed_response(
        Response {
            tag: stale.tag,
            result: Ok(json!({"offset":0,"next_offset":20})),
        },
        &backend,
    );
    assert!(app.change_preview_value().is_none());
    app.action("overlay:preview", &backend);
    let fresh = requests.try_recv().unwrap();
    assert_eq!(fresh.params["changes"].as_array().unwrap().len(), 2);
    app.routed_response(
        Response {
            tag: fresh.tag,
            result: Ok(json!({"offset":0,"next_offset":20,"items":[]})),
        },
        &backend,
    );
    assert!(app.change_preview_value().is_some());
    app.action("change-preview-next", &backend);
    let page = requests.try_recv().unwrap();
    assert_eq!(page.params["offset"], 20);
    app.routed_response(
        Response {
            tag: page.tag,
            result: Ok(json!({"offset":20,"next_offset":null,"items":[]})),
        },
        &backend,
    );
    app.action("change-preview-prev", &backend);
    assert_eq!(requests.try_recv().unwrap().params["offset"], 0);
}
