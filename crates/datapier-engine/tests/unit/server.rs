use super::*;
use crate::protocol;

async fn engine() -> (Arc<Engine>, PathBuf) {
    let root = std::env::temp_dir().join(format!("datapier-engine-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let store = Store::open(&root).await.unwrap();
    let state = State {
        accepting: true,
        clients: HashSet::new(),
        client_cancels: HashMap::new(),
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
                    started: None,
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

#[tokio::test]
async fn disconnect_cancels_profile_connection_waits_and_releases_profile_lock() {
    let (engine, root) = engine().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "stalled".into(),
        provider: model::ProviderKind::Mysql,
        host: "127.0.0.1".into(),
        port: listener.local_addr().unwrap().port(),
        username: "test".into(),
        database: Some("test".into()),
        redis_db: 0,
        access_mode: AccessMode::ReadWrite,
        tls: model::TlsConfig::default(),
        visible_databases: None,
    };
    engine
        .store
        .save_profile(&profile, None, false)
        .await
        .unwrap();
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    assert!(engine.register(first));
    assert!(engine.register(second));
    let request = json!({"profile_id":profile.id,"secret":"test"});
    let first_request = {
        let engine = engine.clone();
        let request = request.clone();
        tokio::spawn(async move { engine.dispatch(first, "connection.open", request).await })
    };
    let (first_socket, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let second_request = {
        let engine = engine.clone();
        tokio::spawn(async move { engine.dispatch(second, "connection.open", request).await })
    };
    engine.disconnect(first).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), first_request)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .code,
        "cancelled"
    );
    let (second_socket, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
        .await
        .unwrap()
        .unwrap();
    engine.disconnect(second).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), second_request)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .code,
        "cancelled"
    );
    drop((first_socket, second_socket));
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn disconnect_cancels_detached_profile_reads() {
    let (engine, root) = engine().await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "stalled".into(),
        provider: model::ProviderKind::Mysql,
        host: "127.0.0.1".into(),
        port: listener.local_addr().unwrap().port(),
        username: "test".into(),
        database: Some("test".into()),
        redis_db: 0,
        access_mode: AccessMode::ReadWrite,
        tls: model::TlsConfig::default(),
        visible_databases: None,
    };
    for method in ["profile.test", "profile.databases"] {
        let owner = Uuid::new_v4();
        assert!(engine.register(owner));
        let task = {
            let engine = engine.clone();
            let params = json!({"profile":profile,"secret":"test"});
            tokio::spawn(async move { engine.dispatch(owner, method, params).await })
        };
        let (socket, _) = tokio::time::timeout(Duration::from_secs(3), listener.accept())
            .await
            .unwrap()
            .unwrap();
        engine.disconnect(owner).await;
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err()
                .code,
            "cancelled"
        );
        drop(socket);
    }
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 实例"]
async fn disconnect_finishes_running_profile_test_record() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (engine, root) = engine().await;
    let upstream: u16 = std::env::var("DATAPIER_TEST_MYSQL_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "fixture".into(),
        provider: model::ProviderKind::Mysql,
        host: "127.0.0.1".into(),
        port: listener.local_addr().unwrap().port(),
        username: "datapier".into(),
        database: Some("datapier_fixture".into()),
        redis_db: 0,
        access_mode: AccessMode::ReadWrite,
        tls: model::TlsConfig::default(),
        visible_databases: None,
    };
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let proxy = tokio::spawn(async move {
        let (client, _) = listener.accept().await.unwrap();
        let server = tokio::net::TcpStream::connect(("127.0.0.1", upstream))
            .await
            .unwrap();
        let (mut client_read, mut client_write) = client.into_split();
        let (mut server_read, mut server_write) = server.into_split();
        let outgoing = async move {
            let mut seen_tx = Some(seen_tx);
            let mut buf = [0u8; 8192];
            loop {
                let n = client_read.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                if buf[..n]
                    .windows(b"SELECT VERSION() AS version".len())
                    .any(|part| part == b"SELECT VERSION() AS version")
                {
                    let _ = seen_tx.take().unwrap().send(());
                    std::future::pending::<()>().await;
                }
                server_write.write_all(&buf[..n]).await.unwrap();
            }
        };
        tokio::select! {
            _ = outgoing => (),
            _ = tokio::io::copy(&mut server_read, &mut client_write) => (),
        }
    });
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let task = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .dispatch(
                    owner,
                    "profile.test",
                    json!({"profile":profile,"secret":secret,"workspace_id":"profile-cancel"}),
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(5), seen_rx)
        .await
        .unwrap()
        .unwrap();
    engine.disconnect(owner).await;
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .code,
        "cancelled"
    );
    let records = engine.store.tasks("profile-cancel").await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, "cancelled");
    proxy.abort();
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancelled_profile_save_keeps_open_snapshot_consistent() {
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
    let mut profile = Profile {
        id: Uuid::new_v4(),
        name: "old".into(),
        provider: model::ProviderKind::Mysql,
        host: "127.0.0.1".into(),
        port: 3306,
        username: "test".into(),
        database: None,
        redis_db: 0,
        access_mode: AccessMode::ReadWrite,
        tls: model::TlsConfig::default(),
        visible_databases: None,
    };
    engine
        .store
        .save_profile(&profile, Some("old-secret".into()), false)
        .await
        .unwrap();
    assert_eq!(engine.store.profiles().await.unwrap()[0].name, "old");
    assert_eq!(
        engine.store.secret(profile.id).await.unwrap().as_deref(),
        Some("old-secret")
    );

    // 占住唯一 SQLite 连接，使保存进入临界区后无法完成，避免依赖调度时序。
    let connection = engine.store.pool.acquire().await.unwrap();
    profile.name = "new".into();
    let lock = engine.profile_lock(profile.id);
    let request = {
        let engine = engine.clone();
        let profile = profile.clone();
        tokio::spawn(async move {
            engine
                .dispatch(
                    owner,
                    "profile.save",
                    json!({
                        "profile":profile, "secret":"new-secret", "save_secret":false
                    }),
                )
                .await
        })
    };
    tokio::time::timeout(Duration::from_secs(3), async {
        while lock.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert!(lock.try_lock().is_err(), "取消等待不能释放仍在保存的配置锁");

    // 模拟 connection.open 在同一锁内依次读取配置和凭据。
    let reader = {
        let engine = engine.clone();
        let lock = lock.clone();
        tokio::spawn(async move {
            let _guard = lock.lock().await;
            let profile = engine.store.profiles().await.unwrap().pop().unwrap();
            let secret = engine.store.secret(profile.id).await.unwrap();
            (profile.name, secret)
        })
    };
    drop(connection);
    let (name, secret) = tokio::time::timeout(Duration::from_secs(3), reader)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        (name.as_str(), secret.as_deref()),
        ("new", Some("new-secret"))
    );
    engine.store.pool.close().await;
    engine.store.cache.close().await;
    drop(engine);
    std::fs::remove_dir_all(root).unwrap();
}
