use super::*;

#[tokio::test]
async fn results_and_large_values_live_only_until_release() {
    let store = fixture().await;
    let task = store
        .create_task(
            Uuid::new_v4(),
            "test",
            "execution.run",
            json!({"text":"select 1"}),
            false,
        )
        .await
        .unwrap();
    let result = store
        .start_result(task.id, vec![Column::new("value", "text")], "query")
        .await
        .unwrap();
    let value = Value::Text("中".repeat(6000));
    store
        .append_rows(result, vec![vec![value.clone()]])
        .await
        .unwrap();
    let page = store.result_page(result, 0, 1, 16384).await.unwrap();
    let Value::Large { id, .. } = &page.rows[0][0] else {
        panic!("大值应使用内存引用")
    };
    assert_eq!(store.read_value(*id).await.unwrap(), value);
    assert_eq!(store.value_result_id(*id).await.unwrap(), result);
    assert!(!store.root.join("cache.sqlite3").exists());
    assert!(!store.root.join("values").exists());
    store.release_result(result).await.unwrap();
    assert!(store.read_value(*id).await.is_err());
    assert!(store.task(task.id).await.unwrap().results.is_empty());
    cleanup(store).await;
}

#[tokio::test]
async fn demand_and_release_wake_waiting_workers() {
    let store = fixture().await;
    let task = store
        .create_task(Uuid::new_v4(), "test", "execution.run", json!({}), false)
        .await
        .unwrap();
    let result = store.start_result(task.id, vec![], "query").await.unwrap();
    let worker = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .wait_result_demand(result, 100, &CancellationToken::new())
                .await
                .unwrap()
        })
    };
    store.set_result_demand(result, 200).await.unwrap();
    assert!(worker.await.unwrap());
    let worker = {
        let store = store.clone();
        tokio::spawn(async move {
            store
                .wait_result_demand(result, 200, &CancellationToken::new())
                .await
                .unwrap()
        })
    };
    store.release_result(result).await.unwrap();
    assert!(!worker.await.unwrap());
    cleanup(store).await;
}

#[tokio::test]
async fn live_page_replacement_invalidates_old_values() {
    let store = fixture().await;
    let task = store
        .create_task(Uuid::new_v4(), "test", "execution.run", json!({}), false)
        .await
        .unwrap();
    let result = store.start_result(task.id, vec![], "query").await.unwrap();
    store
        .append_rows(result, vec![vec![Value::Text("a".repeat(5000))]])
        .await
        .unwrap();
    let page = store.result_page(result, 0, 1, 16384).await.unwrap();
    let Value::Large { id, .. } = &page.rows[0][0] else {
        panic!("缺少引用")
    };
    store
        .replace_result_page(
            result,
            100,
            vec![],
            vec![vec![Value::Integer("1".into())]],
            json!({"has_more": true}),
        )
        .await
        .unwrap();
    assert!(store.read_value(*id).await.is_err());
    assert!(store.result_page(result, 0, 1, 16384).await.is_err());
    assert_eq!(
        store.result_page(result, 100, 1, 16384).await.unwrap().next,
        Some(101)
    );
    cleanup(store).await;
}
