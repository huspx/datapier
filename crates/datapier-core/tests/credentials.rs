//! 需要本机已解锁的系统凭据服务；不连接业务数据库。
use datapier_core::{Store, model::*};
use futures_util::FutureExt;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
#[ignore = "需显式启用本机系统凭据服务验收"]
async fn credentials_survive_store_reopen_without_entering_sqlite() {
    assert_eq!(std::env::var("DATAPIER_TEST_KEYRING").as_deref(), Ok("1"));
    let root = std::env::temp_dir().join(format!("datapier-credentials-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "凭据服务隔离验收".into(),
        provider: ProviderKind::Mysql,
        host: "127.0.0.1".into(),
        port: 1,
        username: "datapier-test".into(),
        database: None,
        redis_db: 0,
        access_mode: AccessMode::ReadOnly,
        tls: TlsConfig::default(),
    };
    let secret = Uuid::new_v4().to_string();
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
            .fetch_one(&store.pool)
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
        reopened.delete_profile(profile.id).await.unwrap();
        assert_eq!(reopened.secret(profile.id).await.unwrap(), None);
        reopened.pool.close().await;
        reopened.cache.close().await;
    })
    .catch_unwind()
    .await;
    let cleanup = store.delete_profile(profile.id).await;
    store.pool.close().await;
    store.cache.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
    cleanup.unwrap();
}
