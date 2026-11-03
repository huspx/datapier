use super::*;

#[test]
fn narrow_navigation_preserves_wide_preference_across_resize_and_restore() {
    let (backend, _, _) = Backend::scripted();
    let mut app = App::new("navigation".into());
    // 应用按终端列宽估算，此处位于 760px 窄屏边界内。
    let mut caps = tern_sdk::Capabilities {
        cols: 97,
        ..Default::default()
    };
    app.update_viewport(&caps);
    assert!(app.navigation_compact());
    app.action("navigation-toggle", &backend);
    assert!(!app.navigation_compact());
    assert!(app.workspace_snapshot().navigation_expanded);
    caps.cols = 98;
    app.update_viewport(&caps);
    assert!(!app.navigation_compact());
    app.action("navigation-toggle", &backend);
    assert!(app.navigation_compact());
    caps.cols = 97;
    app.update_viewport(&caps);
    assert!(app.navigation_compact());
    app.action("navigation-toggle", &backend);
    assert!(!app.navigation_compact());
    let state = serde_json::to_value(app.workspace_snapshot()).unwrap();
    let mut restored = App::new("navigation".into());
    restored.update_viewport(&caps);
    restored.response(
        Response {
            tag: "workspace.load".into(),
            result: Ok(json!({"revision":1,"state":state})),
        },
        &backend,
    );
    assert!(restored.navigation_compact());
    caps.cols = 98;
    restored.update_viewport(&caps);
    assert!(restored.navigation_compact());
    restored.action("navigation-toggle", &backend);
    assert!(!restored.navigation_compact());
    caps.cols = 97;
    restored.update_viewport(&caps);
    assert!(restored.navigation_compact());
}

#[test]
fn changing_scan_criteria_invalidates_only_the_current_tab_scan() {
    let mut app = App::new("test".into());
    second_tab(&mut app);
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    app.scan_token = Some(first);
    app.cursor = Some("first".into());
    app.switch_tab(1);
    app.scan_token = Some(second);
    app.cursor = Some("second".into());
    app.changed_field("filter");
    assert_eq!(app.scan_token, Some(second));
    app.changed_field("pattern");
    assert!(app.scan_token.is_none());
    assert!(app.cursor.is_none());
    app.switch_tab(0);
    assert_eq!(app.scan_token, Some(first));
    assert_eq!(app.cursor.as_deref(), Some("first"));
    app.changed_field("type");
    assert!(app.scan_token.is_none());
    assert!(app.cursor.is_none());
}

#[test]
fn grid_draft_prevents_resource_browse_request() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_with_grid_draft();
    app.action("browse", &backend);
    assert!(requests.try_recv().is_err());
    assert!(app.has_live_drafts());
}

#[test]
fn old_scan_response_does_not_overwrite_new_results() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("test".into());
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.action("list", &backend);
    let old = requests.try_recv().unwrap();
    app.field_set("pattern", "new*");
    app.changed_field("pattern");
    app.action("list", &backend);
    let new = requests.try_recv().unwrap();
    assert_ne!(old.tag, new.tag);

    app.routed_response(Response { tag: new.tag, result: Ok(json!({"resources": [{"name": "new", "kind": "table"}], "cursor": "new-cursor", "complete": false})) }, &backend);
    app.routed_response(Response { tag: old.tag, result: Ok(json!({"resources": [{"name": "old", "kind": "table"}], "cursor": "old-cursor", "complete": false})) }, &backend);
    assert_eq!(app.resources.len(), 1);
    assert_eq!(app.resources[0].name, "new");
    assert_eq!(app.cursor.as_deref(), Some("new-cursor"));
}

#[test]
fn selecting_resource_tab_leaves_history_without_losing_result_or_draft() {
    let mut app = app_with_grid_draft();
    let result = app.grid.as_ref().unwrap().page.result.id;
    app.workbench.overlay = Some("history".into());
    app.select_tab(0);
    assert!(app.workbench.overlay.is_none());
    assert_eq!(app.grid.as_ref().unwrap().page.result.id, result);
    assert!(app.has_live_drafts());

    second_tab(&mut app);
    app.workbench.overlay = Some("history".into());
    app.select_tab(1);
    app.select_tab(0);
    assert_eq!(
        app.grid.as_ref().unwrap().value(Cell { row: 0, column: 0 }),
        Some(&CellValue::Text("draft".into()))
    );
}

#[test]
fn refreshing_database_tree_reloads_expanded_objects_and_ignores_old_catalog() {
    fn page(name: &str, kind: &str) -> Value {
        json!({"resources":[{"name":name,"kind":kind}],"cursor":null,"complete":true})
    }
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = App::new("catalog-refresh".into());
    let profile = ui_profile("mysql", "catalog");
    app.profiles = vec![profile.clone()];
    app.profile = Some(profile.clone());
    app.tabs[0].profile_id = Some(profile.id);
    app.session = Some(json!({"session_id":"catalog-session"}));
    app.navigation_load_tree(&backend, false);
    let root = requests.try_recv().unwrap();
    app.navigation_response(
        &Response {
            tag: root.tag,
            result: Ok(page("commerce", "database")),
        },
        &backend,
    );
    app.navigation_action("navigation-db:commerce", &backend);
    let old = requests.try_recv().unwrap();
    app.navigation_load_tree(&backend, true);
    while let Ok(request) = requests.try_recv() {
        let result = if request.params["database"].as_str() == Some("commerce") {
            page("fresh_table", "table")
        } else {
            page("commerce", "database")
        };
        app.navigation_response(
            &Response {
                tag: request.tag,
                result: Ok(result),
            },
            &backend,
        );
    }
    app.navigation_response(
        &Response {
            tag: old.tag,
            result: Ok(page("stale_table", "table")),
        },
        &backend,
    );
    let node = &app.navigation.databases[&profile.id].nodes["commerce"];
    assert!(node.expanded);
    assert_eq!(
        node.resources
            .iter()
            .map(|r| r.name.as_str())
            .collect::<Vec<_>>(),
        ["fresh_table"]
    );
}

#[test]
fn reopening_disconnected_resource_tab_reconnects_before_loading() {
    for from_tree in [false, true] {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = App::new("reopen-resource".into());
        let profile = ui_profile("mysql", "restored");
        let target = ResourceTarget {
            database: Some("commerce".into()),
            name: "customers".into(),
            key_b64: None,
            kind: "table".into(),
        };
        app.profiles = vec![profile.clone()];
        app.profile = Some(profile.clone());
        app.tabs[0].profile_id = Some(profile.id);
        app.tabs[0].query = Editor::new("SELECT '草稿😀'");
        app.target = Some(target);
        app.database = Some("commerce".into());
        second_tab(&mut app);
        app.switch_tab(1);
        let saved: Workspace =
            serde_json::from_value(serde_json::to_value(app.workspace_snapshot()).unwrap())
                .unwrap();
        app = App::new("reopen-resource".into());
        app.tabs = saved.tabs;
        app.active = saved.active;
        app.profiles = vec![profile.clone()];
        app.restore_browse();
        app.profile = Some(profile.clone());
        app.tabs[1].profile_id = Some(profile.id);
        app.session = Some(json!({"session_id": Uuid::new_v4()}));
        app.resources = vec![
            serde_json::from_value(json!({
                "name":"customers", "kind":"table", "database":"commerce"
            }))
            .unwrap(),
        ];
        if from_tree {
            app.navigation
                .databases
                .entry(profile.id)
                .or_default()
                .nodes
                .entry("commerce".into())
                .or_default()
                .resources = app.resources.clone();
            app.navigation_action("navigation-resource:commerce:0", &backend);
        } else {
            app.select_resource(0, &backend);
        }
        assert_eq!(app.active, 0);
        assert_eq!(app.tabs.len(), 2);
        assert_eq!(app.tabs[0].query.text, "SELECT '草稿😀'");
        let reconnect = requests
            .try_recv()
            .expect("restored resource must reconnect");
        assert_eq!(reconnect.method, "connection.open");
        app.routed_response(
            Response {
                tag: reconnect.tag,
                result: Ok(json!({"session_id":Uuid::new_v4(),"profile":profile})),
            },
            &backend,
        );
        let mut described = false;
        while let Ok(request) = requests.try_recv() {
            if request.method == "resource.describe" {
                assert_eq!(request.params["target"]["database"], "commerce");
                assert_eq!(request.params["target"]["name"], "customers");
                described = true;
            }
        }
        assert!(described, "reconnection must load the original resource");
        assert_eq!(app.tabs[0].query.text, "SELECT '草稿😀'");
    }
}
