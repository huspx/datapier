//! Redis 外部编辑回填使用原 WATCH 的真实服务端回归。
use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::{ExecutionSink, Store, model::*, providers::ProviderSession};
use redis::{Client, aio::MultiplexedConnection};
use serde_json::{Value as Json, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Fixture {
    store: Store,
    session: ProviderSession,
    conn: MultiplexedConnection,
    keys: Vec<Vec<u8>>,
    prefix: String,
}

impl Fixture {
    async fn new() -> Self {
        let port: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")
            .expect("需要隔离 Redis 实例")
            .parse()
            .unwrap();
        let root = std::env::temp_dir().join(format!("datapier-value-watch-{}", Uuid::new_v4()));
        let store = Store::open(&root).await.unwrap();
        let profile = Profile {
            id: Uuid::new_v4(),
            name: "value-watch-test".into(),
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
            }),
        };
        let session = ProviderSession::connect(profile, None).await.unwrap();
        let conn = Client::open(format!("redis://127.0.0.1:{port}/15"))
            .unwrap()
            .get_multiplexed_async_connection()
            .await
            .unwrap();
        Self {
            store,
            session,
            conn,
            keys: Vec::new(),
            prefix: format!("datapier-value-watch:{}:", Uuid::new_v4()),
        }
    }

    fn target(&mut self, name: &str, kind: &str) -> ResourceTarget {
        let key = format!("{}{name}", self.prefix).into_bytes();
        self.keys.push(key.clone());
        ResourceTarget {
            database: Some("15".into()),
            name: String::from_utf8(key.clone()).unwrap(),
            key_b64: Some(STANDARD.encode(key)),
            kind: kind.into(),
        }
    }

    async fn call(&mut self, method: &str, params: Json) -> Result<Json> {
        let task = self
            .store
            .create_task(
                Uuid::new_v4(),
                "redis-value-watch",
                "execution.start",
                json!({}),
                false,
            )
            .await
            .unwrap();
        let sink = ExecutionSink::new(self.store.clone(), task.id);
        self.session
            .call(method, params, &sink, &CancellationToken::new())
            .await
    }

    async fn baseline(&mut self, target: &ResourceTarget, role: &str) -> Json {
        let opened = self
            .call("change.open", json!({"target":target}))
            .await
            .unwrap();
        let mut params = json!({"target":target,"token":opened["token"],"result_id":opened["result_id"],"role":role,"max_bytes":1024});
        if role == "hash_value" {
            params["field"] = json!(Value::Bytes(STANDARD.encode(b"selected")));
        }
        let snapshot = self.call("value.snapshot", params.clone()).await.unwrap();
        params["before"] = snapshot["value"].clone();
        params["score"] = snapshot["score"].clone();
        params["field_expires_at_ms"] = snapshot["field_expires_at_ms"].clone();
        params
    }

    async fn close(mut self) {
        if !self.keys.is_empty() {
            let _: i64 = redis::cmd("DEL")
                .arg(&self.keys)
                .query_async(&mut self.conn)
                .await
                .unwrap();
        }
        self.session.close().await.unwrap();
        let root = (*self.store.root).clone();
        self.store.pool.close().await;
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}

fn key(target: &ResourceTarget) -> Vec<u8> {
    STANDARD.decode(target.key_b64.as_ref().unwrap()).unwrap()
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 隔离实例"]
async fn hash_other_field_and_string_aba_invalidate_original_watch() {
    let mut f = Fixture::new().await;
    let hash = f.target("hash", "hash");
    let hash_key = key(&hash);
    let _: i64 = redis::cmd("HSET")
        .arg(&hash_key)
        .arg("selected")
        .arg("old")
        .arg("other")
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let params = f.baseline(&hash, "hash_value").await;
    let _: i64 = redis::cmd("HSET")
        .arg(&hash_key)
        .arg("other")
        .arg("external")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(
        f.call("value.verify", params.clone())
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    assert!(f.call("value.verify", params.clone()).await.is_err());
    let changes = json!([{"operation":"hash_set","field":Value::Bytes(STANDARD.encode(b"selected")),"value":Value::Bytes(STANDARD.encode(b"new"))}]);
    assert!(
        f.call(
            "change.submit",
            json!({"target":hash,"token":params["token"],"changes":changes})
        )
        .await
        .is_err()
    );
    let fields: Vec<String> = redis::cmd("HMGET")
        .arg(&hash_key)
        .arg("selected")
        .arg("other")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(fields, ["old", "external"]);

    let string = f.target("string", "string");
    let string_key = key(&string);
    let _: () = redis::cmd("SET")
        .arg(&string_key)
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let params = f.baseline(&string, "string").await;
    for value in ["external", "old"] {
        let _: () = redis::cmd("SET")
            .arg(&string_key)
            .arg(value)
            .query_async(&mut f.conn)
            .await
            .unwrap();
    }
    assert_eq!(
        f.call("value.verify", params.clone())
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    assert!(f.call("change.submit", json!({"target":string,"token":params["token"],"changes":[{"operation":"set_string","value":Value::Bytes(STANDARD.encode(b"new"))}]})).await.is_err());
    let value: String = redis::cmd("GET")
        .arg(&string_key)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(value, "old");
    f.close().await;
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 隔离实例"]
async fn repeated_verification_keeps_watch_and_ttl_until_commit() {
    let mut f = Fixture::new().await;
    let target = f.target("success", "string");
    let k = key(&target);
    let _: () = redis::cmd("SET")
        .arg(&k)
        .arg("old")
        .arg("PX")
        .arg(60000)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let expiry: i64 = redis::cmd("PTTL")
        .arg(&k)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let params = f.baseline(&target, "string").await;
    for _ in 0..3 {
        f.call("value.verify", params.clone()).await.unwrap();
    }
    let before: String = redis::cmd("GET")
        .arg(&k)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(before, "old");
    let result = f.call("change.submit", json!({"target":target,"token":params["token"],"changes":[{"operation":"set_string","value":Value::Bytes(STANDARD.encode(b"new"))}]})).await.unwrap();
    assert_eq!(result["items"][0]["status"], "applied");
    let value: String = redis::cmd("GET")
        .arg(&k)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(value, "new");
    let expiry_after: i64 = redis::cmd("PTTL")
        .arg(&k)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert!(expiry_after > 0 && expiry_after <= expiry);

    let params = f.baseline(&target, "string").await;
    f.call("value.verify", params.clone()).await.unwrap();
    let _: () = redis::cmd("SET")
        .arg(&k)
        .arg("external")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(f.call("change.submit", json!({"target":target,"token":params["token"],"changes":[{"operation":"set_string","value":Value::Bytes(STANDARD.encode(b"wrong"))}]})).await.unwrap_err().code, "conflict");
    let value: String = redis::cmd("GET")
        .arg(&k)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(value, "external");
    f.close().await;
}
