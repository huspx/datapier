use super::*;

#[test]
fn redis_editor_preserves_binary_member_identity() {
    let bytes = vec![0, 0xff, b'\\', b'\n', b'a'];
    let value = DataValue::Bytes(STANDARD.encode(&bytes));
    let editor_text = redis_field(&value).unwrap();
    assert_eq!(redis_bytes(&editor_text).unwrap(), bytes);
    assert_eq!(redis_bytes("hello").unwrap(), b"hello");
    assert_eq!(
        redis_field(&DataValue::Bytes(STANDARD.encode("中文"))).unwrap(),
        "中文"
    );
    let prefixed = DataValue::Bytes(STANDARD.encode("base64:literal"));
    assert_eq!(
        redis_bytes(&redis_field(&prefixed).unwrap()).unwrap(),
        b"base64:literal"
    );
    assert!(redis_bytes("base64:!").is_err());
}

#[test]
fn redis_key_copy_preserves_original_bytes() {
    for bytes in [b"hello".as_slice(), b"base64:literal", b"\xff\0key"] {
        let encoded = STANDARD.encode(bytes);
        let text = redis_key_text("lossy display", Some(&encoded));
        assert_eq!(redis_bytes(&text).unwrap(), bytes);
    }
    assert_eq!(redis_key_text("name", None), "name");
}

#[test]
fn cli_toggle_saves_visibility_for_both_directions() {
    for initial in [false, true] {
        let (backend, mut requests, _) = Backend::scripted();
        let mut app = app_for("redis", "string", page(&["value"], vec![]));
        app.workspace_loaded = true;
        app.workbench.cli = initial;
        app.dirty = false;

        app.action("cli-toggle", &backend);
        assert!(app.dirty);
        app.save_workspace(&backend);
        let request = requests.try_recv().unwrap();
        assert_eq!(request.method, "workspace.save");
        assert_eq!(
            request.params["state"]["tabs"][0]["browse"]["cli_visible"],
            !initial
        );
        assert!(app.saving);
        assert!(!app.dirty);
    }
}

#[test]
fn key_type_picker_consumes_enter_before_cli_and_escape_only_closes_picker() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.workbench.cli = true;
    app.focus = "cli_text".into();
    app.field_set("cli_text", "PING");
    app.workbench.key_type_menu = true;
    assert!(app.workbench_key(&key("enter", false), &backend));
    assert!(requests.try_recv().is_err());
    assert!(app.workbench_key(&key("escape", false), &backend));
    assert!(!app.workbench.key_type_menu);
    assert!(app.workbench_key(&key("enter", false), &backend));
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "execution.start");
    assert_eq!(request.params["text"], "PING");
}

#[test]
fn hidden_cli_and_overlay_do_not_handle_background_enter() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.field_set("cli_text", "PING");
    app.workbench_action("cli-toggle", &backend);
    app.workbench_action("cli-toggle", &backend);
    assert_ne!(app.focus, "cli_text");
    app.focus = "cli_text".into();
    assert!(!app.workbench_key(&key("enter", false), &backend));
    assert!(requests.try_recv().is_err());
    app.workbench.cli = true;
    app.workbench.overlay = Some("profile".into());
    assert!(!app.workbench_key(&key("enter", false), &backend));
    app.focus = "grid".into();
    assert!(!app.workbench_key(&key("enter", false), &backend));
    assert_eq!(app.workbench.overlay.as_deref(), Some("profile"));
    assert!(requests.try_recv().is_err());
}

#[test]
fn cli_paging_commits_history_only_after_success() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    let mut first = page(&["reply"], vec![]);
    first.next = Some(200);
    app.workbench.cli_result_id = Some(first.result.id.to_string());
    app.workbench.cli_grid = Some(GridState::new(first.clone(), false));
    app.workbench_action("cli-next", &backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_action("cli-next", &backend);
    app.workbench_action("cli-previous", &backend);
    assert!(requests.try_recv().is_err());
    assert!(app.workbench.cli_page_history.is_empty());
    app.workbench_error(tag, &AppError::new("test", "failed"));
    assert!(app.workbench.cli_page_history.is_empty());
    app.workbench_action("cli-next", &backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    let mut second = first.clone();
    second.start = 200;
    second.next = None;
    app.workbench_response(tag, serde_json::to_value(second).unwrap(), &backend);
    assert_eq!(app.workbench.cli_page_history, vec![0]);
    app.workbench_action("cli-previous", &backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_action("cli-previous", &backend);
    assert!(requests.try_recv().is_err());
    app.workbench_response(tag, json!({"invalid": true}), &backend);
    assert_eq!(app.workbench.cli_page_history, vec![0]);
    assert_eq!(app.workbench.cli_grid.as_ref().unwrap().page.start, 200);
    app.workbench_action("cli-previous", &backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_response(tag, serde_json::to_value(first).unwrap(), &backend);
    assert!(app.workbench.cli_page_history.is_empty());
    assert_eq!(app.workbench.cli_grid.as_ref().unwrap().page.start, 0);
}

#[test]
fn redis_tree_toggle_preserves_current_work() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.workbench.cli = true;
    app.tabs[app.active].cli_query = Editor::new("GET item");
    app.tabs[app.active].pending = json!({"draft": "kept"});
    let target = serde_json::to_value(&app.target).unwrap();
    let profile = app.profile.as_ref().unwrap().clone();
    assert!(app.workbench_action("redis-tree:YTo=", &backend));
    assert!(
        app.workbench
            .redis_expansion
            .contains(profile.id, profile.redis_db, b"a:")
    );
    assert_eq!(serde_json::to_value(&app.target).unwrap(), target);
    assert_eq!(app.tabs[app.active].pending, json!({"draft": "kept"}));
    assert_eq!(app.tabs[app.active].cli_query.text, "GET item");
    assert!(app.workbench.cli);
    assert!(requests.try_recv().is_err());
}

#[test]
fn cli_paging_queue_failure_keeps_history_retryable() {
    let (backend, requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    let mut current = page(&["reply"], vec![]);
    current.start = 200;
    app.workbench.cli_result_id = Some(current.result.id.to_string());
    app.workbench.cli_grid = Some(GridState::new(current, false));
    app.workbench.cli_page_history.push(0);
    drop(requests);
    app.workbench_action("cli-previous", &backend);
    assert_eq!(app.workbench.cli_page_history, vec![0]);
    assert!(app.workbench.cli_page_token.is_none());
    assert!(app.workbench.cli_page_transition.is_none());
}

use datapier_core::model::{Column, ResultMeta};

fn page(columns: &[&str], rows: Vec<Vec<DataValue>>) -> ResultPage {
    ResultPage {
        result: ResultMeta {
            id: Uuid::new_v4(),
            execution_id: Uuid::new_v4(),
            columns: columns
                .iter()
                .map(|name| Column::new(*name, "text"))
                .collect(),
            kind: "rows".into(),
            row_count: rows.len() as u64,
            complete: true,
            summary: Value::Null,
        },
        rows,
        start: 0,
        next: None,
    }
}

fn app_for(provider: &str, kind: &str, page: ResultPage) -> App {
    let mut app = App::new("test".into());
    app.profile_form(None);
    app.field_set("provider", provider);
    app.field_set("access", "read_write");
    app.profile = Some(app.form_profile().unwrap());
    // 此 helper 模拟连接完成后的工作台，连接表单已关闭。
    app.workbench.overlay = None;
    app.session = Some(json!({"session_id":Uuid::new_v4()}));
    app.target = Some(ResourceTarget {
        database: None,
        name: "item".into(),
        key_b64: None,
        kind: kind.into(),
    });
    app.edit_token = Some(json!(Uuid::new_v4()));
    app.browse_result = true;
    app.schema = Some(if provider == "redis" {
        json!({"type":kind})
    } else {
        json!({"keys":[["a"]],"engine":"InnoDB"})
    });
    app.grid = Some(GridState::new(page, provider != "redis"));
    app.focus = "grid".into();
    app
}

#[test]
fn history_selection_clears_only_when_filter_text_changes() {
    let mut app = App::new("history-filter".into());
    app.workbench.overlay = Some("history".into());
    app.workbench.history = vec![json!({"text":"SELECT 1"})];
    app.workbench.history_selected = Some(0);
    app.dispatch_editor_message(Msg::Edit(
        "history_filter".into(),
        0,
        0,
        "select".into(),
        6,
        0,
    ));
    assert_eq!(app.workbench.history_selected, None);
    app.workbench.history_selected = Some(0);
    app.field_set("history_filter", "select");
    assert_eq!(app.workbench.history_selected, Some(0));
    app.field_set("history_filter", "other");
    assert_eq!(app.workbench.history_selected, None);
}

#[test]
fn field_error_is_discarded_when_overlay_changes() {
    let mut app = App::new("overlay-error".into());
    app.workbench.overlay = Some("file".into());
    app.field_error("path", UiMessage::key("请填写路径"));
    app.reconcile_modal_focus();
    assert!(app.workbench.field_error.is_some());
    app.workbench.overlay = Some("export".into());
    app.reconcile_modal_focus();
    assert!(app.workbench.field_error.is_none());
}

#[test]
fn invalid_paths_and_column_width_do_not_request_or_change_layout() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for(
        "mysql",
        "table",
        page(&["a"], vec![vec![DataValue::Text("v".into())]]),
    );
    app.field_set("path", "  ");
    app.action("file-open", &backend);
    assert!(requests.try_recv().is_err());
    assert!(app.file_open_pending.is_none());
    assert_eq!(app.workbench.field_error.as_ref().unwrap().field, "path");
    app.field_set("path", "data.csv");
    app.field_set("mapping", "[]");
    app.action("preview-import", &backend);
    assert!(requests.try_recv().is_err());
    assert!(app.preview_pending.is_none());
    assert_eq!(app.workbench.field_error.as_ref().unwrap().field, "mapping");
    app.field_set("column", "0");
    app.field_set("width", "");
    app.dirty = false;
    let width = app.grid.as_ref().unwrap().settings[0].width;
    app.action("column-width", &backend);
    assert_eq!(app.grid.as_ref().unwrap().settings[0].width, width);
    assert!(!app.dirty);
    assert_eq!(app.workbench.field_error.as_ref().unwrap().field, "width");
}

#[test]
fn fitting_waits_until_inline_edit_finishes() {
    let mut app = app_for(
        "mysql",
        "table",
        page(&["a"], vec![vec![DataValue::Text("value".into())]]),
    );
    app.viewport_width = 350;
    app.fit_grid();
    let before = app.grid.as_ref().unwrap().settings[0].width;
    app.workbench.inline = Some(Cell { row: 0, column: 0 });
    app.viewport_width = 1000;
    app.fit_grid();
    assert_eq!(app.grid.as_ref().unwrap().settings[0].width, before);
    app.workbench.inline = None;
    app.fit_grid();
    assert_ne!(app.grid.as_ref().unwrap().settings[0].width, before);
}

#[test]
fn redis_empty_member_is_data_but_invalid_score_is_rejected() {
    let (backend, _, _) = Backend::scripted();
    let mut app = app_for("redis", "zset", page(&["member", "score"], vec![]));
    app.field_set("operation", "zset_set");
    app.field_set("redis_member", "");
    app.field_set("redis_score", "1e999");
    app.dirty = false;
    app.workbench_action("redis-stage", &backend);
    assert!(app.redis_changes.is_empty());
    assert!(!app.dirty);
    assert_eq!(
        app.workbench.field_error.as_ref().unwrap().field,
        "redis_score"
    );
    app.field_set("redis_score", "1");
    app.workbench_action("redis-stage", &backend);
    assert_eq!(app.redis_changes.len(), 1);
    assert_eq!(app.redis_changes[0]["member"]["value"], "");
}

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

fn key(name: &str, shift: bool) -> tern_sdk::Key {
    let mut key = tern_sdk::Key::named(name);
    key.shift = shift;
    key
}
#[test]
fn regression_refresh_finishes_inline_without_replacing_its_row() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for(
        "mysql",
        "table",
        page(
            &["a", "b"],
            vec![
                vec![
                    DataValue::Text("first".into()),
                    DataValue::Text("unchanged".into()),
                ],
                vec![
                    DataValue::Text("second".into()),
                    DataValue::Text("before".into()),
                ],
            ],
        ),
    );
    let cell = Cell { row: 1, column: 1 };
    app.grid.as_mut().unwrap().select(cell, false).unwrap();
    assert!(app.workbench_key(&key("enter", false), &backend));
    app.field_set("cell", "draft");
    let result = app.grid.as_ref().unwrap().page.result.id;
    app.action("browse", &backend);
    assert!(app.workbench.inline.is_none());
    let grid = app.grid.as_ref().unwrap();
    assert_eq!(grid.page.result.id, result);
    assert_eq!(grid.value(cell), Some(&DataValue::Text("draft".into())));
    assert_eq!(
        grid.value(Cell { row: 0, column: 1 }),
        Some(&DataValue::Text("unchanged".into()))
    );
    assert!(app.has_live_drafts());
    assert!(requests.try_recv().is_err());
}

#[test]
fn regression_redis_history_return_keeps_key_drafts_and_cli_result() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for(
        "redis",
        "string",
        page(&["value"], vec![vec![DataValue::Text("key value".into())]]),
    );
    let key_result = app.grid.as_ref().unwrap().page.result.id;
    let cli = page(&["reply"], vec![vec![DataValue::Text("PONG".into())]]);
    let cli_result = cli.result.id;
    app.workbench.cli_grid = Some(GridState::new(cli, false));
    app.workbench.cli = true;
    app.field_set("cli_text", "PING");
    app.redis_changes.push(json!({"operation":"delete_key"}));
    app.workbench.overlay = Some("history".into());
    app.action("overlay-close", &backend);
    assert!(app.workbench.overlay.is_none());
    assert_eq!(app.grid.as_ref().unwrap().page.result.id, key_result);
    assert_eq!(
        app.workbench.cli_grid.as_ref().unwrap().page.result.id,
        cli_result
    );
    assert_eq!(app.field("cli_text"), "PING");
    assert_eq!(app.redis_changes, vec![json!({"operation":"delete_key"})]);
    assert!(app.workbench.cli);
    assert!(requests.try_recv().is_err());
}

#[test]
fn inline_commit_cancel_and_tab_do_not_send_writes() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for(
        "mysql",
        "table",
        page(
            &["a", "b"],
            vec![vec![
                DataValue::Text("旧".into()),
                DataValue::Text("二".into()),
            ]],
        ),
    );
    assert!(app.workbench_key(&key("enter", false), &backend));
    app.field_set("cell", "修改😀");
    assert!(app.workbench_key(&key("tab", false), &backend));
    assert_eq!(
        app.grid.as_ref().unwrap().value(Cell { row: 0, column: 0 }),
        Some(&DataValue::Text("修改😀".into()))
    );
    assert_eq!(app.workbench.inline, Some(Cell { row: 0, column: 1 }));
    app.field_set("cell", "放弃");
    assert!(app.workbench_key(&key("escape", false), &backend));
    assert_eq!(
        app.grid.as_ref().unwrap().value(Cell { row: 0, column: 1 }),
        Some(&DataValue::Text("二".into()))
    );
    assert!(app.workbench_key(&key("f2", false), &backend));
    app.field_set("cell", "确认");
    assert!(app.workbench_key(&key("enter", false), &backend));
    assert_eq!(
        app.grid.as_ref().unwrap().value(Cell { row: 0, column: 1 }),
        Some(&DataValue::Text("确认".into()))
    );
    assert!(app.workbench_key(&key("f2", false), &backend));
    assert!(app.workbench_key(&key("tab", true), &backend));
    assert_eq!(app.workbench.inline, Some(Cell { row: 0, column: 0 }));
    let mut numeric = app_for(
        "mysql",
        "table",
        page(&["a"], vec![vec![DataValue::Integer("1".into())]]),
    );
    numeric.workbench_key(&key("enter", false), &backend);
    numeric.field_set("cell", "invalid");
    numeric.workbench_key(&key("enter", false), &backend);
    assert!(numeric.workbench.inline.is_some());
    assert_eq!(numeric.field("cell"), "invalid");
    assert!(numeric.grid.as_ref().unwrap().edits().is_empty());
    assert!(requests.try_recv().is_err());
}

#[test]
fn redis_hash_rename_preserves_old_binary_field_and_multiline_value() {
    let (backend, mut requests, _) = Backend::scripted();
    let original = vec![0xff, 0, b'x'];
    let mut app = app_for(
        "redis",
        "hash",
        page(
            &["field", "value"],
            vec![vec![
                DataValue::Bytes(STANDARD.encode(&original)),
                DataValue::Bytes(STANDARD.encode("第一行\n第二行")),
            ]],
        ),
    );
    app.workbench_action("redis-edit", &backend);
    assert_eq!(app.field("redis_value"), "第一行\n第二行");
    app.field_set("redis_field", "新字段");
    app.workbench_action("redis-stage", &backend);
    assert_eq!(app.redis_changes.len(), 2);
    assert_eq!(
        app.redis_changes[0],
        json!({"operation":"hash_delete","field":{"type":"bytes","value":STANDARD.encode(original)}})
    );
    assert_eq!(
        app.redis_changes[1],
        json!({"operation":"hash_set","field":{"type":"text","value":"新字段"},"value":{"type":"text","value":"第一行\n第二行"}})
    );
    app.workbench_action("redis-undo", &backend);
    assert!(app.redis_changes.is_empty());
    assert!(requests.try_recv().is_err());
}

#[test]
fn redis_list_duplicate_values_keep_selected_index_and_zset_rejects_nan() {
    let (backend, mut requests, _) = Backend::scripted();
    let rows = vec![
        vec![
            DataValue::Integer("0".into()),
            DataValue::Bytes(STANDARD.encode("same")),
        ],
        vec![
            DataValue::Integer("1".into()),
            DataValue::Bytes(STANDARD.encode("same")),
        ],
    ];
    let mut app = app_for("redis", "list", page(&["index", "value"], rows));
    app.grid
        .as_mut()
        .unwrap()
        .select(Cell { row: 1, column: 1 }, false)
        .unwrap();
    app.workbench_action("redis-edit", &backend);
    app.field_set("redis_index", "0");
    app.workbench_action("redis-stage", &backend);
    assert!(app.redis_changes.is_empty());
    app.field_set("redis_index", "1");
    app.field_set("redis_value", "changed");
    app.workbench_action("redis-stage", &backend);
    assert_eq!(app.redis_changes[0]["index"], json!(1));
    let mut zset = app_for(
        "redis",
        "zset",
        page(
            &["member", "score"],
            vec![vec![
                DataValue::Bytes(STANDARD.encode("x")),
                DataValue::Float("1".into()),
            ]],
        ),
    );
    zset.workbench_action("redis-edit", &backend);
    zset.field_set("redis_score", "NaN");
    zset.workbench_action("redis-stage", &backend);
    assert!(zset.redis_changes.is_empty());
    assert!(requests.try_recv().is_err());
}

#[test]
fn cli_pages_do_not_replace_key_grid_or_baseline() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for(
        "redis",
        "string",
        page(
            &["value"],
            vec![vec![DataValue::Bytes(STANDARD.encode("original"))]],
        ),
    );
    let key_id = app.grid.as_ref().unwrap().page.result.id;
    let baseline = app.edit_token.clone();
    app.workbench.cli = true;
    app.field_set("cli_text", "PING");
    app.workbench_action("cli-run", &backend);
    let start = requests.try_recv().unwrap();
    assert_eq!(start.method, "execution.start");
    app.field_set("cli_text", "GET another-key");
    fn command_text(node: &Node<Msg>) -> Option<&str> {
        if node.props().get("class").and_then(Value::as_str) == Some("cli-command") {
            return node.props().get("text").and_then(Value::as_str);
        }
        node.child_nodes().iter().find_map(command_text)
    }
    let view = crate::views::render(&app);
    assert_eq!(
        view.regions().into_iter().flatten().find_map(command_text),
        Some("PING")
    );
    app.workbench.history.push(json!({"text":"INFO"}));
    app.workbench_action("history-open:0", &backend);
    assert_eq!(app.field("cli_text"), "INFO");
    let view = crate::views::render(&app);
    assert_eq!(
        view.regions().into_iter().flatten().find_map(command_text),
        Some("PING")
    );
    app.workbench_response("cli.start", json!({"task_id":"task"}), &backend);
    let result = page(&["reply"], vec![vec![DataValue::Text("PONG".into())]]);
    let second = page(&["reply"], vec![vec![DataValue::Text("SECOND".into())]]);
    app.workbench_response(
        "execution.get",
        json!({"state":"succeeded","results":[result.result.id, second.result.id]}),
        &backend,
    );
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "result.page");
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_response(tag, serde_json::to_value(result).unwrap(), &backend);
    app.workbench_action("cli-result:1", &backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_response(tag, serde_json::to_value(second).unwrap(), &backend);
    let view = crate::views::render(&app);
    assert_eq!(
        view.regions().into_iter().flatten().find_map(command_text),
        Some("PING")
    );
    assert_eq!(app.grid.as_ref().unwrap().page.result.id, key_id);
    assert_eq!(app.edit_token, baseline);
    assert_eq!(
        app.workbench.cli_grid.as_ref().unwrap().page.rows[0][0],
        DataValue::Text("SECOND".into())
    );
}

#[test]
fn history_from_table_opens_query_and_keeps_table_baseline() {
    let (backend, _requests, _) = Backend::scripted();
    let mut app = app_for(
        "mysql",
        "table",
        page(&["a"], vec![vec![DataValue::Text("old".into())]]),
    );
    app.results = vec!["old-result".into()];
    app.result_id = Some("old-result".into());
    let original_result = app.grid.as_ref().unwrap().page.result.id;
    let original_token = app.edit_token.clone();
    app.workbench.history.push(json!({"text":"SELECT 2"}));
    app.grid
        .as_mut()
        .unwrap()
        .edit(Cell { row: 0, column: 0 }, DataValue::Text("draft".into()))
        .unwrap();
    app.workbench_action("history-open:0", &backend);
    assert_eq!(app.tabs[0].query.text, "");
    assert!(app.grid.is_some());
    app.grid.as_mut().unwrap().discard();
    app.workbench_action("history-open:0", &backend);
    assert_eq!(app.tabs[app.active].query.text, "SELECT 2");
    assert!(app.grid.is_none() && app.result_id.is_none() && app.results.is_empty());
    assert!(app.target.is_none());
    app.switch_tab(0);
    assert_eq!(app.grid.as_ref().unwrap().page.result.id, original_result);
    assert_eq!(app.edit_token, original_token);
    assert!(app.browse_result);
    assert_eq!(app.view, "data");
    assert_eq!(app.target.as_ref().unwrap().name, "item");
}

#[test]
fn list_staged_edits_follow_original_rows_and_undo() {
    let (backend, _, _) = Backend::scripted();
    let rows = (0..3)
        .map(|i| {
            vec![
                DataValue::Integer(i.to_string()),
                DataValue::Bytes(STANDARD.encode("same")),
            ]
        })
        .collect();
    let mut app = app_for("redis", "list", page(&["index", "value"], rows));
    app.workbench_action("redis-edit", &backend);
    app.workbench_action("redis-op:list_delete", &backend);
    app.workbench_action("redis-stage", &backend);
    assert_eq!(app.redis_changes[0]["index"], json!(0));
    app.grid
        .as_mut()
        .unwrap()
        .select(Cell { row: 1, column: 1 }, false)
        .unwrap();
    app.workbench_action("redis-edit", &backend);
    app.field_set("redis_value", "changed");
    app.workbench_action("redis-stage", &backend);
    assert_eq!(app.redis_changes[1]["index"], json!(0));
    app.workbench_action("redis-undo", &backend);
    app.workbench_action("redis-undo", &backend);
    app.workbench_action("redis-edit", &backend);
    app.workbench_action("redis-op:list_insert", &backend);
    app.field_set("redis_value", "inserted");
    app.workbench_action("redis-stage", &backend);
    app.grid
        .as_mut()
        .unwrap()
        .select(Cell { row: 2, column: 1 }, false)
        .unwrap();
    app.workbench_action("redis-edit", &backend);
    app.workbench_action("redis-stage", &backend);
    assert_eq!(app.redis_changes[1]["index"], json!(3));
    app.workbench_action("redis-undo", &backend);
    app.workbench_action("redis-undo", &backend);
    app.workbench_action("redis-edit", &backend);
    app.workbench_action("redis-stage", &backend);
    assert_eq!(app.redis_changes[0]["index"], json!(2));
    assert_eq!(app.redis_list_index(-1, false).unwrap(), 2);
    assert_eq!(app.redis_list_index(-3, false).unwrap(), 0);
    app.grid
        .as_mut()
        .unwrap()
        .select(Cell { row: 0, column: 1 }, false)
        .unwrap();
    app.workbench_action("redis-edit", &backend);
    app.workbench_action("redis-op:list_delete", &backend);
    app.workbench_action("redis-stage", &backend);
    app.workbench_action("redis-edit", &backend);
    app.workbench_action("redis-stage", &backend);
    assert_eq!(app.redis_changes.len(), 2);
}

#[test]
fn cli_failure_keeps_partial_results_and_chunked_value_separate() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for(
        "redis",
        "string",
        page(
            &["value"],
            vec![vec![DataValue::Bytes(STANDARD.encode("key"))]],
        ),
    );
    let key_id = app.grid.as_ref().unwrap().page.result.id;
    app.field_set("cli_text", "PING\nGET x");
    app.workbench_action("cli-run", &backend);
    requests.try_recv().unwrap();
    app.workbench_response("cli.start", json!({"task_id":"t"}), &backend);
    let full = DataValue::Text("完整😀".into());
    let bytes = serde_json::to_vec(&full).unwrap();
    let id = Uuid::new_v4();
    let result = page(
        &["reply"],
        vec![
            vec![DataValue::Text("PONG".into())],
            vec![DataValue::Large {
                id,
                original_type: "text".into(),
                bytes: bytes.len() as u64,
                preview: "...".into(),
            }],
        ],
    );
    app.workbench_response(
        "execution.get",
        json!({"state":"failed","error":"bad command","results":[result.result.id]}),
        &backend,
    );
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_response(tag, serde_json::to_value(result).unwrap(), &backend);
    assert_eq!(
        app.workbench.cli_grid.as_ref().unwrap().page.rows[0][0],
        DataValue::Text("PONG".into())
    );
    assert!(app.workbench.cli_error.render().contains("bad command"));
    app.workbench_action("cli-open:1:0", &backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    let stale_tag = tag.to_owned();
    app.workbench_action("overlay-close", &backend);
    app.workbench_action("cli-open:1:0", &backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_response(
        &stale_tag,
        json!({"bytes_b64":STANDARD.encode(&bytes),"offset":0,"next":null,"total":bytes.len()}),
        &backend,
    );
    assert_eq!(app.workbench.cli_large.as_ref().unwrap().offset, 0);
    let split = bytes.len() / 2;
    app.workbench_response(tag, json!({"bytes_b64":STANDARD.encode(&bytes[..split]),"offset":0,"next":split,"total":bytes.len()}), &backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_response(tag, json!({"bytes_b64":STANDARD.encode(&bytes[split..]),"offset":split,"next":null,"total":bytes.len()}), &backend);
    assert_eq!(
        app.workbench
            .cli_grid
            .as_ref()
            .unwrap()
            .value(Cell { row: 1, column: 0 }),
        Some(&full)
    );
    assert_eq!(app.grid.as_ref().unwrap().page.result.id, key_id);
    assert!(app.workbench.cli_error.render().contains("bad command"));
}

#[test]
fn redis_history_opens_persistent_cli_without_touching_drafts() {
    let (backend, _, _) = Backend::scripted();
    let mut app = app_for(
        "redis",
        "string",
        page(
            &["value"],
            vec![vec![DataValue::Bytes(STANDARD.encode("key"))]],
        ),
    );
    let key_id = app.grid.as_ref().unwrap().page.result.id;
    app.redis_changes
        .push(json!({"operation":"set_string","value":{"type":"text","value":"draft"}}));
    app.workbench.history.push(json!({"text":"PING"}));
    app.workbench.cli_results.push("old".into());
    app.workbench_action("history-open:0", &backend);
    assert_eq!(app.field("cli_text"), "PING");
    assert_eq!(app.tabs[app.active].cli_query.text, "PING");
    assert!(app.workbench.cli && app.workbench.cli_results == ["old"]);
    assert_eq!(app.view, "data");
    assert_eq!(app.grid.as_ref().unwrap().page.result.id, key_id);
    assert_eq!(app.redis_changes.len(), 1);
}

#[test]
fn ttl_keep_set_and_persist_do_not_confuse_empty_input_with_removal() {
    let (backend, _, _) = Backend::scripted();
    let mut app = app_for(
        "redis",
        "string",
        page(
            &["value"],
            vec![vec![DataValue::Bytes(STANDARD.encode("original"))]],
        ),
    );
    app.workbench_action("redis-op:set_ttl", &backend);
    app.workbench_action("redis-stage", &backend);
    assert!(app.redis_changes.is_empty());

    app.workbench_action("redis-op:set_ttl", &backend);
    app.workbench_action("redis-ttl:set", &backend);
    app.field_set("redis_ttl", "");
    app.workbench_action("redis-stage", &backend);
    assert!(app.redis_changes.is_empty());
    assert_eq!(app.workbench.overlay.as_deref(), Some("redis"));

    app.field_set("redis_ttl", "60000");
    app.workbench_action("redis-stage", &backend);
    assert_eq!(app.redis_changes[0]["ttl_ms"], 60000);

    app.workbench_action("redis-op:set_ttl", &backend);
    app.workbench_action("redis-ttl:persist", &backend);
    app.workbench_action("redis-stage", &backend);
    assert!(app.redis_changes.last().unwrap()["ttl_ms"].is_null());
}

#[test]
fn tree_navigation_normalizes_pages_after_scan_and_collapse() {
    let (backend, _, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.workbench.tree = true;
    app.resources = (0..1000)
        .map(|index| datapier_core::model::Resource {
            name: format!("{}:{index}", if index < 300 { "a" } else { "b" }),
            kind: "string".into(),
            database: None,
            key_b64: None,
            data: Value::Null,
        })
        .collect();
    app.workbench.key_page = 4;
    app.normalize_key_page();
    assert_eq!(app.workbench.key_page, 0);
    app.workbench_action("redis-tree:YTo=", &backend);
    app.workbench_action("keys-next", &backend);
    assert_eq!(app.workbench.key_page, 1);
    app.workbench.key_page = 4;
    app.workbench_action("keys-previous", &backend);
    assert_eq!(app.workbench.key_page, 0);
    app.workbench_action("keys-next", &backend);
    app.workbench_action("redis-tree:YTo=", &backend);
    assert_eq!(app.workbench.key_page, 0);
}
