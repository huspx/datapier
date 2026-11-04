use super::*;
fn pending(app: &App) -> Clipboard {
    Clipboard {
        cut: Some(PendingCut {
            id: Uuid::nil(),
            tab: app.tabs[app.active].id,
            range: 8..10,
            original: app.tabs[app.active].query.text.clone(),
            started: Instant::now(),
            anchor: app.tabs[app.active].query.anchor,
            cursor: app.tabs[app.active].query.cursor,
        }),
        copy: None,
        redis: None,
        grid: None,
        cli: None,
        redis_write: None,
    }
}
fn app() -> App {
    let mut app = App::new("clipboard".into());
    app.tabs[app.active].query = Editor::new("SELECT '😀'; SELECT '😀';");
    app
}

#[test]
fn cli_copy_cannot_resume_after_leaving_its_tab_or_replacing_log() {
    let mut app = app();
    app.profile_form(None);
    app.field_set("provider", "redis");
    app.profile = Some(app.form_profile().unwrap());
    app.workbench.cli_history.push(crate::workbench::CliRecord {
        id: Uuid::new_v4(),
        command: "PING".into(),
        instance: "fixture".into(),
        database: "0".into(),
        state: i18n::UiMessage::raw("succeeded"),
        elapsed: None,
        error: i18n::UiMessage::default(),
        results: Vec::new(),
        result_grids: Vec::new(),
        result_page_histories: Vec::new(),
    });
    let (backend, _requests, _) = Backend::scripted();
    let mut clipboard = Clipboard::default();
    clipboard.copy_cli(&mut app, &backend);
    assert!(clipboard.cli.as_ref().unwrap().matches(&app));
    app.generation += 1;
    clipboard.observe_redis_context(&app);
    app.generation -= 1;
    assert!(clipboard.cli.is_none());
    clipboard.copy_cli(&mut app, &backend);
    let record = app.workbench.cli_history.pop().unwrap();
    clipboard.observe_redis_context(&app);
    app.workbench.cli_history.push(record);
    assert!(clipboard.cli.is_none());
    clipboard.copy_cli(&mut app, &backend);
    clipboard.copy_redis_value(&mut app, &backend);
    assert!(clipboard.cli.is_none());
}
#[test]
fn grid_copy_freezes_large_values_and_rejects_selection_or_page_changes() {
    use datapier_core::model::{Column, ResultMeta, Value as DataValue};
    let mut app = app();
    let large = DataValue::Large {
        id: Uuid::new_v4(),
        original_type: "text".into(),
        bytes: 200,
        preview: "truncated".into(),
    };
    app.grid = Some(GridState::new(
        ResultPage {
            result: ResultMeta {
                id: Uuid::new_v4(),
                execution_id: Uuid::new_v4(),
                columns: vec![Column::new("v", "text")],
                kind: "rows".into(),
                row_count: 2,
                complete: true,
                summary: Value::Null,
            },
            rows: vec![vec![large.clone()], vec![DataValue::Text("other".into())]],
            start: 0,
            next: None,
        },
        false,
    ));
    let (backend, mut requests, _) = Backend::scripted();
    let mut clipboard = Clipboard::default();
    clipboard.copy_grid(&mut app, &backend);
    let request = requests.try_recv().unwrap();
    assert_eq!(request.method, "local.grid.copy");
    assert_eq!(request.params["rows"], json!([[large]]));
    assert!(clipboard.grid.as_ref().unwrap().matches(&app));
    app.grid
        .as_mut()
        .unwrap()
        .select(Cell { row: 1, column: 0 }, false)
        .unwrap();
    clipboard.observe_redis_context(&app);
    assert!(clipboard.grid.is_none());
    clipboard.copy_grid(&mut app, &backend);
    app.grid.as_mut().unwrap().page.rows[1][0] = DataValue::Text("changed".into());
    clipboard.observe_redis_context(&app);
    assert!(clipboard.grid.is_none());
    clipboard.copy_grid(&mut app, &backend);
    app.grid.as_mut().unwrap().page.start = 2;
    clipboard.observe_redis_context(&app);
    assert!(clipboard.grid.is_none());
}

#[test]
fn redis_copy_is_invalidated_when_context_changes_even_if_restored() {
    use datapier_core::model::{Column, ResultMeta};
    let mut app = app();
    app.profile_form(None);
    app.field_set("provider", "redis");
    app.profile = Some(app.form_profile().unwrap());
    app.target = Some(ResourceTarget {
        database: None,
        name: "first".into(),
        key_b64: None,
        kind: "string".into(),
    });
    app.browse_result = true;
    app.schema = Some(json!({"type":"string"}));
    app.grid = Some(GridState::new(
        ResultPage {
            result: ResultMeta {
                id: Uuid::new_v4(),
                execution_id: Uuid::new_v4(),
                columns: vec![Column::new("value", "bytes")],
                kind: "redis".into(),
                row_count: 1,
                complete: true,
                summary: Value::Null,
            },
            rows: vec![vec![datapier_core::model::Value::Bytes("YQ==".into())]],
            start: 0,
            next: None,
        },
        false,
    ));
    let (backend, _requests, _) = Backend::scripted();
    let mut clipboard = Clipboard::default();
    clipboard.copy_redis_value(&mut app, &backend);
    assert!(clipboard.redis.is_some());
    app.target.as_mut().unwrap().name = "second".into();
    clipboard.observe_redis_context(&app);
    app.target.as_mut().unwrap().name = "first".into();
    assert!(clipboard.redis.is_none());
    clipboard.copy_redis_value(&mut app, &backend);
    app.redis_changes.push(json!({"operation":"set_string"}));
    app.redis_changes_revision += 1;
    clipboard.observe_redis_context(&app);
    assert!(clipboard.redis.is_none());
    app.redis_changes.clear();
    app.redis_changes_revision += 1;
    clipboard.copy_redis_value(&mut app, &backend);
    let context = clipboard.redis.take().unwrap();
    let mut write = RedisWrite {
        context: CopyContext::Redis(context),
        id: Uuid::nil(),
        text: "x".repeat(REDIS_COPY_BATCH + 17),
        offset: 0,
        started: false,
    };
    let mut frames = Vec::new();
    assert!(
        !write
            .batch(|bytes| {
                frames.push(bytes.to_vec());
                Ok(())
            })
            .unwrap()
    );
    assert_eq!(write.offset, REDIS_COPY_BATCH);
    let end = format!("\x1b]5522;type=wdata:id={}\x1b\\", write.id);
    assert!(!frames.iter().any(|frame| frame == end.as_bytes()));
    clipboard.redis_write = Some(write);
    app.target.as_mut().unwrap().name = "second".into();
    clipboard.observe_redis_context(&app);
    assert!(clipboard.redis_write.is_none());
    assert!(!frames.iter().any(|frame| frame == end.as_bytes()));

    app.target.as_mut().unwrap().name = "first".into();
    clipboard.copy_redis_value(&mut app, &backend);
    clipboard.redis_write = Some(RedisWrite {
        context: CopyContext::Redis(clipboard.redis.take().unwrap()),
        id: Uuid::nil(),
        text: "pending".into(),
        offset: 0,
        started: false,
    });
    app.redis_changes.push(json!({"operation":"set_string"}));
    app.redis_changes_revision += 1;
    clipboard.observe_redis_context(&app);
    assert!(clipboard.redis_write.is_none());
    app.redis_changes.clear();
    app.redis_changes_revision += 1;
    clipboard.copy_redis_value(&mut app, &backend);
    let mut write = RedisWrite {
        context: CopyContext::Redis(clipboard.redis.take().unwrap()),
        id: Uuid::nil(),
        text: "x".repeat(REDIS_COPY_BATCH + 17),
        offset: 0,
        started: false,
    };
    let mut frames = Vec::new();
    assert!(
        !write
            .batch(|bytes| {
                frames.push(bytes.to_vec());
                Ok(())
            })
            .unwrap()
    );
    assert!(
        write
            .batch(|bytes| {
                frames.push(bytes.to_vec());
                Ok(())
            })
            .unwrap()
    );
    assert_eq!(frames.last().unwrap(), end.as_bytes());
    let decoded: Vec<u8> = frames
        .iter()
        .filter_map(|frame| {
            let text = std::str::from_utf8(frame).unwrap();
            text.split_once(";type=wdata:")?
                .1
                .split_once(';')
                .map(|(_, data)| {
                    STANDARD
                        .decode(data.strip_suffix("\x1b\\").unwrap())
                        .unwrap()
                })
        })
        .flatten()
        .collect();
    assert_eq!(decoded, write.text.as_bytes());

    clipboard.copy_redis_value(&mut app, &backend);
    let mut write = RedisWrite {
        context: CopyContext::Redis(clipboard.redis.take().unwrap()),
        id: Uuid::nil(),
        text: "x".repeat(REDIS_COPY_BATCH + 17),
        offset: 0,
        started: false,
    };
    let mut frames = Vec::new();
    assert!(
        write
            .batch(|bytes| {
                if bytes.windows(11).any(|window| window == b"type=wdata:") {
                    return Err(AppError::invalid("write failed"));
                }
                frames.push(bytes.to_vec());
                Ok(())
            })
            .is_err()
    );
    assert!(!frames.iter().any(|frame| frame == end.as_bytes()));
}

#[test]
fn denied_write_preserves_sql_and_undo() {
    let mut app = app();
    let mut clipboard = pending(&app);
    assert!(clipboard.reply(
        &mut app,
        b"type=write:id=00000000-0000-0000-0000-000000000000:status=EPERM"
    ));
    assert_eq!(app.tabs[app.active].query.text, "SELECT '😀'; SELECT '😀';");
    assert!(!app.tabs[app.active].query.undo());
}
#[test]
fn closed_query_cut_ack_is_consumed_without_recreating_tab() {
    let mut app = app();
    let mut clipboard = pending(&app);
    app.tabs.clear();
    assert!(clipboard.reply(
        &mut app,
        b"type=write:id=00000000-0000-0000-0000-000000000000:status=DONE"
    ));
    assert!(clipboard.cut.is_none());
    assert!(app.tabs.is_empty());
    assert!(!clipboard.reply(
        &mut app,
        b"type=write:id=00000000-0000-0000-0000-000000000000:status=DONE"
    ));
}

#[test]
fn confirmed_write_cuts_exact_unicode_range_in_one_undo_step() {
    let mut app = app();
    let mut clipboard = pending(&app);
    assert!(clipboard.reply(
        &mut app,
        b"type=write:id=00000000-0000-0000-0000-000000000000:status=DONE"
    ));
    assert_eq!(app.tabs[app.active].query.text, "SELECT ''; SELECT '😀';");
    assert!(app.tabs[app.active].query.undo());
    assert_eq!(app.tabs[app.active].query.text, "SELECT '😀'; SELECT '😀';");
}
#[test]
fn changed_context_and_unrelated_ack_cannot_cut() {
    let mut app = app();
    let mut clipboard = pending(&app);
    app.tabs[app.active].query = Editor::new("SELECT '😀'; SELECT 'other';");
    assert!(!clipboard.reply(&mut app, b"type=write:id=other:status=DONE"));
    assert!(clipboard.reply(
        &mut app,
        b"type=write:id=00000000-0000-0000-0000-000000000000:status=DONE"
    ));
    assert_eq!(
        app.tabs[app.active].query.text,
        "SELECT '😀'; SELECT 'other';"
    );
}
#[test]
fn moved_selection_during_cut_preserves_text() {
    let mut app = app();
    let mut clipboard = pending(&app);
    assert!(app.tabs[app.active].query.set_selection(0, 0));
    assert!(clipboard.reply(
        &mut app,
        b"type=write:id=00000000-0000-0000-0000-000000000000:status=DONE"
    ));
    assert_eq!(app.tabs[app.active].query.text, "SELECT '😀'; SELECT '😀';");
    assert!(!app.tabs[app.active].query.undo());
}

#[test]
fn copy_reports_host_result_and_timeout() {
    let mut app = app();
    let mut clipboard = Clipboard {
        cut: None,
        copy: Some((Uuid::nil(), Instant::now())),
        redis: None,
        grid: None,
        cli: None,
        redis_write: None,
    };
    assert!(!clipboard.reply(&mut app, b"type=write:id=other:status=DONE"));
    assert!(clipboard.reply(
        &mut app,
        b"type=write:id=00000000-0000-0000-0000-000000000000:status=EPERM"
    ));
    assert!(matches!(
        &app.status,
        i18n::UiMessage::Template("剪贴板写入失败：{error}", _)
    ));
    clipboard.copy = Some((Uuid::nil(), Instant::now()));
    assert!(clipboard.reply(
        &mut app,
        b"type=write:id=00000000-0000-0000-0000-000000000000:status=DONE"
    ));
    assert!(matches!(&app.status, i18n::UiMessage::Key("已复制")));
    clipboard.copy = Some((Uuid::nil(), Instant::now() - Duration::from_secs(31)));
    assert!(clipboard.expire(&mut app));
    assert!(matches!(
        &app.status,
        i18n::UiMessage::Key("剪贴板未确认写入")
    ));
}
