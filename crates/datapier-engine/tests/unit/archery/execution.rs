use super::*;

#[tokio::test]
async fn query_uses_explicit_selected_database_and_cancelled_sent_wait_is_terminal() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let one = profile(fixture.server.id, 7, "one");
    save(&engine, owner, &fixture, &auth, vec![one.clone()])
        .await
        .unwrap();
    let opened = engine
        .dispatch(
            owner,
            "connection.open",
            json!({"profile_id":one.id,"database":"db_a"}),
        )
        .await
        .unwrap();
    let session = opened["session_id"].clone();
    let before = fixture.count();
    assert!(
        engine
            .dispatch(
                owner,
                "execution.start",
                json!({"session_id":session,"workspace_id":"scope","text":"SELECT 1"})
            )
            .await
            .is_err()
    );
    assert_eq!(engine.dispatch(owner, "execution.start", json!({"session_id":session,"workspace_id":"scope","database":"outside","text":"SELECT 1"})).await.unwrap_err().code, "target_not_selected");
    assert_eq!(fixture.count(), before);
    fixture.block_query.store(true, Ordering::Release);
    let started = engine.dispatch(owner, "execution.start", json!({"session_id":session,"workspace_id":"scope","database":"db_b","text":"SELECT 1"})).await.unwrap();
    fixture.wait().await;
    assert!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.starts_with("POST /prefix/query/ ")
                && request.contains("db_name=db_b"))
    );
    engine
        .dispatch(
            owner,
            "execution.cancel",
            json!({"task_id":started["task_id"]}),
        )
        .await
        .unwrap();
    let terminal = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let task = engine
                .dispatch(
                    owner,
                    "execution.get",
                    json!({"task_id":started["task_id"]}),
                )
                .await
                .unwrap();
            if !matches!(
                task["state"].as_str(),
                Some("queued" | "running" | "cancelling")
            ) {
                break task;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(terminal["state"], "wait_stopped");
    assert_eq!(terminal["summary"]["remote_completion"], "unknown");
    assert_eq!(terminal["context"]["database"], "db_b");
    assert_eq!(
        engine
            .dispatch(
                owner,
                "execution.cancel",
                json!({"task_id":started["task_id"]})
            )
            .await
            .unwrap()["state"],
        "wait_stopped"
    );
    fixture.release.add_permits(1);
    crate::server::tests::cleanup(engine, root).await;
}

#[tokio::test]
async fn completed_query_does_not_keep_engine_alive_until_timeout() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let mut settings = engine.store.settings().await.unwrap();
    settings.limits.query_timeout_seconds = 3600;
    engine.store.set_settings(&settings).await.unwrap();
    let profile = profile(fixture.server.id, 7, "one");
    save(&engine, owner, &fixture, &auth, vec![profile.clone()])
        .await
        .unwrap();
    let opened = engine
        .dispatch(
            owner,
            "connection.open",
            json!({"profile_id":profile.id,"database":"db_a"}),
        )
        .await
        .unwrap();
    fixture.block_query.store(true, Ordering::Release);
    let started = engine
        .dispatch(
            owner,
            "execution.start",
            json!({"session_id":opened["session_id"],"database":"db_a","text":"SELECT 1"}),
        )
        .await
        .unwrap();
    fixture.wait().await;
    fixture.release.add_permits(1);
    let terminal = terminal_task(&engine, owner, &started["task_id"]).await;
    assert_eq!(terminal["state"], "succeeded", "{terminal}");
    tokio::time::timeout(
        Duration::from_secs(3),
        crate::server::tests::cleanup(engine, root),
    )
    .await
    .expect("completed query must not keep the Engine alive until its timeout");
}
