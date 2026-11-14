use super::*;

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
    assert!(!app.workspace_snapshot().tabs[0].pending.is_null());
    app.pending_rows.clear();
    app.redis_changes.clear();
    app.switch_tab(1);

    let snapshot = app.workspace_snapshot();
    assert!(snapshot.tabs[0].pending.is_null());
    assert!(snapshot.tabs[1].pending.is_null());
}

#[test]
fn schema_event_invalidates_matching_background_tabs_without_losing_drafts() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("schema-event".into());
    let profile = ui_profile("mysql", "source");
    app.profile = Some(profile.clone());
    app.tabs[0].profile_id = Some(profile.id);
    app.database = Some("db".into());
    app.schema = Some(json!({"old":true}));
    app.edit_token = Some(json!("old-token"));
    app.pending_rows.push(RowChange {
        operation: "insert".into(),
        key: BTreeMap::new(),
        before: BTreeMap::new(),
        values: BTreeMap::new(),
    });
    second_tab(&mut app);
    app.switch_tab(1);
    app.tabs[1].profile_id = Some(profile.id);
    app.profile = Some(profile.clone());
    app.database = Some("other".into());
    app.schema = Some(json!({"other":true}));
    app.routed_response(Response { tag: "execution.event".into(), result: Ok(json!({
        "kind":"schema.changed", "payload":{"id":Uuid::new_v4(),"revision":1,"state":"invalidated","scope":{"profile_id":profile.id,"database":"db"}}
    })) }, &backend);
    assert_eq!(app.schema, Some(json!({"other":true})));
    app.switch_tab(0);
    assert!(app.schema.is_none());
    assert!(app.edit_token.is_none());
    assert_eq!(app.pending_rows.len(), 1);
}

#[test]
fn draft_scope_requires_matching_ack_and_releases_after_discard() {
    let (backend, mut requests, _responses) = Backend::scripted();
    let mut app = App::new("dirty-scope".into());
    let profile = ui_profile("mysql", "source");
    app.tabs[0].profile_id = Some(profile.id);
    app.profile = Some(profile);
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.edit_token = Some(json!(Uuid::new_v4()));
    app.target = Some(ResourceTarget {
        database: Some("db".into()),
        name: "items".into(),
        kind: "table".into(),
        key_b64: None,
    });
    app.sync_draft_scope(&backend);
    assert!(requests.try_recv().is_err());
    assert!(!app.guard_draft_scope(&backend, workbench::DraftScopeAction::Protect));
    let dirty = requests.try_recv().unwrap();
    assert_eq!(dirty.params["dirty"], true);
    assert_eq!(dirty.params["edit_token"], app.edit_token.clone().unwrap());
    assert!(app.draft_scope_save_check().is_err());
    app.routed_response(
        Response {
            tag: dirty.tag.clone(),
            result: Ok(json!({"revision":dirty.params["revision"]})),
        },
        &backend,
    );
    assert!(app.workbench.draft_scope.is_some());
    assert!(app.draft_scope_save_check().is_ok());
    app.sync_draft_scope(&backend);
    let clean = requests.try_recv().unwrap();
    assert_eq!(clean.params["dirty"], false);
    assert!(clean.params["revision"].as_u64() > dirty.params["revision"].as_u64());
}

#[test]
fn draft_scope_rejection_and_late_ack_never_authorize_new_target() {
    let (backend, mut requests, _responses) = Backend::scripted();
    let mut app = App::new("scope-rejection".into());
    let profile = ui_profile("mysql", "source");
    app.tabs[0].profile_id = Some(profile.id);
    app.profile = Some(profile);
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.edit_token = Some(json!(Uuid::new_v4()));
    app.target = Some(ResourceTarget {
        database: Some("db".into()),
        name: "items".into(),
        kind: "table".into(),
        key_b64: None,
    });
    assert!(!app.guard_draft_scope(&backend, workbench::DraftScopeAction::Protect));
    let first = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: first.tag.clone(),
            result: Err(AppError::new("schema_conflict", "stale baseline")),
        },
        &backend,
    );
    assert_eq!(
        app.draft_scope_save_check().unwrap_err().code,
        "schema_conflict"
    );
    assert!(app.workbench.draft_scope.is_none());
    assert!(!app.guard_draft_scope(&backend, workbench::DraftScopeAction::Protect));
    let retry = requests.try_recv().unwrap();
    app.target.as_mut().unwrap().name = "other".into();
    app.sync_draft_scope(&backend);
    let release = requests.try_recv().unwrap();
    assert_eq!(release.params["dirty"], false);
    app.routed_response(
        Response {
            tag: retry.tag,
            result: Ok(json!({"revision":retry.params["revision"]})),
        },
        &backend,
    );
    assert!(app.workbench.draft_scope.is_none());
    assert!(!app.guard_draft_scope(&backend, workbench::DraftScopeAction::Protect));
    let next = requests.try_recv().unwrap();
    assert_eq!(next.params["target"]["name"], "other");
}

#[test]
fn native_row_input_waits_for_scope_ack() {
    let (backend, mut requests, _responses) = Backend::scripted();
    let mut app = App::new("scope-input".into());
    let profile = ui_profile("mysql", "source");
    app.tabs[0].profile_id = Some(profile.id);
    app.profile = Some(profile);
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.edit_token = Some(json!(Uuid::new_v4()));
    app.target = Some(ResourceTarget {
        database: Some("db".into()),
        name: "items".into(),
        kind: "table".into(),
        key_b64: None,
    });
    app.field_set("row_values", "");
    app.dispatch_guarded_editor_message(
        Msg::Edit("row_values".into(), 0, 0, "x".into(), 1, 0),
        &backend,
    );
    app.dispatch_guarded_editor_message(
        Msg::Edit("row_values".into(), 1, 1, "y".into(), 2, 1),
        &backend,
    );
    assert_eq!(
        app.workbench
            .draft_scope_pending
            .as_ref()
            .unwrap()
            .actions
            .len(),
        1
    );
    let request = requests.try_recv().unwrap();
    assert_eq!(app.field("row_values"), "");
    assert!(app.draft_scope_save_check().is_err());
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"revision":request.params["revision"]})),
        },
        &backend,
    );
    assert_eq!(app.field("row_values"), "xy");
}

#[test]
fn rejected_input_survives_target_change_for_manual_recovery() {
    let (backend, mut requests, _responses) = Backend::scripted();
    let mut app = App::new("scope-recovery".into());
    let profile = ui_profile("mysql", "source");
    app.tabs[0].profile_id = Some(profile.id);
    app.profile = Some(profile);
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.edit_token = Some(json!(Uuid::new_v4()));
    app.target = Some(ResourceTarget {
        database: Some("db".into()),
        name: "items".into(),
        kind: "table".into(),
        key_b64: None,
    });
    app.field_set("row_values", "");
    app.dispatch_guarded_editor_message(
        Msg::Edit("row_values".into(), 0, 0, "retain me".into(), 9, 0),
        &backend,
    );
    let request = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: request.tag,
            result: Err(AppError::new("schema_conflict", "stale")),
        },
        &backend,
    );
    app.target.as_mut().unwrap().name = "other".into();
    app.sync_draft_scope(&backend);
    assert!(app.draft_scope_save_check().is_err());
    app.action("draft-scope-recovery-view", &backend);
    assert!(app.detail.contains("retain me"));
    app.action("draft-scope-recovery-discard", &backend);
    assert!(app.workbench.draft_scope_recovery.is_empty());
}

#[test]
fn delete_confirmation_waits_for_registration_and_cancel_tombstones_it() {
    let (backend, mut requests, _responses) = Backend::scripted();
    let mut app = App::new("scope-delete".into());
    let profile = ui_profile("mysql", "source");
    app.tabs[0].profile_id = Some(profile.id);
    app.profile = Some(profile);
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.edit_token = Some(json!(Uuid::new_v4()));
    app.target = Some(ResourceTarget {
        database: Some("db".into()),
        name: "items".into(),
        kind: "table".into(),
        key_b64: None,
    });
    app.workbench.overlay = Some("delete-rows".into());
    app.action("delete-rows-confirm", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(app.workbench.overlay.as_deref(), Some("delete-rows"));
    assert!(app.pending_rows.is_empty());
    app.cancel_pending_draft_scope(&backend);
    let release = requests.try_recv().unwrap();
    assert_eq!(release.params["dirty"], false);
    assert!(release.params["revision"].as_u64() > request.params["revision"].as_u64());
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"revision":request.params["revision"]})),
        },
        &backend,
    );
    assert!(app.workbench.draft_scope.is_none());
    assert!(app.pending_rows.is_empty());
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
fn grid_draft_prevents_query_execution_request() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_with_grid_draft();
    app.action("run", &backend);
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
fn successful_submission_retracts_stale_rows_and_retains_column_layout() {
    for (operation, status) in [
        (None, "applied"),
        (None, "unchanged"),
        (Some("rename"), "applied"),
        (Some("delete_key"), "applied"),
    ] {
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
                    "summary": {"items": [{"status": status}]}
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
fn incomplete_submission_retains_drafts() {
    for status in ["failed", "outcome_unknown"] {
        let (backend, _, _) = Backend::scripted();
        let mut app = app_with_grid_draft();
        app.response(
            Response {
                tag: "execution.get".into(),
                result: Ok(json!({
                    "state":"succeeded", "method":"change.submit", "results":[],
                    "summary":{"items":[{"status":"unchanged"},{"status":status}]}
                })),
            },
            &backend,
        );
        assert!(app.has_live_drafts());
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

#[test]
fn reconnect_keeps_unconfirmed_text_attached_to_its_original_row() {
    let (backend, mut requests, _responses) = Backend::scripted();
    let mut app = inline_app();
    let original_session = app.session_id();
    app.begin_inline(&backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "workspace.draft_scope");
    assert!(app.workbench.inline.is_none());
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"revision":request.params["revision"]})),
        },
        &backend,
    );
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
fn paging_preserves_inline_draft_and_blocks_page_change() {
    let (backend, mut requests, _responses) = Backend::scripted();
    let mut app = inline_app();
    let grid = app.grid.as_mut().unwrap();
    grid.page.result.row_count = 2;
    grid.page.next = Some(1);
    app.result_id = Some(grid.page.result.id.to_string());
    app.begin_inline(&backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "workspace.draft_scope");
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"revision":request.params["revision"]})),
        },
        &backend,
    );
    app.field_set("cell", "只修改旧行");
    app.action("next", &backend);
    let grid = app.grid.as_ref().unwrap();
    assert!(app.workbench.inline.is_none());
    assert_eq!(
        grid.edits().get(&Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("只修改旧行".into()))
    );
    assert_eq!(grid.page.start, 0);
    assert_eq!(
        grid.value(Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("只修改旧行".into()))
    );
    assert!(requests.try_recv().is_err(), "存在草稿时不得翻页或写库");
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
fn redis_new_and_restored_tabs_reconnect_to_their_selected_database() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("redis-db".into());
    let mut profile = ui_profile("redis", "redis");
    profile.direct_mut().unwrap().redis_db = 5;
    app.profile = Some(profile.clone());
    app.profiles = vec![profile.clone()];
    app.tabs[0].profile_id = Some(profile.id);
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.open_query_tab(&backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "connection.open");
    assert_eq!(request.params["redis_db"], 5);
    let saved: Workspace =
        serde_json::from_value(serde_json::to_value(app.workspace_snapshot()).unwrap()).unwrap();
    let mut restored = App::new("redis-db".into());
    restored.tabs = saved.tabs;
    restored.active = saved.active;
    profile.direct_mut().unwrap().redis_db = 0;
    restored.profiles = vec![profile.clone()];
    restored.profile = Some(profile);
    restored.restore_browse();
    restored.action("open", &backend);
    let reconnect = requests.try_recv().unwrap();
    assert_eq!(reconnect.method, "connection.open");
    assert_eq!(reconnect.params["redis_db"], 5);
}

#[test]
fn change_preview_rejects_changed_drafts_but_accepts_current_response() {
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
            result: Ok(json!({"provider":"redis","transactional":true,"offset":0,"next_offset":null,"items":[]})),
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
            result: Ok(json!({"provider":"redis","transactional":true,"offset":0,"next_offset":null,"items":[]})),
        },
        &backend,
    );
    assert!(app.change_preview_value().is_some());
}

#[test]
fn stale_large_value_failure_does_not_cancel_newer_value() {
    use base64::Engine as _;
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("large-value-race".into());
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let large = |id| CellValue::Large {
        id,
        original_type: "text".into(),
        bytes: 5,
        preview: "...".into(),
    };
    let mut grid = editable_grid();
    grid.page.rows = vec![vec![large(first)], vec![large(second)]];
    grid.page.result.row_count = 2;
    let cell = Cell { row: 1, column: 0 };
    grid.focus = Some(cell);
    app.grid = Some(grid);
    app.open_cell_at(Cell { row: 0, column: 0 }, &backend);
    let old = requests.try_recv().unwrap();
    app.open_cell_at(cell, &backend);
    let current = requests.try_recv().unwrap();
    app.workbench.inline = Some(cell);
    let status = app.status.render().into_owned();
    app.routed_response(
        Response {
            tag: old.tag,
            result: Err(AppError::new("not_found", "old value expired")),
        },
        &backend,
    );
    assert_eq!(app.workbench.inline, Some(cell));
    assert_eq!(app.status.render().as_ref(), status);
    let full = CellValue::Text("complete".into());
    let bytes = serde_json::to_vec(&full).unwrap();
    app.routed_response(Response { tag:current.tag, result:Ok(json!({"data_b64":base64::engine::general_purpose::STANDARD.encode(&bytes),"offset":0,"next":null,"total_bytes":bytes.len()})) }, &backend);
    assert_eq!(app.grid.as_ref().unwrap().value(cell), Some(&full));
    assert_eq!(app.field("cell"), "complete");
}
