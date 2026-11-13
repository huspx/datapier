use super::*;

#[tokio::test]
async fn redis_task_database_records_execution_context_when_present() {
    let store = fixture().await;
    let task = store
        .create_task(
            Uuid::new_v4(),
            "main",
            "execution.start",
            json!({"_task_context":{"connection":"redis","provider":"redis","database":null,"redis_db":0}}),
            false,
        )
        .await
        .unwrap();
    store.record_task_redis_db(task.id, 7).await.unwrap();
    let context = store.task_details(task.id).await.unwrap().context.unwrap();
    assert_eq!(context.redis_db, Some(7));
    assert_eq!(context.connection, "redis");

    let legacy = store
        .create_task(Uuid::new_v4(), "main", "execution.start", json!({}), false)
        .await
        .unwrap();
    store.record_task_redis_db(legacy.id, 3).await.unwrap();
    assert!(
        store
            .task_details(legacy.id)
            .await
            .unwrap()
            .context
            .is_none()
    );
    cleanup(store).await;
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
async fn recovery_terminates_orphaned_approvals_without_retrying_sent_writes() {
    let store = fixture().await;
    let owner = Uuid::new_v4();
    let mut cases = Vec::new();
    for (approval_state, sent) in [
        ("pending", false),
        ("dispatching", false),
        ("dispatching", true),
    ] {
        let task = store
            .create_task(owner, "w", "execution.start", json!({}), false)
            .await
            .unwrap();
        store
            .update_task(task.id, "approval_required", json!({}), None)
            .await
            .unwrap();
        let approval = crate::model::ApprovalRecord {
            id: Uuid::new_v4(),
            execution_id: task.id,
            owner,
            session_id: Uuid::new_v4(),
            profile_id: Uuid::new_v4(),
            request: json!({"text":"UPDATE t SET a=1"}),
            digest: "digest".into(),
            profile_revision: 1,
            policy_revision: 1,
            created_at: 1,
            expires_at: i64::MAX,
            state: "pending".into(),
            decided_at: None,
            decision_source: None,
        };
        store.create_approval(&approval).await.unwrap();
        if approval_state == "dispatching" {
            store
                .decide_approval(approval.id, owner, "approve", "test")
                .await
                .unwrap();
        }
        if sent {
            store.mark_sent(task.id, true).await.unwrap();
        }
        cases.push((task.id, approval.id, sent));
    }
    store.recover_tasks().await.unwrap();
    for (task, approval, sent) in cases {
        assert_eq!(
            store.task(task).await.unwrap().state,
            if sent { "outcome_unknown" } else { "cancelled" }
        );
        assert_eq!(
            store.approval(approval).await.unwrap().state,
            if sent { "dispatched" } else { "invalidated" }
        );
    }
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
async fn history_searches_all_records_and_pages_stably() {
    let store = fixture().await;
    for n in 0..510 {
        store
            .create_task(
                Uuid::new_v4(),
                "history",
                "execution.start",
                json!({
                    "text": if n == 0 { "select old_marker" } else { "select recent" },
                    "_task_context": {"connection":"frozen-target", "provider":"mysql", "database":null, "redis_db":null, "targets":[]}
                }),
                false,
            )
            .await
            .unwrap();
    }
    let result = store
        .history("history", 0, 20, "old_marker", &HashMap::new(), &[])
        .await
        .unwrap();
    assert_eq!(result["total"], 1);
    assert_eq!(result["items"][0]["text"], "select old_marker");
    let first = store
        .history("history", 0, 20, "frozen-target", &HashMap::new(), &[])
        .await
        .unwrap();
    let second = store
        .history("history", 20, 20, "frozen-target", &HashMap::new(), &[])
        .await
        .unwrap();
    assert_eq!(first["total"], 510);
    assert_eq!(second["total"], 510);
    assert_ne!(first["items"][0]["id"], second["items"][0]["id"]);
    assert_eq!(
        store
            .history("history", 0, 20, "queued", &HashMap::new(), &[])
            .await
            .unwrap()["total"],
        510
    );
    cleanup(store).await;
}

#[tokio::test]
async fn history_filters_effective_state_before_count_and_pagination() {
    let store = fixture().await;
    let mut ids = Vec::new();
    for n in 0..4 {
        let task = store
            .create_task(
                Uuid::new_v4(),
                "effective",
                "execution.start",
                json!({"text":format!("statement {n}")}),
                false,
            )
            .await
            .unwrap();
        sqlx::query("UPDATE executions SET created_at=?, state=? WHERE id=?")
            .bind(1_700_000_000_i64 + n)
            .bind(if n == 3 { "failed" } else { "queued" })
            .bind(task.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        ids.push(task.id);
    }
    let overrides = HashMap::from([
        (ids[0], "failed"),
        (ids[1], "failed"),
        (ids[3], "succeeded"),
    ]);
    let first = store
        .history("effective", 0, 1, "failed", &overrides, &[])
        .await
        .unwrap();
    let second = store
        .history("effective", 1, 1, "failed", &overrides, &[])
        .await
        .unwrap();
    assert_eq!(first["total"], 2);
    assert_eq!(second["total"], 2);
    assert_eq!(first["items"][0]["id"], ids[1].to_string());
    assert_eq!(second["items"][0]["id"], ids[0].to_string());
    assert_eq!(first["items"][0]["state"], "failed");
    let localized = store
        .history(
            "effective",
            0,
            10,
            "失败",
            &overrides,
            &["failed".to_string()],
        )
        .await
        .unwrap();
    assert_eq!(localized["total"], 2);
    let succeeded = store
        .history("effective", 0, 10, "succeeded", &overrides, &[])
        .await
        .unwrap();
    assert_eq!(succeeded["total"], 1);
    assert_eq!(succeeded["items"][0]["id"], ids[3].to_string());
    assert_eq!(succeeded["items"][0]["state"], "succeeded");
    assert_eq!(
        store
            .history("effective", 0, 10, "%", &overrides, &[])
            .await
            .unwrap()["total"],
        0
    );
    cleanup(store).await;
}

#[tokio::test]
async fn history_searches_displayed_local_time_and_execution_text() {
    let store = fixture().await;
    let task = store
        .create_task(
            Uuid::new_v4(),
            "time",
            "execution.run",
            json!({"_execution_text":"select from full_source"}),
            false,
        )
        .await
        .unwrap();
    let seconds = 1_700_000_000_i64;
    sqlx::query("UPDATE executions SET created_at=? WHERE id=?")
        .bind(seconds as f64 + 0.25)
        .bind(task.id.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    let displayed = chrono::DateTime::<chrono::Utc>::from_timestamp(seconds, 0)
        .unwrap()
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    let by_time = store
        .history("time", 0, 10, &displayed, &HashMap::new(), &[])
        .await
        .unwrap();
    assert_eq!(by_time["total"], 1);
    assert_eq!(by_time["items"][0]["id"], task.id.to_string());
    assert_eq!(
        store
            .history("time", 0, 10, "full_source", &HashMap::new(), &[])
            .await
            .unwrap()["total"],
        1
    );
    cleanup(store).await;
}

#[tokio::test]
async fn history_large_text_is_bounded_but_details_remain_complete() {
    let store = fixture().await;
    let text = format!("{}{}", "界".repeat(1024), "x".repeat(5 * 1024 * 1024));
    let mut ids = Vec::new();
    for _ in 0..2 {
        let task = store.create_task(Uuid::new_v4(), "large", "execution.start", json!({
            "text":text,
            "_task_context":{"connection":"db", "provider":"mysql", "database":null, "redis_db":null, "targets":[]}
        }), false).await.unwrap();
        ids.push(task.id);
    }
    let page = store
        .history("large", 0, 20, "", &HashMap::new(), &[])
        .await
        .unwrap();
    let items = page["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    for item in items {
        assert_eq!(item["text_truncated"], true);
        assert_eq!(item["text_bytes"].as_u64(), Some(text.len() as u64));
        assert_eq!(item["text"].as_str().unwrap().chars().count(), 1024);
        assert!(item["text"].as_str().unwrap().len() <= 4096);
    }
    assert!(serde_json::to_vec(&page).unwrap().len() < 16384);
    for id in ids {
        assert_eq!(
            store.task_details(id).await.unwrap().text.as_deref(),
            Some(text.as_str())
        );
    }
    cleanup(store).await;
}

#[tokio::test]
async fn history_retention_keeps_live_results_and_pending_execution() {
    let store = fixture().await;
    let owner = Uuid::new_v4();
    let mut tasks = Vec::new();
    for _ in 0..4 {
        tasks.push(
            store
                .create_task(
                    owner,
                    "main",
                    "execution.run",
                    json!({"text":"select 1"}),
                    false,
                )
                .await
                .unwrap(),
        );
    }
    for task in &tasks {
        sqlx::query("UPDATE executions SET created_at=1 WHERE id=?")
            .bind(task.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
    }
    for task in &tasks[..2] {
        store
            .update_task(task.id, "succeeded", json!({}), None)
            .await
            .unwrap();
    }
    sqlx::query("UPDATE executions SET state='approval_required' WHERE id=?")
        .bind(tasks[2].id.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    let result = store
        .start_result(tasks[0].id, vec![], "query")
        .await
        .unwrap();
    store.finish_result(result, json!({})).await.unwrap();
    assert_eq!(store.cleanup_history().await.unwrap(), 0);
    assert!(store.task(tasks[1].id).await.is_ok());
    let mut settings = store.settings().await.unwrap();
    settings.history.auto_cleanup_enabled = true;
    settings.history.retention_days = Some(1);
    store.set_settings(&settings).await.unwrap();
    assert!(store.task(tasks[0].id).await.is_ok());
    assert_eq!(store.task(tasks[1].id).await.unwrap_err().code, "not_found");
    assert!(store.task(tasks[2].id).await.is_ok());
    assert!(store.task(tasks[3].id).await.is_ok());
    store.release_result(result).await.unwrap();
    assert_eq!(store.cleanup_history().await.unwrap(), 1);
    assert_eq!(store.task(tasks[0].id).await.unwrap_err().code, "not_found");
    cleanup(store).await;
}

#[tokio::test]
async fn history_max_entries_uses_original_order_across_deletions() {
    let store = fixture().await;
    let mut ids = Vec::new();
    for index in 1..=5 {
        let task = store
            .create_task(Uuid::new_v4(), "main", "execution.start", json!({}), false)
            .await
            .unwrap();
        store
            .update_task(task.id, "succeeded", json!({}), None)
            .await
            .unwrap();
        sqlx::query("UPDATE executions SET created_at=? WHERE id=?")
            .bind(index as i64)
            .bind(task.id.to_string())
            .execute(&store.pool)
            .await
            .unwrap();
        ids.push(task.id);
    }
    let mut settings = store.settings().await.unwrap();
    settings.history.auto_cleanup_enabled = true;
    settings.history.max_entries = Some(2);
    store.set_settings(&settings).await.unwrap();
    for id in &ids[..3] {
        assert!(store.task(*id).await.is_err());
    }
    for id in &ids[3..] {
        assert!(store.task(*id).await.is_ok());
    }
    cleanup(store).await;
}
