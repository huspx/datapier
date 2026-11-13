//! 需要显式配置的隔离 MySQL，验证页重查保留连接上下文与原始 LIMIT。
use datapier_core::{ExecutionSink, Store, model::*, providers::ProviderSession};
use serde_json::{Value as Json, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn call(db: &mut ProviderSession, store: &Store, method: &str, params: Json) -> Json {
    let task = store
        .create_task(Uuid::new_v4(), "mysql-live", method, json!({}), false)
        .await
        .unwrap();
    db.call(
        method,
        params,
        &ExecutionSink::new(store.clone(), task.id),
        &CancellationToken::new(),
    )
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn live_pages_and_explicit_count_keep_original_session_and_limit() {
    let root = std::env::temp_dir().join(format!("datapier-live-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "live-fixture".into(),
        revision: 1,
        approval_policy: ApprovalPolicy::default(),
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port: std::env::var("DATAPIER_TEST_MYSQL_PORT")
                .unwrap()
                .parse()
                .unwrap(),
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            visible_databases: None,
            mysql: Default::default(),
            redis_db: 0,
            redis_databases_override: None,
            tls: TlsConfig::default(),
        }),
    };
    let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
    let mut db = ProviderSession::connect(profile, Some(secret))
        .await
        .unwrap();
    call(
        &mut db,
        &store,
        "execution.run",
        json!({"text":"CREATE TEMPORARY TABLE datapier_live_fixture (id INT PRIMARY KEY)"}),
    )
    .await;
    call(&mut db, &store, "transaction.begin", json!({})).await;
    call(
        &mut db,
        &store,
        "execution.run",
        json!({"text":"INSERT INTO datapier_live_fixture VALUES (1),(2),(3),(4)"}),
    )
    .await;
    call(
        &mut db,
        &store,
        "execution.run",
        json!({"text":"UPDATE datapier_live_fixture SET id=id WHERE id=1"}),
    )
    .await;
    assert_eq!(
        call(&mut db, &store, "connection.info", json!({})).await["transaction"],
        true
    );
    let result = call(&mut db, &store, "execution.run", json!({"text":"SELECT id FROM datapier_live_fixture WHERE id > ? ORDER BY id LIMIT 3", "parameters":[{"type":"integer","value":"0"}], "page_size":1})).await;
    let id: Uuid = serde_json::from_value(result["results"][0].clone()).unwrap();
    let meta = store.result_meta(id).await.unwrap();
    assert_eq!(meta.summary["retrieval_mode"], "live_page");
    assert!(meta.summary["total_rows"].is_null());
    let second = call(
        &mut db,
        &store,
        "result.read",
        json!({"result_id":id,"start":1,"max_rows":1,"max_bytes":1048576}),
    )
    .await;
    assert_eq!(second["rows"][0][0], json!({"type":"integer","value":"2"}));
    let count = call(&mut db, &store, "result.count", json!({"result_id":id})).await;
    assert_eq!(count["total_rows"], 3);
    let end = call(
        &mut db,
        &store,
        "result.read",
        json!({"result_id":id,"start":3,"max_rows":1,"max_bytes":1048576}),
    )
    .await;
    assert_eq!(end["rows"].as_array().unwrap().len(), 0);
    call(&mut db, &store, "transaction.rollback", json!({})).await;
    db.close().await.unwrap();
}
