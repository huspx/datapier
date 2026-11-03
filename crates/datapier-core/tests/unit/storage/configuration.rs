use super::*;

#[tokio::test]
async fn baseline_reopen_preserves_profiles_settings_and_workspace() {
    let root = std::env::temp_dir().join(format!("datapier-migration-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let pool = open_database(&root.join("datapier.sqlite3")).await.unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0001_config.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("PRAGMA user_version=1")
        .execute(&pool)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    let profile = json!({"id":id,"name":"旧连接","provider":"mysql","access_mode":"read_only","host":"localhost","port":3306,"username":"legacy","database":"sales","redis_db":3,"tls":{"enabled":true,"ca_path":"ca.pem","client_cert_path":"cert.pem","client_key_path":"key.pem"}});
    let workspace = json!({"active":0,"pending":null,"navigation_expanded":true,"tabs":[{"id":Uuid::new_v4(),"title":"orders","profile_id":id,"query":{"text":"SELECT * FROM orders","cursor":0,"anchor":0},"cli_query":{"text":"","cursor":0,"anchor":0},"layout":[{"width":24,"hidden":false,"frozen":false}],"browse":{"database":"sales"},"pending":{"target":{"database":"sales","name":"orders","kind":"table"},"rows":[{"id":7,"status":"draft"}],"redis":[],"edits":[],"rows_include_edits":true,"requires_refresh":true}}]});
    sqlx::query("INSERT INTO profiles(id,name,payload) VALUES(?,?,?)")
        .bind(id.to_string())
        .bind("旧连接")
        .bind(profile.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspaces(id,revision,payload) VALUES('main',3,?)")
        .bind(workspace.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let store = Store::open(&root).await.unwrap();
    let migrated = store.profiles().await.unwrap().remove(0);
    assert_eq!(migrated.id, id);
    assert_eq!(migrated.access_mode, AccessMode::ReadOnly);
    let direct = migrated.direct().unwrap();
    assert_eq!(
        (
            &direct.host,
            direct.port,
            &direct.username,
            direct.database.as_deref(),
            direct.redis_db
        ),
        (
            &"localhost".to_string(),
            3306,
            &"legacy".to_string(),
            Some("sales"),
            3
        )
    );
    assert!(direct.tls.enabled);
    assert_eq!(direct.tls.ca_path.as_deref(), Some("ca.pem"));
    assert_eq!(direct.tls.client_cert_path.as_deref(), Some("cert.pem"));
    assert_eq!(direct.tls.client_key_path.as_deref(), Some("key.pem"));
    assert_eq!(
        store.profiles().await.unwrap()[0]
            .direct()
            .unwrap()
            .visible_databases,
        None
    );
    assert_eq!(
        store.workspace_load("main").await.unwrap(),
        json!({"revision":3,"state":workspace})
    );
    assert_eq!(store.settings().await.unwrap(), AppSettings::default());
    assert_eq!(
        store
            .workspace_save("main", 3, workspace.clone())
            .await
            .unwrap(),
        4
    );
    let mut profile = store.profiles().await.unwrap().remove(0);
    profile.direct_mut().unwrap().visible_databases = Some(vec![]);
    store.save_profile(&profile, None, false).await.unwrap();
    store
        .set_settings(&AppSettings {
            language: LanguagePreference::En,
        })
        .await
        .unwrap();
    store.pool.close().await;
    store.cache.close().await;
    drop(store);

    let store = Store::open(&root).await.unwrap();
    assert_eq!(
        store.workspace_load("main").await.unwrap(),
        json!({"revision":4,"state":workspace})
    );
    assert_eq!(
        store.profiles().await.unwrap()[0]
            .direct()
            .unwrap()
            .visible_databases,
        Some(vec![])
    );
    assert_eq!(
        store.settings().await.unwrap().language,
        LanguagePreference::En
    );
    cleanup(store).await;
}

#[tokio::test]
async fn incomplete_and_newer_baselines_are_rejected() {
    for cache in [false, true] {
        let store = fixture().await;
        let pool = if cache { &store.cache } else { &store.pool };
        let statement = if cache {
            "DROP TABLE value_files"
        } else {
            "DROP TABLE app_settings"
        };
        sqlx::query(statement).execute(pool).await.unwrap();
        let root = (*store.root).clone();
        store.pool.close().await;
        store.cache.close().await;
        drop(store);
        assert_eq!(
            Store::open(&root).await.err().unwrap().code,
            "storage_version"
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
    let store = fixture().await;
    sqlx::query("PRAGMA user_version=3")
        .execute(&store.pool)
        .await
        .unwrap();
    let root = (*store.root).clone();
    store.pool.close().await;
    store.cache.close().await;
    drop(store);
    assert_eq!(
        Store::open(&root).await.err().unwrap().code,
        "storage_version"
    );
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn stale_workspace_save_cannot_overwrite_other_window() {
    let store = fixture().await;
    assert_eq!(
        store
            .workspace_save("shared", 0, json!({"sql":"SELECT '中😀'"}))
            .await
            .unwrap(),
        1
    );
    let conflict = store
        .workspace_save("shared", 0, json!({"sql":"stale"}))
        .await
        .unwrap_err();
    assert_eq!(conflict.code, "conflict");
    assert_eq!(
        store.workspace_load("shared").await.unwrap()["state"]["sql"],
        "SELECT '中😀'"
    );
    assert_eq!(
        store
            .workspace_save("shared", 1, json!({"sql":"SELECT 2"}))
            .await
            .unwrap(),
        2
    );
    let root = (*store.root).clone();
    store.pool.close().await;
    store.cache.close().await;
    let reopened = Store::open(&root).await.unwrap();
    assert_eq!(
        reopened.workspace_load("shared").await.unwrap(),
        json!({"revision":2,"state":{"sql":"SELECT 2"}})
    );
    cleanup(reopened).await;
}

#[tokio::test]
async fn workspace_revision_cannot_overflow_sqlite_integer() {
    let store = fixture().await;
    sqlx::query("INSERT INTO workspaces(id,revision,payload) VALUES('limit',?,?)")
        .bind(i64::MAX)
        .bind("{}")
        .execute(&store.pool)
        .await
        .unwrap();
    let error = store
        .workspace_save("limit", i64::MAX as u64, json!({"lost":true}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "invalid_input");
    assert_eq!(
        store.workspace_load("limit").await.unwrap(),
        json!({"revision":i64::MAX,"state":{}})
    );
    cleanup(store).await;
}

#[tokio::test]
async fn failed_preflight_preserves_configuration_and_cache() {
    let root = std::env::temp_dir().join(format!("datapier-migration-failure-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let pool = open_database(&root.join("datapier.sqlite3")).await.unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0001_config.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("PRAGMA user_version=1")
        .execute(&pool)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    let payload = json!({"id":id,"name":"broken","provider":"mysql","host":"localhost","port":0});
    sqlx::query("INSERT INTO profiles(id,name,payload,credential_saved) VALUES(?,?,?,1)")
        .bind(id.to_string())
        .bind("broken")
        .bind(payload.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(Store::open(&root).await.is_err());
    let pool = open_database(&root.join("datapier.sqlite3")).await.unwrap();
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(version, 1);
    let row = sqlx::query("SELECT payload,credential_saved FROM profiles WHERE id=?")
        .bind(id.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(serde_json::from_str::<Json>(row.get(0)).unwrap(), payload);
    assert_eq!(row.get::<i64, _>(1), 1);
    let cache = open_database(&root.join("cache.sqlite3")).await.unwrap();
    let cache_version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&cache)
        .await
        .unwrap();
    assert_eq!(cache_version, 0);
    let repaired =
        json!({"id":id,"name":"broken","provider":"mysql","host":"localhost","port":3306});
    sqlx::query("UPDATE profiles SET payload=? WHERE id=?")
        .bind(repaired.to_string())
        .bind(id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    cache.close().await;
    let store = Store::open(&root).await.unwrap();
    assert_eq!(store.profiles().await.unwrap()[0].id, id);
    assert_eq!(
        store.profiles().await.unwrap()[0].direct().unwrap().port,
        3306
    );
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&store.cache)
        .await
        .unwrap();
    assert_eq!(version, 1);
    let saved: i64 = sqlx::query_scalar("SELECT credential_saved FROM profiles WHERE id=?")
        .bind(id.to_string())
        .fetch_one(&store.pool)
        .await
        .unwrap();
    assert_eq!(saved, 1);
    cleanup(store).await;
}

#[tokio::test]
async fn incompatible_development_schema_is_preserved() {
    let root = std::env::temp_dir().join(format!("datapier-dev-schema-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let pool = open_database(&root.join("datapier.sqlite3")).await.unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0001_config.sql"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("PRAGMA user_version=2")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    assert!(Store::open(&root).await.is_err());
    let pool = open_database(&root.join("datapier.sqlite3")).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM app_settings")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    pool.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
}
