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
fn canonical_drafts_rebind_live_rows_without_overwriting_later_edits() {
    let mut app = App::new("source".into());
    let old_id = Uuid::new_v4();
    let new_id = Uuid::new_v4();
    let old_file = crate::grid::EditFileRef {
        edit_file_id: old_id,
        edit_file_revision: 1,
    };
    let large = |id| CellValue::Large {
        id,
        original_type: "text".into(),
        bytes: 4,
        preview: "user".into(),
    };
    app.pending_rows.push(datapier_core::model::RowChange {
        operation: "insert".into(),
        key: Default::default(),
        before: Default::default(),
        values: [
            ("file".into(), large(old_id)),
            ("text".into(), CellValue::Text(old_id.to_string())),
        ]
        .into_iter()
        .collect(),
    });
    app.pending_edit_files.push(old_file);
    let sent = serde_json::to_value(app.workspace_snapshot()).unwrap();
    let mut canonical = sent.clone();
    canonical["tabs"][0]["pending"]["edit_files"][0]["edit_file_id"] = json!(new_id);
    canonical["tabs"][0]["pending"]["rows"][0]["values"]["file"]["value"]["id"] = json!(new_id);
    app.pending_rows[0]
        .values
        .insert("later".into(), CellValue::Text("new edit".into()));
    app.rebind_workspace_drafts(&sent, &canonical, false);
    assert_eq!(app.pending_rows[0].values["file"], large(new_id));
    assert_eq!(
        app.pending_rows[0].values["text"],
        CellValue::Text(old_id.to_string())
    );
    assert_eq!(
        app.pending_rows[0].values["later"],
        CellValue::Text("new edit".into())
    );
    assert_eq!(app.pending_edit_files[0].edit_file_id, new_id);
    let next = serde_json::to_value(app.workspace_snapshot()).unwrap();
    assert_eq!(
        next["tabs"][0]["pending"]["edit_files"][0]["edit_file_id"],
        json!(new_id)
    );
}

#[test]
fn copied_drafts_require_fresh_baseline_and_keep_rebound_file() {
    let mut app = App::new("source".into());
    let old = Uuid::new_v4();
    let new = Uuid::new_v4();
    app.edit_token = Some(json!({"token":"old"}));
    app.pending_rows.push(datapier_core::model::RowChange {
        operation: "insert".into(),
        key: Default::default(),
        before: Default::default(),
        values: [(
            "v".into(),
            CellValue::Large {
                id: old,
                original_type: "text".into(),
                bytes: 1,
                preview: "x".into(),
            },
        )]
        .into_iter()
        .collect(),
    });
    app.pending_edit_files.push(crate::grid::EditFileRef {
        edit_file_id: old,
        edit_file_revision: 1,
    });
    let sent = serde_json::to_value(app.workspace_snapshot()).unwrap();
    let mut canonical = sent.clone();
    canonical["tabs"][0]["pending"]["edit_files"][0]["edit_file_id"] = json!(new);
    canonical["tabs"][0]["pending"]["rows"][0]["values"]["v"]["value"]["id"] = json!(new);
    app.rebind_workspace_drafts(&sent, &canonical, true);
    assert!(app.edit_token.is_none());
    assert!(app.pending_rows.is_empty());
    assert!(!app.tabs[0].pending_live);
    assert_eq!(
        app.tabs[0].pending["rows"][0]["values"]["v"]["value"]["id"],
        json!(new)
    );
    assert_eq!(
        app.tabs[0].pending["edit_files"][0]["edit_file_id"],
        json!(new)
    );
}

#[test]
fn copy_ack_keeps_original_state_when_editors_or_tasks_are_active() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    // 检查静止切换、编辑器、异步文件请求、后台任务及保存期间的新编辑。
    for case in 0..5 {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = App::new("source".into());
        let old = Uuid::new_v4();
        let new = Uuid::new_v4();
        app.edit_token = Some(json!({"token":"old"}));
        app.pending_rows.push(datapier_core::model::RowChange {
            operation: "insert".into(),
            key: Default::default(),
            before: Default::default(),
            values: [(
                "v".into(),
                CellValue::Large {
                    id: old,
                    original_type: "text".into(),
                    bytes: 1,
                    preview: "x".into(),
                },
            )]
            .into_iter()
            .collect(),
        });
        app.pending_edit_files.push(crate::grid::EditFileRef {
            edit_file_id: old,
            edit_file_revision: 1,
        });
        if case == 1 {
            app.advanced_value.session = Some("open-value".into());
        }
        if case == 3 {
            second_tab(&mut app);
            app.tabs[1].runtime.task = Some("write-task".into());
        }
        app.save_workspace_copy(&backend);
        let begin = requests.try_recv().unwrap();
        let copy_id = begin.params["workspace_id"].as_str().unwrap().to_owned();
        let upload_id = Uuid::new_v4();
        app.workspace_upload_response(
            Response {
                tag: begin.tag,
                result: Ok(json!({"upload_id": upload_id, "chunk_bytes":262144})),
            },
            &backend,
        );
        let append = requests.try_recv().unwrap();
        let bytes = STANDARD
            .decode(append.params["data_b64"].as_str().unwrap())
            .unwrap();
        let mut canonical: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        canonical["tabs"][0]["pending"]["edit_files"][0]["edit_file_id"] = json!(new);
        canonical["tabs"][0]["pending"]["rows"][0]["values"]["v"]["value"]["id"] = json!(new);
        app.workspace_upload_response(
            Response {
                tag: append.tag,
                result: Ok(json!({"received":bytes.len()})),
            },
            &backend,
        );
        let commit = requests.try_recv().unwrap();
        let canonical_bytes = serde_json::to_vec(&canonical).unwrap();
        app.workspace_upload_response(
            Response {
                tag: commit.tag,
                result: Ok(json!({"revision":1,"bytes":canonical_bytes.len()})),
            },
            &backend,
        );
        let read = requests.try_recv().unwrap();
        if case == 1 {
            app.advanced_value.session = None;
        }
        if case == 2 {
            app.advanced_files.pending = true;
        }
        if case == 4 {
            app.tabs[0].query = Editor::new("later edit");
            app.dirty = true;
        }
        app.workspace_upload_response(Response { tag: read.tag, result: Ok(json!({"data_b64":STANDARD.encode(&canonical_bytes),"next":null,"total_bytes":canonical_bytes.len()})) }, &backend);
        if case == 0 {
            let activate = requests.try_recv().unwrap();
            assert_eq!(activate.method, "workspace.activate");
            assert_eq!(activate.params["workspace_id"], copy_id);
            assert_eq!(app.workspace_id, "source");
            app.workspace_upload_response(
                Response {
                    tag: activate.tag,
                    result: Ok(json!({"workspace_id": copy_id})),
                },
                &backend,
            );
            assert_eq!(app.workspace_id, copy_id);
            assert!(app.edit_token.is_none());
            assert_eq!(
                app.tabs[0].pending["edit_files"][0]["edit_file_id"],
                json!(new)
            );
        } else {
            assert_eq!(app.workspace_id, "source");
            assert_eq!(app.edit_token, Some(json!({"token":"old"})));
            assert_eq!(app.pending_edit_files[0].edit_file_id, old);
            assert!(app.dirty);
            if case == 2 {
                assert!(app.advanced_files.pending);
            }
            if case == 3 {
                assert_eq!(app.tabs[1].runtime.task.as_deref(), Some("write-task"));
            }
            if case == 4 {
                assert_eq!(app.tabs[0].query.text, "later edit");
            }
        }
        let cleanup = requests.try_recv().unwrap();
        assert_eq!(cleanup.method, "workspace.save.close");
    }
}

#[test]
fn canonical_externalization_preserves_changes_made_during_save() {
    let mut app = App::new("source".into());
    app.pending_rows.push(datapier_core::model::RowChange {
        operation: "insert".into(),
        key: Default::default(),
        before: Default::default(),
        values: [
            ("saved".into(), CellValue::Text("sent".into())),
            ("changed".into(), CellValue::Text("old".into())),
        ]
        .into_iter()
        .collect(),
    });
    let sent = serde_json::to_value(app.workspace_snapshot()).unwrap();
    let id = Uuid::new_v4();
    let large = |preview: &str| CellValue::Large {
        id,
        original_type: "text".into(),
        bytes: 4,
        preview: preview.into(),
    };
    let mut canonical = sent.clone();
    canonical["tabs"][0]["pending"]["rows"][0]["values"]["saved"] = json!(large("sent"));
    canonical["tabs"][0]["pending"]["rows"][0]["values"]["changed"] = json!(large("old"));
    canonical["tabs"][0]["pending"]["edit_files"] =
        json!([{ "edit_file_id": id, "edit_file_revision": 1 }]);
    app.pending_rows[0]
        .values
        .insert("changed".into(), CellValue::Text("new".into()));
    app.rebind_workspace_drafts(&sent, &canonical, false);
    assert_eq!(app.pending_rows[0].values["saved"], large("sent"));
    assert_eq!(
        app.pending_rows[0].values["changed"],
        CellValue::Text("new".into())
    );
    assert_eq!(app.pending_edit_files[0].edit_file_id, id);
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

#[test]
fn chunked_workspace_restores_large_state_and_preserves_concurrent_edit() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let (backend, mut requests, _) = Backend::scripted();
    let mut saved = App::new("saved".into());
    saved.tabs[0].query = Editor::new("x".repeat(9 * 1024 * 1024));
    saved.tabs[0].pending = json!({"rows":[{"values":{"message":"keep"}}]});
    saved.tabs[0].replace_draft = Some(json!({"intent":{"find":"old","replace":"new"}}));
    let bytes = serde_json::to_vec(&saved.workspace_snapshot()).unwrap();
    let mut app = App::new("loading".into());
    app.start_workspace_load(&backend);
    let request = requests.try_recv().unwrap();
    let handle = Uuid::new_v4();
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"revision":7,"load_handle":handle,"bytes":bytes.len()})),
        },
        &backend,
    );
    app.tabs[0].query = Editor::new("local edit");
    app.changed_field("query");
    let mut offset = 0;
    while offset < bytes.len() {
        let read = requests.try_recv().unwrap();
        assert_eq!(read.method, "workspace.save.read");
        assert_eq!(read.params["offset"], json!(offset));
        assert!(!app.workspace_loaded);
        let end = (offset + 65536).min(bytes.len());
        app.routed_response(Response { tag: read.tag, result: Ok(json!({"offset":offset,"total_bytes":bytes.len(),"data_b64":STANDARD.encode(&bytes[offset..end]),"next":if end < bytes.len() { Some(end) } else { None }})) }, &backend);
        offset = end;
    }
    assert_eq!(requests.try_recv().unwrap().method, "workspace.save.close");
    assert!(app.workspace_loaded && app.dirty);
    assert_eq!(app.revision, 7);
    assert_eq!(app.tabs[app.active].query.text, "local edit");
    let restored = app
        .tabs
        .iter()
        .find(|tab| tab.query.text.len() == 9 * 1024 * 1024)
        .unwrap();
    assert_eq!(restored.pending["rows"][0]["values"]["message"], "keep");
    assert_eq!(
        restored.replace_draft.as_ref().unwrap()["intent"]["replace"],
        "new"
    );
}

#[test]
fn broken_or_stale_workspace_load_never_overwrites_drafts() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    for stale in [false, true] {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = App::new("original".into());
        app.tabs[0].query = Editor::new("unsaved");
        app.dirty = true;
        app.start_workspace_load(&backend);
        let start = requests.try_recv().unwrap();
        let handle = Uuid::new_v4();
        app.routed_response(
            Response {
                tag: start.tag,
                result: Ok(json!({"revision":3,"load_handle":handle,"bytes":4})),
            },
            &backend,
        );
        let read = requests.try_recv().unwrap();
        if stale {
            app.workspace_id = "different".into();
        }
        app.routed_response(Response { tag: read.tag, result: Ok(json!({"offset":1,"total_bytes":4,"data_b64":STANDARD.encode(b"{}"),"next":null})) }, &backend);
        assert_eq!(requests.try_recv().unwrap().method, "workspace.save.close");
        assert!(!app.workspace_loaded && app.dirty);
        assert_eq!(app.tabs[0].query.text, "unsaved");
        assert_eq!(app.revision, 0);
    }
}

#[test]
fn cancelled_load_closes_late_handle() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.start_workspace_load(&backend);
    let old = requests.try_recv().unwrap();
    app.start_workspace_load(&backend);
    let _new = requests.try_recv().unwrap();
    let handle = Uuid::new_v4();
    app.routed_response(
        Response {
            tag: old.tag,
            result: Ok(json!({"revision":1,"load_handle":handle,"bytes":9})),
        },
        &backend,
    );
    let close = requests.try_recv().unwrap();
    assert_eq!(close.method, "workspace.save.close");
    assert_eq!(close.params["upload_id"], json!(handle));
    assert!(!app.workspace_loaded);
}

#[test]
fn invalid_chunk_protocol_does_not_mark_workspace_loaded() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    let cases = [
        json!({"offset":0,"total_bytes":4,"data_b64":"%%%","next":null}),
        json!({"offset":0,"total_bytes":5,"data_b64":STANDARD.encode(b"{}"),"next":2}),
        json!({"offset":0,"total_bytes":4,"data_b64":STANDARD.encode(b"{}"),"next":null}),
        json!({"offset":0,"total_bytes":4,"data_b64":STANDARD.encode(b"{}"),"next":0}),
        json!({"offset":0,"total_bytes":4,"data_b64":STANDARD.encode(b"{}")}),
    ];
    for value in cases {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = App::new("test".into());
        app.start_workspace_load(&backend);
        let start = requests.try_recv().unwrap();
        let handle = Uuid::new_v4();
        app.routed_response(
            Response {
                tag: start.tag,
                result: Ok(json!({"revision":1,"load_handle":handle,"bytes":4})),
            },
            &backend,
        );
        let read = requests.try_recv().unwrap();
        app.routed_response(
            Response {
                tag: read.tag,
                result: Ok(value),
            },
            &backend,
        );
        assert_eq!(requests.try_recv().unwrap().method, "workspace.save.close");
        assert!(!app.workspace_loaded);
        assert_eq!(app.revision, 0);
    }
}

#[test]
fn workspace_load_enforces_declared_and_parsed_memory_budgets() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    for structural_overflow in [false, true] {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = App::new("test".into());
        app.settings.limits.result_memory_bytes_per_client = 1024;
        app.tabs[0].query = Editor::new("local draft");
        app.dirty = true;
        app.start_workspace_load(&backend);
        let start = requests.try_recv().unwrap();
        let handle = Uuid::new_v4();
        let bytes = if structural_overflow {
            b"[[],[],[],[],[],[],[],[]]".to_vec()
        } else {
            vec![b' '; 257]
        };
        app.routed_response(
            Response {
                tag: start.tag,
                result: Ok(json!({"revision":1,"load_handle":handle,"bytes":bytes.len()})),
            },
            &backend,
        );
        if structural_overflow {
            let read = requests.try_recv().unwrap();
            app.routed_response(Response { tag: read.tag, result: Ok(json!({"offset":0,"total_bytes":bytes.len(),"data_b64":STANDARD.encode(&bytes),"next":null})) }, &backend);
        }
        assert_eq!(requests.try_recv().unwrap().method, "workspace.save.close");
        assert!(!app.workspace_loaded && app.dirty);
        assert_eq!(app.tabs[0].query.text, "local draft");
    }
}
