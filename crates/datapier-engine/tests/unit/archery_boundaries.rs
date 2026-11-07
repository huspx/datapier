use super::*;

async fn opened() -> (Arc<Engine>, PathBuf, Uuid, Fixture, Value) {
    let (engine, root, owner, fixture, auth) = setup().await;
    let item = profile(fixture.server.id, 7, "one");
    save(&engine, owner, &fixture, &auth, vec![item.clone()])
        .await
        .unwrap();
    let session = engine
        .dispatch(
            owner,
            "connection.open",
            json!({"profile_id":item.id,"database":"db_a"}),
        )
        .await
        .unwrap();
    (engine, root, owner, fixture, session["session_id"].clone())
}

async fn run_query(
    engine: &Arc<Engine>,
    owner: Uuid,
    session: &Value,
    text: &str,
    limit: u64,
) -> Value {
    let started = engine
        .dispatch(
            owner,
            "execution.start",
            json!({"session_id":session,"database":"db_a","text":text,"limit":limit}),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let task = engine
                .dispatch(
                    owner,
                    "execution.get",
                    json!({"task_id":started["task_id"]}),
                )
                .await
                .unwrap();
            if !matches!(
                task["state"].as_str(),
                Some("queued" | "running" | "cancelling")
            ) {
                return task;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn sql_batch_prevalidates_all_and_stops_after_failure_or_cancel() {
    let (engine, root, owner, fixture, session) = opened().await;
    let invalid = run_query(&engine, owner, &session, "SELECT 1; DELETE FROM t", 1).await;
    assert_eq!(invalid["state"], "failed");
    assert_eq!(fixture.query_count.load(Ordering::Acquire), 0);

    fixture.fail_query_at.store(2, Ordering::Release);
    let failed = run_query(&engine, owner, &session, "SELECT 1; SELECT 2; SELECT 3", 1).await;
    assert_eq!(failed["state"], "failed");
    assert_eq!(failed["error"]["details"]["failed_index"], 1);
    assert_eq!(failed["error"]["details"]["not_executed"][0]["index"], 2);
    assert_eq!(fixture.query_count.load(Ordering::Acquire), 2);

    fixture.block_query_at.store(4, Ordering::Release);
    let started = engine.dispatch(owner, "execution.start", json!({"session_id":session,"database":"db_a","text":"SELECT 4; SELECT 5; SELECT 6","limit":1})).await.unwrap();
    fixture.wait().await;
    engine
        .dispatch(
            owner,
            "execution.cancel",
            json!({"task_id":started["task_id"]}),
        )
        .await
        .unwrap();
    let stopped = terminal_task(&engine, owner, &started["task_id"]).await;
    assert_eq!(stopped["state"], "wait_stopped");
    assert_eq!(fixture.query_count.load(Ordering::Acquire), 4);
    fixture.release.add_permits(1);
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn sql_size_statement_count_and_limit_boundaries_precede_http() {
    let (engine, root, owner, fixture, session) = opened().await;
    let mut max_text = String::from("SELECT '");
    max_text.push_str(&"x".repeat(1024 * 1024 - max_text.len() - 1));
    max_text.push('\'');
    assert_eq!(max_text.len(), 1024 * 1024);
    assert_eq!(
        run_query(&engine, owner, &session, &max_text, 1).await["state"],
        "succeeded"
    );
    let too_long = format!("{max_text} ");
    assert_eq!(
        run_query(&engine, owner, &session, &too_long, 1).await["error"]["code"],
        "invalid_input"
    );
    assert_eq!(fixture.query_count.load(Ordering::Acquire), 1);

    let statements = vec!["SELECT 1"; 100].join(";");
    assert_eq!(
        run_query(&engine, owner, &session, &statements, 10000).await["state"],
        "succeeded"
    );
    assert_eq!(fixture.query_count.load(Ordering::Acquire), 101);
    let too_many = format!("{statements}; SELECT 1");
    assert_eq!(
        run_query(&engine, owner, &session, &too_many, 1).await["error"]["code"],
        "invalid_input"
    );
    for limit in [0, 10001] {
        assert_eq!(
            run_query(&engine, owner, &session, "SELECT 1", limit).await["error"]["code"],
            "invalid_input"
        );
    }
    assert_eq!(fixture.query_count.load(Ordering::Acquire), 101);
    let requests = fixture.requests.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request.starts_with("POST /prefix/query/ ")
                && request.contains("limit_num=10000"))
    );
    drop(requests);
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn manual_otp_requires_client_handle_and_controller_token() {
    let (engine, root) = engine().await;
    let first = Uuid::new_v4();
    let second = Uuid::new_v4();
    assert!(engine.register(first));
    assert!(engine.register(second));
    let fixture = Fixture::new().await;
    fixture.challenge_auth.store(true, Ordering::Release);
    let challenge = login(&engine, first, &fixture).await;
    assert_eq!(challenge["state"], "challenge_required");
    assert_eq!(challenge["controller"], true);
    assert!(challenge["controller_token"].is_string());
    let verify = json!({"auth_id":challenge["auth_id"],"controller_token":challenge["controller_token"],"auth_generation":challenge["auth_generation"],"operation_id":Uuid::new_v4(),"otp":"123456"});
    assert!(
        engine
            .dispatch(second, "archery.auth.verify", verify.clone())
            .await
            .is_err()
    );
    let mut wrong = verify.clone();
    wrong["controller_token"] = json!(Uuid::new_v4());
    assert!(
        engine
            .dispatch(first, "archery.auth.verify", wrong)
            .await
            .is_err()
    );
    assert!(
        !fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.starts_with("POST /prefix/api/v1/user/2fa/verify/ "))
    );
    assert_eq!(
        engine
            .dispatch(first, "archery.auth.verify", verify)
            .await
            .unwrap()["state"],
        "ready"
    );
    engine.dispatch(first, "archery.server.save", json!({"auth_id":challenge["auth_id"],"server":fixture.server,"expected_revision":0,"password_update":{"kind":"replace","value":"good","persist":false}})).await.unwrap();
    let saved = engine
        .store
        .archery_server(fixture.server.id)
        .await
        .unwrap();
    let before = fixture.count();
    let other = engine
        .dispatch(
            second,
            "archery.auth.begin",
            json!({"server":saved,"expected_revision":1,"operation_id":Uuid::new_v4()}),
        )
        .await
        .unwrap();
    assert_eq!(other["state"], "ready");
    assert_ne!(challenge["auth_id"], other["auth_id"]);
    assert_eq!(fixture.count(), before);
    engine
        .dispatch(
            first,
            "archery.auth.close",
            json!({"auth_id":challenge["auth_id"]}),
        )
        .await
        .unwrap();
    assert!(
        engine
            .dispatch(
                second,
                "archery.instances",
                json!({"auth_id":other["auth_id"],"operation_id":Uuid::new_v4()})
            )
            .await
            .is_ok()
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn concurrent_verify_is_busy_and_sends_one_otp_request() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let fixture = Fixture::new().await;
    fixture.challenge_auth.store(true, Ordering::Release);
    let challenge = login(&engine, owner, &fixture).await;
    assert_eq!(challenge["state"], "challenge_required");
    fixture.block_verify.store(true, Ordering::Release);
    let params = json!({"auth_id":challenge["auth_id"],"controller_token":challenge["controller_token"],"auth_generation":challenge["auth_generation"],"operation_id":Uuid::new_v4(),"otp":"123456"});
    let worker = engine.clone();
    let first_params = params.clone();
    let first = tokio::spawn(async move {
        worker
            .dispatch(owner, "archery.auth.verify", first_params)
            .await
    });
    fixture.wait().await;
    let mut second = params;
    second["operation_id"] = json!(Uuid::new_v4());
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        engine.dispatch(owner, "archery.auth.verify", second),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert_eq!(error.code, "busy");
    fixture.release.add_permits(1);
    let ready = first.await.unwrap().unwrap();
    assert_eq!(ready["state"], "ready");
    assert_eq!(
        ready["auth_generation"].as_u64().unwrap(),
        challenge["auth_generation"].as_u64().unwrap() + 1
    );
    let status = engine
        .dispatch(
            owner,
            "archery.auth.status",
            json!({"auth_id":challenge["auth_id"]}),
        )
        .await
        .unwrap();
    assert_eq!(status["state"], "ready");
    assert_eq!(status["auth_generation"], ready["auth_generation"]);
    assert_eq!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .filter(|request| request.starts_with("POST /prefix/api/v1/user/2fa/verify/ "))
            .count(),
        1
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_and_cancelling_blocked_verify_leave_engine_responsive() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let fixture = Fixture::new().await;
    fixture.challenge_auth.store(true, Ordering::Release);
    let challenge = login(&engine, owner, &fixture).await;
    fixture.block_verify.store(true, Ordering::Release);
    let operation_id = Uuid::new_v4();
    let worker = engine.clone();
    let auth_id = challenge["auth_id"].clone();
    let close_auth_id = auth_id.clone();
    let close_token = challenge["controller_token"].clone();
    let close_generation = challenge["auth_generation"].clone();
    let verify = tokio::spawn(async move {
        worker.dispatch(owner, "archery.auth.verify", json!({"auth_id":auth_id,"controller_token":challenge["controller_token"],"auth_generation":challenge["auth_generation"],"operation_id":operation_id,"otp":"123456"})).await
    });
    fixture.wait().await;
    let closer = engine.clone();
    let canceler = engine.clone();
    let close = tokio::spawn(async move {
        closer.dispatch(owner, "archery.auth.close", json!({"auth_id":close_auth_id,"controller_token":close_token,"auth_generation":close_generation})).await
    });
    let cancel = tokio::spawn(async move {
        canceler
            .dispatch(
                owner,
                "archery.operation.cancel",
                json!({"operation_id":operation_id}),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        let _ = close.await.unwrap();
        let _ = cancel.await.unwrap();
        let _ = verify.await.unwrap();
    })
    .await
    .expect("close and cancel must not deadlock");
    fixture.release.add_permits(1);
    let other = Uuid::new_v4();
    assert!(engine.register(other));
    tokio::time::timeout(
        Duration::from_secs(3),
        engine.dispatch(other, "settings.get", json!({})),
    )
    .await
    .unwrap()
    .unwrap();
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn oversized_server_info_does_not_commit_save() {
    let (engine, root, owner, fixture, _auth) = setup().await;
    let before = engine
        .store
        .archery_server(fixture.server.id)
        .await
        .unwrap();
    let secret = engine
        .store
        .archery_secret(fixture.server.id)
        .await
        .unwrap();
    let mut changed = before.clone();
    let budget = 4 * 1024 * 1024 - 1024;
    let base_len = serde_json::to_vec(&changed).unwrap().len() - changed.name.len();
    changed.name = "s".repeat(budget - base_len - 10);
    assert!(serde_json::to_vec(&changed).unwrap().len() <= budget);
    let error = engine
        .dispatch(
            owner,
            "archery.server.save",
            json!({"server":changed,"expected_revision":1}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "response_too_large");
    let after = engine
        .store
        .archery_server(fixture.server.id)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(after).unwrap(),
        serde_json::to_value(before).unwrap()
    );
    assert_eq!(
        engine
            .store
            .archery_secret(fixture.server.id)
            .await
            .unwrap(),
        secret
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn oversized_deleted_response_preserves_server_profiles_and_credentials() {
    let (engine, root, owner, fixture, auth) = setup().await;
    engine
        .dispatch(
            owner,
            "archery.auth.close",
            json!({"auth_id":auth["auth_id"]}),
        )
        .await
        .unwrap();
    let secret = engine
        .store
        .archery_secret(fixture.server.id)
        .await
        .unwrap();
    let totp = engine
        .store
        .archery_totp_secret(fixture.server.id)
        .await
        .unwrap();
    let template = profile(fixture.server.id, 1, "one");
    let count = 107_600_u64;
    sqlx::query("WITH RECURSIVE numbers(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM numbers WHERE n < ?) INSERT INTO profiles(id,name,payload,credential_saved,archery_server_id,archery_instance_id) SELECT printf('00000000-0000-4000-8000-%012x', n), 'one', json_set(?, '$.id', printf('00000000-0000-4000-8000-%012x', n), '$.connection.instance_id', n), 0, ?, n FROM numbers")
        .bind(count as i64).bind(serde_json::to_string(&template).unwrap()).bind(fixture.server.id.to_string()).execute(&engine.store.pool).await.unwrap();
    let ids: Vec<Uuid> = (1..=count)
        .map(|n| Uuid::parse_str(&format!("00000000-0000-4000-8000-{n:012x}")).unwrap())
        .collect();
    assert!(serde_json::to_vec(&json!({"deleted":ids})).unwrap().len() > 4 * 1024 * 1024 - 1024);
    let error = engine
        .dispatch(
            owner,
            "archery.server.delete",
            json!({"server_id":fixture.server.id,"expected_revision":1,"expected_profile_ids":ids}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "response_too_large");
    assert_eq!(
        engine
            .store
            .archery_server(fixture.server.id)
            .await
            .unwrap()
            .revision,
        1
    );
    let remaining: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiles WHERE archery_server_id = ?")
            .bind(fixture.server.id.to_string())
            .fetch_one(&engine.store.pool)
            .await
            .unwrap();
    assert_eq!(remaining, count as i64);
    assert_eq!(
        engine
            .store
            .archery_secret(fixture.server.id)
            .await
            .unwrap(),
        secret
    );
    assert_eq!(
        engine
            .store
            .archery_totp_secret(fixture.server.id)
            .await
            .unwrap(),
        totp
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn execution_registration_waits_for_server_gate() {
    let (engine, root, owner, fixture, session) = opened().await;
    let gate = engine.service_gate(fixture.server.id);
    let guard = gate.lock().await;
    let worker = engine.clone();
    let request = tokio::spawn(async move {
        worker
            .dispatch(
                owner,
                "execution.start",
                json!({"session_id":session,"database":"db_a","text":"SELECT 1","limit":1}),
            )
            .await
    });
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    assert!(
        !request.is_finished(),
        "execution must wait for the service gate"
    );
    assert_eq!(fixture.query_count.load(Ordering::Acquire), 0);
    drop(guard);
    let started = tokio::time::timeout(Duration::from_secs(3), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        terminal_task(&engine, owner, &started["task_id"]).await["state"],
        "succeeded"
    );
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn automatic_otp_authenticates_and_saves_independent_credentials() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let mut fixture = Fixture::new().await;
    fixture.server.totp_mode = TotpMode::Automatic;
    fixture.challenge_auth.store(true, Ordering::Release);
    let uri = format!("otpauth://totp/Archery:tester?secret={}", "A".repeat(32));
    let auth = engine.dispatch(owner, "archery.auth.begin", json!({"server":fixture.server,"expected_revision":0,"password":"good","totp_uri":uri,"operation_id":Uuid::new_v4()})).await.unwrap();
    assert_eq!(auth["state"], "ready");
    assert!(
        fixture
            .requests
            .lock()
            .unwrap()
            .iter()
            .any(|request| request.starts_with("POST /prefix/api/v1/user/2fa/verify/ "))
    );
    let info = engine.dispatch(owner, "archery.server.save", json!({"auth_id":auth["auth_id"],"server":fixture.server,"expected_revision":0,"password_update":{"kind":"replace","value":"good","persist":false},"totp_update":{"kind":"replace","value":uri,"persist":false}})).await.unwrap();
    assert_eq!(info["totp_configured"], true);
    assert!(!info.to_string().contains(uri.as_str()));
    assert_eq!(
        engine
            .store
            .archery_totp_secret(fixture.server.id)
            .await
            .unwrap()
            .as_deref(),
        Some(uri.as_str())
    );
    engine
        .dispatch(
            owner,
            "archery.auth.close",
            json!({"auth_id":auth["auth_id"]}),
        )
        .await
        .unwrap();
    engine
        .dispatch(
            owner,
            "archery.server.delete",
            json!({"server_id":fixture.server.id,"expected_revision":1,"expected_profile_ids":[]}),
        )
        .await
        .unwrap();
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

async fn ipc_reply(peer: &mut tokio::io::DuplexStream, id: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let message = protocol::read(&mut *peer).await.unwrap();
            if message["id"] == id {
                return message;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn oversized_resource_rpc_keeps_ipc_connection_usable() {
    let (engine, root, owner, fixture, auth) = setup().await;
    let item = profile(fixture.server.id, 7, "one");
    save(&engine, owner, &fixture, &auth, vec![item.clone()])
        .await
        .unwrap();
    let (mut peer, stream) = tokio::io::duplex(4096);
    let handler = tokio::spawn(client(engine.clone(), Uuid::new_v4(), Box::new(stream)));
    protocol::write(
        &mut peer,
        &json!({"id":"open","method":"session.open","params":{"protocol_version":3,"client_kind":"tsp","workspace":"default","allow_connections":[]}}),
    )
    .await
    .unwrap();
    assert_eq!(
        ipc_reply(&mut peer, "open").await["result"]["protocol_version"],
        3
    );
    protocol::write(&mut peer, &json!({"id":"auth","method":"archery.auth.begin","params":{"server_id":fixture.server.id,"expected_revision":1,"operation_id":Uuid::new_v4()}})).await.unwrap();
    let auth_reply = ipc_reply(&mut peer, "auth").await;
    assert_eq!(auth_reply["result"]["state"], "ready", "{auth_reply}");
    protocol::write(&mut peer, &json!({"id":"connect","method":"connection.open","params":{"profile_id":item.id,"database":"db_a"}})).await.unwrap();
    let connect = ipc_reply(&mut peer, "connect").await;
    let session = connect["result"]["session_id"].clone();
    assert!(session.is_string(), "{connect}");
    fixture.large_resource.store(true, Ordering::Release);
    protocol::write(&mut peer, &json!({"id":"resources","method":"resource.list","params":{"session_id":session,"database":"db_a"}})).await.unwrap();
    assert_eq!(
        ipc_reply(&mut peer, "resources").await["error"]["code"],
        "response_too_large"
    );
    protocol::write(
        &mut peer,
        &json!({"id":"settings","method":"settings.get","params":{}}),
    )
    .await
    .unwrap();
    assert!(ipc_reply(&mut peer, "settings").await["result"].is_object());
    fixture.large_resource.store(false, Ordering::Release);
    fixture.large_ddl.store(true, Ordering::Release);
    protocol::write(&mut peer, &json!({"id":"ddl","method":"resource.describe","params":{"session_id":session,"target":{"database":"db_a","name":"db_a","kind":"table"}}})).await.unwrap();
    let ddl_reply = ipc_reply(&mut peer, "ddl").await;
    assert_eq!(
        ddl_reply["error"]["code"], "response_too_large",
        "{ddl_reply}"
    );
    protocol::write(
        &mut peer,
        &json!({"id":"after-ddl","method":"settings.get","params":{}}),
    )
    .await
    .unwrap();
    assert!(ipc_reply(&mut peer, "after-ddl").await["result"].is_object());
    protocol::write(
        &mut peer,
        &json!({"id":"close","method":"session.close","params":{}}),
    )
    .await
    .unwrap();
    assert_eq!(
        ipc_reply(&mut peer, "close").await["result"]["closed"],
        true
    );
    handler.await.unwrap();
    drop(engine);
    let _ = std::fs::remove_dir_all(root);
}
