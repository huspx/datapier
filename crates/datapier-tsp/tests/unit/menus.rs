use super::*;
use crate::grid::GridState;
use datapier_core::model::{Column, ResultMeta, ResultPage, Value};
use uuid::Uuid;

fn app() -> App {
    let mut app = App::new("test".into());
    app.grid = Some(GridState::new(
        ResultPage {
            result: ResultMeta {
                id: Uuid::new_v4(),
                execution_id: Uuid::new_v4(),
                columns: vec![Column::new("name", "text")],
                kind: "rows".into(),
                row_count: 2,
                complete: true,
                summary: serde_json::Value::Null,
            },
            rows: vec![
                vec![Value::Text("first".into())],
                vec![Value::Text("second".into())],
            ],
            start: 10,
            next: None,
        },
        false,
    ));
    app
}

#[test]
fn copy_targets_clicked_cell_and_preserves_existing_selection() {
    let (backend, _, _) = Backend::scripted();
    let mut app = app();
    let first = Cell { row: 10, column: 0 };
    let second = Cell { row: 11, column: 0 };
    assert_eq!(
        app.grid_menu(second, GridMenuAction::CopyCells, &backend)
            .as_deref(),
        Some("second\n")
    );
    let grid = app.grid.as_mut().unwrap();
    grid.select(first, false).unwrap();
    grid.select(second, true).unwrap();
    assert_eq!(
        app.grid_menu(first, GridMenuAction::CopyCells, &backend)
            .as_deref(),
        Some("first\nsecond\n")
    );
    let grid = app.grid.as_ref().unwrap();
    assert_eq!(grid.anchor, Some(first));
    assert_eq!(grid.focus, Some(second));
}

#[test]
fn row_menu_preserves_selected_rows_and_retargets_unselected_row() {
    let (backend, _, _) = Backend::scripted();
    let mut app = app();
    let first = Cell { row: 10, column: 0 };
    let second = Cell { row: 11, column: 0 };
    let grid = app.grid.as_mut().unwrap();
    grid.select_row(10, 0, false, false).unwrap();
    grid.select_row(11, 0, false, true).unwrap();
    assert!(
        app.grid_menu(first, GridMenuAction::CopyRows, &backend)
            .is_some()
    );
    assert_eq!(app.grid.as_ref().unwrap().selected_rows(), vec![10, 11]);
    app.grid
        .as_mut()
        .unwrap()
        .select_row(10, 0, false, false)
        .unwrap();
    assert!(
        app.grid_menu(second, GridMenuAction::CopyRows, &backend)
            .is_some()
    );
    assert_eq!(app.grid.as_ref().unwrap().selected_rows(), vec![11]);
}

#[test]
fn redis_menu_preserves_selected_rows_and_retargets_other_rows() {
    let mut app = app();
    let first = Cell { row: 10, column: 0 };
    let second = Cell { row: 11, column: 0 };
    let grid = app.grid.as_mut().unwrap();
    grid.select_row(10, 0, false, false).unwrap();
    grid.select_row(11, 0, false, true).unwrap();
    assert!(app.prepare_redis_grid_menu(first));
    assert_eq!(app.grid.as_ref().unwrap().selected_rows(), vec![10, 11]);
    app.grid
        .as_mut()
        .unwrap()
        .select_row(10, 0, false, false)
        .unwrap();
    assert!(app.prepare_redis_grid_menu(second));
    assert_eq!(app.grid.as_ref().unwrap().selected_rows(), vec![11]);
    assert!(!app.prepare_redis_grid_menu(Cell { row: 12, column: 0 }));
    assert_eq!(app.grid.as_ref().unwrap().selected_rows(), vec![11]);
}

#[test]
fn failed_inline_commit_keeps_the_original_target() {
    let (backend, _, _) = Backend::scripted();
    let mut app = app();
    let first = Cell { row: 10, column: 0 };
    let second = Cell { row: 11, column: 0 };
    app.workbench.inline = Some(first);
    app.field_set("cell", "draft");
    assert!(
        app.grid_menu(second, GridMenuAction::CopyCells, &backend)
            .is_none()
    );
    assert_eq!(app.grid.as_ref().unwrap().focus, Some(first));
    assert_eq!(app.workbench.inline, Some(first));
    assert_eq!(app.field("cell"), "draft");
}

#[test]
fn invalid_targets_do_not_change_selection() {
    let (backend, _, _) = Backend::scripted();
    let mut app = app();
    let first = Cell { row: 10, column: 0 };
    let second = Cell { row: 11, column: 0 };
    assert!(
        app.grid_menu(
            Cell { row: 0, column: 0 },
            GridMenuAction::CopyCells,
            &backend
        )
        .is_none()
    );
    assert_eq!(app.grid.as_ref().unwrap().focus, Some(first));
    app.grid.as_mut().unwrap().settings[0].hidden = true;
    assert!(
        app.grid_menu(second, GridMenuAction::CopyCells, &backend)
            .is_none()
    );
    assert_eq!(app.grid.as_ref().unwrap().focus, Some(first));
}
