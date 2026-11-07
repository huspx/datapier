use super::*;
use base64::engine::general_purpose::STANDARD;
use std::path::PathBuf;

async fn fixture() -> (Arc<Engine>, PathBuf, Uuid) {
    let (engine, root) = crate::server::tests::engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    (engine, root, owner)
}

async fn cleanup(engine: Arc<Engine>, root: PathBuf) {
    engine.store.pool.close().await;
    drop(engine);
    tokio::fs::remove_dir_all(root).await.unwrap();
}

async fn begin(
    engine: &Arc<Engine>,
    owner: Uuid,
    workspace: &str,
    revision: u64,
    bytes: usize,
    copy_from: Option<&str>,
) -> Value {
    let mut params = json!({"workspace_id":workspace,"expected_revision":revision,"bytes":bytes});
    if let Some(source) = copy_from {
        params["copy_from"] = json!(source);
    }
    let result = engine
        .dispatch(owner, "workspace.save.begin", params)
        .await
        .unwrap();
    assert_eq!(result["chunk_bytes"], 262144);
    result["upload_id"].clone()
}

async fn append(
    engine: &Arc<Engine>,
    owner: Uuid,
    upload: &Value,
    offset: usize,
    bytes: &[u8],
) -> Result<Value> {
    engine
        .dispatch(
            owner,
            "workspace.save.append",
            json!({"upload_id":upload,"offset":offset,"data_b64":STANDARD.encode(bytes)}),
        )
        .await
}

async fn upload(
    engine: &Arc<Engine>,
    owner: Uuid,
    workspace: &str,
    revision: u64,
    state: &Value,
    copy_from: Option<&str>,
) -> Value {
    let bytes = serde_json::to_vec(state).unwrap();
    let id = begin(engine, owner, workspace, revision, bytes.len(), copy_from).await;
    let mut offset = 0;
    for chunk in bytes.chunks(262144) {
        let result = append(engine, owner, &id, offset, chunk).await.unwrap();
        offset += chunk.len();
        assert_eq!(result["received"], offset);
    }
    id
}

async fn commit(engine: &Arc<Engine>, owner: Uuid, id: &Value) -> Result<Value> {
    engine
        .dispatch(owner, "workspace.save.commit", json!({"upload_id":id}))
        .await
}

async fn close(engine: &Arc<Engine>, owner: Uuid, id: &Value) {
    engine
        .dispatch(owner, "workspace.save.close", json!({"upload_id":id}))
        .await
        .unwrap();
}

async fn read_all(engine: &Arc<Engine>, owner: Uuid, id: &Value, response_limit: usize) -> Vec<u8> {
    let mut output = Vec::new();
    loop {
        let result = engine
            .dispatch(
                owner,
                "workspace.save.read",
                json!({"upload_id":id,"offset":output.len()}),
            )
            .await
            .unwrap();
        // IPC 的实际响应包含 id 和 result，限制适用于整个响应。
        assert!(
            serde_json::to_vec(&json!({"id":u64::MAX,"result":result}))
                .unwrap()
                .len()
                <= response_limit
        );
        let chunk = STANDARD
            .decode(result["data_b64"].as_str().unwrap())
            .unwrap();
        assert!(!chunk.is_empty());
        output.extend_from_slice(&chunk);
        assert!(result["total_bytes"].as_u64().unwrap() as usize >= output.len());
        if result["next"].is_null() {
            assert_eq!(
                result["total_bytes"].as_u64().unwrap() as usize,
                output.len()
            );
            return output;
        }
        assert_eq!(result["next"].as_u64().unwrap() as usize, output.len());
    }
}

#[tokio::test]
async fn owner_isolation_and_invalid_chunks_preserve_upload() {
    let (engine, root, owner) = fixture().await;
    let other = Uuid::new_v4();
    assert!(engine.register(other));
    let bytes = br#"{"value":"valid"}"#;
    let id = begin(&engine, owner, "isolated", 0, bytes.len(), None).await;
    assert_eq!(
        append(&engine, owner, &id, 0, &bytes[..5]).await.unwrap()["received"],
        5
    );
    assert!(append(&engine, other, &id, 5, &bytes[5..]).await.is_err());
    assert!(commit(&engine, other, &id).await.is_err());
    assert!(
        engine
            .dispatch(
                other,
                "workspace.save.read",
                json!({"upload_id":id,"offset":0})
            )
            .await
            .is_err()
    );
    assert!(append(&engine, owner, &id, 4, b"x").await.is_err());
    assert!(
        append(&engine, owner, &id, 5, &vec![b'x'; bytes.len()])
            .await
            .is_err()
    );
    assert!(append(&engine, owner, &id, usize::MAX, b"x").await.is_err());
    assert_eq!(
        append(&engine, owner, &id, 5, &bytes[5..]).await.unwrap()["received"],
        bytes.len()
    );
    let first = commit(&engine, owner, &id).await.unwrap();
    assert_eq!(first["revision"], 1);
    assert_eq!(commit(&engine, owner, &id).await.unwrap(), first);
    assert!(
        engine
            .dispatch(
                other,
                "workspace.save.read",
                json!({"upload_id":id,"offset":0})
            )
            .await
            .is_err()
    );
    assert_eq!(
        engine.store.workspace_load("isolated").await.unwrap(),
        json!({"revision":1,"state":{"value":"valid"}})
    );
    close(&engine, owner, &id).await;
    cleanup(engine, root).await;
}

#[tokio::test]
async fn canonical_read_respects_small_complete_response_limits() {
    let (engine, root, owner) = fixture().await;
    let state = json!({"text":"界\\\"".repeat(6000)});
    for limit in [1024, 8192] {
        let mut settings = engine.store.settings().await.unwrap();
        settings.limits.response_bytes = limit;
        settings.limits.value_preview_bytes = settings.limits.value_preview_bytes.min(limit);
        engine.store.set_settings(&settings).await.unwrap();
        let workspace = format!("response-{limit}");
        let id = upload(&engine, owner, &workspace, 0, &state, None).await;
        let saved = commit(&engine, owner, &id).await.unwrap();
        let canonical = read_all(&engine, owner, &id, limit).await;
        assert_eq!(saved["bytes"].as_u64().unwrap() as usize, canonical.len());
        assert_eq!(serde_json::from_slice::<Value>(&canonical).unwrap(), state);
        close(&engine, owner, &id).await;
    }
    cleanup(engine, root).await;
}

#[tokio::test]
async fn large_edit_is_externalized_and_copy_restores_exact_value() {
    let (engine, root, owner) = fixture().await;
    let text = "user-edited-界".repeat(650_000);
    let edited = json!({"type":"text","value":text});
    let state = json!({"tabs":[{"profile_id":Uuid::new_v4(),"pending":{"target":{"database":"db","name":"t"},"rows":[{"operation":"update","key":{},"values":{"body":edited}}],"redis":[],"edit_files":[]}}]});
    assert!(serde_json::to_vec(&state).unwrap().len() > 8 * 1024 * 1024);
    let id = upload(&engine, owner, "large-source", 0, &state, None).await;
    let first = commit(&engine, owner, &id).await.unwrap();
    assert_eq!(commit(&engine, owner, &id).await.unwrap(), first);
    let canonical: Value =
        serde_json::from_slice(&read_all(&engine, owner, &id, 1_048_576).await).unwrap();
    let pending = &canonical["tabs"][0]["pending"];
    assert_eq!(pending["rows"][0]["values"]["body"]["type"], "large");
    let reference = &pending["edit_files"][0];
    let file_id: Uuid = serde_json::from_value(reference["edit_file_id"].clone()).unwrap();
    let revision = reference["edit_file_revision"].as_u64().unwrap();
    let restored = engine
        .store
        .read_edit_file(file_id, revision, 32 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&restored).unwrap(), edited);
    assert_eq!(
        engine.store.workspace_load("large-source").await.unwrap()["revision"],
        1
    );
    close(&engine, owner, &id).await;

    let copied = upload(
        &engine,
        owner,
        "large-copy",
        0,
        &canonical,
        Some("large-source"),
    )
    .await;
    commit(&engine, owner, &copied).await.unwrap();
    let copy: Value =
        serde_json::from_slice(&read_all(&engine, owner, &copied, 1_048_576).await).unwrap();
    let reference = &copy["tabs"][0]["pending"]["edit_files"][0];
    let copy_id: Uuid = serde_json::from_value(reference["edit_file_id"].clone()).unwrap();
    assert_ne!(copy_id, file_id);
    let bytes = engine
        .store
        .read_edit_file(
            copy_id,
            reference["edit_file_revision"].as_u64().unwrap(),
            32 * 1024 * 1024,
        )
        .await
        .unwrap();
    assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), edited);
    close(&engine, owner, &copied).await;
    cleanup(engine, root).await;
}

#[tokio::test]
async fn inline_budget_is_shared_across_user_edits() {
    let (engine, root, owner) = fixture().await;
    let first = json!({"type":"text","value":"a".repeat(140_000)});
    let second = json!({"type":"text","value":"b".repeat(140_000)});
    let state = json!({"tabs":[{"profile_id":Uuid::new_v4(),"pending":{"target":{"database":"db","name":"t"},"rows":[{"operation":"update","key":{},"values":{"a":first,"b":second}}],"redis":[],"edit_files":[]}}]});
    let id = upload(&engine, owner, "aggregate", 0, &state, None).await;
    commit(&engine, owner, &id).await.unwrap();
    let canonical: Value =
        serde_json::from_slice(&read_all(&engine, owner, &id, 1_048_576).await).unwrap();
    let pending = &canonical["tabs"][0]["pending"];
    let files = pending["edit_files"].as_array().unwrap();
    assert_eq!(files.len(), 2);
    for (name, original) in [("a", first), ("b", second)] {
        let value = &pending["rows"][0]["values"][name];
        assert_eq!(value["type"], "large");
        let file_id: Uuid = serde_json::from_value(value["value"]["id"].clone()).unwrap();
        let reference = files
            .iter()
            .find(|file| file["edit_file_id"] == json!(file_id))
            .unwrap();
        let bytes = engine
            .store
            .read_edit_file(
                file_id,
                reference["edit_file_revision"].as_u64().unwrap(),
                1_048_576,
            )
            .await
            .unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&bytes).unwrap(), original);
    }
    close(&engine, owner, &id).await;
    cleanup(engine, root).await;
}

#[tokio::test]
async fn abort_disconnect_and_idle_release_budget_and_invalidate_handles() {
    let (engine, root, owner) = fixture().await;
    let available = engine
        .store
        .available_memory_for_owner(owner)
        .await
        .unwrap();
    let id = begin(&engine, owner, "abort", 0, 1000, None).await;
    assert!(
        engine
            .store
            .available_memory_for_owner(owner)
            .await
            .unwrap()
            < available
    );
    engine
        .dispatch(owner, "workspace.save.abort", json!({"upload_id":id}))
        .await
        .unwrap();
    assert_eq!(
        engine
            .store
            .available_memory_for_owner(owner)
            .await
            .unwrap(),
        available
    );
    assert!(append(&engine, owner, &id, 0, b"{}").await.is_err());

    let idle = begin(&engine, owner, "idle", 0, 1000, None).await;
    assert!(
        engine
            .store
            .available_memory_for_owner(owner)
            .await
            .unwrap()
            < available
    );
    // 零时长确定性地过期所有当前句柄，不依赖计时器或调度速度。
    engine
        .sweep_workspace_uploads(Duration::ZERO)
        .await
        .unwrap();
    assert_eq!(
        engine
            .store
            .available_memory_for_owner(owner)
            .await
            .unwrap(),
        available
    );
    assert!(commit(&engine, owner, &idle).await.is_err());

    let disconnected = begin(&engine, owner, "disconnect", 0, 1000, None).await;
    assert!(
        engine
            .store
            .available_memory_for_owner(owner)
            .await
            .unwrap()
            < available
    );
    engine.disconnect(owner).await;
    assert_eq!(
        engine
            .store
            .available_memory_for_owner(owner)
            .await
            .unwrap(),
        available
    );
    assert!(engine.register(owner));
    assert!(
        append(&engine, owner, &disconnected, 0, b"{}")
            .await
            .is_err()
    );
    cleanup(engine, root).await;
}

#[tokio::test]
async fn dense_json_is_rejected_before_parsing_when_node_budget_is_exhausted() {
    let (engine, root, owner) = fixture().await;
    let mut settings = engine.store.settings().await.unwrap();
    settings.limits.result_memory_bytes_per_client = 1_048_576;
    engine.store.set_settings(&settings).await.unwrap();
    let dense = format!("[{}]", vec!["0"; 8192].join(","));
    let id = begin(&engine, owner, "dense", 0, dense.len(), None).await;
    let available = engine
        .store
        .available_memory_for_owner(owner)
        .await
        .unwrap();
    assert_eq!(
        append(&engine, owner, &id, 0, dense.as_bytes())
            .await
            .unwrap_err()
            .code,
        "resource_limit"
    );
    assert_eq!(
        engine
            .store
            .available_memory_for_owner(owner)
            .await
            .unwrap(),
        available
    );
    assert!(commit(&engine, owner, &id).await.is_err());
    assert_eq!(
        engine.store.workspace_load("dense").await.unwrap(),
        json!({"revision":0,"state":{}})
    );
    engine
        .dispatch(owner, "workspace.save.abort", json!({"upload_id":id}))
        .await
        .unwrap();
    cleanup(engine, root).await;
}

#[tokio::test]
async fn cas_conflict_keeps_previously_saved_workspace() {
    let (engine, root, owner) = fixture().await;
    let stale = upload(&engine, owner, "cas", 0, &json!({"value":"stale"}), None).await;
    let current = upload(&engine, owner, "cas", 0, &json!({"value":"current"}), None).await;
    assert_eq!(
        commit(&engine, owner, &current).await.unwrap()["revision"],
        1
    );
    assert_eq!(
        commit(&engine, owner, &stale).await.unwrap_err().code,
        "conflict"
    );
    assert!(commit(&engine, owner, &stale).await.is_err());
    assert_eq!(
        engine.store.workspace_load("cas").await.unwrap(),
        json!({"revision":1,"state":{"value":"current"}})
    );
    close(&engine, owner, &stale).await;
    close(&engine, owner, &current).await;
    cleanup(engine, root).await;
}

#[tokio::test]
async fn large_workspace_load_is_chunked_owned_and_released() {
    let (engine, root, owner) = fixture().await;
    let state = json!({"query":"完整草稿".repeat(800_000)});
    let id = upload(&engine, owner, "large-load", 0, &state, None).await;
    assert_eq!(commit(&engine, owner, &id).await.unwrap()["revision"], 1);
    close(&engine, owner, &id).await;
    let available = engine
        .store
        .available_memory_for_owner(owner)
        .await
        .unwrap();
    let loaded = engine
        .dispatch(
            owner,
            "workspace.load",
            json!({"workspace_id":"large-load"}),
        )
        .await
        .unwrap();
    let handle = &loaded["load_handle"];
    assert!(loaded["bytes"].as_u64().unwrap() > 8 * 1024 * 1024);
    assert_eq!(loaded["revision"], 1);
    let other = Uuid::new_v4();
    assert!(engine.register(other));
    assert_eq!(
        engine
            .dispatch(
                other,
                "workspace.save.read",
                json!({"upload_id":handle,"offset":0})
            )
            .await
            .unwrap_err()
            .code,
        "permission_denied"
    );
    assert_eq!(
        engine
            .dispatch(
                owner,
                "workspace.save.append",
                json!({"upload_id":handle,"offset":0,"data_b64":"eA=="})
            )
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    let restored: Value =
        serde_json::from_slice(&read_all(&engine, owner, handle, 8 * 1024 * 1024).await).unwrap();
    assert_eq!(restored, state);
    assert!(
        engine
            .store
            .available_memory_for_owner(owner)
            .await
            .unwrap()
            < available
    );
    close(&engine, owner, handle).await;
    assert_eq!(
        engine
            .store
            .available_memory_for_owner(owner)
            .await
            .unwrap(),
        available
    );
    cleanup(engine, root).await;
}

#[tokio::test]
async fn workspace_load_respects_response_limit_and_does_not_erase_on_budget_failure() {
    let (engine, root, owner) = fixture().await;
    let state = json!({"query":"q".repeat(32_768),"items":vec![0;8192]});
    let id = upload(&engine, owner, "load-budget", 0, &state, None).await;
    commit(&engine, owner, &id).await.unwrap();
    close(&engine, owner, &id).await;
    let mut settings = engine.store.settings().await.unwrap();
    settings.limits.response_bytes = 8192;
    settings.limits.value_preview_bytes = 1024;
    engine.store.set_settings(&settings).await.unwrap();
    let loaded = engine
        .dispatch(
            owner,
            "workspace.load",
            json!({"workspace_id":"load-budget"}),
        )
        .await
        .unwrap();
    let handle = &loaded["load_handle"];
    let restored: Value =
        serde_json::from_slice(&read_all(&engine, owner, handle, 8192).await).unwrap();
    assert_eq!(restored, state);
    close(&engine, owner, handle).await;
    settings = engine.store.settings().await.unwrap();
    settings.limits.result_memory_bytes_per_client = 1_048_576;
    engine.store.set_settings(&settings).await.unwrap();
    let available = engine
        .store
        .available_memory_for_owner(owner)
        .await
        .unwrap();
    assert_eq!(
        engine
            .dispatch(
                owner,
                "workspace.load",
                json!({"workspace_id":"load-budget"})
            )
            .await
            .unwrap_err()
            .code,
        "resource_limit"
    );
    assert_eq!(
        engine
            .store
            .available_memory_for_owner(owner)
            .await
            .unwrap(),
        available
    );
    assert_eq!(
        engine.store.workspace_load("load-budget").await.unwrap(),
        json!({"revision":1,"state":state})
    );
    cleanup(engine, root).await;
}
