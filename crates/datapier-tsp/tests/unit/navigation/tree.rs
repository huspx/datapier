use super::*;

#[test]
fn closing_profile_discards_tree_and_late_results_then_reloads_after_reconnect() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile) = mysql_drop_app();
    app.navigation_load_tree(&backend, true);
    let stale = requests.try_recv().unwrap();
    app.action(&format!("profile-close:{}", profile.id), &backend);
    let close = requests.try_recv().unwrap();
    app.tabs_close_response(
        &Response {
            tag: close.tag,
            result: Ok(json!({})),
        },
        &backend,
    );
    assert!(!app.navigation.databases.contains_key(&profile.id));
    app.navigation_response(&Response { tag: stale.tag, result: Ok(json!({"resources":[{"name":"stale","kind":"database"}],"cursor":null,"complete":true})) }, &backend);
    assert!(!app.navigation.databases.contains_key(&profile.id));
    app.select_profile(0, &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.method, "connection.open");
    app.session = Some(json!({"session_id":Uuid::new_v4(),"profile":profile}));
    app.navigation_load_tree(&backend, false);
    let fresh = requests.try_recv().unwrap();
    app.navigation_response(&Response { tag: fresh.tag, result: Ok(json!({"resources":[{"name":"fresh","kind":"database"}],"cursor":null,"complete":true})) }, &backend);
    assert_eq!(
        app.navigation.databases[&profile.id].databases[0].name,
        "fresh"
    );
}

#[test]
fn database_refresh_replaces_only_the_selected_database_and_section_targets_table() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, mut profile, session) = redis_app();
    profile.provider = datapier_core::model::ProviderKind::Mysql;
    app.profile = Some(profile.clone());
    app.profiles[0] = profile.clone();
    let tree = app.navigation.databases.entry(profile.id).or_default();
    tree.nodes.entry("other".into()).or_default().resources =
        vec![serde_json::from_value(json!({"name":"untouched","kind":"table"})).unwrap()];
    tree.nodes.entry("commerce".into()).or_default();
    app.navigation_action("navigation-db-refresh:commerce", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.params["database"], "commerce");
    assert_eq!(request.params["session_id"], json!(session));
    app.navigation_response(&Response { tag: request.tag, result: Ok(json!({"resources":[{"name":"customers","kind":"table"}],"cursor":null,"complete":true})) }, &backend);
    assert_eq!(
        app.navigation.databases[&profile.id].nodes["other"].resources[0].name,
        "untouched"
    );
    app.navigation_action("navigation-resource-section:ddl:commerce:0", &backend);
    assert_eq!(app.target.as_ref().unwrap().name, "customers");
    assert_eq!(
        app.target.as_ref().unwrap().database.as_deref(),
        Some("commerce")
    );
    assert_eq!(app.workbench.section, "ddl");
    let describe = requests.try_recv().unwrap();
    assert_eq!(describe.method, "resource.describe");
    assert_eq!(describe.params["target"]["name"], "customers");
}
#[test]
fn queued_section_is_applied_only_after_the_matching_target_loads() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, mut profile, _) = redis_app();
    profile.provider = datapier_core::model::ProviderKind::Mysql;
    app.profile = Some(profile.clone());
    app.profiles[0] = profile.clone();
    app.tabs[0].query = Editor::new("select 1");
    app.navigation
        .databases
        .entry(profile.id)
        .or_default()
        .nodes
        .entry("shop".into())
        .or_default()
        .resources = vec![serde_json::from_value(json!({"name":"orders","kind":"table"})).unwrap()];
    app.navigation_action("navigation-resource-section:structure:shop:0", &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.method, "connection.open");
    app.routed_response(
        Response {
            tag: open.tag,
            result: Ok(json!({"session_id":Uuid::new_v4(),"profile":profile})),
        },
        &backend,
    );
    app.navigation_apply_queued_section();
    assert_eq!(app.target.as_ref().unwrap().name, "orders");
    assert_eq!(
        app.target.as_ref().unwrap().database.as_deref(),
        Some("shop")
    );
    assert_eq!(app.workbench.section, "structure");
    let mut described = false;
    while let Ok(request) = requests.try_recv() {
        if request.method == "resource.describe" {
            assert_eq!(request.params["target"]["name"], "orders");
            described = true;
        }
    }
    assert!(described);
}
#[test]
fn direct_database_node_only_expands() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _) = mysql_drop_app();
    app.database = Some("fixture".into());
    app.tabs[0].browse.database = app.database.clone();
    app.navigation_action("navigation-db:db`;x", &backend);
    let list = requests.try_recv().unwrap();
    assert_eq!(list.method, "resource.list");
    assert_eq!(list.params["database"], "db`;x");
    assert_eq!(app.database.as_deref(), Some("fixture"));
    assert_eq!(app.tabs[0].browse.database.as_deref(), Some("fixture"));
    assert_eq!(app.tabs.len(), 1);
}

#[test]
fn existing_view_structure_describes_and_plans_edited_definition() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _) = mysql_drop_app();
    let target = ResourceTarget {
        database: Some("db".into()),
        name: "v".into(),
        key_b64: None,
        kind: "view".into(),
    };
    app.target = Some(target.clone());
    app.workbench_action("section:structure", &backend);
    let describe = requests.try_recv().unwrap();
    assert_eq!(describe.method, "schema.describe");
    assert_eq!(describe.params["target"], json!(target));
    let snapshot = json!({"target":target,"server_version":"8.4","sql_mode":"","ddl":"CREATE VIEW v AS SELECT 1","dependencies":[],"fingerprint":"view-baseline","capabilities":{},"completeness":{"state":"complete","reasons":[]},"definition":{"kind":"view","select":"SELECT 1","columns":[],"algorithm":"UNDEFINED","definer":null,"security":"DEFINER","check_option":null,"character_set_client":null,"collation_connection":null}});
    app.routed_response(
        Response {
            tag: describe.tag,
            result: Ok(snapshot),
        },
        &backend,
    );
    let state = app.advanced_schema.as_mut().unwrap();
    assert!(state.embedded);
    state
        .apply_operation(crate::advanced::schema::SchemaOperation::Set {
            path: "/select".into(),
            value: json!("SELECT 2"),
        })
        .unwrap();
    state
        .apply_operation(crate::advanced::schema::SchemaOperation::Set {
            path: "/security".into(),
            value: json!("INVOKER"),
        })
        .unwrap();
    let plan = state.plan().unwrap();
    assert_eq!(
        plan.params["intent"]["baseline"]["fingerprint"],
        "view-baseline"
    );
    assert_eq!(plan.params["intent"]["definition"]["select"], "SELECT 2");
    assert_eq!(plan.params["intent"]["definition"]["security"], "INVOKER");
}
