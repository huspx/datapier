use super::*;

#[tokio::test]
async fn idle_deadline_does_not_wait_for_running_write() {
    let (engine, root) = engine().await;
    let id = Uuid::new_v4();
    {
        let mut state = engine.state.lock().unwrap();
        state.idle = Instant::now() - Duration::from_secs(31);
        state.tasks.insert(
            id,
            Running {
                owner: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                cancel: CancellationToken::new(),
                transition: Arc::new(AsyncMutex::new(())),
                started: Some(Instant::now()),
                abort: None,
            },
        );
        assert!(state.stop_if_idle());
        assert!(!state.accepting);
    }
    crate::server::tests::cleanup(engine, root).await;
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
        crate::protocol::VERSION
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
    {
        let state = engine.state.lock().unwrap();
        assert!(state.clients.is_empty());
        assert!(state.idle > idle);
    }
    crate::server::tests::cleanup(engine, root).await;
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
    crate::server::tests::cleanup(engine, root).await;
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
    crate::server::tests::cleanup(engine, root).await;
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
    crate::server::tests::cleanup(engine, root).await;
}

#[tokio::test]
async fn transfer_send_rechecks_owner_during_disconnect_cleanup() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let task = engine
        .store
        .create_task(owner, "workspace", "transfer.submit", json!({}), false)
        .await
        .unwrap();
    engine.store.mark_sent(task.id, true).await.unwrap();
    let sink = engine.transfer_sink(
        owner,
        task.id,
        Arc::new(AsyncMutex::new(())),
        CancellationToken::new(),
        vec![],
    );
    let blocker = engine
        .store
        .create_task(owner, "workspace", "execution.start", json!({}), false)
        .await
        .unwrap();
    let transition = Arc::new(AsyncMutex::new(()));
    let guard = transition.lock().await;
    engine.state.lock().unwrap().tasks.insert(
        blocker.id,
        Running {
            owner,
            session_id: Uuid::nil(),
            cancel: CancellationToken::new(),
            transition: transition.clone(),
            abort: None,
            started: None,
        },
    );
    let closing = tokio::spawn({
        let engine = engine.clone();
        async move { engine.disconnect(owner).await }
    });
    while engine.state.lock().unwrap().clients.contains(&owner) {
        tokio::task::yield_now().await;
    }
    assert!(!closing.is_finished());
    assert_eq!(sink.mark_sent(true).await.unwrap_err().code, "disconnected");
    drop(guard);
    closing.await.unwrap();
    drop(sink);
    crate::server::tests::cleanup(engine, root).await;
}

#[tokio::test]
async fn resource_list_response_is_not_persisted_in_summary() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let task = engine
        .store
        .create_task(owner, "workspace", "resource.list", json!({}), false)
        .await
        .unwrap();
    let response = json!({"resources":[{"key":"private-key","key_b64":"cHJpdmF0ZS1rZXk=","ttl":37}],"complete":true});
    engine
        .finish_task(owner, task.id, &Ok(response.clone()), None)
        .await;
    let stored = engine.store.task(task.id).await.unwrap();
    assert_eq!(stored.state, "succeeded");
    let text = stored.summary.to_string();
    assert!(
        !text.contains("private-key")
            && !text.contains("cHJpdmF0ZS1rZXk=")
            && !text.contains("ttl")
    );
    assert_eq!(response["resources"][0]["ttl"], 37);
    crate::server::tests::cleanup(engine, root).await;
}

#[tokio::test]
async fn transfer_read_reports_unpersisted_completion_and_recovers_sent_step() {
    use datapier_core::{
        model::transfer::{TransferLocator, TransferStepOutcome, TransferStepState},
        storage::transfer::TransferRunContext,
    };
    use sqlx::Executor;

    let (mut engine, root) = engine().await;
    engine.store.pool.close().await;
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(root.join("datapier.sqlite3"));
    let connection = sqlx::ConnectOptions::connect(&options).await.unwrap();
    Arc::get_mut(&mut engine).unwrap().store.pool =
        datapier_core::storage::FixedPool::new(vec![connection]);
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let task = engine
        .store
        .create_task(owner, "full", "transfer.submit", json!({}), false)
        .await
        .unwrap();
    engine
        .store
        .transfer_create_run(
            task.id,
            task.id,
            "full",
            &"a".repeat(64),
            &TransferRunContext::default(),
            None,
        )
        .await
        .unwrap();
    engine
        .store
        .update_task(task.id, "running", json!({}), None)
        .await
        .unwrap();
    let step = TransferStepOutcome {
        ordinal: 0,
        locator: TransferLocator {
            first_record: Some(1),
            last_record: Some(500),
            ..Default::default()
        },
        phase: "data".into(),
        state: TransferStepState::NotSent,
        digest: "b".repeat(64),
        affected: 0,
        error_code: None,
        database_code: None,
        item_states: Vec::new(),
        error_record: None,
    };
    engine
        .store
        .transfer_put_step(task.id, &step)
        .await
        .unwrap();
    engine
        .store
        .transfer_mark_sent(task.id, task.id, 0, true)
        .await
        .unwrap();
    (&mut *engine.store.pool.acquire().await.unwrap())
        .execute("PRAGMA max_page_count=1")
        .await
        .unwrap();
    let result = Err(AppError::new("database", "写入失败").with_details(json!({
        "state":"outcome_unknown", "progress":{"padding":"x".repeat(131072)}
    })));
    engine.finish_task(owner, task.id, &result, None).await;

    let read = engine
        .dispatch(owner, "transfer.run.get", json!({"run_id":task.id}))
        .await
        .unwrap();
    assert_eq!(read["state"], "outcome_unknown");
    assert_eq!(read["error"]["code"], "storage");
    assert_eq!(
        read["error"]["details"]["persistence_error"]["database_code"],
        "13"
    );
    assert_eq!(read["progress_is_persisted_snapshot"], true);
    assert_eq!(
        engine.store.transfer_run(task.id).await.unwrap().state,
        "running"
    );

    (&mut *engine.store.pool.acquire().await.unwrap())
        .execute("PRAGMA max_page_count=1073741823")
        .await
        .unwrap();
    let read_after_space = engine
        .dispatch(owner, "transfer.run.get", json!({"run_id":task.id}))
        .await
        .unwrap();
    assert_eq!(read_after_space["state"], "outcome_unknown");
    assert_eq!(read_after_space["error"]["code"], "storage");
    for format in ["json", "csv"] {
        let path = root.join(format!("full-report.{format}"));
        engine
            .dispatch(
                owner,
                "transfer.report.export",
                json!({"run_id":task.id,"path":path,"format":format}),
            )
            .await
            .unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        let report_run: Value = if format == "json" {
            serde_json::from_str::<Value>(&contents).unwrap()["run"].clone()
        } else {
            let row = contents.lines().nth(1).unwrap();
            let payload = row
                .strip_prefix("run,\"")
                .unwrap()
                .strip_suffix('"')
                .unwrap();
            serde_json::from_str(&payload.replace("\"\"", "\"")).unwrap()
        };
        assert_eq!(report_run["state"], "outcome_unknown");
        assert_eq!(report_run["error"]["code"], "storage");
        assert_eq!(
            report_run["error"]["details"]["persistence_error"]["database_code"],
            "13"
        );
        assert_eq!(report_run["progress_is_persisted_snapshot"], true);
    }
    let retry = engine
        .dispatch(owner, "transfer.retry.prepare", json!({"run_id":task.id}))
        .await
        .unwrap_err();
    assert_eq!(retry.code, "invalid_input"); // 已越过 busy，仍必须提供有效 spec。
    let unknown_retry = engine
        .dispatch(
            owner,
            "transfer.retry.prepare",
            json!({"run_id":task.id,"states":["outcome_unknown"]}),
        )
        .await
        .unwrap_err();
    assert_eq!(unknown_retry.code, "invalid_input");
    let sent_only = engine
        .dispatch(
            owner,
            "transfer.retry.prepare",
            json!({"run_id":task.id,"states":["not_sent"],"spec":{
                "mode":"copy","provider":"mysql",
                "source":{"profile_id":Uuid::new_v4()},
                "destination":{"profile_id":Uuid::new_v4()}
            }}),
        )
        .await
        .unwrap_err();
    assert_eq!(sent_only.code, "invalid_input");
    assert_eq!(sent_only.message, "原任务没有选定的可续作范围");
    let other = Uuid::new_v4();
    assert!(engine.register(other));
    let denied = engine
        .dispatch(other, "transfer.retry.prepare", json!({"run_id":task.id}))
        .await
        .unwrap_err();
    assert_eq!(denied.code, "forbidden");
    // 重启丢弃内存副本后只依据已落盘的发送阶段恢复，不能声称回滚。
    engine.store.recover_tasks().await.unwrap();
    engine.store.transfer_recover().await.unwrap();
    engine.state.lock().unwrap().unpersisted.clear();
    let recovered = engine
        .dispatch(owner, "transfer.run.get", json!({"run_id":task.id}))
        .await
        .unwrap();
    assert_eq!(recovered["state"], "outcome_unknown");
    assert_eq!(recovered["progress"]["outcome_unknown"], 500);
    assert_eq!(
        engine
            .store
            .transfer_items(task.id, None, 10)
            .await
            .unwrap()[0]
            .state,
        TransferStepState::OutcomeUnknown
    );
    crate::server::tests::cleanup(engine, root).await;
}

#[tokio::test]
async fn sqlite_full_does_not_leave_completed_tasks_running() {
    use sqlx::Executor;

    let (mut engine, root) = engine().await;
    engine.store.pool.close().await;
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(root.join("datapier.sqlite3"));
    let connection = sqlx::ConnectOptions::connect(&options).await.unwrap();
    Arc::get_mut(&mut engine).unwrap().store.pool =
        datapier_core::storage::FixedPool::new(vec![connection]);
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
    (&mut *engine.store.pool.acquire().await.unwrap())
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
        let details = engine.task_details(*id).await.unwrap();
        assert_eq!(details.state, *expected);
        assert_eq!(details.text.as_deref(), Some("SELECT 1"));
        assert_eq!(details.error.unwrap().code, "storage");
        assert!(details.elapsed_ms.unwrap() >= 20);
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
    crate::server::tests::cleanup(engine, root).await;
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
    crate::server::tests::cleanup(engine, root).await;
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
    crate::server::tests::cleanup(engine, root).await;
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
    crate::server::tests::cleanup(engine, root).await;
}
