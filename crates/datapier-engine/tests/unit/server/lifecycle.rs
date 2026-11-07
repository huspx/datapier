use super::*;

#[tokio::test]
async fn invalid_handshake_does_not_extend_idle_lifetime() {
    let (engine, root) = engine().await;
    let idle = engine.state.lock().unwrap().idle;
    let owner = Uuid::new_v4();
    let (mut peer, stream) = tokio::io::duplex(4096);
    let handler = tokio::spawn(client(engine.clone(), owner, Box::new(stream)));
    protocol::write(
        &mut peer,
        &json!({"id":"1","method":"session.open","params":{"protocol_version":1}}),
    )
    .await
    .unwrap();
    let reply = protocol::read(&mut peer).await.unwrap();
    assert_eq!(reply["error"]["code"], "protocol_incompatible");
    handler.await.unwrap();
    {
        let state = engine.state.lock().unwrap();
        assert!(state.clients.is_empty());
        assert_eq!(state.idle, idle);
    }
    let (mut peer, stream) = tokio::io::duplex(4096);
    let handler = tokio::spawn(client(engine.clone(), owner, Box::new(stream)));
    protocol::write(
        &mut peer,
        &json!({"id":"2","method":"session.open","params":{"protocol_version":crate::protocol::VERSION,"client_kind":"tsp","workspace":"default","allow_connections":[]}}),
    )
    .await
    .unwrap();
    assert_eq!(
        protocol::read(&mut peer).await.unwrap()["result"]["protocol_version"],
        4
    );
    assert!(engine.state.lock().unwrap().clients.contains(&owner));
    protocol::write(
        &mut peer,
        &json!({"id":"3","method":"session.close","params":{}}),
    )
    .await
    .unwrap();
    assert_eq!(
        protocol::read(&mut peer).await.unwrap()["result"]["closed"],
        true
    );
    handler.await.unwrap();
    let state = engine.state.lock().unwrap();
    assert!(state.clients.is_empty());
    assert!(state.idle > idle);
    drop(state);
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn window_bridge_requires_binding_and_does_not_keep_engine_alive() {
    let (engine, root) = engine().await;
    let tsp = Uuid::new_v4();
    let bridge = Uuid::new_v4();
    let nonce = Uuid::new_v4().to_string();
    assert!(engine.register(tsp));
    assert!(engine.register_with(
        bridge,
        ClientContext {
            kind: model::ClientKind::WindowBridge,
            workspace: "default".into(),
            allow_connections: vec![],
        }
    ));
    let sync = json!({"nonce":nonce,"window_id":"","pane_id":"pane"});
    assert_eq!(
        engine
            .dispatch(bridge, "ui.window.sync", sync.clone())
            .await
            .unwrap()["bound"],
        false
    );
    engine
        .dispatch(tsp, "ui.window.bind", json!({"nonce":nonce}))
        .await
        .unwrap();
    assert_eq!(
        engine
            .dispatch(bridge, "ui.window.sync", sync.clone())
            .await
            .unwrap()["bound"],
        true
    );
    engine
        .dispatch(tsp, "ui.window.bind", json!({"nonce":nonce}))
        .await
        .unwrap();
    assert_eq!(
        engine
            .dispatch(
                bridge,
                "ui.window.sync",
                json!({"nonce":nonce,"window_id":"","pane_id":"other"})
            )
            .await
            .unwrap_err()
            .code,
        "permission_denied"
    );
    engine.disconnect(tsp).await;
    assert!(engine.state.lock().unwrap().clients.is_empty());
    assert_eq!(
        engine
            .dispatch(bridge, "ui.window.sync", sync)
            .await
            .unwrap()["bound"],
        false
    );
    engine.disconnect(bridge).await;
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn disconnect_cancels_reads_and_unsent_work_but_waits_for_sent_writes() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let mut entries = Vec::new();
    for phase in 0..3 {
        let task = engine
            .store
            .create_task(owner, "workspace", "execution.start", json!({}), false)
            .await
            .unwrap();
        if phase > 0 {
            engine.store.mark_sent(task.id, phase == 2).await.unwrap();
        }
        let cancel = CancellationToken::new();
        engine.state.lock().unwrap().tasks.insert(
            task.id,
            Running {
                owner,
                session_id: Uuid::new_v4(),
                cancel: cancel.clone(),
                transition: Arc::new(AsyncMutex::new(())),
                abort: None,
                started: None,
            },
        );
        entries.push(cancel);
    }
    engine.disconnect(owner).await;
    assert!(entries[0].is_cancelled(), "unsent work must be cancelled");
    assert!(
        entries[1].is_cancelled(),
        "sent read must be cancelled even on a writable connection"
    );
    assert!(
        !entries[2].is_cancelled(),
        "sent write must be allowed to reach a final outcome"
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn disconnect_and_write_dispatch_obey_the_same_gate() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let before = engine
        .store
        .create_task(owner, "workspace", "change.submit", json!({}), false)
        .await
        .unwrap();
    let cancel_before = CancellationToken::new();
    let gate_before = Arc::new(AsyncMutex::new(()));
    let sink_before = ExecutionSink::new(engine.store.clone(), before.id)
        .with_dispatch_control(gate_before.clone(), cancel_before.clone());
    engine
        .cancel_if_safe(before.id, cancel_before, gate_before)
        .await;
    assert_eq!(
        sink_before.mark_sent(true).await.unwrap_err().code,
        "cancelled"
    );
    assert!(!engine.store.task(before.id).await.unwrap().sent);

    let after = engine
        .store
        .create_task(owner, "workspace", "change.submit", json!({}), false)
        .await
        .unwrap();
    let cancel_after = CancellationToken::new();
    let gate_after = Arc::new(AsyncMutex::new(()));
    let sink_after = ExecutionSink::new(engine.store.clone(), after.id)
        .with_dispatch_control(gate_after.clone(), cancel_after.clone());
    sink_after.mark_sent(true).await.unwrap();
    engine
        .cancel_if_safe(after.id, cancel_after.clone(), gate_after)
        .await;
    assert!(!cancel_after.is_cancelled());
    assert!(engine.store.task(after.id).await.unwrap().may_write);
    drop(sink_before);
    drop(sink_after);
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn sqlite_full_does_not_leave_completed_tasks_running() {
    use sqlx::Executor;

    let (mut engine, root) = engine().await;
    engine.store.pool.close().await;
    Arc::get_mut(&mut engine).unwrap().store.pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(
            sqlx::sqlite::SqliteConnectOptions::new().filename(root.join("datapier.sqlite3")),
        )
        .await
        .unwrap();
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let mut events = engine.state.lock().unwrap().event_senders[&owner].subscribe();
    let mut cases = Vec::new();
    for may_write in [false, true] {
        let record = engine
            .store
            .create_task(
                owner,
                "full",
                "execution.start",
                json!({"text":"SELECT 1"}),
                false,
            )
            .await
            .unwrap();
        engine.store.mark_sent(record.id, may_write).await.unwrap();
        engine
            .store
            .update_task(
                record.id,
                "running",
                json!({"_task":{"started_at":123.5}}),
                None,
            )
            .await
            .unwrap();
        cases.push((
            record.id,
            if may_write {
                "outcome_unknown"
            } else {
                "failed"
            },
        ));
    }
    engine
        .store
        .pool
        .execute("PRAGMA max_page_count=1")
        .await
        .unwrap();
    let result = Ok(
        json!({"statements":[{"index":0,"text":format!("SELECT '{}'", "x".repeat(131072)),"status":"succeeded","transaction_state":"autocommit"}]}),
    );
    for (id, expected) in &cases {
        engine
            .finish_task(
                owner,
                *id,
                &result,
                Some(Instant::now() - Duration::from_millis(20)),
            )
            .await;
        let event = events.recv().await.unwrap();
        assert_eq!(event.task_id, *id);
        assert_eq!(event.payload, json!({"state": expected}));
        let task = engine
            .dispatch(owner, "execution.get", json!({"task_id":id}))
            .await
            .unwrap();
        assert_eq!(task["state"], *expected, "持久化失败也必须向客户端报告终态");
        assert_eq!(task["started_at"], 123.5);
        assert!(task["elapsed_ms"].as_u64().unwrap() >= 20);
        assert_eq!(task["error"]["code"], "storage");
        assert_eq!(
            task["error"]["details"]["persistence_error"]["database_code"],
            "13"
        );
        assert_eq!(task["error"]["details"]["observed_state"], "succeeded");
        let cancelled = engine
            .dispatch(owner, "execution.cancel", json!({"task_id":id}))
            .await
            .unwrap();
        assert_eq!(cancelled["state"], *expected);
    }
    let listed = engine
        .dispatch(owner, "execution.list", json!({"workspace_id":"full"}))
        .await
        .unwrap();
    let history = engine
        .dispatch(owner, "history.list", json!({"workspace_id":"full"}))
        .await
        .unwrap();
    let second_page = engine
        .dispatch(
            owner,
            "history.list",
            json!({"workspace_id":"full","offset":1,"limit":1,"search":"SELECT"}),
        )
        .await
        .unwrap();
    assert_eq!(second_page["total"], 2);
    assert_eq!(second_page["items"].as_array().unwrap().len(), 1);
    let page_entry = &second_page["items"][0];
    let expected_page_state = cases
        .iter()
        .find(|(id, _)| page_entry["id"] == id.to_string())
        .unwrap()
        .1;
    assert_eq!(page_entry["state"], expected_page_state);
    let failed = engine
        .dispatch(
            owner,
            "history.list",
            json!({"workspace_id":"full","search":"failed"}),
        )
        .await
        .unwrap();
    assert_eq!(failed["total"], 1);
    assert_eq!(failed["items"][0]["id"], cases[0].0.to_string());
    assert_eq!(failed["items"][0]["state"], "failed");
    let running = engine
        .dispatch(
            owner,
            "history.list",
            json!({"workspace_id":"full","search":"running"}),
        )
        .await
        .unwrap();
    assert_eq!(running["total"], 0);
    assert!(running["items"].as_array().unwrap().is_empty());
    let localized_page = engine
        .dispatch(
            owner,
            "history.list",
            json!({"workspace_id":"full","search":"终态","search_states":["failed","outcome_unknown"],"offset":1,"limit":1}),
        )
        .await
        .unwrap();
    assert_eq!(localized_page["total"], 2);
    assert_eq!(localized_page["items"].as_array().unwrap().len(), 1);
    assert_eq!(localized_page["items"][0]["id"], page_entry["id"]);
    assert_eq!(localized_page["items"][0]["state"], expected_page_state);
    for (id, expected) in cases {
        for entries in [&listed["executions"], &history["items"]] {
            let record = entries
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["id"] == id.to_string())
                .unwrap();
            assert_eq!(record["state"], expected);
        }
    }
    engine.store.pool.close().await;
    drop(engine);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn history_pages_cover_all_matching_executions() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    for index in 0..501 {
        engine
            .store
            .create_task(
                owner,
                "history-pages",
                "execution.start",
                json!({"text":format!("SELECT {index} needle")}),
                false,
            )
            .await
            .unwrap();
    }
    let page = engine
        .dispatch(
            owner,
            "history.list",
            json!({"workspace_id":"history-pages","offset":500,"limit":100,"search":"needle"}),
        )
        .await
        .unwrap();
    assert_eq!(page["total"], 501);
    assert_eq!(page["offset"], 500);
    assert_eq!(page["limit"], 100);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["text"], "SELECT 0 needle");
    let filtered = engine
        .dispatch(
            owner,
            "history.list",
            json!({"workspace_id":"history-pages","search":"SELECT 500 needle"}),
        )
        .await
        .unwrap();
    assert_eq!(filtered["total"], 1);
    assert_eq!(filtered["offset"], 0);
    assert_eq!(filtered["limit"], 20);
    assert_eq!(filtered["items"][0]["text"], "SELECT 500 needle");
    engine.store.pool.close().await;
    drop(engine);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn state_events_are_isolated_ordered_and_stop_on_disconnect() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    assert!(engine.register(owner));
    assert!(engine.register(other));
    let mut events = engine.state.lock().unwrap().event_senders[&owner].subscribe();
    let mut other_events = engine.state.lock().unwrap().event_senders[&other].subscribe();
    let record = engine
        .store
        .create_task(
            owner,
            "workspace",
            "execution.start",
            json!({"text":"secret SQL"}),
            false,
        )
        .await
        .unwrap();
    engine.state.lock().unwrap().tasks.insert(
        record.id,
        Running {
            owner,
            session_id: Uuid::new_v4(),
            cancel: CancellationToken::new(),
            transition: Arc::new(AsyncMutex::new(())),
            abort: None,
            started: None,
        },
    );
    engine.emit_state(owner, record.id, "queued");
    engine
        .dispatch(owner, "execution.cancel", json!({"task_id": record.id}))
        .await
        .unwrap();
    engine
        .finish_task(owner, record.id, &Err(AppError::cancelled()), None)
        .await;
    let mut last = 0;
    for expected in ["queued", "cancelling", "cancelled"] {
        let event = events.recv().await.unwrap();
        assert!(event.seq > last);
        last = event.seq;
        assert_eq!(event.kind, "execution.state");
        assert_eq!(event.task_id, record.id);
        assert_eq!(event.payload, json!({"state":expected}));
    }
    assert!(matches!(
        other_events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    engine.disconnect(owner).await;
    assert!(matches!(
        events.recv().await,
        Err(broadcast::error::RecvError::Closed)
    ));
    engine.emit_state(owner, record.id, "failed");
    assert!(matches!(
        other_events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    engine.disconnect(other).await;
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn workspace_activation_requires_saved_target_and_matching_context() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    assert_eq!(
        engine
            .dispatch(
                owner,
                "workspace.activate",
                json!({"workspace_id":"copy","expected_workspace_id":"default"})
            )
            .await
            .unwrap_err()
            .code,
        "not_found"
    );
    engine
        .store
        .workspace_save("copy", 0, json!({}))
        .await
        .unwrap();
    assert_eq!(
        engine
            .dispatch(
                owner,
                "workspace.activate",
                json!({"workspace_id":"copy","expected_workspace_id":"other"})
            )
            .await
            .unwrap_err()
            .code,
        "stale_revision"
    );
    assert_eq!(
        engine
            .dispatch(
                owner,
                "workspace.activate",
                json!({"workspace_id":"copy","expected_workspace_id":"default"})
            )
            .await
            .unwrap()["workspace_id"],
        "copy"
    );
    assert_eq!(
        engine
            .state
            .lock()
            .unwrap()
            .contexts
            .get(&owner)
            .unwrap()
            .workspace,
        "copy"
    );
    engine.disconnect(owner).await;
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
