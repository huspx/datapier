use super::*;

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT isolated Redis"]
async fn idle_scan_releases_budget_and_preserves_active_scan() {
    let port: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let root = std::env::temp_dir().join(format!("datapier-scan-idle-{}", Uuid::new_v4()));
    let store = crate::Store::open(&root).await.unwrap();
    let owner = Uuid::new_v4();
    let task = store
        .create_task(owner, "redis-scan", "execution.start", json!({}), false)
        .await
        .unwrap();
    let sink = ExecutionSink::new(store.clone(), task.id);
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "redis-scan".into(),
        revision: 1,
        approval_policy: ApprovalPolicy::default(),
        provider: ProviderKind::Redis,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: String::new(),
            database: None,
            redis_db: 15,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    let mut session = RedisSession::connect(profile, None).await.unwrap();
    let mut conn = Client::open(format!("redis://127.0.0.1:{port}/15"))
        .unwrap()
        .get_multiplexed_async_connection()
        .await
        .unwrap();
    let prefix = format!("datapier-idle-{}-", Uuid::new_v4());
    let keys: Vec<_> = (0..300).map(|i| format!("{prefix}{i}")).collect();
    let mut pipe = redis::pipe();
    for key in &keys {
        pipe.cmd("SET").arg(key).arg("v").ignore();
    }
    let _: () = pipe.query_async(&mut conn).await.unwrap();
    let cancel = CancellationToken::new();
    let params = json!({"pattern":format!("{prefix}*"),"limit":1});
    let first = session
        .call("resource.list", params.clone(), &sink, &cancel)
        .await
        .unwrap();
    let second = session
        .call("resource.list", params, &sink, &cancel)
        .await
        .unwrap();
    let old = Uuid::parse_str(first["cursor"].as_str().unwrap()).unwrap();
    let active = Uuid::parse_str(second["cursor"].as_str().unwrap()).unwrap();
    let idle =
        std::time::Duration::from_secs(store.settings().await.unwrap().limits.result_idle_seconds);
    let old_bytes = session.scans[&old].bytes;
    assert!(old_bytes > 0);
    session.scans.get_mut(&old).unwrap().last_used = std::time::Instant::now() - idle;
    let available = store.available_memory_for_owner(owner).await.unwrap();
    session
        .call("connection.info", json!({}), &sink, &cancel)
        .await
        .unwrap();
    assert_eq!(
        store.available_memory_for_owner(owner).await.unwrap(),
        available + old_bytes
    );
    assert!(!session.scans.contains_key(&old));
    assert!(session.scans.contains_key(&active));
    let error = session
        .call(
            "resource.list",
            json!({"cursor":old.to_string()}),
            &sink,
            &cancel,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "handle_expired");
    let previous = session.scans[&active].last_used;
    session
        .call(
            "resource.list",
            json!({"cursor":active.to_string(),"limit":1}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    assert!(session.scans[&active].last_used >= previous);
    session
        .call(
            "resource.scan.discard",
            json!({"cursor":active.to_string()}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    let _: i64 = redis::cmd("DEL")
        .arg(&keys)
        .query_async(&mut conn)
        .await
        .unwrap();
    session.close().await.unwrap();
    drop(sink);
    store.pool.close().await;
    drop(store);
    tokio::fs::remove_dir_all(root).await.unwrap();
}
