//! 配置库重开时的 Archery 服务数据预检。
use datapier_core::Store;
use uuid::Uuid;

#[tokio::test]
async fn server_preflight_rejects_invalid_rows_without_changing_them() {
    let root = std::env::temp_dir().join(format!("datapier-migration-server-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let cases = [
        ("", "https://example.com/", "alice", "{}"),
        ("  ", "https://example.com/", "alice", "{}"),
        ("服务", "not a url", "alice", "{}"),
        ("服务", "ftp://example.com/", "alice", "{}"),
        ("服务", "https://user@example.com/", "alice", "{}"),
        ("服务", "https://example.com/?query=1", "alice", "{}"),
        ("服务", "https://example.com/#fragment", "alice", "{}"),
        ("服务", "https://EXAMPLE.com/", "alice", "{}"),
        ("服务", "https://example.com", "alice", "{}"),
        ("服务", "https://example.com:443/", "alice", "{}"),
        ("服务", "https://example.com/", "  ", "{}"),
        (
            "服务",
            "https://example.com/",
            "alice",
            r#"{"ca_path":"  "}"#,
        ),
        ("服务", "https://example.com/", "alice", r#"{"ca_path":42}"#),
    ];
    for (name, url, username, tls) in cases {
        sqlx::query("DELETE FROM archery_servers")
            .execute(&mut *store.pool.acquire().await.unwrap())
            .await
            .unwrap();
        sqlx::query("INSERT INTO archery_servers(id,name,base_url,username,http_tls,revision) VALUES(?,?,?,?,?,1)")
            .bind(Uuid::new_v4().to_string()).bind(name).bind(url).bind(username).bind(tls)
            .execute(&mut *store.pool.acquire().await.unwrap()).await.unwrap();
        let before: Vec<(String, String, String, String, String, i64)> = sqlx::query_as(
            "SELECT id,name,base_url,username,http_tls,revision FROM archery_servers ORDER BY id",
        )
        .fetch_all(&mut *store.pool.acquire().await.unwrap())
        .await
        .unwrap();
        let error = match Store::open(&root).await {
            Ok(_) => panic!("错误服务配置被接受: {name:?} {url:?} {username:?} {tls:?}"),
            Err(error) => error,
        };
        assert_eq!(error.code, "storage_version");
        let after: Vec<(String, String, String, String, String, i64)> = sqlx::query_as(
            "SELECT id,name,base_url,username,http_tls,revision FROM archery_servers ORDER BY id",
        )
        .fetch_all(&mut *store.pool.acquire().await.unwrap())
        .await
        .unwrap();
        assert_eq!(before, after);
    }
    store.pool.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn server_preflight_checks_normalized_identity_and_accepts_valid_rows() {
    let root = std::env::temp_dir().join(format!("datapier-migration-identity-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let insert = "INSERT INTO archery_servers(id,name,base_url,username,http_tls,revision,totp_mode) VALUES(?,?,?,?,?,1,?)";
    sqlx::query(insert)
        .bind(Uuid::new_v4().to_string())
        .bind("服务一")
        .bind("https://example.com/prefix/")
        .bind("alice")
        .bind(r#"{"ca_path":"/tmp/ca.pem"}"#)
        .bind("automatic")
        .execute(&mut *store.pool.acquire().await.unwrap())
        .await
        .unwrap();
    let reopened = Store::open(&root).await.unwrap();
    assert_eq!(reopened.archery_servers().await.unwrap().len(), 1);
    reopened.pool.close().await;
    sqlx::query(insert)
        .bind(Uuid::new_v4().to_string())
        .bind("服务二")
        .bind("https://EXAMPLE.com:443/prefix")
        .bind("alice")
        .bind("{}")
        .bind("manual")
        .execute(&mut *store.pool.acquire().await.unwrap())
        .await
        .unwrap();
    let before: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id,base_url,username FROM archery_servers ORDER BY id")
            .fetch_all(&mut *store.pool.acquire().await.unwrap())
            .await
            .unwrap();
    let error = match Store::open(&root).await {
        Ok(_) => panic!("规范化后冲突的服务被接受"),
        Err(error) => error,
    };
    assert_eq!(error.code, "storage_version");
    let after: Vec<(String, String, String)> =
        sqlx::query_as("SELECT id,base_url,username FROM archery_servers ORDER BY id")
            .fetch_all(&mut *store.pool.acquire().await.unwrap())
            .await
            .unwrap();
    assert_eq!(before, after);
    store.pool.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
}
