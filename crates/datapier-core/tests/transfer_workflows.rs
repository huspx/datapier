//! 通过完整文件消费路径验证 SQL 恢复预检及输入身份。
use datapier_core::{
    execution::sql_restore::{self, SqlKind},
    model::Value,
};
use std::{fs, path::PathBuf};
use uuid::Uuid;

async fn preparation_sink(store: &datapier_core::Store) -> datapier_core::ExecutionSink {
    let task = store
        .create_task(
            Uuid::new_v4(),
            "preparation-fixture",
            "transfer.prepare",
            serde_json::json!({}),
            false,
        )
        .await
        .unwrap();
    datapier_core::ExecutionSink::new(store.clone(), task.id)
}

const DUMP: &str = include_str!("fixtures/transfer_040/application_dump.sql.fixture");

struct InputFile(PathBuf);
impl InputFile {
    fn new(text: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("datapier-sql-workflow-{}.sql", Uuid::new_v4()));
        fs::write(&path, text).unwrap();
        Self(path)
    }
}
impl Drop for InputFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn indexed_insert_reuses_its_charset_mode_delimiter_and_absolute_offset() {
    let script = "SET NAMES utf8mb4;\nDELIMITER $$\nINSERT INTO t(id,label) VALUES(1,'first')$$\nSET SQL_MODE='NO_BACKSLASH_ESCAPES'$$\nINSERT INTO t(id,label) VALUES(2,'中😀\\tail')$$\n";
    for utf16 in [false, true] {
        let input = InputFile::new(script);
        if utf16 {
            let mut bytes = vec![0xff, 0xfe];
            bytes.extend(script.encode_utf16().flat_map(u16::to_le_bytes));
            fs::write(&input.0, bytes).unwrap();
        }
        let index = sql_restore::inspect(&input.0).unwrap();
        let (number, statement) = index
            .statements
            .iter()
            .enumerate()
            .filter(|(_, s)| s.kind == SqlKind::Insert)
            .nth(1)
            .unwrap();
        let mut rows = Vec::new();
        sql_restore::read_insert_statement(&input.0, &index, number, |columns, values, offset| {
            assert_eq!(columns.unwrap(), &["id", "label"]);
            assert_eq!(Some(offset), statement.tuple_start);
            rows.push(values);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            rows,
            vec![vec![
                Value::Integer("2".into()),
                Value::Text(r"中😀\tail".into())
            ]]
        );
    }
}

#[test]
fn insert_keeps_timezone_context_for_its_own_statement() {
    let input = InputFile::new(
        "SET TIME_ZONE='+02:00'; INSERT INTO t VALUES ('2025-01-01 00:00:00'); SET TIME_ZONE='+00:00'; INSERT INTO t VALUES ('2025-01-01 00:00:00');",
    );
    let index = sql_restore::inspect(&input.0).unwrap();
    let inserts: Vec<_> = index
        .statements
        .iter()
        .filter(|s| s.kind == SqlKind::Insert)
        .collect();
    assert_eq!(inserts.len(), 2);
    assert!(
        inserts[0]
            .context
            .settings
            .iter()
            .any(|s| s.contains("+02:00"))
    );
    assert!(
        inserts[1]
            .context
            .settings
            .last()
            .is_some_and(|s| s.contains("+00:00"))
    );
}

#[test]
fn actual_mysqldump_constant_view_stub_is_not_a_final_definition() {
    let input = InputFile::new(
        "USE app; DROP TABLE IF EXISTS v; DROP VIEW IF EXISTS v; CREATE VIEW v AS SELECT 1 AS id,1 AS label; DROP VIEW IF EXISTS v; CREATE VIEW v AS SELECT items.id,items.label FROM items; CREATE VIEW final_constant AS SELECT 1 AS id;",
    );
    let index = sql_restore::inspect(&input.0).unwrap();
    let views: Vec<_> = index
        .statements
        .iter()
        .filter(|s| s.kind == SqlKind::View)
        .collect();
    assert!(views[0].placeholder_view);
    assert!(!views[1].placeholder_view);
    assert!(!views[2].placeholder_view);
}

#[test]
fn numeric_literals_keep_unsigned_and_decimal_ranges() {
    let input = InputFile::new(
        "INSERT INTO t VALUES (9223372036854775807,9223372036854775808,18446744073709551615,18446744073709551616,-9223372036854775809);",
    );
    let index = sql_restore::inspect(&input.0).unwrap();
    let mut rows = Vec::new();
    sql_restore::read_inserts(&input.0, &index, "t", |_, row, _| {
        rows.push(row);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer("9223372036854775807".into()),
            Value::Unsigned("9223372036854775808".into()),
            Value::Unsigned("18446744073709551615".into()),
            Value::Decimal("18446744073709551616".into()),
            Value::Decimal("-9223372036854775809".into())
        ]]
    );
}

#[test]
fn routine_types_and_table_column_lists_are_not_function_dependencies() {
    let input = InputFile::new(
        "USE app;\nDELIMITER $$\nCREATE FUNCTION f(x VARCHAR(20)) RETURNS INT(11) DETERMINISTIC RETURN 1$$\nCREATE PROCEDURE p(IN id INT(11)) BEGIN DECLARE amount DECIMAL(10,2); INSERT INTO audit(id) VALUES(id); CALL worker(id); SET amount=external_fn(id); SELECT ext.INT(id); END$$\nDELIMITER ;\n",
    );
    let index = sql_restore::inspect(&input.0).unwrap();
    let function = index
        .statements
        .iter()
        .find(|item| item.kind == SqlKind::Function)
        .unwrap();
    assert!(!function.unresolved_references);
    assert!(function.function_calls.is_empty());
    let procedure = index
        .statements
        .iter()
        .find(|item| item.kind == SqlKind::Procedure)
        .unwrap();
    assert_eq!(procedure.function_calls, vec!["external_fn", "ext.INT"]);
    assert!(procedure.references.contains(&"audit".to_owned()));
    assert!(procedure.references.contains(&"worker".to_owned()));
    assert!(procedure.unresolved_references);
}

#[test]
fn mysqldump_object_mode_restore_keeps_the_global_escape_mode() {
    let input = InputFile::new(
        r#"SET SQL_MODE='NO_BACKSLASH_ESCAPES';
SET @OLD_SQL_MODE=@@SQL_MODE;
SET SQL_MODE='';
SET @saved_sql_mode=@@sql_mode;
SET SQL_MODE='ANSI_QUOTES';
SET SQL_MODE=@saved_sql_mode;
INSERT INTO t VALUES ('a\'b');
SET SQL_MODE=@OLD_SQL_MODE;
INSERT INTO t VALUES ('a\b');
"#,
    );
    let index = sql_restore::inspect(&input.0).unwrap();
    let mut rows = Vec::new();
    sql_restore::read_inserts(&input.0, &index, "t", |_, values, _| {
        rows.push(values);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        rows,
        vec![
            vec![Value::Text("a'b".into())],
            vec![Value::Text(r"a\b".into())]
        ]
    );
}

#[test]
fn view_qualifiers_follow_renamed_tables_but_preserve_explicit_aliases() {
    use datapier_core::model::transfer::{DefinerMapping, TransferObject};
    let mappings: Vec<TransferObject> = serde_json::from_value(serde_json::json!([
        {"source":{"database":"app","name":"items","kind":"table"},"target":{"database":"dest","name":"new_items","kind":"table"}},
        {"source":{"database":"app","name":"v","kind":"view"},"target":{"database":"dest","name":"new_view","kind":"view"}}
    ])).unwrap();
    for column in ["id", "*"] {
        for alias in ["", " AS `items`"] {
            let input = InputFile::new(&format!(
                "USE app; CREATE VIEW `v` AS SELECT `items`.{column} FROM `items`{alias};"
            ));
            let index = sql_restore::inspect(&input.0).unwrap();
            let (number, statement) = index
                .statements
                .iter()
                .enumerate()
                .find(|(_, s)| s.kind == SqlKind::View)
                .unwrap();
            let ddl = sql_restore::read_ddl(&input.0, &index, number).unwrap();
            let rewritten =
                sql_restore::rewrite_ddl(&ddl, statement, &mappings, &DefinerMapping::CurrentUser)
                    .unwrap();
            assert!(rewritten.contains("VIEW `dest`.`new_view`"));
            assert!(rewritten.contains("FROM `dest`.`new_items`"));
            let qualifier = if alias.is_empty() {
                "`dest`.`new_items`"
            } else {
                "`items`"
            };
            assert!(
                rewritten.contains(&format!("SELECT {qualifier}.{column}")),
                "{rewritten}"
            );
            if !alias.is_empty() {
                assert!(rewritten.contains("AS `items`"));
            }
        }
    }
}

#[test]
fn complete_dump_preserves_data_objects_and_database_context() {
    let input = InputFile::new(DUMP);
    let index = sql_restore::inspect(&input.0).unwrap();
    assert!(!index.datapier_format);
    let mut rows = Vec::new();
    sql_restore::read_inserts(&input.0, &index, "items", |columns, values, offset| {
        rows.push((columns.map(<[String]>::to_vec), values, offset));
        Ok(())
    })
    .unwrap();
    assert_eq!(rows.len(), 3);
    assert!(rows[0].0.is_none());
    assert_eq!(
        rows[0].1,
        vec![
            Value::Integer("1".into()),
            Value::Text("alpha; beta".into()),
            Value::Bytes("AP8=".into())
        ]
    );
    assert_eq!(
        rows[1].1,
        vec![
            Value::Integer("2".into()),
            Value::Text("quote's".into()),
            Value::Null
        ]
    );
    assert_eq!(rows[2].0.as_ref().unwrap(), &["id", "label", "payload"]);
    assert!(rows.windows(2).all(|w| w[0].2 < w[1].2));

    for kind in [
        SqlKind::Trigger,
        SqlKind::Procedure,
        SqlKind::Function,
        SqlKind::Event,
    ] {
        let (position, statement) = index
            .statements
            .iter()
            .enumerate()
            .find(|(_, s)| s.kind == kind)
            .unwrap();
        let context = statement.database_context.as_ref().unwrap();
        assert_eq!(context.database, "app");
        assert!(index.statements[context.before_statement].alters_database_defaults);
        assert!(index.statements[context.restore_statement].alters_database_defaults);
        assert!(context.before_statement < position && position < context.restore_statement);
        let ddl = sql_restore::read_ddl(&input.0, &index, position).unwrap();
        assert!(ddl.contains("BEGIN") && ddl.contains("END"));
        if kind == SqlKind::Trigger {
            assert_eq!(statement.version, Some(50003));
        }
        if kind == SqlKind::Procedure {
            assert!(ddl.contains("inside;procedure"));
        }
    }
}

#[test]
fn full_scan_rejects_unknown_or_truncated_tail_before_consumption() {
    for tail in [
        "CALL dangerous();\n",
        "SET GLOBAL sql_log_bin=0;\n",
        "INSERT INTO `items` VALUES (4,'unfinished'",
        "CREATE TABLE `x` (id INT);\nCALL p()",
    ] {
        let input = InputFile::new(&format!("{DUMP}{tail}"));
        assert!(
            sql_restore::inspect(&input.0).is_err(),
            "尾部必须拒绝: {tail}"
        );
    }
}

#[test]
fn indexed_input_changed_without_size_change_is_rejected() {
    let input = InputFile::new(DUMP);
    let index = sql_restore::inspect(&input.0).unwrap();
    let changed = DUMP.replacen("alpha; beta", "omega; beta", 1);
    assert_eq!(changed.len(), DUMP.len());
    fs::write(&input.0, changed).unwrap();
    let mut received = 0;
    assert!(
        sql_restore::read_inserts(&input.0, &index, "items", |_, _, _| {
            received += 1;
            Ok(())
        })
        .is_err()
    );
    assert_eq!(received, 0);
    let table = index
        .statements
        .iter()
        .position(|s| s.kind == SqlKind::Table)
        .unwrap();
    assert!(sql_restore::read_ddl(&input.0, &index, table).is_err());
}

#[test]
fn datapier_backup_requires_complete_marker_and_matching_bytes() {
    use sha2::{Digest, Sha256};
    let body = format!(
        "{}-- datapier-objects count=1\n-- datapier-object kind=table database=74657374 name=74 schema=1 data=1\nUSE `test`;\nCREATE TABLE `t` (`id` int);\nINSERT INTO `t` VALUES (7);\n",
        sql_restore::backup_header()
    );
    let footer = sql_restore::backup_footer(&Sha256::digest(body.as_bytes()));
    let complete = format!("{body}{footer}");
    let input = InputFile::new(&complete);
    let index = sql_restore::inspect(&input.0).unwrap();
    assert!(index.datapier_format);
    let mut rows = Vec::new();
    sql_restore::read_inserts(&input.0, &index, "t", |_, values, _| {
        rows.push(values);
        Ok(())
    })
    .unwrap();
    assert_eq!(rows, vec![vec![Value::Integer("7".into())]]);
    fs::write(&input.0, &body).unwrap();
    assert!(sql_restore::inspect(&input.0).is_err());
    fs::write(&input.0, complete.replacen("VALUES (7)", "VALUES (8)", 1)).unwrap();
    assert!(sql_restore::inspect(&input.0).is_err());
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_MYSQL_PORT、DATAPIER_TEST_MYSQL_SECRET、隔离 MySQL 及已接入的 0.4.0 配置库 schema"]
async fn mysql_overwrite_secondary_unique_conflict_rolls_back_batch_and_can_run_again() {
    use datapier_core::{
        ExecutionSink, Store,
        execution::migration,
        model::{
            AccessMode, ConnectionConfig, DirectConnection, Profile, ProviderKind, TlsConfig,
            transfer::TransferSpec,
        },
    };
    use serde_json::json;
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use tokio_util::sync::CancellationToken;

    let port = std::env::var("DATAPIER_TEST_MYSQL_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
    let pool = MySqlPoolOptions::new()
        .max_connections(3)
        .connect_with(
            MySqlConnectOptions::new()
                .ssl_mode(sqlx::mysql::MySqlSslMode::Disabled)
                .host("127.0.0.1")
                .port(port)
                .username("datapier")
                .password(&secret)
                .database("datapier_fixture"),
        )
        .await
        .unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let source = format!("dp_src_{suffix}");
    let target = format!("dp_dst_{suffix}");
    let root = std::env::temp_dir().join(format!("datapier-migration-workflow-{suffix}"));
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: format!("migration-{suffix}"),
        revision: 1,
        approval_policy: Default::default(),
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            redis_db: 0,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    store
        .save_profile(&profile, Some(secret), false)
        .await
        .unwrap();
    let result = async {
        for table in [&source, &target] {
            sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE `{table}` (id INT PRIMARY KEY, email VARCHAR(64) NOT NULL UNIQUE, note VARCHAR(64) NOT NULL) ENGINE=InnoDB"))).execute(&pool).await.unwrap();
        }
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO `{source}` VALUES (1,'one','changed'),(2,'occupied','changed')"
        )))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO `{target}` VALUES (1,'one','original'),(3,'occupied','untouched')"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let spec: TransferSpec = serde_json::from_value(json!({
            "mode":"copy", "provider":"mysql",
            "source":{"profile_id":profile.id,"database":"datapier_fixture"},
            "destination":{"profile_id":profile.id,"database":"datapier_fixture"},
            "objects":[{"source":{"database":"datapier_fixture","name":source,"kind":"table"},"target":{"database":"datapier_fixture","name":target,"kind":"table"},"locator_key":["id"]}],
            "options":{"conflict":"overwrite","include_schema":false,"include_data":true}
        })).unwrap();
        let cancel = CancellationToken::new();
        let prepared = migration::prepare(&preparation_sink(&store).await, spec.clone(), &cancel)
            .await
            .unwrap();
        let owner = Uuid::new_v4();
        let task = store
            .create_task(owner, "migration-test", "transfer.execute", json!({}), true)
            .await
            .unwrap();
        let sink = ExecutionSink::new(store.clone(), task.id).with_owner(owner);
        assert!(
            migration::execute(
                &prepared,
                &sink,
                task.id,
                "migration-test",
                None,
                &[],
                &cancel
            )
            .await
            .is_err()
        );
        let rows: Vec<(i32, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id,email,note FROM `{target}` ORDER BY id"
        )))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                (1, "one".into(), "original".into()),
                (3, "occupied".into(), "untouched".into())
            ]
        );
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE `{source}` SET email='two' WHERE id=2"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let prepared = migration::prepare(&preparation_sink(&store).await, spec, &cancel)
            .await
            .unwrap();
        let task = store
            .create_task(owner, "migration-test", "transfer.execute", json!({}), true)
            .await
            .unwrap();
        let sink = ExecutionSink::new(store.clone(), task.id).with_owner(owner);
        migration::execute(
            &prepared,
            &sink,
            task.id,
            "migration-test",
            None,
            &[],
            &cancel,
        )
        .await
        .unwrap();
        let rows: Vec<(i32, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id,email,note FROM `{target}` ORDER BY id"
        )))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                (1, "one".into(), "changed".into()),
                (2, "two".into(), "changed".into()),
                (3, "occupied".into(), "untouched".into())
            ]
        );
    };
    // 即使断言失败也先释放测试表，避免共享隔离实例积累残留。
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    for table in [&source, &target] {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TABLE IF EXISTS `{table}`"
        )))
        .execute(&pool)
        .await;
    }
    pool.close().await;
    store.pool.close().await;
    let _ = tokio::fs::remove_dir_all(root).await;
    outcome.unwrap();
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_MYSQL_PORT、DATAPIER_TEST_MYSQL_SECRET 和隔离 MySQL"]
async fn mysql_sql_restore_batches_and_retries_only_selected_row_states() {
    use datapier_core::{
        ExecutionSink, Store,
        execution::migration,
        model::{
            AccessMode, ConnectionConfig, DirectConnection, Profile, ProviderKind, TlsConfig,
            transfer::{TransferSpec, TransferStepState},
        },
    };
    use serde_json::json;
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use tokio_util::sync::CancellationToken;

    let port = std::env::var("DATAPIER_TEST_MYSQL_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
    let pool = MySqlPoolOptions::new()
        .max_connections(3)
        .connect_with(
            MySqlConnectOptions::new()
                .ssl_mode(sqlx::mysql::MySqlSslMode::Disabled)
                .host("127.0.0.1")
                .port(port)
                .username("datapier")
                .password(&secret)
                .database("datapier_fixture"),
        )
        .await
        .unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let target = format!("dp_sql_batch_{suffix}");
    let root = std::env::temp_dir().join(format!("datapier-sql-batch-{suffix}"));
    let input = InputFile::new(
        "INSERT INTO `source_rows` (`id`, `email`) VALUES (1,'one'),(2,'occupied'),(3,'three');\n",
    );
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: format!("sql-batch-{suffix}"),
        revision: 1,
        approval_policy: Default::default(),
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            redis_db: 0,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    store
        .save_profile(&profile, Some(secret), false)
        .await
        .unwrap();
    let result = async {
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE `{target}` (id INT PRIMARY KEY, email VARCHAR(64) NOT NULL UNIQUE) ENGINE=InnoDB"))).execute(&pool).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO `{target}` VALUES (9,'occupied')"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let spec: TransferSpec = serde_json::from_value(json!({
            "mode":"sql_restore", "provider":"mysql",
            "destination":{"profile_id":profile.id,"database":"datapier_fixture"},
            "inputs":[{"path":input.0,"format":"sql","target":{"database":"datapier_fixture","name":target,"kind":"table"}}],
            "options":{"conflict":"insert","include_schema":false,"include_data":true}
        })).unwrap();
        let cancel = CancellationToken::new();
        let prepared = migration::prepare(&preparation_sink(&store).await, spec.clone(), &cancel)
            .await
            .unwrap();
        let owner = Uuid::new_v4();
        let task = store
            .create_task(owner, "sql-batch", "transfer.execute", json!({}), true)
            .await
            .unwrap();
        let sink = ExecutionSink::new(store.clone(), task.id).with_owner(owner);
        assert!(
            migration::execute(&prepared, &sink, task.id, "sql-batch", None, &[], &cancel)
                .await
                .is_err()
        );
        let items = store.transfer_items(task.id, None, 100).await.unwrap();
        let data: Vec<_> = items.iter().filter(|item| item.phase == "data").collect();
        assert_eq!(data.len(), 1);
        let batch = data[0];
        assert_eq!(
            batch.item_states,
            vec![
                TransferStepState::RolledBack,
                TransferStepState::Failed,
                TransferStepState::NotSent
            ]
        );
        assert_eq!(batch.locator.record_offsets.len(), 3);
        assert_eq!(
            batch.locator.byte_start,
            batch.locator.record_offsets.first().copied()
        );
        assert_eq!(
            batch.locator.byte_end,
            batch.locator.record_offsets.last().copied()
        );
        assert_eq!(batch.error_record, Some(batch.locator.record_offsets[1]));
        sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM `{target}`")))
            .execute(&pool)
            .await
            .unwrap();

        for (state, expected) in [
            (TransferStepState::Failed, 2),
            (TransferStepState::RolledBack, 1),
            (TransferStepState::NotSent, 3),
        ] {
            let mut retry = migration::prepare_retry(
                &preparation_sink(&store).await,
                spec.clone(),
                task.id,
                &cancel,
            )
            .await
            .unwrap();
            retry.retry_states = vec![state];
            retry.digest = migration::plan_digest(&retry).unwrap();
            let retry_task = store
                .create_task(owner, "sql-batch", "transfer.execute", json!({}), true)
                .await
                .unwrap();
            let retry_sink = ExecutionSink::new(store.clone(), retry_task.id).with_owner(owner);
            migration::execute(
                &retry,
                &retry_sink,
                retry_task.id,
                "sql-batch",
                Some(task.id),
                &[],
                &cancel,
            )
            .await
            .unwrap();
            let ids: Vec<i32> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT id FROM `{target}` ORDER BY id"
            )))
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(ids, vec![expected]);
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM `{target}`")))
                .execute(&pool)
                .await
                .unwrap();
        }

        // 模拟一次写入已发送但失去确认，另一条记录已明确标记为结果未知。
        // 第一条在目标库已有结果，第二条没有结果，续作都不得重放。
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO `{target}` VALUES (1,'one')"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let original = store.transfer_run(task.id).await.unwrap();
        let ancestor_task = store
            .create_task(owner, "sql-batch", "transfer.execute", json!({}), true)
            .await
            .unwrap();
        store
            .transfer_create_run(
                ancestor_task.id,
                ancestor_task.id,
                "sql-batch",
                &original.spec_digest,
                &original.context,
                Some(task.id),
            )
            .await
            .unwrap();
        for (ordinal, state) in [TransferStepState::Sent, TransferStepState::OutcomeUnknown]
            .into_iter()
            .enumerate()
        {
            let mut step = batch.clone();
            let offset = batch.locator.record_offsets[ordinal];
            step.ordinal = ordinal as u64;
            step.locator.first_record = None;
            step.locator.last_record = None;
            step.locator.byte_start = Some(offset);
            step.locator.byte_end = Some(offset);
            step.locator.record_offsets = vec![offset];
            step.state = TransferStepState::NotSent;
            step.affected = 0;
            step.item_states.clear();
            step.error_code = None;
            step.database_code = None;
            step.error_record = None;
            store
                .transfer_put_step(ancestor_task.id, &step)
                .await
                .unwrap();
            store
                .transfer_mark_sent(ancestor_task.id, ancestor_task.id, step.ordinal, true)
                .await
                .unwrap();
            if state == TransferStepState::OutcomeUnknown {
                step.state = state;
                store
                    .transfer_put_step(ancestor_task.id, &step)
                    .await
                    .unwrap();
            }
        }
        store
            .transfer_update_progress(
                ancestor_task.id,
                1,
                "outcome_unknown",
                &datapier_core::model::transfer::TransferProgress::default(),
            )
            .await
            .unwrap();

        let mut prior = ancestor_task.id;
        for expected_written in [1, 0] {
            let mut retry = migration::prepare_retry(
                &preparation_sink(&store).await,
                spec.clone(),
                prior,
                &cancel,
            )
            .await
            .unwrap();
            retry.retry_states = vec![TransferStepState::NotSent];
            retry.digest = migration::plan_digest(&retry).unwrap();
            let retry_task = store
                .create_task(owner, "sql-batch", "transfer.execute", json!({}), true)
                .await
                .unwrap();
            let retry_sink = ExecutionSink::new(store.clone(), retry_task.id).with_owner(owner);
            let error = migration::execute(
                &retry,
                &retry_sink,
                retry_task.id,
                "sql-batch",
                Some(prior),
                &[],
                &cancel,
            )
            .await
            .unwrap_err();
            assert_eq!(error.code, "outcome_unknown");
            let run = store.transfer_run(retry_task.id).await.unwrap();
            assert_eq!(run.state, "outcome_unknown");
            assert_eq!(run.progress.outcome_unknown, 2);
            assert_eq!(run.progress.written_rows, expected_written);
            let rows: Vec<(i32, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT id, email FROM `{target}` ORDER BY id"
            )))
            .fetch_all(&pool)
            .await
            .unwrap();
            assert_eq!(rows, vec![(1, "one".into()), (3, "three".into())]);
            prior = retry_task.id;
        }
    };
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TABLE IF EXISTS `{target}`"
    )))
    .execute(&pool)
    .await;
    pool.close().await;
    store.pool.close().await;
    drop(store);
    tokio::fs::remove_dir_all(root).await.unwrap();
    outcome.unwrap();
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_MYSQL_PORT、DATAPIER_TEST_MYSQL_SECRET 和隔离 MySQL"]
async fn mysql_sql_restore_object_selection_and_drift() {
    use datapier_core::{
        ExecutionSink, Store,
        execution::migration,
        model::{
            AccessMode, ConnectionConfig, DirectConnection, Profile, ProviderKind, TlsConfig,
            transfer::TransferSpec,
        },
    };
    use serde_json::json;
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use tokio_util::sync::CancellationToken;

    let port = std::env::var("DATAPIER_TEST_MYSQL_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
    let pool = MySqlPoolOptions::new()
        .max_connections(3)
        .connect_with(
            MySqlConnectOptions::new()
                .ssl_mode(sqlx::mysql::MySqlSslMode::Disabled)
                .host("127.0.0.1")
                .port(port)
                .username("datapier")
                .password(&secret)
                .database("datapier_fixture"),
        )
        .await
        .unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let a = format!("dp_select_a_{suffix}");
    let b = format!("dp_select_b_{suffix}");
    let sql = format!(
        "USE `datapier_fixture`;\nCREATE TABLE `{a}` (`id` INT PRIMARY KEY) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_general_ci;\nINSERT INTO `{a}` (`id`) VALUES (1);\nCREATE TABLE `{b}` (`id` INT PRIMARY KEY) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_general_ci;\nINSERT INTO `{b}` (`id`) VALUES (2);\n"
    );
    let input = InputFile::new(&sql);
    let root = std::env::temp_dir().join(format!("datapier-sql-selection-{suffix}"));
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: format!("sql-selection-{suffix}"),
        revision: 1,
        approval_policy: Default::default(),
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            redis_db: 0,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    store
        .save_profile(&profile, Some(secret), false)
        .await
        .unwrap();
    let result = async {
        let spec: TransferSpec = serde_json::from_value(json!({"mode":"sql_restore","provider":"mysql","destination":{"profile_id":profile.id,"database":"datapier_fixture"},"inputs":[{"path":input.0,"format":"sql"}],"options":{"create_missing":true}})).unwrap();
        let cancel = CancellationToken::new();
        let mut plan = migration::prepare(&preparation_sink(&store).await, spec, &cancel)
            .await
            .unwrap();
        assert_eq!(
            plan.items
                .iter()
                .filter(|item| item.action == "object_scope" && item.selected)
                .count(),
            2
        );
        for item in &mut plan.items {
            if item.source.as_ref().is_some_and(|source| source.name == a) {
                item.selected = false;
            }
        }
        plan.digest = migration::plan_digest(&plan).unwrap();
        let owner = Uuid::new_v4();
        let confirmations: Vec<_> = plan
            .items
            .iter()
            .filter(|item| item.selected && item.destructive)
            .map(|item| item.id.clone())
            .collect();
        fs::write(&input.0, format!("{sql}-- changed\n")).unwrap();
        let task = store
            .create_task(owner, "sql-selection", "transfer.execute", json!({}), true)
            .await
            .unwrap();
        let sink = ExecutionSink::new(store.clone(), task.id).with_owner(owner);
        assert!(
            migration::execute(
                &plan,
                &sink,
                task.id,
                "sql-selection",
                None,
                &confirmations,
                &cancel
            )
            .await
            .is_err()
        );
        fs::write(&input.0, &sql).unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "CREATE TABLE `{b}` (`id` INT PRIMARY KEY, `changed` INT)"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let task = store
            .create_task(owner, "sql-selection", "transfer.execute", json!({}), true)
            .await
            .unwrap();
        let sink = ExecutionSink::new(store.clone(), task.id).with_owner(owner);
        assert!(
            migration::execute(
                &plan,
                &sink,
                task.id,
                "sql-selection",
                None,
                &confirmations,
                &cancel
            )
            .await
            .is_err()
        );
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP TABLE `{b}`")))
            .execute(&pool)
            .await
            .unwrap();
        let task = store
            .create_task(owner, "sql-selection", "transfer.execute", json!({}), true)
            .await
            .unwrap();
        let sink = ExecutionSink::new(store.clone(), task.id).with_owner(owner);
        migration::execute(
            &plan,
            &sink,
            task.id,
            "sql-selection",
            None,
            &confirmations,
            &cancel,
        )
        .await
        .unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA='datapier_fixture' AND TABLE_NAME=?").bind(&a).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 0);
        let values: Vec<i32> =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT id FROM `{b}`")))
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(values, vec![2]);
    };
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    for table in [&a, &b] {
        let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
            "DROP TABLE IF EXISTS `{table}`"
        )))
        .execute(&pool)
        .await;
    }
    pool.close().await;
    store.pool.close().await;
    let _ = tokio::fs::remove_dir_all(root).await;
    outcome.unwrap();
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_MYSQL_PORT、DATAPIER_TEST_MYSQL_SECRET 和隔离 MySQL"]
async fn mysql_import_overwrite_with_only_existing_ids_skips_without_changes() {
    use datapier_core::{
        ExecutionSink, Store,
        execution::migration,
        model::{
            AccessMode, ConnectionConfig, DirectConnection, Profile, ProviderKind, TlsConfig,
            transfer::TransferSpec,
        },
    };
    use serde_json::json;
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use tokio_util::sync::CancellationToken;

    let port = std::env::var("DATAPIER_TEST_MYSQL_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let secret = std::env::var("DATAPIER_TEST_MYSQL_SECRET").unwrap();
    let pool = MySqlPoolOptions::new()
        .max_connections(3)
        .connect_with(
            MySqlConnectOptions::new()
                .ssl_mode(sqlx::mysql::MySqlSslMode::Disabled)
                .host("127.0.0.1")
                .port(port)
                .username("datapier")
                .password(&secret)
                .database("datapier_fixture"),
        )
        .await
        .unwrap();
    let suffix = Uuid::new_v4().simple().to_string();
    let target = format!("dp_ids_{suffix}");
    let root = std::env::temp_dir().join(format!("datapier-import-ids-{suffix}"));
    let input = InputFile::new("id\n1\n2\n");
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: format!("import-ids-{suffix}"),
        revision: 1,
        approval_policy: Default::default(),
        provider: ProviderKind::Mysql,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: "datapier".into(),
            database: Some("datapier_fixture".into()),
            redis_db: 0,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    store
        .save_profile(&profile, Some(secret), false)
        .await
        .unwrap();
    let result = async {
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE TABLE `{target}` (id INT PRIMARY KEY, note VARCHAR(64) NOT NULL DEFAULT 'new') ENGINE=InnoDB"))).execute(&pool).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT INTO `{target}` VALUES (1,'first'),(2,'second')"
        )))
        .execute(&pool)
        .await
        .unwrap();
        let spec: TransferSpec = serde_json::from_value(json!({
            "mode":"import", "provider":"mysql",
            "destination":{"profile_id":profile.id,"database":"datapier_fixture"},
            "inputs":[{"path":input.0,"format":"csv","target":{"database":"datapier_fixture","name":target,"kind":"table"}}],
            "options":{"conflict":"overwrite","include_schema":false,"include_data":true,"locator_key":["id"]}
        })).unwrap();
        let cancel = CancellationToken::new();
        let prepared = migration::prepare(&preparation_sink(&store).await, spec, &cancel)
            .await
            .unwrap();
        let owner = Uuid::new_v4();
        let task = store
            .create_task(owner, "migration-test", "transfer.execute", json!({}), true)
            .await
            .unwrap();
        let sink = ExecutionSink::new(store.clone(), task.id).with_owner(owner);
        let progress = migration::execute(
            &prepared,
            &sink,
            task.id,
            "migration-test",
            None,
            &[],
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(progress.written_rows, 0);
        assert_eq!(progress.skipped, 2);
        assert_eq!(progress.failed, 0);
        let rows: Vec<(i32, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT id,note FROM `{target}` ORDER BY id"
        )))
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(rows, vec![(1, "first".into()), (2, "second".into())]);
    };
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
        "DROP TABLE IF EXISTS `{target}`"
    )))
    .execute(&pool)
    .await;
    pool.close().await;
    store.pool.close().await;
    let _ = tokio::fs::remove_dir_all(root).await;
    outcome.unwrap();
}

#[tokio::test]
#[ignore = "需要 DATAPIER_TEST_REDIS_PORT、隔离 Redis 6.2+ 及已接入的 0.4.0 配置库 schema"]
async fn redis_overwrite_replaces_type_and_preserves_absolute_expiry() {
    use base64::{Engine as _, engine::general_purpose::STANDARD};
    use datapier_core::{
        ExecutionSink, Store,
        execution::migration,
        model::{
            AccessMode, ConnectionConfig, DirectConnection, Profile, ProviderKind, TlsConfig,
            transfer::TransferSpec,
        },
    };
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    let port: u16 = std::env::var("DATAPIER_TEST_REDIS_PORT")
        .unwrap()
        .parse()
        .unwrap();
    let client = redis::Client::open(format!("redis://127.0.0.1:{port}/15")).unwrap();
    let mut conn = client.get_multiplexed_async_connection().await.unwrap();
    let id = Uuid::new_v4().simple().to_string();
    let source_prefix = format!("datapier-test:{id}:source:");
    let target_prefix = format!("datapier-test:{id}:target:");
    let source = format!("{source_prefix}key");
    let target = format!("{target_prefix}key");
    let root = std::env::temp_dir().join(format!("datapier-redis-migration-{id}"));
    let store = Store::open(&root).await.unwrap();
    let profile = Profile {
        id: Uuid::new_v4(),
        name: format!("redis-migration-{id}"),
        revision: 1,
        approval_policy: Default::default(),
        provider: ProviderKind::Redis,
        access_mode: AccessMode::ReadWrite,
        connection: ConnectionConfig::Direct(DirectConnection {
            host: "127.0.0.1".into(),
            port,
            username: String::new(),
            database: None,
            redis_db: 15,
            redis_databases_override: None,
            tls: TlsConfig::default(),
            visible_databases: None,
            mysql: Default::default(),
        }),
    };
    store.save_profile(&profile, None, false).await.unwrap();
    let result = async {
        let _: () = redis::cmd("RPUSH")
            .arg(&source)
            .arg("first")
            .arg("second")
            .query_async(&mut conn)
            .await
            .unwrap();
        let _: bool = redis::cmd("PEXPIRE")
            .arg(&source)
            .arg(120_000)
            .query_async(&mut conn)
            .await
            .unwrap();
        let _: () = redis::cmd("SET")
            .arg(&target)
            .arg("old string")
            .query_async(&mut conn)
            .await
            .unwrap();
        let spec: TransferSpec = serde_json::from_value(json!({"mode":"copy","provider":"redis", "source":{"profile_id":profile.id,"redis_db":15}, "destination":{"profile_id":profile.id,"redis_db":15}, "redis":{"keys_b64":[STANDARD.encode(&source)],"source_prefix_b64":STANDARD.encode(&source_prefix),"target_prefix_b64":STANDARD.encode(&target_prefix)}, "options":{"conflict":"overwrite","include_schema":false,"include_data":true}})).unwrap();
        let cancel = CancellationToken::new();
        let prepared = migration::prepare(&preparation_sink(&store).await, spec, &cancel)
            .await
            .unwrap();
        let owner = Uuid::new_v4();
        let task = store
            .create_task(owner, "migration-test", "transfer.execute", json!({}), true)
            .await
            .unwrap();
        let sink = ExecutionSink::new(store.clone(), task.id).with_owner(owner);
        migration::execute(
            &prepared,
            &sink,
            task.id,
            "migration-test",
            None,
            &[],
            &cancel,
        )
        .await
        .unwrap();
        let values: Vec<String> = redis::cmd("LRANGE")
            .arg(&target)
            .arg(0)
            .arg(-1)
            .query_async(&mut conn)
            .await
            .unwrap();
        assert_eq!(values, vec!["first", "second"]);
        let source_ttl: i64 = redis::cmd("PTTL")
            .arg(&source)
            .query_async(&mut conn)
            .await
            .unwrap();
        let target_ttl: i64 = redis::cmd("PTTL")
            .arg(&target)
            .query_async(&mut conn)
            .await
            .unwrap();
        assert!(source_ttl > 0 && target_ttl > 0 && (source_ttl - target_ttl).abs() < 1500);
    };
    let outcome = futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(result)).await;
    let _: redis::RedisResult<i64> = redis::cmd("DEL")
        .arg(&source)
        .arg(&target)
        .query_async(&mut conn)
        .await;
    store.pool.close().await;
    let _ = tokio::fs::remove_dir_all(root).await;
    outcome.unwrap();
}
