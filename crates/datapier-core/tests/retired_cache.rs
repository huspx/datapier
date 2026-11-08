//! 退休缓存清理以用户确认的文件集合为边界。
use datapier_core::Store;
use uuid::Uuid;

#[tokio::test]
async fn changed_cache_rejects_old_confirmation_and_preserves_all_files() {
    let root = std::env::temp_dir().join(format!("datapier-retired-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    tokio::fs::create_dir_all(root.join("values/nested"))
        .await
        .unwrap();
    tokio::fs::write(root.join("cache.sqlite3"), b"retired cache")
        .await
        .unwrap();
    tokio::fs::write(root.join("values/nested/first"), b"first")
        .await
        .unwrap();
    let preview = store.retired_cache_preview().await.unwrap();
    tokio::fs::write(root.join("values/second"), b"added after confirmation")
        .await
        .unwrap();
    let error = store.clear_retired_cache(&preview).await.unwrap_err();
    assert_eq!(error.code, "conflict");
    assert_eq!(
        tokio::fs::read(root.join("cache.sqlite3")).await.unwrap(),
        b"retired cache"
    );
    assert_eq!(
        tokio::fs::read(root.join("values/nested/first"))
            .await
            .unwrap(),
        b"first"
    );
    assert_eq!(
        tokio::fs::read(root.join("values/second")).await.unwrap(),
        b"added after confirmation"
    );
    let confirmed = store.retired_cache_preview().await.unwrap();
    store.clear_retired_cache(&confirmed).await.unwrap();
    assert!(!root.join("cache.sqlite3").exists());
    assert!(!root.join("values").exists());
    assert!(root.join("datapier.sqlite3").exists());
    store.pool.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn clearing_retired_symlinks_never_follows_their_targets() {
    let root = std::env::temp_dir().join(format!("datapier-retired-link-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    tokio::fs::create_dir(root.join("retained")).await.unwrap();
    tokio::fs::write(root.join("retained/important"), b"keep")
        .await
        .unwrap();
    std::os::unix::fs::symlink(root.join("retained"), root.join("values")).unwrap();
    let confirmed = store.retired_cache_preview().await.unwrap();
    store.clear_retired_cache(&confirmed).await.unwrap();
    assert!(!root.join("values").exists());
    assert_eq!(
        tokio::fs::read(root.join("retained/important"))
            .await
            .unwrap(),
        b"keep"
    );
    store.pool.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
}
