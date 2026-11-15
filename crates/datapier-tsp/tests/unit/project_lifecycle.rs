//! 项目预览消费与未保存查询保护回归。
use super::*;
use datapier_core::model::AppError;

#[test]
fn failed_commit_releases_consumed_preview_and_preserves_draft() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("project-commit".into());
    app.workbench.project.project = Some(
        json!({"project_id":Uuid::new_v4(),"manifest":{"queries":[],"connections":[{"alias":"db","provider":"mysql"}]}}),
    );
    app.workbench.project.preview =
        Some(json!({"plan_id":Uuid::new_v4(),"revision":1,"digest":"digest"}));
    app.workbench.project.unsaved = true;
    app.project_set("query-path", "draft.sql");
    app.project_set("query-text", "select 1");
    app.project_set("query-alias", "db");
    app.project_action("project-ui-save-commit", &backend);
    let commit = requests.try_recv().unwrap();
    app.project_response(
        commit.tag.splitn(3, ':').nth(2).unwrap(),
        &Err(AppError::new("failed", "commit failed")),
        &backend,
    );
    assert!(app.workbench.project.preview.is_none());
    assert!(app.workbench.project.obsolete_preview.is_none());
    assert!(app.workbench.project.unsaved);
    assert_eq!(app.project_value("query-text"), "select 1");
    app.project_action("project-ui-save-prepare", &backend);
    let begin = requests.try_recv().unwrap();
    assert_eq!(begin.method, "project.save.upload.begin");
    app.project_response(
        begin.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"upload_id":Uuid::new_v4(),"revision":1})),
        &backend,
    );
    let chunk = requests.try_recv().unwrap();
    assert_eq!(chunk.params["path"], "draft.sql");
    assert_eq!(chunk.params["text"], "select 1");
    app.project_response(
        chunk.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"revision":2,"offset":8})),
        &backend,
    );
    let prepare = requests.try_recv().unwrap();
    assert!(prepare.params["replace_plan_id"].is_null());
    assert!(app.status.is_empty());
}

#[test]
fn unsaved_query_cannot_be_replaced_by_switching_queries() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("project-query".into());
    app.workbench.project.project = Some(
        json!({"project_id":Uuid::new_v4(),"manifest":{"queries":[{"name":"other","path":"other.sql","connection_alias":"db"}]}}),
    );
    app.project_set("query-path", "draft.sql");
    app.project_set("query-text", "unsaved text");
    app.workbench.project.unsaved = true;
    app.project_action("project-ui-query:0", &backend);
    assert_eq!(app.project_value("query-text"), "unsaved text");
    assert_eq!(app.project_value("query-path"), "draft.sql");
    assert!(requests.try_recv().is_err());
    app.workbench.project.unsaved = false;
    app.project_action("project-ui-query:0", &backend);
    assert_eq!(requests.try_recv().unwrap().method, "project.file.read");
}

#[test]
fn successful_binding_replaces_previous_error_status() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("project-binding".into());
    app.workbench.project.project = Some(json!({"project_id":Uuid::new_v4()}));
    app.workbench.project.bindings = Some(json!({"bindings":[]}));
    app.project_set("alias", "db");
    app.status = i18n::UiMessage::raw("previous error");
    app.project_action("project-ui-bindings-save", &backend);
    let request = requests.try_recv().unwrap();
    app.project_response(
        request.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"bindings":[{"connection_alias":"db","revision":1}]})),
        &backend,
    );
    assert!(app.status.is_empty());
}

#[test]
fn reload_requires_confirmation_and_keeps_draft_on_read_failure() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("project-reload".into());
    app.workbench.project.project = Some(json!({"project_id":Uuid::new_v4()}));
    app.project_set("query-path", "draft.sql");
    app.project_set("query-text", "local draft");
    app.workbench.project.unsaved = true;
    app.project_action("project-ui-file-reload", &backend);
    assert!(app.workbench.project.reload_confirm);
    assert!(requests.try_recv().is_err());
    app.project_action("project-ui-file-reload-confirm", &backend);
    let request = requests.try_recv().unwrap();
    app.project_response(
        request.tag.splitn(3, ':').nth(2).unwrap(),
        &Err(AppError::new("io", "read failed")),
        &backend,
    );
    assert_eq!(app.project_value("query-text"), "local draft");
    assert!(app.workbench.project.unsaved);
}

#[test]
fn accepting_external_baseline_preserves_local_manifest_and_query() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("project-rebase".into());
    app.workbench.project.project = Some(
        json!({"project_id":Uuid::new_v4(),"manifest":{"name":"local draft"},"digests":{"a":"old"}}),
    );
    app.workbench.project.external =
        Some(json!({"manifest":{"name":"external"},"digests":{"a":"new"}}));
    app.project_set("query-text", "local query");
    app.workbench.project.unsaved = true;
    app.project_action("project-ui-external-accept", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.params["digests"]["a"], "new");
    app.project_response(
        request.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"digests":{"a":"new"}})),
        &backend,
    );
    assert_eq!(
        app.workbench.project.project.as_ref().unwrap()["manifest"]["name"],
        "local draft"
    );
    assert_eq!(app.project_value("query-text"), "local query");
    assert!(app.workbench.project.unsaved);
}

#[test]
fn workspace_restores_project_and_transfer_intent_without_handles() {
    let mut app = App::new("project-workspace".into());
    app.view = "project-workbench".into();
    app.workbench.project.project = Some(
        json!({"project_id":Uuid::new_v4(),"root":"/project","manifest":{"name":"draft","queries":[]},"digests":{"a":"old"}}),
    );
    app.workbench.project.unsaved = true;
    app.workbench
        .project
        .transfer_files
        .insert("copy.json".into(), "{}".into());
    app.project_set("query-text", "local query");
    app.field_set(&app.transfer_key("source-alias"), "local-source");
    let snapshot = serde_json::to_value(app.workspace_snapshot()).unwrap();
    assert!(
        snapshot["tabs"][0]["workbench_draft"]["project"]
            .get("project_id")
            .is_none()
    );
    let mut restored = App::new("project-workspace".into());
    restored.restore_workspace_state(1, snapshot).unwrap();
    assert!(restored.workbench.project.project.is_none());
    assert_eq!(
        restored.workbench.project.restored.as_ref().unwrap()["manifest"]["name"],
        "draft"
    );
    assert_eq!(restored.project_value("query-text"), "local query");
    assert_eq!(
        restored.field(&restored.transfer_key("source-alias")),
        "local-source"
    );
    assert_eq!(restored.workbench.project.transfer_files["copy.json"], "{}");
}

#[test]
fn partial_project_read_preserves_previous_query_until_complete() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("project-chunks".into());
    app.workbench.project.project = Some(
        json!({"project_id":Uuid::new_v4(),"manifest":{"queries":[{"name":"next","path":"next.sql","connection_alias":"db"}]}}),
    );
    app.project_set("query-path", "previous.sql");
    app.project_set("query-text", "previous text");
    app.project_action("project-ui-query:0", &backend);
    let first = requests.try_recv().unwrap();
    app.project_response(
        first.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"text":"first","next":5})),
        &backend,
    );
    assert!(app.workbench.project.file_loading);
    assert_eq!(app.project_value("query-path"), "previous.sql");
    assert_eq!(app.project_value("query-text"), "previous text");
    app.project_action("project-ui-file-next", &backend);
    let second = requests.try_recv().unwrap();
    assert_eq!(second.params["path"], "next.sql");
    app.project_response(
        second.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"text":" second","next":null})),
        &backend,
    );
    assert_eq!(app.project_value("query-path"), "next.sql");
    assert_eq!(app.project_value("query-text"), "first second");
}

#[test]
fn external_deletion_requires_an_explicit_decision() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("project-delete".into());
    let query = json!({"id":Uuid::new_v4(),"name":"local","path":"q.sql"});
    let manifest = json!({"queries":[query],"connections":[],"transfers":[]});
    app.workbench.project.project = Some(json!({"project_id":Uuid::new_v4(),"manifest":manifest}));
    app.workbench.project.baseline_manifest = Some(manifest);
    app.workbench.project.external =
        Some(json!({"manifest":{"queries":[],"connections":[],"transfers":[]},"digests":{}}));
    app.project_action("project-ui-external-accept", &backend);
    assert!(requests.try_recv().is_err());
    app.project_action("project-ui-delete-decision:queries:0:keep", &backend);
    app.project_action("project-ui-external-accept", &backend);
    assert_eq!(
        requests.try_recv().unwrap().method,
        "project.external.accept"
    );
}

#[test]
fn reopening_restored_project_sends_saved_baseline_and_checks_disk() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("project-restore".into());
    let id = Uuid::new_v4();
    let baseline = json!({"id":id,"name":"disk","queries":[],"connections":[],"transfers":[]});
    app.project_set("path", "/project");
    app.workbench.project.restored = Some(
        json!({"root":"/project","manifest":{"name":"local"},"baseline_manifest":baseline,"digests":{"a":"old"}}),
    );
    app.workbench.project.unsaved = true;
    app.project_action("project-ui-open", &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.params["baseline_manifest"]["name"], "disk");
    assert_eq!(open.params["baseline_digests"]["a"], "old");
    app.project_response(
        open.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"project_id":id,"root":"/project","manifest":baseline,"digests":{"a":"old"}})),
        &backend,
    );
    let read = requests.try_recv().unwrap();
    assert_eq!(read.method, "project.external.read");
    assert!(app.workbench.project.needs_baseline_check);
    app.project_response(
        read.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"manifest":baseline,"digests":{"a":"new"}})),
        &backend,
    );
    assert!(app.workbench.project.external.is_some());
    assert_eq!(
        app.workbench.project.project.as_ref().unwrap()["manifest"]["name"],
        "local"
    );
}

#[test]
fn redis_query_uploads_utf8_in_bounded_chunks() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("project-redis".into());
    app.workbench.project.project = Some(
        json!({"project_id":Uuid::new_v4(),"manifest":{"queries":[],"connections":[{"alias":"cache","provider":"redis"}]}}),
    );
    app.project_set("query-path", "query.redis");
    app.project_set("query-alias", "cache");
    let text = format!("{}x", "键".repeat((16 * 1024 * 1024 - 1) / 3));
    app.project_set("query-text", &text);
    app.project_action("project-ui-save-prepare", &backend);
    let begin = requests.try_recv().unwrap();
    app.project_response(
        begin.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"upload_id":Uuid::new_v4(),"revision":1})),
        &backend,
    );
    let mut received = String::new();
    let mut revision = 1;
    loop {
        let request = requests.try_recv().unwrap();
        if request.method == "project.save.prepare" {
            assert_eq!(
                request.params["manifest"]["queries"][0]["language"],
                "redis"
            );
            break;
        }
        assert_eq!(request.method, "project.save.upload.chunk");
        let chunk = request.params["text"].as_str().unwrap();
        assert!(chunk.len() <= 256 * 1024);
        assert_eq!(request.params["offset"], received.len());
        received.push_str(chunk);
        revision += 1;
        app.project_response(
            request.tag.splitn(3, ':').nth(2).unwrap(),
            &Ok(json!({"revision":revision,"offset":received.len()})),
            &backend,
        );
    }
    assert_eq!(received, text);
}

#[test]
fn workspace_copy_does_not_activate_with_project_or_transfer_in_flight() {
    let mut app = App::new("project-copy".into());
    app.workbench.project.project = Some(json!({"project_id":Uuid::new_v4()}));
    assert!(app.workspace_copy_has_active_editors());
    app.workbench.project.project = None;
    app.workbench.transfer.pending = Some((Uuid::new_v4(), 0, "transfer.prepare".into()));
    assert!(app.workspace_copy_has_active_editors());
}

#[test]
fn short_binding_and_artifact_pages_return_to_the_immediately_previous_page() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("project-pages".into());
    app.workbench.project.project = Some(json!({"project_id":Uuid::new_v4()}));
    app.project_set("group", "default");
    app.project_action("project-ui-bindings", &backend);
    for next in [Some(3), Some(7), Some(11)] {
        let request = requests.try_recv().unwrap();
        app.project_response(
            request.tag.splitn(3, ':').nth(2).unwrap(),
            &Ok(json!({"bindings":[],"next":next})),
            &backend,
        );
        if next != Some(11) {
            app.project_action("project-ui-bindings-next", &backend);
        }
    }
    app.project_action("project-ui-bindings-previous", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.params["offset"], 3);
    app.project_response(
        request.tag.splitn(3, ':').nth(2).unwrap(),
        &Err(AppError::new("failed", "read failed")),
        &backend,
    );
    assert_eq!(app.workbench.project.binding_offset, 7);
    app.project_action("project-ui-bindings-previous", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.params["offset"], 3);
    app.project_response(
        request.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"bindings":[],"next":7})),
        &backend,
    );
    assert_eq!(app.workbench.project.binding_offset, 3);

    app.project_action("project-ui-artifact-list", &backend);
    for next in [Some(2), Some(5), Some(9)] {
        let discover = requests.try_recv().unwrap();
        app.project_response(
            discover.tag.splitn(3, ':').nth(2).unwrap(),
            &Ok(json!({})),
            &backend,
        );
        let list = requests.try_recv().unwrap();
        app.project_response(
            list.tag.splitn(3, ':').nth(2).unwrap(),
            &Ok(json!({"artifacts":[],"next":next})),
            &backend,
        );
        if next != Some(9) {
            app.project_action("project-ui-artifact-next", &backend);
        }
    }
    app.project_action("project-ui-artifact-previous", &backend);
    let discover = requests.try_recv().unwrap();
    app.project_response(
        discover.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({})),
        &backend,
    );
    let list = requests.try_recv().unwrap();
    assert_eq!(list.params["offset"], 2);
    app.project_response(
        list.tag.splitn(3, ':').nth(2).unwrap(),
        &Err(AppError::new("failed", "read failed")),
        &backend,
    );
    assert_eq!(app.workbench.project.artifact_offset, 5);
    app.project_action("project-ui-artifact-previous", &backend);
    let discover = requests.try_recv().unwrap();
    app.project_response(
        discover.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({})),
        &backend,
    );
    let list = requests.try_recv().unwrap();
    assert_eq!(list.params["offset"], 2);
    app.project_response(
        list.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"artifacts":[],"next":5})),
        &backend,
    );
    assert_eq!(app.workbench.project.artifact_offset, 2);
}

#[test]
fn external_change_requires_full_file_and_explicit_decision_before_acceptance() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("project-external-change".into());
    let query = json!({"id":Uuid::new_v4(),"name":"q","path":"q.sql","connection_alias":"db"});
    let manifest =
        json!({"queries":[query],"connections":[{"alias":"db","provider":"mysql"}],"transfers":[]});
    app.workbench.project.project =
        Some(json!({"project_id":Uuid::new_v4(),"manifest":manifest,"digests":{"q.sql":"old"}}));
    app.workbench.project.baseline_manifest = Some(manifest.clone());
    app.workbench.project.external = Some(json!({"manifest":manifest,"digests":{"q.sql":"new"}}));
    app.project_set("query-path", "q.sql");
    app.project_set("query-name", "q");
    app.project_set("query-alias", "db");
    app.project_set("query-text", "local");
    app.project_action("project-ui-external-accept", &backend);
    assert!(requests.try_recv().is_err());
    app.project_set("external-path", "q.sql");
    app.project_action("project-ui-external-file", &backend);
    let read = requests.try_recv().unwrap();
    app.project_response(
        read.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"text":"external ","next":9,"digests":{"q.sql":"new"}})),
        &backend,
    );
    app.project_action("project-ui-change-decision:queries:0:merge", &backend);
    assert!(app.workbench.project.change_decisions.is_empty());
    assert_eq!(app.project_value("query-text"), "local");
    app.project_action("project-ui-external-next", &backend);
    let read = requests.try_recv().unwrap();
    app.project_response(
        read.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"text":"body","next":null,"digests":{"q.sql":"new"}})),
        &backend,
    );
    app.project_action("project-ui-change-decision:queries:0:merge", &backend);
    assert_eq!(app.project_value("query-text"), "external body");
    app.project_action("project-ui-external-accept", &backend);
    let accept = requests.try_recv().unwrap();
    app.project_response(
        accept.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"digests":{"q.sql":"new"}})),
        &backend,
    );
    app.project_action("project-ui-save-prepare", &backend);
    let begin = requests.try_recv().unwrap();
    app.project_response(
        begin.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"upload_id":Uuid::new_v4(),"revision":1})),
        &backend,
    );
    let chunk = requests.try_recv().unwrap();
    assert_eq!(chunk.params["path"], "q.sql");
    assert_eq!(chunk.params["text"], "external body");
}

#[test]
fn accepting_external_deletion_keeps_other_draft_and_uploads_closed_manifest() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("project-delete-save".into());
    let deleted =
        json!({"id":Uuid::new_v4(),"name":"deleted","path":"gone.sql","connection_alias":"db"});
    let kept = json!({"id":Uuid::new_v4(),"name":"kept","path":"keep.sql","connection_alias":"db"});
    let manifest = json!({"queries":[deleted,kept],"connections":[{"alias":"db","provider":"mysql"}],"transfers":[]});
    let external =
        json!({"queries":[kept],"connections":[{"alias":"db","provider":"mysql"}],"transfers":[]});
    app.workbench.project.project = Some(
        json!({"project_id":Uuid::new_v4(),"manifest":manifest,"digests":{"gone.sql":"old","keep.sql":"same"}}),
    );
    app.workbench.project.baseline_manifest = Some(manifest);
    app.workbench.project.external =
        Some(json!({"manifest":external,"digests":{"keep.sql":"same"}}));
    app.workbench
        .project
        .transfer_files
        .insert("gone.sql".into(), "stale".into());
    app.project_set("query-path", "keep.sql");
    app.project_set("query-name", "kept");
    app.project_set("query-alias", "db");
    app.project_set("query-text", "local edit");
    app.project_action("project-ui-delete-decision:queries:0:delete", &backend);
    app.project_action("project-ui-external-accept", &backend);
    let accept = requests.try_recv().unwrap();
    app.project_response(
        accept.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"digests":{"keep.sql":"same"}})),
        &backend,
    );
    app.project_action("project-ui-save-prepare", &backend);
    let begin = requests.try_recv().unwrap();
    app.project_response(
        begin.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"upload_id":Uuid::new_v4(),"revision":1})),
        &backend,
    );
    let chunk = requests.try_recv().unwrap();
    assert_eq!(chunk.params["path"], "keep.sql");
    assert_eq!(chunk.params["text"], "local edit");
    app.project_response(
        chunk.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"revision":2,"offset":10})),
        &backend,
    );
    let prepare = requests.try_recv().unwrap();
    assert_eq!(prepare.method, "project.save.prepare");
    assert_eq!(
        prepare.params["manifest"]["queries"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(prepare.params["manifest"]["queries"][0]["path"], "keep.sql");
}
