use super::*;
use datapier_core::model::{Column, ResultMeta, RowChange, Value as CellValue};

fn second_tab(app: &mut App) {
    app.tabs.push(Tab {
        title: "second".into(),
        query: Editor::new(""),
        profile_id: None,
        id: Uuid::new_v4(),
        layout: Vec::new(),
        browse: BrowseSettings::default(),
        pending: Value::Null,
        pending_live: false,
        runtime: TabRuntime::default(),
    });
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
