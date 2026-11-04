use super::*;

#[tokio::test]
async fn pagination_rpc_uses_registered_client_and_cached_result_pages() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let result_id = engine
        .store
        .start_result(Uuid::new_v4(), vec![], "rows")
        .await
        .unwrap();
    engine
        .store
        .append_rows(
            result_id,
            vec![vec![model::Value::Text("a".repeat(200))]; 45],
        )
        .await
        .unwrap();
    let params = json!({"result_id": result_id, "page_size": 20, "max_bytes": 1024});
    assert_eq!(
        engine
            .dispatch(owner, "result.pagination", params.clone())
            .await
            .unwrap_err()
            .code,
        "disconnected"
    );
    assert!(engine.register(owner));
    let plan = engine
        .dispatch(owner, "result.pagination", params)
        .await
        .unwrap();
    assert_eq!(plan["row_count"], 45);
    assert_eq!(plan["complete"], false);
    let starts = plan["starts"].as_array().unwrap();
    let mut count = 0u64;
    for (index, start) in starts.iter().enumerate() {
        assert_eq!(start.as_u64(), Some(count));
        let page = engine
            .dispatch(
                owner,
                "result.page",
                json!({"result_id":result_id,"start":count,"max_rows":20,"max_bytes":1024}),
            )
            .await
            .unwrap();
        count += page["rows"].as_array().unwrap().len() as u64;
        assert_eq!(
            page["next"].as_u64(),
            starts.get(index + 1).and_then(Value::as_u64)
        );
    }
    assert_eq!(count, 45);
    assert_eq!(
        engine
            .dispatch(
                owner,
                "result.pagination",
                json!({"result_id":result_id,"page_size":1})
            )
            .await
            .unwrap_err()
            .code,
        "invalid_input"
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn terminal_states_keep_real_start_and_monotonic_elapsed() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    for (result, sent, expected) in [
        (Ok(json!({"affected_rows":1})), false, "succeeded"),
        (Err(AppError::invalid("bad")), false, "failed"),
        (Err(AppError::cancelled()), false, "cancelled"),
        (Err(AppError::cancelled()), true, "outcome_unknown"),
    ] {
        let task = engine
            .store
            .create_task(owner, "w", "execution.start", json!({}), false)
            .await
            .unwrap();
        engine
            .store
            .update_task(
                task.id,
                "running",
                json!({"_task":{"started_at":123.5}}),
                None,
            )
            .await
            .unwrap();
        if sent {
            engine.store.mark_sent(task.id, true).await.unwrap();
        }
        let started = Instant::now() - Duration::from_millis(25);
        engine
            .finish_task(owner, task.id, &result, Some(started))
            .await;
        let task = engine.task(task.id).await.unwrap();
        assert_eq!(task.state, expected);
        assert_eq!(task.started_at, Some(123.5));
        assert!(task.elapsed_ms.unwrap() >= 25);
    }
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancelled_sent_batch_requires_complete_confirmed_rollback() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let rolled_back = json!({"items":[
        {"index":0,"status":"rolled_back"},
        {"index":1,"status":"rolled_back"}
    ]});
    for (method, code, details, expected) in [
        (
            "change.submit",
            "cancelled",
            rolled_back.clone(),
            "cancelled",
        ),
        (
            "change.submit",
            "cancelled",
            json!({"items":[{"index":0,"status":"rolled_back"},{"index":1,"status":"applied"}]}),
            "outcome_unknown",
        ),
        (
            "change.submit",
            "cancelled",
            json!({"items":[{"index":0,"status":"unknown"}]}),
            "outcome_unknown",
        ),
        (
            "change.submit",
            "cancelled",
            json!({"items":[]}),
            "outcome_unknown",
        ),
        ("change.submit", "cancelled", json!({}), "outcome_unknown"),
        (
            "change.submit",
            "outcome_unknown",
            rolled_back.clone(),
            "outcome_unknown",
        ),
        (
            "transfer.import",
            "cancelled",
            rolled_back,
            "outcome_unknown",
        ),
    ] {
        let task = engine
            .store
            .create_task(owner, "workspace", method, json!({}), false)
            .await
            .unwrap();
        engine.store.mark_sent(task.id, true).await.unwrap();
        let result: Result<Value> = Err(AppError::new(code, "test").with_details(details));
        engine.finish_task(owner, task.id, &result, None).await;
        assert_eq!(engine.store.task(task.id).await.unwrap().state, expected);
    }
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn result_release_dispatch_retries_external_file_cleanup() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let execution_id = Uuid::new_v4();
    let result_id = engine
        .store
        .start_result(execution_id, vec![model::Column::new("v", "TEXT")], "rows")
        .await
        .unwrap();
    engine
        .store
        .append_rows(result_id, vec![vec![model::Value::Text("x".repeat(70000))]])
        .await
        .unwrap();
    let page = engine
        .store
        .result_page(result_id, 0, 1, 16384)
        .await
        .unwrap();
    let value_id = match &page.rows[0][0] {
        model::Value::Large { id, .. } => *id,
        _ => panic!("字段应当外置"),
    };
    let path = root.join("values").join(value_id.to_string());
    engine.state.lock().unwrap().tasks.insert(
        execution_id,
        Running {
            owner,
            session_id: Uuid::new_v4(),
            cancel: CancellationToken::new(),
            transition: Arc::new(AsyncMutex::new(())),
            abort: None,
            started: None,
        },
    );
    let params = json!({"result_id": result_id});
    assert_eq!(
        engine
            .dispatch(owner, "result.release", params.clone())
            .await
            .unwrap_err()
            .code,
        "busy"
    );
    engine.state.lock().unwrap().tasks.remove(&execution_id);
    tokio::fs::remove_file(&path).await.unwrap();
    tokio::fs::create_dir(&path).await.unwrap();
    assert!(
        engine
            .dispatch(owner, "result.release", params.clone())
            .await
            .is_err()
    );
    assert_eq!(
        engine
            .dispatch(owner, "result.page", params.clone())
            .await
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(
        engine.store.read_value(value_id).await.unwrap_err().code,
        "not_found"
    );
    tokio::fs::remove_dir(&path).await.unwrap();
    tokio::fs::write(&path, b"remaining").await.unwrap();
    assert_eq!(
        engine
            .dispatch(owner, "result.release", params.clone())
            .await
            .unwrap(),
        json!({"released": true})
    );
    assert!(!path.exists());
    assert_eq!(
        engine
            .dispatch(owner, "result.page", params)
            .await
            .unwrap_err()
            .code,
        "not_found"
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn settings_are_shared_and_reject_unknown_language() {
    let (engine, root) = engine().await;
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    assert!(engine.register(first));
    assert!(engine.register(second));
    let mut events = engine.state.lock().unwrap().event_senders[&second].subscribe();
    assert_eq!(
        engine
            .dispatch(first, "settings.get", json!({}))
            .await
            .unwrap()["language"],
        "system"
    );
    engine
        .dispatch(
            first,
            "settings.set",
            json!({"settings":{"language":"zh-CN"}}),
        )
        .await
        .unwrap();
    assert_eq!(events.try_recv().unwrap().kind, "settings.changed");
    assert_eq!(
        engine
            .dispatch(second, "settings.get", json!({}))
            .await
            .unwrap()["language"],
        "zh-CN"
    );
    assert_eq!(
        engine
            .dispatch(
                second,
                "settings.set",
                json!({"settings":{"language":"unknown"}})
            )
            .await
            .unwrap_err()
            .code,
        "invalid_input"
    );
    assert_eq!(
        engine
            .dispatch(first, "settings.get", json!({}))
            .await
            .unwrap()["language"],
        "zh-CN"
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
