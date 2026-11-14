use super::*;

async fn redis_test_session(engine: &Arc<Engine>, owner: Uuid, mode: AccessMode) -> Uuid {
    let port = std::env::var("DATAPIER_TEST_REDIS_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "redis-test".into(),
        revision: 1,
        approval_policy: Default::default(),
        provider: model::ProviderKind::Redis,
        access_mode: mode,
        connection: model::ConnectionConfig::Direct(model::DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: String::new(),
            database: None,
            redis_db: 0,
            redis_databases_override: None,
            tls: model::TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    engine
        .store
        .save_profile(&profile, None, false)
        .await
        .unwrap();
    let session = ProviderSession::connect(profile.clone(), None)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    engine.state.lock().unwrap().databases.insert(
        id,
        Arc::new(Database {
            owner,
            profile: profile.clone(),
            database: None,
            redis_db: Some(0),
            capabilities: Mutex::new(profile.capabilities()),
            session: AsyncMutex::new(session),
            closing: AtomicBool::new(false),
        }),
    );
    id
}

async fn redis_test_command(
    engine: &Arc<Engine>,
    owner: Uuid,
    session: Uuid,
    text: &str,
) -> model::TaskRecord {
    let started = engine
        .dispatch(
            owner,
            "execution.start",
            json!({"session_id":session,"workspace_id":"redis-test","text":text}),
        )
        .await
        .unwrap();
    let id = serde_json::from_value(started["task_id"].clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let task = engine.store.task(id).await.unwrap();
            if !matches!(task.state.as_str(), "queued" | "running" | "cancelling") {
                break task;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis instance"]
async fn stale_profile_rejects_new_requests_and_close_releases_only_its_results() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    engine.register(owner);
    engine.register(other);
    let session = redis_test_session(&engine, owner, AccessMode::ReadWrite).await;
    let other_session = redis_test_session(&engine, other, AccessMode::ReadWrite).await;
    let first = redis_test_command(&engine, owner, session, "PING").await;
    let second = redis_test_command(&engine, other, other_session, "PING").await;
    let result = first.results[0];
    let other_result = second.results[0];
    let profile_id = engine.state.lock().unwrap().databases[&session].profile.id;
    let dispatched = engine
        .store
        .create_task(owner, "redis-test", "execution.start", json!({}), false)
        .await
        .unwrap();
    let sink = ExecutionSink::new(engine.store.clone(), dispatched.id).with_profile_revision(
        profile_id,
        1,
        engine.profile_lock(profile_id),
    );
    sink.mark_sent(true).await.unwrap();
    let mut profile = engine.store.profile(profile_id).await.unwrap();
    profile.access_mode = AccessMode::ReadOnly;
    engine
        .store
        .save_profile(&profile, None, false)
        .await
        .unwrap();
    // 已发送的同一请求继续收尾，不再被新版本配置中断。
    sink.mark_sent(true).await.unwrap();
    let rejected = redis_test_command(&engine, owner, session, "SET datapier-stale-test x").await;
    assert_eq!(rejected.state, "failed");
    assert!(!rejected.sent);
    assert_eq!(rejected.error.unwrap().code, "invalidated");
    engine
        .dispatch(owner, "connection.close", json!({"session_id":session}))
        .await
        .unwrap();
    assert!(engine.store.result_meta(result).await.is_err());
    assert!(engine.store.result_meta(other_result).await.is_ok());
    engine
        .dispatch(
            other,
            "connection.close",
            json!({"session_id":other_session}),
        )
        .await
        .unwrap();
    engine.store.pool.close().await;
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis instance"]
async fn mcp_rollback_uses_current_authorized_profile_revision() {
    let (engine, root) = engine().await;
    let controller = Uuid::new_v4();
    let owner = Uuid::new_v4();
    engine.register(controller);
    engine.register_with(
        owner,
        ClientContext {
            kind: model::ClientKind::Mcp,
            workspace: "default".into(),
            allow_connections: vec![],
        },
    );
    let initial_session = redis_test_session(&engine, controller, AccessMode::ReadWrite).await;
    let profile_id = engine.state.lock().unwrap().databases[&initial_session]
        .profile
        .id;
    let opened = engine
        .dispatch(
            owner,
            "mcp.open_session",
            json!({"connection_id":profile_id}),
        )
        .await
        .unwrap();
    let session = opened["session_id"].clone();
    let begin = engine
        .dispatch(
            owner,
            "mcp.begin_transaction",
            json!({"session_id":session}),
        )
        .await
        .unwrap();
    let begin_id: Uuid = serde_json::from_value(begin["execution_id"].clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let task = engine.store.task(begin_id).await.unwrap();
            if !matches!(task.state.as_str(), "queued" | "running" | "cancelling") {
                assert_eq!(task.state, "succeeded");
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    engine
        .dispatch(
            controller,
            "profile.rules.set",
            json!({"profile_id":profile_id,"expected_revision":1,"rules":[]}),
        )
        .await
        .unwrap();
    let rollback = engine
        .dispatch(
            owner,
            "mcp.rollback_transaction",
            json!({"session_id":session}),
        )
        .await
        .unwrap();
    let id: Uuid = serde_json::from_value(rollback["execution_id"].clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let task = engine.store.task(id).await.unwrap();
            if !matches!(task.state.as_str(), "queued" | "running" | "cancelling") {
                assert_eq!(task.state, "succeeded", "{:?}", task.error);
                assert!(task.sent);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    engine
        .dispatch(owner, "mcp.close_session", json!({"session_id":session}))
        .await
        .unwrap();
    engine
        .dispatch(
            controller,
            "connection.close",
            json!({"session_id":initial_session}),
        )
        .await
        .unwrap();
    engine.store.pool.close().await;
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis instance"]
async fn redis_auth_is_rejected_before_any_task_is_persisted() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    engine.register(owner);
    let session = redis_test_session(&engine, owner, AccessMode::ReadOnly).await;
    for method in ["execution.start", "execution.run"] {
        for text in [
            "AUTH secret-marker",
            "PING\n'AU'\\x54H secret-marker",
            "HELLO 3 AUTH user secret-marker",
            "AUTH 'secret-marker",
        ] {
            assert!(
                engine
                    .start(
                        owner,
                        method,
                        json!({"session_id":session,"workspace_id":"redis-test","text":text}),
                        false
                    )
                    .await
                    .is_err()
            );
        }
    }
    let records = engine.store.tasks("redis-test").await.unwrap();
    assert!(records.is_empty());
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM executions WHERE params LIKE '%secret-marker%'")
            .fetch_one(&mut *engine.store.pool.acquire().await.unwrap())
            .await
            .unwrap();
    assert_eq!(count, 0);
    engine
        .dispatch(owner, "connection.close", json!({"session_id":session}))
        .await
        .unwrap();
    engine.store.pool.close().await;
    drop(engine);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis instance"]
async fn redis_select_changes_command_task_db_but_not_browser_db() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    engine.register(owner);
    let session = redis_test_session(&engine, owner, AccessMode::ReadWrite).await;
    let key = format!("datapier-db-test-{}", Uuid::new_v4());
    assert_eq!(
        redis_test_command(&engine, owner, session, &format!("SET {key} db0"))
            .await
            .state,
        "succeeded"
    );
    assert_eq!(
        redis_test_command(&engine, owner, session, "SELECT 1")
            .await
            .state,
        "succeeded"
    );
    let set = redis_test_command(&engine, owner, session, &format!("SET {key} db1")).await;
    assert_eq!(set.state, "succeeded");
    assert_eq!(set.context.unwrap().redis_db, Some(1));
    let get = redis_test_command(&engine, owner, session, &format!("GET {key}")).await;
    assert_eq!(get.context.unwrap().redis_db, Some(1));
    assert_eq!(get.summary["command_db"], 1);
    assert_eq!(get.summary["redis_results"][0]["redis_db"], 1);
    assert_eq!(
        get.summary["redis_results"][0]["result_id"],
        json!(get.results[0])
    );
    let page = engine
        .store
        .result_page(get.results[0], 0, 10, 4096)
        .await
        .unwrap();
    assert_eq!(page.result.summary["redis_db"], 1);
    assert!(serde_json::to_string(&page.rows).unwrap().contains("ZGIx"));
    // 先排队 GET，再在持有会话锁期间完成 SELECT，验证 DB 在串行执行时确定。
    assert_eq!(
        redis_test_command(&engine, owner, session, "SELECT 0")
            .await
            .state,
        "succeeded"
    );
    let database = engine
        .database(owner, &json!({"session_id":session}))
        .unwrap();
    let mut held = database.session.lock().await;
    let queued = engine
        .dispatch(
            owner,
            "execution.start",
            json!({"session_id":session,"workspace_id":"redis-test","text":format!("GET {key}")}),
        )
        .await
        .unwrap();
    let queued_id: Uuid = serde_json::from_value(queued["task_id"].clone()).unwrap();
    let transition_task = engine
        .store
        .create_task(owner, "internal", "execution.run", json!({}), false)
        .await
        .unwrap();
    let sink = ExecutionSink::new(engine.store.clone(), transition_task.id);
    held.call(
        "execution.run",
        json!({"text":"SELECT 1"}),
        &sink,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    drop(held);
    let queued_task = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let task = engine.store.task(queued_id).await.unwrap();
            if !matches!(task.state.as_str(), "queued" | "running" | "cancelling") {
                break task;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(queued_task.state, "succeeded");
    assert_eq!(queued_task.context.unwrap().redis_db, Some(1));
    let queued_page = engine
        .store
        .result_page(queued_task.results[0], 0, 10, 4096)
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&queued_page.rows)
            .unwrap()
            .contains("ZGIx")
    );
    let browse = engine
        .start(
            owner,
            "resource.browse",
            json!({"session_id":session,"target":{"database":"0","name":key,"kind":"string"}}),
            false,
        )
        .await
        .unwrap();
    let result: Uuid = serde_json::from_value(browse["results"][0].clone()).unwrap();
    let page = engine.store.result_page(result, 0, 10, 4096).await.unwrap();
    assert!(serde_json::to_string(&page.rows).unwrap().contains("ZGIw"));
    assert_eq!(
        redis_test_command(&engine, owner, session, "SELECT 0")
            .await
            .state,
        "succeeded"
    );
    let failed = redis_test_command(
        &engine,
        owner,
        session,
        &format!("SELECT 1\nGET {key}\nNOT_A_REAL_COMMAND"),
    )
    .await;
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.context.as_ref().unwrap().redis_db, Some(0));
    assert_eq!(failed.summary["command_db"], 1);
    let attribution = failed.summary["redis_results"].as_array().unwrap();
    assert_eq!(attribution.len(), 2);
    assert_eq!(attribution[0]["redis_db"], 0);
    assert_eq!(attribution[1]["redis_db"], 1);
    let selected: Uuid = serde_json::from_value(attribution[1]["result_id"].clone()).unwrap();
    let selected_page = engine
        .store
        .result_page(selected, 0, 10, 4096)
        .await
        .unwrap();
    assert_eq!(selected_page.result.summary["redis_db"], 1);
    assert!(
        serde_json::to_string(&selected_page.rows)
            .unwrap()
            .contains("ZGIx")
    );
    assert_eq!(
        redis_test_command(
            &engine,
            owner,
            session,
            &format!("DEL {key}\nSELECT 0\nDEL {key}")
        )
        .await
        .state,
        "succeeded"
    );
    engine
        .dispatch(owner, "connection.close", json!({"session_id":session}))
        .await
        .unwrap();
    engine.store.pool.close().await;
    drop(engine);
    std::fs::remove_dir_all(root).unwrap();
}

async fn redis_assert_get(
    engine: &Arc<Engine>,
    owner: Uuid,
    session: Uuid,
    key: &str,
    db: u32,
    encoded: &str,
) {
    let task = redis_test_command(engine, owner, session, &format!("GET {key}")).await;
    assert_eq!(task.state, "succeeded");
    assert_eq!(task.context.unwrap().redis_db, Some(db));
    let page = engine
        .store
        .result_page(task.results[0], 0, 10, 4096)
        .await
        .unwrap();
    assert!(serde_json::to_string(&page.rows).unwrap().contains(encoded));
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis instance"]
async fn redis_transaction_select_tracks_only_successful_exec() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    engine.register(owner);
    let session = redis_test_session(&engine, owner, AccessMode::ReadWrite).await;
    let key = format!("datapier-tx-db-{}", Uuid::new_v4());
    assert_eq!(
        redis_test_command(
            &engine,
            owner,
            session,
            &format!("SET {key} db0\nSELECT 1\nSET {key} db1\nSELECT 0")
        )
        .await
        .state,
        "succeeded"
    );
    assert_eq!(
        redis_test_command(&engine, owner, session, "MULTI")
            .await
            .state,
        "succeeded"
    );
    let queued = redis_test_command(&engine, owner, session, "SELECT 1").await;
    assert_eq!(queued.context.unwrap().redis_db, Some(0));
    assert_eq!(queued.summary["command_db"], 0);
    let before = redis_test_command(&engine, owner, session, &format!("GET {key}")).await;
    assert_eq!(before.context.unwrap().redis_db, Some(0));
    assert_eq!(before.summary["command_db"], 0);
    let executed = redis_test_command(&engine, owner, session, "EXEC").await;
    assert_eq!(executed.context.unwrap().redis_db, Some(0));
    assert_eq!(executed.summary["command_db"], 1);
    redis_assert_get(&engine, owner, session, &key, 1, "ZGIx").await;

    assert_eq!(
        redis_test_command(
            &engine,
            owner,
            session,
            "SELECT 0\nMULTI\nSELECT 1\nDISCARD"
        )
        .await
        .state,
        "succeeded"
    );
    redis_assert_get(&engine, owner, session, &key, 0, "ZGIw").await;
    assert_eq!(
        redis_test_command(&engine, owner, session, "MULTI\nSELECT 1")
            .await
            .state,
        "succeeded"
    );
    assert_eq!(
        redis_test_command(&engine, owner, session, "NOT_A_REAL_COMMAND")
            .await
            .state,
        "failed"
    );
    assert_eq!(
        redis_test_command(&engine, owner, session, "EXEC")
            .await
            .state,
        "failed"
    );
    redis_assert_get(&engine, owner, session, &key, 0, "ZGIw").await;

    let invalid = redis_test_command(&engine, owner, session, "MULTI\nSELECT 999999\nEXEC").await;
    assert_eq!(invalid.summary["command_db"], 0);
    redis_assert_get(&engine, owner, session, &key, 0, "ZGIw").await;
    assert_eq!(
        redis_test_command(
            &engine,
            owner,
            session,
            &format!("DEL {key}\nSELECT 1\nDEL {key}")
        )
        .await
        .state,
        "succeeded"
    );
    engine
        .dispatch(owner, "connection.close", json!({"session_id":session}))
        .await
        .unwrap();
    engine.store.pool.close().await;
    drop(engine);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis instance"]
async fn redis_first_command_connect_cancels_before_proxy_releases() {
    use tokio::io::AsyncReadExt;
    let upstream: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let arrived = Arc::new(tokio::sync::Notify::new());
    let proxy_arrived = arrived.clone();
    let block_new = Arc::new(AtomicBool::new(false));
    let proxy_block = block_new.clone();
    let proxy = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let blocked = proxy_block.load(Ordering::Acquire);
            let arrived = proxy_arrived.clone();
            tokio::spawn(async move {
                if blocked {
                    let mut byte = [0; 1];
                    socket.read_exact(&mut byte).await.unwrap();
                    arrived.notify_one();
                    std::future::pending::<()>().await;
                } else {
                    let mut server = tokio::net::TcpStream::connect(("127.0.0.1", upstream))
                        .await
                        .unwrap();
                    let _ = tokio::io::copy_bidirectional(&mut socket, &mut server).await;
                }
            });
        }
    });
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    engine.register(owner);
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "redis-proxy".into(),
        revision: 1,
        approval_policy: Default::default(),
        provider: model::ProviderKind::Redis,
        access_mode: AccessMode::ReadWrite,
        connection: model::ConnectionConfig::Direct(model::DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: String::new(),
            database: None,
            redis_db: 1,
            redis_databases_override: None,
            tls: model::TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    engine
        .store
        .save_profile(&profile, None, false)
        .await
        .unwrap();
    let session = ProviderSession::connect(profile.clone(), None)
        .await
        .unwrap();
    // 初始化可能使用多个物理连接；仅阻塞随后首次 CLI 连接的握手。
    block_new.store(true, Ordering::Release);
    let id = Uuid::new_v4();
    engine.state.lock().unwrap().databases.insert(
        id,
        Arc::new(Database {
            owner,
            profile: profile.clone(),
            database: None,
            redis_db: Some(1),
            capabilities: Mutex::new(profile.capabilities()),
            session: AsyncMutex::new(session),
            closing: AtomicBool::new(false),
        }),
    );
    let started = engine
        .dispatch(
            owner,
            "execution.start",
            json!({"session_id":id,"text":"GET test"}),
        )
        .await
        .unwrap();
    let task_id: Uuid = serde_json::from_value(started["task_id"].clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(2), arrived.notified())
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_millis(300),
        engine.dispatch(owner, "connection.close", json!({"session_id":id})),
    )
    .await
    .unwrap()
    .unwrap();
    let task = tokio::time::timeout(Duration::from_millis(300), async {
        loop {
            let task = engine.store.task(task_id).await.unwrap();
            if !matches!(task.state.as_str(), "queued" | "running" | "cancelling") {
                break task;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(task.state, "cancelled");
    assert!(!task.sent);
    proxy.abort();
    engine.store.pool.close().await;
    drop(engine);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis instance; pauses the server"]
async fn redis_cancelled_reads_release_both_client_sessions() {
    let (engine, root) = engine().await;
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    let third = Uuid::new_v4();
    engine.register(first);
    engine.register(second);
    engine.register(third);
    let a = redis_test_session(&engine, first, AccessMode::ReadWrite).await;
    let b = redis_test_session(&engine, second, AccessMode::ReadWrite).await;
    let c = redis_test_session(&engine, third, AccessMode::ReadWrite).await;
    for (owner, session) in [(first, a), (second, b), (third, c)] {
        engine
            .dispatch(
                owner,
                "resource.list",
                json!({"session_id":session,"limit":1}),
            )
            .await
            .unwrap();
    }
    let resume_at = tokio::time::Instant::now() + Duration::from_millis(900);
    assert_eq!(
        redis_test_command(&engine, first, a, "CLIENT PAUSE 800 ALL")
            .await
            .state,
        "succeeded"
    );
    let read_a = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .dispatch(first, "resource.list", json!({"session_id":a}))
                .await
        })
    };
    let read_b = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine.dispatch(second, "resource.describe", json!({"session_id":b,"target":{"database":"0","name":"missing","kind":"string"}})).await
        })
    };
    let read_c = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .dispatch(third, "connection.info", json!({"session_id":c}))
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(100)).await;
    let close_a = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .dispatch(first, "connection.close", json!({"session_id":a}))
                .await
        })
    };
    let close_b = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .dispatch(second, "connection.close", json!({"session_id":b}))
                .await
        })
    };
    let close_c = {
        let engine = engine.clone();
        tokio::spawn(async move {
            engine
                .dispatch(third, "connection.close", json!({"session_id":c}))
                .await
        })
    };
    tokio::time::timeout(Duration::from_millis(500), async {
        assert_eq!(read_a.await.unwrap().unwrap_err().code, "cancelled");
        assert_eq!(read_b.await.unwrap().unwrap_err().code, "cancelled");
        assert_eq!(read_c.await.unwrap().unwrap_err().code, "cancelled");
        close_a.await.unwrap().unwrap();
        close_b.await.unwrap().unwrap();
        close_c.await.unwrap().unwrap();
    })
    .await
    .expect("取消必须及时释放读取");
    // 已在暂停窗口内证明读取取消和关闭完成；等待注入的全局暂停到期，避免污染后续用例。
    tokio::time::sleep_until(resume_at).await;
    engine.store.pool.close().await;
    drop(engine);
    std::fs::remove_dir_all(root).unwrap();
}
