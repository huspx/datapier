use base64::{Engine as _, engine::general_purpose::STANDARD};
async fn preparation_sink(store: &datapier_core::Store) -> datapier_core::ExecutionSink {
    let task = store
        .create_task(
            uuid::Uuid::new_v4(),
            "preparation-fixture",
            "transfer.prepare",
            serde_json::json!({}),
            false,
        )
        .await
        .unwrap();
    datapier_core::ExecutionSink::new(store.clone(), task.id)
}

use datapier_core::{
    ExecutionSink, Store,
    execution::{
        data_files::{RowEvent, RowReader, fingerprint},
        migration,
    },
    model::{
        AccessMode, ConnectionConfig, DirectConnection, Profile, ProviderKind, TlsConfig, Value,
        transfer::{TransferInput, TransferSpec, TransferStepState},
    },
};
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 和隔离 Redis DB 15"]
async fn redis_skip_reasons_and_progress_survive_report_readback() {
    let port: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let client = redis::Client::open(format!("redis://127.0.0.1:{port}/15")).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let id = Uuid::new_v4().simple().to_string();
    let source_prefix = format!("datapier-test:{id}:source:");
    let target_prefix = format!("datapier-test:{id}:target:");
    let source = format!("{source_prefix}key");
    let target = format!("{target_prefix}key");
    let root = std::env::temp_dir().join(format!("datapier-redis-observability-{id}"));
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: format!("redis-observability-{id}"),
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
    store.save_profile(&profile, None, false).await.unwrap();
    let result = async {
        let _: () = redis::cmd("SET")
            .arg(&source)
            .arg("source payload")
            .query_async(&mut conn)
            .await
            .unwrap();
        let _: () = redis::cmd("SET")
            .arg(&target)
            .arg("protected")
            .query_async(&mut conn)
            .await
            .unwrap();
        let spec: TransferSpec = serde_json::from_value(json!({"mode":"copy","provider":"redis", "source":{"profile_id":profile.id,"redis_db":15}, "destination":{"profile_id":profile.id,"redis_db":15}, "redis":{"keys_b64":[STANDARD.encode(&source)],"source_prefix_b64":STANDARD.encode(&source_prefix),"target_prefix_b64":STANDARD.encode(&target_prefix)}, "options":{"conflict":"skip","include_schema":false,"include_data":true}})).unwrap();
        let cancel = CancellationToken::new();
        let prepared = migration::prepare(&preparation_sink(&store).await, spec, &cancel)
            .await
            .unwrap();
        let owner = Uuid::new_v4();
        let task = store
            .create_task(
                owner,
                "redis-observability",
                "transfer.execute",
                json!({}),
                true,
            )
            .await
            .unwrap();
        let sink = ExecutionSink::new(store.clone(), task.id).with_owner(owner);
        let progress = migration::execute(
            &prepared,
            &sink,
            task.id,
            "redis-observability",
            None,
            &[],
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(progress.processed_bytes, b"source payload".len() as u64);
        assert_eq!(progress.skipped, 1);
        assert_eq!(
            progress.current_object.as_deref(),
            Some(format!("{:x}", Sha256::digest(source.as_bytes())).as_str())
        );
        let items = store.transfer_items(task.id, None, 10).await.unwrap();
        assert_eq!(items[0].error_code.as_deref(), Some("redis_target_exists"));
        let current: String = redis::cmd("GET")
            .arg(&target)
            .query_async(&mut conn)
            .await
            .unwrap();
        assert_eq!(current, "protected");

        for format in ["json", "csv"] {
            let output = root.join(format!("export.{format}"));
            let spec: TransferSpec = serde_json::from_value(json!({"mode":"export","provider":"redis","source":{"profile_id":profile.id,"redis_db":15},"output":{"path":output,"format":format},"redis":{"keys_b64":[STANDARD.encode(&source)]},"options":{"include_schema":false,"include_data":true}})).unwrap();
            let prepared = migration::prepare(&preparation_sink(&store).await, spec, &cancel)
                .await
                .unwrap();
            let task = store
                .create_task(
                    owner,
                    "redis-observability",
                    "transfer.execute",
                    json!({}),
                    true,
                )
                .await
                .unwrap();
            let sink = ExecutionSink::new(store.clone(), task.id).with_owner(owner);
            let progress = migration::execute(
                &prepared,
                &sink,
                task.id,
                "redis-observability",
                None,
                &[],
                &cancel,
            )
            .await
            .unwrap();
            let bytes = tokio::fs::metadata(&output).await.unwrap().len();
            assert_eq!(progress.processed_bytes, bytes);
            let run = store.transfer_run(task.id).await.unwrap();
            assert_eq!(run.state, "succeeded");
            assert_eq!(run.progress.processed_bytes, bytes);
            let items = store.transfer_items(task.id, None, 10).await.unwrap();
            assert_eq!(items.len(), 2);
            assert!(
                items
                    .iter()
                    .all(|item| item.state == TransferStepState::Applied)
            );

            let input: TransferInput =
                serde_json::from_value(json!({"path":output,"format":format})).unwrap();
            let fingerprint = fingerprint(&input.path, 1024 * 1024, 1024 * 1024).unwrap();
            let mut reader = RowReader::open(&input, &fingerprint, ProviderKind::Redis).unwrap();
            assert!(
                matches!(reader.next_event().unwrap(), Some(RowEvent::RedisKeyHeader { key_b64, .. }) if key_b64 == STANDARD.encode(&source))
            );
            assert!(
                matches!(reader.next_event().unwrap(), Some(RowEvent::RedisChunk { values, .. }) if values == vec![Value::Bytes(STANDARD.encode("source payload"))])
            );
            assert!(matches!(
                reader.next_event().unwrap(),
                Some(RowEvent::RedisKeyEnd { .. })
            ));
            assert!(reader.next_event().unwrap().is_none());
        }

        let spec: TransferSpec = serde_json::from_value(json!({"mode":"copy","provider":"redis", "source":{"profile_id":profile.id,"redis_db":15}, "destination":{"profile_id":profile.id,"redis_db":15}, "redis":{"keys_b64":[STANDARD.encode(&source)],"source_prefix_b64":STANDARD.encode(&source_prefix),"target_prefix_b64":STANDARD.encode(&target_prefix)}, "options":{"conflict":"overwrite","include_schema":false,"include_data":true}})).unwrap();
        let prepared = migration::prepare(&preparation_sink(&store).await, spec, &cancel)
            .await
            .unwrap();
        let _: i64 = redis::cmd("DEL")
            .arg(&source)
            .query_async(&mut conn)
            .await
            .unwrap();
        let task = store
            .create_task(
                owner,
                "redis-observability",
                "transfer.execute",
                json!({}),
                true,
            )
            .await
            .unwrap();
        let sink = ExecutionSink::new(store.clone(), task.id).with_owner(owner);
        let progress = migration::execute(
            &prepared,
            &sink,
            task.id,
            "redis-observability",
            None,
            &[],
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(progress.skipped, 1);
        let items = store.transfer_items(task.id, None, 10).await.unwrap();
        assert_eq!(items[0].error_code.as_deref(), Some("redis_source_missing"));

        let input = root.join("expired.json");
        let document = json!({"format":"datapier","version":2,"records":[{"key_b64":STANDARD.encode(&target),"type":"string","expires_at_ms":"1","rows":[[{"type":"bytes","value":STANDARD.encode("expired")}]]}]});
        let bytes = serde_json::to_vec(&document).unwrap();
        tokio::fs::write(&input, &bytes).await.unwrap();
        let spec: TransferSpec = serde_json::from_value(json!({"mode":"import","provider":"redis","destination":{"profile_id":profile.id,"redis_db":15},"inputs":[{"path":input,"format":"json"}],"options":{"conflict":"overwrite","include_schema":false,"include_data":true}})).unwrap();
        let prepared = migration::prepare(&preparation_sink(&store).await, spec, &cancel)
            .await
            .unwrap();
        let task = store
            .create_task(
                owner,
                "redis-observability",
                "transfer.execute",
                json!({}),
                true,
            )
            .await
            .unwrap();
        let sink = ExecutionSink::new(store.clone(), task.id).with_owner(owner);
        let progress = migration::execute(
            &prepared,
            &sink,
            task.id,
            "redis-observability",
            None,
            &[],
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(progress.processed_bytes, bytes.len() as u64);
        assert_eq!(progress.skipped, 1);
        let items = store.transfer_items(task.id, None, 10).await.unwrap();
        assert_eq!(
            items[0].error_code.as_deref(),
            Some("redis_expired_before_publish")
        );
        let current: String = redis::cmd("GET")
            .arg(&target)
            .query_async(&mut conn)
            .await
            .unwrap();
        assert_eq!(current, "protected");
    };
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    let _: redis::RedisResult<i64> = redis::cmd("DEL")
        .arg(&source)
        .arg(&target)
        .query_async(&mut conn)
        .await;
    store.pool.close().await;
    let _ = tokio::fs::remove_dir_all(root).await;
    outcome.unwrap();
}
