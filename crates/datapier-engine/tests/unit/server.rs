use super::*;
use crate::protocol;

pub(in crate::server) async fn engine() -> (Arc<Engine>, PathBuf) {
    let root = std::env::temp_dir().join(format!("datapier-engine-test-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();
    let store = Store::open(&root).await.unwrap();
    let state = State {
        accepting: true,
        clients: HashSet::new(),
        contexts: HashMap::new(),
        windows: HashMap::new(),
        mcp: Default::default(),
        mcp_owners: HashSet::new(),
        client_cancels: HashMap::new(),
        event_senders: HashMap::new(),
        event_seq: 0,
        idle: Instant::now() - Duration::from_secs(10),
        databases: HashMap::new(),
        tasks: HashMap::new(),
        result_sessions: HashMap::new(),
        discovery: HashMap::new(),
        unpersisted: HashMap::new(),
        profile_locks: HashMap::new(),
        archery: Default::default(),
        editing: Default::default(),
        workspace_uploads: Default::default(),
    };
    (
        Arc::new(Engine {
            id: Uuid::new_v4(),
            store,
            state: Mutex::new(state),
        }),
        root,
    )
}

#[path = "server/lifecycle.rs"]
mod lifecycle;
#[path = "server/redis.rs"]
mod redis;
#[path = "server/results.rs"]
mod results;
#[path = "server/sessions.rs"]
mod sessions;

#[path = "archery.rs"]
mod archery_tests;

#[tokio::test]
async fn schema_conflict_and_cancelled_results_keep_their_task_terminal_state() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    for (business, terminal) in [("conflict", "failed"), ("cancelled", "cancelled")] {
        let task = engine
            .store
            .create_task(owner, "test", "schema.execute", json!({}), false)
            .await
            .unwrap();
        let result = Ok(json!({"state":business,"steps":[{"state":"not_sent"}]}));
        engine.finish_task(owner, task.id, &result, None).await;
        let recorded = engine.store.task(task.id).await.unwrap();
        assert_eq!(recorded.state, terminal);
        assert_eq!(recorded.summary["steps"][0]["state"], "not_sent");
        assert_eq!(recorded.error.unwrap().code, business);
    }
    engine.store.pool.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
async fn redis_partial_failure_preserves_item_details() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let task = engine
        .store
        .create_task(owner, "test", "change.submit", json!({}), false)
        .await
        .unwrap();
    engine.store.mark_sent(task.id, true).await.unwrap();
    let error = AppError::new("partial_failure", "部分操作失败").with_details(
        json!({"items":[{"status":"applied"},{"status":"error","value_may_have_changed":true}]}),
    );
    engine.finish_task(owner, task.id, &Err(error), None).await;
    let result = engine.store.task(task.id).await.unwrap();
    assert_eq!(result.state, "failed");
    assert_eq!(result.summary["items"][1]["value_may_have_changed"], true);
    assert_eq!(result.error.unwrap().code, "partial_failure");
    engine.store.pool.close().await;
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
