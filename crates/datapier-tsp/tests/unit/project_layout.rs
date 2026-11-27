use super::*;
use datapier_core::model::{Column, ResultMeta};

fn project(app: &mut App, root: &str, id: Uuid, a: Uuid, b: Uuid) {
    app.workbench.project.project = Some(json!({
        "project_id": id, "root": root,
        "manifest": {"id":id,"queries":[
            {"id":a,"name":"A","path":"queries/a.sql","language":"sql","connection_alias":"db"},
            {"id":b,"name":"B","path":"queries/b.sql","language":"sql","connection_alias":"db"}
        ],"layout":{"tabs":[b,a],"splits":[[a,b]],"columns":{(a.to_string()):["name"]},"resources":[]}}
    }));
}

fn apply(app: &mut App) {
    let mut load = app.prepare_project_layout().unwrap();
    for q in &load.queries {
        load.texts.insert(q.id, format!("select '{}'", q.name));
    }
    app.finish_project_layout(load);
}

#[test]
fn split_editing_preserves_drafts_and_isolates_project_roots() {
    let (id, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut app = App::new("layout".into());
    project(&mut app, "/first", id, a, b);
    let owner = app.tabs[0].id;
    apply(&mut app);
    assert_eq!(app.tabs[0].title, "B");
    assert_eq!(app.tabs[1].title, "A");
    let a_tab = app.tabs[1].id;
    let b_tab = app.tabs[0].id;
    assert_eq!(active_group(&app).unwrap(), &[a_tab, b_tab]);
    assert_eq!(
        app.resolve_project_editor(format!("project-query:{a_tab}")),
        Some("query".into())
    );
    app.tabs[app.active].query = Editor::new("unsaved");
    let owner_index = app.tabs.iter().position(|tab| tab.id == owner).unwrap();
    app.switch_tab(owner_index);
    apply(&mut app);
    assert_eq!(
        app.tabs
            .iter()
            .find(|tab| tab.id == a_tab)
            .unwrap()
            .query
            .text,
        "unsaved"
    );
    let owner_index = app.tabs.iter().position(|tab| tab.id == owner).unwrap();
    app.switch_tab(owner_index);
    project(&mut app, "/second", id, a, b);
    apply(&mut app);
    assert_ne!(app.tabs[1].id, a_tab);
    assert!(
        app.resolve_project_editor(format!("project-query:{a_tab}"))
            .is_none()
    );
    assert_eq!(
        app.tabs
            .iter()
            .find(|tab| tab.id == a_tab)
            .unwrap()
            .query
            .text,
        "unsaved"
    );
}

#[test]
fn split_sql_rejects_old_revision_and_other_tab_edit_events() {
    let (id, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut app = App::new("layout-native".into());
    project(&mut app, "/first", id, a, b);
    apply(&mut app);
    app.select_tab(0);
    app.focus = "query".into();
    let active = app.active;
    let stale = app.sql_editor_name(active);
    let other = app.sql_editor_name(if active == 0 { 1 } else { 0 });
    assert!(app.resolve_sql_editor(other.clone(), false).is_none());
    assert!(app.resolve_sql_editor(other, true).is_some());
    app.select_tab(active);
    app.tabs[active].query.edit(0..1, "S");
    assert!(app.resolve_sql_editor(stale, false).is_none());
    assert_eq!(
        app.resolve_sql_editor(app.sql_editor_name(active), false),
        Some("query".into())
    );
}

#[test]
fn workspace_restore_keeps_split_order_and_unsaved_query_text() {
    let (id, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut app = App::new("layout-restore".into());
    project(&mut app, "/first", id, a, b);
    apply(&mut app);
    let groups = app.project_layout.groups.clone();
    let active = app.tabs[app.active].id;
    app.tabs[app.active].query = Editor::new("select '未保存😀'");
    let snapshot = serde_json::to_value(app.workspace_snapshot()).unwrap();
    let mut restored = App::new("layout-restore".into());
    restored.restore_workspace_state(1, snapshot).unwrap();
    assert_eq!(active_group(&restored), Some(groups[0].as_slice()));
    assert_eq!(restored.tabs[restored.active].id, active);
    assert_eq!(
        restored.tabs[restored.active].query.text,
        "select '未保存😀'"
    );
    assert!(restored.project_layout.execution.is_none());
}

#[test]
fn project_columns_use_names_and_reject_ambiguous_results() {
    let mut app = App::new("layout".into());
    app.tabs[0].project_columns = Some(vec!["name".into()]);
    let page = ResultPage {
        result: ResultMeta {
            id: Uuid::new_v4(),
            execution_id: Uuid::new_v4(),
            columns: vec![Column::new("other", "text"), Column::new("name", "text")],
            kind: "rows".into(),
            row_count: 0,
            complete: true,
            summary: Value::Null,
        },
        rows: vec![],
        start: 0,
        next: None,
    };
    let mut grid = GridState::new(page, false);
    app.apply_project_columns(&mut grid);
    assert!(grid.settings[0].hidden);
    assert!(!grid.settings[1].hidden);
    grid.page.result.columns[0].name = "name".into();
    grid.settings[0].hidden = false;
    app.apply_project_columns(&mut grid);
    assert!(!grid.settings[0].hidden);
}

#[test]
fn execution_resolves_exact_root_and_aborts_when_draft_changes() {
    let (backend, mut requests, _) = Backend::scripted();
    let (id, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut app = App::new("layout".into());
    project(&mut app, "/first", id, a, b);
    apply(&mut app);
    assert!(app.project_layout_execute_text("select 'B'", &backend));
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "project.query.resolve");
    assert_eq!(request.params["expected_root"], "/first");
    assert_eq!(request.params["query_id"], b.to_string());
    app.tabs[app.active].query = Editor::new("changed while resolving");
    let identity = app.tabs[app.active].project_query.clone().unwrap();
    app.project_layout_response(
        &Response {
            tag: request.tag,
            result: Ok(json!({
                "root":"/first", "query":identity.query_snapshot, "profile_id":Uuid::new_v4(),
                "binding_group":"default", "database":null, "redis_db":null
            })),
        },
        &backend,
    );
    assert!(requests.try_recv().is_err());
    assert_eq!(app.tabs[app.active].query.text, "changed while resolving");
    assert!(app.project_layout.execution.is_none());
}

#[test]
fn execution_rejects_resolution_from_another_project_root() {
    let (backend, mut requests, _) = Backend::scripted();
    let (id, a, b) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
    let mut app = App::new("layout".into());
    project(&mut app, "/first", id, a, b);
    apply(&mut app);
    assert!(app.project_layout_execute_text("select 'B'", &backend));
    let request = requests.try_recv().unwrap();
    let identity = app.tabs[app.active].project_query.clone().unwrap();
    app.project_layout_response(
        &Response {
            tag: request.tag,
            result: Ok(json!({
                "root":"/second", "query":identity.query_snapshot, "profile_id":Uuid::new_v4(),
                "binding_group":"default", "database":null, "redis_db":null
            })),
        },
        &backend,
    );
    assert!(requests.try_recv().is_err());
    assert!(app.project_layout.execution.is_none());
}
