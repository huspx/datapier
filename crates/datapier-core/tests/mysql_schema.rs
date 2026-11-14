//! 使用隔离 MySQL 验证结构计划的真实执行和服务端读回。
use datapier_core::{
    ExecutionSink, Store,
    model::{schema::*, *},
    providers::mysql::{MysqlSession, schema::build_plan},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

#[tokio::test]
#[ignore = "需要隔离的 MySQL 5.7、8.0 或 8.4 实例"]
async fn create_table_with_inherited_defaults_is_verified() {
    let root = std::env::temp_dir().join(format!("datapier-schema-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "isolated-schema".into(),
        revision: 1,
        approval_policy: ApprovalPolicy::Dangerous,
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port: std::env::var("DATAPIER_TEST_MYSQL_PORT")
                .unwrap()
                .parse()
                .unwrap(),
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            redis_db: 15,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
    let mut session = MysqlSession::connect(profile, Some(secret)).await.unwrap();
    let cancel = CancellationToken::new();
    let (server_version, sql_mode, _, _) = session.schema_context(&cancel).await.unwrap();
    let name = format!("dp_schema_{}", Uuid::new_v4().simple());
    let target = ResourceTarget {
        database: Some("datapier_fixture".into()),
        name: name.clone(),
        key_b64: None,
        kind: "table".into(),
    };
    let definition = TableDefinition {
        columns: [
            ("id", "int"),
            ("enabled", "BOOLEAN"),
            ("amount", "NUMERIC(12,2)"),
        ]
        .into_iter()
        .map(|(name, native_type)| ColumnDefinition {
            id: name.into(),
            name: name.into(),
            native_type: native_type.into(),
            nullable: false,
            default: ColumnDefault::Unspecified,
            on_update: None,
            auto_increment: false,
            generated: None,
            charset: None,
            collation: None,
            visible: true,
            comment: String::new(),
            extra: vec![],
        })
        .collect(),
        indexes: vec![],
        constraints: vec![],
        engine: "InnoDB".into(),
        charset: None,
        collation: None,
        comment: String::new(),
        options: vec![],
    };
    let plan = build_plan(&SchemaEditIntent {
        target,
        baseline: None,
        definition: Some(SchemaDefinition::Table(definition)),
        profile_revision: "1".into(),
        algorithm: None,
        lock: None,
        server_version,
        sql_mode,
    })
    .unwrap();
    let task = store
        .create_task(
            Uuid::new_v4(),
            "schema-fixture",
            "schema.execute",
            json!({}),
            false,
        )
        .await
        .unwrap();
    let sink = ExecutionSink::new(store.clone(), task.id);
    let result = session
        .schema_executor()
        .unwrap()
        .execute(json!({"plan": plan}), &sink, &cancel)
        .await
        .unwrap();
    let cleanup_task = store
        .create_task(
            Uuid::new_v4(),
            "schema-cleanup",
            "execution.run",
            json!({}),
            false,
        )
        .await
        .unwrap();
    session
        .call(
            "execution.run",
            json!({"text": format!("DROP TABLE IF EXISTS `{name}`")}),
            &ExecutionSink::new(store.clone(), cleanup_task.id),
            &cancel,
        )
        .await
        .unwrap();
    session.close().await.unwrap();
    drop(sink);
    store.pool.close().await;
    drop(store);
    tokio::fs::remove_dir_all(root).await.unwrap();
    assert_eq!(result["state"], "applied", "{result}");
    assert_eq!(result["snapshot"]["completeness"]["state"], "complete");
    assert_eq!(result["snapshot"]["definition"]["options"], json!([]));
}

#[tokio::test]
#[ignore = "需要隔离的 MySQL 5.7、8.0 或 8.4 实例"]
async fn detached_ddl_allows_transaction_end_and_close_to_release_metadata_locks() {
    let root = std::env::temp_dir().join(format!("datapier-mdl-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "mdl".into(),
        revision: 1,
        approval_policy: ApprovalPolicy::Dangerous,
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port: std::env::var("DATAPIER_TEST_MYSQL_PORT")
                .unwrap()
                .parse()
                .unwrap(),
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            redis_db: 15,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
    let mut session = MysqlSession::connect(profile.clone(), Some(secret.clone()))
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let name = format!("dp_mdl_{}", Uuid::new_v4().simple());
    let target = ResourceTarget {
        database: Some("datapier_fixture".into()),
        name: name.clone(),
        key_b64: None,
        kind: "table".into(),
    };
    let task = store
        .create_task(Uuid::new_v4(), "mdl", "execution.run", json!({}), false)
        .await
        .unwrap();
    let sink = ExecutionSink::new(store.clone(), task.id);
    session
        .call(
            "execution.run",
            json!({"text":format!("CREATE TABLE `{name}` (id INT PRIMARY KEY) ENGINE=InnoDB")}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    for (index, end) in [
        "COMMIT WORK",
        "ROLLBACK WORK",
        "BEGIN",
        "START TRANSACTION",
        "SET autocommit=1",
        "close",
    ]
    .iter()
    .enumerate()
    {
        let snapshot: SchemaSnapshot = serde_json::from_value(
            session
                .schema_describe(json!({"target":target}), &cancel)
                .await
                .unwrap(),
        )
        .unwrap();
        let mut definition = snapshot.definition.clone();
        let SchemaDefinition::Table(table) = &mut definition else {
            panic!("expected table")
        };
        table.columns.push(ColumnDefinition {
            id: format!("c{index}"),
            name: format!("c{index}"),
            native_type: "int".into(),
            nullable: true,
            default: ColumnDefault::Unspecified,
            on_update: None,
            auto_increment: false,
            generated: None,
            charset: None,
            collation: None,
            visible: true,
            comment: String::new(),
            extra: vec![],
        });
        let (server_version, sql_mode, _, _) = session.schema_context(&cancel).await.unwrap();
        let plan = build_plan(&SchemaEditIntent {
            target: target.clone(),
            baseline: Some(snapshot),
            definition: Some(definition),
            profile_revision: "1".into(),
            algorithm: None,
            lock: None,
            server_version,
            sql_mode,
        })
        .unwrap();
        if *end == "SET autocommit=1" {
            session
                .call(
                    "execution.run",
                    json!({"text":"SET autocommit=0"}),
                    &sink,
                    &cancel,
                )
                .await
                .unwrap();
        }
        session.call("execution.run", json!({"text":format!("BEGIN; INSERT INTO `{name}` (id) VALUES ({}); SELECT * FROM `{name}`", index + 1)}), &sink, &cancel).await.unwrap();
        let executor = session.schema_executor().unwrap();
        let ddl_cancel = CancellationToken::new();
        let task = store
            .create_task(Uuid::new_v4(), "mdl", "schema.execute", json!({}), false)
            .await
            .unwrap();
        let ddl_id = task.id;
        let ddl_sink = ExecutionSink::new(store.clone(), task.id);
        let ddl_token = ddl_cancel.clone();
        let ddl = tokio::spawn(async move {
            executor
                .execute(json!({"plan":plan}), &ddl_sink, &ddl_token)
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !store.task(ddl_id).await.unwrap().sent {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("DDL was not sent");
        assert!(
            !ddl.is_finished(),
            "DDL must wait for transaction metadata lock"
        );
        ddl_cancel.cancel();
        if *end == "close" {
            session.close().await.unwrap();
        } else {
            session
                .call("execution.run", json!({"text":end}), &sink, &cancel)
                .await
                .unwrap();
        }
        let result = tokio::time::timeout(std::time::Duration::from_secs(10), ddl)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(result["state"], "applied", "{result}");
    }
    let mut cleanup = MysqlSession::connect(profile, Some(secret)).await.unwrap();
    let selected = cleanup
        .call(
            "execution.run",
            json!({"text":format!("SELECT id FROM `{name}` ORDER BY id")}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    let result_id: Uuid = serde_json::from_value(selected["results"][0].clone()).unwrap();
    assert_eq!(
        store
            .result_page(result_id, 0, 10, 1024 * 1024)
            .await
            .unwrap()
            .rows,
        vec![
            vec![Value::Integer("1".into())],
            vec![Value::Integer("3".into())],
            vec![Value::Integer("4".into())],
            vec![Value::Integer("5".into())]
        ]
    );
    cleanup
        .call(
            "execution.run",
            json!({"text":format!("DROP TABLE `{name}`")}),
            &sink,
            &cancel,
        )
        .await
        .unwrap();
    cleanup.close().await.unwrap();
    drop(sink);
    store.pool.close().await;
    drop(store);
    tokio::fs::remove_dir_all(root).await.unwrap();
}
