//! Real Redis integration tests. Run explicitly against a disposable single-node instance.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::{
    ExecutionSink, Store, execution::transfer, model::*, providers::ProviderSession,
};
use futures_util::StreamExt;
use redis::{Client, aio::MultiplexedConnection};
use serde_json::{Value as Json, json};
use std::{path::PathBuf, time::Duration};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Fixture {
    store: Store,
    profile: Profile,
    client: Client,
    raw: MultiplexedConnection,
    session: ProviderSession,
    prefix: String,
    keys: Vec<Vec<u8>>,
}
impl Fixture {
    async fn new() -> Self {
        let port: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")
            .expect("set DATAPIER_TEST_REDIS_PORT to an isolated Redis instance")
            .parse()
            .unwrap();
        let root = std::env::temp_dir().join(format!("datapier-redis-test-{}", Uuid::new_v4()));
        let store = Store::open(&root).await.unwrap();
        let profile = Profile {
            id: Uuid::new_v4(),
            name: "redis-integration".into(),
            provider: ProviderKind::Redis,
            host: "127.0.0.1".into(),
            port,
            username: String::new(),
            database: None,
            redis_db: 15,
            access_mode: AccessMode::ReadWrite,
            tls: TlsConfig::default(),
        };
        let client = Client::open(format!("redis://127.0.0.1:{port}/15")).unwrap();
        let raw = client.get_multiplexed_async_connection().await.unwrap();
        let session = ProviderSession::connect(profile.clone(), None)
            .await
            .unwrap();
        Self {
            store,
            profile,
            client,
            raw,
            session,
            prefix: format!("datapier-test:{}:", Uuid::new_v4()),
            keys: Vec::new(),
        }
    }
    fn target(&mut self, suffix: &[u8], kind: &str) -> ResourceTarget {
        let mut key = self.prefix.as_bytes().to_vec();
        key.extend_from_slice(suffix);
        self.keys.push(key.clone());
        ResourceTarget {
            database: Some("15".into()),
            name: String::from_utf8_lossy(&key).into_owned(),
            key_b64: Some(STANDARD.encode(key)),
            kind: kind.into(),
        }
    }
    async fn sink(&self) -> ExecutionSink {
        let task = self
            .store
            .create_task(
                Uuid::new_v4(),
                "redis-integration",
                "execution.start",
                json!({}),
                false,
            )
            .await
            .unwrap();
        ExecutionSink::new(self.store.clone(), task.id)
    }
    async fn call(&mut self, method: &str, params: Json) -> Result<Json> {
        let sink = self.sink().await;
        self.session
            .call(method, params, &sink, &CancellationToken::new())
            .await
    }
    async fn rows(&mut self, target: &ResourceTarget) -> Vec<Vec<Value>> {
        let response = self
            .call("resource.browse", json!({"target":target}))
            .await
            .unwrap();
        let id: Uuid = serde_json::from_value(response["results"][0].clone()).unwrap();
        self.store
            .result_page(id, 0, 1000, 1024 * 1024)
            .await
            .unwrap()
            .rows
    }
    async fn edit(&mut self, target: &ResourceTarget, changes: Json) {
        let opened = self
            .call("change.open", json!({"target":target}))
            .await
            .unwrap();
        let result = self
            .call(
                "change.submit",
                json!({"target":target,"token":opened["token"],"changes":changes}),
            )
            .await
            .unwrap();
        assert_applied(&result);
    }
    async fn expiry(&mut self, key: &[u8]) -> i64 {
        let script = "local ttl=redis.call('PTTL',KEYS[1]); local t=redis.call('TIME'); return {ttl,t[1],t[2]}";
        let (ttl, sec, micros): (i64, i64, i64) = redis::cmd("EVAL")
            .arg(script)
            .arg(1)
            .arg(key)
            .query_async(&mut self.raw)
            .await
            .unwrap();
        assert!(ttl > 0, "key must retain a finite TTL, got {ttl}");
        sec * 1000 + micros / 1000 + ttl
    }
    async fn close(mut self) {
        if !self.keys.is_empty() {
            let _: i64 = redis::cmd("DEL")
                .arg(&self.keys)
                .query_async(&mut self.raw)
                .await
                .unwrap();
        }
        self.session.close().await.unwrap();
        drop(self.raw);
        drop(self.client);
        let root = (*self.store.root).clone();
        self.store.pool.close().await;
        self.store.cache.close().await;
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
fn raw_key(target: &ResourceTarget) -> Vec<u8> {
    STANDARD.decode(target.key_b64.as_ref().unwrap()).unwrap()
}
fn binary(bytes: &[u8]) -> Value {
    Value::Bytes(STANDARD.encode(bytes))
}
fn assert_applied(result: &Json) {
    let items = result["items"].as_array().unwrap();
    assert!(!items.is_empty());
    assert!(
        items.iter().all(|item| item["status"] == "applied"),
        "{result}"
    );
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis 6.2/7.2/8.0 instance"]
async fn all_five_types_browse_reversible_binary_values() {
    let mut f = Fixture::new().await;
    let input = [0, 255, b'\n', 0xc3, 0xa9];
    let field = [255, 0, b'f'];
    let member = [0, 254, b'm'];
    let string = f.target(b"string\0\xff", "string");
    let sk = raw_key(&string);
    let _: () = redis::cmd("SET")
        .arg(&sk)
        .arg(&input[..])
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(f.rows(&string).await, vec![vec![binary(&input)]]);
    f.edit(
        &string,
        json!([{"operation":"set_string","value":binary(b"\xff\0next")}]),
    )
    .await;
    let reread: Vec<u8> = redis::cmd("GET")
        .arg(&sk)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(reread, b"\xff\0next");
    let hash = f.target(b"hash", "hash");
    let hk = raw_key(&hash);
    let _: i64 = redis::cmd("HSET")
        .arg(&hk)
        .arg(&field[..])
        .arg(&input[..])
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(
        f.rows(&hash).await,
        vec![vec![binary(&field), binary(&input)]]
    );
    f.edit(
        &hash,
        json!([{"operation":"hash_set","field":binary(&field),"value":binary(b"\xff\0next")}]),
    )
    .await;
    let reread: Vec<u8> = redis::cmd("HGET")
        .arg(&hk)
        .arg(&field[..])
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(reread, b"\xff\0next");
    let list = f.target(b"list", "list");
    let lk = raw_key(&list);
    let _: i64 = redis::cmd("RPUSH")
        .arg(&lk)
        .arg(&input[..])
        .arg(&input[..])
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(
        f.rows(&list).await,
        vec![
            vec![Value::Integer("0".into()), binary(&input)],
            vec![Value::Integer("1".into()), binary(&input)]
        ]
    );
    f.edit(
        &list,
        json!([{"operation":"list_set","index":1,"value":binary(b"\xff\0next")}]),
    )
    .await;
    let reread: Vec<Vec<u8>> = redis::cmd("LRANGE")
        .arg(&lk)
        .arg(0)
        .arg(-1)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(reread, vec![input.to_vec(), b"\xff\0next".to_vec()]);
    let set = f.target(b"set", "set");
    let sek = raw_key(&set);
    let _: i64 = redis::cmd("SADD")
        .arg(&sek)
        .arg(&member[..])
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(f.rows(&set).await, vec![vec![binary(&member)]]);
    f.edit(
        &set,
        json!([{"operation":"set_add","member":binary(b"\xff\0next")}]),
    )
    .await;
    let present: i64 = redis::cmd("SISMEMBER")
        .arg(&sek)
        .arg(b"\xff\0next".as_slice())
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(present, 1);
    let zset = f.target(b"zset", "zset");
    let zk = raw_key(&zset);
    let _: i64 = redis::cmd("ZADD")
        .arg(&zk)
        .arg("1.25")
        .arg(&member[..])
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(
        f.rows(&zset).await,
        vec![vec![binary(&member), Value::Float("1.25".into())]]
    );
    f.edit(
        &zset,
        json!([{"operation":"zset_set","member":binary(b"\xff\0next"),"score":"-12.5"}]),
    )
    .await;
    let score: String = redis::cmd("ZSCORE")
        .arg(&zk)
        .arg(b"\xff\0next".as_slice())
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(score.parse::<f64>().unwrap(), -12.5);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis 6.2/7.2/8.0 instance"]
async fn last_element_replacement_preserves_absolute_expiry_for_all_collections() {
    let mut f = Fixture::new().await;
    for kind in ["hash", "set", "zset"] {
        let target = f.target(kind.as_bytes(), kind);
        let key = raw_key(&target);
        match kind {
            "hash" => {
                let _: i64 = redis::cmd("HSET")
                    .arg(&key)
                    .arg(b"old\0".as_slice())
                    .arg(b"oldvalue".as_slice())
                    .query_async(&mut f.raw)
                    .await
                    .unwrap();
            }
            "set" => {
                let _: i64 = redis::cmd("SADD")
                    .arg(&key)
                    .arg(b"old\0".as_slice())
                    .query_async(&mut f.raw)
                    .await
                    .unwrap();
            }
            _ => {
                let _: i64 = redis::cmd("ZADD")
                    .arg(&key)
                    .arg("1")
                    .arg(b"old\0".as_slice())
                    .query_async(&mut f.raw)
                    .await
                    .unwrap();
            }
        }
        let _: i64 = redis::cmd("PEXPIRE")
            .arg(&key)
            .arg(120000)
            .query_async(&mut f.raw)
            .await
            .unwrap();
        let baseline = f.expiry(&key).await;
        let opened = f
            .call("change.open", json!({"target":target}))
            .await
            .unwrap();
        let old = binary(b"old\0");
        let new = binary(b"new\xff");
        let changes = match kind {
            "hash" => {
                json!([{"operation":"hash_delete","field":old},{"operation":"hash_set","field":new,"value":binary(b"new\0value")}])
            }
            "set" => {
                json!([{"operation":"set_remove","member":old},{"operation":"set_add","member":new}])
            }
            _ => {
                json!([{"operation":"zset_remove","member":old},{"operation":"zset_set","member":new,"score":"2.5"}])
            }
        };
        let submitted = f
            .call(
                "change.submit",
                json!({"target":target,"token":opened["token"],"changes":changes}),
            )
            .await
            .unwrap();
        assert_applied(&submitted);
        let after = f.expiry(&key).await;
        assert!(
            (after - baseline).abs() <= 4,
            "{kind}: expiration moved from {baseline} to {after}"
        );
        let rows = f.rows(&target).await;
        match kind {
            "hash" => assert_eq!(rows, vec![vec![binary(b"new\xff"), binary(b"new\0value")]]),
            "set" => assert_eq!(rows, vec![vec![binary(b"new\xff")]]),
            _ => assert_eq!(
                rows,
                vec![vec![binary(b"new\xff"), Value::Float("2.5".into())]]
            ),
        }
    }
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis 6.2/7.2/8.0 instance"]
async fn rename_is_non_overwriting_and_retains_absolute_expiry() {
    let mut f = Fixture::new().await;
    let source = f.target(b"rename-source", "string");
    let occupied = f.target(b"rename-occupied", "string");
    let destination = f.target(b"rename-destination", "string");
    let src = raw_key(&source);
    let dst = raw_key(&destination);
    let taken = raw_key(&occupied);
    let _: () = redis::cmd("SET")
        .arg(&src)
        .arg(b"source\0".as_slice())
        .arg("PX")
        .arg(120000)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let _: () = redis::cmd("SET")
        .arg(&taken)
        .arg("untouched")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let baseline = f.expiry(&src).await;
    let edit = f
        .call("change.open", json!({"target":source}))
        .await
        .unwrap();
    let error=f.call("change.submit",json!({"target":source,"token":edit["token"],"changes":[{"operation":"rename","new_key_b64":occupied.key_b64}]})).await.unwrap_err();
    assert_eq!(error.code, "conflict");
    let preserved: Vec<u8> = redis::cmd("GET")
        .arg(&taken)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(preserved, b"untouched");
    let edit = f
        .call("change.open", json!({"target":source}))
        .await
        .unwrap();
    let result=f.call("change.submit",json!({"target":source,"token":edit["token"],"changes":[{"operation":"rename","new_key_b64":destination.key_b64}]})).await.unwrap();
    assert_applied(&result);
    let missing: i64 = redis::cmd("EXISTS")
        .arg(&src)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(missing, 0);
    let value: Vec<u8> = redis::cmd("GET")
        .arg(&dst)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(value, b"source\0");
    let after = f.expiry(&dst).await;
    assert!(
        (after - baseline).abs() <= 4,
        "rename changed absolute expiry"
    );
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis 6.2/7.2/8.0 instance"]
async fn expiration_invalidates_watch_and_read_only_rejects_writes() {
    let mut f = Fixture::new().await;
    let target = f.target(b"expiring", "string");
    let key = raw_key(&target);
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg("before")
        .arg("PX")
        .arg(600)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let edit = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let exists: i64 = redis::cmd("EXISTS")
                .arg(&key)
                .query_async(&mut f.raw)
                .await
                .unwrap();
            if exists == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let error=f.call("change.submit",json!({"target":target,"token":edit["token"],"changes":[{"operation":"set_string","value":binary(b"after")}]})).await.unwrap_err();
    assert_eq!(error.code, "conflict");
    let mut read_only = f.profile.clone();
    read_only.access_mode = AccessMode::ReadOnly;
    let mut session = ProviderSession::connect(read_only, None).await.unwrap();
    let sink = f.sink().await;
    let cmd = format!("SET {} forbidden", String::from_utf8(key.clone()).unwrap());
    let error = session
        .call(
            "execution.run",
            json!({"text":cmd}),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "permission_denied");
    let exists: i64 = redis::cmd("EXISTS")
        .arg(&key)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(exists, 0);
    session.close().await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis 6.2/7.2/8.0 instance"]
async fn export_does_not_publish_when_key_type_changes_during_browse() {
    let mut f = Fixture::new().await;
    let target = f.target(b"race", "string");
    let key = raw_key(&target);
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg("original")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let mut monitor = f.client.get_async_monitor().await.unwrap();
    let sink = f.sink().await;
    let mut lock = f.store.cache.acquire().await.unwrap();
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let path: PathBuf = f.store.root.join("changed-type.json");
    let export_path = path.clone();
    let export_target = target.clone();
    let mut exporting = ProviderSession::connect(f.profile.clone(), None)
        .await
        .unwrap();
    let job = tokio::spawn(async move {
        let result = transfer::run(
            &mut exporting,
            "transfer.export",
            json!({"format":"json","path":export_path,"target":export_target}),
            &sink,
            &CancellationToken::new(),
        )
        .await;
        (result, exporting)
    });
    let mut messages = monitor.on_message::<String>();
    let key_text = String::from_utf8(key.clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = messages.next().await.expect("monitor disconnected");
            if line.contains("\"PTTL\"") && line.contains(&key_text) && !line.contains(" lua]") {
                break;
            }
        }
    })
    .await
    .expect("browse did not reach PTTL");
    let _: () = redis::pipe()
        .atomic()
        .cmd("DEL")
        .arg(&key)
        .ignore()
        .cmd("HSET")
        .arg(&key)
        .arg("field")
        .arg("value")
        .ignore()
        .query_async(&mut f.raw)
        .await
        .unwrap();
    sqlx::query("ROLLBACK").execute(&mut *lock).await.unwrap();
    drop(lock);
    let (result, mut exporting) = tokio::time::timeout(Duration::from_secs(10), job)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err(), "changed key type must fail export");
    assert!(!path.exists(), "failed export must not publish final file");
    exporting.close().await.unwrap();
    drop(messages);
    drop(monitor);
    f.close().await;
}
