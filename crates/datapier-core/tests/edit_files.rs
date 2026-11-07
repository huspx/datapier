//! 单值文件登记、恢复和草稿引用的持久化边界。
use datapier_core::{
    Store,
    model::editing::{
        EditFilePurpose, EditFileRegistration, EditFileState, EditingLimits, ExternalEditorConfig,
        ExternalEditorMode,
    },
};
use serde_json::json;
use uuid::Uuid;

async fn store() -> Store {
    let root = std::env::temp_dir().join(format!("datapier-edit-files-{}", Uuid::new_v4()));
    Store::open(&root).await.unwrap()
}
async fn cleanup(store: Store) {
    let root = (*store.root).clone();
    store.pool.close().await;
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[test]
fn editor_configuration_requires_one_placeholder_and_valid_limits() {
    let mut config = ExternalEditorConfig {
        mode: ExternalEditorMode::Gui,
        program: "editor".into(),
        args: vec!["--file={file}".into()],
    };
    assert!(config.validate().is_ok());
    let path = std::env::temp_dir().join("value");
    assert_eq!(
        config.arguments(&path).unwrap(),
        vec![format!("--file={}", path.display())]
    );
    config.args.push("{file}".into());
    assert!(config.validate().is_err());
    let mut limits = EditingLimits::default();
    assert!(limits.validate().is_ok());
    limits.max_text_editor_bytes = limits.max_value_bytes + 1;
    assert!(limits.validate().is_err());
}

#[tokio::test]
async fn draft_reference_blocks_cleanup_until_workspace_switches() {
    let store = store().await;
    let file = store
        .register_edit_file(EditFileRegistration {
            workspace_id: "workspace",
            profile_id: "profile",
            purpose: EditFilePurpose::Draft,
            target: json!({"column":"body"}),
            format: "txt",
            baseline_digest: "digest",
            draft_revision: 7,
        })
        .await
        .unwrap();
    let file = store
        .publish_edit_file(file.id, file.revision, b"user value", 1024)
        .await
        .unwrap();
    assert_eq!(
        store
            .read_edit_file(file.id, file.revision, 1024)
            .await
            .unwrap(),
        b"user value"
    );
    let state =
        json!({"value_drafts":[{"edit_file_id": file.id, "edit_file_revision":file.revision}]});
    assert_eq!(
        store
            .workspace_save_with_edit_files("workspace", 0, &state)
            .await
            .unwrap(),
        1
    );
    assert!(store.edit_file_referenced(file.id).await.unwrap());
    assert_eq!(
        store
            .cleanup_edit_file(file.id, file.revision)
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    assert_eq!(
        store
            .claim_edit_file(file.id, file.revision)
            .await
            .unwrap_err()
            .code,
        "conflict"
    );
    assert!(
        store
            .workspace_save_with_edit_files("workspace", 0, &json!({}))
            .await
            .is_err()
    );
    store
        .workspace_save_with_edit_files("workspace", 1, &json!({}))
        .await
        .unwrap();
    assert!(!store.edit_file_referenced(file.id).await.unwrap());
    let pending = store.edit_file(file.id).await.unwrap();
    assert_eq!(pending.state, EditFileState::CleanupPending);
    store
        .cleanup_edit_file(file.id, pending.revision)
        .await
        .unwrap();
    assert_eq!(
        store.edit_file(file.id).await.unwrap_err().code,
        "not_found"
    );
    cleanup(store).await;
}

#[tokio::test]
async fn external_edit_survives_restart_and_atomic_replacement() {
    let store = store().await;
    let file = store
        .register_edit_file(EditFileRegistration {
            workspace_id: "workspace",
            profile_id: "profile",
            purpose: EditFilePurpose::External,
            target: json!({"column":"x"}),
            format: "txt",
            baseline_digest: "digest",
            draft_revision: 1,
        })
        .await
        .unwrap();
    let file = store
        .publish_edit_file(file.id, file.revision, b"before", 64)
        .await
        .unwrap();
    let path = store.edit_file_path(file.id, file.revision).await.unwrap();
    let replacement = path.with_extension("replacement");
    std::fs::write(&replacement, b"after").unwrap();
    #[cfg(windows)]
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    assert_eq!(
        store
            .read_edit_file(file.id, file.revision, 64)
            .await
            .unwrap(),
        b"after"
    );
    assert_eq!(
        store
            .read_edit_file(file.id, file.revision, 4)
            .await
            .unwrap_err()
            .code,
        "resource_limit"
    );
    let exported = store.root.join("exported.txt");
    store
        .export_edit_file(file.id, file.revision, 64, &exported)
        .await
        .unwrap();
    assert_eq!(std::fs::read(&exported).unwrap(), b"after");
    assert!(
        store
            .export_edit_file(file.id, file.revision, 64, &exported)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&exported).unwrap(), b"after");
    store.recover_edit_files().await.unwrap();
    let file = store.edit_file(file.id).await.unwrap();
    assert_eq!(file.state, EditFileState::Orphaned);
    let file = store.claim_edit_file(file.id, file.revision).await.unwrap();
    store
        .cleanup_edit_file(file.id, file.revision)
        .await
        .unwrap();
    cleanup(store).await;
}

#[cfg(unix)]
#[tokio::test]
async fn external_symlink_is_rejected_and_cleanup_record_is_retained() {
    use std::os::unix::fs::symlink;
    let store = store().await;
    let file = store
        .register_edit_file(EditFileRegistration {
            workspace_id: "workspace",
            profile_id: "profile",
            purpose: EditFilePurpose::External,
            target: json!({}),
            format: "txt",
            baseline_digest: "digest",
            draft_revision: 1,
        })
        .await
        .unwrap();
    let file = store
        .publish_edit_file(file.id, file.revision, b"value", 64)
        .await
        .unwrap();
    let path = store.edit_file_path(file.id, file.revision).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    symlink("/etc/passwd", &path).unwrap();
    assert_eq!(
        store
            .read_edit_file(file.id, file.revision, 1024)
            .await
            .unwrap_err()
            .code,
        "permission_denied"
    );
    assert!(
        store
            .cleanup_edit_file(file.id, file.revision)
            .await
            .is_err()
    );
    assert_eq!(
        store.edit_file(file.id).await.unwrap().state,
        EditFileState::CleanupPending
    );
    std::fs::remove_file(path).unwrap();
    let file = store.edit_file(file.id).await.unwrap();
    store
        .cleanup_edit_file(file.id, file.revision)
        .await
        .unwrap();
    cleanup(store).await;
}

#[tokio::test]
async fn concurrent_workspace_saves_return_revision_conflicts_instead_of_sqlite_busy() {
    let store = store().await;
    let gate = std::sync::Arc::new(tokio::sync::Barrier::new(16));
    let results = futures_util::future::join_all((0..16).map(|writer| {
        let store = store.clone();
        let gate = gate.clone();
        async move {
            gate.wait().await;
            store
                .workspace_save_with_edit_files("race", 0, &json!({"writer":writer}))
                .await
        }
    }))
    .await;
    let mut saved = 0;
    for result in results {
        match result {
            Ok(revision) => {
                assert_eq!(revision, 1);
                saved += 1;
            }
            Err(error) => assert_eq!(error.code, "conflict", "{error}"),
        }
    }
    assert_eq!(saved, 1);
    assert_eq!(store.workspace_load("race").await.unwrap()["revision"], 1);
    cleanup(store).await;
}

#[tokio::test]
async fn workspace_intents_share_budget_and_restore_without_exposing_markers() {
    let store = store().await;
    let state = json!({"tabs":[{"profile_id":"profile",
        "schema_draft":{"text":"s".repeat(140_000),"nested":{"edit_file_id":"user data"}},
        "replace_draft":{"text":"r".repeat(140_000)}
    }]});
    let (revision, canonical) = store
        .workspace_save_prepared("workspace", 0, state.clone(), None, 16 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(revision, 1);
    assert_eq!(canonical, state);
    let saved = store.workspace_load("workspace").await.unwrap();
    let tab = &saved["state"]["tabs"][0];
    assert!(serde_json::to_vec(tab).unwrap().len() < 256 * 1024);
    let marker = &tab["schema_draft"]["$datapier_intent"];
    let file_id = Uuid::parse_str(marker["edit_file_id"].as_str().unwrap()).unwrap();
    let file_revision = marker["edit_file_revision"].as_u64().unwrap();
    assert!(
        store
            .cleanup_edit_file(file_id, file_revision)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .workspace_load_prepared("workspace", 16 * 1024 * 1024)
            .await
            .unwrap()["state"],
        state
    );
    assert_eq!(
        store
            .workspace_load_prepared("workspace", 1024)
            .await
            .unwrap_err()
            .code,
        "resource_limit"
    );
    assert_eq!(
        store
            .workspace_load_prepared("workspace", 800_000)
            .await
            .unwrap_err()
            .code,
        "resource_limit"
    );
    store
        .workspace_save_prepared("copy", 0, canonical, Some("workspace"), 16 * 1024 * 1024)
        .await
        .unwrap();
    let copied = store.workspace_load("copy").await.unwrap();
    assert_ne!(
        copied["state"]["tabs"][0]["schema_draft"]["$datapier_intent"]["edit_file_id"],
        marker["edit_file_id"]
    );
    assert_eq!(
        store
            .workspace_load_prepared("copy", 16 * 1024 * 1024)
            .await
            .unwrap()["state"],
        state
    );
    let (_, copied_wire) = store
        .workspace_save_prepared(
            "copy-markers",
            0,
            saved["state"].clone(),
            Some("workspace"),
            16 * 1024 * 1024,
        )
        .await
        .unwrap();
    assert_eq!(copied_wire, state);
    let copied_marker = store.workspace_load("copy-markers").await.unwrap();
    assert_ne!(
        copied_marker["state"]["tabs"][0]["schema_draft"]["$datapier_intent"]["edit_file_id"],
        marker["edit_file_id"]
    );
    store.recover_edit_files().await.unwrap();
    assert_eq!(
        store.edit_file(file_id).await.unwrap().state,
        EditFileState::Ready
    );
    cleanup(store).await;
}

#[tokio::test]
async fn workspace_intent_rejects_tampered_markers_and_file_records() {
    let store = store().await;
    let state =
        json!({"tabs":[{"profile_id":"profile","schema_draft":{"text":"x".repeat(300_000)}}]});
    store
        .workspace_save_prepared("workspace", 0, state, None, 16 * 1024 * 1024)
        .await
        .unwrap();
    let saved = store.workspace_load("workspace").await.unwrap();
    let mut state = saved["state"].clone();
    let marker = &state["tabs"][0]["schema_draft"]["$datapier_intent"];
    let id = marker["edit_file_id"].as_str().unwrap().to_owned();
    state["tabs"][0]["schema_draft"]["$datapier_intent"]["version"] = json!(2);
    sqlx::query("UPDATE workspaces SET payload=? WHERE id='workspace'")
        .bind(state.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    assert!(
        store
            .workspace_load_prepared("workspace", 16 * 1024 * 1024)
            .await
            .is_err()
    );
    state["tabs"][0]["schema_draft"]["$datapier_intent"]["version"] = json!(1);
    sqlx::query("UPDATE workspaces SET payload=? WHERE id='workspace'")
        .bind(state.to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE value_edit_files SET format='json' WHERE id=?")
        .bind(&id)
        .execute(&store.pool)
        .await
        .unwrap();
    assert!(
        store
            .workspace_load_prepared("workspace", 16 * 1024 * 1024)
            .await
            .is_err()
    );
    sqlx::query("UPDATE value_edit_files SET format='intent' WHERE id=?")
        .bind(&id)
        .execute(&store.pool)
        .await
        .unwrap();
    std::fs::remove_file(
        store
            .root
            .join("edit-files")
            .join(&id)
            .join(format!("{id}.intent")),
    )
    .unwrap();
    assert!(
        store
            .workspace_load_prepared("workspace", 16 * 1024 * 1024)
            .await
            .is_err()
    );
    assert!(
        store
            .workspace_save_prepared("workspace", 1, state.clone(), None, 16 * 1024 * 1024)
            .await
            .is_err()
    );
    assert_eq!(
        store.workspace_load("workspace").await.unwrap()["revision"],
        1
    );
    sqlx::query("DELETE FROM value_edit_files WHERE id=?")
        .bind(&id)
        .execute(&store.pool)
        .await
        .unwrap();
    assert!(
        store
            .workspace_load_prepared("workspace", 16 * 1024 * 1024)
            .await
            .is_err()
    );
    cleanup(store).await;
}

#[tokio::test]
async fn typed_user_json_metadata_names_are_preserved() {
    let store = store().await;
    let state = json!({"tabs":[{"profile_id":"profile", "pending":{"rows":[{"values":{
        "v":{"type":"json","value":{"edit_file_id":"literal","edit_file_revision":7,"$datapier_intent":{}}}
    }}]}}]});
    let (_, saved) = store
        .workspace_save_prepared("workspace", 0, state.clone(), None, 16 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(saved, state);
    assert_eq!(
        store
            .workspace_load_prepared("workspace", 1024 * 1024)
            .await
            .unwrap()["state"],
        state
    );
    cleanup(store).await;
}

#[tokio::test]
async fn saving_a_small_marker_cannot_restore_a_large_intent_over_budget() {
    let store = store().await;
    let state =
        json!({"tabs":[{"profile_id":"profile","schema_draft":{"text":"x".repeat(500_000)}}]});
    store
        .workspace_save_prepared("workspace", 0, state.clone(), None, 16 * 1024 * 1024)
        .await
        .unwrap();
    let saved = store.workspace_load("workspace").await.unwrap();
    assert!(serde_json::to_vec(&saved["state"]).unwrap().len() < 4096);
    let error = store
        .workspace_save_prepared("workspace", 1, saved["state"].clone(), None, 128 * 1024)
        .await
        .unwrap_err();
    assert_eq!(error.code, "resource_limit");
    assert_eq!(store.workspace_load("workspace").await.unwrap(), saved);
    assert_eq!(
        store
            .workspace_load_prepared("workspace", 16 * 1024 * 1024)
            .await
            .unwrap()["state"],
        state
    );
    cleanup(store).await;
}

#[tokio::test]
async fn normalized_descriptors_are_charged_before_workspace_cas() {
    let store = store().await;
    store
        .workspace_save_prepared("workspace", 0, json!({}), None, 16 * 1024 * 1024)
        .await
        .unwrap();
    let rows: Vec<_> = (0..500)
        .map(|_| json!({"values":{"v":{"type":"text","value":"x"}}}))
        .collect();
    let state = json!({"tabs":[{"profile_id":"profile","schema_draft":{"text":"x".repeat(270_000)},"pending":{"rows":rows}}]});
    let encoded = serde_json::to_vec(&state).unwrap();
    let mut estimate = datapier_core::model::editing::JsonMemoryEstimate::default();
    estimate.feed(&encoded);
    let memory_budget = estimate.heap_bound() + encoded.len() * 2 + 64 * 1024;
    drop(encoded);
    let error = store
        .workspace_save_prepared("workspace", 1, state, None, memory_budget)
        .await
        .unwrap_err();
    assert_eq!(error.code, "resource_limit");
    let saved = store.workspace_load("workspace").await.unwrap();
    assert_eq!(saved["revision"], 1);
    assert_eq!(saved["state"], json!({}));
    cleanup(store).await;
}

#[tokio::test]
async fn column_names_survive_copy_and_reference_lifecycle() {
    let store = store().await;
    let file = store
        .register_edit_file(EditFileRegistration {
            workspace_id: "source",
            profile_id: "profile",
            purpose: EditFilePurpose::Draft,
            target: json!({}),
            format: "json",
            baseline_digest: "",
            draft_revision: 1,
        })
        .await
        .unwrap();
    let file = store
        .publish_edit_file(
            file.id,
            file.revision,
            br#"{"type":"text","value":"hello"}"#,
            1024,
        )
        .await
        .unwrap();
    let large = json!({"type":"large","value":{"id":file.id,"original_type":"text","bytes":5,"preview":""}});
    let user_json = json!({"type":"json","value":{"edit_file_id":"literal","edit_file_revision":4,"schema_draft":{"id":file.id},"replace_draft":true}});
    let state = json!({"tabs":[{"profile_id":"profile","pending":{
        "edit_files":[{"edit_file_id":file.id,"edit_file_revision":file.revision}],
        "rows":[{"key":{"edit_file_revision":{"type":"text","value":"key"}},"values":{
            "edit_file_id":{"type":"text","value":"column"},
            "edit_file_revision":{"type":"text","value":"column"},
            "schema_draft":large,"replace_draft":large,"json":user_json
        }}]
    }}]});
    let (_, saved) = store
        .workspace_save_prepared("source", 0, state, None, 8 * 1024 * 1024)
        .await
        .unwrap();
    store.recover_edit_files().await.unwrap();
    assert_eq!(
        store.edit_file(file.id).await.unwrap().state,
        EditFileState::Ready
    );
    let (_, copy) = store
        .workspace_save_prepared("copy", 0, saved, Some("source"), 8 * 1024 * 1024)
        .await
        .unwrap();
    let pending = &copy["tabs"][0]["pending"];
    let copy_id = &pending["edit_files"][0]["edit_file_id"];
    assert_ne!(copy_id, &json!(file.id));
    assert_eq!(
        &pending["rows"][0]["values"]["schema_draft"]["value"]["id"],
        copy_id
    );
    assert_eq!(
        &pending["rows"][0]["values"]["replace_draft"]["value"]["id"],
        copy_id
    );
    assert_eq!(pending["rows"][0]["values"]["json"], user_json);
    store
        .workspace_save_prepared("copy", 1, copy, None, 8 * 1024 * 1024)
        .await
        .unwrap();
    cleanup(store).await;
}

#[tokio::test]
async fn unrelated_workspace_does_not_prevent_recovery_or_dereference() {
    let store = store().await;
    sqlx::query("INSERT INTO workspaces(id,revision,payload) VALUES(?,1,?)")
        .bind("unrelated")
        .bind(json!({"data":"x".repeat(2_000_000)}).to_string())
        .execute(&store.pool)
        .await
        .unwrap();
    store.recover_edit_files().await.unwrap();
    let file = store
        .register_edit_file(EditFileRegistration {
            workspace_id: "source",
            profile_id: "profile",
            purpose: EditFilePurpose::Draft,
            target: json!({}),
            format: "json",
            baseline_digest: "",
            draft_revision: 1,
        })
        .await
        .unwrap();
    let file = store
        .publish_edit_file(
            file.id,
            file.revision,
            br#"{"type":"text","value":"x"}"#,
            1024,
        )
        .await
        .unwrap();
    let state =
        json!({"value_drafts":[{"edit_file_id":file.id,"edit_file_revision":file.revision}]});
    store
        .workspace_save_prepared("source", 0, state, None, 128 * 1024)
        .await
        .unwrap();
    store
        .workspace_save_prepared("source", 1, json!({}), None, 128 * 1024)
        .await
        .unwrap();
    assert_eq!(
        store.edit_file(file.id).await.unwrap().state,
        EditFileState::CleanupPending
    );
    cleanup(store).await;
}

#[tokio::test]
async fn dense_draft_is_checked_before_decoding_and_after_file_changes() {
    let store = store().await;
    let file = store
        .register_edit_file(EditFileRegistration {
            workspace_id: "source",
            profile_id: "profile",
            purpose: EditFilePurpose::Draft,
            target: json!({}),
            format: "json",
            baseline_digest: "",
            draft_revision: 1,
        })
        .await
        .unwrap();
    let bytes = serde_json::to_vec(&json!({"type":"json","value":vec![0; 5000]})).unwrap();
    let file = store
        .publish_edit_file(file.id, file.revision, &bytes, 1024 * 1024)
        .await
        .unwrap();
    let bound = store
        .draft_value_memory_bound(file.id, file.revision)
        .await
        .unwrap();
    assert!(bound > bytes.len() * 3);
    assert_eq!(
        store
            .read_draft_value_bounded(file.id, file.revision, bound - 1)
            .await
            .unwrap_err()
            .code,
        "resource_limit"
    );
    store
        .read_draft_value_bounded(file.id, file.revision, bound)
        .await
        .unwrap();
    let path = store
        .root
        .join(&file.relative_dir)
        .join(format!("{}.json", file.id));
    std::fs::write(
        path,
        serde_json::to_vec(&json!({"type":"json","value":vec![0; 10000]})).unwrap(),
    )
    .unwrap();
    assert_eq!(
        store
            .read_draft_value_bounded(file.id, file.revision, bound)
            .await
            .unwrap_err()
            .code,
        "resource_limit"
    );
    cleanup(store).await;
}

#[tokio::test]
async fn recovery_preserves_references_and_claims_interrupted_files() {
    let store = store().await;
    let registration = || EditFileRegistration {
        workspace_id: "source",
        profile_id: "profile",
        purpose: EditFilePurpose::Draft,
        target: json!({}),
        format: "json",
        baseline_digest: "",
        draft_revision: 1,
    };
    let preparing = store.register_edit_file(registration()).await.unwrap();
    let ready = store.register_edit_file(registration()).await.unwrap();
    let ready = store
        .publish_edit_file(ready.id, ready.revision, b"{}", 1024)
        .await
        .unwrap();
    let kept = store.register_edit_file(registration()).await.unwrap();
    let kept = store
        .publish_edit_file(kept.id, kept.revision, b"{}", 1024)
        .await
        .unwrap();
    store.workspace_save_with_edit_files("source", 0, &json!({"tabs":[{"pending":{"edit_files":[{"edit_file_id":kept.id,"edit_file_revision":kept.revision}]}}]})).await.unwrap();
    store.recover_edit_files().await.unwrap();
    assert_eq!(
        store.edit_file(kept.id).await.unwrap().state,
        EditFileState::Ready
    );
    for id in [preparing.id, ready.id] {
        let file = store.edit_file(id).await.unwrap();
        assert_eq!(file.state, EditFileState::CleanupPending);
        store.claim_edit_file(id, file.revision).await.unwrap();
    }
    assert!(
        store
            .workspace_save_with_edit_files(
                "source",
                1,
                &json!({"tabs":[{"pending":{"edit_files":[{"edit_file_id":kept.id}]}}]})
            )
            .await
            .is_err()
    );
    assert!(store.edit_file_referenced(kept.id).await.unwrap());
    cleanup(store).await;
}
