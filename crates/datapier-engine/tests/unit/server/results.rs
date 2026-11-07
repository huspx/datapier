use super::*;

#[tokio::test]
async fn result_pages_require_the_owner_and_obey_byte_limits() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    assert!(engine.register(owner));
    assert!(engine.register(other));
    let task = engine
        .store
        .create_task(owner, "w", "execution.start", json!({}), false)
        .await
        .unwrap();
    let id = engine
        .store
        .start_result(task.id, vec![], "rows")
        .await
        .unwrap();
    engine
        .store
        .append_rows(id, vec![vec![model::Value::Text("a".repeat(200))]; 45])
        .await
        .unwrap();
    engine.store.finish_result(id, json!({})).await.unwrap();
    let params = json!({"result_id":id,"start":0,"max_rows":20,"max_bytes":1024});
    assert_eq!(
        engine
            .dispatch(other, "result.read", params.clone())
            .await
            .unwrap_err()
            .code,
        "permission_denied"
    );
    let mut start = 0;
    loop {
        let page = engine
            .dispatch(
                owner,
                "result.read",
                json!({"result_id":id,"start":start,"max_rows":20,"max_bytes":1024}),
            )
            .await
            .unwrap();
        let count = page["rows"].as_array().unwrap().len() as u64;
        assert!(count > 0);
        start += count;
        if page["next"].is_null() {
            break;
        }
        assert_eq!(page["next"].as_u64(), Some(start));
    }
    assert_eq!(start, 45);
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
async fn release_invalidates_result_and_is_not_available_to_other_clients() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    assert!(engine.register(owner));
    assert!(engine.register(other));
    let task = engine
        .store
        .create_task(owner, "w", "execution.start", json!({}), false)
        .await
        .unwrap();
    let id = engine
        .store
        .start_result(task.id, vec![], "rows")
        .await
        .unwrap();
    engine
        .store
        .append_rows(id, vec![vec![model::Value::Text("value".into())]])
        .await
        .unwrap();
    engine.store.finish_result(id, json!({})).await.unwrap();
    let params = json!({"result_id":id});
    assert_eq!(
        engine
            .dispatch(other, "result.release", params.clone())
            .await
            .unwrap_err()
            .code,
        "permission_denied"
    );
    assert_eq!(
        engine
            .dispatch(owner, "result.release", params.clone())
            .await
            .unwrap(),
        json!({"released":true})
    );
    assert_eq!(
        engine
            .dispatch(owner, "result.read", params)
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
