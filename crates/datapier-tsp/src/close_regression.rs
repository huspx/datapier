use super::*;

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
