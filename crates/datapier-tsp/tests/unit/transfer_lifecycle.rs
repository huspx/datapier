//! 迁移计划与在途回复的生命周期回归。
use super::*;

fn restore_app(backend: &Backend) -> App {
    let mut app = App::new("transfer-lifecycle".into());
    app.transfer_action("transfer-new:sql_restore", backend);
    app.transfer_set("destination-profile", Uuid::new_v4().to_string());
    app.transfer_set("file-0", "first.sql");
    app
}
#[test]
fn missing_and_readonly_targets_return_to_source_form() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    app.transfer_set("destination-profile", "");
    app.transfer_action("transfer-ui-prepare", &backend);
    assert_eq!(app.workbench.transfer.stage, "source");
    assert_eq!(
        app.workbench.field_error.as_ref().unwrap().field,
        app.transfer_key("destination-profile")
    );
    assert!(requests.try_recv().is_err());

    app.transfer_set("destination-profile", Uuid::new_v4().to_string());
    app.transfer_action("transfer-ui-prepare", &backend);
    let prepare = requests.try_recv().unwrap();
    let id = Uuid::new_v4();
    app.transfer_response(
        prepare.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"id":id,"revision":1,"state":"preparing"})),
        &backend,
    );
    app.transfer_action("transfer-ui-refresh-plan", &backend);
    let poll = requests.try_recv().unwrap();
    app.transfer_response(poll.tag.splitn(3, ':').nth(2).unwrap(), &Ok(json!({"id":id,"revision":1,"state":"failed","error":{"code":"transfer_target_read_only"}})), &backend);
    assert_eq!(app.workbench.transfer.stage, "source");
    assert_eq!(
        app.workbench.field_error.as_ref().unwrap().field,
        app.transfer_key("destination-profile")
    );
}

#[test]
fn late_prepare_is_discarded_before_new_prepare() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    app.transfer_action("transfer-ui-prepare", &backend);
    let first = requests.try_recv().unwrap();
    app.transfer_set("file-0", "second.sql");
    app.workbench.transfer.invalidate();
    app.transfer_action("transfer-ui-prepare", &backend);
    assert!(requests.try_recv().is_err());
    let id = Uuid::new_v4();
    app.transfer_response(
        first.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"id":id,"revision":2,"state":"ready"})),
        &backend,
    );
    assert!(app.workbench.transfer.plan.is_none());
    let discard = requests.try_recv().unwrap();
    assert_eq!(discard.method, "transfer.discard");
    assert_eq!(discard.params["plan_id"], json!(id));
    assert_eq!(discard.params["revision"], 2);
    app.transfer_response(
        discard.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({})),
        &backend,
    );
    let next = requests.try_recv().unwrap();
    assert_eq!(next.method, "transfer.prepare");
    assert_eq!(next.params["spec"]["inputs"][0]["path"], "second.sql");
}

#[test]
fn late_submit_keeps_run_and_waits_for_its_completion() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    let plan_id = Uuid::new_v4();
    let run_id = Uuid::new_v4();
    let task_id = Uuid::new_v4();
    let spec = json!({"inputs":[{"path":"first.sql"}]});
    app.workbench.transfer.plan =
        Some(json!({"id":plan_id,"revision":1,"state":"ready","spec":spec}));
    app.transfer_action("transfer-ui-submit", &backend);
    let submit = requests.try_recv().unwrap();
    app.transfer_set("file-0", "second.sql");
    app.workbench.transfer.invalidate();
    app.transfer_action("transfer-ui-prepare", &backend);
    app.transfer_response(
        submit.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"run_id":run_id,"task_id":task_id,"state":"running"})),
        &backend,
    );
    assert_eq!(app.workbench.transfer.run_spec, Some(spec));
    assert_eq!(
        app.workbench.transfer.run.as_ref().unwrap()["run_id"],
        json!(run_id)
    );
    assert!(requests.try_recv().is_err());
    app.transfer_action("transfer-ui-cancel", &backend);
    let cancel = requests.try_recv().unwrap();
    assert_eq!(cancel.params["task_id"], json!(task_id));
    app.transfer_response(
        cancel.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({})),
        &backend,
    );
    app.transfer_action("transfer-ui-refresh-run", &backend);
    let poll = requests.try_recv().unwrap();
    app.transfer_response(
        poll.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"run_id":run_id,"state":"cancelled"})),
        &backend,
    );
    let discard = requests.try_recv().unwrap();
    assert_eq!(discard.method, "transfer.discard");
    assert_eq!(discard.params["plan_id"], json!(plan_id));
}

#[test]
fn late_selection_update_discards_new_revision_and_excludes_database_items() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    let id = Uuid::new_v4();
    let object = json!({"database":"db","name":"t","kind":"table"});
    app.workbench.transfer.plan = Some(json!({"id":id,"revision":1,"state":"ready","items":[
        {"id":"database","phase":"database","selected":true},
        {"id":"table","phase":"restore","source":object,"target":object,"selected":true}
    ]}));
    app.transfer_action("transfer-ui-item:0", &backend);
    assert!(requests.try_recv().is_err());
    app.transfer_action("transfer-ui-item:1", &backend);
    let update = requests.try_recv().unwrap();
    assert_eq!(update.params["item_selection"][0]["id"], "table");
    app.workbench.transfer.invalidate();
    app.transfer_action("transfer-ui-prepare", &backend);
    app.transfer_response(
        update.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"id":id,"revision":2,"state":"ready"})),
        &backend,
    );
    let discard = requests.try_recv().unwrap();
    assert_eq!(discard.method, "transfer.discard");
    assert_eq!(discard.params["revision"], 2);
    assert!(app.workbench.transfer.plan.is_none());
}

#[test]
fn explicit_transfer_action_clears_previous_error_but_poll_does_not() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    app.workbench.transfer.run =
        Some(json!({"run_id":Uuid::new_v4(),"task_id":Uuid::new_v4(),"state":"running"}));
    app.status = i18n::UiMessage::raw("old error");
    app.transfer_action("transfer-ui-refresh-run", &backend);
    let poll = requests.try_recv().unwrap();
    assert_eq!(app.status.render(), "old error");
    app.transfer_response(
        poll.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"state":"running"})),
        &backend,
    );
    assert_eq!(app.status.render(), "old error");
    app.transfer_action("transfer-ui-cancel", &backend);
    assert!(app.status.is_empty());
}

#[test]
fn pruned_plan_does_not_block_queued_preparation() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    app.workbench.transfer.plan = Some(json!({"id":Uuid::new_v4(),"revision":1,"state":"ready"}));
    app.transfer_action("transfer-ui-prepare", &backend);
    let discard = requests.try_recv().unwrap();
    assert_eq!(discard.method, "transfer.discard");
    app.transfer_response(
        discard.tag.splitn(3, ':').nth(2).unwrap(),
        &Err(datapier_core::model::AppError::new("not_found", "pruned")),
        &backend,
    );
    assert!(app.workbench.transfer.obsolete_plan.is_none());
    assert!(app.workbench.transfer.queued_prepare.is_none());
    assert_eq!(requests.try_recv().unwrap().method, "transfer.prepare");
}

#[test]
fn explicit_empty_redis_key_does_not_become_pattern_scan() {
    let (backend, _, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    app.transfer_set("mode", "copy");
    app.transfer_set("provider", "redis");
    app.transfer_set("source-profile", Uuid::new_v4().to_string());
    app.transfer_set("redis-selection", "keys");
    app.transfer_set("redis-keys", "");
    let spec = app.transfer_spec().unwrap();
    assert_eq!(spec["redis"]["keys_b64"], json!([""]));
    assert!(spec["redis"]["pattern"].is_null());
}

#[test]
fn loaded_csv_options_and_ranges_survive_preparation() {
    let (backend, _, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    let profile = Uuid::new_v4();
    app.transfer_load_preset(json!({"name":"csv","spec":{
        "mode":"import","provider":"mysql","destination":{"profile_id":profile},
        "inputs":[{"path":"data.csv","format":"csv","csv":{"encoding":"utf-8","delimiter":",","header":false,"columns":["a","b"],"null_marker":"NULL","backslash_escape":false,"binary_columns":["b"]},"record_ranges":[{"first":1,"last":3},{"first":7,"last":9}]}],
        "options":{"conflict":"insert","conversion_errors":"stop"}
    }}));
    let spec = app.transfer_spec().unwrap();
    assert_eq!(
        spec["inputs"][0]["csv"],
        json!({"encoding":"utf-8","delimiter":",","header":false,"columns":["a","b"],"null_marker":"NULL","backslash_escape":false,"binary_columns":["b"]})
    );
    assert_eq!(
        spec["inputs"][0]["record_ranges"],
        json!([{"first":1,"last":3},{"first":7,"last":9}])
    );
    assert!(spec["destination"]["redis_db"].is_null());
}

#[test]
fn project_new_cannot_replace_unsubmitted_plan() {
    let (backend, _, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    let id = Uuid::new_v4();
    app.workbench.transfer.plan = Some(json!({"id":id,"revision":1,"state":"ready"}));
    app.transfer_action("transfer-project-new:copy", &backend);
    assert_eq!(
        app.workbench.transfer.plan.as_ref().unwrap()["id"],
        json!(id)
    );
    assert_eq!(app.transfer_value("file-0"), "first.sql");
}

#[test]
fn preset_pages_request_bounded_offsets() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    app.transfer_action("transfer-ui-preset-list", &backend);
    let first = requests.try_recv().unwrap();
    assert_eq!(first.params["offset"], 0);
    assert_eq!(first.params["limit"], 100);
    app.transfer_response(
        first.tag.splitn(3, ':').nth(2).unwrap(),
        &Ok(json!({"items":vec![json!({}); 100]})),
        &backend,
    );
    app.transfer_action("transfer-ui-preset-next", &backend);
    let second = requests.try_recv().unwrap();
    assert_eq!(second.params["offset"], 100);
    assert_eq!(second.params["limit"], 100);
}

#[test]
fn invalid_csv_and_mapping_focus_their_fields() {
    let (backend, _, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    app.transfer_set("mode", "import");
    app.transfer_set("file-format-0", "csv");
    app.transfer_set("file-delimiter-0", "::");
    app.transfer_action("transfer-ui-prepare", &backend);
    let field = app.transfer_key("file-delimiter-0");
    assert_eq!(app.workbench.field_error.as_ref().unwrap().field, field);
    assert_eq!(app.workbench.transfer.stage, "source");
    assert_eq!(
        app.workbench.focus_request.as_deref(),
        Some(format!("input-{field}").as_str())
    );
    app.transfer_set("file-delimiter-0", ",");
    app.transfer_set("file-mapping-0", "invalid");
    app.transfer_action("transfer-ui-prepare", &backend);
    assert_eq!(
        app.workbench.field_error.as_ref().unwrap().field,
        app.transfer_key("file-mapping-0")
    );
}

#[test]
fn preset_requires_new_trigger_confirmation() {
    let (backend, _, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    app.transfer_load_preset(json!({"name":"restore","spec":{"mode":"sql_restore","provider":"mysql","options":{"existing_triggers_confirmed":true}}}));
    assert_eq!(app.transfer_value("triggers-confirmed"), "false");
}

#[test]
fn invalid_redis_key_and_numeric_filter_focus_object_fields() {
    let (backend, _, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    app.transfer_set("mode", "copy");
    app.transfer_set("source-profile", Uuid::new_v4().to_string());
    app.transfer_set("provider", "redis");
    app.transfer_set("redis-selection", "keys");
    app.transfer_set("redis-keys", "not base64!");
    app.transfer_action("transfer-ui-prepare", &backend);
    assert_eq!(app.workbench.transfer.stage, "objects");
    assert_eq!(
        app.workbench.field_error.as_ref().unwrap().field,
        app.transfer_key("redis-keys")
    );
    app.transfer_set("provider", "mysql");
    app.transfer_set("object-source-0", "items");
    app.transfer_set("object-filter-column-0-0", "id");
    app.transfer_set("object-filter-type-0-0", "integer");
    app.transfer_set("object-filter-value-0-0", "oops");
    app.transfer_action("transfer-ui-prepare", &backend);
    assert_eq!(
        app.workbench.field_error.as_ref().unwrap().field,
        app.transfer_key("object-filter-value-0-0")
    );
}

#[test]
fn object_mapping_preserves_default_columns_and_rejects_empty_projection() {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    let (backend, _, _) = Backend::scripted();
    let mut app = restore_app(&backend);
    app.transfer_set("mode", "copy");
    app.transfer_set("source-profile", Uuid::new_v4().to_string());
    app.transfer_set("source-database", "source_db");
    app.transfer_set("destination-database", "target_db");
    app.transfer_set("object-source-0", "orders");
    app.transfer_set("object-target-0", "archive");
    app.workbench.transfer.catalog.insert(
        "source:columns:source_db:orders".into(),
        TransferCatalog {
            value: Some(json!({"columns":[{"name":"id"},{"name":"email"},{"name":"total"}]})),
            ..Default::default()
        },
    );
    let action = |source: &str, target: &str| {
        format!(
            "transfer-map:object:0:{}:{}",
            URL_SAFE_NO_PAD.encode(source),
            URL_SAFE_NO_PAD.encode(target)
        )
    };
    app.transfer_action(&action("id", "order_id"), &backend);
    assert_eq!(
        app.transfer_spec().unwrap()["objects"][0]["columns"],
        json!([
            {"source":"email","target":"email"},
            {"source":"id","target":"order_id"},
            {"source":"total","target":"total"}
        ])
    );
    app.transfer_action(&action("email", "\0"), &backend);
    assert_eq!(
        app.transfer_spec().unwrap()["objects"][0]["columns"],
        json!([
            {"source":"id","target":"order_id"},
            {"source":"total","target":"total"}
        ])
    );
    app.transfer_action(&action("id", "\0"), &backend);
    app.transfer_action(&action("total", "\0"), &backend);
    assert_eq!(app.transfer_spec().unwrap_err().0, "object-mapping-0");
}
