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
async fn task_details_expose_only_text_and_compact_result_status() {
    let store = fixture().await;
    let task = store
        .create_task(
            Uuid::new_v4(),
            "main",
            "execution.start",
            json!({"text":"SELECT 1", "secret":"不应返回"}),
            false,
        )
        .await
        .unwrap();
    let first = store
        .start_result(
            task.id,
            vec![Column::new("private_column", "text")],
            "query",
        )
        .await
        .unwrap();
    let second = store.start_result(task.id, vec![], "query").await.unwrap();
    store
        .append_rows(first, vec![vec![Value::Text("private_row".into())]])
        .await
        .unwrap();
    store.finish_result(first, json!({})).await.unwrap();
    assert!(store.task(task.id).await.unwrap().text.is_none());
    let detail = store.task_details(task.id).await.unwrap();
    assert_eq!(detail.text.as_deref(), Some("SELECT 1"));
    assert!(detail.created_at > 0.0);
    assert_eq!(detail.results, vec![first, second]);
    assert_eq!(
        serde_json::to_value(&detail.result_summaries).unwrap(),
        json!([
            {"id":first,"row_count":1,"complete":true,"kind":"query","column_count":1,"redis_type":null},
            {"id":second,"row_count":0,"complete":false,"kind":"query","column_count":0,"redis_type":null}
        ])
    );
    let serialized = serde_json::to_string(&detail).unwrap();
    assert!(!serialized.contains("不应返回"));
    assert!(!serialized.contains("private_column"));
    assert!(!serialized.contains("private_row"));
    cleanup(store).await;
}

#[tokio::test]
async fn task_context_unknown_columns_and_bounded_text_survive_recovery() {
    let store = fixture().await;
    let task = store.create_task(Uuid::new_v4(), "w", "resource.browse", json!({
        "_task_context":{"connection":"frozen","provider":"mysql","database":"db","redis_db":null,"targets":[]},
        "target":{"database":"db","name":"t","kind":"table"}
    }), false).await.unwrap();
    let unknown = store
        .start_result(task.id, vec![], "columns_unknown")
        .await
        .unwrap();
    store
        .finish_result(unknown, json!({"columns_known":false}))
        .await
        .unwrap();
    store
        .append_execution_text(task.id, &"中".repeat(70000), false)
        .await
        .unwrap();
    store
        .update_task(
            task.id,
            "running",
            json!({"_task":{"started_at":123.5}}),
            None,
        )
        .await
        .unwrap();
    store.mark_sent(task.id, true).await.unwrap();
    store.recover_tasks().await.unwrap();
    let record = store.task_details(task.id).await.unwrap();
    assert_eq!(record.state, "outcome_unknown");
    assert_eq!(record.started_at, Some(123.5));
    assert_eq!(record.elapsed_ms, None);
    assert_eq!(record.context.unwrap().targets[0].name, "t");
    assert_eq!(record.result_summaries[0].kind, "columns_unknown");
    assert_eq!(record.result_summaries[0].column_count, None);
    assert!(record.execution_text_truncated);
    assert_eq!(record.text.unwrap().chars().count(), 65536);
    store.release_result(unknown).await.unwrap();
    assert!(
        store
            .task(task.id)
            .await
            .unwrap()
            .result_summaries
            .is_empty()
    );
    let historical = store
        .create_task(Uuid::new_v4(), "w", "old", json!({}), false)
        .await
        .unwrap();
    assert!(
        historical.started_at.is_none()
            && historical.elapsed_ms.is_none()
            && historical.context.is_none()
    );
    cleanup(store).await;
}

#[tokio::test]
async fn generated_execution_text_never_replaces_request_text() {
    let store = fixture().await;
    let task = store
        .create_task(
            Uuid::new_v4(),
            "w",
            "execution.start",
            json!({"text":"SELECT ?"}),
            false,
        )
        .await
        .unwrap();
    store
        .append_execution_text(task.id, "constructed SQL", false)
        .await
        .unwrap();
    assert_eq!(
        store.task_details(task.id).await.unwrap().text.as_deref(),
        Some("SELECT ?")
    );
    cleanup(store).await;
}

#[tokio::test]
async fn result_summary_uses_cached_redis_type() {
    let store = fixture().await;
    let task = store
        .create_task(Uuid::new_v4(), "w", "resource.browse", json!({}), false)
        .await
        .unwrap();
    let id = store
        .start_result(
            task.id,
            vec![Column::new("field", "bytes"), Column::new("value", "bytes")],
            "redis",
        )
        .await
        .unwrap();
    store
        .finish_result(id, json!({"redis_type":"hash"}))
        .await
        .unwrap();
    let record = store.task(task.id).await.unwrap();
    assert_eq!(
        record.result_summaries[0].redis_type.as_deref(),
        Some("hash")
    );
    assert_eq!(record.result_summaries[0].column_count, Some(2));
    cleanup(store).await;
}

#[tokio::test]
async fn large_task_texts_are_loaded_only_for_details() {
    let store = fixture().await;
    let text = format!("PING{}", " ".repeat(5 * 1024 * 1024));
    let mut ids = Vec::new();
    for _ in 0..2 {
        let task = store
            .create_task(
                Uuid::new_v4(),
                "large",
                "execution.start",
                json!({"text":text}),
                false,
            )
            .await
            .unwrap();
        assert!(task.text.is_none());
        ids.push(task.id);
    }
    let listed = store.tasks("large").await.unwrap();
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|task| task.text.is_none()));
    assert!(serde_json::to_vec(&listed).unwrap().len() < 4096);
    for id in ids {
        assert_eq!(
            store.task_details(id).await.unwrap().text.as_deref(),
            Some(text.as_str())
        );
    }
    cleanup(store).await;
}

#[tokio::test]
async fn result_gates_are_removed_after_invalid_requests_and_cancelled_waiters() {
    let store = fixture().await;
    for _ in 0..10 {
        let id = Uuid::new_v4();
        assert_eq!(
            store.result_page(id, 0, 1, 16384).await.unwrap_err().code,
            "not_found"
        );
    }
    assert!(store.result_gates.lock().unwrap().is_empty());

    let id = Uuid::new_v4();
    let held = store.result_guard(id).await;
    let mut cancelled = Box::pin(store.result_guard(id));
    assert!(futures_util::poll!(cancelled.as_mut()).is_pending());
    drop(cancelled);
    assert_eq!(
        store.result_gates.lock().unwrap().get(&id).unwrap().users,
        1
    );

    let mut waiting = Box::pin(store.result_guard(id));
    assert!(futures_util::poll!(waiting.as_mut()).is_pending());
    drop(held);
    assert_eq!(
        store.result_gates.lock().unwrap().get(&id).unwrap().users,
        1
    );
    let acquired = waiting.await;
    drop(acquired);
    assert!(store.result_gates.lock().unwrap().is_empty());
    cleanup(store).await;
}

#[tokio::test]
async fn baseline_reopen_preserves_profiles_settings_and_workspace() {
    let store = fixture().await;
    let root = (*store.root).clone();
    let id = Uuid::new_v4();
    let profile =
        json!({"id":id,"name":"旧连接","provider":"mysql","host":"localhost","port":3306});
    sqlx::query("INSERT INTO profiles(id,name,payload) VALUES(?,?,?)")
        .bind(id.to_string())
        .bind("旧连接")
        .bind(profile.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO workspaces(id,revision,payload) VALUES('main',3,'{\"tab\":1}')")
        .execute(&store.pool)
        .await
        .unwrap();
    store.pool.close().await;
    store.cache.close().await;
    drop(store);
    let store = Store::open(&root).await.unwrap();
    assert_eq!(store.profiles().await.unwrap()[0].visible_databases, None);
    assert_eq!(
        store.workspace_load("main").await.unwrap(),
        json!({"revision":3,"state":{"tab":1}})
    );
    assert_eq!(store.settings().await.unwrap(), AppSettings::default());
    let mut profile = store.profiles().await.unwrap().remove(0);
    profile.visible_databases = Some(vec![]);
    store.save_profile(&profile, None, false).await.unwrap();
    store
        .set_settings(&AppSettings {
            language: LanguagePreference::En,
        })
        .await
        .unwrap();
    store.pool.close().await;
    store.cache.close().await;
    drop(store);

    let store = Store::open(&root).await.unwrap();
    assert_eq!(
        store.profiles().await.unwrap()[0].visible_databases,
        Some(vec![])
    );
    assert_eq!(
        store.settings().await.unwrap().language,
        LanguagePreference::En
    );
    cleanup(store).await;
}

#[tokio::test]
async fn failed_release_keeps_cleanup_retryable_and_hides_result() {
    let store = fixture().await;
    let id = store
        .start_result(
            Uuid::new_v4(),
            vec![Column::new("a", "TEXT"), Column::new("b", "TEXT")],
            "rows",
        )
        .await
        .unwrap();
    store
        .append_rows(
            id,
            vec![vec![
                Value::Text("a".repeat(70000)),
                Value::Text("b".repeat(70000)),
            ]],
        )
        .await
        .unwrap();
    let page = store.result_page(id, 0, 1, 16384).await.unwrap();
    let mut values: Vec<Uuid> = page.rows[0]
        .iter()
        .map(|v| match v {
            Value::Large { id, .. } => *id,
            _ => panic!("字段应当外置"),
        })
        .collect();
    values.sort();
    let blocked = store.root.join("values").join(values[1].to_string());
    tokio::fs::remove_file(&blocked).await.unwrap();
    tokio::fs::create_dir(&blocked).await.unwrap();
    assert!(store.release_result(id).await.is_err());
    assert_eq!(store.result_meta(id).await.unwrap_err().code, "not_found");
    assert_eq!(
        store.result_page(id, 0, 1, 16384).await.unwrap_err().code,
        "not_found"
    );
    assert_eq!(
        store.read_value(values[1]).await.unwrap_err().code,
        "not_found"
    );
    assert!(
        !store
            .root
            .join("values")
            .join(values[0].to_string())
            .exists()
    );
    tokio::fs::remove_dir(&blocked).await.unwrap();
    tokio::fs::write(&blocked, b"remaining").await.unwrap();
    store.release_result(id).await.unwrap();
    assert!(!blocked.exists());
    cleanup(store).await;
}

#[tokio::test]
async fn external_file_creation_failure_can_be_released_after_directory_is_restored() {
    let store = fixture().await;
    let id = store
        .start_result(Uuid::new_v4(), vec![Column::new("v", "TEXT")], "rows")
        .await
        .unwrap();
    let values = store.root.join("values");
    tokio::fs::remove_dir(&values).await.unwrap();
    tokio::fs::write(&values, b"blocks directory access")
        .await
        .unwrap();
    let error = store
        .append_rows(id, vec![vec![Value::Text("x".repeat(70000))]])
        .await
        .unwrap_err();
    assert_eq!(error.code, "io");
    assert_eq!(store.result_meta(id).await.unwrap().row_count, 0);
    assert!(store.release_result(id).await.is_err());
    assert_eq!(store.result_meta(id).await.unwrap_err().code, "not_found");
    tokio::fs::remove_file(&values).await.unwrap();
    tokio::fs::create_dir(&values).await.unwrap();
    store.release_result(id).await.unwrap();
    assert!(
        tokio::fs::read_dir(&values)
            .await
            .unwrap()
            .next_entry()
            .await
            .unwrap()
            .is_none()
    );
    cleanup(store).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn failed_external_write_keeps_cleanup_owner() {
    use std::os::unix::fs::symlink;
    let store = fixture().await;
    let id = store
        .start_result(Uuid::new_v4(), vec![], "rows")
        .await
        .unwrap();
    let file_id = Uuid::new_v4();
    let path = store.root.join("values").join(file_id.to_string());
    symlink("/dev/full", &path).unwrap();
    let value = Value::Text("failure".into());
    assert!(
        store
            .externalize_file(id, file_id, &value, serde_json::to_vec(&value).unwrap(), 0)
            .await
            .is_err()
    );
    assert!(path.exists());
    store.release_result(id).await.unwrap();
    assert!(!path.exists());
    cleanup(store).await;
}

#[tokio::test]
async fn baseline_cache_retains_existing_result_and_cleans_its_files() {
    let store = fixture().await;
    let id = store
        .start_result(Uuid::new_v4(), vec![Column::new("v", "TEXT")], "rows")
        .await
        .unwrap();
    store
        .append_rows(id, vec![vec![Value::Text("x".repeat(70000))]])
        .await
        .unwrap();
    let root = (*store.root).clone();
    store.pool.close().await;
    store.cache.close().await;
    let reopened = Store::open(&root).await.unwrap();
    let page = reopened.result_page(id, 0, 1, 16384).await.unwrap();
    let Value::Large { id: value_id, .. } = &page.rows[0][0] else {
        panic!("字段应当外置")
    };
    assert_eq!(
        reopened.read_value(*value_id).await.unwrap(),
        Value::Text("x".repeat(70000))
    );
    reopened.release_result(id).await.unwrap();
    assert!(!root.join("values").join(value_id.to_string()).exists());
    cleanup(reopened).await;
}

#[tokio::test]
async fn incomplete_and_newer_baselines_are_rejected() {
    for cache in [false, true] {
        let store = fixture().await;
        let pool = if cache { &store.cache } else { &store.pool };
        let statement = if cache {
            "DROP TABLE value_files"
        } else {
            "DROP TABLE app_settings"
        };
        sqlx::query(statement).execute(pool).await.unwrap();
        let root = (*store.root).clone();
        store.pool.close().await;
        store.cache.close().await;
        drop(store);
        assert_eq!(
            Store::open(&root).await.err().unwrap().code,
            "storage_version"
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
    let store = fixture().await;
    sqlx::query("PRAGMA user_version=2")
        .execute(&store.pool)
        .await
        .unwrap();
    let root = (*store.root).clone();
    store.pool.close().await;
    store.cache.close().await;
    drop(store);
    assert_eq!(
        Store::open(&root).await.err().unwrap().code,
        "storage_version"
    );
    tokio::fs::remove_dir_all(root).await.unwrap();
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
async fn workspace_revision_cannot_overflow_sqlite_integer() {
    let store = fixture().await;
    sqlx::query("INSERT INTO workspaces(id,revision,payload) VALUES('limit',?,?)")
        .bind(i64::MAX)
        .bind("{}")
        .execute(&store.pool)
        .await
        .unwrap();
    let error = store
        .workspace_save("limit", i64::MAX as u64, json!({"lost":true}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "invalid_input");
    assert_eq!(
        store.workspace_load("limit").await.unwrap(),
        json!({"revision":i64::MAX,"state":{}})
    );
    cleanup(store).await;
}

#[tokio::test]
async fn result_page_with_large_offset_does_not_wrap_to_first_row() {
    let store = fixture().await;
    let id = store
        .start_result(Uuid::new_v4(), vec![Column::new("v", "TEXT")], "rows")
        .await
        .unwrap();
    store
        .append_rows(id, vec![vec![Value::Text("first".into())]])
        .await
        .unwrap();
    for start in [1, i64::MAX as u64 + 1, u64::MAX] {
        let page = store.result_page(id, start, 10, 16384).await.unwrap();
        assert!(page.rows.is_empty());
        assert_eq!(page.start, start);
        assert_eq!(page.next, None);
    }
    store.release_result(id).await.unwrap();
    cleanup(store).await;
}

#[tokio::test]
async fn many_short_rows_respect_serialized_page_budget() {
    let store = fixture().await;
    let id = store
        .start_result(Uuid::new_v4(), vec![Column::new("v", "TEXT")], "rows")
        .await
        .unwrap();
    store
        .append_rows(id, vec![vec![Value::Null]; 1000])
        .await
        .unwrap();
    let page = store.result_page(id, 0, 1000, 16384).await.unwrap();
    assert!(page.next.is_some());
    assert!(serde_json::to_vec(&page).unwrap().len() <= 16384);
    store.release_result(id).await.unwrap();
    cleanup(store).await;
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
