use super::*;

#[test]
fn script_errors_preserve_completed_and_unexecuted_work() {
    let id = Uuid::new_v4();
    let done = vec![
        json!({"index":0,"status":"executed","affected_rows":1,"results":[id],"transaction_state":"unknown"}),
    ];
    let error =
        AppError::new("database", "table missing").with_details(json!({"mysql_errno":1146}));
    let error = script_error(
        error,
        &done,
        &[id],
        1,
        "INSERT INTO t VALUES (2)",
        Some(("SELECT * FROM missing", "failed")),
    );
    assert_eq!(error.details["mysql_errno"], 1146);
    assert_eq!(error.details["statements"][0]["status"], "executed");
    assert_eq!(error.details["statements"][1]["status"], "failed");
    assert_eq!(error.details["affected_rows"], 1);
    assert_eq!(error.details["results"][0], id.to_string());
    assert_eq!(error.details["unexecuted_sql"], "INSERT INTO t VALUES (2)");
}

fn fixture_profile(mode: AccessMode) -> (Profile, Option<String>) {
    (
        Profile {
            id: Uuid::new_v4(),
            name: "mysql-fixture".into(),
            provider: ProviderKind::Mysql,
            host: "127.0.0.1".into(),
            port: std::env::var("DATAPIER_TEST_MYSQL_PORT")
                .unwrap()
                .parse()
                .unwrap(),
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            redis_db: 0,
            access_mode: mode,
            tls: TlsConfig::default(),
            visible_databases: None,
        },
        Some(std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap()),
    )
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 实例"]
async fn cancelled_catalog_connection_releases_session_lock() {
    let (profile, secret) = fixture_profile(AccessMode::ReadWrite);
    let mut session = MysqlSession::connect(profile, secret).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    session.options = session
        .options
        .clone()
        .port(listener.local_addr().unwrap().port());
    let session = std::sync::Arc::new(tokio::sync::Mutex::new(session));
    for method in ["list", "describe"] {
        let cancel = CancellationToken::new();
        let task = {
            let session = session.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                let mut session = session.lock().await;
                if method == "list" {
                    catalog::list(&mut session, json!({}), &cancel).await
                } else {
                    catalog::describe(
                        &mut session,
                        json!({"target": {
                            "database":"datapier_fixture", "name":"any_table", "kind":"table"
                        }}),
                        &cancel,
                    )
                    .await
                }
            })
        };
        let (socket, _) =
            tokio::time::timeout(std::time::Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
        cancel.cancel();
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(3), task)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err()
                .code,
            "cancelled"
        );
        let mut session = tokio::time::timeout(std::time::Duration::from_secs(3), session.lock())
            .await
            .unwrap();
        let one: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(session.connection().unwrap())
            .await
            .unwrap();
        assert_eq!(one, 1);
        drop(socket);
    }
    session.lock().await.close().await.unwrap();
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 实例"]
async fn cancelled_current_database_query_discards_shared_connection_and_unlocks() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (mut profile, secret) = fixture_profile(AccessMode::ReadWrite);
    let upstream = profile.port;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    profile.port = listener.local_addr().unwrap().port();
    let blocked = Arc::new(AtomicBool::new(false));
    let (seen_tx, seen_rx) = tokio::sync::oneshot::channel();
    let proxy = {
        let blocked = blocked.clone();
        tokio::spawn(async move {
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
                    if blocked.load(Ordering::Acquire) {
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
        })
    };
    let mut session = MysqlSession::connect(profile, secret).await.unwrap();
    session.profile.database = None;
    let session = Arc::new(tokio::sync::Mutex::new(session));
    blocked.store(true, Ordering::Release);
    let cancel = CancellationToken::new();
    let task = {
        let session = session.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut session = session.lock().await;
            catalog::describe(
                &mut session,
                json!({"target":{"name":"any_table","kind":"table"}}),
                &cancel,
            )
            .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(3), seen_rx)
        .await
        .unwrap()
        .unwrap();
    cancel.cancel();
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
            .code,
        "cancelled"
    );
    let mut session = tokio::time::timeout(std::time::Duration::from_secs(3), session.lock())
        .await
        .unwrap();
    assert_eq!(session.connection().unwrap_err().code, "connection_closed");
    proxy.abort();
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 8.0 或 8.4 实例"]
async fn every_readonly_physical_connection_uses_server_constraint() {
    let (profile, secret) = fixture_profile(AccessMode::ReadOnly);
    let mut session = MysqlSession::connect(profile, secret).await.unwrap();
    let mut browse = session.new_connection().await.unwrap();
    let readonly: i64 = sqlx::query_scalar("SELECT @@SESSION.transaction_read_only")
        .fetch_one(&mut browse)
        .await
        .unwrap();
    assert_eq!(readonly, 1);
    browse.close().await.unwrap();
    session.close().await.unwrap();
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 8.0 或 8.4 实例"]
async fn failed_script_preserves_completed_work_and_remaining_sql() {
    let root = std::env::temp_dir().join(format!("datapier-mysql-script-{}", Uuid::new_v4()));
    let store = crate::Store::open(&root).await.unwrap();
    let task = store
        .create_task(Uuid::new_v4(), "test", "execution.run", json!({}), true)
        .await
        .unwrap();
    let sink = ExecutionSink::new(store.clone(), task.id);
    let (profile, secret) = fixture_profile(AccessMode::ReadWrite);
    let mut session = MysqlSession::connect(profile, secret).await.unwrap();
    let text = "CREATE TEMPORARY TABLE dp_script_progress(id INT); INSERT INTO dp_script_progress VALUES (7); SELECT * FROM dp_table_does_not_exist; INSERT INTO dp_script_progress VALUES (8)";
    let error = session
        .call(
            "execution.run",
            json!({"text":text}),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.details["statements"][1]["status"], "executed");
    assert_eq!(error.details["statements"][2]["status"], "failed");
    assert_eq!(error.details["affected_rows"], 1);
    assert!(
        error.details["unexecuted_sql"]
            .as_str()
            .unwrap()
            .contains("VALUES (8)")
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dp_script_progress")
        .fetch_one(session.connection().unwrap())
        .await
        .unwrap();
    assert_eq!(count, 1);
    session.close().await.unwrap();
    store.pool.close().await;
    store.cache.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
}
