use super::*;
#[test]
fn submit_target_must_match_token_target() {
    let target = model::ResourceTarget {
        database: Some("db_a".into()),
        name: "items".into(),
        key_b64: None,
        kind: "table".into(),
    };
    assert!(Engine::validate_submit_target(&target, None).is_ok());
    assert!(
        Engine::validate_submit_target(&target, Some(&json!({"database":"db_a", "name":"items"})))
            .is_ok()
    );
    assert!(
        Engine::validate_submit_target(&target, Some(&json!({"database":"db_a", "name":"other"})))
            .is_err()
    );
    assert!(
        Engine::validate_submit_target(&target, Some(&json!({"database":"db_b", "name":"items"})))
            .is_err()
    );
}
#[tokio::test]
async fn profile_gate_and_dirty_checks_cover_other_sessions() {
    let (engine, root) = crate::server::tests::engine().await;
    let profile = Uuid::new_v4();
    let gate = engine.editing_gate(profile);
    let other = engine.editing_gate(profile);
    let independent = engine.editing_gate(Uuid::new_v4());
    let guard = gate.lock().await;
    assert!(other.try_lock().is_err());
    assert!(independent.try_lock().is_ok());
    drop(guard);
    assert!(other.try_lock().is_ok());
    let owner = Uuid::new_v4();
    let session = Uuid::new_v4();
    engine
        .state
        .lock()
        .unwrap()
        .editing
        .coordination
        .scopes
        .insert(
            (owner, session, "db".into(), "items".into()),
            Scope {
                profile,
                revision: 1,
                dirty: true,
            },
        );
    assert_eq!(
        engine
            .editing_check_dirty(profile, Some(&json!({"database":"db","name":"items"})))
            .unwrap_err()
            .code,
        "busy"
    );
    assert!(
        engine
            .editing_check_dirty(profile, Some(&json!({"database":"db","name":"other"})))
            .is_ok()
    );
    assert!(
        engine
            .editing_check_dirty_except(profile, None, Some(session))
            .is_ok()
    );
    assert_eq!(
        engine.editing_check_dirty(profile, None).unwrap_err().code,
        "busy"
    );
    engine.editing_forget_scopes(owner);
    assert!(engine.editing_check_dirty(profile, None).is_ok());
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
#[tokio::test]
async fn other_clients_receive_scoped_invalidation_without_sql() {
    let (engine, root) = crate::server::tests::engine().await;
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    assert!(engine.register(first));
    assert!(engine.register(second));
    let mut events = engine.state.lock().unwrap().event_senders[&second].subscribe();
    let profile = Uuid::new_v4();
    engine
        .editing_invalidate_schema(
            profile,
            None,
            Some(&json!({"database":"db","name":"table","sql":"secret"})),
        )
        .await;
    let event = events.recv().await.unwrap();
    assert_eq!(event.kind, "schema.changed");
    assert_eq!(event.payload["scope"]["profile_id"], profile.to_string());
    assert!(!event.payload.to_string().contains("secret"));
    engine.disconnect(first).await;
    engine.disconnect(second).await;
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
