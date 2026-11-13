//! Redis 编辑与扫描契约；需要隔离实例。
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
    keys: Vec<String>,
}
impl Fixture {
    async fn new() -> Self {
        let port: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let root = std::env::temp_dir().join(format!("datapier-redis-edit-{}", Uuid::new_v4()));
        let store = Store::open(&root).await.unwrap();
        let profile = Profile {
            id: Uuid::new_v4(),
            name: "redis-edit".into(),
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
        }
    }
    fn target(&mut self, kind: &str) -> ResourceTarget {
        let key = format!("d{}", Uuid::new_v4().simple());
        self.keys.push(key.clone());
        ResourceTarget {
            database: Some("15".into()),
            name: key.clone(),
            key_b64: Some(STANDARD.encode(key)),
            kind: kind.into(),
        }
    }
    async fn call(&mut self, method: &str, params: Json) -> Result<Json> {
        let task = self
            .store
            .create_task(
                Uuid::new_v4(),
                "redis-edit",
                "execution.start",
                json!({}),
                false,
            )
            .await
            .unwrap();
        self.session
            .call(
                method,
                params,
                &ExecutionSink::new(self.store.clone(), task.id),
                &CancellationToken::new(),
            )
            .await
    }
    async fn submit(&mut self, target: &ResourceTarget, changes: Json) -> Result<Json> {
        let opened = self
            .call("change.open", json!({"target":target}))
            .await
            .unwrap();
        self.call(
            "change.submit",
            json!({"target":target,"token":opened["token"],"changes":changes}),
        )
        .await
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
fn bytes(value: &str) -> Value {
    Value::Bytes(STANDARD.encode(value))
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT isolated Redis"]
async fn identity_collisions_leave_collections_unchanged_and_list_reports_partial_failure() {
    let mut f = Fixture::new().await;
    for kind in ["hash", "set", "zset"] {
        let t = f.target(kind);
        let k = &t.name;
        match kind {
            "hash" => {
                let _: i64 = redis::cmd("HSET")
                    .arg(k)
                    .arg("old")
                    .arg("v1")
                    .arg("new")
                    .arg("v2")
                    .query_async(&mut f.conn)
                    .await
                    .unwrap();
            }
            "set" => {
                let _: i64 = redis::cmd("SADD")
                    .arg(k)
                    .arg("old")
                    .arg("new")
                    .query_async(&mut f.conn)
                    .await
                    .unwrap();
            }
            _ => {
                let _: i64 = redis::cmd("ZADD")
                    .arg(k)
                    .arg(1)
                    .arg("old")
                    .arg(2)
                    .arg("new")
                    .query_async(&mut f.conn)
                    .await
                    .unwrap();
            }
        }
        let before: Vec<u8> = redis::cmd("DUMP")
            .arg(k)
            .query_async(&mut f.conn)
            .await
            .unwrap();
        let change = match kind {
            "hash" => {
                json!({"operation":"hash_replace_field","field":bytes("old"),"new_field":bytes("new")})
            }
            "set" => {
                json!({"operation":"set_replace_member","member":bytes("old"),"new_member":bytes("new")})
            }
            _ => {
                json!({"operation":"zset_replace_member","member":bytes("old"),"new_member":bytes("new"),"score":"9"})
            }
        };
        let error = f.submit(&t, json!([change])).await.unwrap_err();
        assert_eq!(error.code, "partial_failure");
        assert_eq!(error.details["items"][0]["status"], "conflict");
        let after: Vec<u8> = redis::cmd("DUMP")
            .arg(k)
            .query_async(&mut f.conn)
            .await
            .unwrap();
        assert_eq!(before, after);
        if kind == "zset" {
            f.submit(&t, json!([{"operation":"zset_replace_member","member":bytes("old"),"new_member":bytes("third"),"score":"9"}])).await.unwrap();
            let score: f64 = redis::cmd("ZSCORE")
                .arg(k)
                .arg("third")
                .query_async(&mut f.conn)
                .await
                .unwrap();
            assert_eq!(score, 9.0);
        }
    }
    let t = f.target("list");
    let _: i64 = redis::cmd("RPUSH")
        .arg(&t.name)
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let error = f.submit(&t, json!([{"operation":"list_set","index":0,"value":bytes("new")},{"operation":"list_set","index":9,"value":bytes("bad")}])).await.unwrap_err();
    assert_eq!(error.code, "partial_failure");
    assert_eq!(error.details["items"][0]["status"], "applied");
    assert_eq!(error.details["items"][1]["status"], "error");
    let value: String = redis::cmd("LINDEX")
        .arg(&t.name)
        .arg(0)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(value, "new");
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT Redis 7.4+ isolated instance"]
async fn hash_field_updates_and_renames_preserve_expiry() {
    let mut f = Fixture::new().await;
    let t = f.target("hash");
    let _: i64 = redis::cmd("HSET")
        .arg(&t.name)
        .arg("old")
        .arg("v")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let _: i64 = redis::cmd("PEXPIRE")
        .arg(&t.name)
        .arg(180000)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let _: Vec<i64> = redis::cmd("HPEXPIRE")
        .arg(&t.name)
        .arg(120000)
        .arg("FIELDS")
        .arg(1)
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let expiry: Vec<i64> = redis::cmd("HPEXPIRETIME")
        .arg(&t.name)
        .arg("FIELDS")
        .arg(1)
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let key_expiry: i64 = redis::cmd("PEXPIRETIME")
        .arg(&t.name)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    f.submit(&t, json!([{"operation":"hash_set","field":bytes("old"),"value":bytes("updated"),"preserve_field_ttl":true}])).await.unwrap();
    f.submit(&t, json!([{"operation":"hash_replace_field","field":bytes("old"),"new_field":bytes("new"),"value":bytes("renamed")}])).await.unwrap();
    let actual: Vec<i64> = redis::cmd("HPEXPIRETIME")
        .arg(&t.name)
        .arg("FIELDS")
        .arg(1)
        .arg("new")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(expiry, actual);
    let actual_key: i64 = redis::cmd("PEXPIRETIME")
        .arg(&t.name)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(key_expiry, actual_key);
    let value: String = redis::cmd("HGET")
        .arg(&t.name)
        .arg("new")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(value, "renamed");
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT isolated Redis"]
async fn short_keys_fit_minimum_response_budget() {
    let mut f = Fixture::new().await;
    let t = f.target("string");
    let _: () = redis::cmd("SET")
        .arg(&t.name)
        .arg("v")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let mut settings = f.store.settings().await.unwrap();
    settings.limits.response_bytes = 1024;
    settings.limits.value_preview_bytes = settings.limits.value_preview_bytes.min(1024);
    f.store.set_settings(&settings).await.unwrap();
    let page = f
        .call("resource.list", json!({"pattern":t.name,"limit":10}))
        .await
        .unwrap();
    assert_eq!(page["resources"].as_array().unwrap().len(), 1);
    assert_eq!(page["resource_limited"], false);
    f.close().await;
}

#[tokio::test]
#[ignore = "requires DATAPIER_TEST_REDIS_PORT isolated Redis"]
async fn scan_memory_limit_reports_incomplete_terminal_page() {
    let mut f = Fixture::new().await;
    let key = format!("d{}{}", Uuid::new_v4().simple(), "x".repeat(600));
    f.keys.push(key.clone());
    let _: () = redis::cmd("SET")
        .arg(&key)
        .arg("v")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let mut settings = f.store.settings().await.unwrap();
    settings.limits.response_bytes = 1024;
    settings.limits.result_memory_bytes_per_client = 1024;
    settings.limits.value_preview_bytes = settings.limits.value_preview_bytes.min(1024);
    f.store.set_settings(&settings).await.unwrap();
    let page = f
        .call("resource.list", json!({"pattern":key,"limit":10}))
        .await
        .unwrap();
    assert_eq!(page["resource_limited"], true);
    assert_eq!(page["complete"], false);
    assert!(page["cursor"].is_null());
    let model: ResourcePage = serde_json::from_value(page).unwrap();
    assert!(model.resource_limited);
    f.close().await;
}
