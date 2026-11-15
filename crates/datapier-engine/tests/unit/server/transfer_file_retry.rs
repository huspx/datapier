use super::*;
use sqlx::{
    MySqlPool,
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
};

#[test]
fn retry_selects_only_definite_item_states() {
    use super::super::transfer::selected_file_records;
    use datapier_core::model::transfer::{RecordRange, TransferStepOutcome, TransferStepState};

    let mut item: TransferStepOutcome = serde_json::from_value(json!({
        "ordinal":0, "phase":"data", "state":"outcome_unknown", "digest":"",
        "affected":0, "error_code":null, "database_code":null, "error_record":null,
        "locator":{"object":null,"input":0,"first_record":10,"last_record":15,
            "byte_start":null,"byte_end":null,"source_digest":null,"target_digest":null},
        "item_states":["applied","rolled_back","failed","not_sent","outcome_unknown","sent"]
    }))
    .unwrap();
    let states = [
        TransferStepState::Failed,
        TransferStepState::RolledBack,
        TransferStepState::NotSent,
    ];
    let mut ranges = Vec::new();
    assert!(selected_file_records(&item, &states, &mut ranges).unwrap());
    assert_eq!(
        ranges,
        vec![RecordRange {
            first: 11,
            last: 13
        }]
    );
    ranges.clear();
    item.state = TransferStepState::Sent;
    assert!(!selected_file_records(&item, &states, &mut ranges).unwrap());
    assert!(ranges.is_empty());
    item.state = TransferStepState::OutcomeUnknown;
    item.item_states.clear();
    assert!(!selected_file_records(&item, &states, &mut ranges).unwrap());
    assert!(ranges.is_empty());
}

async fn plan(engine: &Arc<Engine>, owner: Uuid, method: &str, params: Value) -> Value {
    let started = engine.dispatch(owner, method, params).await.unwrap();
    let id = started["plan_id"].clone();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let result = engine
                .dispatch(owner, "transfer.plan.read", json!({"plan_id":id}))
                .await
                .unwrap();
            if result["state"] != "preparing" {
                assert_eq!(result["state"], "ready", "{result}");
                break result;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

async fn submit(engine: &Arc<Engine>, owner: Uuid, plan: &Value) -> Uuid {
    let started = engine
        .dispatch(
            owner,
            "transfer.submit",
            json!({
                "plan_id":plan["plan_id"], "revision":plan["revision"], "digest":plan["digest"]
            }),
        )
        .await
        .unwrap();
    let id: Uuid = serde_json::from_value(started["run_id"].clone()).unwrap();
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let task = engine.store.task(id).await.unwrap();
            if !matches!(task.state.as_str(), "queued" | "running" | "cancelling")
                && !engine.state.lock().unwrap().tasks.contains_key(&id)
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    engine
        .dispatch(
            owner,
            "transfer.discard",
            json!({
                "plan_id":plan["plan_id"], "revision":plan["revision"]
            }),
        )
        .await
        .unwrap();
    id
}

async fn ids(pool: &MySqlPool, table: &str) -> Vec<i32> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM `{table}` ORDER BY id"
    )))
    .fetch_all(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_MYSQL_PORT、DATAPIER_TEST_MYSQL_SECRET 和隔离 MySQL"]
async fn mysql_multifile_csv_json_retry_preserves_commits_and_includes_unread_tail() {
    let port = std::env::var("DATAPIER_TEST_MYSQL_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
    let pool = MySqlPoolOptions::new()
        .max_connections(3)
        .connect_with(
            MySqlConnectOptions::new()
                .host("127.0.0.1")
                .port(port)
                .username("datapier")
                .password(&secret)
                .database("datapier_fixture"),
        )
        .await
        .unwrap();
    let (engine, root) = engine().await;
    let owner = Uuid::new_v4();
    engine.register_with(
        owner,
        ClientContext {
            kind: model::ClientKind::Tsp,
            workspace: "file-retry".into(),
            allow_connections: Vec::new(),
        },
    );
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "file-retry".into(),
        revision: 1,
        approval_policy: Default::default(),
        provider: model::ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: model::ConnectionConfig::Direct(model::DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            redis_db: 0,
            redis_databases_override: None,
            tls: model::TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    engine
        .store
        .save_profile(&profile, Some(secret), false)
        .await
        .unwrap();
    let table = format!("dp_retry_{}", Uuid::new_v4().simple());
    let task_engine = engine.clone();
    let task_pool = pool.clone();
    let task_table = table.clone();
    let task_root = root.clone();
    let outcome = tokio::spawn(async move {
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE `{task_table}` (id INT PRIMARY KEY, email VARCHAR(64) NOT NULL UNIQUE) ENGINE=InnoDB")))
            .execute(&task_pool).await.unwrap();
        for format in ["csv", "json"] {
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM `{task_table}`"))).execute(&task_pool).await.unwrap();
            sqlx::query(sqlx::AssertSqlSafe(format!("INSERT INTO `{task_table}` VALUES (9000,'occupied')"))).execute(&task_pool).await.unwrap();
            let files: Vec<_> = [0, 1].into_iter().map(|part| {
                let path = task_root.join(format!("retry-{format}-{part}.{format}"));
                let (start, end) = if part == 0 { (1, 1200) } else { (1201, 1210) };
                if format == "csv" {
                    let mut text = String::from("id,email\n");
                    for id in start..=end {
                        let email = if id == 650 { "occupied".to_owned() } else { format!("mail{id}") };
                        text.push_str(&format!("{id},{email}\n"));
                    }
                    std::fs::write(&path, text).unwrap();
                } else {
                    let rows: Vec<_> = (start..=end).map(|id| {
                        let email = if id == 650 { "occupied".to_owned() } else { format!("mail{id}") };
                        json!([{"type":"integer","value":id.to_string()},{"type":"text","value":email}])
                    }).collect();
                    std::fs::write(&path, serde_json::to_vec(&json!({
                        "format":"datapier", "version":1,
                        "columns":[{"name":"id","type_name":"INT"},{"name":"email","type_name":"VARCHAR"}],
                        "rows":rows
                    })).unwrap()).unwrap();
                }
                path
            }).collect();
            let target = json!({"database":"datapier_fixture","name":task_table,"kind":"table"});
            let spec = json!({"mode":"import","provider":"mysql",
                "destination":{"profile_id":profile.id,"database":"datapier_fixture"},
                "inputs":[{"path":files[0],"format":format,"target":target}, {"path":files[1],"format":format,"target":target}],
                "options":{"conflict":"insert","include_schema":false,"include_data":true}
            });
            let original = plan(&task_engine, owner, "transfer.prepare", json!({"workspace_id":"file-retry","spec":spec})).await;
            let run = submit(&task_engine, owner, &original).await;
            assert_eq!(task_engine.store.transfer_run(run).await.unwrap().state, "failed");
            let before = ids(&task_pool, &task_table).await;
            assert_eq!(before, (1..=500).chain(std::iter::once(9000)).collect::<Vec<_>>());
            let steps = task_engine.store.transfer_items(run, None, 100).await.unwrap();
            let failed = steps.iter().find(|item| item.item_states.contains(&datapier_core::model::transfer::TransferStepState::Failed)).unwrap();
            assert_eq!(failed.error_record, Some(650));
            assert!(failed.item_states.contains(&datapier_core::model::transfer::TransferStepState::RolledBack));
            assert!(failed.item_states.contains(&datapier_core::model::transfer::TransferStepState::NotSent));
            let original_second = std::fs::read(&files[1]).unwrap();
            let mut changed_second = original_second.clone();
            changed_second.push(b' ');
            std::fs::write(&files[1], changed_second).unwrap();
            let drift = task_engine.dispatch(owner, "transfer.retry.prepare", json!({
                "run_id":run,"spec":spec,"states":["failed"]
            })).await.unwrap_err();
            assert_eq!(drift.code, "conflict");
            std::fs::write(&files[1], original_second).unwrap();
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM `{task_table}` WHERE id=9000"))).execute(&task_pool).await.unwrap();
            let failed_only = plan(&task_engine, owner, "transfer.retry.prepare", json!({"run_id":run,"spec":spec,"states":["failed"]})).await;
            let failed_run = submit(&task_engine, owner, &failed_only).await;
            assert_eq!(task_engine.store.transfer_run(failed_run).await.unwrap().state, "succeeded");
            assert_eq!(ids(&task_pool, &task_table).await, (1..=500).chain(std::iter::once(650)).collect::<Vec<_>>());
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM `{task_table}` WHERE id=650"))).execute(&task_pool).await.unwrap();
            let retry = plan(&task_engine, owner, "transfer.retry.prepare", json!({"run_id":run,"spec":spec})).await;
            let retry_run = submit(&task_engine, owner, &retry).await;
            assert_eq!(task_engine.store.transfer_run(retry_run).await.unwrap().state, "succeeded");
            assert_eq!(ids(&task_pool, &task_table).await, (1..=1210).collect::<Vec<_>>());
        }
    }).await;
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TABLE IF EXISTS `{table}`"
    )))
    .execute(&pool)
    .await;
    pool.close().await;
    crate::server::tests::cleanup(engine, root).await;
    outcome.unwrap();
}
