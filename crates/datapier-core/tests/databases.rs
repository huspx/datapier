//! 真实数据库回归。只连接显式环境变量指定的隔离测试实例。
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bigdecimal::BigDecimal;
use datapier_core::{ExecutionSink, Store, model::*, providers::ProviderSession};
use serde_json::{Value as Json, json};
use std::{collections::BTreeMap, str::FromStr};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn context(kind: ProviderKind) -> (Store, Profile, Option<String>) {
    let root = std::env::temp_dir().join(format!("datapier-database-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let (port, secret) = match kind {
        ProviderKind::Mysql => (
            std::env::var("DATAPIER_TEST_MYSQL_PORT").expect("设置隔离MySQL端口"),
            Some(std::env::var("DATAPIER_TEST_MYSQL_SECRET").expect("设置测试MySQL凭据")),
        ),
        ProviderKind::Redis => (
            std::env::var("DATAPIER_TEST_REDIS_PORT").expect("设置隔离Redis端口"),
            None,
        ),
    };
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "isolated-fixture".into(),
        provider: kind,
        host: "127.0.0.1".into(),
        port: port.parse().unwrap(),
        username: if kind == ProviderKind::Mysql {
            "datapier".into()
        } else {
            String::new()
        },
        database: if kind == ProviderKind::Mysql {
            Some("datapier_fixture".into())
        } else {
            None
        },
        redis_db: 15,
        access_mode: AccessMode::ReadWrite,
        tls: TlsConfig::default(),
        visible_databases: None,
    };
    (store, profile, secret)
}
async fn sink(store: &Store) -> ExecutionSink {
    let task = store
        .create_task(
            Uuid::new_v4(),
            "fixture",
            "execution.start",
            json!({}),
            false,
        )
        .await
        .unwrap();
    ExecutionSink::new(store.clone(), task.id)
}
async fn call(
    session: &mut ProviderSession,
    store: &Store,
    method: &str,
    params: Json,
) -> Result<Json> {
    session
        .call(
            method,
            params,
            &sink(store).await,
            &CancellationToken::new(),
        )
        .await
}
async fn execute(session: &mut ProviderSession, store: &Store, text: &str) -> Result<Json> {
    call(session, store, "execution.run", json!({"text":text})).await
}
async fn result_rows(store: &Store, result: &Json) -> Vec<Vec<Value>> {
    let id: Uuid = serde_json::from_value(result["results"][0].clone()).unwrap();
    store
        .result_page(id, 0, 1000, 1024 * 1024)
        .await
        .unwrap()
        .rows
}
async fn clean(store: Store) {
    let root = (*store.root).clone();
    store.pool.close().await;
    store.cache.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
#[ignore = "需要隔离的 MySQL 5.7、8.0 或 8.4 实例"]
async fn mysql_preserves_values_empty_columns_and_conflicting_edits() {
    let (store, profile, secret) = context(ProviderKind::Mysql).await;
    let mut db = ProviderSession::connect(profile, secret).await.unwrap();
    let table = format!("dp_{}", Uuid::new_v4().simple());
    let decimal = "999999999999999999999999999999.1234567890";
    execute(&mut db, &store, &format!("CREATE TABLE `{table}` (id BIGINT UNSIGNED PRIMARY KEY, amount DECIMAL(40,10), label VARCHAR(100), payload VARBINARY(100), optional TEXT NULL) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4")).await.unwrap();
    execute(
        &mut db,
        &store,
        &format!(
            "INSERT INTO `{table}` VALUES(18446744073709551615,{decimal},'中😀',X'00FF005C',NULL)"
        ),
    )
    .await
    .unwrap();
    let queried = execute(&mut db, &store, &format!("SELECT * FROM `{table}`"))
        .await
        .unwrap();
    let row = result_rows(&store, &queried).await.remove(0);
    assert_eq!(row[0].text(), "18446744073709551615");
    match &row[1] {
        Value::Decimal(value) => assert_eq!(
            BigDecimal::from_str(value).unwrap(),
            BigDecimal::from_str(decimal).unwrap()
        ),
        other => panic!("精确小数被改变类型: {other:?}"),
    }
    assert_eq!(row[2], Value::Text("中😀".into()));
    assert_eq!(row[3], Value::Bytes(STANDARD.encode([0, 255, 0, 92])));
    assert_eq!(row[4], Value::Null);
    let empty = execute(
        &mut db,
        &store,
        &format!("SELECT * FROM `{table}` WHERE FALSE"),
    )
    .await
    .unwrap();
    let empty_id: Uuid = serde_json::from_value(empty["results"][0].clone()).unwrap();
    let page = store
        .result_page(empty_id, 0, 200, 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(
        page.result
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        vec!["id", "amount", "label", "payload", "optional"]
    );
    assert!(page.rows.is_empty());
    let multiple = execute(
        &mut db,
        &store,
        "SELECT 1 AS one; SELECT 2 AS empty_set FROM DUAL WHERE FALSE; SELECT 3 AS three",
    )
    .await
    .unwrap();
    let ids: Vec<Uuid> = serde_json::from_value(multiple["results"].clone()).unwrap();
    assert_eq!(ids.len(), 3);
    assert_eq!(
        store.result_meta(ids[1]).await.unwrap().columns[0].name,
        "empty_set"
    );
    let target = ResourceTarget {
        database: Some("datapier_fixture".into()),
        name: table.clone(),
        key_b64: None,
        kind: "table".into(),
    };
    let schema: TableSchema = serde_json::from_value(
        call(
            &mut db,
            &store,
            "resource.describe",
            json!({"target":target}),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let opened = call(&mut db, &store, "change.open", json!({"target":target}))
        .await
        .unwrap();
    let before: BTreeMap<String, Value> = schema
        .columns
        .iter()
        .zip(row.iter())
        .map(|(c, v)| (c.name.clone(), v.clone()))
        .collect();
    let change = RowChange {
        operation: "update".into(),
        key: BTreeMap::from([("id".into(), row[0].clone())]),
        before,
        values: BTreeMap::from([("label".into(), Value::Text("更新😀".into()))]),
    };
    let changed = call(
        &mut db,
        &store,
        "change.submit",
        json!({"target":target,"token":opened["token"],"rows":[change]}),
    )
    .await
    .unwrap();
    assert_eq!(changed["items"][0]["status"], "applied");
    let current = execute(&mut db, &store, &format!("SELECT label FROM `{table}`"))
        .await
        .unwrap();
    assert_eq!(
        result_rows(&store, &current).await[0][0],
        Value::Text("更新😀".into())
    );
    let stale_opened = call(&mut db, &store, "change.open", json!({"target":target}))
        .await
        .unwrap();
    let stale = call(
        &mut db,
        &store,
        "change.submit",
        json!({"target":target,"token":stale_opened["token"],"rows":[change]}),
    )
    .await
    .unwrap_err();
    assert_eq!(stale.code, "conflict");
    execute(&mut db, &store, &format!("DROP TABLE `{table}`"))
        .await
        .unwrap();
    db.close().await.unwrap();
    clean(store).await;
}

#[tokio::test]
#[ignore = "需要隔离的 MySQL，验证真实 KILL QUERY"]
async fn mysql_cancel_does_not_kill_the_next_query() {
    let (store, profile, secret) = context(ProviderKind::Mysql).await;
    let mut db = ProviderSession::connect(profile, secret).await.unwrap();
    execute(
        &mut db,
        &store,
        "CREATE TEMPORARY TABLE dp_cancel (value INT); INSERT INTO dp_cancel VALUES (1)",
    )
    .await
    .unwrap();
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        signal.cancel();
    });
    let interrupted = db
        .call(
            "execution.run",
            json!({"text":"SELECT 1 FROM dp_cancel WHERE SLEEP(15)"}),
            &sink(&store).await,
            &cancel,
        )
        .await
        .unwrap_err();
    assert!(
        matches!(interrupted.code.as_str(), "cancelled" | "outcome_unknown"),
        "{interrupted}"
    );
    let after = execute(&mut db, &store, "SELECT 42 AS alive")
        .await
        .unwrap();
    assert_eq!(result_rows(&store, &after).await[0][0].text(), "42");
    db.close().await.unwrap();
    clean(store).await;
}

#[tokio::test]
#[ignore = "需要隔离的 Redis 6.2/7.2/8.0 实例"]
async fn redis_watch_preserves_ttl_and_list_index_identity() {
    let (store, profile, secret) = context(ProviderKind::Redis).await;
    let mut db = ProviderSession::connect(profile.clone(), secret.clone())
        .await
        .unwrap();
    let mut other = ProviderSession::connect(profile, secret).await.unwrap();
    let key = format!("datapier:{}", Uuid::new_v4());
    let target = ResourceTarget {
        database: None,
        name: key.clone(),
        key_b64: Some(STANDARD.encode(key.as_bytes())),
        kind: "list".into(),
    };
    execute(
        &mut db,
        &store,
        &format!("RPUSH {key} same target same\nPEXPIRE {key} 60000"),
    )
    .await
    .unwrap();
    let opened = call(&mut db, &store, "change.open", json!({"target":target}))
        .await
        .unwrap();
    let before = call(
        &mut db,
        &store,
        "resource.describe",
        json!({"target":target}),
    )
    .await
    .unwrap();
    let result = call(&mut db,&store,"change.submit",json!({"target":target,"token":opened["token"],"changes":[{"operation":"list_delete","index":2}]})).await.unwrap();
    assert!(
        result["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|x| x["status"] == "applied")
    );
    let browsed = call(&mut db, &store, "resource.browse", json!({"target":target}))
        .await
        .unwrap();
    let rows = result_rows(&store, &browsed).await;
    assert_eq!(
        rows.iter().map(|r| r[1].clone()).collect::<Vec<_>>(),
        vec![
            Value::Bytes(STANDARD.encode("same")),
            Value::Bytes(STANDARD.encode("target"))
        ]
    );
    let after = call(
        &mut db,
        &store,
        "resource.describe",
        json!({"target":target}),
    )
    .await
    .unwrap();
    let ttl_before = before["ttl_ms"].as_i64().unwrap();
    let ttl_after = after["ttl_ms"].as_i64().unwrap();
    assert!(
        ttl_after > 0 && ttl_after <= ttl_before,
        "TTL 必须保留而不是重置"
    );
    let edit = call(&mut db, &store, "change.open", json!({"target":target}))
        .await
        .unwrap();
    execute(&mut other, &store, &format!("LPUSH {key} same"))
        .await
        .unwrap();
    let conflict = call(&mut db,&store,"change.submit",json!({"target":target,"token":edit["token"],"changes":[{"operation":"list_delete","index":0}]})).await.unwrap_err();
    assert_eq!(conflict.code, "conflict");
    let still = call(&mut db, &store, "resource.browse", json!({"target":target}))
        .await
        .unwrap();
    assert_eq!(result_rows(&store, &still).await.len(), 3);
    execute(&mut db, &store, &format!("DEL {key}"))
        .await
        .unwrap();
    db.close().await.unwrap();
    other.close().await.unwrap();
    clean(store).await;
}
