use super::*;

#[test]
fn cli_records_keep_command_and_database_across_runs() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.tabs[app.active].browse.redis_db = Some(1);
    app.field_set("cli_text", "PING");
    app.workbench_action("cli-run", &backend);
    requests.try_recv().unwrap();
    app.workbench_response("cli.start", json!({"task_id":"first"}), &backend);
    app.workbench_response(
        "execution.get",
        json!({"state":"failed","error":"first error"}),
        &backend,
    );
    app.clear_cli_session();
    app.tabs[app.active].browse.redis_db = Some(2);
    app.field_set("cli_text", "GET x");
    app.workbench_action("cli-run", &backend);
    requests.try_recv().unwrap();
    app.workbench_response("cli.start", json!({"task_id":"second"}), &backend);
    app.workbench_response(
        "execution.get",
        json!({"state":"failed","error":"second error"}),
        &backend,
    );
    let history = &app.workbench.cli_history;
    assert_eq!(history.len(), 2);
    assert_eq!((&*history[0].command, &*history[0].database), ("PING", "1"));
    assert_eq!(
        (&*history[1].command, &*history[1].database),
        ("GET x", "2")
    );
    assert!(history[0].error.render().contains("first error"));
    assert!(history[1].error.render().contains("second error"));
}

#[test]
fn cli_completion_does_not_replace_selected_previous_record() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.field_set("cli_text", "PING");
    app.workbench_action("cli-run", &backend);
    requests.try_recv().unwrap();
    app.workbench_response("cli.start", json!({"task_id":"first"}), &backend);
    let first = page(&["reply"], vec![vec![DataValue::Text("PONG".into())]]);
    app.workbench_response(
        "execution.get",
        json!({"state":"succeeded","results":[first.result.id]}),
        &backend,
    );
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_response(tag, serde_json::to_value(&first).unwrap(), &backend);
    let first_id = app.workbench.cli_history[0].id;
    app.field_set("cli_text", "GET x");
    app.workbench_action("cli-run", &backend);
    requests.try_recv().unwrap();
    app.workbench_response("cli.start", json!({"task_id":"second"}), &backend);
    app.workbench_action(&format!("cli-record:{first_id}:0"), &backend);
    let second_id = Uuid::new_v4();
    app.workbench_response(
        "execution.get",
        json!({"state":"succeeded","results":[second_id]}),
        &backend,
    );
    assert_eq!(app.workbench.cli_selected, Some(first_id));
    assert_eq!(
        app.workbench.cli_grid.as_ref().unwrap().page.result.id,
        first.result.id
    );
    assert_eq!(
        app.workbench.cli_history[1].results,
        [second_id.to_string()]
    );
}

#[test]
fn cli_export_uses_selected_result_instead_of_key_result() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    let selected = Uuid::new_v4();
    app.result_id = Some(Uuid::new_v4().to_string());
    app.workbench.cli_result_id = Some(selected.to_string());
    app.field_set("path", "/tmp/cli-export-test.json");
    app.action("cli-export", &backend);
    app.action("export", &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "transfer.export");
    assert_eq!(request.params["result_id"], selected.to_string());
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
    let view = crate::views::render_window(&app, Some(Uuid::new_v4()));
    assert_eq!(
        view.regions().into_iter().flatten().find_map(command_text),
        Some("PING")
    );
    app.workbench
        .archery_history
        .items
        .push(json!({"text":"INFO"}));
    app.workbench.archery_history.selected = Some(0);
    app.application_action("history-open", &backend);
    assert_eq!(app.field("cli_text"), "GET another-key");
    let view = crate::views::render_window(&app, Some(Uuid::new_v4()));
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
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_response(tag, serde_json::to_value(result).unwrap(), &backend);
    let preload = requests.try_recv().unwrap();
    let preload_tag = preload.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_response(
        preload_tag,
        serde_json::to_value(&second).unwrap(),
        &backend,
    );
    let grids = &app.workbench.cli_history[0].result_grids;
    assert_eq!(grids.len(), 2);
    assert!(grids[0].is_none());
    assert_eq!(
        app.workbench.cli_grid.as_ref().unwrap().page.rows[0][0],
        DataValue::Text("PONG".into())
    );
    assert_eq!(
        grids[1].as_ref().unwrap().page.rows[0][0],
        DataValue::Text("SECOND".into())
    );
    app.workbench.cli_grid.as_mut().unwrap().page.next = Some(200);
    let mut later = app.workbench.cli_grid.as_ref().unwrap().page.clone();
    later.start = 200;
    later.next = None;
    app.workbench_action("cli-next", &backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_response(tag, serde_json::to_value(later).unwrap(), &backend);
    assert_eq!(app.workbench.cli_page_history, [0]);
    let record = app.workbench.cli_selected.unwrap();
    app.workbench_action(&format!("cli-record:{record}:1"), &backend);
    let view = crate::views::render_window(&app, Some(Uuid::new_v4()));
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
    app.workbench_action(&format!("cli-record:{record}:0"), &backend);
    assert_eq!(app.workbench.cli_grid.as_ref().unwrap().page.start, 200);
    assert_eq!(app.workbench.cli_page_history, [0]);
    app.workbench_action(&format!("cli-record:{record}:0"), &backend);
    assert_eq!(app.workbench.cli_grid.as_ref().unwrap().page.start, 200);
    app.workbench_action("cli-previous", &backend);
    assert_eq!(requests.try_recv().unwrap().params["start"], 0);
}

#[test]
fn cli_wait_stopped_finishes_without_claiming_cancellation() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.field_set("cli_text", "PING");
    app.workbench_action("cli-run", &backend);
    requests.try_recv().unwrap();
    app.workbench_response("cli.start", json!({"task_id":"t"}), &backend);
    assert!(app.workbench.cli_running);
    app.workbench_response(
        "execution.get",
        json!({"state":"wait_stopped","results":[]}),
        &backend,
    );
    assert!(!app.workbench.cli_running);
    assert!(app.task.is_none());
    assert!(!app.operation_pending);
    assert!(
        app.workbench
            .cli_error
            .render()
            .contains(crate::i18n::tr("已结束等待").as_ref())
    );
    assert!(requests.try_recv().is_err());
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
        json!({"data_b64":STANDARD.encode(&bytes),"offset":0,"next":null,"total_bytes":bytes.len()}),
        &backend,
    );
    assert_eq!(app.workbench.cli_large.as_ref().unwrap().offset, 0);
    let split = bytes.len() / 2;
    app.workbench_response(tag, json!({"data_b64":STANDARD.encode(&bytes[..split]),"offset":0,"next":split,"total_bytes":bytes.len()}), &backend);
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_response(tag, json!({"data_b64":STANDARD.encode(&bytes[split..]),"offset":split,"next":null,"total_bytes":bytes.len()}), &backend);
    assert_eq!(
        app.workbench
            .cli_grid
            .as_ref()
            .unwrap()
            .value(Cell { row: 1, column: 0 }),
        Some(&full)
    );
    assert!(app.workbench.cli_error.render().contains("bad command"));
    let cell = Cell { row: 1, column: 0 };
    let pointer = match app
        .workbench
        .cli_grid
        .as_ref()
        .unwrap()
        .value(cell)
        .unwrap()
    {
        DataValue::Text(text) => text.as_ptr(),
        _ => panic!("expected resolved text"),
    };
    app.workbench_action("cli-open:1:0", &backend);
    assert!(requests.try_recv().is_err());
    assert!(
        matches!(app.workbench.cli_grid.as_ref().unwrap().value(cell), Some(DataValue::Text(text)) if text.as_ptr() == pointer)
    );
    let record_id = app.workbench.cli_history[0].id;
    let result_id = app.workbench.cli_grid.as_ref().unwrap().page.result.id;
    assert!(app.workbench.cli_history[0].result_grids[0].is_none());
    app.clear_cli_session();
    let stored = app.workbench.cli_history[0].result_grids[0]
        .as_ref()
        .unwrap();
    assert!(matches!(stored.value(cell), Some(DataValue::Text(text)) if text.as_ptr() == pointer));
    app.workbench_action(&format!("cli-record:{record_id}:0"), &backend);
    assert!(requests.try_recv().is_err());
    assert!(app.workbench.cli_history[0].result_grids[0].is_none());
    let mut stale = page(&["reply"], vec![vec![DataValue::Text("stale".into())]]);
    stale.result.id = result_id;
    app.workbench_response(
        &format!("cli.record/{record_id}/0"),
        serde_json::to_value(stale).unwrap(),
        &backend,
    );
    assert!(app.workbench.cli_history[0].result_grids[0].is_none());
    assert!(
        matches!(app.workbench.cli_grid.as_ref().unwrap().value(cell), Some(DataValue::Text(text)) if text.as_ptr() == pointer)
    );
    app.create_query_tab();
    let snapshot = app.workspace_snapshot();
    assert!(snapshot.tabs[0].runtime.workbench.cli_history.is_empty());
    assert!(
        matches!(app.tabs[0].runtime.workbench.cli_grid.as_ref().unwrap().value(cell), Some(DataValue::Text(text)) if text.as_ptr() == pointer)
    );
    app.switch_tab(0);
    assert!(
        matches!(app.workbench.cli_grid.as_ref().unwrap().value(cell), Some(DataValue::Text(text)) if text.as_ptr() == pointer)
    );
    assert_eq!(app.grid.as_ref().unwrap().page.result.id, key_id);
}
#[test]
fn cli_auth_never_creates_request_or_execution_record() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    for command in [
        "AUTH secret",
        "ping\nAuTh user secret",
        "HELLO 3 AUTH user secret",
        "AUTH \"unfinished",
    ] {
        app.field_set("cli_text", command);
        app.workbench_action("cli-run", &backend);
        assert!(requests.try_recv().is_err());
        assert!(app.workbench.cli_history.is_empty());
        assert!(!app.status.render().contains("secret"));
    }
    app.field_set("cli_text", "GET AUTH");
    app.workbench_action("cli-run", &backend);
    assert_eq!(requests.try_recv().unwrap().method, "execution.start");
}

#[test]
fn cli_header_attributes_selected_result_to_its_record() {
    fn heads(node: &Node<Msg>, found: &mut Vec<String>) {
        if node.props().get("class").and_then(Value::as_str) == Some("cli-head") {
            fn text(node: &Node<Msg>, result: &mut String) {
                if let Some(value) = node.props().get("text").and_then(Value::as_str) {
                    result.push_str(value);
                }
                for child in node.child_nodes() {
                    text(child, result);
                }
            }
            let mut value = String::new();
            text(node, &mut value);
            found.push(value);
        }
        for child in node.child_nodes() {
            heads(child, found);
        }
    }
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.workbench.cli = true;
    app.tabs[0].browse.redis_db = Some(2);
    let result = page(&["reply"], vec![vec![DataValue::Text("PONG".into())]]);
    let id = Uuid::new_v4();
    app.workbench.cli_history.push(crate::workbench::CliRecord {
        id,
        command: "PING".into(),
        instance: "old-instance".into(),
        database: "1".into(),
        state: UiMessage::key("完成"),
        elapsed: Some(std::time::Duration::from_millis(13)),
        error: UiMessage::default(),
        results: vec![result.result.id.to_string()],
        result_grids: vec![None],
        result_page_histories: vec![vec![]],
    });
    app.workbench.cli_selected = Some(id);
    app.workbench.cli_result_id = Some(result.result.id.to_string());
    app.workbench
        .cli_result_databases
        .insert(result.result.id.to_string(), 7);
    app.workbench.cli_database = Some(Some(3));
    app.workbench.cli_grid = Some(GridState::new(result, false));
    app.workbench.cli_elapsed = Some(std::time::Duration::from_millis(99));
    let view = crate::views::render_window(&app, Some(Uuid::new_v4()));
    let mut found = Vec::new();
    for node in view.regions().into_iter().flatten() {
        heads(node, &mut found);
    }
    assert_eq!(found.len(), 2);
    assert!(found[0].contains("DB 3"));
    assert!(
        found[1].contains("PING") && found[1].contains("old-instance") && found[1].contains("DB 7")
    );
    assert!(found[1].contains("13 ms"));
    assert!(!found[1].contains("99 ms"));
}

#[test]
fn cli_tracks_command_db_independently_from_browse_and_per_result() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.tabs[0].browse.redis_db = Some(0);
    app.field_set("cli_text", "SELECT 2");
    app.workbench_action("cli-run", &backend);
    requests.try_recv().unwrap();
    app.workbench_response("cli.start", json!({"task_id":"select"}), &backend);
    let first = page(&["reply"], vec![vec![DataValue::Text("OK".into())]]);
    app.workbench_response(
        "execution.get",
        json!({
            "state":"succeeded", "context":{"redis_db":0},
            "summary":{"command_db":2,"redis_results":[{"result_id":first.result.id,"redis_db":0}]},
            "results":[first.result.id]
        }),
        &backend,
    );
    let request = requests.try_recv().unwrap();
    let tag = request.tag.splitn(3, ':').nth(2).unwrap();
    app.workbench_response(tag, serde_json::to_value(&first).unwrap(), &backend);
    assert_eq!(app.workbench.cli_database, Some(Some(2)));
    assert_eq!(app.tabs[0].browse.redis_db, Some(0));
    assert_eq!(app.workbench.cli_history[0].database, "0");
    assert_eq!(
        app.workbench.cli_result_databases[&first.result.id.to_string()],
        0
    );

    app.field_set("cli_text", "GET key\nSELECT 3\nGET other");
    app.workbench_action("cli-run", &backend);
    requests.try_recv().unwrap();
    assert_eq!(app.workbench.cli_history[1].database, "2");
    app.workbench_response("cli.start", json!({"task_id":"second"}), &backend);
    let second = Uuid::new_v4();
    let third = Uuid::new_v4();
    app.workbench_response("execution.get", json!({
        "state":"failed", "context":{"redis_db":2}, "error":"later failure",
        "summary":{"command_db":3,"redis_results":[{"result_id":second,"redis_db":2},{"result_id":third,"redis_db":3}]},
        "results":[second,third]
    }), &backend);
    assert_eq!(app.workbench.cli_database, Some(Some(3)));
    assert_eq!(app.tabs[0].browse.redis_db, Some(0));
    assert_eq!(app.workbench.cli_history[1].database, "2");
    assert_eq!(app.workbench.cli_result_databases[&second.to_string()], 2);
    assert_eq!(app.workbench.cli_result_databases[&third.to_string()], 3);
    let first_record = app.workbench.cli_history[0].id;
    app.workbench_action(&format!("cli-record:{first_record}:0"), &backend);
    assert_eq!(
        app.workbench.cli_result_id.as_deref(),
        Some(first.result.id.to_string().as_str())
    );
    assert_eq!(app.workbench.cli_database, Some(Some(3)));
    app.workbench_action("cli-export", &backend);
    assert!(app.workbench.cli_export);
}

#[test]
fn cli_unknown_command_db_is_not_replaced_by_browse_db() {
    let (backend, mut requests, _) = Backend::scripted();
    let mut app = app_for("redis", "string", page(&["value"], vec![]));
    app.tabs[0].browse.redis_db = Some(1);
    app.field_set("cli_text", "PING");
    app.workbench_action("cli-run", &backend);
    requests.try_recv().unwrap();
    app.workbench_response("cli.start", json!({"task_id":"t"}), &backend);
    app.workbench_response(
        "execution.get",
        json!({"state":"failed","context":{"redis_db":1},"summary":{"command_db":null}}),
        &backend,
    );
    assert_eq!(app.workbench.cli_database, Some(None));
    assert_eq!(app.tabs[0].browse.redis_db, Some(1));
}
