//! 只在显式配置的一次性 MySQL／Redis TLS 测试实例上运行。
//! 必填：DATAPIER_TLS_MYSQL_PORT、DATAPIER_TLS_MYSQL_SECRET、
//! DATAPIER_TLS_REDIS_PORT、DATAPIER_TLS_CA、DATAPIER_TLS_WRONG_CA。
//! DATAPIER_TLS_BAD_HOST 默认使用 127.1：它连接同一回环服务，
//! 但不匹配仅包含 IP 127.0.0.1 和 DNS localhost 的证书，用于区分连通与身份验证。
//! MySQL 5.7 测试使用 DATAPIER_TLS_MYSQL57_PORT、DATAPIER_TLS_MYSQL57_SECRET，服务端须启用 require_secure_transport。
use datapier_core::{ExecutionSink, Store, model::*, providers::ProviderSession};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name} for isolated TLS fixtures"))
}

struct TempStore(std::path::PathBuf);
impl Drop for TempStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn exercise(kind: ProviderKind) {
    let (port, secret) = match kind {
        ProviderKind::Mysql => (
            env("DATAPIER_TLS_MYSQL_PORT"),
            Some(env("DATAPIER_TLS_MYSQL_SECRET")),
        ),
        ProviderKind::Redis => (env("DATAPIER_TLS_REDIS_PORT"), None),
    };
    let mut profile = Profile {
        id: Uuid::new_v4(),
        name: "tls-fixture".into(),
        revision: 1,
        approval_policy: ApprovalPolicy::Dangerous,
        provider: kind,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port: port.parse().unwrap(),
            username: if kind == ProviderKind::Mysql {
                "root".into()
            } else {
                String::new()
            },
            database: None,
            redis_db: 0,
            redis_databases_override: None,
            tls: TlsConfig {
                enabled: true,
                ca_path: Some(env("DATAPIER_TLS_CA")),
                ..Default::default()
            },
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    let root =
        TempStore(std::env::temp_dir().join(format!("datapier-tls-test-{}", Uuid::new_v4())));
    let store = Store::open(&root.0).await.unwrap();
    let task = store
        .create_task(
            Uuid::new_v4(),
            "tls-fixture",
            "execution.start",
            json!({}),
            false,
        )
        .await
        .unwrap();
    let sink = ExecutionSink::new(store.clone(), task.id);
    let mut session = ProviderSession::connect(profile.clone(), secret.clone())
        .await
        .unwrap();
    let text = if kind == ProviderKind::Mysql {
        "SHOW STATUS LIKE 'Ssl_cipher'"
    } else {
        "PING"
    };
    let result = session
        .call(
            "execution.run",
            json!({"text":text}),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let id: Uuid = serde_json::from_value(result["results"][0].clone()).unwrap();
    let page = store.result_page(id, 0, 10, 65536).await.unwrap();
    if kind == ProviderKind::Mysql {
        assert_eq!(page.rows[0][0].text(), "Ssl_cipher");
        assert!(
            !page.rows[0][1].text().is_empty(),
            "MySQL must negotiate a TLS cipher"
        );
    } else {
        assert!(
            page.rows
                .iter()
                .flatten()
                .any(|v| matches!(v, Value::Json(reply) if reply["value"] == "PONG")),
            "unexpected Redis PING result: {:?}",
            page.rows
        );
    }
    session.close().await.unwrap();
    profile.direct_mut().unwrap().tls.ca_path = Some(env("DATAPIER_TLS_WRONG_CA"));
    let err = match ProviderSession::connect(profile.clone(), secret.clone()).await {
        Ok(_) => panic!("wrong CA accepted"),
        Err(e) => e,
    };
    assert_ne!(
        err.code, "tls_required",
        "wrong CA must remain a certificate error"
    );
    let message = err.to_string().to_lowercase();
    assert!(
        message.contains("cert") || message.contains("issuer") || message.contains("tls"),
        "wrong CA failed for unrelated reason: {err}"
    );
    profile.direct_mut().unwrap().tls.ca_path = Some(env("DATAPIER_TLS_CA"));
    profile.direct_mut().unwrap().host =
        std::env::var("DATAPIER_TLS_BAD_HOST").unwrap_or_else(|_| "127.1".into());
    let err = match ProviderSession::connect(profile, secret).await {
        Ok(_) => panic!("wrong server identity accepted"),
        Err(e) => e,
    };
    assert_ne!(
        err.code, "tls_required",
        "wrong identity must remain a certificate error"
    );
    let message = err.to_string().to_lowercase();
    assert!(
        message.contains("name")
            || message.contains("identity")
            || message.contains("valid for")
            || message.contains("notvalidfor"),
        "identity failed for unrelated reason: {err}"
    );
    store.pool.close().await;
}

#[tokio::test]
#[ignore = "requires explicit isolated TLS-only MySQL 8.4 fixture"]
async fn mysql_tls_verifies_ca_and_identity() {
    exercise(ProviderKind::Mysql).await;
}

#[tokio::test]
#[ignore = "requires isolated MySQL 5.7 with require_secure_transport=ON and TLS fixtures"]
async fn mysql_57_secure_transport_requires_and_negotiates_tls() {
    let mut profile = Profile {
        id: Uuid::new_v4(),
        name: "mysql-57-secure-transport".into(),
        revision: 1,
        approval_policy: ApprovalPolicy::Dangerous,
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port: env("DATAPIER_TLS_MYSQL57_PORT").parse().unwrap(),
            username: "root".into(),
            database: None,
            redis_db: 0,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    let secret = Some(env("DATAPIER_TLS_MYSQL57_SECRET"));
    let err = match ProviderSession::connect(profile.clone(), secret.clone()).await {
        Ok(_) => panic!("secure transport server accepted a plaintext connection"),
        Err(err) => err,
    };
    assert_eq!(err.code, "tls_required");
    assert_eq!(err.details["mysql_errno"], 3159);
    assert_eq!(err.details["sqlstate"], "HY000");
    assert_eq!(err.database_code.as_deref(), Some("HY000"));
    assert!(
        err.message.contains("3159"),
        "original cause missing: {err}"
    );

    profile.direct_mut().unwrap().tls.enabled = true;
    profile.direct_mut().unwrap().tls.ca_path = Some(env("DATAPIER_TLS_CA"));
    let mut session = ProviderSession::connect(profile, secret).await.unwrap();
    let root =
        TempStore(std::env::temp_dir().join(format!("datapier-tls-test-{}", Uuid::new_v4())));
    let store = Store::open(&root.0).await.unwrap();
    let task = store
        .create_task(
            Uuid::new_v4(),
            "tls-fixture",
            "execution.start",
            json!({}),
            false,
        )
        .await
        .unwrap();
    let sink = ExecutionSink::new(store.clone(), task.id);
    let result = session
        .call(
            "execution.run",
            json!({"text":"SELECT VERSION(), @@GLOBAL.require_secure_transport"}),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let id: Uuid = serde_json::from_value(result["results"][0].clone()).unwrap();
    let page = store.result_page(id, 0, 10, 65536).await.unwrap();
    assert!(
        page.rows[0][0].text().starts_with("5.7."),
        "expected MySQL 5.7: {:?}",
        page.rows
    );
    assert_eq!(
        page.rows[0][1].text(),
        "1",
        "require_secure_transport must be ON"
    );
    let result = session
        .call(
            "execution.run",
            json!({"text":"SHOW STATUS LIKE 'Ssl_cipher'"}),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let id: Uuid = serde_json::from_value(result["results"][0].clone()).unwrap();
    let page = store.result_page(id, 0, 10, 65536).await.unwrap();
    assert_eq!(page.rows[0][0].text(), "Ssl_cipher");
    assert!(
        !page.rows[0][1].text().is_empty(),
        "MySQL must negotiate a TLS cipher"
    );
    session.close().await.unwrap();
    store.pool.close().await;
}

#[tokio::test]
#[ignore = "requires explicit isolated TLS-only Redis 8 fixture"]
async fn redis_tls_verifies_ca_and_identity() {
    exercise(ProviderKind::Redis).await;
}
