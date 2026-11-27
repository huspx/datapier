use super::*;
use crate::server::tests::{cleanup, engine};
use std::time::Duration;

#[tokio::test]
async fn dropped_scope_task_is_finalized() {
    let (engine, root) = engine().await;
    let task = ScopeTask::start(
        engine.store.clone(),
        Uuid::new_v4(),
        "test".into(),
        json!({}),
    )
    .await
    .unwrap();
    let id = task.id;
    assert_eq!(engine.store.task(id).await.unwrap().state, "running");
    drop(task);
    let record = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let record = engine.store.task(id).await.unwrap();
            if record.state == "cancelled" {
                break record;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("scope inspection was not finalized");
    assert_eq!(record.error.unwrap().code, "cancelled");
    cleanup(engine, root).await;
}

#[tokio::test]
async fn scope_task_preserves_summary_and_provider_result() {
    let (engine, root) = engine().await;
    let task = ScopeTask::start(
        engine.store.clone(),
        Uuid::new_v4(),
        "test".into(),
        json!({}),
    )
    .await
    .unwrap();
    let id = task.id;
    engine.store.mark_sent(id, false).await.unwrap();
    engine.store.mark_remote_completed(id).await.unwrap();
    let value = json!({"ddl": "CREATE TABLE t (id INT)"});
    assert_eq!(task.finish(Ok(value.clone())).await.unwrap(), value);
    let record = engine.store.task(id).await.unwrap();
    assert_eq!(record.state, "succeeded");
    assert!(record.sent);
    assert_eq!(record.summary["remote_completion"], "completed");
    cleanup(engine, root).await;
}
