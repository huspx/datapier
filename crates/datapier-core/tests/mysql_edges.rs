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
            provider: ProviderKind::Mysql,
            host: "127.0.0.1".into(),
            port: std::env::var("DATAPIER_TEST_MYSQL_PORT")
                .expect("设置隔离MySQL端口")
                .parse()
                .unwrap(),
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            redis_db: 0,
            access_mode: AccessMode::ReadWrite,
            tls: TlsConfig::default(),
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
        self.store.cache.close().await;
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
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
#[ignore = "需要隔离 MySQL 8.0 或 8.4"]
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
        "/* header */ CREATE\nDEFINER=CURRENT_USER\nPROCEDURE `{procedure}`()\nBEGIN\n SELECT 1 AS empty_value WHERE FALSE;\n SELECT 2 AS value;\nEND"
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
#[ignore = "需要隔离 MySQL 8.0 或 8.4"]
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
    assert_eq!(
        submit(&mut f, &target, &same).await.unwrap()["items"][0]["status"],
        "applied"
    );
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
#[ignore = "需要隔离 MySQL 8.0 或 8.4"]
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
