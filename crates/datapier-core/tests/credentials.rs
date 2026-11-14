//! 需要本机已解锁的系统凭据服务；不连接业务数据库。
use datapier_core::{Store, model::*};
use futures_util::FutureExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use uuid::Uuid;

#[test]
fn mysql_options_preserve_legacy_defaults_and_reject_unsafe_values() {
    let legacy = json!({"host":"localhost","port":3306});
    let mut direct: DirectConnection = serde_json::from_value(legacy).unwrap();
    assert_eq!(direct.mysql.timezone.as_deref(), Some("+00:00"));
    assert_eq!(direct.mysql.connect_timeout_seconds, None);
    direct.mysql.timezone = None;
    direct.mysql.connect_timeout_seconds = Some(15);
    direct.mysql.query_timeout_seconds = Some(60);
    let saved = serde_json::to_value(&direct).unwrap();
    let restored: DirectConnection = serde_json::from_value(saved).unwrap();
    assert_eq!(restored.mysql, direct.mysql);
    for value in ["SYSTEM", "+14:00", "-13:59", "Asia/Shanghai", "Etc/GMT+8"] {
        direct.mysql.timezone = Some(value.into());
        assert!(direct.mysql.validate().is_ok(), "{value}");
    }
    for value in [
        "+14:01",
        "-14:00",
        "UTC' OR 1=1",
        "UTC\\x",
        "Asia/ Shanghai",
    ] {
        direct.mysql.timezone = Some(value.into());
        assert!(direct.mysql.validate().is_err(), "{value}");
    }
    direct.mysql.timezone = None;
    for seconds in [0, 86_401] {
        direct.mysql.query_timeout_seconds = Some(seconds);
        assert!(direct.mysql.validate().is_err());
    }
}

#[tokio::test]
#[ignore = "需显式启用本机系统凭据服务验收"]
async fn credentials_survive_store_reopen_without_entering_sqlite() {
    assert_eq!(std::env::var("DATAPIER_TEST_KEYRING").as_deref(), Ok("1"));
    let root = std::env::temp_dir().join(format!("datapier-credentials-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "凭据服务隔离验收".into(),
        revision: 1,
        approval_policy: ApprovalPolicy::Dangerous,
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadOnly,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port: 1,
            username: "datapier-test".into(),
            database: None,
            redis_db: 0,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    let secret = Uuid::new_v4().to_string();
    let account = format!(
        "{:x}:{}",
        Sha256::digest(store.root.to_string_lossy().as_bytes()),
        profile.id
    );
    let entry = keyring::v1::Entry::new("io.datapier.credentials", &account).unwrap();
    let outcome = std::panic::AssertUnwindSafe(async {
        assert!(
            store
                .save_profile(&profile, Some(secret.clone()), true)
                .await
                .unwrap(),
            "系统凭据服务未接受保存，不能宣称持久凭据验收通过"
        );
        let payload: String = sqlx::query_scalar("SELECT payload FROM profiles WHERE id=?")
            .bind(profile.id.to_string())
            .fetch_one(&mut *store.pool.acquire().await.unwrap())
            .await
            .unwrap();
        assert!(!payload.contains(&secret));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&payload).unwrap()["name"],
            json!(profile.name)
        );
        let reopened = Store::open(&root).await.unwrap();
        assert_eq!(
            reopened.secret(profile.id).await.unwrap(),
            Some(secret.clone())
        );
        // 未提供新密码时只更新配置，省略 save_secret 不应清除既有凭据。
        assert!(reopened.save_profile(&profile, None, false).await.unwrap());
        assert_eq!(entry.get_secret().unwrap(), secret.as_bytes());
        let profile = reopened.profile(profile.id).await.unwrap();
        assert!(
            !reopened
                .save_profile(&profile, Some("session-only".into()), false)
                .await
                .unwrap()
        );
        assert_eq!(
            reopened.secret(profile.id).await.unwrap().as_deref(),
            Some("session-only")
        );
        assert!(matches!(
            entry.get_secret(),
            Err(keyring::v1::Error::NoEntry)
        ));
        reopened.delete_profile(profile.id).await.unwrap();
        assert_eq!(reopened.secret(profile.id).await.unwrap(), None);
        reopened.pool.close().await;
    })
    .catch_unwind()
    .await;
    let cleanup = store.delete_profile(profile.id).await;
    let native_cleanup = entry.delete_credential();
    store.pool.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    cleanup.unwrap();
    assert!(matches!(
        native_cleanup,
        Ok(()) | Err(keyring::v1::Error::NoEntry)
    ));
}
