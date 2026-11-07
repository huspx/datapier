//! Real Redis integration tests. Run explicitly against a disposable single-node instance.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::{
    ExecutionSink, Store, execution::transfer, model::*, providers::ProviderSession,
};
use futures_util::FutureExt;
use redis::{Client, aio::MultiplexedConnection};
use serde_json::{Value as Json, json};
use std::time::Duration;
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
            revision: 1,
            approval_policy: ApprovalPolicy::Dangerous,
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
            }),
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
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis instance"]
async fn export_does_not_publish_when_scan_exceeds_memory_budget() {
    let mut f = Fixture::new().await;
    let mut settings = f.store.settings().await.unwrap();
    settings.limits.result_memory_bytes_per_client = 1024 * 1024;
    f.store.set_settings(&settings).await.unwrap();
    let mut pipe = redis::pipe();
    for index in 0..160 {
        let key = f.target(
            format!("{index:04}{}", "x".repeat(8192)).as_bytes(),
            "string",
        );
        pipe.cmd("SET").arg(raw_key(&key)).arg("value").ignore();
    }
    pipe.query_async::<()>(&mut f.raw).await.unwrap();
    let sink = f.sink().await;
    let path = f.store.root.join("limited-export.json");
    let result = transfer::run(
        &mut f.session,
        "transfer.export",
        json!({"format":"json","path":path,"pattern":format!("{}*", f.prefix)}),
        &sink,
        &CancellationToken::new(),
    )
    .await;
    assert_eq!(result.unwrap_err().code, "resource_limit");
    assert!(!path.exists(), "受限导出不能发布为完整文件");
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis 6.2/7.2/8.0 instance"]
async fn import_replacement_never_mutates_or_deletes_external_key() {
    let mut f = Fixture::new().await;
    for kind in ["string", "hash", "list", "set", "zset"] {
        for action in ["append", "finish", "discard", "expired"] {
            let target = f.target(format!("{kind}-{action}").as_bytes(), kind);
            let expiry = if action == "expired" {
                json!("1")
            } else {
                Json::Null
            };
            let begin = f
                .call(
                    "import.begin",
                    json!({"target":target,"type":kind,"expires_at_ms":expiry}),
                )
                .await
                .unwrap();
            let token = begin["token"].clone();
            let temp = format!("__datapier_import_{}", token.as_str().unwrap()).into_bytes();
            f.keys.push(temp.clone());
            let row = match kind {
                "string" | "set" => json!([binary(b"first\0value")]),
                "hash" => json!([binary(b"field"), binary(b"first\0value")]),
                "list" => json!([Value::Unsigned("0".into()), binary(b"first\0value")]),
                _ => json!([binary(b"member"), Value::Float("1.5".into())]),
            };
            if action != "append" {
                f.call("import.append", json!({"token":token,"rows":[row.clone()]}))
                    .await
                    .unwrap();
            }
            let mut replacement = redis::pipe();
            replacement.atomic().cmd("DEL").arg(&temp).ignore();
            match kind {
                "string" => {
                    replacement
                        .cmd("SET")
                        .arg(&temp)
                        .arg(b"external\0value")
                        .ignore();
                }
                "hash" => {
                    replacement
                        .cmd("HSET")
                        .arg(&temp)
                        .arg(b"external")
                        .arg(b"value")
                        .ignore();
                }
                "list" => {
                    replacement
                        .cmd("RPUSH")
                        .arg(&temp)
                        .arg(b"external\0value")
                        .ignore();
                }
                "set" => {
                    replacement
                        .cmd("SADD")
                        .arg(&temp)
                        .arg(b"external\0value")
                        .ignore();
                }
                _ => {
                    replacement
                        .cmd("ZADD")
                        .arg(&temp)
                        .arg("2")
                        .arg(b"external\0value")
                        .ignore();
                }
            }
            let _: () = replacement.query_async(&mut f.raw).await.unwrap();
            let before: Vec<u8> = redis::cmd("DUMP")
                .arg(&temp)
                .query_async(&mut f.raw)
                .await
                .unwrap();
            match action {
                "append" => assert!(
                    f.call("import.append", json!({"token":token,"rows":[row]}))
                        .await
                        .is_err()
                ),
                "finish" | "expired" => assert!(
                    f.call("import.finish", json!({"token":token}))
                        .await
                        .is_err()
                ),
                _ => {
                    f.call("import.discard", json!({"token":token}))
                        .await
                        .unwrap();
                }
            }
            if action != "discard" {
                f.call("import.discard", json!({"token":token}))
                    .await
                    .unwrap();
            }
            assert!(
                f.call("import.discard", json!({"token":token}))
                    .await
                    .is_err()
            );
            let after: Vec<u8> = redis::cmd("DUMP")
                .arg(&temp)
                .query_async(&mut f.raw)
                .await
                .unwrap();
            assert_eq!(after, before, "{kind} {action}");
            let exists: i64 = redis::cmd("EXISTS")
                .arg(raw_key(&target))
                .query_async(&mut f.raw)
                .await
                .unwrap();
            assert_eq!(exists, 0);
        }
    }
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis 6.2/7.2/8.0 instance"]
async fn import_preserves_values_and_removes_owned_staging_keys() {
    let mut f = Fixture::new().await;
    for kind in ["string", "hash", "list", "set", "zset"] {
        let target = f.target(format!("normal-{kind}").as_bytes(), kind);
        let begin = f
            .call("import.begin", json!({"target":target,"type":kind}))
            .await
            .unwrap();
        let token = begin["token"].clone();
        let temp = format!("__datapier_import_{}", token.as_str().unwrap()).into_bytes();
        f.keys.push(temp.clone());
        // 取得真实标记作为输入，验证集合元素与标记碰撞时仍完整保留。
        let marker: Vec<u8> = match kind {
            "hash" => redis::cmd("HKEYS")
                .arg(&temp)
                .query_async::<Vec<Vec<u8>>>(&mut f.raw)
                .await
                .unwrap()
                .remove(0),
            "set" => redis::cmd("SMEMBERS")
                .arg(&temp)
                .query_async::<Vec<Vec<u8>>>(&mut f.raw)
                .await
                .unwrap()
                .remove(0),
            "zset" => redis::cmd("ZRANGE")
                .arg(&temp)
                .arg(0)
                .arg(-1)
                .query_async::<Vec<Vec<u8>>>(&mut f.raw)
                .await
                .unwrap()
                .remove(0),
            _ => Vec::new(),
        };
        let row = match kind {
            "string" => json!([binary(b"binary\0value")]),
            "hash" => json!([binary(&marker), binary(b"binary\0value")]),
            "list" => json!([Value::Unsigned("0".into()), binary(b"binary\0value")]),
            "set" => json!([binary(&marker)]),
            _ => json!([binary(&marker), Value::Float("1.5".into())]),
        };
        f.call("import.append", json!({"token":token,"rows":[row.clone()]}))
            .await
            .unwrap();
        f.call("import.finish", json!({"token":token}))
            .await
            .unwrap();
        let key = raw_key(&target);
        let actual: Vec<u8> = match kind {
            "string" => redis::cmd("GET")
                .arg(&key)
                .query_async(&mut f.raw)
                .await
                .unwrap(),
            "hash" => redis::cmd("HGET")
                .arg(&key)
                .arg(&marker)
                .query_async(&mut f.raw)
                .await
                .unwrap(),
            "list" => redis::cmd("LINDEX")
                .arg(&key)
                .arg(0)
                .query_async(&mut f.raw)
                .await
                .unwrap(),
            "set" => redis::cmd("SMEMBERS")
                .arg(&key)
                .query_async::<Vec<Vec<u8>>>(&mut f.raw)
                .await
                .unwrap()
                .remove(0),
            _ => redis::cmd("ZRANGE")
                .arg(&key)
                .arg(0)
                .arg(-1)
                .query_async::<Vec<Vec<u8>>>(&mut f.raw)
                .await
                .unwrap()
                .remove(0),
        };
        assert_eq!(
            actual,
            if kind == "set" || kind == "zset" {
                marker
            } else {
                b"binary\0value".to_vec()
            }
        );
        let exists: i64 = redis::cmd("EXISTS")
            .arg(&temp)
            .query_async(&mut f.raw)
            .await
            .unwrap();
        assert_eq!(exists, 0);
        let cancel_target = f.target(format!("cancel-{kind}").as_bytes(), kind);
        let cancel = f
            .call("import.begin", json!({"target":cancel_target,"type":kind}))
            .await
            .unwrap();
        let cancel_temp =
            format!("__datapier_import_{}", cancel["token"].as_str().unwrap()).into_bytes();
        f.keys.push(cancel_temp.clone());
        f.call(
            "import.append",
            json!({"token":cancel["token"],"rows":[row]}),
        )
        .await
        .unwrap();
        f.call("import.discard", json!({"token":cancel["token"]}))
            .await
            .unwrap();
        let exists: i64 = redis::cmd("EXISTS")
            .arg(&cancel_temp)
            .query_async(&mut f.raw)
            .await
            .unwrap();
        assert_eq!(exists, 0);
    }
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis 6.2/7.2/8.0 instance"]
async fn import_close_preserves_replacement_and_cleans_owned_key() {
    let mut f = Fixture::new().await;
    let mut session = ProviderSession::connect(f.profile.clone(), None)
        .await
        .unwrap();
    let sink = f.sink().await;
    let cancel = CancellationToken::new();
    let mut replaced = Vec::new();
    let mut owned = Vec::new();
    for kind in ["string", "hash", "list", "set", "zset"] {
        for replace in [false, true] {
            let target = f.target(format!("close-{kind}-{replace}").as_bytes(), kind);
            let begin = session
                .call(
                    "import.begin",
                    json!({"target":target,"type":kind}),
                    &sink,
                    &cancel,
                )
                .await
                .unwrap();
            let token = begin["token"].clone();
            let temp = format!("__datapier_import_{}", token.as_str().unwrap()).into_bytes();
            f.keys.push(temp.clone());
            let row = match kind {
                "string" | "set" => json!([binary(b"initial")]),
                "hash" => json!([binary(b"field"), binary(b"initial")]),
                "list" => json!([Value::Unsigned("0".into()), binary(b"initial")]),
                _ => json!([binary(b"member"), Value::Float("1".into())]),
            };
            session
                .call(
                    "import.append",
                    json!({"token":token,"rows":[row]}),
                    &sink,
                    &cancel,
                )
                .await
                .unwrap();
            if replace {
                let _: () = redis::pipe()
                    .atomic()
                    .cmd("DEL")
                    .arg(&temp)
                    .ignore()
                    .cmd("SET")
                    .arg(&temp)
                    .arg(b"external")
                    .ignore()
                    .query_async(&mut f.raw)
                    .await
                    .unwrap();
                replaced.push(temp);
            } else {
                owned.push(temp);
            }
        }
    }
    session.close().await.unwrap();
    for key in replaced {
        let value: Vec<u8> = redis::cmd("GET")
            .arg(key)
            .query_async(&mut f.raw)
            .await
            .unwrap();
        assert_eq!(value, b"external");
    }
    for key in owned {
        let exists: i64 = redis::cmd("EXISTS")
            .arg(key)
            .query_async(&mut f.raw)
            .await
            .unwrap();
        assert_eq!(exists, 0);
    }
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis 6.2/7.2/8.0 instance"]
async fn preview_preserves_watch_token_and_absolute_ttl() {
    let mut f = Fixture::new().await;
    let target = f.target(b"preview\xff", "string");
    let key = raw_key(&target);
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg("before")
        .arg("PX")
        .arg(60000)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let expiry = f.expiry(&key).await;
    let opened = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    let params = json!({"target":target,"token":opened["token"],"changes":[{"operation":"set_string","value":binary(b"\xff\0new")}]});
    let preview = f.call("change.preview", params.clone()).await.unwrap();
    assert_eq!(preview["items"][0]["statements"][0]["text"], "SET");
    assert_eq!(
        preview["items"][0]["statements"][0]["parameters"][1]["kind"],
        "bytes"
    );
    assert_eq!(
        preview["items"][0]["statements"][0]["parameters"][1]["display"],
        STANDARD.encode(b"\xff\0new")
    );
    assert_eq!(preview["items"][1]["operation"], "ttl");
    let value: Vec<u8> = redis::cmd("GET")
        .arg(&key)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(value, b"before");
    assert!((f.expiry(&key).await - expiry).abs() <= 4);
    assert_applied(&f.call("change.submit", params).await.unwrap());
    assert!((f.expiry(&key).await - expiry).abs() <= 4);
    let opened = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    let params = json!({"target":target,"token":opened["token"],"changes":[{"operation":"set_string","value":binary(b"next")}]});
    f.call("change.preview", params.clone()).await.unwrap();
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg("external")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(
        f.call("change.submit", params).await.unwrap_err().code,
        "conflict"
    );
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis 6.2/7.2/8.0 instance"]
async fn preview_list_dynamic_marker_pagination_ttl_and_exec_errors() {
    let mut f = Fixture::new().await;
    let target = f.target(b"preview-list", "list");
    let key = raw_key(&target);
    let _: i64 = redis::cmd("RPUSH")
        .arg(&key)
        .arg("same")
        .arg("same")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let opened = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    let changes = json!([{"operation":"list_delete","index":1},{"operation":"list_set","index":99,"value":binary(b"x")},{"operation":"set_ttl","ttl_ms":50000},{"operation":"set_ttl","ttl_ms":60000}]);
    let params = json!({"target":target,"token":opened["token"],"changes":changes,"limit":2});
    let first = f.call("change.preview", params.clone()).await.unwrap();
    assert_eq!(first["next_offset"], 2);
    let marker = &first["items"][0]["statements"][0]["parameters"][4];
    assert_eq!(marker["dynamic"], true);
    assert_eq!(marker["complete"], false);
    let mut last_params = params.clone();
    last_params["offset"] = json!(2);
    let last = f.call("change.preview", last_params).await.unwrap();
    assert!(last["next_offset"].is_null());
    assert_eq!(last["items"][2]["index"], Json::Null);
    let ttl = &last["items"][2]["statements"][0]["parameters"];
    assert_eq!(ttl[5]["display"], "\"60000\"");
    let result = f.call("change.submit", params).await.unwrap();
    assert_eq!(result["items"][0]["status"], "applied");
    assert_eq!(result["items"][1]["status"], "error");
    let values: Vec<Vec<u8>> = redis::cmd("LRANGE")
        .arg(&key)
        .arg(0)
        .arg(-1)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(values, vec![b"same".to_vec()]);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis 6.2/7.2/8.0 instance"]
async fn preview_rename_defers_destination_check_until_submit() {
    let mut f = Fixture::new().await;
    let source = f.target(b"preview-rename-source", "string");
    let destination = f.target(b"preview-rename-destination", "string");
    let src = raw_key(&source);
    let dst = raw_key(&destination);
    let _: () = redis::cmd("SET")
        .arg(&src)
        .arg("source")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let _: () = redis::cmd("SET")
        .arg(&dst)
        .arg("occupied")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let opened = f
        .call("change.open", json!({"target":source}))
        .await
        .unwrap();
    let params = json!({"target":source,"token":opened["token"],"changes":[{"operation":"rename","new_key_b64":destination.key_b64}]});
    let preview = f.call("change.preview", params.clone()).await.unwrap();
    assert_eq!(preview["items"][0]["statements"][0]["text"], "RENAMENX");
    assert_eq!(
        f.call("change.submit", params).await.unwrap_err().code,
        "conflict"
    );
    let value: String = redis::cmd("GET")
        .arg(&dst)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(value, "occupied");
    f.close().await;
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 和隔离的 Redis 实例"]
async fn batch_delete_preserves_binary_identity_and_watch_contract() {
    let mut f = Fixture::new().await;
    let a = f.target(b"batch\0\xff", "string");
    let b = f.target(b"batch-second", "string");
    let keys = [raw_key(&a), raw_key(&b)];
    for key in &keys {
        let _: () = redis::cmd("SET")
            .arg(key)
            .arg("value")
            .query_async(&mut f.raw)
            .await
            .unwrap();
    }
    let targets = json!([a, b]);
    let opened = f
        .call("change.open", json!({"targets":targets}))
        .await
        .unwrap();
    let token = opened["token"].clone();
    assert_eq!(opened["targets"], targets);
    let input = json!({"token":token,"targets":targets,"operation":"delete_keys"});
    let preview = f.call("change.preview", input.clone()).await.unwrap();
    let statements = preview["items"][0]["statements"].as_array().unwrap();
    assert_eq!(
        statements
            .iter()
            .map(|s| s["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["MULTI", "DEL", "EXEC"]
    );
    assert_eq!(statements[1]["parameters"][0]["kind"], "bytes");
    assert_eq!(
        statements[1]["parameters"][0]["display"],
        STANDARD.encode(&keys[0])
    );
    let mut altered = input.clone();
    altered["targets"][0]["name"] = json!("tampered");
    assert!(f.call("change.preview", altered).await.is_err());
    let mut injected = input.clone();
    injected["changes"] = json!([{"operation":"delete_key"}]);
    assert!(f.call("change.preview", injected).await.is_err());
    let _: () = redis::cmd("SET")
        .arg(&keys[1])
        .arg("changed")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(
        f.call("change.submit", input.clone())
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    assert!(f.call("change.submit", input).await.is_err());
    let exists: i64 = redis::cmd("EXISTS")
        .arg(&keys)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(exists, 2);
    let opened = f
        .call("change.open", json!({"targets":targets}))
        .await
        .unwrap();
    let input = json!({"token":opened["token"],"targets":targets,"operation":"delete_keys"});
    let mut forged = input.clone();
    forged["token"] = json!(Uuid::new_v4());
    assert!(f.call("change.submit", forged).await.is_err());
    let result = f.call("change.submit", input).await.unwrap();
    assert_eq!(result["affected"], 2);
    let exists: i64 = redis::cmd("EXISTS")
        .arg(&keys)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(exists, 0);
    f.close().await;
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 和隔离的 Redis 实例"]
async fn batch_delete_rejects_invalid_targets_and_read_only() {
    let mut f = Fixture::new().await;
    let target = f.target(b"batch-validation", "string");
    let key = raw_key(&target);
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg("value")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let target_json = json!(target);
    assert!(f.call("change.open", json!({"targets":[]})).await.is_err());
    assert!(
        f.call(
            "change.open",
            json!({"target":target_json,"targets":[target_json]})
        )
        .await
        .is_err()
    );
    assert!(
        f.call("change.open", json!({"targets":[target_json,target_json]}))
            .await
            .is_err()
    );
    for (field, value) in [
        ("database", json!("14")),
        ("key_b64", json!("%%%")),
        ("key_b64", Json::Null),
        ("kind", json!("table")),
    ] {
        let mut invalid = target_json.clone();
        invalid[field] = value;
        assert!(
            f.call("change.open", json!({"targets":[invalid]}))
                .await
                .is_err()
        );
    }
    let opened = f
        .call("change.open", json!({"targets":[target_json]}))
        .await
        .unwrap();
    let token = opened["token"].clone();
    assert_eq!(
        f.call("change.discard", json!({"token":token}))
            .await
            .unwrap()["discarded"],
        true
    );
    assert!(
        f.call(
            "change.preview",
            json!({"token":token,"targets":[target_json],"operation":"delete_keys"})
        )
        .await
        .is_err()
    );
    let opened = f
        .call("change.open", json!({"targets":[target_json]}))
        .await
        .unwrap();
    let tampered = json!({"token":opened["token"],"targets":[target_json],"operation":"delete_keys","changes":[]});
    assert!(f.call("change.submit", tampered).await.is_err());
    let exists: i64 = redis::cmd("EXISTS")
        .arg(&key)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(exists, 1);
    let opened = f
        .call("change.open", json!({"targets":[target_json]}))
        .await
        .unwrap();
    let _: i64 = redis::cmd("PEXPIREAT")
        .arg(&key)
        .arg(1)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let expired =
        json!({"token":opened["token"],"targets":[target_json],"operation":"delete_keys"});
    assert_eq!(
        f.call("change.submit", expired).await.unwrap_err().code,
        "conflict"
    );
    let mut read_only = f.profile.clone();
    read_only.access_mode = AccessMode::ReadOnly;
    let mut session = ProviderSession::connect(read_only, None).await.unwrap();
    let sink = f.sink().await;
    let err = session
        .call(
            "change.open",
            json!({"targets":[target_json]}),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code, "permission_denied");
    session.close().await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 和隔离的 Redis 实例"]
async fn empty_key_field_and_member_are_valid_identities() {
    let mut f = Fixture::new().await;
    // NX 避免覆盖已有的空 key；临时过期时间保证异常退出后不会永久残留。
    let marker = Uuid::new_v4().to_string();
    let claimed: Option<String> = redis::cmd("SET")
        .arg(&b""[..])
        .arg(&marker)
        .arg("NX")
        .arg("PX")
        .arg(60000)
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert!(claimed.is_some(), "隔离 Redis 中的空 key 已被占用");
    let empty = ResourceTarget {
        database: Some("15".into()),
        name: String::new(),
        key_b64: Some(String::new()),
        kind: "string".into(),
    };
    let other = f.target(b"empty-identity-other", "string");
    let _: () = redis::cmd("SET")
        .arg(raw_key(&other))
        .arg("value")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let targets = json!([empty, other]);
    let opened = f
        .call("change.open", json!({"targets":targets}))
        .await
        .unwrap();
    let input = json!({"token":opened["token"],"targets":targets,"operation":"delete_keys"});
    let preview = f.call("change.preview", input.clone()).await.unwrap();
    assert_eq!(
        preview["items"][0]["statements"][1]["parameters"][0]["byte_len"],
        0
    );
    assert_eq!(f.call("change.submit", input).await.unwrap()["affected"], 2);

    let hash = f.target(b"empty-field", "hash");
    let set = f.target(b"empty-member", "set");
    let _: i64 = redis::cmd("HSET")
        .arg(raw_key(&hash))
        .arg(&b""[..])
        .arg("value")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let _: i64 = redis::cmd("SADD")
        .arg(raw_key(&set))
        .arg(&b""[..])
        .query_async(&mut f.raw)
        .await
        .unwrap();
    f.edit(
        &hash,
        json!([{"operation":"hash_delete","field":binary(b"")}]),
    )
    .await;
    f.edit(
        &set,
        json!([{"operation":"set_remove","member":binary(b"")}]),
    )
    .await;
    let remaining: i64 = redis::cmd("EXISTS")
        .arg(raw_key(&hash))
        .arg(raw_key(&set))
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(remaining, 0);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT and an isolated Redis instance; pauses the server"]
async fn cancelled_read_discards_connection_and_next_read_reconnects() {
    let mut f = Fixture::new().await;
    let target = f.target(b"cancel-read", "string");
    let _: () = redis::cmd("SET")
        .arg(raw_key(&target))
        .arg("alive")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(f.rows(&target).await, vec![vec![binary(b"alive")]]);
    let _: () = redis::cmd("CLIENT")
        .arg("PAUSE")
        .arg(600)
        .arg("ALL")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let sink = f.sink().await;
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let (result, ()) = tokio::join!(
        f.session.call(
            "resource.describe",
            json!({"target":target}),
            &sink,
            &cancel
        ),
        async move {
            tokio::time::sleep(Duration::from_millis(75)).await;
            signal.cancel();
        }
    );
    assert_eq!(result.unwrap_err().code, "cancelled");
    // 先解除故障再验证重连；仍被暂停的服务可能超过驱动的响应超时。
    f.raw.set_response_timeout(Duration::from_secs(2));
    let _: () = redis::cmd("CLIENT")
        .arg("UNPAUSE")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let rows = tokio::time::timeout(Duration::from_secs(2), f.rows(&target))
        .await
        .unwrap();
    assert_eq!(rows, vec![vec![binary(b"alive")]]);
    f.close().await;
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 和隔离的 Redis 实例"]
async fn database_discovery_does_not_change_command_database_or_transaction() {
    let mut f = Fixture::new().await;
    let sink = f.sink().await;
    let cancel = CancellationToken::new();
    f.session
        .call(
            "execution.run",
            json!({"text":"SELECT 1\nMULTI"}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    let discovery = f
        .session
        .call("database.list", json!({}), &sink, &cancel)
        .await
        .unwrap();
    assert!(matches!(
        discovery["state"].as_str(),
        Some("known" | "unknown")
    ));
    if discovery["state"] == "known" {
        assert_eq!(discovery["source"], "server_config");
    }
    let info = f
        .session
        .call("connection.info", json!({}), &sink, &cancel)
        .await
        .unwrap();
    assert_eq!(info["database"], 1);
    assert_eq!(info["browse_database"], 15);
    assert_eq!(info["transaction"], true);
    f.session
        .call("execution.run", json!({"text":"DISCARD"}), &sink, &cancel)
        .await
        .unwrap();
    let mut profile = f.profile.clone();
    if let ConnectionConfig::Direct(direct) = &mut profile.connection {
        direct.redis_databases_override = Some(3);
    }
    let mut manual = ProviderSession::connect(profile, None).await.unwrap();
    let result = manual
        .call("database.list", json!({}), &sink, &cancel)
        .await
        .unwrap();
    assert_eq!(result["source"], "manual_override");
    assert_eq!(result["databases"], json!(["0", "1", "2"]));
    manual.close().await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "requires an isolated Redis instance"]
async fn value_hash_update_preserves_field_expiry_and_rejects_conflicts() {
    let mut f = Fixture::new().await;
    let target = f.target(b"advanced-hash", "hash");
    let key = raw_key(&target);
    let field = b"field\0\xff";
    let _: i64 = redis::cmd("HSET")
        .arg(&key)
        .arg(field.as_slice())
        .arg("before")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let info: String = redis::cmd("INFO")
        .arg("server")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let major: u32 = info
        .lines()
        .find_map(|line| {
            line.strip_prefix("redis_version:")
                .and_then(|v| v.split('.').next())
                .and_then(|v| v.parse().ok())
        })
        .unwrap();
    let expiry = if major >= 8 {
        let _: Vec<i64> = redis::cmd("HPEXPIRE")
            .arg(&key)
            .arg(120_000)
            .arg("FIELDS")
            .arg(1)
            .arg(field.as_slice())
            .query_async(&mut f.raw)
            .await
            .unwrap();
        let times: Vec<i64> = redis::cmd("HPEXPIRETIME")
            .arg(&key)
            .arg("FIELDS")
            .arg(1)
            .arg(field.as_slice())
            .query_async(&mut f.raw)
            .await
            .unwrap();
        Some(times[0])
    } else {
        None
    };
    let opened = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    let params = json!({"target":target,"token":opened["token"],"changes":[{"operation":"hash_set","field":binary(field),"value":binary(b"after"),"preserve_field_ttl":true,"field_expires_at_ms":expiry}]});
    let preview = f.call("change.preview", params.clone()).await.unwrap();
    assert_eq!(preview["items"][0]["statements"][0]["text"], "EVAL");
    let result = f.call("change.submit", params).await.unwrap();
    assert_applied(&result);
    let actual: Vec<u8> = redis::cmd("HGET")
        .arg(&key)
        .arg(field.as_slice())
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(actual, b"after");
    if let Some(expiry) = expiry {
        let actual: Vec<i64> = redis::cmd("HPEXPIRETIME")
            .arg(&key)
            .arg("FIELDS")
            .arg(1)
            .arg(field.as_slice())
            .query_async(&mut f.raw)
            .await
            .unwrap();
        assert_eq!(actual[0], expiry);
        let opened = f
            .call("change.open", json!({"target":target}))
            .await
            .unwrap();
        let error = f.call("change.submit", json!({"target":target,"token":opened["token"],"changes":[{"operation":"hash_set","field":binary(field),"value":binary(b"wrong"),"preserve_field_ttl":true,"field_expires_at_ms":expiry + 1}]})).await.unwrap_err();
        assert_eq!(error.code, "conflict");
        let actual: Vec<u8> = redis::cmd("HGET")
            .arg(&key)
            .arg(field.as_slice())
            .query_async(&mut f.raw)
            .await
            .unwrap();
        assert_eq!(actual, b"after");
    }
    // 恢复的意图没有旧期限，预览应在新 WATCH 连接上冻结当前基线。
    let opened = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    let restored = json!({"target":target,"token":opened["token"],"changes":[{"operation":"hash_set","field":binary(field),"value":binary(b"restored"),"preserve_field_ttl":true}]});
    f.call("change.preview", restored.clone()).await.unwrap();
    let result = f.call("change.submit", restored).await.unwrap();
    assert_applied(&result);
    if let Some(expiry) = expiry {
        let actual: Vec<i64> = redis::cmd("HPEXPIRETIME")
            .arg(&key)
            .arg("FIELDS")
            .arg(1)
            .arg(field.as_slice())
            .query_async(&mut f.raw)
            .await
            .unwrap();
        assert_eq!(actual[0], expiry);
    }
    let opened = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    let changed = json!({"target":target,"token":opened["token"],"changes":[{"operation":"hash_set","field":binary(field),"value":binary(b"should-not-write"),"preserve_field_ttl":true}]});
    f.call("change.preview", changed.clone()).await.unwrap();
    let _: i64 = redis::cmd("HSET")
        .arg(&key)
        .arg(field.as_slice())
        .arg("external")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(
        f.call("change.submit", changed).await.unwrap_err().code,
        "conflict"
    );
    let actual: Vec<u8> = redis::cmd("HGET")
        .arg(&key)
        .arg(field.as_slice())
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(actual, b"external");
    f.close().await;
}

#[tokio::test]
#[ignore = "requires an isolated Redis instance"]
async fn value_zset_member_replacement_preserves_score_and_rejects_collision() {
    let mut f = Fixture::new().await;
    let target = f.target(b"advanced-zset", "zset");
    let key = raw_key(&target);
    let _: i64 = redis::cmd("ZADD")
        .arg(&key)
        .arg("-12.5")
        .arg(b"old\0\xff".as_slice())
        .arg("9")
        .arg("occupied")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let opened = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    let params = json!({"target":target,"token":opened["token"],"changes":[{"operation":"zset_replace_member","member":binary(b"old\0\xff"),"new_member":binary(b"new\0\xff"),"preserve_score":true}]});
    let preview = f.call("change.preview", params.clone()).await.unwrap();
    assert_eq!(preview["items"][0]["statements"][0]["text"], "EVAL");
    let result = f.call("change.submit", params).await.unwrap();
    assert_applied(&result);
    let score: String = redis::cmd("ZSCORE")
        .arg(&key)
        .arg(b"new\0\xff".as_slice())
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(score.parse::<f64>().unwrap(), -12.5);
    let old: Option<String> = redis::cmd("ZSCORE")
        .arg(&key)
        .arg(b"old\0\xff".as_slice())
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert!(old.is_none());
    let opened = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    let result = f.call("change.submit", json!({"target":target,"token":opened["token"],"changes":[{"operation":"zset_replace_member","member":binary(b"new\0\xff"),"new_member":binary(b"occupied"),"preserve_score":true}]})).await.unwrap();
    assert_eq!(result["items"][0]["status"], "conflict");
    let score: String = redis::cmd("ZSCORE")
        .arg(&key)
        .arg(b"new\0\xff".as_slice())
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(score.parse::<f64>().unwrap(), -12.5);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires an isolated Redis instance"]
async fn value_set_member_replacement_rejects_collisions() {
    let mut f = Fixture::new().await;
    let target = f.target(b"advanced-set", "set");
    let key = raw_key(&target);
    let _: i64 = redis::cmd("SADD")
        .arg(&key)
        .arg(b"old\0\xff".as_slice())
        .arg("occupied")
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let opened = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    let unchanged = f.call("change.submit", json!({"target":target,"token":opened["token"],"changes":[{"operation":"set_replace_member","member":binary(b"old\0\xff"),"new_member":binary(b"old\0\xff")}]})).await.unwrap();
    assert_applied(&unchanged);
    let retained: i64 = redis::cmd("SISMEMBER")
        .arg(&key)
        .arg(b"old\0\xff".as_slice())
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(retained, 1);
    let opened = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    let params = json!({"target":target,"token":opened["token"],"changes":[{"operation":"set_replace_member","member":binary(b"old\0\xff"),"new_member":binary(b"new\0\xff")}]});
    let preview = f.call("change.preview", params.clone()).await.unwrap();
    assert_eq!(preview["items"][0]["statements"][0]["text"], "EVAL");
    assert_applied(&f.call("change.submit", params).await.unwrap());
    let old: i64 = redis::cmd("SISMEMBER")
        .arg(&key)
        .arg(b"old\0\xff".as_slice())
        .query_async(&mut f.raw)
        .await
        .unwrap();
    let new: i64 = redis::cmd("SISMEMBER")
        .arg(&key)
        .arg(b"new\0\xff".as_slice())
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!((old, new), (0, 1));
    let opened = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    let result = f.call("change.submit", json!({"target":target,"token":opened["token"],"changes":[{"operation":"set_replace_member","member":binary(b"new\0\xff"),"new_member":binary(b"occupied")}]})).await.unwrap();
    assert_eq!(result["items"][0]["status"], "conflict");
    let retained: i64 = redis::cmd("SISMEMBER")
        .arg(&key)
        .arg(b"new\0\xff".as_slice())
        .query_async(&mut f.raw)
        .await
        .unwrap();
    assert_eq!(retained, 1);
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 Redis 6.2+ 及 ACL 用户管理权限"]
async fn value_verification_preserves_original_watch_without_rewatch_permission() {
    let mut f = Fixture::new().await;
    let target = f.target(b"original-watch", "string");
    let key = STANDARD.decode(target.key_b64.as_ref().unwrap()).unwrap();
    let username = format!("dp_watch_{}", Uuid::new_v4().simple());
    let secret = Uuid::new_v4().to_string();
    let outcome = std::panic::AssertUnwindSafe(async {
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg("original")
            .query_async(&mut f.raw)
            .await
            .unwrap();
        let _: () = redis::cmd("ACL")
            .arg("SETUSER")
            .arg(&username)
            .arg("on")
            .arg(format!(">{secret}"))
            .arg(format!("~{}*", f.prefix))
            .arg("+@all")
            .query_async(&mut f.raw)
            .await
            .unwrap();
        let mut profile = f.profile.clone();
        profile.direct_mut().unwrap().username = username.clone();
        f.session.close().await.unwrap();
        f.session = ProviderSession::connect(profile, Some(secret))
            .await
            .unwrap();
        let opened = f
            .call("change.open", json!({"target":target}))
            .await
            .unwrap();
        let mut params = json!({"target":target,"token":opened["token"],
            "result_id":opened["result_id"],"role":"string","max_bytes":4096});
        let ProviderSession::Redis(session) = &mut f.session else {
            unreachable!()
        };
        let baseline = session.value_snapshot(&params).await.unwrap();
        params["before"] = baseline["value"].clone();
        params["score"] = baseline["score"].clone();
        params["field_expires_at_ms"] = baseline["field_expires_at_ms"].clone();
        let _: () = redis::cmd("ACL")
            .arg("SETUSER")
            .arg(&username)
            .arg("-watch")
            .query_async(&mut f.raw)
            .await
            .unwrap();
        session.value_verify(&params).await.unwrap();
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg("other")
            .query_async(&mut f.raw)
            .await
            .unwrap();
        let _: () = redis::cmd("SET")
            .arg(&key)
            .arg("original")
            .query_async(&mut f.raw)
            .await
            .unwrap();
        assert_eq!(
            session.value_verify(&params).await.unwrap_err().code,
            "conflict"
        );
        let actual: Vec<u8> = redis::cmd("GET")
            .arg(&key)
            .query_async(&mut f.raw)
            .await
            .unwrap();
        assert_eq!(actual, b"original");
    })
    .catch_unwind()
    .await;
    let cleanup: redis::RedisResult<i64> = redis::cmd("ACL")
        .arg("DELUSER")
        .arg(&username)
        .query_async(&mut f.raw)
        .await;
    f.close().await;
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    cleanup.unwrap();
}
