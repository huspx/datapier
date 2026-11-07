use super::*;

#[test]
fn archery_database_selection_preserves_idle_query_and_changes_execution_target() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, _, session) = archery_app("mysql", &["fixture", "fixture_second"]);
    app.database = Some("fixture".into());
    app.tabs[0].browse.database = app.database.clone();
    app.tabs[0].query = Editor::new("SELECT 42");
    let generation = app.generation;

    app.navigation_action("navigation-db:hidden", &backend);
    assert!(requests.try_recv().is_err());
    assert_eq!(app.database.as_deref(), Some("fixture"));
    assert_eq!(app.generation, generation);

    app.navigation_action("navigation-db:fixture_second", &backend);
    let list = requests.try_recv().unwrap();
    assert_eq!(list.method, "resource.list");
    assert_eq!(list.params["database"], "fixture_second");
    assert!(requests.try_recv().is_err());
    assert_eq!(app.tabs.len(), 1);
    assert_eq!(app.tabs[0].query.text, "SELECT 42");
    assert_eq!(app.database.as_deref(), Some("fixture_second"));
    assert_eq!(
        app.tabs[0].browse.database.as_deref(),
        Some("fixture_second")
    );
    assert_eq!(app.generation, generation.wrapping_add(1));
    assert!(app.dirty);

    app.execute_query_text(app.tabs[0].query.text.clone(), &backend);
    let execution = requests.try_recv().unwrap();
    assert_eq!(execution.method, "execution.start");
    assert_eq!(execution.params["database"], "fixture_second");
    assert_eq!(execution.params["text"], "SELECT 42");
    assert_eq!(execution.params["session_id"], json!(session));
}

#[test]
fn archery_database_selection_isolates_active_work_in_new_tab() {
    for condition in ["task", "operation", "resource", "draft", "inline"] {
        let (backend, mut requests, _) = Backend::scripted();
        let (mut app, _, session) = archery_app("mysql", &["fixture", "fixture_second"]);
        app.database = Some("fixture".into());
        app.tabs[0].browse.database = app.database.clone();
        app.tabs[0].query = Editor::new("SELECT 42");
        match condition {
            "task" => app.task = Some("running".into()),
            "operation" => app.operation_pending = true,
            "resource" => {
                app.target = Some(ResourceTarget {
                    database: Some("fixture".into()),
                    name: "items".into(),
                    kind: "table".into(),
                    key_b64: None,
                })
            }
            "draft" => app.pending_rows.push(RowChange {
                operation: "insert".into(),
                key: Default::default(),
                before: Default::default(),
                values: Default::default(),
            }),
            "inline" => {
                app.workbench.inline = Some(Cell { row: 0, column: 0 });
                app.field_set("cell", "draft text");
            }
            _ => unreachable!(),
        }

        app.navigation_action("navigation-db:fixture_second", &backend);
        let open = requests.try_recv().unwrap();
        assert_eq!(open.method, "connection.open", "{condition}");
        assert_eq!(open.params["database"], "fixture_second", "{condition}");
        let list = requests.try_recv().unwrap();
        assert_eq!(list.method, "resource.list", "{condition}");
        assert_eq!(list.params["session_id"], json!(session));
        assert!(requests.try_recv().is_err());
        assert_eq!(app.active, 1);
        assert_eq!(app.database.as_deref(), Some("fixture_second"));
        assert_eq!(
            app.tabs[1].browse.database.as_deref(),
            Some("fixture_second")
        );
        assert!(app.tabs[1].query.text.is_empty());

        app.switch_tab(0);
        assert_eq!(app.database.as_deref(), Some("fixture"), "{condition}");
        assert_eq!(app.tabs[0].query.text, "SELECT 42", "{condition}");
        assert_eq!(app.session_id(), Some(json!(session)));
        match condition {
            "task" => assert_eq!(app.task.as_deref(), Some("running")),
            "operation" => assert!(app.operation_pending),
            "resource" => assert_eq!(app.target.as_ref().unwrap().name, "items"),
            "draft" => assert_eq!(app.pending_rows.len(), 1),
            "inline" => {
                assert!(app.workbench.inline.is_some());
                assert_eq!(app.field("cell"), "draft text");
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn archery_navigation_limits_databases_and_opens_unknown_relations() {
    for section in ["structure", "ddl"] {
        let (backend, mut requests, _) = Backend::scripted();
        let (mut app, profile, _) = archery_app("mysql", &["shop"]);
        app.navigation_load_tree(&backend, false);
        let list = requests.try_recv().unwrap();
        assert_eq!(list.method, "resource.list");
        assert!(list.params["database"].is_null());
        app.navigation_response(
            &Response {
                tag: list.tag,
                result: Ok(json!({"resources":[
                {"name":"shop","kind":"database"},
                {"name":"hidden-db","kind":"database"}
            ],"cursor":null,"complete":true})),
            },
            &backend,
        );
        app.navigation_action("navigation-db:hidden-db", &backend);
        assert!(requests.try_recv().is_err());
        app.navigation_action("navigation-db:shop", &backend);
        let list = requests.try_recv().unwrap();
        assert_eq!(list.params["database"], "shop");
        app.navigation_response(&Response {
            tag: list.tag,
            result: Ok(json!({"resources":[{"name":"unknown-relation","kind":"unknown"}],"cursor":null,"complete":true})),
        }, &backend);
        let view = crate::views::render_window(&app, Some(Uuid::new_v4()));
        assert!(
            view.regions()
                .into_iter()
                .flatten()
                .any(|node| tree_has_text(node, "shop"))
        );
        assert!(
            !view
                .regions()
                .into_iter()
                .flatten()
                .any(|node| tree_has_text(node, "hidden-db"))
        );
        assert!(
            view.regions()
                .into_iter()
                .flatten()
                .any(|node| tree_has_text(node, "unknown-relation"))
        );
        app.navigation_action(
            &format!("navigation-resource-section:{section}:shop:0"),
            &backend,
        );
        assert_eq!(app.target.as_ref().unwrap().name, "unknown-relation");
        assert_eq!(app.target.as_ref().unwrap().kind, "unknown");
        assert_eq!(app.workbench.section, section);
        assert_eq!(requests.try_recv().unwrap().method, "resource.describe");
        assert_eq!(app.profile.as_ref().unwrap().id, profile.id);
    }
}

#[test]
fn archery_redis_switch_only_opens_selected_database_without_key_listing() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, old_session) = archery_app("redis", &["0", "3"]);
    app.workbench.cli = true;
    app.navigation_action("redis-db:2", &backend);
    assert!(requests.try_recv().is_err());
    app.navigation_action("redis-db:3", &backend);
    let open = requests.try_recv().unwrap();
    assert_eq!(open.method, "connection.open");
    assert_eq!(open.params["redis_db"], 3);
    let new_session = Uuid::new_v4();
    app.navigation_response(
        &Response {
            tag: open.tag,
            result: Ok(json!({"session_id":new_session,"profile":profile,"redis_db":3})),
        },
        &backend,
    );
    assert_eq!(app.tabs[0].browse.redis_db, Some(3));
    assert!(app.workbench.cli);
    let close = requests.try_recv().unwrap();
    assert_eq!(close.method, "connection.close");
    assert_eq!(close.params["session_id"], json!(old_session));
    assert!(requests.try_recv().is_err());
}

#[test]
fn archery_rejects_database_and_table_deletion() {
    let (backend, mut requests, _) = Backend::scripted();
    let (mut app, profile, _) = archery_app("mysql", &["shop"]);
    let tree = app.navigation.databases.entry(profile.id).or_default();
    tree.databases =
        vec![serde_json::from_value(json!({"name":"shop","kind":"database"})).unwrap()];
    tree.nodes.entry("shop".into()).or_default().resources =
        vec![serde_json::from_value(json!({"name":"orders","kind":"table"})).unwrap()];
    app.navigation_action("navigation-drop-db:shop", &backend);
    assert!(app.navigation_drop_confirmation().is_none());
    app.navigation_action("navigation-drop-table:shop:0", &backend);
    assert!(app.navigation_drop_confirmation().is_none());
    assert!(requests.try_recv().is_err());
}
