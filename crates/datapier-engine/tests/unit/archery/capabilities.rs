use super::*;

#[tokio::test]
async fn info_keeps_observed_capabilities_and_local_rejection_does_not_revoke_query() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let profile = profile(fixture.server.id, 7, "one");
    save(&engine, owner, &fixture, &auth, vec![profile.clone()])
        .await
        .unwrap();
    let opened = engine
        .dispatch(owner, "connection.open", json!({"profile_id":profile.id}))
        .await
        .unwrap();
    let session = &opened["session_id"];
    let query = engine
        .dispatch(
            owner,
            "execution.start",
            json!({"session_id":session,"database":"db_a","text":"SELECT 1"}),
        )
        .await
        .unwrap();
    assert_eq!(
        terminal_task(&engine, owner, &query["task_id"]).await["state"],
        "succeeded"
    );
    let rejected = engine
        .dispatch(
            owner,
            "execution.start",
            json!({"session_id":session,"database":"db_a","text":"UPDATE t SET a=1"}),
        )
        .await
        .unwrap();
    assert_eq!(
        terminal_task(&engine, owner, &rejected["task_id"]).await["state"],
        "failed"
    );
    let denied = engine
        .dispatch(
            owner,
            "archery.query_history",
            json!({"session_id":session,"operation_id":Uuid::new_v4()}),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.code, "permission_denied");
    let info = engine
        .dispatch(owner, "connection.info", json!({"session_id":session}))
        .await
        .unwrap();
    assert_eq!(info["capabilities"]["query"]["state"], "available");
    assert_eq!(info["capabilities"]["platform_history"]["state"], "denied");
    assert!(info["capabilities"]["platform_history"]["reason"].is_string());
    let retained = engine
        .dispatch(
            owner,
            "archery.auth.begin",
            json!({"server_id":fixture.server.id,"expected_revision":1,"operation_id":Uuid::new_v4()}),
        )
        .await
        .unwrap();
    assert_eq!(retained["auth_generation"], auth["auth_generation"]);
    engine.disconnect(owner).await;
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancellation_after_validated_response_preserves_remote_completion() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let task=engine.store.create_task(owner,"completed","execution.start",json!({"_task_context":{
        "connection":"fixture","provider":"mysql","source":"archery","database":"db_a","redis_db":null,"targets":[]
    }}),false).await.unwrap();
    let sink = ExecutionSink::new(engine.store.clone(), task.id);
    sink.mark_sent(false).await.unwrap();
    sink.mark_remote_completed().await.unwrap();
    engine
        .finish_task(owner, task.id, &Err(AppError::cancelled()), None)
        .await;
    let terminal = engine.task(task.id).await.unwrap();
    assert_eq!(terminal.state, "wait_stopped");
    assert_eq!(terminal.summary["remote_completion"], "completed");
    // 开始后续请求必须重新变为未知，不能沿用上一条 SQL 的完成状态。
    let next = engine
        .store
        .create_task(owner, "completed", "execution.start", json!({}), false)
        .await
        .unwrap();
    let sink = ExecutionSink::new(engine.store.clone(), next.id);
    sink.mark_sent(false).await.unwrap();
    sink.mark_remote_completed().await.unwrap();
    sink.mark_sent(false).await.unwrap();
    assert_eq!(
        engine.task(next.id).await.unwrap().summary["remote_completion"],
        "unknown"
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn generic_profile_save_rejects_archery_changes_without_mutation() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let original = profile(fixture.server.id, 7, "one");
    save(&engine, owner, &fixture, &auth, vec![original.clone()])
        .await
        .unwrap();
    let revision = engine
        .store
        .archery_server(fixture.server.id)
        .await
        .unwrap()
        .revision;
    let mut invalid_database = original.clone();
    if let ConnectionConfig::Archery(connection) = &mut invalid_database.connection {
        connection.databases = vec!["not_on_server".into()];
        connection.default_database = "not_on_server".into();
    }
    let mut stale = original.clone();
    stale.name = "stale rename".into();
    let mut current = original.clone();
    current.name = "current rename".into();
    save(&engine, owner, &fixture, &auth, vec![current.clone()])
        .await
        .unwrap();
    let current = engine.store.profile(current.id).await.unwrap();
    for changed in [invalid_database, stale] {
        assert!(
            engine
                .dispatch(owner, "profile.save", json!({"profile":changed}))
                .await
                .is_err()
        );
    }
    let profiles = engine.store.profiles().await.unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(
        serde_json::to_value(&profiles[0]).unwrap(),
        serde_json::to_value(&current).unwrap()
    );
    assert_eq!(
        engine
            .store
            .archery_server(fixture.server.id)
            .await
            .unwrap()
            .revision,
        revision
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
