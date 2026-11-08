use super::*;
use crate::{App, Backend, Response};

fn target() -> ResourceTarget {
    ResourceTarget {
        database: Some("db".into()),
        name: "items".into(),
        key_b64: None,
        kind: "table".into(),
    }
}

#[test]
fn embedded_drafts_preserve_container_and_clean_states_are_not_saved() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("schema-draft".into());
    let session = json!(Uuid::new_v4());
    app.session = Some(json!({"session_id": session}));
    let mut editor = SchemaEditor::new(session.clone(), target(), "1".into(), None);
    editor.embedded = true;
    app.advanced_schema = Some(editor);
    assert!(app.advanced_schema_draft(0).is_none());
    let editor = app.advanced_schema.as_mut().unwrap();
    editor.dirty = true;
    editor.draft.baseline_fingerprint = Some("fingerprint".into());
    editor.draft.operations.push(SchemaOperation::Set {
        path: "/comment".into(),
        value: json!("draft"),
    });
    let saved = serde_json::to_value(app.advanced_schema_draft(0).unwrap()).unwrap();
    assert_eq!(saved["embedded"], true);
    let draft: SchemaDraft = serde_json::from_value(saved.clone()).unwrap();
    app.advanced_schema = None;
    app.view = "query".into();
    app.advanced_schema_restore(&backend, draft);
    assert_eq!(app.view, "data");
    assert_eq!(app.workbench.section, "structure");
    assert_eq!(app.session.as_ref().unwrap()["session_id"], session);
    let restored = app.advanced_schema.as_ref().unwrap();
    assert!(restored.embedded && restored.restore_confirmation_required);
    assert_eq!(restored.session, session);
    assert_eq!(restored.draft.operations.len(), 1);
    assert_eq!(requests.try_recv().unwrap().method, "schema.describe");
    let mut legacy = saved;
    legacy.as_object_mut().unwrap().remove("embedded");
    assert!(
        !serde_json::from_value::<SchemaDraft>(legacy)
            .unwrap()
            .embedded
    );
}

#[test]
fn schema_changed_keeps_submit_and_task_responses_routable() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("schema-event".into());
    let profile = Uuid::new_v4();
    app.tabs[0].profile_id = Some(profile);
    app.database = Some("db".into());
    app.target = Some(target());
    app.view = "data".into();
    let mut editor = SchemaEditor::new(json!(Uuid::new_v4()), target(), "1".into(), None);
    editor.embedded = true;
    editor.summary = Some(
        json!({"id":"plan","revision":1,"plan_digest":"digest","complete":true,"submittable":true}),
    );
    editor.preview = Some(json!({}));
    editor.preview_complete = true;
    let submit = editor.submit().unwrap();
    app.advanced_schema = Some(editor);
    app.advanced_schema_send(&backend, submit);
    let submitted = requests.try_recv().unwrap();
    let old_generation = app.generation;
    let event = || Response {
        tag: "execution.event".into(),
        result: Ok(
            json!({"kind":"schema.changed","payload":{"scope":{"profile_id":profile,"database":"db"}}}),
        ),
    };
    app.response(event(), &backend);
    assert_eq!(app.generation, old_generation.wrapping_add(1));
    assert!(requests.try_recv().is_err());
    let task = Uuid::new_v4().to_string();
    let execution = Uuid::new_v4().to_string();
    app.routed_response(
        Response {
            tag: submitted.tag,
            result: Ok(json!({"task_id":task,"execution_id":execution})),
        },
        &backend,
    );
    assert_eq!(app.task.as_deref(), Some(task.as_str()));
    assert!(app.advanced_schema.as_ref().unwrap().execution_pending);
    let task_generation = app.generation;
    app.response(event(), &backend);
    assert_eq!(app.generation, task_generation.wrapping_add(1));
    app.routed_response(
        Response {
            tag: format!(
                "{}:{}:execution.get/{}",
                app.tabs[0].id, task_generation, task
            ),
            result: Ok(json!({"id":task,"state":"outcome_unknown"})),
        },
        &backend,
    );
    assert!(app.advanced_schema.as_ref().unwrap().outcome_unknown);
    assert!(app.task.is_none());
}

#[test]
fn embedded_terminal_readback_keeps_structure_section_and_refreshes_after_generation_change() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("schema-terminal".into());
    let session = json!(Uuid::new_v4());
    app.session = Some(json!({"session_id": session}));
    app.database = Some("db".into());
    app.target = Some(target());
    app.view = "data".into();
    app.workbench.section = "structure".into();
    let task = Uuid::new_v4().to_string();
    let execution = Uuid::new_v4().to_string();
    let mut editor = SchemaEditor::new(session, target(), "1".into(), None);
    editor.embedded = true;
    editor.execution_pending = true;
    editor.execution = Some(json!({"task_id": task, "execution_id": execution}));
    app.task = Some(task.clone());
    app.advanced_schema = Some(editor);
    let generation = app.generation;
    let result = Ok(json!({"id": task, "state":"failed", "summary":{"state":"partial"}}));
    assert!(app.advanced_schema_task_response(&task, &result, &backend));
    assert_eq!(app.workbench.section, "structure");
    assert_eq!(app.generation, generation.wrapping_add(1));
    let mut readback = None;
    while let Ok(request) = requests.try_recv() {
        if request.method == "schema.describe" {
            readback = Some(request);
        }
    }
    let readback = readback.expect("schema readback must be requested after data reload");
    assert!(
        readback
            .tag
            .starts_with(&format!("{}:{}:", app.tabs[0].id, app.generation))
    );
}

#[test]
fn completed_schema_task_allows_new_browse_result_and_keeps_structure_readback() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("schema-after-submit".into());
    let profile = Uuid::new_v4();
    let session = json!(Uuid::new_v4());
    let task = Uuid::new_v4().to_string();
    let execution = Uuid::new_v4().to_string();
    app.tabs[0].profile_id = Some(profile);
    app.session = Some(json!({"session_id": session}));
    app.database = Some("db".into());
    app.target = Some(target());
    app.view = "data".into();
    app.workbench.section = "structure".into();
    app.browse_result = true;
    let mut editor = SchemaEditor::new(session, target(), "1".into(), None);
    editor.embedded = true;
    editor.dirty = true;
    editor.execution_pending = true;
    editor.execution = Some(json!({"task_id":task,"execution_id":execution}));
    app.advanced_schema = Some(editor);
    app.task = Some(task.clone());
    let before_event = app.generation;
    app.response(
        Response {
            tag: "execution.event".into(),
            result: Ok(json!({
                "kind":"schema.changed", "payload":{"scope":{"profile_id":profile,"database":"db"}}
            })),
        },
        &backend,
    );
    assert!(app.workbench.schema_invalidated_task);
    app.routed_response(Response {
        tag: format!("{}:{}:execution.get/{}", app.tabs[0].id, before_event, task),
        result: Ok(json!({"id":task,"state":"succeeded","summary":{"state":"applied","steps":[{"state":"applied"}]}})),
    }, &backend);
    assert!(!app.workbench.schema_invalidated_task);
    assert_eq!(app.workbench.section, "structure");
    let mut resource_describe = None;
    let mut schema_describe = None;
    while let Ok(request) = requests.try_recv() {
        match request.method.as_str() {
            "resource.describe" => resource_describe = Some(request),
            "schema.describe" => schema_describe = Some(request),
            _ => {}
        }
    }
    let schema_describe = schema_describe.expect("structure readback");
    let column_id = Uuid::new_v4().to_string();
    app.routed_response(Response { tag: schema_describe.tag, result: Ok(json!({
        "target":target(), "server_version":"8.4", "sql_mode":"", "ddl":"CREATE TABLE items (note text NOT NULL)",
        "dependencies":[], "fingerprint":"after", "capabilities":{},
        "completeness":{"state":"complete","reasons":[]},
        "definition":{"kind":"table","columns":[{"id":column_id,"name":"note","native_type":"text","nullable":false,
            "default":{"kind":"unspecified"},"on_update":null,"auto_increment":false,"generated":null,
            "charset":null,"collation":null,"visible":true,"comment":"","extra":[]}],
            "indexes":[],"constraints":[],"engine":"InnoDB","charset":null,"collation":null,"comment":"","options":[]}
    })) }, &backend);
    let actual = serde_json::to_value(
        app.advanced_schema
            .as_ref()
            .unwrap()
            .definition
            .as_ref()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(actual["columns"][0]["nullable"], false);
    app.routed_response(
        Response {
            tag: resource_describe.expect("resource readback").tag,
            result: Ok(json!({"target":target(),"columns":[],"keys":[],"details":{}})),
        },
        &backend,
    );
    let browse = requests.try_recv().unwrap();
    assert_eq!(browse.method, "resource.browse");
    let browse_task = Uuid::new_v4().to_string();
    let result_id = Uuid::new_v4().to_string();
    app.routed_response(
        Response {
            tag: browse.tag,
            result: Ok(json!({"task_id":browse_task})),
        },
        &backend,
    );
    app.routed_response(Response {
        tag: format!("{}:{}:execution.get/{}", app.tabs[0].id, app.generation, browse_task),
        result: Ok(json!({"id":browse_task,"state":"succeeded","method":"resource.browse","results":[result_id]})),
    }, &backend);
    assert!(app.browse_result);
    assert_eq!(app.result_id.as_deref(), Some(result_id.as_str()));
    assert_eq!(app.workbench.section, "structure");
    assert!(app.advanced_schema.as_ref().unwrap().definition.is_some());
}

#[test]
fn plan_response_focuses_preview_only_when_structure_is_visible() {
    for visible in [true, false] {
        let (backend, _, _) = Backend::scripted();
        let mut app = App::new("schema-preview-focus".into());
        app.view = if visible { "data" } else { "query" }.into();
        app.workbench.section = "structure".into();
        app.focus = "old-focus".into();
        app.workbench.focus_request = None;
        let definition: SchemaDefinition = serde_json::from_value(json!({
            "kind":"table","columns":[],"indexes":[],"constraints":[],"engine":"InnoDB",
            "charset":null,"collation":null,"comment":"","options":[]
        }))
        .unwrap();
        let mut editor =
            SchemaEditor::new(json!("session"), target(), "1".into(), Some(definition));
        editor.embedded = true;
        let request = editor.plan().unwrap();
        app.advanced_schema = Some(editor);
        let result = Ok(
            json!({"id":"plan","revision":1,"plan_digest":"digest","complete":true,"submittable":true,"total_items":0}),
        );
        app.advanced_schema_response(&request.tag, &result, &backend);
        assert_eq!(app.advanced_schema.as_ref().unwrap().section, "ddl_preview");
        if visible {
            assert_eq!(app.focus, "schema-section-ddl_preview");
            assert_eq!(app.workbench.focus_request.as_deref(), Some(""));
        } else {
            assert_eq!(app.focus, "old-focus");
            assert!(app.workbench.focus_request.is_none());
        }
    }
}
