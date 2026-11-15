use super::*;
use datapier_core::{
    model::project::{ProjectConnection, ProjectQuery},
    project,
};
use std::{collections::BTreeMap, fs};

#[tokio::test]
async fn query_resolution_requires_confirmed_directory_and_preserves_text() {
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    assert!(engine.register(owner));
    let direct = Profile {
        id: Uuid::new_v4(),
        name: "direct".into(),
        revision: 1,
        approval_policy: Default::default(),
        provider: model::ProviderKind::Mysql,
        access_mode: AccessMode::ReadOnly,
        connection: model::ConnectionConfig::Direct(model::DirectConnection {
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
    let server_id = Uuid::new_v4();
    let server = model::ArcheryServer {
        id: server_id,
        name: "archery".into(),
        base_url: "http://127.0.0.1:8080/".into(),
        username: "test".into(),
        http_tls: Default::default(),
        totp_mode: model::TotpMode::Manual,
        revision: 0,
    };
    engine
        .store
        .save_archery_server(
            &server,
            0,
            model::CredentialUpdate::Keep,
            model::CredentialUpdate::Keep,
        )
        .await
        .unwrap();
    let archery = Profile {
        id: Uuid::new_v4(),
        name: "archery-query".into(),
        revision: 1,
        approval_policy: Default::default(),
        provider: model::ProviderKind::Mysql,
        access_mode: AccessMode::ReadOnly,
        connection: model::ConnectionConfig::Archery(model::ArcheryConnection {
            server_id,
            instance_id: 7,
            instance_name: "fixture".into(),
            databases: vec!["db_a".into()],
            default_database: "db_a".into(),
        }),
    };
    engine
        .store
        .apply_archery_profiles(server_id, 1, &[], std::slice::from_ref(&archery))
        .await
        .unwrap();

    let project_path = root.join("project");
    let staging = root.join("fixture-stages");
    fs::create_dir(&staging).unwrap();
    let initial = project::create(&project_path, "query-test".into()).unwrap();
    let mut manifest = initial.manifest.clone();
    for (alias, source_kind) in [("local", "direct"), ("remote", "archery")] {
        manifest.connections.push(ProjectConnection {
            alias: alias.into(),
            provider: "mysql".into(),
            source_kind: source_kind.into(),
            database: None,
            description: String::new(),
        });
    }
    let local_query = Uuid::new_v4();
    let remote_query = Uuid::new_v4();
    for (id, alias, path) in [
        (local_query, "local", "queries/local.sql"),
        (remote_query, "remote", "queries/remote.sql"),
    ] {
        manifest.queries.push(ProjectQuery {
            id,
            name: alias.into(),
            path: path.into(),
            language: "sql".into(),
            connection_alias: alias.into(),
            database: None,
        });
    }
    let files = BTreeMap::from([
        ("queries/local.sql".into(), "SELECT 'local draft';".into()),
        ("queries/remote.sql".into(), "SELECT 'remote draft';".into()),
    ]);
    let preview = project::preview(&initial, manifest, files).unwrap();
    let stage = project::prepare_save(&preview, &preview.digest, &staging).unwrap();
    project::commit(&staging, stage.identity.id).unwrap();
    let saved = project::open(&project_path).unwrap();
    let archive = root.join("project.zip");
    project::export_archive(&saved, &archive).unwrap();
    let copy_path = root.join("project-copy");
    let import = project::prepare_import(&archive, &copy_path, &staging).unwrap();
    project::commit_import(&staging, import.id).unwrap();

    let opened = engine
        .dispatch(owner, "project.open", json!({"path":project_path}))
        .await
        .unwrap();
    let id = opened["project_id"].clone();
    let root_path = opened["root"].clone();
    engine.dispatch(owner, "project.bindings.save", json!({"project_id":id,"binding_group":"default","bindings":[
        {"connection_alias":"local","profile_id":direct.id,"database":"local_db","revision":0},
        {"connection_alias":"remote","profile_id":archery.id,"database":"db_a","revision":0}
    ]})).await.unwrap();
    let resolve = |query_id, expected_root: Value| json!({"project_id":id,"query_id":query_id,"binding_group":"default","expected_root":expected_root});
    assert_eq!(
        engine
            .dispatch(
                owner,
                "project.query.resolve",
                resolve(local_query, root_path.clone())
            )
            .await
            .unwrap_err()
            .code,
        "binding_required"
    );
    let bindings = engine
        .dispatch(
            owner,
            "project.bindings.get",
            json!({"project_id":id,"binding_group":"default"}),
        )
        .await
        .unwrap();
    assert_eq!(bindings["confirmed"], false);
    engine.dispatch(owner, "project.bindings.confirm", json!({"project_id":id,"binding_group":"default","binding_digest":bindings["binding_digest"]})).await.unwrap();
    let local = engine
        .dispatch(
            owner,
            "project.query.resolve",
            resolve(local_query, root_path.clone()),
        )
        .await
        .unwrap();
    assert_eq!(local["profile_id"], direct.id.to_string());
    assert_eq!(local["database"], "local_db");
    assert_eq!(local["query"]["id"], local_query.to_string());
    assert!(local.get("text").is_none());
    assert_eq!(
        fs::read_to_string(project_path.join("queries/local.sql")).unwrap(),
        "SELECT 'local draft';"
    );
    let remote = engine
        .dispatch(
            owner,
            "project.query.resolve",
            resolve(remote_query, root_path.clone()),
        )
        .await
        .unwrap();
    assert_eq!(remote["profile_id"], archery.id.to_string());
    assert_eq!(remote["database"], "db_a");
    assert_eq!(
        engine
            .dispatch(
                owner,
                "project.query.resolve",
                resolve(local_query, json!("/different"))
            )
            .await
            .unwrap_err()
            .code,
        "project_conflict"
    );

    let copy_owner = Uuid::new_v4();
    assert!(engine.register(copy_owner));
    let copy = engine
        .dispatch(copy_owner, "project.open", json!({"path":copy_path}))
        .await
        .unwrap();
    assert_eq!(copy["project_id"], id);
    assert_eq!(
        engine
            .dispatch(
                copy_owner,
                "project.query.resolve",
                resolve(local_query, copy["root"].clone())
            )
            .await
            .unwrap_err()
            .code,
        "binding_required"
    );
    let restore_owner = Uuid::new_v4();
    assert!(engine.register(restore_owner));
    let restored = engine.dispatch(restore_owner, "project.open", json!({"path":project_path,"baseline_manifest":saved.manifest,"baseline_digests":saved.digests})).await.unwrap();
    assert_eq!(
        engine
            .dispatch(
                restore_owner,
                "project.query.resolve",
                resolve(local_query, restored["root"].clone())
            )
            .await
            .unwrap_err()
            .code,
        "project_conflict"
    );
    engine.store.pool.close().await;
    drop(engine);
    tokio::fs::remove_dir_all(root).await.unwrap();
}
