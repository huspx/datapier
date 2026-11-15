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
    crate::server::tests::cleanup(engine, root).await;
}

#[tokio::test]
async fn mcp_catalog_preserves_unknown_incomplete_and_local_continuation() {
    let (engine, root, controller, fixture, auth) = setup().await;
    let profile = profile(fixture.server.id, 7, "one");
    save(&engine, controller, &fixture, &auth, vec![profile.clone()])
        .await
        .unwrap();
    let owner = Uuid::new_v4();
    let (stream, server) = tokio::io::duplex(65536);
    let handler = tokio::spawn(client(engine.clone(), owner, Box::new(server)));
    let mcp = crate::Client::handshake(
        Box::new(stream),
        crate::ClientOptions {
            kind: model::ClientKind::Mcp,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let opened = mcp
        .request("mcp.open_session", json!({"connection_id":profile.id}))
        .await
        .unwrap();
    let session = &opened["session_id"];
    let unfiltered = mcp
        .request(
            "mcp.list_resources",
            json!({"session_id":session,"database":"db_a"}),
        )
        .await
        .unwrap();
    assert_eq!(unfiltered["complete"], false);
    assert_eq!(unfiltered["has_more"], false);
    assert!(unfiltered["next_cursor"].is_null());
    let resources = unfiltered["resources"].as_array().unwrap();
    assert_eq!(resources.len(), 2);
    for name in ["db_a", "db_b"] {
        assert!(resources.iter().any(|item| item["name"] == name
            && item["kind"] == "unknown"
            && item["database"] == "db_a"
            && item["data"]["relation_kind"] == "unknown"));
    }

    fixture.many_resources.store(true, Ordering::Release);
    let mut settings = engine.store.settings().await.unwrap();
    settings.limits.response_bytes = 1024;
    settings.limits.value_preview_bytes = settings.limits.value_preview_bytes.min(1024);
    engine.store.set_settings(&settings).await.unwrap();
    let mut cursor = Value::Null;
    let mut names = std::collections::BTreeSet::new();
    let mut pages = 0;
    loop {
        let page = mcp
            .request(
                "mcp.list_resources",
                json!({"session_id":session,"database":"db_a","kind":"table","cursor":cursor}),
            )
            .await
            .unwrap();
        pages += 1;
        assert!(pages <= 24);
        assert_eq!(page["complete"], false);
        let resources = page["resources"].as_array().unwrap();
        assert!(!resources.is_empty());
        for item in resources {
            assert_eq!(item["kind"], "unknown");
            assert_eq!(item["database"], "db_a");
            assert_eq!(item["data"]["relation_kind"], "unknown");
            assert!(names.insert(item["name"].as_str().unwrap().to_owned()));
        }
        cursor = page["next_cursor"].clone();
        if page["has_more"] == false {
            assert!(cursor.is_null());
            break;
        }
        assert_eq!(page["has_more"], true);
        assert!(cursor.is_string());
    }
    assert!(pages > 1);
    let expected: std::collections::BTreeSet<_> =
        (0..24).map(|i| format!("table_{i:02}")).collect();
    assert_eq!(names, expected);
    mcp.close().await.unwrap();
    handler.await.unwrap();
    engine.disconnect(controller).await;
    crate::server::tests::cleanup(engine, root).await;
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
    drop(sink);
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
    drop(sink);
    crate::server::tests::cleanup(engine, root).await;
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
    crate::server::tests::cleanup(engine, root).await;
}
