use super::*;

#[path = "workbench/cli.rs"]
mod cli;
#[path = "workbench/sql_edit.rs"]
mod sql_edit;

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
fn cli_toggle_persists_visibility_for_both_directions() {
    for initial in [false, true] {
        let (backend, _, _) = Backend::scripted();
        let mut app = app_for("redis", "string", page(&["value"], vec![]));
        app.workspace_loaded = true;
        app.workbench.cli = initial;
        app.dirty = false;

        app.action("cli-toggle", &backend);
        assert!(app.dirty);
        let state = serde_json::to_value(app.workspace_snapshot()).unwrap();
        let mut restored = App::new("restored".into());
        restored.response(
            Response {
                tag: "workspace.load".into(),
                result: Ok(json!({"revision":1,"state":state})),
            },
            &backend,
        );
        assert_eq!(restored.workbench.cli, !initial);
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
fn redis_database_picker_does_not_send_navigation_keys_to_cli() {
    let (backend, _, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.workbench.cli = true;
    app.focus = "cli_text".into();
    app.field_set("cli_text", "PING\nGET key");
    app.navigation.redis_db_menu = true;
    let before = (
        app.tabs[app.active].cli_query.cursor,
        app.tabs[app.active].cli_query.anchor,
    );
    for name in ["up", "down", "home", "end"] {
        assert!(app.workbench_key(&key(name, false), &backend));
    }
    assert_eq!(
        (
            app.tabs[app.active].cli_query.cursor,
            app.tabs[app.active].cli_query.anchor
        ),
        before
    );
    assert_eq!(app.field("cli_text"), "PING\nGET key");
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
            .contains(profile.id, profile.redis_db(), b"a:")
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

fn key(name: &str, shift: bool) -> tern_sdk::Key {
    let mut key = tern_sdk::Key::named(name);
    key.shift = shift;
    key
}
#[test]
fn regression_refresh_finishes_inline_without_replacing_its_row() {
    let (backend, mut requests, _responses) = Backend::scripted();
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
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "workspace.draft_scope");
    assert!(app.workbench.inline.is_none());
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"revision":request.params["revision"]})),
        },
        &backend,
    );
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
    let (backend, mut requests, _responses) = Backend::scripted();
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
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "workspace.draft_scope");
    assert!(app.workbench.inline.is_none());
    app.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"revision":request.params["revision"]})),
        },
        &backend,
    );
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
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "workspace.draft_scope");
    numeric.routed_response(
        Response {
            tag: request.tag,
            result: Ok(json!({"revision":request.params["revision"]})),
        },
        &backend,
    );
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
    app.workbench
        .archery_history
        .items
        .push(json!({"text":"SELECT 2"}));
    app.workbench.archery_history.selected = Some(0);
    app.grid
        .as_mut()
        .unwrap()
        .edit(Cell { row: 0, column: 0 }, DataValue::Text("draft".into()))
        .unwrap();
    app.application_action("history-open", &backend);
    assert_eq!(app.tabs[0].query.text, "");
    assert!(app.grid.is_some());
    app.grid.as_mut().unwrap().discard();
    app.application_action("history-open", &backend);
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
    app.workbench
        .archery_history
        .items
        .push(json!({"text":"PING"}));
    app.workbench.archery_history.selected = Some(0);
    app.workbench.cli_results.push("old".into());
    app.application_action("history-open", &backend);
    assert_ne!(app.field("cli_text"), "PING");
    assert_eq!(app.workbench.cli_results, ["old"]);
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
    app.workbench.redis_tree.reset(&app.resources);
    app.workbench.redis_tree_owner = app.redis_selection_scope();
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

#[test]
fn redis_tree_keeps_selection_on_append_and_resets_for_another_db() {
    let (backend, _, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.workbench.tree = true;
    let resource = |key: &str| {
        json!({
            "name": key, "kind": "string", "database": null,
            "key_b64": STANDARD.encode(key), "data": null
        })
    };
    app.response(Response {
        tag: "resource.list".into(),
        result: Ok(json!({"resources": [resource("b:one"), resource("z")], "cursor": "next", "complete": false})),
    }, &backend);
    app.workbench_action("redis-tree:Yjo=", &backend);
    app.redis_key_click(0, false, true, &backend);
    assert!(app.redis_key_selected(0));
    app.response(Response {
        tag: "resource.list".into(),
        result: Ok(json!({"resources": [resource("a:one"), resource("b:two")], "cursor": null, "complete": true})),
    }, &backend);
    assert!(app.redis_key_selected(0));
    let rows = app
        .workbench
        .redis_tree
        .rows(&app.workbench.redis_expansion, app.redis_selection_scope());
    let labels: Vec<_> = rows.iter().map(|row| row.label).collect();
    assert_eq!(labels, [b"b".as_slice(), b"one", b"two", b"z", b"a"]);

    app.tabs[app.active].browse.redis_db = Some(1);
    assert!(!app.redis_tree_matches_resources());
    app.response(
        Response {
            tag: "resource.list".into(),
            result: Ok(json!({"resources": [resource("b:new")], "cursor": null, "complete": true})),
        },
        &backend,
    );
    assert_eq!(app.resources.len(), 1);
    assert!(!app.redis_key_selected(0));
    let rows = app
        .workbench
        .redis_tree
        .rows(&app.workbench.redis_expansion, app.redis_selection_scope());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, b"b");
    assert!(matches!(
        rows[0].kind,
        crate::redis_tree::Kind::Directory {
            expanded: false,
            ..
        }
    ));

    app.action("list", &backend);
    app.response(
        Response {
            tag: "resource.list".into(),
            result: Ok(json!({"resources": [resource("c:new")], "cursor": null, "complete": true})),
        },
        &backend,
    );
    let rows = app
        .workbench
        .redis_tree
        .rows(&app.workbench.redis_expansion, app.redis_selection_scope());
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].label, b"c");
}

#[test]
fn cli_height_keyboard_preserves_input_and_workspace_preference() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.workbench.cli = true;
    app.field_set("cli_text", "GET retained");
    app.resize_cli(0.62);
    app.focus = "cli_text".into();

    assert!(app.workbench_key(&key("tab", false), &backend));
    assert_eq!(app.focus, "cli_height");
    assert!(app.workbench_key(&key("up", false), &backend));
    assert!((app.cli_share - 0.67).abs() < 1e-12);
    assert!(app.workbench_key(&key("end", false), &backend));
    assert!(app.workbench_key(&key("up", false), &backend));
    assert_eq!(app.cli_share, 0.85);
    assert!(app.workbench_key(&key("home", false), &backend));
    assert!(app.workbench_key(&key("down", false), &backend));
    assert_eq!(app.cli_share, 0.15);
    assert_eq!(app.field("cli_text"), "GET retained");
    assert!(requests.try_recv().is_err());

    app.resize_cli(0.62);
    app.workbench_action("cli-toggle", &backend);
    app.resize_cli(0.3);
    app.workbench_action("cli-toggle", &backend);
    assert_eq!(app.cli_share, 0.62);
    app.resize_cli(f64::NAN);
    assert_eq!(app.cli_share, 0.62);

    let mut other = app.tabs[0].clone();
    other.id = Uuid::new_v4();
    app.tabs.push(other);
    app.switch_tab(1);
    assert_eq!(app.cli_share, 0.62);
    app.switch_tab(0);
    assert_eq!(app.cli_share, 0.62);
    app.focus = "cli_height".into();
    assert!(app.workbench_key(&key("enter", false), &backend));
    assert_eq!(app.focus, "cli_text");
    assert!(requests.try_recv().is_err());
}

#[test]
fn readonly_redis_large_values_load_in_chunks_without_edit_baseline() {
    for kind in ["string", "hash"] {
        let (backend, mut requests, _) = Backend::scripted();
        let full = DataValue::Bytes(STANDARD.encode(br#"{"a":1}"#));
        let bytes = serde_json::to_vec(&full).unwrap();
        let id = Uuid::new_v4();
        let large = DataValue::Large {
            id,
            original_type: "bytes".into(),
            bytes: bytes.len() as u64,
            preview: "preview".into(),
        };
        let mut app = app_for("redis", kind, page(&["value"], vec![vec![large]]));
        app.profile.as_mut().unwrap().access_mode = AccessMode::ReadOnly;
        app.edit_token = None;
        app.grid.as_mut().unwrap().focus = Some(Cell { row: 0, column: 0 });
        app.action(
            if kind == "string" {
                "redis-view-string"
            } else {
                "overlay:cell"
            },
            &backend,
        );
        let request = requests.try_recv().unwrap();
        assert_eq!(request.method, "result.value");
        let tag = request.tag.splitn(3, ':').nth(2).unwrap();
        let split = bytes.len() / 2;
        app.large_response(tag, json!({"bytes_b64":STANDARD.encode(&bytes[..split]),"offset":0,"next":split,"total":bytes.len()}), &backend);
        let request = requests.try_recv().unwrap();
        assert_eq!(request.params["offset"], split);
        let tag = request.tag.splitn(3, ':').nth(2).unwrap();
        app.large_response(tag, json!({"bytes_b64":STANDARD.encode(&bytes[split..]),"offset":split,"next":null,"total":bytes.len()}), &backend);
        assert_eq!(
            app.grid.as_ref().unwrap().value(Cell { row: 0, column: 0 }),
            Some(&full)
        );
        assert_eq!(app.workbench.overlay.as_deref(), Some("cell"));
        assert!(app.edit_token.is_none());
        assert!(app.redis_changes.is_empty());
        if kind == "string" {
            fn content(node: &Node<Msg>) -> Option<&str> {
                if node.props().get("class").and_then(Value::as_str) == Some("value-content") {
                    return node.props().get("text").and_then(Value::as_str);
                }
                node.child_nodes().iter().find_map(content)
            }
            app.action("overlay-close", &backend);
            for (format, expected) in [
                ("text", "{\"a\":1}"),
                ("json", "{\n  \"a\": 1\n}"),
                ("hex", "7B 22 61 22 3A 31 7D"),
            ] {
                app.workbench_action(&format!("format:{format}"), &backend);
                let view = crate::views::render_window(&app, Some(Uuid::new_v4()));
                assert_eq!(
                    view.regions().into_iter().flatten().find_map(content),
                    Some(expected)
                );
            }
        }
    }
}
