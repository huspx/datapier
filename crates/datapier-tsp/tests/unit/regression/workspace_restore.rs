use super::*;

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

#[test]
fn saved_grid_edit_disappears_from_snapshot_after_undo() {
    let mut app = App::new("test".into());
    let mut grid = editable_grid();
    grid.edit(Cell { row: 0, column: 0 }, CellValue::Text("after".into()))
        .unwrap();
    app.grid = Some(grid);

    let saved = app.workspace_snapshot();
    assert!(!saved.tabs[0].pending.is_null());
    assert!(app.grid.as_mut().unwrap().undo());
    let undone = app.workspace_snapshot();
    assert!(undone.tabs[0].pending.is_null());
}

#[test]
fn restored_pending_survives_an_unrelated_snapshot() {
    let mut original = App::new("test".into());
    original
        .redis_changes
        .push(json!({"operation": "delete_key"}));
    let saved = original.workspace_snapshot();
    let persisted = serde_json::to_value(saved).unwrap();
    let loaded: Workspace = serde_json::from_value(persisted).unwrap();
    let pending = loaded.tabs[0].pending.clone();
    assert!(!pending.is_null());

    let mut reopened = App::new("test".into());
    reopened.tabs = loaded.tabs;
    reopened.active = loaded.active;
    assert_eq!(reopened.workspace_snapshot().tabs[0].pending, pending);
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

    let serialized = serde_json::to_value(app.workspace_snapshot()).unwrap();
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
fn workspace_omits_redis_credentials_without_changing_in_memory_drafts() {
    let mut app = App::new("auth-draft".into());
    let redis = ui_profile("redis", "redis");
    app.tabs[0].profile_id = Some(redis.id);
    app.profiles.push(redis);
    app.tabs[0].query = Editor::new("AUTH default old-private-marker");
    app.tabs[0].cli_query = Editor::new("HELLO 3 AUTH default new-private-marker");

    let saved = app.workspace_snapshot();
    assert!(saved.tabs[0].query.text.is_empty());
    assert!(saved.tabs[0].cli_query.text.is_empty());
    assert_eq!(app.tabs[0].query.text, "AUTH default old-private-marker");
    assert_eq!(
        app.tabs[0].cli_query.text,
        "HELLO 3 AUTH default new-private-marker"
    );

    app.tabs[0].cli_query = Editor::new("GET ordinary-key");
    let saved = app.workspace_snapshot();
    assert_eq!(saved.tabs[0].cli_query.text, "GET ordinary-key");
}

#[test]
fn workspace_load_keeps_edits_made_while_loading_and_saved_tabs() {
    let (backend, _, _) = Backend::scripted();
    let mut saved = App::new("saved".into());
    saved.tabs[0].query = Editor::new("SELECT saved");
    let state = serde_json::to_value(saved.workspace_snapshot()).unwrap();
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
