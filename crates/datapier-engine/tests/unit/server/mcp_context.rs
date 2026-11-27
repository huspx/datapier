use super::*;

fn profile(name: &str, port: u16) -> Profile {
    Profile {
        id: Uuid::new_v4(),
        name: name.into(),
        revision: 1,
        approval_policy: model::ApprovalPolicy::Dangerous,
        provider: model::ProviderKind::Mysql,
        access_mode: model::AccessMode::ReadOnly,
        connection: model::ConnectionConfig::Direct(model::DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: "fixture".into(),
            database: Some("sales".into()),
            redis_db: 0,
            redis_databases_override: None,
            tls: model::TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    }
}

fn register_mcp(engine: &Engine) -> Uuid {
    let owner = Uuid::new_v4();
    assert!(engine.register_with(
        owner,
        ClientContext {
            kind: model::ClientKind::Mcp,
            workspace: "default".into(),
            allow_connections: Vec::new(),
        }
    ));
    owner
}

fn request<'a>(
    engine: &'a Arc<Engine>,
    owner: Uuid,
    method: &'a str,
    params: Value,
) -> impl Future<Output = Result<Value>> + 'a {
    Box::pin(engine.dispatch(owner, method, params))
}

async fn connections(engine: &Arc<Engine>, owner: Uuid) -> Vec<Value> {
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        engine.dispatch(owner, "mcp.list_connections", json!({})),
    )
    .await
    .expect("推荐不能等待 provider 建连")
    .unwrap();
    assert_eq!(result["has_more"], false);
    result["connections"].as_array().unwrap().clone()
}

fn by_id(items: &[Value], id: Uuid) -> &Value {
    items
        .iter()
        .find(|item| item["connection_id"] == id.to_string())
        .unwrap()
}

#[tokio::test]
async fn mcp_roots_and_environments_are_isolated_without_provider_probes() {
    let (engine, root) = engine().await;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let first = profile("z-associated-first", port);
    let second = profile("y-associated-second", port);
    let unrelated = profile("a-unassociated", port);
    for profile in [&first, &second, &unrelated] {
        engine
            .store
            .save_profile(profile, None, false)
            .await
            .unwrap();
    }

    let controller = Uuid::new_v4();
    assert!(engine.register(controller));
    let client_a = register_mcp(&engine);
    let client_b = register_mcp(&engine);
    let root_a = root.join("repo-a");
    let root_b = root.join("repo-b");
    std::fs::create_dir_all(&root_a).unwrap();
    std::fs::create_dir_all(&root_b).unwrap();
    let project_a = Uuid::new_v4();
    let project_b = Uuid::new_v4();
    for (directory, project, environment, resource) in [
        (&root_a, project_a, "dev", &first),
        (&root_b, project_b, "staging", &second),
    ] {
        engine.dispatch(controller, "local_project.save", json!({
            "expected_revision":null,
            "project":{
                "id":project,"name":environment,"description":"","default_environment_id":null,"revision":0,
                "roots":[{"path":directory.to_str().unwrap(),"identity":""}],
                "environments":[{"id":Uuid::new_v4(),"name":environment,"environment_type":environment,"description":"",
                    "resources":[{"profile_id":resource.id,"database":"sales","redis_db":null,"description":"fixture"}]}]
            }
        })).await.unwrap();
    }
    for (owner, directory, environment) in
        [(client_a, &root_a, "dev"), (client_b, &root_b, "staging")]
    {
        engine
            .dispatch(
                owner,
                "mcp.context.update",
                json!({
                    "roots":[directory.to_str().unwrap()],"source":"roots","environment":environment
                }),
            )
            .await
            .unwrap();
    }

    let a = connections(&engine, client_a).await;
    let b = connections(&engine, client_b).await;
    assert_eq!(a.len(), 3);
    assert_eq!(b.len(), 3);
    assert_eq!(a[0]["connection_id"], first.id.to_string());
    assert_eq!(b[0]["connection_id"], second.id.to_string());
    assert_eq!(
        by_id(&a, first.id)["recommendation"][0]["project_id"],
        project_a.to_string()
    );
    assert_eq!(
        by_id(&b, second.id)["recommendation"][0]["project_id"],
        project_b.to_string()
    );
    assert!(
        by_id(&a, second.id)["recommendation"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        by_id(&b, first.id)["recommendation"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        by_id(&a, unrelated.id)["recommendation"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let info = engine
        .dispatch(client_a, "mcp.get_server_info", json!({}))
        .await
        .unwrap();
    assert_eq!(
        info["project_context"]["roots"][0]["project_id"],
        project_a.to_string()
    );
    assert_eq!(
        info["project_context"]["roots"][0]["environments"][0]["name"],
        "dev"
    );
    let global_info = request(
        &engine,
        client_a,
        "mcp.get_server_info",
        json!({"context_mode":"global"}),
    )
    .await
    .unwrap();
    assert!(global_info["project_context"].is_null());
    let global = request(
        &engine,
        client_a,
        "mcp.list_connections",
        json!({"context_mode":"global"}),
    )
    .await
    .unwrap();
    assert!(
        global["connections"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["recommendation"].as_array().unwrap().is_empty())
    );
    let still_scoped = request(&engine, client_a, "mcp.get_server_info", json!({}))
        .await
        .unwrap();
    assert_eq!(
        still_scoped["project_context"]["roots"][0]["project_id"],
        project_a.to_string()
    );

    request(
        &engine,
        client_a,
        "mcp.context.update",
        json!({"roots":[],"source":"roots_error","environment":"dev"}),
    )
    .await
    .unwrap();
    let unavailable = request(&engine, client_a, "mcp.get_server_info", json!({}))
        .await
        .unwrap();
    assert_eq!(unavailable["project_context"]["status"], "unavailable");
    assert!(
        by_id(&connections(&engine, client_a).await, first.id)["recommendation"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    request(
        &engine,
        client_a,
        "mcp.context.update",
        json!({"roots":[root_a.to_str().unwrap()],"source":"roots","environment":"dev"}),
    )
    .await
    .unwrap();
    assert_eq!(
        by_id(&connections(&engine, client_a).await, first.id)["recommendation"][0]["project_id"],
        project_a.to_string()
    );

    engine
        .dispatch(
            client_a,
            "mcp.context.update",
            json!({
                "roots":[root_a.to_str().unwrap()],"source":"roots","environment":"missing"
            }),
        )
        .await
        .unwrap();
    let missing = engine
        .dispatch(client_a, "mcp.get_server_info", json!({}))
        .await
        .unwrap();
    assert_eq!(
        missing["project_context"]["roots"][0]["status"],
        "environment_unmatched"
    );
    assert!(
        by_id(&connections(&engine, client_a).await, first.id)["recommendation"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        by_id(&connections(&engine, client_b).await, second.id)["recommendation"][0]["project_id"],
        project_b.to_string()
    );

    engine
        .dispatch(
            client_a,
            "mcp.context.update",
            json!({
                "roots":[root_b.to_str().unwrap()],"source":"roots","environment":"staging"
            }),
        )
        .await
        .unwrap();
    let updated_a = connections(&engine, client_a).await;
    let unchanged_b = connections(&engine, client_b).await;
    assert_eq!(updated_a[0]["connection_id"], second.id.to_string());
    assert!(
        by_id(&updated_a, first.id)["recommendation"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        by_id(&unchanged_b, second.id)["recommendation"][0]["project_id"],
        project_b.to_string()
    );

    let mut changed = engine.store.profile(second.id).await.unwrap();
    if let model::ConnectionConfig::Direct(connection) = &mut changed.connection {
        connection.host = "127.0.0.2".into();
    }
    engine
        .store
        .save_profile(&changed, None, false)
        .await
        .unwrap();
    for owner in [client_a, client_b] {
        let items = connections(&engine, owner).await;
        assert_eq!(items.len(), 3);
        assert!(
            by_id(&items, second.id)["recommendation"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        assert!(
            by_id(&items, unrelated.id)["recommendation"]
                .as_array()
                .unwrap()
                .is_empty()
        );
    }
    let restricted = Uuid::new_v4();
    assert!(engine.register_with(
        restricted,
        ClientContext {
            kind: model::ClientKind::Mcp,
            workspace: "default".into(),
            allow_connections: vec![first.id],
        }
    ));
    request(
        &engine,
        restricted,
        "mcp.context.update",
        json!({"roots":[root_b.to_str().unwrap()],"source":"roots","environment":"staging"}),
    )
    .await
    .unwrap();
    let hidden = request(&engine, restricted, "mcp.get_server_info", json!({}))
        .await
        .unwrap();
    assert_eq!(
        hidden["project_context"]["roots"][0]["environments"][0]["invalid_resource_count"],
        0
    );
    let visible = request(&engine, client_b, "mcp.get_server_info", json!({}))
        .await
        .unwrap();
    assert_eq!(
        visible["project_context"]["roots"][0]["environments"][0]["invalid_resource_count"],
        1
    );
    assert!(
        matches!(&listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
        "上下文和连接发现不应连接 provider"
    );
    assert!(engine.state.lock().unwrap().databases.is_empty());
    assert!(engine.state.lock().unwrap().discovery.is_empty());
    drop(listener);
    cleanup(engine, root).await;
}

#[tokio::test]
async fn catalog_cursors_reject_changed_connection_and_database_order() {
    let (engine, root) = engine().await;
    let mut settings = engine.store.settings().await.unwrap();
    settings.limits.response_bytes = 1024;
    settings.limits.value_preview_bytes = settings.limits.value_preview_bytes.min(1024);
    engine.store.set_settings(&settings).await.unwrap();
    let owner = register_mcp(&engine);
    let mut profiles = Vec::new();
    for i in 0..12 {
        let item = profile(&format!("connection-{i}-{}", "x".repeat(100)), 3306);
        engine.store.save_profile(&item, None, false).await.unwrap();
        profiles.push(item);
    }
    let first = request(&engine, owner, "mcp.list_connections", json!({}))
        .await
        .unwrap();
    assert_eq!(first["has_more"], true);
    let cursor = first["next_cursor"].clone();
    let added = profile("new connection", 3306);
    engine
        .store
        .save_profile(&added, None, false)
        .await
        .unwrap();
    let error = request(
        &engine,
        owner,
        "mcp.list_connections",
        json!({"cursor":cursor}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "expired");

    let id = profiles[0].id;
    let databases: Vec<String> = (0..30)
        .map(|i| format!("db-{i:02}-{}", "x".repeat(70)))
        .collect();
    let discovery = |names: &[String]| json!({"state":"known","source":"cache","databases":names,"configured_count":null,"reason":null});
    engine
        .state
        .lock()
        .unwrap()
        .discovery
        .insert(id, (profiles[0].revision, discovery(&databases)));
    let page = request(
        &engine,
        owner,
        "mcp.list_databases",
        json!({"connection_id":id}),
    )
    .await
    .unwrap();
    assert_eq!(page["has_more"], true);
    let mut changed = databases;
    changed.remove(0);
    changed.push(format!("db-99-{}", "x".repeat(70)));
    engine
        .state
        .lock()
        .unwrap()
        .discovery
        .insert(id, (profiles[0].revision, discovery(&changed)));
    let error = request(
        &engine,
        owner,
        "mcp.list_databases",
        json!({"connection_id":id,"cursor":page["next_cursor"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, "expired");
    cleanup(engine, root).await;
}
