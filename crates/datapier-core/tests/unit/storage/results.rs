use super::*;

#[tokio::test]
async fn large_value_previews_show_content_and_preserve_full_values() {
    let store = fixture().await;
    let id = store
        .start_result(Uuid::new_v4(), vec![Column::new("v", "value")], "query")
        .await
        .unwrap();
    let values = [
        Value::Text(format!("首行\n\"引号\"😀{}", "中😀".repeat(12000))),
        Value::Text(format!("{}😀{}", "a".repeat(4095), "后".repeat(22000))),
        Value::Bytes("YWJj".repeat(18000)),
        Value::Json(json!({"message": format!("中文😀\n\"{}", "字".repeat(23000))})),
    ];
    store
        .append_rows(
            id,
            values.iter().cloned().map(|value| vec![value]).collect(),
        )
        .await
        .unwrap();
    for (index, expected) in values.iter().enumerate() {
        let page = store.result_page(id, index as u64, 1, 16384).await.unwrap();
        let Value::Large {
            id: value_id,
            preview,
            ..
        } = &page.rows[0][0]
        else {
            panic!("大值应外置");
        };
        let display = expected.text();
        assert!(!preview.is_empty());
        assert!(display.starts_with(preview));
        assert!(preview.len() <= 4096);
        assert!(display[preview.len()..].chars().next().unwrap().len_utf8() + preview.len() > 4096);
        assert_eq!(store.read_value(*value_id).await.unwrap(), *expected);
    }
    // 页预算不足时清空预览，外置值仍可完整读取。
    let page = store.result_page(id, 0, 1, 1024).await.unwrap();
    let Value::Large {
        id: value_id,
        preview,
        ..
    } = &page.rows[0][0]
    else {
        panic!("大值应外置");
    };
    assert!(preview.is_empty());
    assert_eq!(store.read_value(*value_id).await.unwrap(), values[0]);
    cleanup(store).await;
}

#[tokio::test]
async fn pagination_matches_pages_for_byte_limits_projection_and_incomplete_results() {
    let store = fixture().await;
    let id = store
        .start_result(Uuid::new_v4(), vec![Column::new("v", "text")], "query")
        .await
        .unwrap();
    let other = store
        .start_result(Uuid::new_v4(), vec![], "query")
        .await
        .unwrap();
    store
        .append_rows(other, vec![vec![Value::Text("other".repeat(500))]; 11])
        .await
        .unwrap();
    let empty = store.result_pagination(id, 20, 1024).await.unwrap();
    assert_eq!(empty.starts, vec![0]);
    assert_eq!(empty.row_count, 0);
    assert!(!empty.complete);
    let rows = (0..137)
        .map(|i| {
            vec![Value::Text(if i == 31 {
                "wide".repeat(1000)
            } else {
                "x".repeat((i % 9 + 1) * 39)
            })]
        })
        .collect();
    store.append_rows(id, rows).await.unwrap();
    for page_size in [20, 50, 100] {
        for max_bytes in [1024, 4096, 1024 * 1024] {
            let plan = store
                .result_pagination(id, page_size, max_bytes)
                .await
                .unwrap();
            assert_eq!(plan.row_count, 137);
            assert_eq!(plan.page_size, page_size as u64);
            assert!(!plan.complete);
            let mut seen = 0;
            for (index, start) in plan.starts.iter().enumerate() {
                assert_eq!(*start, seen);
                let page = store
                    .result_page(id, *start, page_size, max_bytes)
                    .await
                    .unwrap();
                assert!(!page.rows.is_empty());
                seen += page.rows.len() as u64;
                assert_eq!(page.next, plan.starts.get(index + 1).copied());
            }
            assert_eq!(seen, 137);
        }
    }
    assert_eq!(
        store
            .result_pagination(id, 21, 1024)
            .await
            .unwrap_err()
            .code,
        "invalid_input"
    );
    store.finish_result(id, json!({})).await.unwrap();
    assert!(
        store
            .result_pagination(id, 20, 1024)
            .await
            .unwrap()
            .complete
    );
    store.release_result(id).await.unwrap();
    assert_eq!(
        store
            .result_pagination(id, 20, 1024)
            .await
            .unwrap_err()
            .code,
        "not_found"
    );
    cleanup(store).await;
}

#[tokio::test]
async fn pagination_reports_rows_that_cannot_fit_even_as_references() {
    let store = fixture().await;
    let id = store
        .start_result(Uuid::new_v4(), vec![], "query")
        .await
        .unwrap();
    store
        .append_rows(id, vec![vec![Value::Text("x".into()); 1000]])
        .await
        .unwrap();
    assert_eq!(
        store
            .result_pagination(id, 20, 1024)
            .await
            .unwrap_err()
            .code,
        "result_too_wide"
    );
    assert_eq!(
        store.result_page(id, 0, 20, 1024).await.unwrap_err().code,
        "result_too_wide"
    );
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
    // 父路径为文件时，Windows 将缺失子文件视为 NotFound，Unix 可能返回 NotADirectory。
    // 首轮共同保证逻辑释放；目录恢复后统一验证重试能完成全部清理。
    let _ = store.release_result(id).await;
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
