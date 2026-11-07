//! Redis 批量替换的真实服务端边界；仅在隔离实例上显式运行。
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
        let root = std::env::temp_dir().join(format!("datapier-replace-{}", Uuid::new_v4()));
        let store = Store::open(&root).await.unwrap();
        let profile = Profile {
            id: Uuid::new_v4(),
            name: "replace-test".into(),
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
            keys: vec![],
            prefix: format!("datapier-replace:{}:", Uuid::new_v4()),
        }
    }
    fn target(&mut self, suffix: &str, kind: &str) -> ResourceTarget {
        let key = format!("{}{suffix}", self.prefix).into_bytes();
        self.keys.push(key.clone());
        ResourceTarget {
            database: Some("15".into()),
            name: String::from_utf8_lossy(&key).into_owned(),
            key_b64: Some(STANDARD.encode(key)),
            kind: kind.into(),
        }
    }
    async fn call(&mut self, method: &str, params: Json) -> Result<Json> {
        let task = self
            .store
            .create_task(
                Uuid::new_v4(),
                "redis-replace",
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
fn spec(pattern: &str, replacement: &str, roles: &[&str]) -> Json {
    json!({"pattern":pattern,"replacement":replacement,"regex":false,"case_sensitive":true,"multi_line":false,"dot_matches_new_line":false,"scope":"selected","columns":[],"roles":roles})
}
fn submit(summary: &Json) -> Json {
    json!({"plan_id":summary["id"],"revision":summary["revision"],"digest":summary["plan_digest"]})
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 隔离实例"]
async fn list_duplicates_are_replaced_by_index_and_ttl_is_preserved() {
    let mut f = Fixture::new().await;
    let target = f.target("list", "list");
    let k = key(&target);
    let _: i64 = redis::cmd("RPUSH")
        .arg(&k)
        .arg("old")
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let _: bool = redis::cmd("PEXPIRE")
        .arg(&k)
        .arg(60000)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let plan = f
        .call(
            "replace.prepare",
            json!({"targets":[target],"spec":spec("old","new", &["list_value"])}),
        )
        .await
        .unwrap();
    let summary = &plan["summary"];
    let page = f
        .call(
            "replace.read",
            json!({"plan_id":summary["id"],"revision":summary["revision"]}),
        )
        .await
        .unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    let updated = f.call("replace.update", json!({"plan_id":summary["id"],"expected_revision":summary["revision"],"selections":[{"id":1,"selected":false}]})).await.unwrap();
    assert!(f.call("replace.submit", submit(summary)).await.is_err());
    let result = f
        .call("replace.submit", submit(&updated["summary"]))
        .await
        .unwrap();
    assert_eq!(result["status"], "ok");
    let values: Vec<String> = redis::cmd("LRANGE")
        .arg(&k)
        .arg(0)
        .arg(-1)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(values, ["new", "old"]);
    let ttl: i64 = redis::cmd("PTTL")
        .arg(&k)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert!(ttl > 0 && ttl <= 60000);
    f.close().await;
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 隔离实例"]
async fn member_collisions_reject_and_watch_conflicts_never_retry() {
    let mut f = Fixture::new().await;
    let target = f.target("set", "set");
    let k = key(&target);
    let _: i64 = redis::cmd("SADD")
        .arg(&k)
        .arg("old")
        .arg("new")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let plan = f
        .call(
            "replace.prepare",
            json!({"targets":[target.clone()],"spec":spec("old","new", &["member"])}),
        )
        .await
        .unwrap();
    assert_eq!(plan["summary"]["submittable"], false);
    assert!(
        f.call("replace.submit", submit(&plan["summary"]))
            .await
            .is_err()
    );
    let plan = f
        .call(
            "replace.prepare",
            json!({"targets":[target],"spec":spec("old","other", &["member"])}),
        )
        .await
        .unwrap();
    let _: i64 = redis::cmd("SADD")
        .arg(&k)
        .arg("external")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let error = f
        .call("replace.submit", submit(&plan["summary"]))
        .await
        .unwrap_err();
    assert_eq!(error.code, "conflict");
    let exists: bool = redis::cmd("SISMEMBER")
        .arg(&k)
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert!(exists);
    f.close().await;
}

#[tokio::test]
#[ignore = "需要支持 HPEXPIREAT 的隔离 Redis 7.4/8 实例"]
async fn hash_field_rename_preserves_absolute_field_and_key_expiry() {
    let mut f = Fixture::new().await;
    let target = f.target("hash", "hash");
    let k = key(&target);
    let _: i64 = redis::cmd("HSET")
        .arg(&k)
        .arg("old")
        .arg("old value")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let _: Vec<i64> = redis::cmd("HPEXPIRE")
        .arg(&k)
        .arg(60000)
        .arg("FIELDS")
        .arg(1)
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let _: bool = redis::cmd("PEXPIRE")
        .arg(&k)
        .arg(120000)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let before: Vec<i64> = redis::cmd("HPEXPIRETIME")
        .arg(&k)
        .arg("FIELDS")
        .arg(1)
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let plan = f
        .call(
            "replace.prepare",
            json!({"targets":[target],"spec":spec("old","new", &["hashfield","hash_value"])}),
        )
        .await
        .unwrap();
    assert_eq!(
        f.call("replace.submit", submit(&plan["summary"]))
            .await
            .unwrap()["status"],
        "ok"
    );
    let after: Vec<i64> = redis::cmd("HPEXPIRETIME")
        .arg(&k)
        .arg("FIELDS")
        .arg(1)
        .arg("new")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(before, after);
    let value: String = redis::cmd("HGET")
        .arg(&k)
        .arg("new")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(value, "new value");
    let ttl: i64 = redis::cmd("PTTL")
        .arg(&k)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert!(ttl > 0 && ttl <= 120000);
    f.close().await;
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 隔离实例"]
async fn zset_member_rename_keeps_score_and_binary_members() {
    let mut f = Fixture::new().await;
    let target = f.target("zset", "zset");
    let k = key(&target);
    let _: i64 = redis::cmd("ZADD")
        .arg(&k)
        .arg("1.25")
        .arg("old")
        .arg("-4")
        .arg(&[255u8][..])
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let plan = f
        .call(
            "replace.prepare",
            json!({"targets":[target],"spec":spec("old","new", &["member"])}),
        )
        .await
        .unwrap();
    assert_eq!(plan["summary"]["non_utf8"], 1);
    assert_eq!(
        f.call("replace.submit", submit(&plan["summary"]))
            .await
            .unwrap()["status"],
        "ok"
    );
    let score: f64 = redis::cmd("ZSCORE")
        .arg(&k)
        .arg("new")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(score, 1.25);
    let binary: f64 = redis::cmd("ZSCORE")
        .arg(&k)
        .arg(&[255u8][..])
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(binary, -4.0);
    f.close().await;
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 隔离实例"]
async fn destination_key_collision_and_creation_after_watch_are_rejected() {
    let mut f = Fixture::new().await;
    let source = f.target("source-old", "string");
    let destination = f.target("source-new", "string");
    let src = key(&source);
    let dst = key(&destination);
    let _: () = redis::cmd("SET")
        .arg(&src)
        .arg("value")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let _: () = redis::cmd("SET")
        .arg(&dst)
        .arg("existing")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let first = f
        .call(
            "replace.prepare",
            json!({"targets":[source.clone()],"spec":spec("source-old","source-new", &["key"])}),
        )
        .await
        .unwrap();
    assert_eq!(first["summary"]["submittable"], false);
    assert!(
        f.call("replace.submit", submit(&first["summary"]))
            .await
            .is_err()
    );
    let _: i64 = redis::cmd("DEL")
        .arg(&dst)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let second = f
        .call(
            "replace.prepare",
            json!({"targets":[source],"spec":spec("source-old","source-new", &["key"])}),
        )
        .await
        .unwrap();
    let _: () = redis::cmd("SET")
        .arg(&dst)
        .arg("concurrent")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(
        f.call("replace.submit", submit(&second["summary"]))
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    let src_value: String = redis::cmd("GET")
        .arg(&src)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let dst_value: String = redis::cmd("GET")
        .arg(&dst)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(
        (src_value.as_str(), dst_value.as_str()),
        ("value", "concurrent")
    );
    f.close().await;
}

#[tokio::test]
#[ignore = "需要支持 field TTL 的隔离 Redis 7.4/8 实例"]
async fn expired_hash_field_cannot_be_resurrected_by_submit() {
    let mut f = Fixture::new().await;
    let target = f.target("expires", "hash");
    let k = key(&target);
    let _: i64 = redis::cmd("HSET")
        .arg(&k)
        .arg("old")
        .arg("value")
        .arg("keeper")
        .arg("v")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let _: Vec<i64> = redis::cmd("HPEXPIRE")
        .arg(&k)
        .arg(1000)
        .arg("FIELDS")
        .arg(1)
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let plan = f
        .call(
            "replace.prepare",
            json!({"targets":[target],"spec":spec("old","new", &["hashfield"])}),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1300)).await;
    assert!(
        f.call("replace.submit", submit(&plan["summary"]))
            .await
            .is_err()
    );
    let exists: bool = redis::cmd("HEXISTS")
        .arg(&k)
        .arg("new")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert!(!exists);
    f.close().await;
}

#[tokio::test]
#[ignore = "需要允许 ACL 管理的隔离 Redis 实例"]
async fn missing_eval_permission_never_falls_back_to_unwatched_writes() {
    let mut f = Fixture::new().await;
    let target = f.target("acl", "string");
    let k = key(&target);
    let _: () = redis::cmd("SET")
        .arg(&k)
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let username = format!("datapier-replace-{}", Uuid::new_v4());
    let password = Uuid::new_v4().to_string();
    let _: String = redis::cmd("ACL")
        .arg("SETUSER")
        .arg(&username)
        .arg("on")
        .arg(format!(">{password}"))
        .arg("~*")
        .arg("+@all")
        .arg("-eval")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let port: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "restricted".into(),
        revision: 1,
        approval_policy: ApprovalPolicy::default(),
        provider: ProviderKind::Redis,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: username.clone(),
            database: None,
            redis_db: 15,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
        }),
    };
    let mut session = ProviderSession::connect(profile, Some(password))
        .await
        .unwrap();
    let task = f
        .store
        .create_task(
            Uuid::new_v4(),
            "redis-acl",
            "execution.start",
            json!({}),
            false,
        )
        .await
        .unwrap();
    let sink = ExecutionSink::new(f.store.clone(), task.id);
    let cancel = CancellationToken::new();
    let plan = session
        .call(
            "replace.prepare",
            json!({"targets":[target],"spec":spec("old","new", &["string_value"])}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    assert!(
        session
            .call("replace.submit", submit(&plan["summary"]), &sink, &cancel)
            .await
            .is_err()
    );
    let value: String = redis::cmd("GET")
        .arg(&k)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(value, "old");
    session.close().await.unwrap();
    let _: i64 = redis::cmd("ACL")
        .arg("DELUSER")
        .arg(username)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 隔离实例"]
async fn prepare_rejects_values_outside_engine_budget_and_reports_retained_bytes() {
    let mut f = Fixture::new().await;
    let target = f.target("budget", "string");
    let k = key(&target);
    let value = "old".repeat(2048);
    let _: () = redis::cmd("SET")
        .arg(&k)
        .arg(&value)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let small = f.call("replace.prepare", json!({"targets":[target.clone()],"spec":spec("old","new", &["string_value"]),"_max_plan_bytes":4096})).await;
    assert_eq!(small.unwrap_err().code, "resource_limit");
    let plan = f.call("replace.prepare", json!({"targets":[target],"spec":spec("old","new", &["string_value"]),"_max_plan_bytes":65536})).await.unwrap();
    let retained = plan["summary"]["retained_bytes"].as_u64().unwrap();
    assert!(retained >= value.len() as u64 && retained <= 65536);
    f.call(
        "replace.discard",
        json!({"plan_id":plan["summary"]["id"],"revision":plan["summary"]["revision"]}),
    )
    .await
    .unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要支持 field TTL 且允许 ACL 管理的隔离 Redis 7.4/8 实例"]
async fn unreadable_field_expiry_rejects_prepare_before_any_write() {
    let mut f = Fixture::new().await;
    let target = f.target("acl-hash", "hash");
    let k = key(&target);
    let _: i64 = redis::cmd("HSET")
        .arg(&k)
        .arg("old")
        .arg("value")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let _: Vec<i64> = redis::cmd("HPEXPIRE")
        .arg(&k)
        .arg(60000)
        .arg("FIELDS")
        .arg(1)
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let username = format!("datapier-replace-{}", Uuid::new_v4());
    let password = Uuid::new_v4().to_string();
    let _: String = redis::cmd("ACL")
        .arg("SETUSER")
        .arg(&username)
        .arg("on")
        .arg(format!(">{password}"))
        .arg("~*")
        .arg("+@all")
        .arg("-hpexpiretime")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let port: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "restricted-hash".into(),
        revision: 1,
        approval_policy: ApprovalPolicy::default(),
        provider: ProviderKind::Redis,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: username.clone(),
            database: None,
            redis_db: 15,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
        }),
    };
    let mut session = ProviderSession::connect(profile, Some(password))
        .await
        .unwrap();
    let task = f
        .store
        .create_task(
            Uuid::new_v4(),
            "redis-acl-hash",
            "execution.start",
            json!({}),
            false,
        )
        .await
        .unwrap();
    let sink = ExecutionSink::new(f.store.clone(), task.id);
    assert!(
        session
            .call(
                "replace.prepare",
                json!({"targets":[target],"spec":spec("old","new", &["hashfield"])}),
                &sink,
                &CancellationToken::new()
            )
            .await
            .is_err()
    );
    let value: String = redis::cmd("HGET")
        .arg(&k)
        .arg("old")
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(value, "value");
    session.close().await.unwrap();
    let _: i64 = redis::cmd("ACL")
        .arg("DELUSER")
        .arg(username)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 隔离实例"]
async fn key_command_and_payload_limits_fail_before_writing() {
    let mut f = Fixture::new().await;
    let targets: Vec<ResourceTarget> = (0..129)
        .map(|n| f.target(&format!("many-{n}"), "string"))
        .collect();
    let error = f
        .call(
            "replace.prepare",
            json!({"targets":targets,"spec":spec("a","b", &["string_value"])}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "invalid_input");
    let list = f.target("many-writes", "list");
    let lk = key(&list);
    let values = vec!["old"; 1001];
    let _: i64 = redis::cmd("RPUSH")
        .arg(&lk)
        .arg(values)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let error = f
        .call(
            "replace.prepare",
            json!({"targets":[list],"spec":spec("old","new", &["list_value"])}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "resource_limit");
    let count: i64 = redis::cmd("LLEN")
        .arg(&lk)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(count, 1001);
    let large = f.target("large-write", "string");
    let sk = key(&large);
    let value = "a".repeat(4 * 1024 * 1024);
    let _: () = redis::cmd("SET")
        .arg(&sk)
        .arg(&value)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let error = f
        .call(
            "replace.prepare",
            json!({"targets":[large],"spec":spec("a","b", &["string_value"])}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "resource_limit");
    let first: String = redis::cmd("GETRANGE")
        .arg(&sk)
        .arg(0)
        .arg(0)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(first, "a");
    f.close().await;
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 隔离实例"]
async fn key_only_rename_does_not_read_large_string_content() {
    let mut f = Fixture::new().await;
    let source = f.target("huge-old", "string");
    let destination = f.target("huge-new", "string");
    let source_key = key(&source);
    let destination_key = key(&destination);
    let _: () = redis::cmd("SET")
        .arg(&source_key)
        .arg(vec![b'x'; 5 * 1024 * 1024])
        .query_async(&mut f.conn)
        .await
        .unwrap();
    let plan = f.call("replace.prepare", json!({"targets":[source],"spec":spec("huge-old","huge-new", &["key"]),"_max_plan_bytes":8192})).await.unwrap();
    assert_eq!(
        f.call("replace.submit", submit(&plan["summary"]))
            .await
            .unwrap()["status"],
        "ok"
    );
    let length: usize = redis::cmd("STRLEN")
        .arg(&destination_key)
        .query_async(&mut f.conn)
        .await
        .unwrap();
    assert_eq!(length, 5 * 1024 * 1024);
    f.close().await;
}
