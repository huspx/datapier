use super::*;

#[tokio::test]
async fn schema_one_migrates_without_creating_result_cache() {
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
    let profile = json!({"id":id,"name":"旧连接","provider":"mysql","access_mode":"read_only","host":"localhost","port":3306,"username":"legacy","database":"sales","redis_db":3,"tls":{"enabled":true}});
    sqlx::query("INSERT INTO profiles(id,name,payload,credential_saved) VALUES(?,?,?,1)")
        .bind(id.to_string())
        .bind("旧连接")
        .bind(profile.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let store = Store::open(&root).await.unwrap();
    let profile = store.profile(id).await.unwrap();
    assert_eq!(profile.direct().unwrap().database.as_deref(), Some("sales"));
    assert_eq!(profile.revision, 1);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT credential_saved FROM profiles WHERE id=?")
            .bind(id.to_string())
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("PRAGMA user_version")
            .fetch_one(&store.pool)
            .await
            .unwrap(),
        3
    );
    assert!(!root.join("cache.sqlite3").exists());
    assert!(!root.join("values").exists());
    cleanup(store).await;
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
