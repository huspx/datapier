use super::*;
use crate::workspace::finish_after_input_closed;

// 模拟分块服务并保留完整保存结果，让退出测试检查用户可恢复的草稿。
fn scripted_workspace_service(
    mut requests: tokio::sync::mpsc::Receiver<crate::backend::Request>,
    replies: tokio::sync::mpsc::Sender<crate::backend::Response>,
    conflict: bool,
    commit_gate: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
) -> std::thread::JoinHandle<(String, Workspace)> {
    std::thread::spawn(move || {
        use base64::{Engine as _, engine::general_purpose::STANDARD};
        let mut conflict_pending = conflict;
        let mut gate = commit_gate;
        let mut destination = String::new();
        let mut bytes = Vec::new();
        let mut expected_bytes = 0;
        let upload_id = Uuid::new_v4();
        loop {
            let request = requests.blocking_recv().expect("workspace service request");
            let result = match request.method.as_str() {
                "workspace.save.begin" if conflict_pending => {
                    conflict_pending = false;
                    assert_eq!(request.params["workspace_id"], "conflicting-workspace");
                    assert_eq!(request.params["expected_revision"], 3);
                    Err(AppError::conflict("stale workspace"))
                }
                "workspace.save.begin" => {
                    destination = request.params["workspace_id"].as_str().unwrap().to_owned();
                    assert_eq!(
                        request.params["expected_revision"],
                        if conflict { 0 } else { 7 }
                    );
                    if conflict {
                        assert_eq!(request.params["copy_from"], "conflicting-workspace");
                    }
                    expected_bytes = request.params["bytes"].as_u64().unwrap() as usize;
                    Ok(json!({"upload_id": upload_id, "chunk_bytes": 64}))
                }
                "workspace.save.append" => {
                    assert_eq!(request.params["upload_id"], json!(upload_id));
                    assert_eq!(request.params["offset"], bytes.len());
                    bytes.extend(
                        STANDARD
                            .decode(request.params["data_b64"].as_str().unwrap())
                            .unwrap(),
                    );
                    Ok(json!({"received": bytes.len()}))
                }
                "workspace.save.commit" => {
                    assert_eq!(bytes.len(), expected_bytes);
                    if let Some((ready, release)) = gate.take() {
                        ready.send(()).unwrap();
                        release.recv().unwrap();
                    }
                    Ok(json!({"revision": if conflict { 1 } else { 8 }, "bytes": bytes.len()}))
                }
                "workspace.save.read" => {
                    let offset = request.params["offset"].as_u64().unwrap() as usize;
                    let end = (offset + 64).min(bytes.len());
                    Ok(
                        json!({"data_b64": STANDARD.encode(&bytes[offset..end]), "next": if end < bytes.len() { Some(end) } else { None }, "total_bytes": bytes.len()}),
                    )
                }
                "workspace.activate" => {
                    assert_eq!(request.params["workspace_id"], destination);
                    assert_eq!(
                        request.params["expected_workspace_id"],
                        "conflicting-workspace"
                    );
                    Ok(json!({"workspace_id": destination}))
                }
                "workspace.save.close" => {
                    let saved = serde_json::from_slice(&bytes).expect("saved workspace");
                    return (destination, saved);
                }
                method => panic!("unexpected workspace service request: {method}"),
            };
            replies
                .blocking_send(Response {
                    tag: request.tag,
                    result,
                })
                .expect("send workspace response");
        }
    })
}

#[test]
fn closing_input_waits_for_workspace_save_acknowledgment() {
    let (mut backend, requests, replies) = Backend::scripted();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let worker = scripted_workspace_service(requests, replies, false, Some((ready_tx, release_rx)));
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let closer = std::thread::spawn(move || {
        let mut app = App::new("closing-workspace".into());
        app.workspace_loaded = true;
        app.revision = 7;
        app.tabs[0].query = Editor::new("SELECT 42");
        app.dirty = true;
        let result = finish_after_input_closed(&mut app, &mut backend);
        done_tx
            .send((result, app.revision, app.saving, app.dirty))
            .unwrap();
    });
    ready_rx.recv().unwrap();
    assert!(
        done_rx
            .recv_timeout(std::time::Duration::from_millis(100))
            .is_err()
    );
    release_tx.send(()).unwrap();
    let (result, revision, saving, dirty) = done_rx.recv().unwrap();
    result.expect("save acknowledged before close");
    closer.join().unwrap();
    let (destination, saved) = worker.join().expect("scripted worker");
    assert_eq!(destination, "closing-workspace");
    assert_eq!(saved.tabs[0].query.text, "SELECT 42");
    assert_eq!(revision, 8);
    assert!(!saving && !dirty);
}

#[test]
fn closing_input_saves_a_copy_after_a_conflict() {
    let (mut backend, requests, replies) = Backend::scripted();
    let worker = scripted_workspace_service(requests, replies, true, None);
    let mut app = App::new("conflicting-workspace".into());
    app.workspace_loaded = true;
    app.revision = 3;
    app.tabs[0].query = Editor::new("SELECT 99");
    app.dirty = true;

    finish_after_input_closed(&mut app, &mut backend).expect("conflict recovered before close");
    let (copy_id, saved) = worker.join().expect("scripted worker");
    assert!(copy_id.starts_with("conflicting-workspace-copy-"));
    assert_eq!(saved.tabs[0].query.text, "SELECT 99");
    assert_eq!(app.workspace_id, copy_id);
    assert_eq!(app.revision, 1);
    assert!(!app.save_blocked && !app.saving && !app.dirty);
}

#[test]
fn batch_close_preflights_every_tab_and_uses_clicked_identity() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("batch".into());
    app.action("new-tab", &backend);
    app.action("new-tab", &backend);
    let ids: Vec<_> = app.tabs.iter().map(|tab| tab.id).collect();
    app.tabs[0].pending = json!({"restored": true});
    app.action(&format!("tab-close-left:{}", ids[2]), &backend);
    assert_eq!(app.tabs.len(), 3);
    assert!(requests.try_recv().is_err());
    app.tabs[0].pending = Value::Null;
    app.action(&format!("tab-close-right:{}", ids[0]), &backend);
    assert_eq!(
        app.tabs.iter().map(|tab| tab.id).collect::<Vec<_>>(),
        vec![ids[0]]
    );
    app.action(&format!("tab-close-all:{}", ids[0]), &backend);
    assert!(app.tabs.is_empty());
    assert!(app.session.is_none());
    assert!(app.workspace_snapshot().tabs.is_empty());
    app.action("new-tab", &backend);
    assert_eq!(app.tabs.len(), 1);
}

#[test]
fn close_waits_for_ack_and_ignores_late_session_response() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("ack".into());
    let id = app.tabs[0].id;
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.close_tab(id, &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(app.tabs.len(), 1);
    app.routed_response(
        Response {
            tag: format!("{id}:{}:resource.list", app.generation),
            result: Ok(json!({"resources":[],"cursor":null})),
        },
        &backend,
    );
    assert_eq!(app.tabs.len(), 1);
    app.routed_response(
        Response {
            tag: request.tag,
            result: Err(AppError::new("failed", "close failed")),
        },
        &backend,
    );
    assert_eq!(app.tabs.len(), 1);
    assert!(app.session.is_some());
    app.close_tab(id, &backend);
    let request = requests.try_recv().unwrap();
    app.routed_response(
        Response {
            tag: request.tag,
            result: Err(AppError::new("not_found", "gone")),
        },
        &backend,
    );
    assert!(app.tabs.is_empty());
    app.routed_response(
        Response {
            tag: format!("{id}:0:connection.open"),
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(app.session.is_none());
}

#[test]
fn empty_workspace_round_trips_and_profile_can_open() {
    let (backend, _requests, _) = Backend::scripted();
    let mut app = App::new("empty".into());
    let id = app.tabs[0].id;
    app.close_tab(id, &backend);
    let state = serde_json::to_value(app.workspace_snapshot()).unwrap();
    let mut restored = App::new("empty".into());
    restored.response(
        Response {
            tag: "workspace.load".into(),
            result: Ok(json!({"revision":1,"state":state})),
        },
        &backend,
    );
    assert!(restored.tabs.is_empty());
    restored.profile_form(None);
    let profile = restored.form_profile().unwrap();
    restored.profiles.push(profile);
    restored.workbench.overlay = None;
    restored.select_profile(0, &backend);
    assert_eq!(restored.tabs.len(), 1);
}

#[test]
fn empty_workspace_visible_actions_restore_query_and_navigation() {
    let (backend, _requests, _) = Backend::scripted();
    let mut app = App::new("empty-actions".into());
    app.close_tab(app.tabs[0].id, &backend);
    assert!(app.tabs.is_empty());
    let caps = tern_sdk::Capabilities {
        cols: 60,
        ..Default::default()
    };
    app.update_viewport(&caps);
    assert!(app.navigation_compact());
    app.action("navigation-toggle", &backend);
    assert!(!app.navigation_compact());
    app.action("view-query", &backend);
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.view, "query");
    assert_eq!(app.focus, "query");
}

#[test]
fn last_tab_keeps_target_across_workspace_restore_and_reconnect() {
    for provider in ["mysql", "redis"] {
        for profiles_first in [true, false] {
            let (backend, mut requests, _) = Backend::scripted();
            let mut app = App::new("retained-target".into());
            app.profile_form(None);
            app.field_set("provider", provider);
            app.field_set("name", "retained-connection");
            let profile = app.form_profile().unwrap();
            app.profiles.push(profile.clone());
            app.profile = Some(profile.clone());
            app.tabs[0].profile_id = Some(profile.id);
            app.database = Some(
                if provider == "redis" {
                    "3"
                } else {
                    "analytics"
                }
                .into(),
            );
            app.tabs[0].browse.database = app.database.clone();
            app.tabs[0].browse.redis_db = (provider == "redis").then_some(3);
            app.workbench.overlay = None;
            app.session = Some(json!({"session_id":Uuid::new_v4()}));
            app.close_tab(app.tabs[0].id, &backend);
            let close = requests.try_recv().unwrap();
            app.routed_response(
                Response {
                    tag: close.tag,
                    result: Ok(json!({})),
                },
                &backend,
            );
            assert!(app.tabs.is_empty());
            assert!(app.session.is_none());
            assert_eq!(app.profile.as_ref().unwrap().id, profile.id);

            let state = serde_json::to_value(app.workspace_snapshot()).unwrap();
            let mut restored = App::new("retained-target".into());
            let profiles = Response {
                tag: "profile.list".into(),
                result: Ok(json!({"profiles":[profile]})),
            };
            let workspace = Response {
                tag: "workspace.load".into(),
                result: Ok(json!({"revision":1,"state":state})),
            };
            let responses = if profiles_first {
                [profiles, workspace]
            } else {
                [workspace, profiles]
            };
            for response in responses {
                restored.response(response, &backend);
            }
            assert!(restored.tabs.is_empty());
            assert_eq!(
                restored.profile.as_ref().unwrap().id,
                app.profile.as_ref().unwrap().id
            );
            assert_eq!(restored.database, app.database);
            restored.select_profile(0, &backend);
            let open = requests.try_recv().unwrap();
            assert_eq!(open.method, "connection.open");
            if provider == "redis" {
                assert_eq!(open.params["redis_db"], 3);
                assert_eq!(restored.current_redis_db(), 3);
            } else {
                assert_eq!(open.params["database"], "analytics");
            }
        }
    }
}

#[test]
fn selecting_another_connection_after_last_close_does_not_reuse_its_database() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("other-connection".into());
    app.profile_form(None);
    app.field_set("provider", "redis");
    app.field_set("name", "first");
    let first = app.form_profile().unwrap();
    let mut second = first.clone();
    second.id = Uuid::new_v4();
    second.name = "second".into();
    second.direct_mut().unwrap().redis_db = 7;
    app.profiles = vec![first.clone(), second.clone()];
    app.profile = Some(first.clone());
    app.tabs[0].profile_id = Some(first.id);
    app.tabs[0].browse.redis_db = Some(3);
    app.database = Some("3".into());
    app.workbench.overlay = None;
    app.close_tab(app.tabs[0].id, &backend);
    app.select_profile(1, &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.params["profile_id"], json!(second.id));
    assert_eq!(open.params["redis_db"], 7);
    assert_eq!(app.current_redis_db(), 7);
}
