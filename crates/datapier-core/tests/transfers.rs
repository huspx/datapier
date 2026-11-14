//! 真实导入导出回归，只连接显式配置的隔离测试实例。
use base64::{Engine as _, engine::general_purpose::STANDARD};
use bigdecimal::BigDecimal;
use datapier_core::{
    ExecutionSink, Store, execution::transfer, model::*, providers::ProviderSession,
};
use futures_util::FutureExt;
use serde_json::{Value as Json, json};
use std::{panic::AssertUnwindSafe, str::FromStr};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

async fn context(kind: ProviderKind) -> (Store, Profile, Option<String>) {
    let root = std::env::temp_dir().join(format!("datapier-transfers-{}", Uuid::new_v4()));
    let store = Store::open(&root).await.unwrap();
    let (port, secret) = match kind {
        ProviderKind::Mysql => (
            std::env::var("DATAPIER_TEST_MYSQL_PORT").expect("设置隔离 MySQL 端口"),
            Some(std::env::var("DATAPIER_TEST_MYSQL_SECRET").expect("设置 MySQL 测试密码")),
        ),
        ProviderKind::Redis => (
            std::env::var("DATAPIER_TEST_REDIS_PORT").expect("设置隔离 Redis 端口"),
            None,
        ),
    };
    let profile = Profile {
        id: Uuid::new_v4(),
        name: "transfer-fixture".into(),
        revision: 1,
        approval_policy: ApprovalPolicy::Dangerous,
        provider: kind,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port: port.parse().unwrap(),
            username: if kind == ProviderKind::Mysql {
                "datapier".into()
            } else {
                String::new()
            },
            database: if kind == ProviderKind::Mysql {
                Some("datapier_fixture".into())
            } else {
                None
            },
            redis_db: 15,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    (store, profile, secret)
}
async fn sink(store: &Store, method: &str, profile: &Profile) -> ExecutionSink {
    let task = store
        .create_task(
            Uuid::new_v4(),
            "transfer-fixture",
            method,
            json!({"_task_context": {
                "source":"direct", "connection":profile.name, "provider":profile.provider,
                "database":profile.default_database(), "redis_db":profile.redis_db(), "targets":[]
            }}),
            false,
        )
        .await
        .unwrap();
    ExecutionSink::new(store.clone(), task.id)
}
async fn call(
    session: &mut ProviderSession,
    store: &Store,
    method: &str,
    params: Json,
) -> datapier_core::Result<Json> {
    let sink = sink(store, method, session.profile()).await;
    session
        .call(method, params, &sink, &CancellationToken::new())
        .await
}
async fn sql(session: &mut ProviderSession, store: &Store, text: &str) -> Json {
    call(session, store, "execution.run", json!({"text":text}))
        .await
        .unwrap()
}
async fn transfer_call(
    session: &mut ProviderSession,
    store: &Store,
    method: &str,
    params: Json,
) -> Json {
    let sink = sink(store, method, session.profile()).await;
    transfer::run(session, method, params, &sink, &CancellationToken::new())
        .await
        .unwrap()
}
async fn rows(store: &Store, response: &Json) -> Vec<Vec<Value>> {
    let id: Uuid = serde_json::from_value(response["results"][0].clone()).unwrap();
    let mut start = 0;
    let mut all = Vec::new();
    loop {
        let page = store
            .result_page(id, start, 1000, 1024 * 1024)
            .await
            .unwrap();
        all.extend(page.rows);
        match page.next {
            Some(n) => start = n,
            None => break,
        }
    }
    all
}
fn mysql_target(name: &str) -> ResourceTarget {
    ResourceTarget {
        database: Some("datapier_fixture".into()),
        name: name.into(),
        key_b64: None,
        kind: "table".into(),
    }
}
async fn clean(store: Store) {
    let root = (*store.root).clone();
    store.pool.close().await;
    drop(store);
    tokio::fs::remove_dir_all(root).await.unwrap();
}

#[tokio::test]
#[ignore = "需要显式配置的隔离 MySQL，处理十万行"]
async fn mysql_hundred_thousand_rows_csv_and_json_round_trip() {
    let (store, profile, secret) = context(ProviderKind::Mysql).await;
    let mut session = ProviderSession::connect(profile, secret).await.unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let source = format!("dp_src_{suffix}");
    let csv_table = format!("dp_csv_{suffix}");
    let json_table = format!("dp_json_{suffix}");
    let root = (*store.root).clone();
    let result = AssertUnwindSafe(async {
        let definition = "(id BIGINT UNSIGNED PRIMARY KEY, yr YEAR NOT NULL, amount DECIMAL(40,10) NOT NULL, label VARCHAR(100) NOT NULL, payload VARBINARY(100) NOT NULL, optional TEXT NULL, doubled INT GENERATED ALWAYS AS (yr * 2) STORED) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";
        for table in [&source,&csv_table,&json_table] { sql(&mut session,&store,&format!("CREATE TABLE `{table}` {definition}")).await; }
        let digits = "(SELECT 0 n UNION ALL SELECT 1 UNION ALL SELECT 2 UNION ALL SELECT 3 UNION ALL SELECT 4 UNION ALL SELECT 5 UNION ALL SELECT 6 UNION ALL SELECT 7 UNION ALL SELECT 8 UNION ALL SELECT 9)";
        let n = "(a.n+10*b.n+100*c.n+1000*d.n+10000*e.n)";
        let populate = format!("INSERT INTO `{source}` (id,yr,amount,label,payload,optional) SELECT 18446744073709451615+{n},2026,999999999999999999999999999999.1234567890,CONCAT('中😀',{n}),X'00FF005C',IF(MOD({n},2)=0,NULL,'') FROM {digits} a CROSS JOIN {digits} b CROSS JOIN {digits} c CROSS JOIN {digits} d CROSS JOIN {digits} e");
        sql(&mut session,&store,&populate).await;
        let csv_path = root.join("all.csv"); let json_path = root.join("all.json");
        let exported_csv = transfer_call(&mut session,&store,"transfer.export",json!({"format":"csv","path":csv_path,"target":mysql_target(&source)})).await;
        assert_eq!(exported_csv["progress"]["completed_rows"],100000);
        // 故意重排列顺序后导出已缓存结果，不能通过重执行 SQL 冒充结果缓存导出。
        let query = sql(&mut session,&store,&format!("SELECT label,yr,doubled,id,optional,amount,payload FROM `{source}` ORDER BY id")).await;
        let result_id = query["results"][0].clone();
        let exported_json = transfer_call(&mut session,&store,"transfer.export",json!({"format":"json","path":json_path,"result_id":result_id})).await;
        assert_eq!(exported_json["progress"]["completed_rows"],100000);
        for (format,path,table) in [("csv",&csv_path,&csv_table),("json",&json_path,&json_table)] {
            let imported = transfer_call(&mut session,&store,"transfer.import",json!({"format":format,"path":path,"target":mysql_target(table)})).await;
            assert_eq!(imported["progress"]["completed_rows"],100000);
            let summary = sql(&mut session,&store,&format!("SELECT COUNT(*),MIN(id),MAX(id),SUM(yr<>2026 OR doubled<>4052 OR amount<>999999999999999999999999999999.1234567890 OR payload<>X'00FF005C' OR (optional IS NOT NULL AND optional<>'')) FROM `{table}`")).await;
            let summary = rows(&store,&summary).await; let s = &summary[0];
            assert_eq!(s[0].text(),"100000"); assert_eq!(s[1].text(),"18446744073709451615"); assert_eq!(s[2].text(),"18446744073709551614"); assert_eq!(s[3].text(),"0");
            let endpoints = sql(&mut session,&store,&format!("SELECT id,yr,amount,label,payload,optional,doubled FROM `{table}` WHERE id IN (18446744073709451615,18446744073709551614) ORDER BY id")).await;
            let endpoints = rows(&store,&endpoints).await; assert_eq!(endpoints.len(),2);
            for (index,row) in endpoints.iter().enumerate() {
                assert_eq!(row[1].text(),"2026");
                assert_eq!(BigDecimal::from_str(&row[2].text()).unwrap(),BigDecimal::from_str("999999999999999999999999999999.1234567890").unwrap());
                assert_eq!(row[3],Value::Text(if index==0 {"中😀0"} else {"中😀99999"}.into()));
                assert_eq!(row[4],Value::Bytes(STANDARD.encode([0,255,0,92])));
                assert_eq!(row[5],if index==0 {Value::Null} else {Value::Text(String::new())}); assert_eq!(row[6].text(),"4052");
            }
        }
    }).catch_unwind().await;
    for table in [&source, &csv_table, &json_table] {
        let _ = call(
            &mut session,
            &store,
            "execution.run",
            json!({"text":format!("DROP TABLE IF EXISTS `{table}`")}),
        )
        .await;
    }
    let _ = session.close().await;
    clean(store).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[tokio::test]
#[ignore = "需要显式配置的隔离 MySQL"]
async fn mysql_large_rows_stop_at_budget_after_committed_batch() {
    let (store, profile, secret) = context(ProviderKind::Mysql).await;
    let mut settings = store.settings().await.unwrap();
    settings.limits.result_memory_bytes_per_client = 16 * 1024 * 1024;
    store.set_settings(&settings).await.unwrap();
    let mut session = ProviderSession::connect(profile, secret).await.unwrap();
    let table = format!("dp_budget_{}", Uuid::new_v4().simple());
    sql(
        &mut session,
        &store,
        &format!("CREATE TABLE `{table}` (id INT PRIMARY KEY, payload LONGTEXT NOT NULL)"),
    )
    .await;
    for format in ["csv", "json"] {
        let path = store.root.join(format!("budget.{format}"));
        if format == "csv" {
            let mut data = String::from("id,payload\n");
            for i in 0..500 {
                data.push_str(&format!("{i},ok\n"));
            }
            data.push_str(&format!("500,{}\n", "x".repeat(300_000)));
            std::fs::write(&path, data).unwrap();
        } else {
            let mut data = Vec::new();
            for i in 0..500 {
                data.push(vec![
                    Value::Integer(i.to_string()),
                    Value::Text("ok".into()),
                ]);
            }
            data.push(vec![
                Value::Integer("500".into()),
                Value::Text("x".repeat(300_000)),
            ]);
            std::fs::write(&path, serde_json::to_vec(&json!({"format":"datapier","version":1,"columns":[Column::new("id", "int"),Column::new("payload", "longtext")],"rows":data})).unwrap()).unwrap();
        }
        let sink = sink(&store, "transfer.import", session.profile()).await;
        let error = transfer::run(
            &mut session,
            "transfer.import",
            json!({"format":format,"path":path,"target":mysql_target(&table)}),
            &sink,
            &CancellationToken::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(error.code, "resource_limit");
        assert_eq!(error.details["progress"]["completed_rows"], 500);
        let result = sql(
            &mut session,
            &store,
            &format!("SELECT COUNT(*) FROM `{table}`"),
        )
        .await;
        assert_eq!(rows(&store, &result).await[0][0].text(), "500");
        sql(&mut session, &store, &format!("TRUNCATE TABLE `{table}`")).await;
    }
    sql(&mut session, &store, &format!("DROP TABLE `{table}`")).await;
    let _ = session.close().await;
    clean(store).await;
}

#[tokio::test]
#[ignore = "需要显式配置的隔离 MySQL"]
async fn mysql_csv_enum_set_type_names_round_trip() {
    let (store, profile, secret) = context(ProviderKind::Mysql).await;
    let mut session = ProviderSession::connect(profile, secret).await.unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let source = format!("dp_enum_src_{suffix}");
    let destination = format!("dp_enum_dst_{suffix}");
    let csv_path = store.root.join("enum-set.csv");
    let result = AssertUnwindSafe(async {
        let definition = "(id INT UNSIGNED PRIMARY KEY, choice ENUM('print','other') NOT NULL, bit_choice ENUM('BIT','other') NOT NULL, flags SET('BIT','REAL','JSON','DATE','BLOB') NOT NULL, payload VARBINARY(8) NOT NULL) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4";
        for table in [&source, &destination] {
            sql(&mut session, &store, &format!("CREATE TABLE `{table}` {definition}")).await;
        }
        sql(&mut session, &store, &format!("INSERT INTO `{source}` VALUES (1,'print','BIT','BIT,REAL,JSON,DATE,BLOB',X'00FF')")).await;
        let exported = transfer_call(&mut session, &store, "transfer.export", json!({"format":"csv","path":csv_path,"target":mysql_target(&source)})).await;
        assert_eq!(exported["progress"]["completed_rows"], 1);
        let preview_sink = sink(&store, "transfer.preview", session.profile()).await;
        let preview = transfer::preview(
            &mut session,
            json!({"format":"csv","path":csv_path,"target":mysql_target(&destination)}),
            &preview_sink,
            &CancellationToken::new(),
        ).await.unwrap();
        assert_eq!(preview["errors"], json!([]));
        assert_eq!(preview["sample_complete"], true);
        let preview_rows: Vec<Vec<Value>> = serde_json::from_value(preview["rows"].clone()).unwrap();
        assert_eq!(preview_rows, vec![vec![
            Value::Unsigned("1".into()),
            Value::Text("print".into()),
            Value::Text("BIT".into()),
            Value::Text("BIT,REAL,JSON,DATE,BLOB".into()),
            Value::Bytes(STANDARD.encode([0, 255])),
        ]]);
        let imported = transfer_call(&mut session, &store, "transfer.import", json!({"format":"csv","path":csv_path,"target":mysql_target(&destination)})).await;
        assert_eq!(imported["progress"]["completed_rows"], 1);
        let query = |table: &str| format!("SELECT id, CAST(choice AS CHAR), CAST(bit_choice AS CHAR), CAST(flags AS CHAR), payload FROM `{table}` ORDER BY id");
        let before = sql(&mut session, &store, &query(&source)).await;
        let after = sql(&mut session, &store, &query(&destination)).await;
        assert_eq!(rows(&store, &before).await, rows(&store, &after).await);
    }).catch_unwind().await;
    for table in [&source, &destination] {
        let _ = call(
            &mut session,
            &store,
            "execution.run",
            json!({"text":format!("DROP TABLE IF EXISTS `{table}`")}),
        )
        .await;
    }
    let _ = session.close().await;
    clean(store).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn redis_target(key: &[u8], kind: &str) -> ResourceTarget {
    ResourceTarget {
        database: Some("15".into()),
        name: String::from_utf8_lossy(key).into_owned(),
        key_b64: Some(STANDARD.encode(key)),
        kind: kind.into(),
    }
}
fn redis_database() -> ResourceTarget {
    ResourceTarget {
        database: Some("15".into()),
        name: String::new(),
        key_b64: None,
        kind: "database".into(),
    }
}
async fn redis_rows(
    session: &mut ProviderSession,
    store: &Store,
    target: &ResourceTarget,
) -> Vec<Vec<Value>> {
    let response = call(session, store, "resource.browse", json!({"target":target}))
        .await
        .unwrap();
    let mut rows = rows(store, &response).await;
    if target.kind == "hash" || target.kind == "set" || target.kind == "zset" {
        rows.sort_by_key(|r| serde_json::to_string(r).unwrap());
    }
    rows
}
async fn absolute_expiry(
    session: &mut ProviderSession,
    store: &Store,
    target: &ResourceTarget,
) -> i64 {
    let meta = call(session, store, "export.meta", json!({"target":target}))
        .await
        .unwrap();
    meta["expires_at_ms"].as_str().unwrap().parse().unwrap()
}
async fn redis_delete(conn: &mut redis::aio::MultiplexedConnection, keys: &[Vec<u8>]) {
    let _: i64 = redis::cmd("DEL").arg(keys).query_async(conn).await.unwrap();
}

#[tokio::test]
#[ignore = "需要显式配置的隔离 Redis，使用 DB 15"]
async fn redis_five_types_binary_keys_and_absolute_expiry_round_trip() {
    let (store, profile, secret) = context(ProviderKind::Redis).await;
    let mut session = ProviderSession::connect(profile.clone(), secret)
        .await
        .unwrap();
    let client = redis::Client::open(format!(
        "redis://127.0.0.1:{}/",
        profile.direct().unwrap().port
    ))
    .unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let _: () = redis::cmd("SELECT")
        .arg(15)
        .query_async(&mut conn)
        .await
        .unwrap();
    let command_info: Vec<Option<redis::Value>> = redis::cmd("COMMAND")
        .arg("INFO")
        .arg("HPEXPIRETIME")
        .query_async(&mut conn)
        .await
        .unwrap();
    let supports_field_expiry = command_info.first().is_some_and(Option::is_some);
    let prefix = format!("datapier-transfer-{}:", Uuid::new_v4().simple());
    let kinds = ["string", "hash", "list", "set", "zset"];
    let keys: Vec<Vec<u8>> = kinds
        .iter()
        .enumerate()
        .map(|(i, kind)| {
            let mut k = format!("{prefix}{kind}:").into_bytes();
            k.extend([0, 255, i as u8]);
            k
        })
        .collect();
    let targets: Vec<ResourceTarget> = keys
        .iter()
        .zip(kinds)
        .map(|(key, kind)| redis_target(key, kind))
        .collect();
    let root = (*store.root).clone();
    let result = AssertUnwindSafe(async {
        let _: () = redis::cmd("SET")
            .arg(&keys[0])
            .arg(&[0u8, 255, 92][..])
            .query_async(&mut conn)
            .await
            .unwrap();
        let _: i64 = redis::cmd("HSET")
            .arg(&keys[1])
            .arg(&[0u8, 255][..])
            .arg(&[92u8, 0][..])
            .arg("field")
            .arg("value")
            .query_async(&mut conn)
            .await
            .unwrap();
        let original_field_expiry = if supports_field_expiry {
            let applied: Vec<i64> = redis::cmd("HPEXPIRE")
                .arg(&keys[1])
                .arg(1_800_000)
                .arg("FIELDS")
                .arg(1)
                .arg("field")
                .query_async(&mut conn)
                .await
                .unwrap();
            assert_eq!(applied, vec![1]);
            let expiry: Vec<i64> = redis::cmd("HPEXPIRETIME")
                .arg(&keys[1])
                .arg("FIELDS")
                .arg(2)
                .arg("field")
                .arg(&[0u8, 255][..])
                .query_async(&mut conn)
                .await
                .unwrap();
            assert!(expiry[0] > 0);
            assert_eq!(expiry[1], -1);
            Some(expiry)
        } else {
            None
        };
        let _: i64 = redis::cmd("RPUSH")
            .arg(&keys[2])
            .arg("same")
            .arg(&[0u8, 255][..])
            .arg("same")
            .query_async(&mut conn)
            .await
            .unwrap();
        let _: i64 = redis::cmd("SADD")
            .arg(&keys[3])
            .arg(&[0u8, 255][..])
            .arg("member")
            .query_async(&mut conn)
            .await
            .unwrap();
        let _: i64 = redis::cmd("ZADD")
            .arg(&keys[4])
            .arg("1.25")
            .arg(&[0u8, 255][..])
            .arg("-2.5")
            .arg("member")
            .arg("+inf")
            .arg("positive-infinity")
            .arg("-inf")
            .arg("negative-infinity")
            .query_async(&mut conn)
            .await
            .unwrap();
        for key in &keys {
            let _: bool = redis::cmd("PEXPIRE")
                .arg(key)
                .arg(3_600_000)
                .query_async(&mut conn)
                .await
                .unwrap();
        }
        let mut original = Vec::new();
        let mut expiry = Vec::new();
        for target in &targets {
            original.push(redis_rows(&mut session, &store, target).await);
            expiry.push(absolute_expiry(&mut session, &store, target).await);
        }
        let json_path = root.join("redis.json");
        let csv_path = root.join("redis.csv");
        for (format, path) in [("json", &json_path), ("csv", &csv_path)] {
            let exported = transfer_call(
                &mut session,
                &store,
                "transfer.export",
                json!({"format":format,"path":path,"pattern":format!("{prefix}*")}),
            )
            .await;
            assert_eq!(exported["progress"]["completed_keys"], 5);
        }
        for (format, path) in [("json", &json_path), ("csv", &csv_path)] {
            redis_delete(&mut conn, &keys).await;
            let imported = transfer_call(
                &mut session,
                &store,
                "transfer.import",
                json!({"format":format,"path":path,"target":redis_database()}),
            )
            .await;
            assert_eq!(imported["progress"]["completed_keys"], 5);
            assert_eq!(imported["progress"]["skipped"], 0);
            if let Some(original_field_expiry) = &original_field_expiry {
                let restored: Vec<i64> = redis::cmd("HPEXPIRETIME")
                    .arg(&keys[1])
                    .arg("FIELDS")
                    .arg(2)
                    .arg("field")
                    .arg(&[0u8, 255][..])
                    .query_async(&mut conn)
                    .await
                    .unwrap();
                assert_eq!(&restored, original_field_expiry);
            }
            for (index, target) in targets.iter().enumerate() {
                assert_eq!(
                    redis_rows(&mut session, &store, target).await,
                    original[index]
                );
                let restored = absolute_expiry(&mut session, &store, target).await;
                assert!(
                    (restored - expiry[index]).abs() <= 10,
                    "absolute TTL changed: {restored} vs {}",
                    expiry[index]
                );
            }
        }
    })
    .catch_unwind()
    .await;
    redis_delete(&mut conn, &keys).await;
    let _ = session.close().await;
    clean(store).await;
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
