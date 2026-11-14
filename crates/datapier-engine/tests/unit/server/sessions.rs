use super::*;

#[tokio::test]
async fn duplex_sessions_forward_only_owner_events_and_close() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    let (first, first_server) = tokio::io::duplex(4096);
    let (second, second_server) = tokio::io::duplex(4096);
    let first_handler = tokio::spawn(client(engine.clone(), owner, Box::new(first_server)));
    let second_handler = tokio::spawn(client(engine.clone(), other, Box::new(second_server)));
    let first = crate::Client::handshake(Box::new(first), crate::ClientOptions::default())
        .await
        .unwrap();
    let second = crate::Client::handshake(Box::new(second), crate::ClientOptions::default())
        .await
        .unwrap();
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
    let other_client =
        crate::Client::handshake(Box::new(other_peer), crate::ClientOptions::default())
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
            &json!({"id":"open","method":"session.open","params":{"protocol_version":crate::protocol::VERSION,"client_kind":"tsp","workspace":"default","allow_connections":[]}}),
        )
        .await
        .unwrap();
        assert_eq!(
            protocol::read(&mut peer).await.unwrap()["result"]["protocol_version"],
            4
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
        revision: 1,
        approval_policy: Default::default(),
        provider: model::ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: model::ConnectionConfig::Direct(model::DirectConnection {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            username: "test".into(),
            database: Some("test".into()),
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
        revision: 1,
        approval_policy: Default::default(),
        provider: model::ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: model::ConnectionConfig::Direct(model::DirectConnection {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            username: "test".into(),
            database: Some("test".into()),
            redis_db: 0,
            redis_databases_override: None,
            tls: model::TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
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
        revision: 1,
        approval_policy: Default::default(),
        provider: model::ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: model::ConnectionConfig::Direct(model::DirectConnection {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            redis_db: 0,
            redis_databases_override: None,
            tls: model::TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
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
    let options = sqlx::sqlite::SqliteConnectOptions::new().filename(root.join("datapier.sqlite3"));
    let connection = sqlx::ConnectOptions::connect(&options).await.unwrap();
    Arc::get_mut(&mut engine).unwrap().store.pool =
        datapier_core::storage::FixedPool::new(vec![connection]);
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let mut profile = Profile {
        id: Uuid::new_v4(),
        name: "old".into(),
        revision: 1,
        approval_policy: Default::default(),
        provider: model::ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: model::ConnectionConfig::Direct(model::DirectConnection {
            host: "127.0.0.1".into(),
            port: 3306,
            username: "test".into(),
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
    drop(engine);
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 实例"]
async fn schema_read_timeouts_release_the_owner_editing_gate() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let port = std::env::var("DATAPIER_TEST_MYSQL_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
    let mysql = model::MysqlConnectionOptions {
        query_timeout_seconds: Some(1),
        ..Default::default()
    };
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "schema-timeout".into(),
        revision: 1,
        approval_policy: Default::default(),
        provider: model::ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: model::ConnectionConfig::Direct(model::DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            redis_db: 0,
            redis_databases_override: None,
            tls: model::TlsConfig::default(),
            visible_databases: None,
            mysql,
        }),
    };
    engine
        .store
        .save_profile(&profile, Some(secret), false)
        .await
        .unwrap();
    let opened = engine
        .dispatch(owner, "connection.open", json!({"profile_id":profile.id}))
        .await
        .unwrap();
    let session_id: Uuid = serde_json::from_value(opened["session_id"].clone()).unwrap();
    let db = engine.state.lock().unwrap().databases[&session_id].clone();
    let target = json!({"kind":"table","name":"timeout_test","database":"datapier_fixture"});
    for (method, params) in [
        (
            "schema.describe",
            json!({"session_id":session_id,"target":target}),
        ),
        (
            "schema.plan",
            json!({"session_id":session_id,"intent":{"target":target,"baseline":null,"definition":null,"profile_revision":"","algorithm":null,"lock":null,"server_version":"","sql_mode":""}}),
        ),
    ] {
        let guard = db.session.lock().await;
        let result = tokio::time::timeout(
            Duration::from_secs(4),
            engine.dispatch(owner, method, params),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(result.code, "timeout");
        drop(guard);
        let next = tokio::time::timeout(
            Duration::from_secs(2),
            engine.dispatch(
                owner,
                "schema.discard",
                json!({"plan_id":Uuid::new_v4(),"revision":1}),
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(next.code, "handle_expired");
    }
    engine.disconnect(owner).await;
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

fn mysql_mcp_profile(port: u16, timeout: u64) -> Profile {
    Profile {
        id: Uuid::new_v4(),
        name: "mcp-mysql-regression".into(),
        revision: 1,
        approval_policy: model::ApprovalPolicy::Off,
        provider: model::ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: model::ConnectionConfig::Direct(model::DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            redis_db: 0,
            redis_databases_override: None,
            tls: model::TlsConfig::default(),
            visible_databases: None,
            mysql: model::MysqlConnectionOptions {
                query_timeout_seconds: Some(timeout),
                ..Default::default()
            },
        }),
    }
}

async fn mysql_mcp_client(
    engine: &Arc<Engine>,
) -> (crate::Client, Uuid, tokio::task::JoinHandle<()>) {
    let owner = Uuid::new_v4();
    let (stream, server) = tokio::io::duplex(65536);
    let handler = tokio::spawn(client(engine.clone(), owner, Box::new(server)));
    let options = crate::ClientOptions {
        kind: model::ClientKind::Mcp,
        ..Default::default()
    };
    let client = crate::Client::handshake(Box::new(stream), options)
        .await
        .unwrap();
    (client, owner, handler)
}

async fn mysql_mcp_wait_result(client: &crate::Client, execution: &Value) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let state = client
                .request("mcp.get_execution", json!({"execution_id":execution}))
                .await
                .unwrap();
            if let Some(result) = state["results"].as_array().and_then(|ids| ids.first()) {
                break result.clone();
            }
            assert!(
                !matches!(
                    state["state"].as_str(),
                    Some("failed" | "cancelled" | "outcome_unknown")
                ),
                "{state}"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[test]
#[ignore = "需要 DATAPIER_TEST_MYSQL_PORT、DATAPIER_TEST_MYSQL_SECRET 和隔离 MySQL"]
fn mcp_ipc_reads_mysql_results_and_closes_locked_stream() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_stack_size(2 * 1024 * 1024)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        use sqlx::Connection;
        let port: u16 = std::env::var("DATAPIER_TEST_MYSQL_PORT").unwrap().parse().unwrap();
        let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
        let options = sqlx::mysql::MySqlConnectOptions::new().host("127.0.0.1").port(port)
            .username("datapier").password(&secret).database("datapier_fixture");
        let mut direct = sqlx::MySqlConnection::connect_with(&options).await.unwrap();
        let table = format!("mcp_stream_{}", Uuid::new_v4().simple());
        // 动态标识符仅来自本测试生成的 UUID，rows 仅含本地整数与固定文本。
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE `{table}` (id INT PRIMARY KEY, label VARCHAR(32) NOT NULL) ENGINE=InnoDB")))
            .execute(&mut direct).await.unwrap();
        let rows = (0..220).map(|n| format!("({n},'row-{n}')")).collect::<Vec<_>>().join(",");
        sqlx::query(sqlx::AssertSqlSafe(format!("INSERT INTO `{table}` VALUES {rows}"))).execute(&mut direct).await.unwrap();
        let (engine, root) = engine().await;
        let profile = mysql_mcp_profile(port, 10);
        engine.store.save_profile(&profile, Some(secret), false).await.unwrap();
        let (mcp, _, mcp_handler) = mysql_mcp_client(&engine).await;
        let (other_stream, other_server) = tokio::io::duplex(65536);
        let other_handler = tokio::spawn(client(engine.clone(), Uuid::new_v4(), Box::new(other_server)));
        let other = crate::Client::handshake(Box::new(other_stream), crate::ClientOptions::default()).await.unwrap();
        let opened = mcp.request("mcp.open_session", json!({"connection_id":profile.id})).await.unwrap();
        let session = opened["session_id"].clone();
        let started = mcp.request("mcp.execute_command", json!({"session_id":session,"text":format!("SELECT id,label FROM `{table}` ORDER BY id LIMIT 4")})).await.unwrap();
        let result = mysql_mcp_wait_result(&mcp, &started["execution_id"]).await;
        let page = mcp.request("mcp.read_result", json!({"result_id":result,"page_size":4})).await.unwrap();
        let rows: Vec<Vec<model::Value>> = serde_json::from_value(page["rows"].clone()).unwrap();
        assert_eq!(rows.len(), 4);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row[0].text(), index.to_string());
            assert_eq!(row[1].text(), format!("row-{index}"));
        }
        assert_eq!(other.request("session.heartbeat", json!({})).await.unwrap()["ok"], true);
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let state = mcp.request("mcp.get_execution", json!({"execution_id":started["execution_id"]})).await.unwrap();
                if state["state"] == "succeeded" { break; }
                assert!(!matches!(state["state"].as_str(), Some("failed" | "cancelled" | "outcome_unknown")), "{state}");
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        let begin = mcp.request("mcp.begin_transaction", json!({"session_id":session})).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let state = mcp.request("mcp.get_execution", json!({"execution_id":begin["execution_id"]})).await.unwrap();
                if state["state"] == "succeeded" { break; }
                assert!(!matches!(state["state"].as_str(), Some("failed" | "cancelled" | "outcome_unknown")), "{state}");
                tokio::task::yield_now().await;
            }
        }).await.unwrap();
        let locked = mcp.request("mcp.execute_command", json!({"session_id":session,"text":format!("SELECT id,label FROM `{table}` ORDER BY id FOR UPDATE")})).await.unwrap();
        let result = mysql_mcp_wait_result(&mcp, &locked["execution_id"]).await;
        let first = mcp.request("mcp.read_result", json!({"result_id":result,"page_size":100})).await.unwrap();
        assert_eq!(first["row_count"], 100);
        assert_eq!(first["has_more"], true);
        sqlx::query("SET SESSION innodb_lock_wait_timeout=1").execute(&mut direct).await.unwrap();
        let locked_update = tokio::time::timeout(Duration::from_secs(4), sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE `{table}` SET label='blocked' WHERE id=0"))).execute(&mut direct)).await.unwrap();
        assert!(locked_update.is_err(), "锁定查询应阻塞其他连接的更新");
        tokio::time::timeout(Duration::from_secs(4), mcp.request("mcp.close_session", json!({"session_id":session}))).await.unwrap().unwrap();
        tokio::time::timeout(Duration::from_secs(4), sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE `{table}` SET label='released' WHERE id=0"))).execute(&mut direct)).await.unwrap().unwrap();
        assert_eq!(other.request("session.heartbeat", json!({})).await.unwrap()["ok"], true);
        mcp.close().await.unwrap();
        other.close().await.unwrap();
        mcp_handler.await.unwrap();
        other_handler.await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE `{table}`"))).execute(&mut direct).await.unwrap();
        drop(direct);
        engine.store.pool.close().await;
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要 DATAPIER_TEST_MYSQL_PORT、DATAPIER_TEST_MYSQL_SECRET 和隔离 MySQL"]
async fn mcp_preflight_blackhole_obeys_deadline_and_disconnect() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let upstream: u16 = std::env::var("DATAPIER_TEST_MYSQL_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
    for eof in [false, true] {
        let (engine, root) = engine().await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let blocked = Arc::new(AtomicBool::new(false));
        let proxy_blocked = blocked.clone();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
        let proxy = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let upstream = tokio::net::TcpStream::connect(("127.0.0.1", upstream))
                .await
                .unwrap();
            let (mut from_client, mut to_client) = socket.into_split();
            let (mut from_server, mut to_server) = upstream.into_split();
            let outgoing = async move {
                let mut seen = Some(seen_tx);
                let mut buf = [0u8; 8192];
                loop {
                    let n = from_client.read(&mut buf).await.unwrap();
                    if n == 0 {
                        break;
                    }
                    if proxy_blocked.load(Ordering::Acquire)
                        && buf[..n]
                            .windows(b"SELECT VERSION() AS version".len())
                            .any(|part| part == b"SELECT VERSION() AS version")
                    {
                        if let Some(seen) = seen.take() {
                            let _ = seen.send(());
                        }
                        std::future::pending::<()>().await;
                    }
                    to_server.write_all(&buf[..n]).await.unwrap();
                }
            };
            tokio::select! {
                _ = outgoing => (),
                _ = tokio::io::copy(&mut from_server, &mut to_client) => (),
            }
        });
        let profile = mysql_mcp_profile(port, if eof { 30 } else { 1 });
        engine
            .store
            .save_profile(&profile, Some(secret.clone()), false)
            .await
            .unwrap();
        let owner = Uuid::new_v4();
        let (app, bridge_client) = tokio::io::duplex(65536);
        let (bridge_server, server) = tokio::io::duplex(65536);
        let bridge = tokio::spawn(async move {
            let (mut a_read, mut a_write) = tokio::io::split(bridge_client);
            let (mut b_read, mut b_write) = tokio::io::split(bridge_server);
            tokio::select! {
                _ = tokio::io::copy(&mut a_read, &mut b_write) => (),
                _ = tokio::io::copy(&mut b_read, &mut a_write) => (),
            }
        });
        let handler = tokio::spawn(client(engine.clone(), owner, Box::new(server)));
        let mcp = crate::Client::handshake(
            Box::new(app),
            crate::ClientOptions {
                kind: model::ClientKind::Mcp,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let (other_stream, other_server) = tokio::io::duplex(65536);
        let other_handler = tokio::spawn(client(
            engine.clone(),
            Uuid::new_v4(),
            Box::new(other_server),
        ));
        let other =
            crate::Client::handshake(Box::new(other_stream), crate::ClientOptions::default())
                .await
                .unwrap();
        let opened = mcp
            .request("mcp.open_session", json!({"connection_id":profile.id}))
            .await
            .unwrap();
        let session = opened["session_id"].clone();
        let session_id: Uuid = serde_json::from_value(session.clone()).unwrap();
        blocked.store(true, Ordering::Release);
        let caller = mcp.clone();
        let request = tokio::spawn(async move {
            caller
                .request("mcp.get_session", json!({"session_id":session}))
                .await
        });
        tokio::time::timeout(Duration::from_secs(5), seen_rx)
            .await
            .unwrap()
            .unwrap();
        if eof {
            bridge.abort();
            tokio::time::timeout(Duration::from_secs(4), handler)
                .await
                .unwrap()
                .unwrap();
            let error = tokio::time::timeout(Duration::from_secs(4), request)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert!(matches!(error.code.as_str(), "disconnected" | "cancelled"));
            tokio::time::timeout(Duration::from_secs(4), async {
                loop {
                    if !engine
                        .state
                        .lock()
                        .unwrap()
                        .databases
                        .contains_key(&session_id)
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
        } else {
            let error = tokio::time::timeout(Duration::from_secs(4), request)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            assert_eq!(error.code, "timeout");
            mcp.close().await.unwrap();
            tokio::time::timeout(Duration::from_secs(4), handler)
                .await
                .unwrap()
                .unwrap();
            bridge.abort();
        }
        assert_eq!(
            other.request("session.heartbeat", json!({})).await.unwrap()["ok"],
            true
        );
        other.close().await.unwrap();
        other_handler.await.unwrap();
        proxy.abort();
        engine.store.pool.close().await;
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要隔离 MySQL、DATAPIER_TEST_MYSQL_PORT/SECRET 和 CREATE/DROP DATABASE 权限"]
async fn mcp_implicit_transaction_rechecks_original_database_after_use() {
    use sqlx::Connection;

    let port: u16 = std::env::var("DATAPIER_TEST_MYSQL_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
    let options = sqlx::mysql::MySqlConnectOptions::new()
        .host("127.0.0.1")
        .port(port)
        .username("datapier")
        .password(&secret)
        .database("datapier_fixture");
    let mut observer = sqlx::MySqlConnection::connect_with(&options).await.unwrap();
    let table = format!("mcp_tx_{}", Uuid::new_v4().simple());
    let database_b = format!("mcp_b_{}", Uuid::new_v4().simple());
    // 表名和数据库名仅来自本测试生成的 UUID。
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY) ENGINE=InnoDB"
    )))
    .execute(&mut observer)
    .await
    .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE DATABASE `{database_b}`"
    )))
    .execute(&mut observer)
    .await
    .unwrap();

    let (engine, root) = engine().await;
    let mut settings = engine.store.settings().await.unwrap();
    settings.mcp.fine_grained_access = true;
    engine.store.set_settings(&settings).await.unwrap();
    let profile = mysql_mcp_profile(port, 10);
    engine
        .store
        .save_profile(&profile, Some(secret), false)
        .await
        .unwrap();
    let rule = |database: &str| model::McpAccessRule {
        profile_id: profile.id,
        scope_kind: "database".into(),
        scope_key: database.into(),
        level: model::McpAccessLevel::ReadWrite,
    };
    let revision = engine
        .store
        .save_mcp_rules(
            profile.id,
            1,
            &[rule("datapier_fixture"), rule(&database_b)],
        )
        .await
        .unwrap();
    let (mcp, owner, handler) = mysql_mcp_client(&engine).await;
    let opened = mcp
        .request("mcp.open_session", json!({"connection_id":profile.id}))
        .await
        .unwrap();
    let session = opened["session_id"].clone();
    let session_id: Uuid = serde_json::from_value(session.clone()).unwrap();
    // 在新物理会话上设置服务器默认行为；之后的写入、切库及事务控制均走 MCP IPC。
    let db = engine.state.lock().unwrap().databases[&session_id].clone();
    let setup = engine
        .store
        .create_task(owner, "default", "execution.run", json!({}), false)
        .await
        .unwrap();
    let setup_sink = ExecutionSink::new(engine.store.clone(), setup.id);
    db.session
        .lock()
        .await
        .call(
            "execution.run",
            json!({"text":"SET SESSION autocommit=0"}),
            &setup_sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(
        mcp.request("mcp.get_session", json!({"session_id":session}))
            .await
            .unwrap()["database"],
        "datapier_fixture"
    );

    for text in [
        format!("INSERT INTO `datapier_fixture`.`{table}` VALUES (1)"),
        format!("USE `{database_b}`"),
    ] {
        let started = mcp
            .request(
                "mcp.execute_command",
                json!({"session_id":session,"text":text}),
            )
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let state = mcp
                    .request(
                        "mcp.get_execution",
                        json!({"execution_id":started["execution_id"]}),
                    )
                    .await
                    .unwrap();
                if state["state"] == "succeeded" {
                    break;
                }
                assert!(
                    !matches!(
                        state["state"].as_str(),
                        Some("failed" | "cancelled" | "outcome_unknown")
                    ),
                    "{state}"
                );
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }
    assert_eq!(
        mcp.request("mcp.get_session", json!({"session_id":session}))
            .await
            .unwrap()["database"],
        database_b
    );
    engine
        .store
        .save_mcp_rules(profile.id, revision, &[rule(&database_b)])
        .await
        .unwrap();
    assert_eq!(
        mcp.request("mcp.get_session", json!({"session_id":session}))
            .await
            .unwrap()["database"],
        database_b
    );
    for method in ["mcp.commit_transaction", "mcp.begin_transaction"] {
        let error = mcp
            .request(method, json!({"session_id":session}))
            .await
            .unwrap_err();
        assert_eq!(error.code, "permission_denied", "{method}: {error}");
        let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT COUNT(*) FROM `{table}` WHERE id=1"
        )))
        .fetch_one(&mut observer)
        .await
        .unwrap();
        assert_eq!(count, 0, "撤权后不能提交 A 库写入");
    }
    mcp.request("mcp.close_session", json!({"session_id":session}))
        .await
        .unwrap();
    let count: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT COUNT(*) FROM `{table}` WHERE id=1"
    )))
    .fetch_one(&mut observer)
    .await
    .unwrap();
    assert_eq!(count, 0, "关闭会话必须回滚未提交写入");
    mcp.close().await.unwrap();
    handler.await.unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE `{table}`")))
        .execute(&mut observer)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP DATABASE `{database_b}`")))
        .execute(&mut observer)
        .await
        .unwrap();
    drop(observer);
    engine.store.pool.close().await;
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
