use super::*;
use crate::ExecutionSink;
use tokio_util::sync::CancellationToken;

async fn fixture() -> Store {
    let root = std::env::temp_dir().join(format!("datapier-store-{}", Uuid::new_v4()));
    Store::open(&root).await.expect("打开隔离测试目录")
}
async fn cleanup(store: Store) {
    let root = (*store.root).clone();
    store.pool.close().await;
    store.cache.close().await;
    tokio::fs::remove_dir_all(root).await.expect("清理测试目录");
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
async fn crash_recovery_preserves_unknown_writes_and_finished_tasks() {
    let store = fixture().await;
    let owner = Uuid::new_v4();
    let queued = store
        .create_task(owner, "w", "execution.start", json!({}), false)
        .await
        .unwrap();
    let read = store
        .create_task(owner, "w", "execution.start", json!({}), false)
        .await
        .unwrap();
    let write = store
        .create_task(owner, "w", "change.submit", json!({}), false)
        .await
        .unwrap();
    let finished = store
        .create_task(owner, "w", "change.submit", json!({}), false)
        .await
        .unwrap();
    store.mark_sent(read.id, false).await.unwrap();
    store.mark_sent(write.id, true).await.unwrap();
    store.mark_sent(finished.id, true).await.unwrap();
    store
        .update_task(finished.id, "succeeded", json!({"affected_rows":1}), None)
        .await
        .unwrap();
    store.recover_tasks().await.unwrap();
    assert_eq!(store.task(queued.id).await.unwrap().state, "cancelled");
    assert_eq!(store.task(read.id).await.unwrap().state, "cancelled");
    let unknown = store.task(write.id).await.unwrap();
    assert_eq!(unknown.state, "outcome_unknown");
    assert_eq!(unknown.error.unwrap().code, "outcome_unknown");
    let done = store.task(finished.id).await.unwrap();
    assert_eq!(done.state, "succeeded");
    assert_eq!(done.summary["affected_rows"], 1);
    cleanup(store).await;
}

#[tokio::test]
async fn wide_row_is_paged_losslessly_and_external_files_are_released() {
    let store = fixture().await;
    let task = store
        .create_task(Uuid::new_v4(), "w", "execution.start", json!({}), false)
        .await
        .unwrap();
    let columns = (0..20)
        .map(|i| Column::new(format!("c{i}"), "TEXT"))
        .collect();
    let id = store.start_result(task.id, columns, "rows").await.unwrap();
    let expected = Value::Text("中😀".repeat(8000));
    store
        .append_rows(id, vec![vec![expected.clone(); 20], vec![Value::Null; 20]])
        .await
        .unwrap();
    store.finish_result(id, json!({})).await.unwrap();
    let page = store.result_page(id, 0, 200, 16384).await.unwrap();
    assert_eq!(page.rows.len(), 2);
    assert!(serde_json::to_vec(&page).unwrap().len() <= 16384);
    for value in &page.rows[0] {
        match value {
            Value::Large { id, .. } => assert_eq!(store.read_value(*id).await.unwrap(), expected),
            _ => panic!("宽行字段必须被外置，不能截断或丢弃"),
        }
    }
    assert_eq!(page.rows[1], vec![Value::Null; 20]);
    let again = store.result_page(id, 0, 200, 16384).await.unwrap();
    assert_eq!(page.rows, again.rows);
    store.release_result(id).await.unwrap();
    assert_eq!(store.result_meta(id).await.unwrap_err().code, "not_found");
    assert!(
        tokio::fs::read_dir(store.root.join("values"))
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
    cleanup(store).await;
}

#[tokio::test]
async fn cancellation_before_dispatch_never_records_a_sent_write() {
    let store = fixture().await;
    let task = store
        .create_task(Uuid::new_v4(), "w", "change.submit", json!({}), false)
        .await
        .unwrap();
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    let cancel = CancellationToken::new();
    let sink = ExecutionSink::new(store.clone(), task.id)
        .with_dispatch_control(gate.clone(), cancel.clone());
    let guard = gate.lock().await;
    let operation = tokio::spawn(async move { sink.mark_sent(true).await });
    cancel.cancel();
    drop(guard);
    assert_eq!(operation.await.unwrap().unwrap_err().code, "cancelled");
    let task = store.task(task.id).await.unwrap();
    assert!(!task.sent);
    assert!(!task.may_write);
    cleanup(store).await;
}

#[tokio::test]
async fn sub_kib_fields_are_externalized_to_fit_the_requested_page() {
    let store = fixture().await;
    let columns = (0..20)
        .map(|i| Column::new(format!("c{i}"), "TEXT"))
        .collect();
    let id = store
        .start_result(Uuid::new_v4(), columns, "rows")
        .await
        .unwrap();
    let expected = Value::Text("x".repeat(800));
    store
        .append_rows(id, vec![vec![expected.clone(); 20]])
        .await
        .unwrap();
    let page = store.result_page(id, 0, 200, 16384).await.unwrap();
    assert!(serde_json::to_vec(&page).unwrap().len() <= 16384);
    assert_eq!(page.rows.len(), 1);
    for value in &page.rows[0] {
        let actual = match value {
            Value::Large { id, .. } => store.read_value(*id).await.unwrap(),
            value => value.clone(),
        };
        assert_eq!(actual, expected);
    }
    store.release_result(id).await.unwrap();
    assert!(
        tokio::fs::read_dir(store.root.join("values"))
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
    cleanup(store).await;
}

#[tokio::test]
async fn concurrent_wide_pages_finish_and_release_every_external_file() {
    let store = fixture().await;
    let mut ids = Vec::new();
    for _ in 0..4 {
        let columns = (0..20)
            .map(|i| Column::new(format!("c{i}"), "TEXT"))
            .collect();
        let id = store
            .start_result(Uuid::new_v4(), columns, "rows")
            .await
            .unwrap();
        store
            .append_rows(id, vec![vec![Value::Text("x".repeat(4000)); 20]])
            .await
            .unwrap();
        ids.push(id);
    }
    let mut requests = tokio::task::JoinSet::new();
    let barrier = Arc::new(tokio::sync::Barrier::new(9));
    for id in ids.iter().copied().cycle().take(8) {
        let store = store.clone();
        let barrier = barrier.clone();
        requests.spawn(async move {
            barrier.wait().await;
            let page = store.result_page(id, 0, 200, 16384).await?;
            assert_eq!(page.rows.len(), 1);
            assert!(serde_json::to_vec(&page).unwrap().len() <= 16384);
            Ok::<_, AppError>(())
        });
    }
    barrier.wait().await;
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(result) = requests.join_next().await {
            result.unwrap().unwrap();
        }
    })
    .await
    .expect("并发分页不能耗尽缓存连接池");
    for id in ids {
        store.release_result(id).await.unwrap();
    }
    assert!(
        tokio::fs::read_dir(store.root.join("values"))
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
    cleanup(store).await;
}

#[tokio::test]
async fn full_cache_reports_failure_without_publishing_partial_rows() {
    let mut store = fixture().await;
    let id = store
        .start_result(
            Uuid::new_v4(),
            vec![Column::new("payload", "TEXT"); 20],
            "rows",
        )
        .await
        .unwrap();
    store.cache.close().await;
    let options = SqliteConnectOptions::new()
        .filename(store.root.join("cache.sqlite3"))
        .journal_mode(SqliteJournalMode::Wal);
    store.cache = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await
        .unwrap();
    sqlx::query("PRAGMA max_page_count=1")
        .execute(&store.cache)
        .await
        .unwrap();
    let row = vec![Value::Text("x".repeat(5000)); 20];
    let error = store.append_rows(id, vec![row.clone()]).await.unwrap_err();
    assert_eq!(error.database_code.as_deref(), Some("13"));
    let incomplete = store.result_page(id, 0, 200, 1024 * 1024).await.unwrap();
    assert!(incomplete.rows.is_empty());
    assert_eq!(incomplete.result.row_count, 0);
    assert!(!incomplete.result.complete);
    sqlx::query("PRAGMA max_page_count=100000")
        .execute(&store.cache)
        .await
        .unwrap();
    store.append_rows(id, vec![row.clone()]).await.unwrap();
    store.finish_result(id, json!({})).await.unwrap();
    let recovered = store.result_page(id, 0, 200, 1024 * 1024).await.unwrap();
    assert_eq!(recovered.rows, vec![row]);
    assert_eq!(recovered.result.row_count, 1);
    assert!(recovered.result.complete);
    cleanup(store).await;
}
