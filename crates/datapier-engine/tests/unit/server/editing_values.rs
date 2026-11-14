use super::*;

async fn settled(engine: &Arc<Engine>, started: Value) -> model::TaskRecord {
    let id = serde_json::from_value(started["task_id"].clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let task = engine.store.task(id).await.unwrap();
            if !matches!(task.state.as_str(), "queued" | "running" | "cancelling") {
                return task;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

async fn command(
    engine: &Arc<Engine>,
    owner: Uuid,
    session: Uuid,
    text: &str,
) -> model::TaskRecord {
    let started = engine
        .dispatch(
            owner,
            "execution.start",
            json!({"session_id":session,"text":text}),
        )
        .await
        .unwrap();
    let task = settled(engine, started).await;
    assert_eq!(task.state, "succeeded", "{:?}", task.error);
    task
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 指定的隔离 Redis"]
async fn imported_value_remains_stageable_after_idle_sweep() {
    let (engine, root) = crate::server::tests::engine().await;
    let owner = Uuid::new_v4();
    engine.register(owner);
    let profile_id = Uuid::new_v4();
    let port: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")
        .unwrap()
        .parse()
        .unwrap();
    engine.dispatch(owner, "profile.save", json!({"profile":{"id":profile_id,"name":"import-idle-test","provider":"redis","access_mode":"read_write","connection":{"kind":"direct","host":"127.0.0.1","port":port,"username":"","redis_db":15,"tls":{"enabled":false}}}})).await.unwrap();
    let opened = engine
        .dispatch(owner, "connection.open", json!({"profile_id":profile_id}))
        .await
        .unwrap();
    let session = serde_json::from_value(opened["session_id"].clone()).unwrap();
    let key = format!("datapier-import-idle-{}", Uuid::new_v4());
    command(&engine, owner, session, &format!("SET {key} before")).await;
    let target = json!({"database":"15","name":key,"kind":"string"});
    let baseline = engine
        .dispatch(
            owner,
            "change.open",
            json!({"session_id":session,"target":target}),
        )
        .await
        .unwrap();
    let value = engine.dispatch(owner, "value.open", json!({"session_id":session,"target":target,"result_id":baseline["result_id"],"row_no":0,"column":"value","edit_token":baseline["token"],"role":"string","draft_revision":0})).await.unwrap();
    let id = serde_json::from_value(value["value_session"].clone()).unwrap();
    let entry = engine.value_entry(owner, id).unwrap();
    // 直接推进旧会话的空闲年龄，不依赖 sleep、调度速度或修改系统时钟。
    entry.lock().await.touched = Instant::now() - Duration::from_secs(60);
    let file = root.join("returned.txt");
    tokio::fs::write(&file, b"edited").await.unwrap();
    let imported = engine
        .dispatch(
            owner,
            "value.file.import",
            json!({"value_session":id,"revision":value["revision"],"path":file}),
        )
        .await
        .unwrap();
    engine
        .editing_expire_values(Duration::from_secs(30))
        .await
        .unwrap();
    let staged = engine
        .dispatch(
            owner,
            "value.stage",
            json!({"value_session":id,"revision":imported["revision"],"draft_revision":0}),
        )
        .await;
    let idle_error = if staged.is_ok() {
        entry.lock().await.touched = Instant::now() - Duration::from_secs(60);
        engine
            .editing_expire_values(Duration::from_secs(30))
            .await
            .unwrap();
        Some(
            engine
                .dispatch(
                    owner,
                    "value.read",
                    json!({"value_session":id,"revision":imported["revision"]}),
                )
                .await
                .unwrap_err(),
        )
    } else {
        None
    };
    let get = command(&engine, owner, session, &format!("GET {key}")).await;
    let page = engine
        .store
        .result_page(get.results[0], 0, 1, 4096)
        .await
        .unwrap();
    command(&engine, owner, session, &format!("DEL {key}")).await;
    engine
        .dispatch(owner, "connection.close", json!({"session_id":session}))
        .await
        .unwrap();
    engine.store.pool.close().await;
    drop(engine);
    tokio::fs::remove_dir_all(root).await.unwrap();
    let staged = staged.expect("刚导入的值必须可继续暂存");
    let typed: TypedValue = serde_json::from_value(staged["value"].clone()).unwrap();
    assert_eq!(value::to_bytes(&typed).unwrap().1.unwrap(), b"edited");
    assert_eq!(staged["redis_changes"][0]["operation"], "set_string");
    let raw = value::to_bytes(&page.rows[0][0]).unwrap().1.unwrap();
    let unchanged: TypedValue = serde_json::from_slice(&raw).unwrap();
    assert_eq!(value::to_bytes(&unchanged).unwrap().1.unwrap(), b"before");
    assert_eq!(
        idle_error.unwrap().code,
        "expired",
        "真正再次空闲后仍须回收"
    );
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT 指定的隔离 Redis"]
async fn value_pages_obey_response_budget_and_schema_invalidation_preserves_edits() {
    let (engine, root) = crate::server::tests::engine().await;
    let owner = Uuid::new_v4();
    engine.register(owner);
    let profile_id = Uuid::new_v4();
    let port: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")
        .unwrap()
        .parse()
        .unwrap();
    engine.dispatch(owner, "profile.save", json!({"profile":{"id":profile_id,"name":"value-budget-test","provider":"redis","access_mode":"read_write","connection":{"kind":"direct","host":"127.0.0.1","port":port,"username":"","redis_db":15,"tls":{"enabled":false}}}})).await.unwrap();
    let opened = engine
        .dispatch(owner, "connection.open", json!({"profile_id":profile_id}))
        .await
        .unwrap();
    let session = serde_json::from_value(opened["session_id"].clone()).unwrap();
    let key = format!("datapier-value-budget-{}", Uuid::new_v4());
    let original = "x".repeat(12000);
    command(&engine, owner, session, &format!("SET {key} {original}")).await;
    let target = json!({"database":"15","name":key,"kind":"string"});
    let baseline = engine
        .dispatch(
            owner,
            "change.open",
            json!({"session_id":session,"target":target}),
        )
        .await
        .unwrap();
    for limit in [1024, 8192] {
        let mut settings = engine.store.settings().await.unwrap();
        settings.limits.response_bytes = limit;
        settings.limits.value_preview_bytes = settings.limits.value_preview_bytes.min(limit);
        engine.store.set_settings(&settings).await.unwrap();
        let mut page = engine.dispatch(owner, "value.open", json!({"session_id":session,"target":target,"result_id":baseline["result_id"],"row_no":0,"column":"value","edit_token":baseline["token"],"role":"string","draft_revision":0})).await.unwrap();
        let id = page["value_session"].clone();
        let revision = page["revision"].clone();
        let mut bytes = Vec::new();
        loop {
            assert!(serde_json::to_vec(&page).unwrap().len() + 128 <= limit);
            bytes.extend(STANDARD.decode(page["data_b64"].as_str().unwrap()).unwrap());
            let Some(offset) = page["next"].as_u64() else {
                break;
            };
            page = engine.dispatch(owner, "value.read", json!({"value_session":id,"revision":revision,"offset":offset,"max_bytes":262144})).await.unwrap();
        }
        assert_eq!(bytes, original.as_bytes());
        let patched = engine.dispatch(owner, "value.patch", json!({"value_session":id,"revision":revision,"offset":0,"delete_bytes":1,"data_b64":STANDARD.encode(b"y"),"text":true})).await.unwrap();
        assert!(serde_json::to_vec(&patched).unwrap().len() + 128 <= limit);
        engine.editing_invalidate_value_baselines(profile_id).await;
        let error = engine
            .dispatch(
                owner,
                "value.stage",
                json!({"value_session":id,"revision":patched["revision"],"draft_revision":0}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "conflict");
        let file = root.join("stale-import.txt");
        tokio::fs::write(&file, b"imported").await.unwrap();
        let error = engine
            .dispatch(
                owner,
                "value.file.import",
                json!({"value_session":id,"revision":patched["revision"],"path":file}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "conflict");
        let read = engine
            .dispatch(
                owner,
                "value.read",
                json!({"value_session":id,"revision":patched["revision"],"offset":0}),
            )
            .await
            .unwrap();
        assert!(
            STANDARD
                .decode(read["data_b64"].as_str().unwrap())
                .unwrap()
                .starts_with(b"y")
        );
        let gif = STANDARD
            .decode("R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==")
            .unwrap();
        let image = engine.dispatch(owner, "value.patch", json!({"value_session":id,"revision":patched["revision"],"offset":0,"delete_bytes":original.len(),"data_b64":STANDARD.encode(gif),"text":false})).await.unwrap();
        let id_uuid = serde_json::from_value(id.clone()).unwrap();
        let entry = engine.value_entry(owner, id_uuid).unwrap();
        let initial_charge = entry.lock().await.reserved;
        for _ in 0..3 {
            let preview = engine
                .dispatch(
                    owner,
                    "value.preview",
                    json!({"value_session":id,"revision":image["revision"],"mode":"image"}),
                )
                .await
                .unwrap();
            let preview_id = preview["preview_id"].clone();
            let mut offset = 0;
            let mut png = Vec::new();
            loop {
                let chunk = engine.dispatch(owner, "value.read", json!({"value_session":preview_id,"revision":image["revision"],"offset":offset,"max_bytes":262144})).await.unwrap();
                assert!(serde_json::to_vec(&chunk).unwrap().len() + 128 <= limit);
                png.extend(
                    STANDARD
                        .decode(chunk["data_b64"].as_str().unwrap())
                        .unwrap(),
                );
                match chunk["next"].as_u64() {
                    Some(next) => offset = next,
                    None => break,
                }
            }
            assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"));
            engine
                .dispatch(
                    owner,
                    "value.release",
                    json!({"value_session":preview_id,"revision":image["revision"]}),
                )
                .await
                .unwrap();
            assert_eq!(entry.lock().await.reserved, initial_charge);
        }
        engine
            .dispatch(
                owner,
                "value.release",
                json!({"value_session":id,"revision":image["revision"]}),
            )
            .await
            .unwrap();
    }
    command(&engine, owner, session, &format!("DEL {key}")).await;
    engine
        .dispatch(owner, "connection.close", json!({"session_id":session}))
        .await
        .unwrap();
    engine.store.pool.close().await;
    drop(engine);
    tokio::fs::remove_dir_all(root).await.unwrap();
}
