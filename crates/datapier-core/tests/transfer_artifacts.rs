//! Redis 清理确认不能跨所有者、版本、资源类型或发布状态改变登记事实。
use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::Store;
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn redis_cleanup_confirmation_requires_exact_unknown_artifact_ownership() {
    let root = std::env::temp_dir().join(format!("datapier-artifact-cas-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let owner = Uuid::new_v4();
    let identity = json!({
        "profile_id": Uuid::new_v4(), "redis_db": 15, "kind": "string",
        "temp_b64": STANDARD.encode(format!("__datapier_import_{}", Uuid::new_v4())),
        "marker_b64": STANDARD.encode(Uuid::new_v4().to_string())
    });
    let id = store
        .transfer_register_artifact(owner, None, "redis_temporary", &identity)
        .await
        .unwrap();
    assert_eq!(
        store
            .transfer_confirm_redis_cleanup(id, owner, 1, "cleaned")
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    let revision = store
        .transfer_update_artifact(id, owner, Some(1), "outcome_unknown", None)
        .await
        .unwrap();
    assert!(
        store
            .transfer_update_artifact(id, owner, Some(revision), "cleaned", None)
            .await
            .is_err()
    );
    for (candidate, version) in [(Uuid::new_v4(), revision), (owner, revision + 1)] {
        assert_eq!(
            store
                .transfer_confirm_redis_cleanup(id, candidate, version, "cleaned")
                .await
                .unwrap_err()
                .code,
            "conflict"
        );
    }
    let confirmed = store
        .transfer_confirm_redis_cleanup(id, owner, revision, "cleaned")
        .await
        .unwrap();
    assert_eq!(confirmed, revision + 1);
    assert_eq!(
        store
            .transfer_confirm_redis_cleanup(id, owner, revision, "recovery_conflict")
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    let artifact = store.transfer_artifact(id).await.unwrap();
    assert_eq!(artifact.state, "cleaned");
    assert_eq!(artifact.revision, confirmed);

    let published = store
        .transfer_register_artifact(owner, None, "redis_temporary", &identity)
        .await
        .unwrap();
    let revision = store
        .transfer_update_artifact(published, owner, Some(1), "published", None)
        .await
        .unwrap();
    assert_eq!(
        store
            .transfer_confirm_redis_cleanup(published, owner, revision, "cleaned")
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    assert_eq!(
        store.transfer_artifact(published).await.unwrap().state,
        "published"
    );

    let file = store
        .transfer_register_artifact(owner, None, "report", &json!({}))
        .await
        .unwrap();
    let revision = store
        .transfer_update_artifact(file, owner, Some(1), "outcome_unknown", None)
        .await
        .unwrap();
    assert_eq!(
        store
            .transfer_confirm_redis_cleanup(file, owner, revision, "cleaned")
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    assert_eq!(
        store.transfer_artifact(file).await.unwrap().state,
        "outcome_unknown"
    );
    store.pool.close().await;
    std::fs::remove_dir_all(root).unwrap();
}
