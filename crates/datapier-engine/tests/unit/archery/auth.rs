use super::*;

#[tokio::test]
async fn credential_read_failure_releases_controller_for_another_client() {
    let (engine, root) = engine().await;
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    assert!(engine.register(first));
    assert!(engine.register(second));
    let fixture = Fixture::new().await;
    crate::server::archery::FAIL_CREDENTIAL_READ
        .lock()
        .unwrap()
        .push(fixture.server.id);
    let failure = engine
        .dispatch(
            first,
            "archery.auth.begin",
            json!({
                "server":fixture.server,"expected_revision":0,"operation_id":Uuid::new_v4()
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(failure.code, "credentials");
    crate::server::archery::FAIL_CREDENTIAL_READ
        .lock()
        .unwrap()
        .retain(|id| *id != fixture.server.id);
    let auth = login(&engine, second, &fixture).await;
    assert_eq!(auth["state"], "ready");
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn credential_failure_in_candidate_preserves_shared_ready_session() {
    let (engine, root, first, fixture, ready) = setup().await;
    let second = Uuid::new_v4();
    assert!(engine.register(second));
    let server = engine
        .store
        .archery_server(fixture.server.id)
        .await
        .unwrap();
    crate::server::archery::FAIL_CREDENTIAL_READ
        .lock()
        .unwrap()
        .push(server.id);
    let error = engine
        .dispatch(
            second,
            "archery.auth.begin",
            json!({
                "server":server,"expected_revision":1,"candidate":true,"operation_id":Uuid::new_v4()
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "credentials");
    crate::server::archery::FAIL_CREDENTIAL_READ
        .lock()
        .unwrap()
        .retain(|id| *id != fixture.server.id);
    let status = engine
        .dispatch(
            first,
            "archery.auth.status",
            json!({"auth_id":ready["auth_id"]}),
        )
        .await
        .unwrap();
    assert_eq!(status["state"], "ready");
    let resumed = engine
        .dispatch(
            second,
            "archery.auth.begin",
            json!({
                "server_id":fixture.server.id,"expected_revision":1,"operation_id":Uuid::new_v4()
            }),
        )
        .await
        .unwrap();
    assert_eq!(resumed["state"], "ready");
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn failed_first_auth_releases_wizard_for_another_owner() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    assert!(engine.register(owner));
    assert!(engine.register(other));
    let fixture = Fixture::new().await;
    assert_eq!(
        engine
            .dispatch(
                owner,
                "archery.auth.begin",
                json!({"server":fixture.server,"expected_revision":0,"password":"wrong","operation_id":Uuid::new_v4()})
            )
            .await
            .unwrap_err()
            .code,
        "auth_failed"
    );
    let auth = login(&engine, other, &fixture).await;
    engine.dispatch(other, "archery.server.save", json!({"auth_id":auth["auth_id"],"server":fixture.server,"expected_revision":0,"password_update":{"kind":"replace","value":"good","persist":false}})).await.unwrap();
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancelled_first_auth_releases_wizard_for_another_owner() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    let other = Uuid::new_v4();
    assert!(engine.register(owner));
    assert!(engine.register(other));
    let fixture = Fixture::new().await;
    fixture.block_auth.store(true, Ordering::Release);
    let id = Uuid::new_v4();
    let task = {
        let engine = engine.clone();
        let server = fixture.server.clone();
        tokio::spawn(async move {
            engine
                .dispatch(
                    owner,
                    "archery.auth.begin",
                    json!({"server":server,"expected_revision":0,"password":"good","operation_id":id}),
                )
                .await
        })
    };
    fixture.wait().await;
    engine
        .dispatch(
            owner,
            "archery.operation.cancel",
            json!({"operation_id":id}),
        )
        .await
        .unwrap();
    assert_eq!(task.await.unwrap().unwrap_err().code, "cancelled");
    fixture.block_auth.store(false, Ordering::Release);
    fixture.release.add_permits(1);
    let auth = login(&engine, other, &fixture).await;
    engine.dispatch(other, "archery.server.save", json!({"auth_id":auth["auth_id"],"server":fixture.server,"expected_revision":0,"password_update":{"kind":"replace","value":"good","persist":false}})).await.unwrap();
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn candidate_failure_and_name_only_save_preserve_shared_login() {
    let (engine, root, owner, fixture, first) = setup().await;
    let other = Uuid::new_v4();
    assert!(engine.register(other));
    let saved = engine
        .store
        .archery_server(fixture.server.id)
        .await
        .unwrap();
    let second = engine
        .dispatch(
            other,
            "archery.auth.begin",
            json!({"server":saved,"expected_revision":1,"operation_id":Uuid::new_v4()}),
        )
        .await
        .unwrap();
    assert_ne!(first["auth_id"], second["auth_id"]);
    assert_eq!(second["state"], "ready");
    assert_eq!(
        engine
            .dispatch(
                other,
                "archery.instances",
                json!({"auth_id":first["auth_id"],"operation_id":Uuid::new_v4()})
            )
            .await
            .unwrap_err()
            .code,
        "auth_required"
    );
    assert_eq!(
        engine
            .dispatch(
                other,
                "archery.auth.close",
                json!({"auth_id":first["auth_id"]})
            )
            .await
            .unwrap_err()
            .code,
        "auth_required"
    );
    let mut candidate = saved.clone();
    candidate.name = "candidate".into();
    assert_eq!(engine.dispatch(owner, "archery.auth.begin", json!({"server":candidate,"expected_revision":1,"password":"wrong","operation_id":Uuid::new_v4()})).await.unwrap_err().code, "auth_failed");
    assert_eq!(
        engine
            .store
            .archery_secret(fixture.server.id)
            .await
            .unwrap()
            .as_deref(),
        Some("good")
    );
    let before = fixture.count();
    let mut renamed = saved;
    renamed.name = "renamed".into();
    let info = engine
        .dispatch(
            owner,
            "archery.server.save",
            json!({"server":renamed,"expected_revision":1}),
        )
        .await
        .unwrap();
    assert_eq!(info["name"], "renamed");
    assert_eq!(fixture.count(), before);
    engine.disconnect(owner).await;
    assert!(
        engine
            .dispatch(
                other,
                "archery.instances",
                json!({"auth_id":second["auth_id"],"operation_id":Uuid::new_v4()})
            )
            .await
            .is_ok()
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn candidate_save_binds_verified_password_and_preserves_shared_slot() {
    let (engine, root, owner, fixture, first) = setup().await;
    let other = Uuid::new_v4();
    assert!(engine.register(other));
    let saved = engine
        .store
        .archery_server(fixture.server.id)
        .await
        .unwrap();
    let observer = engine
        .dispatch(
            other,
            "archery.auth.begin",
            json!({"server":saved,"expected_revision":1,"operation_id":Uuid::new_v4()}),
        )
        .await
        .unwrap();
    let candidate = engine.dispatch(owner, "archery.auth.begin", json!({"server":saved,"expected_revision":1,"password":"newpass","operation_id":Uuid::new_v4()})).await.unwrap();
    assert_eq!(candidate["state"], "ready");
    let rejected = engine.dispatch(owner, "archery.server.save", json!({"server":saved,"expected_revision":1,"auth_id":candidate["auth_id"],"password_update":{"kind":"replace","value":"different","persist":false}})).await;
    assert!(rejected.is_err());
    assert_eq!(
        engine
            .store
            .archery_secret(fixture.server.id)
            .await
            .unwrap()
            .as_deref(),
        Some("good")
    );
    engine.dispatch(owner, "archery.server.save", json!({"server":saved,"expected_revision":1,"auth_id":candidate["auth_id"],"password_update":{"kind":"replace","value":"newpass","persist":false}})).await.unwrap();
    assert_eq!(
        engine
            .store
            .archery_secret(fixture.server.id)
            .await
            .unwrap()
            .as_deref(),
        Some("newpass")
    );
    assert!(
        engine
            .dispatch(
                other,
                "archery.instances",
                json!({"auth_id":observer["auth_id"],"operation_id":Uuid::new_v4()})
            )
            .await
            .is_ok()
    );
    assert!(first["auth_id"].is_string());
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn saved_server_handles_share_ready_slot_without_another_login() {
    let (engine, root, owner, fixture, first) = setup().await;
    let other = Uuid::new_v4();
    assert!(engine.register(other));
    let saved = engine
        .store
        .archery_server(fixture.server.id)
        .await
        .unwrap();
    let before = fixture.count();
    let second = engine
        .dispatch(
            other,
            "archery.auth.begin",
            json!({"server":saved,"expected_revision":1,"operation_id":Uuid::new_v4()}),
        )
        .await
        .unwrap();
    assert_ne!(first["auth_id"], second["auth_id"]);
    assert_eq!(second["auth_generation"], first["auth_generation"]);
    assert_eq!(second["controller"], false);
    assert!(second["controller_token"].is_null());
    assert_eq!(fixture.count(), before);
    engine
        .dispatch(
            owner,
            "archery.auth.close",
            json!({"auth_id":first["auth_id"]}),
        )
        .await
        .unwrap();
    assert!(
        engine
            .dispatch(
                other,
                "archery.instances",
                json!({"auth_id":second["auth_id"],"operation_id":Uuid::new_v4()})
            )
            .await
            .is_ok()
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn cancelled_before_start_never_sends_http_or_changes_auth_generation() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let id = Uuid::new_v4();
    engine
        .dispatch(
            owner,
            "archery.operation.cancel",
            json!({"operation_id":id}),
        )
        .await
        .unwrap();
    let before = fixture.count();
    assert_eq!(
        engine
            .dispatch(
                owner,
                "archery.instances",
                json!({"auth_id":auth["auth_id"],"operation_id":id})
            )
            .await
            .unwrap_err()
            .code,
        "cancelled"
    );
    assert_eq!(fixture.count(), before);
    let auth_operation = Uuid::new_v4();
    engine
        .dispatch(
            owner,
            "archery.operation.cancel",
            json!({"operation_id":auth_operation}),
        )
        .await
        .unwrap();
    assert_eq!(engine.dispatch(owner, "archery.auth.begin", json!({"server_id":fixture.server.id,"expected_revision":1,"operation_id":auth_operation})).await.unwrap_err().code, "cancelled");
    assert_eq!(fixture.count(), before);
    let still = engine
        .dispatch(
            owner,
            "archery.auth.begin",
            json!({"server_id":fixture.server.id,"expected_revision":1,"operation_id":Uuid::new_v4()}),
        )
        .await
        .unwrap();
    assert_eq!(still["auth_generation"], auth["auth_generation"]);
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
