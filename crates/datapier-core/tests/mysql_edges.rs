//! 真实 MySQL 边界回归；只连接显式配置的隔离测试实例。
use datapier_core::{ExecutionSink, Store, model::*, providers::ProviderSession};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Fixture {
    store: Store,
    profile: Profile,
    secret: Option<String>,
    db: ProviderSession,
}
impl Fixture {
    async fn open() -> Self {
        let root = std::env::temp_dir().join(format!("datapier-mysql-edges-{}", Uuid::new_v4()));
        let store = Store::open(&root).await.unwrap();
        let profile = Profile {
            id: Uuid::new_v4(),
            name: "isolated-fixture".into(),
            revision: 1,
            approval_policy: ApprovalPolicy::default(),
            provider: ProviderKind::Mysql,
            access_mode: AccessMode::ReadWrite,
            connection: ConnectionConfig::Direct(DirectConnection {
                host: "127.0.0.1".into(),
                port: std::env::var("DATAPIER_TEST_MYSQL_PORT")
                    .expect("设置隔离MySQL端口")
                    .parse()
                    .unwrap(),
                username: "datapier".into(),
                database: Some("datapier_fixture".into()),
                redis_db: 0,
                redis_databases_override: None,
                tls: TlsConfig::default(),
                visible_databases: None,
                mysql: Default::default(),
            }),
        };
        let secret = Some(std::env::var("DATAPIER_TEST_MYSQL_SECRET").expect("设置测试MySQL凭据"));
        let db = ProviderSession::connect(profile.clone(), secret.clone())
            .await
            .unwrap();
        Self {
            store,
            profile,
            secret,
            db,
        }
    }
    async fn call(&mut self, method: &str, params: Json) -> Result<Json> {
        let task = self
            .store
            .create_task(Uuid::new_v4(), "fixture", method, json!({}), false)
            .await?;
        let sink = ExecutionSink::new(self.store.clone(), task.id);
        self.db
            .call(method, params, &sink, &CancellationToken::new())
            .await
    }
    async fn sql(&mut self, text: &str) -> Result<Json> {
        self.call("execution.run", json!({"text":text})).await
    }
    async fn rows(&self, result: &Json) -> Vec<Vec<Value>> {
        let id: Uuid = serde_json::from_value(result["results"][0].clone()).unwrap();
        self.store
            .result_page(id, 0, 200, 1024 * 1024)
            .await
            .unwrap()
            .rows
    }
    async fn close(mut self) {
        self.db.close().await.unwrap();
        let root = (*self.store.root).clone();
        self.store.pool.close().await;
        drop(self.store);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn mysql_session_uses_configured_timezone_and_preserves_server_mode() {
    let mut f = Fixture::open().await;
    f.db.close().await.unwrap();
    f.profile.direct_mut().unwrap().mysql.timezone = Some("+03:00".into());
    f.db = ProviderSession::connect(f.profile.clone(), f.secret.clone())
        .await
        .unwrap();
    let result = f.sql("SELECT @@session.time_zone, @@character_set_connection, REPLACE(CONCAT(',', @@session.sql_mode, ','), ',IGNORE_SPACE,', ',') = REPLACE(CONCAT(',', @@global.sql_mode, ','), ',IGNORE_SPACE,', ',')").await.unwrap();
    let rows = f.rows(&result).await;
    assert_eq!(rows[0][0].text(), "+03:00");
    assert_eq!(rows[0][1].text(), "utf8mb4");
    assert_eq!(rows[0][2].text(), "1");

    f.db.close().await.unwrap();
    f.profile.direct_mut().unwrap().mysql.timezone = None;
    f.db = ProviderSession::connect(f.profile.clone(), f.secret.clone())
        .await
        .unwrap();
    let result = f
        .sql("SELECT @@session.time_zone = @@global.time_zone")
        .await
        .unwrap();
    assert_eq!(f.rows(&result).await[0][0].text(), "1");
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn task_database_follows_use_instead_of_cached_prepare_context() {
    let mut f = Fixture::open().await;
    f.sql("SELECT 1").await.unwrap();
    assert!(
        f.call(
            "resource.describe",
            json!({"target":{"name":"no_such_table_for_context","kind":"table"}})
        )
        .await
        .is_err()
    );
    f.sql("USE information_schema").await.unwrap();
    let described = f
        .call(
            "resource.describe",
            json!({"target":{"name":"TABLES","kind":"table"}}),
        )
        .await
        .unwrap();
    assert_eq!(described["target"]["database"], "information_schema");
    let task = f.store.create_task(Uuid::new_v4(), "fixture", "execution.run", json!({
        "_task_context":{"connection":"isolated-fixture","provider":"mysql","database":"datapier_fixture","redis_db":null,"targets":[]}
    }), false).await.unwrap();
    let result =
        f.db.call(
            "execution.run",
            json!({"text":"SELECT DATABASE() AS active_database"}),
            &ExecutionSink::new(f.store.clone(), task.id),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    let recorded = f.store.task(task.id).await.unwrap();
    assert_eq!(
        recorded.context.unwrap().database.as_deref(),
        Some("information_schema")
    );
    assert_eq!(
        f.rows(&result).await[0][0],
        Value::Text("information_schema".into())
    );
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn edit_token_keeps_database_selected_when_opened() {
    let mut f = Fixture::open().await;
    let table = name("dp_token_db");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    let opened = f
        .call(
            "change.open",
            json!({"target":{"name":table,"kind":"table"}}),
        )
        .await
        .unwrap();
    assert_eq!(opened["schema"]["target"]["database"], "datapier_fixture");
    f.sql("USE information_schema").await.unwrap();
    let token: Uuid = serde_json::from_value(opened["token"].clone()).unwrap();
    let ProviderSession::Mysql(mysql) = &f.db else {
        panic!("expected MySQL")
    };
    assert!(
        mysql
            .validate_draft_scope(token, "datapier_fixture", &table)
            .is_ok()
    );
    assert!(
        mysql
            .validate_draft_scope(token, "information_schema", &table)
            .is_err()
    );
    let change = RowChange {
        operation: "insert".into(),
        key: BTreeMap::new(),
        before: BTreeMap::new(),
        values: [("id".into(), Value::Integer("1".into()))]
            .into_iter()
            .collect(),
    };
    f.call("change.submit", json!({"token":token,"rows":[change]}))
        .await
        .unwrap();
    let result = f
        .sql(&format!("SELECT id FROM datapier_fixture.`{table}`"))
        .await
        .unwrap();
    assert_eq!(
        f.rows(&result).await,
        vec![vec![Value::Integer("1".into())]]
    );
    f.sql(&format!("DROP TABLE datapier_fixture.`{table}`"))
        .await
        .unwrap();
    f.close().await;
}
fn name(prefix: &str) -> String {
    format!("{prefix}_{}", Uuid::new_v4().simple())
}
fn target(name: &str) -> ResourceTarget {
    ResourceTarget {
        database: Some("datapier_fixture".into()),
        name: name.into(),
        key_b64: None,
        kind: "table".into(),
    }
}
async fn describe(fixture: &mut Fixture, target: &ResourceTarget) -> TableSchema {
    serde_json::from_value(
        fixture
            .call("resource.describe", json!({"target":target}))
            .await
            .unwrap(),
    )
    .unwrap()
}
async fn before(
    fixture: &mut Fixture,
    table: &str,
    schema: &TableSchema,
) -> BTreeMap<String, Value> {
    let result = fixture
        .sql(&format!("SELECT * FROM `{table}` WHERE id=1"))
        .await
        .unwrap();
    let row = fixture.rows(&result).await.remove(0);
    schema
        .columns
        .iter()
        .zip(row)
        .map(|(column, value)| (column.name.clone(), value))
        .collect()
}
async fn submit(
    fixture: &mut Fixture,
    target: &ResourceTarget,
    change: &RowChange,
) -> Result<Json> {
    let opened = fixture
        .call("change.open", json!({"target":target}))
        .await?;
    fixture
        .call(
            "change.submit",
            json!({"target":target,"token":opened["token"],"rows":[change]}),
        )
        .await
}
fn update(before: BTreeMap<String, Value>, column: &str, value: Value) -> RowChange {
    RowChange {
        operation: "update".into(),
        key: [("id".into(), before["id"].clone())].into_iter().collect(),
        before,
        values: [(column.into(), value)].into_iter().collect(),
    }
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 8.0.23 或更高版本"]
async fn browse_includes_invisible_columns_with_comments_and_values() {
    let mut f = Fixture::open().await;
    let table = name("dp_invisible");
    let outcome = async {
        f.sql(&format!(
            "CREATE TABLE `{table}` (id INT NOT NULL PRIMARY KEY COMMENT '主键', secret VARCHAR(40) INVISIBLE COMMENT '隐藏列注释', visible VARCHAR(40) NOT NULL COMMENT '普通列注释') ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
        ))
        .await?;
        f.sql(&format!(
            "INSERT INTO `{table}` (id, secret, visible) VALUES (1, 'hidden value', 'visible value')"
        ))
        .await?;

        let browse = f
            .call("resource.browse", json!({"target":target(&table)}))
            .await?;
        let browse_id: Uuid = serde_json::from_value(browse["results"][0].clone())?;
        let browse_meta = f.store.result_meta(browse_id).await?;
        let browse_rows = f.store.result_page(browse_id, 0, 200, 1024 * 1024).await?.rows;

        let select = f.sql(&format!("SELECT * FROM `{table}`")).await?;
        let select_id: Uuid = serde_json::from_value(select["results"][0].clone())?;
        let select_meta = f.store.result_meta(select_id).await?;
        let select_rows = f.store.result_page(select_id, 0, 200, 1024 * 1024).await?.rows;
        Ok::<_, AppError>((browse_meta, browse_rows, select_meta, select_rows))
    }
    .await;
    let cleanup = f.sql(&format!("DROP TABLE IF EXISTS `{table}`")).await;
    f.close().await;
    cleanup.unwrap();
    let (browse_meta, browse_rows, select_meta, select_rows) = outcome.unwrap();

    assert_eq!(
        browse_meta
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "secret", "visible"]
    );
    assert_eq!(
        browse_meta
            .columns
            .iter()
            .map(|c| c.comment.as_deref())
            .collect::<Vec<_>>(),
        [Some("主键"), Some("隐藏列注释"), Some("普通列注释")]
    );
    assert_eq!(
        browse_rows,
        vec![vec![
            Value::Integer("1".into()),
            Value::Text("hidden value".into()),
            Value::Text("visible value".into())
        ]]
    );
    assert_eq!(
        select_meta
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>(),
        ["id", "visible"]
    );
    assert_eq!(
        select_rows,
        vec![vec![
            Value::Integer("1".into()),
            Value::Text("visible value".into())
        ]]
    );
}

#[tokio::test]
#[ignore = "需要隔离 MySQL、CREATE ROUTINE 和 CREATE VIEW 权限"]
async fn readonly_view_cannot_call_writing_function() {
    let mut f = Fixture::open().await;
    let table = name("dp_guard");
    let function = name("dp_fn");
    let view = name("dp_view");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT NOT NULL) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    f.sql(&format!("CREATE FUNCTION `{function}`() RETURNS INT MODIFIES SQL DATA SQL SECURITY DEFINER BEGIN INSERT INTO `{table}` VALUES (1); RETURN 1; END")).await.unwrap();
    f.sql(&format!(
        "CREATE VIEW `{view}` AS SELECT `{function}`() AS value"
    ))
    .await
    .unwrap();
    let mut readonly = f.profile.clone();
    readonly.access_mode = AccessMode::ReadOnly;
    let mut reader = ProviderSession::connect(readonly, f.secret.clone())
        .await
        .unwrap();
    let task = f
        .store
        .create_task(
            Uuid::new_v4(),
            "fixture",
            "resource.browse",
            json!({}),
            false,
        )
        .await
        .unwrap();
    let result = reader
        .call(
            "resource.browse",
            json!({"target":target(&view)}),
            &ExecutionSink::new(f.store.clone(), task.id),
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_err(), "只读连接必须拒绝视图函数写入");
    reader.close().await.unwrap();
    let result = f
        .sql(&format!("SELECT COUNT(*) FROM `{table}`"))
        .await
        .unwrap();
    assert_eq!(f.rows(&result).await[0][0].text(), "0");
    f.sql(&format!("DROP VIEW `{view}`")).await.unwrap();
    f.sql(&format!("DROP FUNCTION `{function}`")).await.unwrap();
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL、CREATE ROUTINE 和 CREATE VIEW 权限"]
async fn indirect_writing_view_is_never_replayed_for_sql_or_browse() {
    let mut f = Fixture::open().await;
    let table = name("dp_effects");
    let function = name("dp_effect_fn");
    let inner = name("dp_effect_inner");
    let outer = name("dp_effect_outer");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT NOT NULL) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    f.sql(&format!("CREATE FUNCTION `{function}`() RETURNS INT MODIFIES SQL DATA SQL SECURITY DEFINER BEGIN INSERT INTO `{table}` VALUES (1); RETURN 1; END")).await.unwrap();
    f.sql(&format!(
        "CREATE VIEW `{inner}` AS SELECT `{function}`() AS value"
    ))
    .await
    .unwrap();
    f.sql(&format!(
        "CREATE VIEW `{outer}` AS SELECT value FROM `{inner}`"
    ))
    .await
    .unwrap();
    for (method, params) in [
        (
            "execution.run",
            json!({"text":format!("SELECT value FROM `{outer}`"), "page_size":1}),
        ),
        (
            "resource.browse",
            json!({"target":target(&outer), "page_size":1}),
        ),
    ] {
        f.sql(&format!("DELETE FROM `{table}`")).await.unwrap();
        let result = f.call(method, params).await.unwrap();
        let id: Uuid = serde_json::from_value(result["results"][0].clone()).unwrap();
        assert_eq!(
            f.store.result_meta(id).await.unwrap().summary["retrieval_mode"],
            "single_execution_stream"
        );
        let _ = f.store.result_page(id, 0, 10, 1024 * 1024).await.unwrap();
        let count = f
            .sql(&format!("SELECT COUNT(*) FROM `{table}`"))
            .await
            .unwrap();
        assert_eq!(f.rows(&count).await[0][0].text(), "1");
    }
    f.sql(&format!("DROP VIEW `{outer}`")).await.unwrap();
    f.sql(&format!("DROP VIEW `{inner}`")).await.unwrap();
    f.sql(&format!("DROP FUNCTION `{function}`")).await.unwrap();
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 和 LOCK TABLES 权限"]
async fn cancelled_live_queries_and_counts_preserve_transaction() {
    let mut f = Fixture::open().await;
    let mut blocker = Fixture::open().await;
    let table = name("dp_live_lock");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    f.sql(&format!("INSERT INTO `{table}` VALUES (1)"))
        .await
        .unwrap();
    let initial = f.sql(&format!("SELECT id FROM `{table}`")).await.unwrap();
    let id: Uuid = serde_json::from_value(initial["results"][0].clone()).unwrap();
    f.sql("CREATE TEMPORARY TABLE dp_cancel_tx (id INT)")
        .await
        .unwrap();
    f.call("transaction.begin", json!({})).await.unwrap();
    f.sql("INSERT INTO dp_cancel_tx VALUES (7)").await.unwrap();
    blocker
        .sql(&format!("LOCK TABLES `{table}` WRITE"))
        .await
        .unwrap();
    for (method, params) in [
        ("result.count", json!({"result_id":id})),
        (
            "execution.run",
            json!({"text":format!("SELECT id FROM `{table}`")}),
        ),
    ] {
        let task = f
            .store
            .create_task(Uuid::new_v4(), "fixture", method, json!({}), false)
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let signal = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            signal.cancel();
        });
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(8),
            f.db.call(
                method,
                params,
                &ExecutionSink::new(f.store.clone(), task.id),
                &cancel,
            ),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(error.code, "cancelled");
        assert_eq!(
            f.call("connection.info", json!({})).await.unwrap()["transaction"],
            true
        );
        let result = f.sql("SELECT id FROM dp_cancel_tx").await.unwrap();
        assert_eq!(f.rows(&result).await[0][0].text(), "7");
    }
    blocker.sql("UNLOCK TABLES").await.unwrap();
    f.call("transaction.rollback", json!({})).await.unwrap();
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    blocker.close().await;
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn decimal_write_rejects_rounding_but_accepts_trailing_zero() {
    let mut f = Fixture::open().await;
    let table = name("dp_decimal");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY, amount DECIMAL(5,2) NOT NULL) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    f.sql(&format!("INSERT INTO `{table}` VALUES (1, 1.00)"))
        .await
        .unwrap();
    let target = target(&table);
    let schema = describe(&mut f, &target).await;
    let baseline = before(&mut f, &table, &schema).await;
    let invalid = update(baseline.clone(), "amount", Value::Decimal("1.234".into()));
    assert!(submit(&mut f, &target, &invalid).await.is_err());
    let valid = update(baseline, "amount", Value::Decimal("1.2300".into()));
    assert_eq!(
        submit(&mut f, &target, &valid).await.unwrap()["items"][0]["status"],
        "applied"
    );
    let actual = f
        .sql(&format!("SELECT amount FROM `{table}` WHERE id=1"))
        .await
        .unwrap();
    let rows = f.rows(&actual).await;
    let Value::Decimal(value) = &rows[0][0] else {
        panic!("DECIMAL 类型丢失")
    };
    assert_eq!(
        value.parse::<bigdecimal::BigDecimal>().unwrap(),
        "1.23".parse::<bigdecimal::BigDecimal>().unwrap()
    );
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 和 CREATE ROUTINE 权限"]
async fn definer_program_and_commented_call_keep_empty_dynamic_boundary() {
    let mut f = Fixture::open().await;
    let procedure = name("dp_proc");
    let create = format!(
        "/* header */ CREATE\nDEFINER=CURRENT_USER\nPROCEDURE `{procedure}`()\nBEGIN\n SELECT 1 AS empty_value FROM DUAL WHERE FALSE;\n SELECT 2 AS value;\nEND"
    );
    f.sql(&create).await.unwrap();
    let call = f
        .sql(&format!("/* report */ CALL `{procedure}`()"))
        .await
        .unwrap();
    let ids: Vec<Uuid> = serde_json::from_value(call["results"].clone()).unwrap();
    let mut unknown = false;
    let mut value_seen = false;
    for id in ids {
        let page = f.store.result_page(id, 0, 200, 1024 * 1024).await.unwrap();
        if page.result.kind == "columns_unknown" {
            assert_eq!(page.result.summary["columns_known"], false);
            unknown = true;
        }
        if page
            .rows
            .iter()
            .any(|row| row.first().is_some_and(|value| value.text() == "2"))
        {
            value_seen = true;
        }
    }
    assert!(unknown, "空动态结果必须明确列定义未知");
    assert!(value_seen, "后续结果必须保留");
    f.sql(&format!("DROP PROCEDURE `{procedure}`"))
        .await
        .unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn failed_script_reports_preceding_write_and_unexecuted_tail() {
    let mut f = Fixture::open().await;
    let table = name("dp_script");
    let missing = name("dp_missing");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    let sql = format!(
        "INSERT INTO `{table}` VALUES (1); SELECT * FROM `{missing}`; INSERT INTO `{table}` VALUES (2)"
    );
    let error = f.sql(&sql).await.unwrap_err();
    assert_eq!(error.details["statements"][0]["status"], "executed");
    assert_eq!(error.details["statements"][1]["status"], "failed");
    assert_eq!(error.details["affected_rows"], 1);
    assert!(
        error.details["unexecuted_sql"]
            .as_str()
            .unwrap()
            .contains("VALUES (2)")
    );
    assert!(error.details.get("transaction_state").is_some());
    let result = f
        .sql(&format!("SELECT id FROM `{table}` ORDER BY id"))
        .await
        .unwrap();
    let rows = f.rows(&result).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0].text(), "1");
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 且启用 MyISAM"]
async fn myisam_cas_detects_case_and_trailing_spaces_and_accepts_same_value() {
    let mut f = Fixture::open().await;
    let table = name("dp_myisam");
    f.sql(&format!("CREATE TABLE `{table}` (id INT PRIMARY KEY, label VARCHAR(40) COLLATE utf8mb4_general_ci NOT NULL) ENGINE=MyISAM DEFAULT CHARSET=utf8mb4")).await.unwrap();
    f.sql(&format!("INSERT INTO `{table}` VALUES (1, 'alpha')"))
        .await
        .unwrap();
    let target = target(&table);
    let schema = describe(&mut f, &target).await;
    assert_eq!(schema.engine.to_ascii_lowercase(), "myisam");
    let baseline = before(&mut f, &table, &schema).await;
    let same = update(baseline.clone(), "label", Value::Text("alpha".into()));
    let unchanged = submit(&mut f, &target, &same).await.unwrap();
    assert_eq!(unchanged["affected_rows"], 0);
    assert_eq!(before(&mut f, &table, &schema).await, baseline);
    f.sql(&format!("UPDATE `{table}` SET label='ALPHA' WHERE id=1"))
        .await
        .unwrap();
    let stale = update(baseline, "label", Value::Text("new".into()));
    assert_eq!(
        submit(&mut f, &target, &stale).await.unwrap_err().code,
        "conflict"
    );
    let baseline = before(&mut f, &table, &schema).await;
    f.sql(&format!("UPDATE `{table}` SET label='ALPHA ' WHERE id=1"))
        .await
        .unwrap();
    let stale = update(baseline, "label", Value::Text("new".into()));
    assert_eq!(
        submit(&mut f, &target, &stale).await.unwrap_err().code,
        "conflict"
    );
    let actual = f
        .sql(&format!("SELECT label FROM `{table}` WHERE id=1"))
        .await
        .unwrap();
    assert_eq!(f.rows(&actual).await[0][0], Value::Text("ALPHA ".into()));
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn enum_and_set_survive_comment_only_structure_edit() {
    use datapier_core::{
        model::schema::{SchemaDefinition, SchemaEditIntent, SchemaSnapshot},
        providers::mysql::schema::build_plan,
    };
    let mut f = Fixture::open().await;
    let table = name("dp_enum_type");
    let result = f.sql("SELECT @@session.sql_mode").await.unwrap();
    let mode = f.rows(&result).await[0][0].text();
    let member = if mode
        .split(',')
        .any(|m| m.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"))
    {
        r"a\b"
    } else {
        r"a\\b"
    };
    f.sql(&format!("CREATE TABLE `{table}` (id INT PRIMARY KEY, e ENUM('{member}'), s SET('{member}')) ENGINE=InnoDB")).await.unwrap();
    f.sql(&format!("INSERT INTO `{table}` VALUES (1, 1, 1)"))
        .await
        .unwrap();
    let target = target(&table);
    let snapshot: SchemaSnapshot = serde_json::from_value(
        f.call("schema.describe", json!({"target":target}))
            .await
            .unwrap(),
    )
    .unwrap();
    let mut definition = snapshot.definition.clone();
    let SchemaDefinition::Table(table_definition) = &mut definition else {
        panic!("expected table")
    };
    table_definition
        .columns
        .iter_mut()
        .find(|c| c.name == "e")
        .unwrap()
        .comment = "changed".into();
    let plan = build_plan(&SchemaEditIntent {
        target: target.clone(),
        server_version: snapshot.server_version.clone(),
        sql_mode: snapshot.sql_mode.clone(),
        baseline: Some(snapshot),
        definition: Some(definition),
        profile_revision: "1".into(),
        algorithm: None,
        lock: None,
    })
    .unwrap();
    let outcome = f
        .call("schema.execute", json!({"plan":plan}))
        .await
        .unwrap();
    assert_eq!(outcome["state"], "applied", "{outcome}");
    let result = f
        .sql(&format!("SELECT HEX(e), HEX(s), e+0, s+0 FROM `{table}`"))
        .await
        .unwrap();
    let rows = f.rows(&result).await;
    assert_eq!(rows[0][0].text(), "615C62");
    assert_eq!(rows[0][1].text(), "615C62");
    assert_eq!(rows[0][2].text(), "1");
    assert_eq!(rows[0][3].text(), "1");
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 且启用 MyISAM"]
async fn latin1_cas_uses_utf8_baselines_for_both_engines() {
    let mut f = Fixture::open().await;
    for engine in ["InnoDB", "MyISAM"] {
        let table = name("dp_latin1");
        f.sql(&format!("CREATE TABLE `{table}` (id INT PRIMARY KEY, label VARCHAR(40), n INT) ENGINE={engine} DEFAULT CHARSET=latin1")).await.unwrap();
        f.sql(&format!(
            "INSERT INTO `{table}` VALUES (1, CONVERT(X'E9' USING latin1), 1)"
        ))
        .await
        .unwrap();
        let target = target(&table);
        let schema = describe(&mut f, &target).await;
        let baseline = before(&mut f, &table, &schema).await;
        let same = update(baseline.clone(), "label", Value::Text("é".into()));
        assert_eq!(
            submit(&mut f, &target, &same).await.unwrap()["affected_rows"],
            0
        );
        let change = update(baseline, "n", Value::Integer("2".into()));
        assert_eq!(
            submit(&mut f, &target, &change).await.unwrap()["affected_rows"],
            1
        );
        f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    }
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL、MyISAM 和 TRIGGER 权限"]
async fn myisam_trigger_error_preserves_unknown_current_write() {
    let mut f = Fixture::open().await;
    let table = name("dp_myisam_error");
    let trigger = name("dp_trigger");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY) ENGINE=MyISAM"
    ))
    .await
    .unwrap();
    f.sql(&format!("CREATE TRIGGER `{trigger}` AFTER INSERT ON `{table}` FOR EACH ROW SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT='trigger failure'")).await.unwrap();
    let opened = f
        .call("change.open", json!({"target":target(&table)}))
        .await
        .unwrap();
    let change = RowChange {
        operation: "insert".into(),
        key: BTreeMap::new(),
        before: BTreeMap::new(),
        values: [("id".into(), Value::Integer("1".into()))]
            .into_iter()
            .collect(),
    };
    let error = f
        .call(
            "change.submit",
            json!({"token":opened["token"],"rows":[change]}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "outcome_unknown");
    assert_eq!(error.details["items"][0]["status"], "unknown");
    assert_eq!(error.details["original_error"]["code"], "database");
    let result = f
        .sql(&format!("SELECT COUNT(*) FROM `{table}`"))
        .await
        .unwrap();
    assert_eq!(f.rows(&result).await[0][0].text(), "1");
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 且启用 MyISAM"]
async fn table_edit_token_rejects_engine_change_before_writing() {
    let mut f = Fixture::open().await;
    let mut other = Fixture::open().await;
    let table = name("dp_changed_engine");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    let opened = f
        .call("change.open", json!({"target":target(&table)}))
        .await
        .unwrap();
    other
        .sql(&format!("ALTER TABLE `{table}` ENGINE=MyISAM"))
        .await
        .unwrap();
    let change = RowChange {
        operation: "insert".into(),
        key: BTreeMap::new(),
        before: BTreeMap::new(),
        values: [("id".into(), Value::Integer("1".into()))]
            .into_iter()
            .collect(),
    };
    let error = f
        .call(
            "change.submit",
            json!({"token":opened["token"],"rows":[change]}),
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "conflict");
    let result = other
        .sql(&format!("SELECT COUNT(*) FROM `{table}`"))
        .await
        .unwrap();
    assert_eq!(other.rows(&result).await[0][0].text(), "0");
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    other.close().await;
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4、MyISAM 和 TRIGGER 权限"]
async fn unchanged_updates_validate_visible_baseline_without_writing() {
    let mut f = Fixture::open().await;
    let mut observer = Fixture::open().await;
    let mut observations = Vec::new();
    for engine in ["InnoDB", "MyISAM"] {
        for is_view in [false, true] {
            let table = name("dp_noop");
            let audit = name("dp_noop_audit");
            let view = name("dp_noop_view");
            let trigger = name("dp_noop_trigger");
            f.sql(&format!("CREATE TABLE `{table}` (id INT PRIMARY KEY, label VARCHAR(40) COLLATE utf8mb4_general_ci NOT NULL, note VARCHAR(40) NOT NULL, enabled INT NOT NULL) ENGINE={engine} DEFAULT CHARSET=utf8mb4")).await.unwrap();
            f.sql(&format!(
                "CREATE TABLE `{audit}` (id INT NOT NULL) ENGINE={engine}"
            ))
            .await
            .unwrap();
            f.sql(&format!(
                "INSERT INTO `{table}` VALUES (1, 'alpha', 'note', 1), (2, 'beta', 'note', 1)"
            ))
            .await
            .unwrap();
            f.sql(&format!("CREATE TRIGGER `{trigger}` AFTER UPDATE ON `{table}` FOR EACH ROW INSERT INTO `{audit}` VALUES (NEW.id)")).await.unwrap();
            let mut target = target(&table);
            if is_view {
                f.sql(&format!("CREATE ALGORITHM=MERGE VIEW `{view}` AS SELECT id, label, note FROM `{table}` WHERE enabled > 0")).await.unwrap();
                target.name = view.clone();
                target.kind = "view".into();
            }
            let schema = describe(&mut f, &target).await;
            let baseline = before(&mut f, &target.name, &schema).await;
            let same = update(baseline, "label", Value::Text("alpha".into()));
            let opened = f
                .call("change.open", json!({"target":target}))
                .await
                .unwrap();
            let preview = f
                .call(
                    "change.preview",
                    json!({"token":opened["token"],"rows":[same]}),
                )
                .await
                .unwrap();
            let unchanged = f
                .call(
                    "change.submit",
                    json!({"token":opened["token"],"rows":[same]}),
                )
                .await
                .unwrap();
            let count = f
                .sql(&format!("SELECT COUNT(*) FROM `{audit}`"))
                .await
                .unwrap();
            let audit_count = f.rows(&count).await[0][0].text();
            let mut conflicts = Vec::new();
            for mutation in [
                format!("UPDATE `{table}` SET label='ALPHA' WHERE id=1"),
                format!("UPDATE `{table}` SET note='changed' WHERE id=1"),
                format!("UPDATE `{table}` SET enabled=0 WHERE id=1"),
                format!("DELETE FROM `{table}` WHERE id=1"),
            ] {
                observer
                    .sql(&format!("DELETE FROM `{table}` WHERE id=1"))
                    .await
                    .unwrap();
                observer
                    .sql(&format!(
                        "INSERT INTO `{table}` VALUES (1, 'alpha', 'note', 1)"
                    ))
                    .await
                    .unwrap();
                let opened = f
                    .call("change.open", json!({"target":target}))
                    .await
                    .unwrap();
                observer.sql(&mutation).await.unwrap();
                let result = f
                    .call(
                        "change.submit",
                        json!({"token":opened["token"],"rows":[same]}),
                    )
                    .await;
                conflicts.push(result.map(|_| "success".into()).unwrap_or_else(|e| e.code));
            }
            let second = f
                .sql(&format!("SELECT * FROM `{}` WHERE id=2", target.name))
                .await
                .unwrap();
            let second_before = schema
                .columns
                .iter()
                .zip(f.rows(&second).await.remove(0))
                .map(|(column, value)| (column.name.clone(), value))
                .collect();
            let first = update(second_before, "label", Value::Text("written".into()));
            let opened = f
                .call("change.open", json!({"target":target}))
                .await
                .unwrap();
            let batch = f
                .call(
                    "change.submit",
                    json!({"token":opened["token"],"rows":[first,same]}),
                )
                .await;
            let actual = f
                .sql(&format!("SELECT label FROM `{table}` WHERE id=2"))
                .await
                .unwrap();
            let first_value = f.rows(&actual).await[0][0].text();
            let batch_error = batch
                .err()
                .map(|e| json!({"code":e.code,"items":e.details["items"]}));
            observations.push(json!({"engine":engine,"view":is_view,"unchanged":unchanged,"audit":audit_count,"conflicts":conflicts,"batch_error":batch_error,"first_value":first_value,"preview":preview,"target":target.name}));
            if is_view {
                f.sql(&format!("DROP VIEW `{view}`")).await.unwrap();
            }
            f.sql(&format!("DROP TABLE `{table}`, `{audit}`"))
                .await
                .unwrap();
        }
    }
    observer.close().await;
    f.close().await;
    // 在所有断言前清理 UUID 资源，失败复现也不遗留表、视图或触发器。
    for observed in observations {
        assert_eq!(
            observed["conflicts"],
            json!(["conflict", "conflict", "conflict", "conflict"]),
            "{observed}"
        );
        assert_eq!(observed["unchanged"]["affected_rows"], 0);
        assert_eq!(observed["unchanged"]["items"][0]["status"], "unchanged");
        assert_eq!(observed["audit"], "0", "同值更新不能触发 UPDATE 触发器");
        assert_eq!(observed["batch_error"]["code"], "conflict");
        assert_eq!(observed["batch_error"]["items"][1]["status"], "conflict");
        let transactional = observed["engine"] == "InnoDB";
        assert_eq!(
            observed["first_value"],
            if transactional { "beta" } else { "written" }
        );
        assert_eq!(
            observed["batch_error"]["items"][0]["status"],
            if transactional {
                "rolled_back"
            } else {
                "applied"
            }
        );
        let statements = observed["preview"]["items"][0]["statements"]
            .as_array()
            .unwrap();
        assert_eq!(statements.len(), 1);
        assert_eq!(statements[0]["role"], "check");
        let text = statements[0]["text"].as_str().unwrap();
        assert!(text.starts_with("SELECT 1 FROM "));
        assert!(text.contains(&format!("`{}`", observed["target"].as_str().unwrap())));
        assert_eq!(
            statements[0]["parameters"].as_array().unwrap().len(),
            if observed["view"] == true { 3 } else { 4 }
        );
        assert_eq!(
            text.contains("FOR UPDATE"),
            transactional && observed["view"] == false
        );
    }
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn describe_and_browse_preserve_column_comments_and_order() {
    let mut f = Fixture::open().await;
    let table = name("dp_comments");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT NOT NULL COMMENT '编号', description VARCHAR(64) COMMENT '第一行\n第二行', empty_comment INT COMMENT '', PRIMARY KEY (id)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4"
    ))
    .await
    .unwrap();
    f.sql(&format!("INSERT INTO `{table}` VALUES (1, 'value', 7)"))
        .await
        .unwrap();
    let target = target(&table);
    let schema = describe(&mut f, &target).await;
    let expected = vec![
        ("id", Some("编号")),
        ("description", Some("第一行\n第二行")),
        ("empty_comment", Some("")),
    ];
    let columns = |columns: &[Column]| {
        columns
            .iter()
            .map(|column| (column.name.clone(), column.comment.clone()))
            .collect::<Vec<_>>()
    };
    let expected: Vec<_> = expected
        .into_iter()
        .map(|(name, comment)| (name.to_owned(), comment.map(str::to_owned)))
        .collect();
    assert_eq!(columns(&schema.columns), expected);

    let browsed = f
        .call("resource.browse", json!({"target":target}))
        .await
        .unwrap();
    let result_id: Uuid = serde_json::from_value(browsed["results"][0].clone()).unwrap();
    let metadata = f.store.result_meta(result_id).await.unwrap();
    assert_eq!(columns(&metadata.columns), expected);
    assert_eq!(f.rows(&browsed).await.len(), 1);

    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn composite_unique_keys_require_all_full_non_null_columns() {
    let mut f = Fixture::open().await;
    let table = name("dp_composite");
    let prefix_table = name("dp_prefix");
    f.sql(&format!("CREATE TABLE `{table}` (a INT NOT NULL, b INT NOT NULL, value VARCHAR(32) NOT NULL, UNIQUE KEY uq (a, b)) ENGINE=InnoDB")).await.unwrap();
    f.sql(&format!(
        "INSERT INTO `{table}` VALUES (1, 1, 'first'), (1, 2, 'second')"
    ))
    .await
    .unwrap();
    let composite_target = target(&table);
    let schema = describe(&mut f, &composite_target).await;
    assert_eq!(schema.keys, vec![vec!["a".to_owned(), "b".to_owned()]]);
    let selected = f
        .sql(&format!("SELECT * FROM `{table}` WHERE a=1 AND b=2"))
        .await
        .unwrap();
    let row = f.rows(&selected).await.remove(0);
    let before: BTreeMap<String, Value> = schema
        .columns
        .iter()
        .zip(row)
        .map(|(column, value)| (column.name.clone(), value))
        .collect();
    let change = RowChange {
        operation: "update".into(),
        key: [
            ("a".into(), before["a"].clone()),
            ("b".into(), before["b"].clone()),
        ]
        .into_iter()
        .collect(),
        before,
        values: [("value".into(), Value::Text("updated".into()))]
            .into_iter()
            .collect(),
    };
    assert_eq!(
        submit(&mut f, &composite_target, &change).await.unwrap()["items"][0]["status"],
        "applied"
    );
    let result = f
        .sql(&format!("SELECT value FROM `{table}` ORDER BY b"))
        .await
        .unwrap();
    assert_eq!(
        f.rows(&result).await,
        vec![
            vec![Value::Text("first".into())],
            vec![Value::Text("updated".into())]
        ]
    );

    f.sql(&format!("CREATE TABLE `{prefix_table}` (a INT NOT NULL, code VARCHAR(32) NOT NULL, value TEXT, UNIQUE KEY uq (a, code(4))) ENGINE=InnoDB")).await.unwrap();
    let prefix_target = target(&prefix_table);
    assert!(describe(&mut f, &prefix_target).await.keys.is_empty());
    assert!(
        f.call("change.open", json!({"target":prefix_target}))
            .await
            .is_err()
    );
    f.sql(&format!("DROP TABLE `{table}`, `{prefix_table}`"))
        .await
        .unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 8.0.13 或更新版本"]
async fn expression_unique_key_does_not_enable_row_editing() {
    let mut f = Fixture::open().await;
    let table = name("dp_expression");
    f.sql(&format!("CREATE TABLE `{table}` (code VARCHAR(32) NOT NULL, value TEXT, UNIQUE KEY uq ((LOWER(code)))) ENGINE=InnoDB")).await.unwrap();
    let target = target(&table);
    let schema = describe(&mut f, &target).await;
    assert!(schema.keys.is_empty());
    assert!(
        schema.details["indexes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|index| index["expression"]
                .as_str()
                .is_some_and(|expression| !expression.is_empty()))
    );
    assert!(
        f.call("change.open", json!({"target":target}))
            .await
            .is_err()
    );
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn nullable_unique_key_does_not_enable_row_editing() {
    let mut f = Fixture::open().await;
    let table = name("dp_nullable");
    f.sql(&format!(
        "CREATE TABLE `{table}` (code VARCHAR(32) NULL UNIQUE, value TEXT) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    f.sql(&format!(
        "INSERT INTO `{table}` VALUES (NULL, 'a'), (NULL, 'b')"
    ))
    .await
    .unwrap();
    let target = target(&table);
    let schema = describe(&mut f, &target).await;
    assert!(schema.keys.is_empty(), "可空唯一键不能作为可靠行标识");
    assert!(
        f.call("change.open", json!({"target":target}))
            .await
            .is_err()
    );
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn bit_numeric_and_boolean_writes_preserve_bit_values() {
    let mut f = Fixture::open().await;
    let table = name("dp_bit");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY, flag BIT(1), bits BIT(9)) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    let target = target(&table);
    f.call(
        "import.rows",
        json!({"target":target,"columns":["id","flag","bits"],"rows":[[
            Value::Integer("1".into()), Value::Bool(true), Value::Unsigned("511".into())
        ]]}),
    )
    .await
    .unwrap();
    let result = f
        .sql(&format!(
            "SELECT flag + 0, bits + 0 FROM `{table}` WHERE id=1"
        ))
        .await
        .unwrap();
    let rows = f.rows(&result).await;
    assert_eq!(rows[0][0].text(), "1");
    assert_eq!(rows[0][1].text(), "511");
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn zero_year_survives_import_and_row_updates() {
    let mut f = Fixture::open().await;
    let table = name("dp_year");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY, y YEAR NOT NULL) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    let target = target(&table);
    f.call(
        "import.rows",
        json!({"target":target,"columns":["id","y"],"rows":[
            [Value::Integer("1".into()), Value::Integer("0".into())],
            [Value::Integer("2".into()), Value::Unsigned("0".into())]
        ]}),
    )
    .await
    .unwrap();
    let result = f
        .sql(&format!("SELECT y + 0 FROM `{table}` ORDER BY id"))
        .await
        .unwrap();
    let rows = f.rows(&result).await;
    assert_eq!(rows[0][0].text(), "0");
    assert_eq!(rows[1][0].text(), "0");

    let schema = describe(&mut f, &target).await;
    let baseline = before(&mut f, &table, &schema).await;
    let change = update(baseline, "y", Value::Unsigned("2026".into()));
    submit(&mut f, &target, &change).await.unwrap();
    let baseline = before(&mut f, &table, &schema).await;
    let change = update(baseline, "y", Value::Integer("0".into()));
    submit(&mut f, &target, &change).await.unwrap();
    let result = f
        .sql(&format!("SELECT y + 0 FROM `{table}` WHERE id=1"))
        .await
        .unwrap();
    assert_eq!(f.rows(&result).await[0][0].text(), "0");
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4 及 TRIGGER 权限"]
async fn cancellation_during_last_insert_rolls_back_transaction() {
    let mut f = Fixture::open().await;
    let table = name("dp_cancel_batch");
    let trigger = name("dp_cancel_trigger");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    f.sql(&format!("CREATE TRIGGER `{trigger}` BEFORE INSERT ON `{table}` FOR EACH ROW SET @dp_delay = SLEEP(2)"))
        .await
        .unwrap();
    let task = f
        .store
        .create_task(Uuid::new_v4(), "fixture", "import.rows", json!({}), false)
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let store = f.store.clone();
    let task_id = task.id;
    tokio::spawn(async move {
        while !store.task(task_id).await.unwrap().sent {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        signal.cancel();
    });
    let error =
        f.db.call(
            "import.rows",
            json!({"target":target(&table),"columns":["id"],"rows":[[Value::Integer("1".into())]]}),
            &ExecutionSink::new(f.store.clone(), task.id),
            &cancel,
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, "cancelled");
    assert_eq!(error.details["items"][0]["status"], "rolled_back");
    let result = f
        .sql(&format!("SELECT COUNT(*) FROM `{table}`"))
        .await
        .unwrap();
    assert_eq!(f.rows(&result).await[0][0].text(), "0");
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4、MyISAM 和 TRIGGER 权限"]
async fn cancellation_with_nontransactional_trigger_is_outcome_unknown() {
    let mut f = Fixture::open().await;
    let mut observer = Fixture::open().await;
    let table = name("dp_cancel_innodb");
    let audit = name("dp_cancel_audit");
    let trigger = name("dp_cancel_trigger");
    let delay_trigger = name("dp_delay_trigger");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    f.sql(&format!(
        "CREATE TABLE `{audit}` (id INT NOT NULL) ENGINE=MyISAM"
    ))
    .await
    .unwrap();
    f.sql(&format!("CREATE TRIGGER `{trigger}` BEFORE INSERT ON `{table}` FOR EACH ROW INSERT INTO `{audit}` (id) VALUES (NEW.id)"))
        .await.unwrap();
    f.sql(&format!("CREATE TRIGGER `{delay_trigger}` AFTER INSERT ON `{table}` FOR EACH ROW SET @dp_delay = IF(GET_LOCK('{delay_trigger}', 0) = 1, SLEEP(3), 0)"))
        .await.unwrap();
    let task = f
        .store
        .create_task(Uuid::new_v4(), "fixture", "import.rows", json!({}), false)
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    let observed_lock = delay_trigger.clone();
    let watcher = tokio::spawn(async move {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let result = observer
                    .sql(&format!("SELECT IS_USED_LOCK('{observed_lock}')"))
                    .await
                    .unwrap();
                if !matches!(&observer.rows(&result).await[0][0], Value::Null) {
                    signal.cancel();
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("等待非事务表写入");
        observer.close().await;
    });
    let error =
        f.db.call(
            "import.rows",
            json!({"target":target(&table),"columns":["id"],"rows":[[Value::Integer("1".into())]]}),
            &ExecutionSink::new(f.store.clone(), task.id),
            &cancel,
        )
        .await
        .unwrap_err();
    watcher.await.unwrap();
    assert_eq!(error.code, "outcome_unknown");
    assert_eq!(error.details["items"][0]["status"], "unknown");
    let result = f
        .sql(&format!("SELECT COUNT(*) FROM `{table}`"))
        .await
        .unwrap();
    assert_eq!(f.rows(&result).await[0][0].text(), "0");
    let result = f
        .sql(&format!("SELECT COUNT(*) FROM `{audit}`"))
        .await
        .unwrap();
    assert_eq!(f.rows(&result).await[0][0].text(), "1");
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.sql(&format!("DROP TABLE `{audit}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4，且启用 MyISAM"]
async fn preview_reuses_bindings_preserves_token_and_detects_conflicts() {
    let mut f = Fixture::open().await;
    for engine in ["InnoDB", "MyISAM"] {
        let table = name("dp_preview");
        f.sql(&format!("CREATE TABLE `{table}` (id INT PRIMARY KEY, label LONGTEXT, bin VARBINARY(32), amount DECIMAL(30,6), optional VARCHAR(20), y YEAR, bits BIT(9), n FLOAT) ENGINE={engine}")).await.unwrap();
        f.sql(&format!("INSERT INTO `{table}` VALUES (1, REPEAT('x',70000), X'00FF', 123456789012345678901234.123456, NULL, 2024, b'100000001', 1.25)")).await.unwrap();
        let target = target(&table);
        let opened = f
            .call("change.open", json!({"target":target}))
            .await
            .unwrap();
        let schema: TableSchema = serde_json::from_value(opened["schema"].clone()).unwrap();
        let result = f
            .sql(&format!("SELECT * FROM `{table}` WHERE id=1"))
            .await
            .unwrap();
        let result_id: Uuid = serde_json::from_value(result["results"][0].clone()).unwrap();
        let baseline: BTreeMap<String, Value> = schema
            .columns
            .iter()
            .zip(f.rows(&result).await.remove(0))
            .map(|(c, v)| (c.name.clone(), v))
            .collect();
        assert!(matches!(baseline["label"], Value::Large { .. }));
        let mut change = update(
            baseline.clone(),
            "label",
            Value::Text("changed\nvalue".into()),
        );
        change
            .values
            .insert("bin".into(), Value::Bytes("AP8=".into()));
        change.values.insert(
            "amount".into(),
            Value::Decimal("123456789012345678901234.123456".into()),
        );
        change.values.insert("optional".into(), Value::Null);
        change
            .values
            .insert("y".into(), Value::Integer("2025".into()));
        change
            .values
            .insert("bits".into(), Value::Unsigned("511".into()));
        change.values.insert("n".into(), Value::Float("1.5".into()));
        let rows = vec![change.clone(); 21];
        let args = json!({"token":opened["token"],"rows":rows});
        let page = f.call("change.preview", args.clone()).await.unwrap();
        assert_eq!(page["transactional"], engine == "InnoDB");
        assert_eq!(page["total_changes"], 21);
        assert_eq!(page["items"].as_array().unwrap().len(), 20);
        assert_eq!(page["next_offset"], 20);
        let statements = page["items"][0]["statements"].as_array().unwrap();
        assert_eq!(statements.len(), if engine == "InnoDB" { 2 } else { 1 });
        let check_or_write = &statements[0];
        assert!(check_or_write["text"].as_str().unwrap().contains("IS NULL"));
        assert!(
            check_or_write["parameters"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p["byte_len"] == 70000 && p["complete"] == false)
        );
        let write = statements.last().unwrap();
        let bindings = write["parameters"].as_array().unwrap();
        assert!(
            bindings
                .iter()
                .any(|p| p["kind"] == "bytes" && p["display"] == "AP8=")
        );
        assert!(
            bindings
                .iter()
                .any(|p| p["kind"] == "text"
                    && p["display"] == "\"123456789012345678901234.123456\"")
        );
        assert!(bindings.iter().any(|p| p["kind"] == "null"));
        assert!(
            bindings
                .iter()
                .any(|p| p["kind"] == "year" && p["display"] == "2025")
        );
        assert!(
            bindings
                .iter()
                .any(|p| p["kind"] == "float32" && p["display"] == "1.5")
        );
        let last = f
            .call(
                "change.preview",
                json!({"token":opened["token"],"rows":rows,"offset":20,"limit":20}),
            )
            .await
            .unwrap();
        assert_eq!(last["items"][0]["index"], 20);
        assert!(last["next_offset"].is_null());
        let unchanged = f
            .sql(&format!("SELECT label FROM `{table}` WHERE id=1"))
            .await
            .unwrap();
        let label = f.rows(&unchanged).await.remove(0).remove(0);
        assert!(matches!(label, Value::Large { .. }));
        f.call(
            "change.submit",
            json!({"token":opened["token"],"rows":[change]}),
        )
        .await
        .unwrap();
        let result = f
            .sql(&format!(
                "SELECT label, bin, amount, optional, y, bits, n FROM `{table}` WHERE id=1"
            ))
            .await
            .unwrap();
        let actual = f.rows(&result).await.remove(0);
        assert_eq!(actual[0], Value::Text("changed\nvalue".into()));
        assert_eq!(actual[1], Value::Bytes("AP8=".into()));
        assert_eq!(actual[2].text(), "123456789012345678901234.123456");
        assert_eq!(actual[3], Value::Null);
        assert_eq!(actual[4].text(), "2025");
        let stale = update(baseline.clone(), "label", Value::Text("stale".into()));
        let opened = f
            .call("change.open", json!({"target":target}))
            .await
            .unwrap();
        f.call(
            "change.preview",
            json!({"token":opened["token"],"rows":[stale]}),
        )
        .await
        .unwrap();
        let error = f
            .call(
                "change.submit",
                json!({"token":opened["token"],"rows":[stale]}),
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "conflict");
        let opened = f
            .call("change.open", json!({"target":target}))
            .await
            .unwrap();
        f.store.release_result(result_id).await.unwrap();
        f.call(
            "change.preview",
            json!({"token":opened["token"],"rows":[stale]}),
        )
        .await
        .expect_err("已释放的大值引用不能生成预览");
        let unchanged = f
            .sql(&format!("SELECT label FROM `{table}` WHERE id=1"))
            .await
            .unwrap();
        assert_eq!(
            f.rows(&unchanged).await[0][0],
            Value::Text("changed\nvalue".into())
        );
        f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    }
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7 或 8.4"]
async fn preview_json_check_and_write_match_server_version() {
    let mut f = Fixture::open().await;
    let version_result = f.sql("SELECT VERSION()").await.unwrap();
    let version = f.rows(&version_result).await[0][0].text();
    assert!(
        version.starts_with("5.7.") || version.starts_with("8.4."),
        "此回归分别在 5.7 和 8.4 运行: {version}"
    );
    let table = name("dp_preview_json");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY, doc JSON NOT NULL) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    f.sql(&format!(
        "INSERT INTO `{table}` VALUES (1, JSON_OBJECT('version', 1, 'text', 'old'))"
    ))
    .await
    .unwrap();
    let target = target(&table);
    let opened = f
        .call("change.open", json!({"target":target}))
        .await
        .unwrap();
    let schema: TableSchema = serde_json::from_value(opened["schema"].clone()).unwrap();
    let baseline = before(&mut f, &table, &schema).await;
    let value = json!({"version":2,"text":"new"});
    let change = update(baseline, "doc", Value::Json(value.clone()));
    let preview = f
        .call(
            "change.preview",
            json!({"token":opened["token"],"rows":[change]}),
        )
        .await
        .unwrap();
    let statements = preview["items"][0]["statements"].as_array().unwrap();
    assert!(
        statements[0]["text"]
            .as_str()
            .unwrap()
            .contains("CAST(? AS JSON)")
    );
    let expected_display = serde_json::to_string(&value.to_string()).unwrap();
    assert!(
        statements[1]["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["kind"] == "text" && p["display"] == expected_display)
    );
    f.call(
        "change.submit",
        json!({"token":opened["token"],"rows":[change]}),
    )
    .await
    .unwrap();
    let result = f
        .sql(&format!("SELECT doc FROM `{table}` WHERE id=1"))
        .await
        .unwrap();
    assert_eq!(f.rows(&result).await[0][0], Value::Json(value));
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}

#[tokio::test]
#[ignore = "需要非 strict 默认模式的隔离 MySQL 5.7、8.0 或 8.4"]
async fn structured_writes_reject_overlong_text_on_non_strict_server() {
    let mut f = Fixture::open().await;
    let mode = f.sql("SELECT @@SESSION.sql_mode").await.unwrap();
    let mode = f.rows(&mode).await;
    let Value::Text(mode) = &mode[0][0] else {
        panic!("缺少 SQL 模式")
    };
    assert!(!mode.to_ascii_uppercase().contains("STRICT_"));
    for engine in ["InnoDB", "MyISAM"] {
        let table = name("dp_length");
        f.sql(&format!("CREATE TABLE `{table}` (id INT PRIMARY KEY, value VARCHAR(100) NOT NULL, day DATE NULL) ENGINE={engine} DEFAULT CHARSET=utf8mb4")).await.unwrap();
        f.sql(&format!(
            "INSERT INTO `{table}` (id,value) VALUES (1, 'original')"
        ))
        .await
        .unwrap();
        let t = target(&table);
        let schema = describe(&mut f, &t).await;
        let long = Value::Text(format!("{} ", "界".repeat(100)));
        let insert = RowChange {
            operation: "insert".into(),
            key: BTreeMap::new(),
            before: BTreeMap::new(),
            values: [
                ("id".into(), Value::Integer("2".into())),
                ("value".into(), long.clone()),
            ]
            .into_iter()
            .collect(),
        };
        assert!(submit(&mut f, &t, &insert).await.is_err());
        let mut ordinary = insert.clone();
        ordinary
            .values
            .insert("value".into(), Value::Text("x".repeat(101)));
        assert!(submit(&mut f, &t, &ordinary).await.is_err());
        let old = before(&mut f, &table, &schema).await;
        assert!(
            submit(&mut f, &t, &update(old, "value", long.clone()))
                .await
                .is_err()
        );
        assert!(f.call("import.rows", json!({"target":t,"columns":["id","value"],"rows":[[Value::Integer("3".into()),long]]})).await.is_err());
        // DATE 类型匹配但值无效，由结构化写连接的 strict 模式阻止服务端修正。
        assert!(f.call("import.rows", json!({"target":t,"columns":["id","value","day"],"rows":[[Value::Integer("5".into()),Value::Text("valid".into()),Value::Date("2024-02-31".into())]]})).await.is_err());
        let result = f
            .sql(&format!("SELECT id, value FROM `{table}` ORDER BY id"))
            .await
            .unwrap();
        assert_eq!(
            f.rows(&result).await,
            vec![vec![
                Value::Integer("1".into()),
                Value::Text("original".into())
            ]]
        );
        let mut boundary = insert;
        boundary
            .values
            .insert("value".into(), Value::Text("界".repeat(100)));
        submit(&mut f, &t, &boundary).await.unwrap();
        let result = f
            .sql(&format!("SELECT value FROM `{table}` WHERE id=2"))
            .await
            .unwrap();
        assert_eq!(f.rows(&result).await[0][0], Value::Text("界".repeat(100)));
        // 查询标签继续使用服务端默认的非 strict 模式。
        f.sql(&format!(
            "INSERT INTO `{table}` (id,value) VALUES (4, '{}')",
            "x".repeat(101)
        ))
        .await
        .unwrap();
        let warning_count = f.sql("show /* count */ count(*) warnings").await.unwrap();
        assert_eq!(f.rows(&warning_count).await[0][0].text(), "1");
        let warnings = f.sql("/* diagnostics */ sHoW wArNiNgS").await.unwrap();
        let warnings = f.rows(&warnings).await;
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0][0].text(), "Warning");
        assert_eq!(warnings[0][1].text(), "1265");
        let error_count = f.sql("SHOW COUNT(*) ERRORS").await.unwrap();
        assert_eq!(f.rows(&error_count).await[0][0].text(), "0");
        let errors = f.sql("SHOW ERRORS").await.unwrap();
        let error_result: Uuid = serde_json::from_value(errors["results"][0].clone()).unwrap();
        let error_meta = f.store.result_meta(error_result).await.unwrap();
        assert_eq!(error_meta.kind, "columns_unknown");
        assert_eq!(error_meta.summary["columns_known"], false);
        assert!(f.rows(&errors).await.is_empty());
        let result = f
            .sql(&format!(
                "SELECT CHAR_LENGTH(value) FROM `{table}` WHERE id=4"
            ))
            .await
            .unwrap();
        assert_eq!(f.rows(&result).await[0][0], Value::Integer("100".into()));
        let batch = f
            .sql(&format!(
                "INSERT INTO `{table}` (id,value) VALUES (6, '{}'); SHOW WARNINGS",
                "x".repeat(101)
            ))
            .await
            .unwrap();
        assert_eq!(f.rows(&batch).await[0][1].text(), "1265");
        f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    }
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL TLS 服务、私有 CA、匹配和不匹配的主机名"]
async fn mysql_tls_requires_trusted_chain_and_matching_identity() {
    let f = Fixture::open().await;
    let mut profile = f.profile.clone();
    let direct = profile.direct_mut().unwrap();
    direct.host = std::env::var("DATAPIER_TEST_MYSQL_TLS_HOST").unwrap();
    direct.port = std::env::var("DATAPIER_TEST_MYSQL_TLS_PORT")
        .unwrap()
        .parse()
        .unwrap();
    direct.tls.enabled = true;
    let untrusted = match ProviderSession::connect(profile.clone(), f.secret.clone()).await {
        Ok(_) => panic!("未配置 CA 却信任了私有 CA"),
        Err(error) => error,
    };
    assert!(
        untrusted
            .message
            .to_ascii_lowercase()
            .contains("certificate")
    );
    profile.direct_mut().unwrap().tls.ca_path =
        Some(std::env::var("DATAPIER_TEST_MYSQL_TLS_CA").unwrap());
    let mut trusted = ProviderSession::connect(profile.clone(), f.secret.clone())
        .await
        .unwrap();
    trusted.close().await.unwrap();
    profile.direct_mut().unwrap().host =
        std::env::var("DATAPIER_TEST_MYSQL_TLS_WRONG_HOST").unwrap();
    let wrong_host = match ProviderSession::connect(profile, f.secret.clone()).await {
        Ok(_) => panic!("错误主机名通过身份验证"),
        Err(error) => error,
    };
    assert!(
        wrong_host
            .message
            .to_ascii_lowercase()
            .contains("certificate")
    );
    f.close().await;
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 和 CREATE ROUTINE 权限"]
async fn releasing_call_result_still_drains_later_write() {
    let mut f = Fixture::open().await;
    let table = name("dp_call_release");
    let procedure = name("dp_call_release_proc");
    f.sql(&format!(
        "CREATE TABLE `{table}` (id INT PRIMARY KEY, value INT NOT NULL) ENGINE=InnoDB"
    ))
    .await
    .unwrap();
    f.sql(&format!("INSERT INTO `{table}` VALUES (1, 0)"))
        .await
        .unwrap();
    f.sql(&format!("CREATE PROCEDURE `{procedure}`() BEGIN SELECT value FROM `{table}` CROSS JOIN information_schema.COLUMNS LIMIT 100; UPDATE `{table}` SET value=value+1 WHERE id=1; END")).await.unwrap();
    let task = f
        .store
        .create_task(Uuid::new_v4(), "fixture", "execution.run", json!({}), false)
        .await
        .unwrap();
    let replacement = ProviderSession::connect(f.profile.clone(), f.secret.clone())
        .await
        .unwrap();
    let mut worker = std::mem::replace(&mut f.db, replacement);
    let store = f.store.clone();
    let task_id = task.id;
    let call_sql = format!("CALL `{procedure}`()");
    let handle = tokio::spawn(async move {
        let result = worker
            .call(
                "execution.run",
                json!({"text":call_sql}),
                &ExecutionSink::new(store, task_id),
                &CancellationToken::new(),
            )
            .await;
        (worker, result)
    });
    let id = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Some(id) = f.store.result_ids(task.id).await.unwrap().first().copied() {
                break id;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        f.store.wait_result_rows(id, 100),
    )
    .await
    .unwrap()
    .unwrap();
    let page = f.store.result_page(id, 0, 100, 1024 * 1024).await.unwrap();
    assert_eq!(page.rows.len(), 100);
    assert!(!page.result.complete);
    f.store.release_result(id).await.unwrap();
    let (mut worker, result) = tokio::time::timeout(std::time::Duration::from_secs(10), handle)
        .await
        .unwrap()
        .unwrap();
    result.unwrap();
    worker.close().await.unwrap();
    let result = f
        .sql(&format!("SELECT value FROM `{table}` WHERE id=1"))
        .await
        .unwrap();
    assert_eq!(f.rows(&result).await[0][0], Value::Integer("1".into()));
    f.sql(&format!("DROP PROCEDURE `{procedure}`"))
        .await
        .unwrap();
    f.sql(&format!("DROP TABLE `{table}`")).await.unwrap();
    f.close().await;
}
