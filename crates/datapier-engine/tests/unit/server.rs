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
        projects: Default::default(),
        transfers: Default::default(),
        operations: Default::default(),
    };
    let (destroyed_signal, destroyed) = tokio::sync::oneshot::channel();
    (
        Arc::new(Engine {
            id: Uuid::new_v4(),
            store,
            state: Mutex::new(state),
            send_gate: Arc::new(AsyncMutex::new(())),
            destroyed: Mutex::new(Some(destroyed)),
            _destroyed_signal: Some(destroyed_signal),
        }),
        root,
    )
}

pub(in crate::server) async fn shutdown(engine: Arc<Engine>) {
    let destroyed = engine.destroyed.lock().unwrap().take().unwrap();
    engine.store.wait_input_index_workers().await;
    engine.store.pool.close().await;
    drop(engine);
    tokio::time::timeout(Duration::from_secs(60), destroyed)
        .await
        .expect("Engine owners did not finish during cleanup")
        .expect_err("Engine destruction signal must be dropped");
}

pub(in crate::server) async fn cleanup(engine: Arc<Engine>, root: PathBuf) {
    shutdown(engine).await;
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[path = "server/lifecycle.rs"]
mod lifecycle;
#[path = "server/project_queries.rs"]
mod project_queries;
#[path = "server/redis.rs"]
mod redis;
#[path = "server/results.rs"]
mod results;
#[path = "server/sessions.rs"]
mod sessions;
#[path = "server/transfer_file_retry.rs"]
mod transfer_file_retry;

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
    crate::server::tests::cleanup(engine, root).await;
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
    crate::server::tests::cleanup(engine, root).await;
}

#[tokio::test]
async fn input_index_task_guards_claim_and_file_identity() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let claimant = Uuid::new_v4();
    assert!(engine.register(owner));
    assert!(engine.register(claimant));
    let task = engine
        .store
        .create_task(owner, "test", "transfer.prepare", json!({}), false)
        .await
        .unwrap();
    let path = root.join("index.json");
    std::fs::write(&path, b"initial").unwrap();
    let baseline = datapier_core::execution::data_files::output_snapshot(&path, true).unwrap();
    let identity = json!({"task_id":task.id,"path":baseline.canonical_path,"canonical_path":baseline.canonical_path,"file_id":baseline.file_id,"size":baseline.size,"sha256":baseline.sha256,"export_name":"sql-input-index.json","media_type":"application/json"});
    let id = engine
        .store
        .transfer_register_artifact(owner, None, "input_index", &identity)
        .await
        .unwrap();
    engine.state.lock().unwrap().tasks.insert(
        task.id,
        Running {
            owner,
            session_id: Uuid::nil(),
            cancel: CancellationToken::new(),
            transition: Arc::new(AsyncMutex::new(())),
            abort: None,
            started: None,
        },
    );
    let args = json!({"id":id,"revision":1});
    assert_eq!(
        engine
            .project_rpc(owner, "transfer.artifact.cleanup", args.clone())
            .await
            .unwrap_err()
            .code,
        "busy"
    );
    engine.disconnect(owner).await;
    assert_eq!(
        engine
            .project_rpc(claimant, "transfer.artifact.claim", args.clone())
            .await
            .unwrap_err()
            .code,
        "busy"
    );
    assert_eq!(
        engine
            .project_rpc(claimant, "transfer.artifact.get", args.clone())
            .await
            .unwrap_err()
            .code,
        "busy"
    );
    engine.state.lock().unwrap().tasks.remove(&task.id);
    std::fs::write(&path, b"grown index").unwrap();
    let observed = engine
        .project_rpc(claimant, "transfer.artifact.get", args.clone())
        .await
        .unwrap();
    assert_eq!(observed["requires_partial_confirmation"], true);
    assert_eq!(
        engine
            .project_rpc(claimant, "transfer.artifact.claim", args.clone())
            .await
            .unwrap_err()
            .code,
        "confirmation_required"
    );
    let claimed = engine.project_rpc(claimant, "transfer.artifact.claim", json!({"id":id,"revision":1,"confirm_partial_sha256":observed["observed_identity"]["sha256"]})).await.unwrap();
    assert_eq!(claimed["artifact"]["state"], "cleanup_pending");
    assert_eq!(
        claimed["artifact"]["identity"]["task_id"],
        task.id.to_string()
    );
    let revision = claimed["artifact"]["revision"].as_u64().unwrap();
    let export = root.join("export.json");
    engine
        .project_rpc(
            claimant,
            "transfer.artifact.export",
            json!({"id":id,"revision":revision,"path":export}),
        )
        .await
        .unwrap();
    assert_eq!(std::fs::read(export).unwrap(), b"grown index");
    let original = root.join("original.json");
    std::fs::rename(&path, &original).unwrap();
    std::fs::write(&path, b"grown index").unwrap();
    assert_eq!(
        engine
            .project_rpc(
                claimant,
                "transfer.artifact.cleanup",
                json!({"id":id,"revision":revision})
            )
            .await
            .unwrap_err()
            .code,
        "recovery_conflict"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"grown index");
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(original, &path).unwrap();
    let revision = engine.store.transfer_artifact(id).await.unwrap().revision;
    engine
        .project_rpc(
            claimant,
            "transfer.artifact.cleanup",
            json!({"id":id,"revision":revision}),
        )
        .await
        .unwrap();
    assert!(!path.exists());
    let ready_path = root.join("ready-index.json");
    std::fs::write(&ready_path, b"ready index").unwrap();
    let snapshot =
        datapier_core::execution::data_files::output_snapshot(&ready_path, true).unwrap();
    let ready_identity = json!({"task_id":task.id,"path":snapshot.canonical_path,"canonical_path":snapshot.canonical_path,"file_id":snapshot.file_id,"size":snapshot.size,"sha256":snapshot.sha256,"export_name":"sql-input-index.json","media_type":"application/json"});
    let ready = engine
        .store
        .transfer_register_artifact(claimant, None, "input_index", &ready_identity)
        .await
        .unwrap();
    let ready_revision = engine
        .store
        .transfer_update_artifact(ready, claimant, Some(1), "ready", None)
        .await
        .unwrap();
    engine
        .project_rpc(
            claimant,
            "transfer.artifact.cleanup",
            json!({"id":ready,"revision":ready_revision}),
        )
        .await
        .unwrap();
    assert!(!ready_path.exists());
    crate::server::tests::cleanup(engine, root).await;
}
