use super::*;
use datapier_core::model::{Column, ResultMeta, RowChange, Value as CellValue};
#[path = "regression/connection_flow.rs"]
mod connection_flow;
#[path = "regression/input_routing.rs"]
mod input_routing;
#[path = "regression/query_flow.rs"]
mod query_flow;
#[path = "regression/redis_flow.rs"]
mod redis_flow;
#[path = "regression/resource_navigation.rs"]
mod resource_navigation;
#[path = "regression/result_state.rs"]
mod result_state;
#[path = "regression/workspace_restore.rs"]
mod workspace_restore;

fn second_tab(app: &mut App) {
    app.tabs.push(Tab {
        title: "second".into(),
        query: Editor::new(""),
        cli_query: Editor::default(),
        profile_id: None,
        id: Uuid::new_v4(),
        layout: Vec::new(),
        browse: BrowseSettings::default(),
        pending: Value::Null,
        pending_live: false,
        schema_draft: None,
        replace_draft: None,
        runtime: TabRuntime::default(),
    });
}

fn editable_grid() -> GridState {
    GridState::new(
        ResultPage {
            result: ResultMeta {
                id: Uuid::new_v4(),
                execution_id: Uuid::new_v4(),
                columns: vec![Column::new("name", "text")],
                kind: "rows".into(),
                row_count: 1,
                complete: true,
                summary: Value::Null,
            },
            rows: vec![vec![CellValue::Text("before".into())]],
            start: 0,
            next: None,
        },
        true,
    )
}

fn app_with_grid_draft() -> App {
    let mut app = App::new("test".into());
    let mut grid = editable_grid();
    grid.edit(Cell { row: 0, column: 0 }, CellValue::Text("draft".into()))
        .unwrap();
    app.grid = Some(grid);
    app.session = Some(json!({"session_id": Uuid::new_v4()}));
    app.target = Some(ResourceTarget {
        database: None,
        name: "table".into(),
        key_b64: None,
        kind: "table".into(),
    });
    app.tabs[0].query = Editor::new("select 1");
    app
}

fn ui_profile(provider: &str, name: &str) -> Profile {
    serde_json::from_value(json!({
        "id":Uuid::new_v4(),"name":name,"provider":provider,"access_mode":"read_write",
        "connection":{"kind":"direct","host":"127.0.0.1",
            "port":if provider == "mysql" {3306} else {6379}}
    }))
    .unwrap()
}

fn inline_app() -> App {
    let mut app = app_with_grid_draft();
    app.grid.as_mut().unwrap().discard();
    app.profile = Some(ui_profile("mysql", "inline"));
    app.edit_token = Some(json!(Uuid::new_v4()));
    app.browse_result = true;
    app.schema = Some(json!({"engine":"InnoDB","keys":[["name"]]}));
    app
}
