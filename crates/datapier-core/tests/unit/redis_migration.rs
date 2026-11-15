use super::*;
use crate::{
    ExecutionSink, Store,
    model::{
        AccessMode, ConnectionConfig, DirectConnection, Profile, ProviderKind, ResourceTarget,
        TlsConfig, Value,
    },
};
use tokio_util::sync::CancellationToken;

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 指向隔离的 Redis 实例"]
async fn watch_rejects_source_and_target_changes() {
    let port: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let root = std::env::temp_dir().join(format!("datapier-redis-migration-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "migration-test".into(),
        revision: 1,
        approval_policy: Default::default(),
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
    let client = Client::open(format!("redis://127.0.0.1:{port}/15")).unwrap();
    let mut raw = client.get_multiplexed_async_connection().await.unwrap();
    let source_key = format!("datapier-migration-test:{}:source", Uuid::new_v4());
    let target_key = format!("datapier-migration-test:{}:target", Uuid::new_v4());
    let _: i64 = redis::cmd("RPUSH")
        .arg(&source_key)
        .arg(vec!["a"; 128])
        .query_async(&mut raw)
        .await
        .unwrap();
    let mut source = RedisMigrationSource::new(&session).await.unwrap();
    let (tx, mut rx) = mpsc::channel(2);
    let cancel = CancellationToken::new();
    let producer = source.stream_key(source_key.as_bytes().to_vec(), tx, &cancel);
    let mutate = async {
        let mut changed = false;
        while let Some(chunk) = rx.recv().await {
            if matches!(chunk, RedisMigrationChunk::Begin { .. }) {
                let _: i64 = redis::cmd("RPUSH")
                    .arg(&source_key)
                    .arg("changed")
                    .query_async(&mut raw)
                    .await
                    .unwrap();
                changed = true;
            }
            assert!(!matches!(chunk, RedisMigrationChunk::Verified));
        }
        assert!(changed);
    };
    let (read, ()) = tokio::join!(producer, mutate);
    assert_eq!(read.unwrap_err().code, "conflict");

    let task = store
        .create_task(
            Uuid::new_v4(),
            "redis-migration",
            "execution.start",
            serde_json::json!({}),
            false,
        )
        .await
        .unwrap();
    let sink = ExecutionSink::new(store.clone(), task.id);
    let target = ResourceTarget {
        database: Some("15".into()),
        name: target_key.clone(),
        key_b64: Some(STANDARD.encode(target_key.as_bytes())),
        kind: "string".into(),
    };
    let opened = session
        .call(
            "import.begin",
            serde_json::json!({"target":target,"type":"string","conflict_policy":"overwrite"}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    let token = opened["token"].clone();
    session
        .call(
            "import.append",
            serde_json::json!({"token":token,"rows":[[Value::Bytes(STANDARD.encode(b"new"))]]}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    let _: () = redis::cmd("SET")
        .arg(&target_key)
        .arg("external")
        .query_async(&mut raw)
        .await
        .unwrap();
    let error = session
        .call(
            "import.finish",
            serde_json::json!({"token":token}),
            &sink,
            &cancel,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "conflict");
    let current: Vec<u8> = redis::cmd("GET")
        .arg(&target_key)
        .query_async(&mut raw)
        .await
        .unwrap();
    assert_eq!(current, b"external");
    session
        .call(
            "import.discard",
            serde_json::json!({"token":token}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    let _: i64 = redis::cmd("DEL")
        .arg(&source_key)
        .arg(&target_key)
        .query_async(&mut raw)
        .await
        .unwrap();
    session.close().await.unwrap();
    store.pool.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
}
