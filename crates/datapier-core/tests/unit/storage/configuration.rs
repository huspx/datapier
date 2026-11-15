use super::*;

#[tokio::test]
async fn schema_one_migrates_without_creating_result_cache() {
    let root = std::env::temp_dir().join(format!("datapier-migration-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let mut connection = SqliteConnectOptions::new()
        .filename(root.join("datapier.sqlite3"))
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    sqlx::raw_sql(include_str!("../../../migrations/0001_config.sql"))
        .execute(&mut connection)
        .await
        .unwrap();
    sqlx::query("PRAGMA user_version=1")
        .execute(&mut connection)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    let profile = json!({"id":id,"name":"旧连接","provider":"mysql","access_mode":"read_only","host":"localhost","port":3306,"username":"legacy","database":"sales","redis_db":3,"tls":{"enabled":true}});
    sqlx::query("INSERT INTO profiles(id,name,payload,credential_saved) VALUES(?,?,?,1)")
        .bind(id.to_string())
        .bind("旧连接")
        .bind(profile.to_string())
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    let store = Store::open(&root).await.unwrap();
    let profile = store.profile(id).await.unwrap();
    assert_eq!(profile.direct().unwrap().database.as_deref(), Some("sales"));
    assert_eq!(profile.revision, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT credential_saved FROM profiles WHERE id=?")
            .bind(id.to_string())
            .fetch_one(&mut *store.pool.acquire().await.unwrap())
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut *store.pool.acquire().await.unwrap())
            .await
            .unwrap(),
        5
    );
    assert!(!root.join("cache.sqlite3").exists());
    assert!(!root.join("values").exists());
    cleanup(store).await;
}

#[tokio::test]
async fn incompatible_v5_candidate_is_rejected_without_changing_records() {
    let root = std::env::temp_dir().join(format!("datapier-index-migration-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let options = SqliteConnectOptions::new()
        .filename(root.join("datapier.sqlite3"))
        .create_if_missing(true);
    let mut connection = options.clone().connect().await.unwrap();
    for migration in [
        include_str!("../../../migrations/0001_config.sql"),
        include_str!("../../../migrations/0002_config.sql"),
        include_str!("../../../migrations/0003_config.sql"),
        include_str!("../../../migrations/0004_config.sql"),
    ] {
        sqlx::raw_sql(migration)
            .execute(&mut connection)
            .await
            .unwrap();
    }
    let legacy = include_str!("../../../migrations/0005_config.sql").replace(", 'input_index'", "");
    sqlx::raw_sql(sqlx::AssertSqlSafe(legacy.as_str()))
        .execute(&mut connection)
        .await
        .unwrap();
    let owner = Uuid::new_v4();
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO transfer_artifacts VALUES (?,NULL,?,'report','ready','{}',1)")
        .bind(id.to_string())
        .bind(owner.to_string())
        .execute(&mut connection)
        .await
        .unwrap();
    connection.close().await.unwrap();
    assert_eq!(
        Store::open(&root).await.err().unwrap().code,
        "storage_version"
    );
    let mut connection = options.connect().await.unwrap();
    let kind: String = sqlx::query_scalar("SELECT kind FROM transfer_artifacts WHERE id=?")
        .bind(id.to_string())
        .fetch_one(&mut connection)
        .await
        .unwrap();
    assert_eq!(kind, "report");
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&mut connection)
            .await
            .unwrap(),
        5
    );
    connection.close().await.unwrap();
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn schema_four_migrates_and_keeps_binding_confirmations_local() {
    let root = std::env::temp_dir().join(format!("datapier-binding-migration-{}", Uuid::new_v4()));
    tokio::fs::create_dir_all(&root).await.unwrap();
    let mut connection = SqliteConnectOptions::new()
        .filename(root.join("datapier.sqlite3"))
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    for migration in [
        include_str!("../../../migrations/0001_config.sql"),
        include_str!("../../../migrations/0002_config.sql"),
        include_str!("../../../migrations/0003_config.sql"),
        include_str!("../../../migrations/0004_config.sql"),
    ] {
        sqlx::raw_sql(migration)
            .execute(&mut connection)
            .await
            .unwrap();
    }
    connection.close().await.unwrap();
    let store = Store::open(&root).await.unwrap();
    let id = Uuid::new_v4();
    let digest = "a".repeat(64);
    assert!(
        !store
            .project_binding_confirmed(id, "root-a", "default", &digest)
            .await
            .unwrap()
    );
    store
        .project_confirm_binding(id, "root-a", "default", &digest)
        .await
        .unwrap();
    assert!(
        store
            .project_binding_confirmed(id, "root-a", "default", &digest)
            .await
            .unwrap()
    );
    assert!(
        !store
            .project_binding_confirmed(id, "root-b", "default", &digest)
            .await
            .unwrap()
    );
    assert!(
        !store
            .project_binding_confirmed(id, "root-a", "default", &"b".repeat(64))
            .await
            .unwrap()
    );
    cleanup(store).await;
}

#[tokio::test]
async fn incomplete_or_newer_schema_is_rejected_without_repair() {
    for newer in [false, true] {
        let root = std::env::temp_dir().join(format!("datapier-schema-reject-{}", Uuid::new_v4()));
        tokio::fs::create_dir_all(&root).await.unwrap();
        let options = SqliteConnectOptions::new()
            .filename(root.join("datapier.sqlite3"))
            .create_if_missing(true);
        let mut connection = options.clone().connect().await.unwrap();
        for migration in [
            include_str!("../../../migrations/0001_config.sql"),
            include_str!("../../../migrations/0002_config.sql"),
            include_str!("../../../migrations/0003_config.sql"),
            include_str!("../../../migrations/0004_config.sql"),
            include_str!("../../../migrations/0005_config.sql"),
        ] {
            sqlx::raw_sql(migration)
                .execute(&mut connection)
                .await
                .unwrap();
        }
        if newer {
            sqlx::query("PRAGMA user_version=6")
                .execute(&mut connection)
                .await
                .unwrap();
        } else {
            sqlx::query("DROP TABLE project_binding_confirmations")
                .execute(&mut connection)
                .await
                .unwrap();
        }
        connection.close().await.unwrap();
        assert_eq!(
            Store::open(&root).await.err().unwrap().code,
            "storage_version"
        );
        let mut connection = options.connect().await.unwrap();
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut connection)
            .await
            .unwrap();
        assert_eq!(version, if newer { 6 } else { 5 });
        let tables: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='project_binding_confirmations'").fetch_one(&mut connection).await.unwrap();
        assert_eq!(tables, if newer { 1 } else { 0 });
        connection.close().await.unwrap();
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}

#[tokio::test]
async fn settings_cas_rejects_stale_changes() {
    let store = fixture().await;
    let mut first = store.settings().await.unwrap();
    let stale = first.clone();
    first.language = LanguagePreference::En;
    store.set_settings(&first).await.unwrap();
    assert_eq!(
        store.set_settings(&stale).await.unwrap_err().code,
        "conflict"
    );
    assert_eq!(store.settings().await.unwrap().revision, 2);
    cleanup(store).await;
}

#[tokio::test]
async fn approval_decision_is_atomic_and_bound_to_owner() {
    let store = fixture().await;
    let owner = Uuid::new_v4();
    let approval = ApprovalRecord {
        id: Uuid::new_v4(),
        execution_id: Uuid::new_v4(),
        owner,
        session_id: Uuid::new_v4(),
        profile_id: Uuid::new_v4(),
        request: json!({"text":"DELETE FROM t"}),
        digest: "digest".into(),
        profile_revision: 1,
        policy_revision: 1,
        created_at: 1,
        expires_at: i64::MAX,
        state: "pending".into(),
        decided_at: None,
        decision_source: None,
    };
    store.create_approval(&approval).await.unwrap();
    assert!(
        store
            .decide_approval(approval.id, Uuid::new_v4(), "approve", "test")
            .await
            .is_err()
    );
    assert_eq!(
        store
            .decide_approval(approval.id, owner, "approve", "test")
            .await
            .unwrap()
            .state,
        "dispatching"
    );

    assert_eq!(
        store
            .decide_approval(approval.id, owner, "reject", "test")
            .await
            .unwrap()
            .state,
        "dispatching"
    );
    cleanup(store).await;
}

#[tokio::test]
async fn expired_decisions_atomically_finish_the_unsent_execution() {
    let store = fixture().await;
    let owner = Uuid::new_v4();
    for decision in ["cancel", "invalidate", "approve"] {
        let task = store
            .create_task(
                owner,
                "approval",
                "execution.start",
                json!({"text":"DELETE FROM t"}),
                true,
            )
            .await
            .unwrap();
        store
            .update_task(task.id, "approval_required", json!({}), None)
            .await
            .unwrap();
        let approval = ApprovalRecord {
            id: Uuid::new_v4(),
            execution_id: task.id,
            owner,
            session_id: Uuid::new_v4(),
            profile_id: Uuid::new_v4(),
            request: json!({"text":"DELETE FROM t"}),
            digest: "frozen".into(),
            profile_revision: 1,
            policy_revision: 1,
            created_at: 1,
            expires_at: 2,
            state: "pending".into(),
            decided_at: None,
            decision_source: None,
        };
        store.create_approval(&approval).await.unwrap();
        let decided = store
            .decide_approval(approval.id, owner, decision, "test")
            .await
            .unwrap();
        assert_eq!(decided.state, "expired");
        let ended = store.task(task.id).await.unwrap();
        assert_eq!(ended.state, "expired");
        assert!(!ended.sent);
        store
            .decide_approval(approval.id, owner, "approve", "retry")
            .await
            .unwrap();
        assert_eq!(store.task(task.id).await.unwrap().state, "expired");
    }
    cleanup(store).await;
}

#[tokio::test]
async fn shutdown_waits_for_detached_index_writer_to_close() {
    let root = std::env::temp_dir().join(format!("datapier-index-shutdown-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let worker_store = store.clone();
    let path = root.join("index.json");
    let output = path.clone();
    let (started, ready) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let outer = tokio::spawn(async move {
        let lease = worker_store.input_index_work().await;
        tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let _lease = lease;
            let mut file = std::fs::File::create(output).unwrap();
            started.send(()).unwrap();
            wait.recv().unwrap();
            file.write_all(b"{\"complete\":true}").unwrap();
            drop(file);
        })
        .await
        .unwrap();
    });
    ready.await.unwrap();
    outer.abort();
    assert!(outer.await.unwrap_err().is_cancelled());
    let drained = store.wait_input_index_workers();
    tokio::pin!(drained);
    assert!(futures_util::poll!(&mut drained).is_pending());
    release.send(()).unwrap();
    drained.await;
    assert_eq!(std::fs::read(path).unwrap(), b"{\"complete\":true}");
    store.pool.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
}
