//! 视图编辑与触发器资源的真实 MySQL 契约回归。
use datapier_core::{ExecutionSink, Store, model::*, providers::ProviderSession};
use serde_json::{Value as Json, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

struct Fixture {
    store: Store,
    db: ProviderSession,
    table: String,
    view: String,
    trigger: String,
}

impl Fixture {
    async fn open() -> Self {
        let root = std::env::temp_dir().join(format!("datapier-view-{}", Uuid::new_v4()));
        let store = Store::open(&root).await.unwrap();
        let profile = Profile {
            id: Uuid::new_v4(),
            name: "view-fixture".into(),
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
        let id = Uuid::new_v4().simple().to_string();
        Self {
            store,
            db,
            table: format!("dp_t_{id}"),
            view: format!("dp_v_{id}"),
            trigger: format!("dp_g_{id}"),
        }
    }
    async fn call(&mut self, method: &str, params: Json) -> Result<Json> {
        let task = self
            .store
            .create_task(Uuid::new_v4(), "view-fixture", method, json!({}), false)
            .await?;
        let sink = ExecutionSink::new(self.store.clone(), task.id);
        self.db
            .call(method, params, &sink, &CancellationToken::new())
            .await
    }
    async fn sql(&mut self, text: &str) -> Result<Json> {
        self.call("execution.run", json!({"text":text})).await
    }
    async fn close(mut self) {
        let view = self.view.clone();
        let table = self.table.clone();
        self.sql(&format!("DROP VIEW IF EXISTS `{view}`"))
            .await
            .unwrap();
        self.sql(&format!("DROP TABLE IF EXISTS `{table}`"))
            .await
            .unwrap();
        self.db.close().await.unwrap();
        let root = (*self.store.root).clone();
        self.store.pool.close().await;
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}

#[tokio::test]
#[ignore = "需要隔离 MySQL 5.7、8.0 或 8.4 及触发器权限"]
async fn view_mapping_token_and_trigger_listing_follow_real_server_state() {
    let mut f = Fixture::open().await;
    let table = f.table.clone();
    let view = f.view.clone();
    let trigger = f.trigger.clone();
    let outcome: Result<()> = async {
        f.sql(&format!("CREATE TABLE `{table}` (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, body VARCHAR(100) NOT NULL) ENGINE=InnoDB")).await?;
        f.sql(&format!("CREATE VIEW `{view}` AS SELECT id, body FROM `{table}`")).await?;
        f.sql(&format!("CREATE TRIGGER `{trigger}` BEFORE INSERT ON `{table}` FOR EACH ROW SET NEW.body = NEW.body")).await?;
        f.sql(&format!("INSERT INTO `{table}` (body) VALUES ('old')")).await?;
        let target = ResourceTarget { database: Some("datapier_fixture".into()), name: view.clone(), kind: "view".into(), key_b64: None };
        let described = f.call("resource.describe", json!({"target":target})).await?;
        let opened = f.call("change.open", json!({"target":target})).await?;
        let mapping = &opened["schema"]["details"]["view_mapping"];
        let mut described_writable = described["details"]["view_mapping"]["writable"].as_array().unwrap().clone();
        let mut opened_writable = mapping["writable"].as_array().unwrap().clone();
        described_writable.sort_by_key(|v| v.as_str().unwrap().to_owned());
        opened_writable.sort_by_key(|v| v.as_str().unwrap().to_owned());
        assert_eq!(described_writable, opened_writable);
        assert_eq!(opened_writable, vec![json!("body"), json!("id")]);
        assert_eq!(described["details"]["view_mapping"]["fingerprint"], mapping["fingerprint"]);
        let listed = f.call("resource.list", json!({"database":"datapier_fixture","kind":"trigger","table":table})).await?;
        assert_eq!(listed["complete"], true);
        assert!(listed["resources"].as_array().unwrap().iter().any(|item| item["name"] == trigger && item["kind"] == "trigger" && item["database"] == "datapier_fixture" && item["data"]["table"] == table));
        let change = RowChange {
            operation: "update".into(), key: [("id".into(), Value::Integer("1".into()))].into_iter().collect(),
            before: [("id".into(), Value::Integer("1".into())), ("body".into(), Value::Text("old".into()))].into_iter().collect(),
            values: [("body".into(), Value::Text("new".into()))].into_iter().collect(),
        };
        f.sql(&format!("INSERT INTO `{table}` (body) VALUES ('other')")).await?;
        f.call("change.submit", json!({"target":target,"token":opened["token"],"rows":[change]})).await?;
        let stale = f.call("change.open", json!({"target":target})).await?;
        f.sql(&format!("ALTER TABLE `{table}` ADD COLUMN extra INT NULL")).await?;
        let error = f.call("change.submit", json!({"target":target,"token":stale["token"],"rows":[change]})).await.unwrap_err();
        assert_eq!(error.code, "conflict");
        Ok(())
    }.await;
    f.close().await;
    outcome.unwrap();
}
