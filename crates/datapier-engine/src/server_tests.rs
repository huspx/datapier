use super::*;

async fn engine() -> (Arc<Engine>, PathBuf) {
    let root = std::env::temp_dir().join(format!("datapier-engine-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let store = Store::open(&root).await.unwrap();
    let state = State {
        accepting: true,
        clients: HashSet::new(),
        event_senders: HashMap::new(),
        event_seq: 0,
        idle: Instant::now() - Duration::from_secs(10),
        databases: HashMap::new(),
        tasks: HashMap::new(),
        unpersisted: HashMap::new(),
        profile_locks: HashMap::new(),
    };
    (
        Arc::new(Engine {
            id: Uuid::new_v4(),
            store,
            state: Mutex::new(state),
        }),
        root,
    )
}

#[tokio::test]
async fn invalid_handshake_does_not_extend_idle_lifetime() {
    let (engine, root) = engine().await;
    let idle = engine.state.lock().unwrap().idle;
    let owner = Uuid::new_v4();
    let (mut peer, stream) = tokio::io::duplex(4096);
    let handler = tokio::spawn(client(engine.clone(), owner, Box::new(stream)));
    protocol::write(
        &mut peer,
        &json!({"id":"1","method":"session.open","params":{"protocol_version":2}}),
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
        &json!({"id":"2","method":"session.open","params":{"protocol_version":1}}),
    )
    .await
    .unwrap();
    assert_eq!(
        protocol::read(&mut peer).await.unwrap()["result"]["protocol_version"],
        1
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
            .update_task(record.id, "running", Value::Null, None)
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
        engine.finish_task(owner, *id, &result).await;
        let event = events.recv().await.unwrap();
        assert_eq!(event.task_id, *id);
        assert_eq!(event.payload, json!({"state": expected}));
        let task = engine
            .dispatch(owner, "execution.get", json!({"task_id":id}))
            .await
            .unwrap();
        assert_eq!(task["state"], *expected, "持久化失败也必须向客户端报告终态");
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
    engine.store.cache.close().await;
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
        },
    );
    engine.emit_state(owner, record.id, "queued");
    engine
        .dispatch(owner, "execution.cancel", json!({"task_id": record.id}))
        .await
        .unwrap();
    engine
        .finish_task(owner, record.id, &Err(AppError::cancelled()))
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
async fn duplex_sessions_forward_only_owner_events_and_close() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    let (first, first_server) = tokio::io::duplex(4096);
    let (second, second_server) = tokio::io::duplex(4096);
    let first_handler = tokio::spawn(client(engine.clone(), owner, Box::new(first_server)));
    let second_handler = tokio::spawn(client(engine.clone(), other, Box::new(second_server)));
    let first = crate::Client::handshake(Box::new(first)).await.unwrap();
    let second = crate::Client::handshake(Box::new(second)).await.unwrap();
    let mut first_events = first.subscribe_events();
    let mut second_events = second.subscribe_events();
    let first_task = Uuid::new_v4();
    let second_task = Uuid::new_v4();
    engine.emit_state(owner, first_task, "running");
    engine.emit_state(other, second_task, "queued");
    let (reply, first_event, second_event) = tokio::join!(
        first.request("session.heartbeat", json!({})),
        first_events.recv(),
        second_events.recv()
    );
    assert_eq!(reply.unwrap()["ok"], true);
    let first_event = first_event.unwrap();
    let second_event = second_event.unwrap();
    assert_eq!(first_event.task_id, first_task);
    assert_eq!(second_event.task_id, second_task);
    assert!(first_event.seq < second_event.seq);
    first.close().await.unwrap();
    second.close().await.unwrap();
    first_handler.await.unwrap();
    second_handler.await.unwrap();
    assert!(matches!(
        first_events.recv().await,
        Err(broadcast::error::RecvError::Closed)
    ));
    assert!(matches!(
        second_events.recv().await,
        Err(broadcast::error::RecvError::Closed)
    ));
    drop(first);
    drop(second);
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn saturated_duplex_releases_lease_and_preserves_dispatch_gate() {
    let (engine, root) = engine().await;
    let other = Uuid::new_v4();
    let (other_peer, other_stream) = tokio::io::duplex(4096);
    let other_handler = tokio::spawn(client(engine.clone(), other, Box::new(other_stream)));
    let other_client = crate::Client::handshake(Box::new(other_peer))
        .await
        .unwrap();
    for method in [Some("session.heartbeat"), None, Some("session.close")] {
        let owner = Uuid::new_v4();
        let (mut peer, stream) = tokio::io::duplex(64);
        let handler = tokio::spawn(client_with_lease(
            engine.clone(),
            owner,
            Box::new(stream),
            Duration::from_millis(500),
            1,
        ));
        protocol::write(
            &mut peer,
            &json!({"id":"open","method":"session.open","params":{"protocol_version":1}}),
        )
        .await
        .unwrap();
        assert_eq!(
            protocol::read(&mut peer).await.unwrap()["result"]["protocol_version"],
            1
        );
        let mut tokens = Vec::new();
        for may_write in [false, true] {
            let task = engine
                .store
                .create_task(owner, "lease", "execution.start", json!({}), false)
                .await
                .unwrap();
            if may_write {
                engine.store.mark_sent(task.id, true).await.unwrap();
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
                },
            );
            tokens.push(cancel);
        }
        for _ in 0..32 {
            engine.emit_state(owner, Uuid::new_v4(), "running");
            tokio::task::yield_now().await;
        }
        let request = match method {
            Some(method) => json!({"id":"x","method":method}),
            None => json!({"id":"x"}),
        };
        protocol::write(&mut peer, &request).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), handler)
            .await
            .unwrap()
            .unwrap();
        assert!(!engine.state.lock().unwrap().clients.contains(&owner));
        assert!(tokens[0].is_cancelled());
        assert!(!tokens[1].is_cancelled());
        assert_eq!(
            other_client
                .request("session.heartbeat", json!({}))
                .await
                .unwrap()["ok"],
            true
        );
        drop(peer);
    }
    other_client.close().await.unwrap();
    other_handler.await.unwrap();
    drop(other_client);
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
