use super::*;
use crate::workspace::finish_after_input_closed;

#[test]
fn closing_input_waits_for_workspace_save_acknowledgment() {
    let (mut backend, mut requests, replies) = Backend::scripted();
    let worker_replies = replies.clone();
    let worker = std::thread::spawn(move || {
        let request = requests.blocking_recv().expect("workspace save request");
        worker_replies
            .blocking_send(Response {
                tag: request.tag.clone(),
                result: Ok(json!({"revision": 8})),
            })
            .expect("send save acknowledgment");
        request
    });
    let mut app = App::new("closing-workspace".into());
    app.workspace_loaded = true;
    app.revision = 7;
    app.tabs[0].query = Editor::new("SELECT 42");
    app.dirty = true;

    finish_after_input_closed(&mut app, &mut backend).expect("save acknowledged before close");
    let request = worker.join().expect("scripted worker");
    assert_eq!(request.tag, "workspace.save");
    assert_eq!(request.method, "workspace.save");
    assert_eq!(request.params["workspace_id"], "closing-workspace");
    assert_eq!(request.params["expected_revision"], 7);
    let saved: Workspace =
        serde_json::from_value(request.params["state"].clone()).expect("saved workspace");
    assert_eq!(saved.tabs[0].query.text, "SELECT 42");
    assert_eq!(app.revision, 8);
    assert!(!app.saving);
    assert!(!app.dirty);
}

#[test]
fn closing_input_saves_a_copy_after_a_conflict() {
    let (mut backend, mut requests, replies) = Backend::scripted();
    let worker_replies = replies.clone();
    let worker = std::thread::spawn(move || {
        let original = requests.blocking_recv().expect("original save request");
        worker_replies
            .blocking_send(Response {
                tag: original.tag.clone(),
                result: Err(AppError::conflict("stale workspace")),
            })
            .expect("send conflict");
        let copy = requests.blocking_recv().expect("copy save request");
        worker_replies
            .blocking_send(Response {
                tag: copy.tag.clone(),
                result: Ok(json!({"revision": 1})),
            })
            .expect("send copy acknowledgment");
        let followup = requests.blocking_recv().expect("follow-up save request");
        worker_replies
            .blocking_send(Response {
                tag: followup.tag.clone(),
                result: Ok(json!({"revision": 2})),
            })
            .expect("send follow-up acknowledgment");
        (original, copy, followup)
    });
    let mut app = App::new("conflicting-workspace".into());
    app.workspace_loaded = true;
    app.revision = 3;
    app.tabs[0].query = Editor::new("SELECT 99");
    app.dirty = true;

    finish_after_input_closed(&mut app, &mut backend).expect("conflict recovered before close");
    let (original, copy, followup) = worker.join().expect("scripted worker");
    assert_eq!(original.tag, "workspace.save");
    assert_eq!(original.params["workspace_id"], "conflicting-workspace");
    assert_eq!(original.params["expected_revision"], 3);
    assert_eq!(copy.tag, "workspace.copy");
    assert_eq!(copy.method, "workspace.save");
    let copy_id = copy.params["workspace_id"].as_str().expect("copy id");
    assert!(copy_id.starts_with("conflicting-workspace-copy-"));
    assert_eq!(copy.params["expected_revision"], 0);
    let saved: Workspace =
        serde_json::from_value(copy.params["state"].clone()).expect("saved copy");
    assert_eq!(saved.tabs[0].query.text, "SELECT 99");
    assert_eq!(followup.tag, "workspace.save");
    assert_eq!(followup.method, "workspace.save");
    assert_eq!(followup.params["workspace_id"], copy_id);
    assert_eq!(followup.params["expected_revision"], 1);
    assert_eq!(app.workspace_id, copy_id);
    assert_eq!(app.revision, 2);
    assert!(!app.save_blocked);
    assert!(!app.saving);
    assert!(!app.dirty);
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
