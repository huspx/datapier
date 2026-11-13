//! MySQL 替换计划的真实服务端回归；仅连接显式配置的隔离实例。
use datapier_core::{ExecutionSink, Store, model::*, providers::ProviderSession};
use serde_json::{Value as Json, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Fixture {
    store: Store,
    db: ProviderSession,
    table: String,
}

impl Fixture {
    async fn open() -> Self {
        let root = std::env::temp_dir().join(format!("datapier-mysql-replace-{}", Uuid::new_v4()));
        let store = Store::open(&root).await.unwrap();
        let profile = Profile {
            id: Uuid::new_v4(),
            name: "replace-fixture".into(),
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
        let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").expect("设置测试MySQL凭据");
        let db = ProviderSession::connect(profile, Some(secret))
            .await
            .unwrap();
        Self {
            store,
            db,
            table: format!("dp_replace_{}", Uuid::new_v4().simple()),
        }
    }

    fn target(&self) -> ResourceTarget {
        ResourceTarget {
            database: Some("datapier_fixture".into()),
            name: self.table.clone(),
            key_b64: None,
            kind: "table".into(),
        }
    }

    async fn call(&mut self, method: &str, params: Json) -> Result<Json> {
        let task = self
            .store
            .create_task(Uuid::new_v4(), "replace-fixture", method, json!({}), false)
            .await?;
        let sink = ExecutionSink::new(self.store.clone(), task.id);
        self.db
            .call(method, params, &sink, &CancellationToken::new())
            .await
    }

    async fn sql(&mut self, sql: &str) -> Result<Json> {
        self.call("execution.run", json!({"text": sql})).await
    }

    async fn rows(&self, result: &Json) -> Vec<Vec<Value>> {
        let id: Uuid = serde_json::from_value(result["results"][0].clone()).unwrap();
        self.store
            .result_page(id, 0, 100, 1024 * 1024)
            .await
            .unwrap()
            .rows
    }

    async fn close(mut self) {
        let table = self.table.clone();
        self.sql(&format!("DROP TABLE IF EXISTS `{table}`"))
            .await
            .unwrap();
        self.db.close().await.unwrap();
        let root = (*self.store.root).clone();
        self.store.pool.close().await;
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}

fn spec() -> Json {
    json!({"pattern":"old","replacement":"new","regex":false,"case_sensitive":true,"multi_line":false,"dot_matches_new_line":false,"scope":"filtered","columns":["body"],"roles":[]})
}

fn submit(summary: &Json) -> Json {
    json!({"plan_id":summary["id"],"revision":summary["revision"],"digest":summary["plan_digest"]})
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn oversized_unselected_and_unmatched_values_make_collection_incomplete() {
    let mut f = Fixture::open().await;
    let table = f.table.clone();
    let outcome: Result<()> = async {
        f.sql(&format!("CREATE TABLE `{table}` (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, body LONGTEXT, payload LONGBLOB, document JSON) ENGINE=InnoDB")).await?;
        // 第一个匹配行可以进入计划；第二行即使没有匹配也必须完整读取或标记超限。
        for oversized in [
            "('no match', REPEAT('x', 500000), NULL)",
            "(REPEAT('x', 500000), NULL, NULL)",
            "('no match', NULL, JSON_OBJECT('data', REPEAT('x', 500000)))",
        ] {
            f.sql(&format!("DELETE FROM `{table}`")).await?;
            f.sql(&format!("INSERT INTO `{table}` (body, payload, document) VALUES ('old', NULL, NULL), {oversized}")).await?;
            let plan = f.call("replace.prepare", json!({"target":f.target(),"spec":spec(),"_max_plan_bytes":65536})).await?;
            let summary = &plan["summary"];
            assert_eq!(summary["complete"], false);
            assert_eq!(summary["submittable"], false);
            assert!(summary["scanned"].as_u64().unwrap() >= 1);
            assert!(f.call("replace.submit", submit(summary)).await.is_err());
            let result = f.sql(&format!("SELECT COUNT(*) FROM `{table}` WHERE body = 'new'")).await?;
            assert_eq!(f.rows(&result).await[0][0], Value::Integer("0".into()));
        }
        Ok(())
    }.await;
    f.close().await;
    outcome.unwrap();
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4"]
async fn fixed_keys_survive_auto_increment_changes_but_not_schema_changes() {
    let mut f = Fixture::open().await;
    let table = f.table.clone();
    let outcome: Result<()> = async {
        f.sql(&format!("CREATE TABLE `{table}` (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, body VARCHAR(100), payload BLOB, document JSON) ENGINE=InnoDB")).await?;
        f.sql(&format!("INSERT INTO `{table}` (body, payload, document) VALUES ('old first', X'00FF', JSON_OBJECT('key', 'value'))")).await?;
        let plan = f.call("replace.prepare", json!({"target":f.target(),"spec":spec()})).await?;
        assert_eq!(plan["summary"]["complete"], true);
        assert_eq!(plan["summary"]["total_items"], 1);
        f.sql(&format!("INSERT INTO `{table}` (body) VALUES ('old second')")).await?;
        f.call("replace.submit", submit(&plan["summary"])).await?;
        let result = f.sql(&format!("SELECT body FROM `{table}` ORDER BY id")).await?;
        assert_eq!(f.rows(&result).await, vec![vec![Value::Text("new first".into())], vec![Value::Text("old second".into())]]);

        let plan = f.call("replace.prepare", json!({"target":f.target(),"spec":spec()})).await?;
        assert_eq!(plan["summary"]["complete"], true);
        f.sql(&format!("ALTER TABLE `{table}` ADD COLUMN extra INT NULL")).await?;
        let error = f.call("replace.submit", submit(&plan["summary"])).await.unwrap_err();
        assert_eq!(error.code, "conflict");
        let result = f.sql(&format!("SELECT body FROM `{table}` WHERE id = 2")).await?;
        assert_eq!(f.rows(&result).await[0][0], Value::Text("old second".into()));
        Ok(())
    }.await;
    f.close().await;
    outcome.unwrap();
}
