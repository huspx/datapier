use super::*;

#[tokio::test]
async fn oversized_batch_response_leaves_frozen_baseline_unchanged() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let original = profile(fixture.server.id, 7, "one");
    save(&engine, owner, &fixture, &auth, vec![original.clone()])
        .await
        .unwrap();
    let mut changed = original.clone();
    changed.name = "x".repeat(5 * 1024 * 1024);
    let result = engine.dispatch(owner, "archery.profile.apply_batch", json!({"server_id":fixture.server.id,"expected_server_revision":1,"expected_profiles":[original.clone()],"desired_profiles":[changed]})).await;
    assert_eq!(result.unwrap_err().code, "response_too_large");
    let profiles = engine.store.profiles().await.unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0].name, original.name);
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn oversized_archery_profile_list_returns_bounded_error() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let first = profile(fixture.server.id, 7, "one");
    let direct = Profile {
        id: Uuid::new_v4(),
        name: "d".repeat(2 * 1024 * 1024 + 1024),
        revision: 1,
        approval_policy: Default::default(),
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(model::DirectConnection {
            host: "127.0.0.1".into(),
            port: 3306,
            username: "test".into(),
            database: None,
            redis_db: 0,
            redis_databases_override: None,
            tls: model::TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    engine
        .store
        .save_profile(&direct, None, false)
        .await
        .unwrap();
    save(&engine, owner, &fixture, &auth, vec![first.clone()])
        .await
        .unwrap();
    let expected = vec![first];
    let mut desired = expected.clone();
    desired[0].name = "x".repeat(2 * 1024 * 1024 + 1024);
    engine
        .store
        .apply_archery_profiles(fixture.server.id, 1, &expected, &desired)
        .await
        .unwrap();
    let (mut peer, stream) = tokio::io::duplex(4096);
    let handler = tokio::spawn(client(engine.clone(), Uuid::new_v4(), Box::new(stream)));
    protocol::write(
        &mut peer,
        &json!({"id":"open","method":"session.open","params":{"protocol_version":crate::protocol::VERSION,"client_kind":"tsp","workspace":"default","allow_connections":[]}}),
    )
    .await
    .unwrap();
    assert_eq!(
        protocol::read(&mut peer).await.unwrap()["result"]["protocol_version"],
        4
    );
    protocol::write(
        &mut peer,
        &json!({"id":"list","method":"profile.list","params":{}}),
    )
    .await
    .unwrap();
    assert_eq!(
        protocol::read(&mut peer).await.unwrap()["error"]["code"],
        "response_too_large"
    );
    protocol::write(
        &mut peer,
        &json!({"id":"settings","method":"settings.get","params":{}}),
    )
    .await
    .unwrap();
    assert!(protocol::read(&mut peer).await.unwrap()["result"].is_object());
    protocol::write(
        &mut peer,
        &json!({"id":"close","method":"session.close","params":{}}),
    )
    .await
    .unwrap();
    assert_eq!(
        protocol::read(&mut peer).await.unwrap()["result"]["closed"],
        true
    );
    handler.await.unwrap();
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn server_handoff_survives_last_owner_disconnect() {
    for delete in [false, true] {
        let (engine, root, owner, fixture, auth) = setup().await;
        if delete {
            engine
                .dispatch(
                    owner,
                    "archery.auth.close",
                    json!({"auth_id":auth["auth_id"]}),
                )
                .await
                .unwrap();
        }
        let mut writer = engine.store.pool.acquire().await.unwrap();
        sqlx::query("BEGIN IMMEDIATE")
            .execute(&mut *writer)
            .await
            .unwrap();
        let gate = engine.service_gate(fixture.server.id);
        let worker = engine.clone();
        let server_id = fixture.server.id;
        let mut server = fixture.server.clone();
        server.revision = 1;
        server.name = "changed".into();
        let mut request = tokio::spawn(async move {
            if delete {
                worker
                    .dispatch(
                        owner,
                        "archery.server.delete",
                        json!({"server_id":server_id,"expected_revision":1,"expected_profile_ids":[]}),
                    )
                    .await
            } else {
                worker
                    .dispatch(
                        owner,
                        "archery.server.save",
                        json!({"auth_id":auth["auth_id"],"server":server,"expected_revision":1}),
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::select! {
                outcome = &mut request => panic!("handoff ended before SQLite lock blocked it (delete={delete}): {outcome:?}"),
                _ = async {
                    loop {
                        if gate.try_lock().is_err() { break; }
                        tokio::task::yield_now().await;
                    }
                } => (),
            }
        }).await.unwrap();
        for _ in 0..20 {
            tokio::task::yield_now().await;
        }
        engine.disconnect(owner).await;
        let protected = {
            let mut state = engine.state.lock().unwrap();
            state.idle = Instant::now() - Duration::from_secs(31);
            !state.stop_if_idle()
        };
        sqlx::query("COMMIT").execute(&mut *writer).await.unwrap();
        let outcome = tokio::time::timeout(Duration::from_secs(3), request)
            .await
            .unwrap()
            .unwrap();
        assert!(
            protected,
            "handoff must prevent idle shutdown (delete={delete})"
        );
        outcome.unwrap();
        if delete {
            assert!(engine.store.archery_server(server_id).await.is_err());
        } else {
            assert_eq!(
                engine
                    .store
                    .archery_server(server_id)
                    .await
                    .unwrap()
                    .revision,
                2
            );
        }
        assert!(
            engine.state.lock().unwrap().stop_if_idle(),
            "completed handoff must permit idle shutdown"
        );
        drop(writer);
        drop(engine);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[tokio::test]
async fn sql_check_paginates_large_platform_details() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let item = profile(fixture.server.id, 7, "one");
    save(&engine, owner, &fixture, &auth, vec![item.clone()])
        .await
        .unwrap();
    let opened = engine
        .dispatch(owner, "connection.open", json!({"profile_id":item.id}))
        .await
        .unwrap();
    let started = engine
        .dispatch(
            owner,
            "archery.sql_check",
            json!({"session_id":opened["session_id"],"database":"db_a","text":"UPDATE t SET a=1"}),
        )
        .await
        .unwrap();
    let task = terminal_task(&engine, owner, &started["task_id"]).await;
    assert_eq!(task["state"], "succeeded", "{task}");
    assert_eq!(task["summary"]["verdict"], "warning");
    let result = &task["summary"]["result_id"];
    let mut start = 0;
    let mut rows = 0;
    let mut pages = 0;
    loop {
        let page = engine
            .dispatch(
                owner,
                "result.read",
                json!({"result_id":result,"start":start,"max_rows":80,"max_bytes":16384}),
            )
            .await
            .unwrap();
        rows += page["rows"].as_array().unwrap().len();
        pages += 1;
        match page["next"].as_u64() {
            Some(next) => {
                assert!(next > start);
                start = next;
            }
            None => break,
        }
    }
    assert_eq!(rows, 80);
    assert!(pages > 1);
    let requests = fixture.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("POST /prefix/api/v1/workflow/sqlcheck/ "))
    );
    assert!(
        !requests
            .iter()
            .any(|request| request.starts_with("POST /prefix/query/ "))
    );
    drop(requests);
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn retained_export_survives_switch_and_auth_expiry_but_not_restart() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let item = profile(fixture.server.id, 7, "one");
    save(&engine, owner, &fixture, &auth, vec![item.clone()])
        .await
        .unwrap();
    let first = engine
        .dispatch(
            owner,
            "connection.open",
            json!({"profile_id":item.id,"database":"db_a"}),
        )
        .await
        .unwrap();
    let second = engine
        .dispatch(
            owner,
            "connection.open",
            json!({"profile_id":item.id,"database":"db_b"}),
        )
        .await
        .unwrap();
    let query = engine
        .dispatch(
            owner,
            "execution.start",
            json!({"session_id":first["session_id"],"database":"db_a","text":"SELECT 1"}),
        )
        .await
        .unwrap();
    let complete = terminal_task(&engine, owner, &query["task_id"]).await;
    assert_eq!(complete["state"], "succeeded", "{complete}");
    let result_id = complete["summary"]["results"][0].clone();
    fixture.expire_query.store(true, Ordering::Release);
    let expired = engine
        .dispatch(
            owner,
            "execution.start",
            json!({"session_id":first["session_id"],"database":"db_a","text":"SELECT 1"}),
        )
        .await
        .unwrap();
    let expired = terminal_task(&engine, owner, &expired["task_id"]).await;
    assert_eq!(expired["error"]["code"], "auth_required");
    let before = fixture.count();
    let path = root.join("cached.json");
    let export = engine.dispatch(owner, "transfer.export", json!({"session_id":second["session_id"],"result_id":result_id,"format":"json","path":path})).await.unwrap();
    let exported = terminal_task(&engine, owner, &export["task_id"]).await;
    assert_eq!(exported["state"], "succeeded", "{exported}");
    let file: Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(file["format"], "datapier-query-result");
    assert_eq!(file["summary"]["origin"]["source"], "archery");
    assert_eq!(file["summary"]["origin"]["database"], "db_a");
    assert_eq!(file["rows"].as_array().unwrap().len(), 1);
    assert_eq!(file["rows"][0][0], json!({"type":"integer","value":"1"}));
    assert_eq!(fixture.count(), before);
    let interrupted = engine.store.create_task(owner, "restart", "execution.start", json!({"_task_context":{"connection":"one","provider":"mysql","source":"archery","database":"db_a","targets":[]}}), false).await.unwrap();
    engine.store.mark_sent(interrupted.id, false).await.unwrap();
    engine.disconnect(owner).await;
    drop(engine);
    let reopened = Store::open(&root).await.unwrap();
    reopened.recover_tasks().await.unwrap();
    let recovered = reopened.task(interrupted.id).await.unwrap();
    assert_eq!(recovered.state, "failed");
    assert_eq!(recovered.error.unwrap().code, "interrupted");
    let id: Uuid = serde_json::from_value(result_id).unwrap();
    assert_eq!(
        reopened
            .result_page(id, 0, 10, 16384)
            .await
            .unwrap_err()
            .code,
        "not_found"
    );
    assert_eq!(fixture.count(), before);
    drop(reopened);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn direct_only_profile_list_keeps_existing_response_limit() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let direct = Profile {
        id: Uuid::new_v4(),
        name: "d".repeat(5 * 1024 * 1024),
        revision: 1,
        approval_policy: Default::default(),
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(model::DirectConnection {
            host: "127.0.0.1".into(),
            port: 3306,
            username: "test".into(),
            database: None,
            redis_db: 0,
            redis_databases_override: None,
            tls: model::TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    engine
        .store
        .save_profile(&direct, None, false)
        .await
        .unwrap();
    let result = engine
        .dispatch(owner, "profile.list", json!({}))
        .await
        .unwrap();
    assert_eq!(
        result["profiles"][0]["name"].as_str().unwrap().len(),
        5 * 1024 * 1024
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

async fn queued_profile_handoff_survives_disconnect() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let item = profile(fixture.server.id, 7, "one");
    let profile_id = item.id;
    let server_id = fixture.server.id;
    let gate = engine.service_gate(server_id);
    let guard = gate.lock().await;
    let worker = engine.clone();
    let (started, observed) = tokio::sync::oneshot::channel();
    let request = tokio::spawn(async move {
        let _ = started.send(());
        worker.dispatch(owner, "archery.profile.apply_batch", json!({"auth_id":auth["auth_id"],"server_id":server_id,"expected_server_revision":1,"expected_profiles":[],"desired_profiles":[item]})).await
    });
    observed.await.unwrap();
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        !request.is_finished(),
        "request must be waiting for the service gate"
    );
    engine.disconnect(owner).await;
    let protected = {
        let mut state = engine.state.lock().unwrap();
        state.idle = Instant::now() - Duration::from_secs(31);
        !state.stop_if_idle()
    };
    drop(guard);
    let outcome = tokio::time::timeout(Duration::from_secs(3), request)
        .await
        .unwrap()
        .unwrap();
    assert!(
        protected,
        "queued profile handoff must keep the engine alive"
    );
    outcome.unwrap();
    let saved = engine.store.profiles().await.unwrap();
    assert!(saved.iter().any(|profile| profile.id == profile_id));
    assert!(engine.state.lock().unwrap().stop_if_idle());
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn queued_batch_handoff_survives_owner_disconnect() {
    queued_profile_handoff_survives_disconnect().await;
}

#[tokio::test]
async fn queued_profile_delete_survives_owner_disconnect() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let item = profile(fixture.server.id, 7, "one");
    save(&engine, owner, &fixture, &auth, vec![item.clone()])
        .await
        .unwrap();
    let gate = engine.service_gate(fixture.server.id);
    let guard = gate.lock().await;
    let worker = engine.clone();
    let (started, observed) = tokio::sync::oneshot::channel();
    let request = tokio::spawn(async move {
        let _ = started.send(());
        worker
            .dispatch(owner, "profile.delete", json!({"profile_id":item.id}))
            .await
    });
    observed.await.unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if engine
                .state
                .lock()
                .unwrap()
                .archery
                .has_persistent_operations()
            {
                break;
            }
            assert!(
                !request.is_finished(),
                "delete ended before registering its handoff"
            );
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("delete must register while waiting for the service gate");
    engine.disconnect(owner).await;
    let protected = {
        let mut state = engine.state.lock().unwrap();
        state.idle = Instant::now() - Duration::from_secs(31);
        !state.stop_if_idle()
    };
    drop(guard);
    let outcome = tokio::time::timeout(Duration::from_secs(3), request)
        .await
        .unwrap()
        .unwrap();
    assert!(protected, "queued delete must keep the engine alive");
    outcome.unwrap();
    assert!(engine.store.profiles().await.unwrap().is_empty());
    assert!(engine.state.lock().unwrap().stop_if_idle());
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
