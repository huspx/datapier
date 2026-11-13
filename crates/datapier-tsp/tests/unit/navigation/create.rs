use super::*;

#[test]
fn database_creation_requires_an_existing_writable_mysql_session() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.session = None;
    app.action(&format!("profile-create-database:{}", profile.id), &backend);
    assert!(requests.try_recv().is_err());
    assert!(app.navigation.create_database.is_none());
    app.session = Some(json!({"session_id":Uuid::new_v4(),"profile":profile}));
    app.profiles[0].access_mode = datapier_core::model::AccessMode::ReadOnly;
    app.action(&format!("profile-create-database:{}", profile.id), &backend);
    assert!(requests.try_recv().is_err());
    assert!(app.navigation.create_database.is_none());
}

#[test]
fn incomplete_server_metadata_never_enables_database_creation() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.action(&format!("profile-create-database:{}", profile.id), &backend);
    let open = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: open.tag,
            result: Ok(json!({"session_id":Uuid::new_v4(),"profile":profile})),
        },
        &backend,
    );
    let options = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: options.tag,
            result: Ok(json!({"charset":"utf8mb4","collation":"utf8mb4_bin","collations":[]})),
        },
        &backend,
    );
    let form = app.navigation_database_create().unwrap();
    assert!(form.options.is_none());
    assert!(form.error.is_some());
    app.field_set("database-create-name", "test_db");
    app.navigation_poll_create_database(&backend);
    assert!(requests.try_recv().is_err());
}

#[test]
fn cancelling_database_form_closes_session_without_discard_confirmation() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.action(&format!("profile-create-database:{}", profile.id), &backend);
    let open = requests.try_recv().unwrap();
    let session = Uuid::new_v4();
    app.navigation_response(
        &Response {
            tag: open.tag,
            result: Ok(json!({"session_id":session,"profile":profile})),
        },
        &backend,
    );
    let options = requests.try_recv().unwrap();
    app.navigation_response(&Response { tag: options.tag, result: Ok(json!({"charset":"utf8mb4","collation":"utf8mb4_bin","collations":[{"charset":"utf8mb4","name":"utf8mb4_bin","default":true}]})) }, &backend);
    app.field_set("database-create-name", "draft_db");
    let form = app.navigation.create_database.as_mut().unwrap();
    form.charset = "utf8mb4".into();
    form.plan = Some(json!({"id":Uuid::new_v4(),"revision":1}));
    app.action("overlay-close", &backend);
    assert!(!app.workbench.overlay_close_confirm);
    let close = requests.try_recv().unwrap();
    assert_eq!(close.method, "connection.close");
    assert_eq!(close.params["session_id"], json!(session));
    app.navigation_response(
        &Response {
            tag: close.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(app.navigation.create_database.is_none());
    assert!(app.workbench.overlay.is_none());
    assert_eq!(requests.try_recv().unwrap().method, "schema.discard");
}

#[test]
fn database_creation_previews_frozen_sql_and_preserves_unknown_outcome() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.navigation_action(&format!("profile-create-database:{}", profile.id), &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.method, "connection.open");
    let session = Uuid::new_v4();
    app.navigation_response(
        &Response {
            tag: open.tag,
            result: Ok(json!({"session_id":session,"profile":profile})),
        },
        &backend,
    );
    let options = requests.try_recv().unwrap();
    assert_eq!(options.method, "schema.describe");
    app.navigation_response(
        &Response {
            tag: options.tag,
            result: Ok(json!({"charset":"utf8mb4","collation":"utf8mb4_bin","collations":[{"charset":"utf8mb4","name":"utf8mb4_bin","default":true}]})),
        },
        &backend,
    );
    app.field_set("database-create-name", "new_db");
    app.navigation_poll_create_database(&backend);
    app.navigation.create_database.as_mut().unwrap().changed_at =
        Instant::now() - Duration::from_secs(1);
    app.navigation_poll_create_database(&backend);
    let plan = requests.try_recv().unwrap();
    assert_eq!(plan.method, "schema.plan");
    assert_eq!(plan.params["intent"]["target"]["name"], "new_db");
    assert_eq!(plan.params["intent"]["target"]["database"], "new_db");
    app.field_set("database-create-name", "newer_db");
    app.navigation_response(&Response { tag: plan.tag, result: Ok(json!({"id":Uuid::new_v4(),"revision":1,"plan_digest":"stale","submittable":true,"complete":true,"total_items":1})) }, &backend);
    assert_eq!(requests.try_recv().unwrap().method, "schema.discard");
    app.navigation_action("database-create-submit", &backend);
    assert!(requests.try_recv().is_err());
    app.field_set("database-create-name", "new_db");
    app.navigation_poll_create_database(&backend);
    app.navigation.create_database.as_mut().unwrap().changed_at =
        Instant::now() - Duration::from_secs(1);
    app.navigation_poll_create_database(&backend);
    let plan = requests.try_recv().unwrap();
    let plan_id = Uuid::new_v4();
    let summary = json!({"id":plan_id,"revision":1,"plan_digest":"digest","submittable":true,"complete":true,"total_items":1});
    app.navigation_response(
        &Response {
            tag: plan.tag,
            result: Ok(summary.clone()),
        },
        &backend,
    );
    let preview = requests.try_recv().unwrap();
    assert_eq!(preview.method, "schema.preview");
    let mut preview_value = summary;
    preview_value["offset"] = json!(0);
    preview_value["steps"] = json!([{"sql":"CREATE DATABASE `new_db`"}]);
    preview_value["next"] = Value::Null;
    app.navigation_response(
        &Response {
            tag: preview.tag,
            result: Ok(preview_value),
        },
        &backend,
    );
    assert_eq!(
        app.navigation_database_create().unwrap().sql.as_deref(),
        Some("CREATE DATABASE `new_db`")
    );
    app.field_set("database-create-name", "changed");
    app.navigation_action("database-create-submit", &backend);
    assert!(requests.try_recv().is_err());
    app.field_set("database-create-name", "new_db");
    app.navigation_action("database-create-submit", &backend);
    let submit = requests.try_recv().unwrap();
    assert_eq!(submit.method, "schema.submit");
    app.navigation_response(
        &Response {
            tag: submit.tag,
            result: Err(AppError::new("outcome_unknown", "unknown")),
        },
        &backend,
    );
    assert!(app.navigation_database_create().unwrap().unknown);
    app.navigation_action("database-create-submit", &backend);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.field("database-create-name"), "new_db");
}
