use super::*;

fn mysql_rows() -> App {
    let mut page = page(
        &["a", "b"],
        vec![
            vec![DataValue::Text("1".into()), DataValue::Text("first".into())],
            vec![
                DataValue::Text("2".into()),
                DataValue::Text("second".into()),
            ],
            vec![DataValue::Text("3".into()), DataValue::Text("third".into())],
        ],
    );
    page.result.columns[0].primary_key = true;
    page.result.columns[0].auto_increment = true;
    let result_id = page.result.id.to_string();
    let columns = page.result.columns.clone();
    let mut app = app_for("mysql", "table", page);
    app.database = Some("db".into());
    app.result_id = Some(result_id);
    app.schema = Some(json!({"keys":[["a"]],"engine":"InnoDB","columns":columns}));
    let grid = app.grid.as_mut().unwrap();
    grid.select_row(0, 0, false, false).unwrap();
    grid.select_row(2, 0, false, true).unwrap();
    app
}

#[test]
fn mysql_menu_copy_and_insert_preserve_selected_rows() {
    use crate::menus::GridMenuAction;
    let (backend, _, _) = Backend::scripted();
    let mut app = mysql_rows();
    let cell = Cell { row: 0, column: 1 };
    assert_eq!(app.grid_menu_row_count(cell), 2);
    assert_eq!(
        app.grid_menu(cell, GridMenuAction::CopyRows, &backend)
            .unwrap(),
        "1\tfirst\n3\tthird\n"
    );
    for (action, expected) in [
        (GridMenuAction::CopyInsert, vec!["1", "first", "3", "third"]),
        (
            GridMenuAction::CopyInsertWithoutAutoPrimary,
            vec!["first", "third"],
        ),
    ] {
        let text = app.grid_menu(cell, action, &backend).unwrap();
        let dialect = sqlparser::dialect::MySqlDialect {};
        let statements = sqlparser::parser::Parser::parse_sql(&dialect, &text).unwrap();
        assert!(
            statements
                .iter()
                .all(|statement| matches!(statement, sqlparser::ast::Statement::Insert(_)))
        );
        let values: Vec<String> = sqlparser::tokenizer::Tokenizer::new(&dialect, &text)
            .tokenize()
            .unwrap()
            .into_iter()
            .filter_map(|token| match token {
                sqlparser::tokenizer::Token::HexStringLiteral(hex) => {
                    let bytes: Vec<_> = hex
                        .as_bytes()
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| {
                            u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap()
                        })
                        .collect();
                    Some(String::from_utf8(bytes).unwrap())
                }
                _ => None,
            })
            .collect();
        assert_eq!(values, expected);
        assert_eq!(app.grid.as_ref().unwrap().selected_rows(), vec![0, 2]);
    }
    app.grid.as_mut().unwrap().settings[1].hidden = true;
    assert_eq!(
        app.grid_menu(
            Cell { row: 0, column: 0 },
            GridMenuAction::CopyRows,
            &backend
        )
        .unwrap(),
        "1\tfirst\n3\tthird\n"
    );
    app.grid.as_mut().unwrap().settings[1].hidden = false;
    let other = Cell { row: 1, column: 0 };
    assert_eq!(app.grid_menu_row_count(other), 1);
    assert_eq!(
        app.grid_menu(other, GridMenuAction::CopyRows, &backend)
            .as_deref(),
        Some("2\tsecond\n")
    );
    assert_eq!(app.grid.as_ref().unwrap().selected_rows(), vec![1]);
    let cell = Cell { row: 1, column: 1 };
    app.grid.as_mut().unwrap().select(cell, false).unwrap();
    assert_eq!(
        app.grid_menu(cell, GridMenuAction::CopyCells, &backend)
            .as_deref(),
        Some("second\n")
    );
}

#[test]
fn row_menus_work_when_every_column_is_hidden() {
    use crate::menus::GridMenuAction;
    let (backend, _, _) = Backend::scripted();
    let mut app = mysql_rows();
    let cell = Cell { row: 1, column: 0 };
    let expected: Vec<_> = [
        GridMenuAction::CopyRows,
        GridMenuAction::CopyInsert,
        GridMenuAction::CopyInsertWithoutAutoPrimary,
    ]
    .into_iter()
    .map(|action| app.grid_menu(cell, action, &backend).unwrap())
    .collect();
    assert_eq!(expected[0], "2\tsecond\n");
    let grid = app.grid.as_mut().unwrap();
    grid.select_row(0, 0, false, false).unwrap();
    for setting in &mut grid.settings {
        setting.hidden = true;
    }
    for (action, expected) in [
        GridMenuAction::CopyRows,
        GridMenuAction::CopyInsert,
        GridMenuAction::CopyInsertWithoutAutoPrimary,
    ]
    .into_iter()
    .zip(expected)
    {
        assert_eq!(
            app.grid_menu(cell, action, &backend).as_deref(),
            Some(expected.as_str())
        );
        assert_eq!(app.grid.as_ref().unwrap().selected_rows(), vec![1]);
    }
    app.grid_menu(cell, GridMenuAction::DeleteRows, &backend);
    assert_eq!(app.delete_rows_count(), 1);
    app.workbench_action("delete-rows-confirm", &backend);
    assert_eq!(app.pending_rows.len(), 1);
    assert_eq!(app.pending_rows[0].key["a"], DataValue::Text("2".into()));
    assert_eq!(
        app.pending_rows[0].before["b"],
        DataValue::Text("second".into())
    );
}

#[test]
fn deleted_identity_blocks_inline_drawer_and_entire_paste() {
    use crate::menus::GridMenuAction;
    let (backend, _, _) = Backend::scripted();
    let mut app = mysql_rows();
    let deleted = Cell { row: 2, column: 1 };
    app.grid_menu(deleted, GridMenuAction::DeleteRows, &backend);
    app.workbench_action("delete-rows-confirm", &backend);
    assert_eq!(app.pending_rows.len(), 2);

    app.grid.as_mut().unwrap().select(deleted, false).unwrap();
    app.begin_inline(&backend);
    assert!(app.workbench.inline.is_none());
    app.open_cell(&backend);
    app.workbench.cell_editor = Editor::new("changed");
    app.action("set-cell", &backend);
    assert!(app.grid.as_ref().unwrap().edits().is_empty());

    app.workbench.overlay = None;
    app.grid
        .as_mut()
        .unwrap()
        .select(Cell { row: 1, column: 1 }, false)
        .unwrap();
    let preview = app
        .grid
        .as_ref()
        .unwrap()
        .preview_paste("safe\nblocked")
        .unwrap();
    assert!(
        app.check_rows_not_deleted(preview.edits.iter().map(|edit| edit.cell.row))
            .is_err()
    );
    app.paste_preview = Some(preview);
    app.action("paste-confirm", &backend);
    assert!(app.grid.as_ref().unwrap().edits().is_empty());

    // 不同结果行号指向同一个可靠键时也必须拒绝。
    app.grid.as_mut().unwrap().page.rows[1][0] = DataValue::Text("3".into());
    assert!(app.check_rows_not_deleted([1]).is_err());
    assert_eq!(app.pending_rows.len(), 2);
}

#[test]
fn delete_confirmation_cancel_and_confirm_are_atomic() {
    use crate::menus::GridMenuAction;
    let (backend, _, _) = Backend::scripted();
    let mut app = mysql_rows();
    let cell = Cell { row: 0, column: 0 };
    app.grid_menu(cell, GridMenuAction::DeleteRows, &backend);
    assert_eq!(app.delete_rows_count(), 2);
    assert!(app.pending_rows.is_empty());
    app.workbench_action("overlay-close", &backend);
    assert_eq!(app.delete_rows_count(), 0);
    assert!(app.pending_rows.is_empty());
    app.grid_menu(cell, GridMenuAction::DeleteRows, &backend);
    assert!(app.workbench_key(&key("escape", false), &backend));
    assert!(app.pending_rows.is_empty());
    assert_eq!(app.delete_rows_count(), 0);
    app.grid_menu(cell, GridMenuAction::DeleteRows, &backend);
    app.workbench_action("delete-rows-confirm", &backend);
    assert_eq!(app.pending_rows.len(), 2);
    assert_eq!(app.pending_rows[0].key["a"], DataValue::Text("1".into()));
    assert_eq!(app.pending_rows[1].key["a"], DataValue::Text("3".into()));
    assert_eq!(
        app.pending_rows[0].before["b"],
        DataValue::Text("first".into())
    );
    assert!(app.pending_rows.iter().all(|row| row.operation == "delete"));
    app.grid_menu(cell, GridMenuAction::DeleteRows, &backend);
    app.workbench_action("delete-rows-confirm", &backend);
    assert_eq!(app.pending_rows.len(), 2);
}

#[test]
fn delete_rejects_permissions_keys_stale_context_and_draft_conflicts() {
    use crate::menus::GridMenuAction;
    let (backend, _, _) = Backend::scripted();
    let cell = Cell { row: 0, column: 0 };
    let mut readonly = mysql_rows();
    readonly.profile.as_mut().unwrap().access_mode = AccessMode::ReadOnly;
    readonly.grid_menu(cell, GridMenuAction::DeleteRows, &backend);
    assert_eq!(readonly.delete_rows_count(), 0);
    let mut no_key = mysql_rows();
    no_key.schema.as_mut().unwrap()["keys"] = json!([]);
    no_key.grid_menu(cell, GridMenuAction::DeleteRows, &backend);
    assert_eq!(no_key.delete_rows_count(), 0);
    let mut stale = mysql_rows();
    stale.grid_menu(cell, GridMenuAction::DeleteRows, &backend);
    stale.edit_token = Some(json!(Uuid::new_v4()));
    stale.workbench_action("delete-rows-confirm", &backend);
    assert!(stale.pending_rows.is_empty());
    let mut changed_row = mysql_rows();
    changed_row.grid_menu(cell, GridMenuAction::DeleteRows, &backend);
    changed_row.grid.as_mut().unwrap().page.rows[2][1] = DataValue::Text("different".into());
    changed_row.workbench_action("delete-rows-confirm", &backend);
    assert!(changed_row.pending_rows.is_empty());
    let mut draft = mysql_rows();
    draft
        .grid
        .as_mut()
        .unwrap()
        .edit(Cell { row: 2, column: 1 }, DataValue::Text("draft".into()))
        .unwrap();
    draft.grid_menu(cell, GridMenuAction::DeleteRows, &backend);
    draft.workbench_action("delete-rows-confirm", &backend);
    assert!(draft.pending_rows.is_empty());
    assert_eq!(draft.grid.as_ref().unwrap().edits().len(), 1);
    let mut invalid_key = mysql_rows();
    invalid_key.grid.as_mut().unwrap().page.rows[2][0] = DataValue::Null;
    invalid_key.grid_menu(cell, GridMenuAction::DeleteRows, &backend);
    invalid_key.workbench_action("delete-rows-confirm", &backend);
    assert!(invalid_key.pending_rows.is_empty());
}
