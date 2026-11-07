use super::*;

#[tokio::test]
async fn batch_failure_is_atomic_and_fixed_ids_allow_outcome_reconciliation() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let one = profile(fixture.server.id, 7, "one");
    let mut invalid = profile(fixture.server.id, 8, "two");
    if let ConnectionConfig::Archery(binding) = &mut invalid.connection {
        binding.databases.push("missing".into());
    }
    assert_eq!(
        save(&engine, owner, &fixture, &auth, vec![one.clone(), invalid])
            .await
            .unwrap_err()
            .code,
        "target_not_selected"
    );
    assert!(engine.store.profiles().await.unwrap().is_empty());
    let two = profile(fixture.server.id, 8, "two");
    let result = save(
        &engine,
        owner,
        &fixture,
        &auth,
        vec![one.clone(), two.clone()],
    )
    .await
    .unwrap();
    assert_eq!(result["added"].as_array().unwrap().len(), 2);
    let stored = engine.store.profiles().await.unwrap();
    assert_eq!(stored.len(), 2);
    assert!(stored.iter().any(|p| p.id == one.id));
    assert!(stored.iter().any(|p| p.id == two.id));
    let repeated = save(
        &engine,
        owner,
        &fixture,
        &auth,
        vec![one.clone(), two.clone()],
    )
    .await
    .unwrap();
    assert!(repeated["added"].as_array().unwrap().is_empty());
    let after = engine.store.profiles().await.unwrap();
    assert_eq!(after.len(), 2);
    assert!(after.iter().any(|p| p.id == one.id));
    assert!(after.iter().any(|p| p.id == two.id));
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn batch_rechecks_full_baseline_and_local_changes_need_no_discovery() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let one = profile(fixture.server.id, 7, "one");
    let two = profile(fixture.server.id, 8, "two");
    save(
        &engine,
        owner,
        &fixture,
        &auth,
        vec![one.clone(), two.clone()],
    )
    .await
    .unwrap();
    let mut renamed = one.clone();
    renamed.name = "renamed".into();
    let before = fixture.count();
    let stale = engine.dispatch(owner, "archery.profile.apply_batch", json!({"server_id":fixture.server.id,"expected_server_revision":1,"expected_profiles":[one.clone()],"desired_profiles":[renamed.clone()]})).await;
    assert!(stale.is_err());
    assert_eq!(engine.store.profiles().await.unwrap().len(), 2);
    let changed = engine.dispatch(owner, "archery.profile.apply_batch", json!({"server_id":fixture.server.id,"expected_server_revision":1,"expected_profiles":[one, two.clone()],"desired_profiles":[renamed.clone()]})).await.unwrap();
    assert_eq!(changed["modified"], json!([renamed.id]));
    assert_eq!(changed["deleted"], json!([two.id]));
    assert_eq!(fixture.count(), before);
    let renamed = engine.store.profile(renamed.id).await.unwrap();
    let result = engine.dispatch(owner, "archery.profile.apply_batch", json!({"server_id":fixture.server.id,"expected_server_revision":1,"expected_profiles":[renamed],"desired_profiles":[]})).await.unwrap();
    assert_eq!(result["profiles"], json!([]));
    assert!(engine.store.archery_server(fixture.server.id).await.is_ok());
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn server_delete_checks_profile_set_and_removes_profiles_and_credentials() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let one = profile(fixture.server.id, 7, "one");
    save(&engine, owner, &fixture, &auth, vec![one.clone()])
        .await
        .unwrap();
    assert!(engine.dispatch(owner, "archery.server.delete", json!({"server_id":fixture.server.id,"expected_revision":1,"expected_profile_ids":[]})).await.is_err());
    assert_eq!(engine.store.profiles().await.unwrap().len(), 1);
    let other = Uuid::new_v4();
    assert!(engine.register(other));
    let saved = engine
        .store
        .archery_server(fixture.server.id)
        .await
        .unwrap();
    let other_auth = engine
        .dispatch(
            other,
            "archery.auth.begin",
            json!({"server":saved,"expected_revision":1,"operation_id":Uuid::new_v4()}),
        )
        .await
        .unwrap();
    let opened = engine
        .dispatch(
            other,
            "connection.open",
            json!({"profile_id":one.id,"database":"db_a"}),
        )
        .await
        .unwrap();
    assert_eq!(engine.dispatch(owner, "archery.server.delete", json!({"server_id":fixture.server.id,"expected_revision":1,"expected_profile_ids":[one.id]})).await.unwrap_err().code, "busy");
    engine
        .dispatch(
            other,
            "connection.close",
            json!({"session_id":opened["session_id"]}),
        )
        .await
        .unwrap();
    engine
        .dispatch(
            other,
            "archery.auth.close",
            json!({"auth_id":other_auth["auth_id"]}),
        )
        .await
        .unwrap();
    engine
        .dispatch(
            owner,
            "archery.auth.close",
            json!({"auth_id":auth["auth_id"]}),
        )
        .await
        .unwrap();
    engine.dispatch(owner, "archery.server.delete", json!({"server_id":fixture.server.id,"expected_revision":1,"expected_profile_ids":[one.id]})).await.unwrap();
    assert!(engine.store.profiles().await.unwrap().is_empty());
    assert!(
        engine
            .store
            .archery_server(fixture.server.id)
            .await
            .is_err()
    );
    assert!(
        engine
            .store
            .archery_secret(fixture.server.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        engine
            .store
            .archery_totp_secret(fixture.server.id)
            .await
            .unwrap()
            .is_none()
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn duplicate_fixed_profile_ids_return_without_deadlock_and_write_nothing() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let one = profile(fixture.server.id, 7, "one");
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        save(&engine, owner, &fixture, &auth, vec![one.clone(), one]),
    )
    .await;
    assert!(result.expect("duplicate IDs must not deadlock").is_err());
    assert!(engine.store.profiles().await.unwrap().is_empty());
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn service_and_profile_changes_cannot_race_open_registration() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let one = profile(fixture.server.id, 7, "one");
    save(&engine, owner, &fixture, &auth, vec![one.clone()])
        .await
        .unwrap();
    fixture.block_resource.store(true, Ordering::Release);
    let opening = {
        let engine = engine.clone();
        let id = one.id;
        tokio::spawn(async move {
            engine
                .dispatch(
                    owner,
                    "connection.open",
                    json!({"profile_id":id,"database":"db_a"}),
                )
                .await
        })
    };
    fixture.wait().await;
    assert_eq!(
        engine
            .dispatch(owner, "profile.delete", json!({"profile_id":one.id}))
            .await
            .unwrap_err()
            .code,
        "busy"
    );
    assert_eq!(
        engine
            .dispatch(
                owner,
                "archery.server.delete",
                json!({"server_id":fixture.server.id,"expected_revision":1,"expected_profile_ids":[one.id]})
            )
            .await
            .unwrap_err()
            .code,
        "busy"
    );
    fixture.release.add_permits(1);
    let opened = tokio::time::timeout(Duration::from_secs(3), opening)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(opened["database"], "db_a");
    assert_eq!(
        engine
            .dispatch(owner, "profile.delete", json!({"profile_id":one.id}))
            .await
            .unwrap_err()
            .code,
        "busy"
    );
    engine
        .dispatch(
            owner,
            "connection.close",
            json!({"session_id":opened["session_id"]}),
        )
        .await
        .unwrap();
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
