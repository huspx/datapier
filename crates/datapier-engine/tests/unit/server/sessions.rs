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
