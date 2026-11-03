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
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
