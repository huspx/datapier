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
            visible_databases: None,
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

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn task_database_follows_use_instead_of_cached_prepare_context() {
    let mut f = Fixture::open().await;
    f.sql("SELECT 1").await.unwrap();
    f.sql("USE information_schema").await.unwrap();
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
