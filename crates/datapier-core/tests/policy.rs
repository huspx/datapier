use datapier_core::policy::{SqlContext, analyze_mysql, analyze_view_ddl};

fn context() -> SqlContext {
    SqlContext {
        database: Some("main".into()),
        ..Default::default()
    }
}

#[test]
fn read_scope_and_replayability() {
    let access = analyze_mysql("SELECT x.id FROM a.items x JOIN b.items y ON x.id = y.id WHERE EXISTS (SELECT 1 FROM c.log)", &context()).unwrap();
    assert_eq!(
        access.read_databases.into_iter().collect::<Vec<_>>(),
        ["a", "b", "c"]
    );
    assert!(access.write_databases.is_empty());
    assert!(access.replayable);
    assert!(
        !analyze_mysql("SELECT RAND() FROM a.items", &context())
            .unwrap()
            .replayable
    );
    assert!(
        !analyze_mysql("SELECT * FROM a.items FOR UPDATE", &context())
            .unwrap()
            .replayable
    );
    assert!(
        analyze_mysql("SELECT 1", &SqlContext::default())
            .unwrap()
            .replayable
    );
}

#[test]
fn cte_scope_does_not_hide_qualified_or_forward_tables() {
    let access = analyze_mysql(
        "WITH t AS (SELECT * FROM source.base) SELECT * FROM t JOIN other.t ON 1=1",
        &context(),
    )
    .unwrap();
    assert_eq!(
        access.read_databases.into_iter().collect::<Vec<_>>(),
        ["other", "source"]
    );
    let access = analyze_mysql("WITH first AS (SELECT * FROM later), later AS (SELECT * FROM source.base) SELECT * FROM first", &context()).unwrap();
    assert_eq!(
        access.read_databases.into_iter().collect::<Vec<_>>(),
        ["main", "source"]
    );
    let access = analyze_mysql("WITH t AS (SELECT * FROM real.t) SELECT * FROM (WITH t AS (SELECT * FROM inner_db.data) SELECT * FROM t) x JOIN t ON 1=1", &context()).unwrap();
    assert_eq!(
        access.read_databases.into_iter().collect::<Vec<_>>(),
        ["inner_db", "real"]
    );
}

#[test]
fn show_create_view_reads_only_query_dependencies() {
    let sql = "CREATE ALGORITHM=UNDEFINED DEFINER=`owner`@`localhost` SQL SECURITY DEFINER VIEW `main`.`v` AS SELECT * FROM `source`.`t` JOIN local_t ON 1=1";
    let access = analyze_view_ddl(sql, &context()).unwrap();
    assert_eq!(
        access.read_databases.into_iter().collect::<Vec<_>>(),
        ["main", "source"]
    );
    assert!(access.write_databases.is_empty());
    assert!(access.objects.iter().all(|o| !o.write && o.name != "v"));
    assert!(!access.replayable);
    for sql in [
        "CREATE VIEW v AS SELECT @secret",
        "CREATE VIEW v AS SELECT * FROM information_schema.tables",
        "CREATE VIEW v AS SELECT untrusted_function()",
        "CREATE VIEW v AS SELECT 1; SELECT * FROM secret.t",
        "SELECT * FROM source.t",
    ] {
        assert!(analyze_view_ddl(sql, &context()).is_err(), "{sql}");
    }
}

#[test]
fn writes_and_cross_database_sources() {
    let insert = analyze_mysql("INSERT INTO dst.t SELECT * FROM src.t", &context()).unwrap();
    assert!(insert.write_databases.contains("dst"));
    assert!(insert.read_databases.contains("src"));
    assert!(!insert.replayable);
    let update = analyze_mysql(
        "UPDATE dst.t d JOIN src.t s ON d.id=s.id SET d.v=s.v WHERE d.id=1",
        &context(),
    )
    .unwrap();
    assert!(update.write_databases.contains("dst"));
    assert!(!update.write_databases.contains("src"));
    assert!(update.read_databases.contains("src"));
    let delete = analyze_mysql(
        "DELETE d FROM dst.t d JOIN src.t s ON d.id=s.id WHERE d.id=1",
        &context(),
    )
    .unwrap();
    assert!(delete.write_databases.contains("dst"));
    assert!(delete.read_databases.contains("src"));
    assert!(!delete.write_databases.contains("src"));
    assert!(
        analyze_mysql("TRUNCATE TABLE dst.t", &context())
            .unwrap()
            .dangerous
    );
    assert!(
        analyze_mysql("CREATE TABLE dst.t (id INT)", &context())
            .unwrap()
            .dangerous
    );
}

#[test]
fn trigger_statement_uses_object_database_and_checks_body() {
    let ctx = SqlContext {
        database: Some("trigger_db".into()),
        ..Default::default()
    };
    let insert = analyze_mysql("INSERT INTO audit (id) VALUES (NEW.id)", &ctx).unwrap();
    assert!(insert.write_databases.contains("trigger_db"));
    let update = analyze_mysql(
        "UPDATE other.counters SET n = n + 1 WHERE id = NEW.id",
        &ctx,
    )
    .unwrap();
    assert!(update.write_databases.contains("other"));
    assert!(analyze_mysql("BEGIN INSERT INTO hidden.t VALUES (NEW.id); END", &ctx).is_err());
}

#[test]
fn batch_use_and_transactions() {
    let access = analyze_mysql("USE other; SELECT * FROM t", &context()).unwrap();
    assert_eq!(access.change_database.as_deref(), Some("other"));
    assert!(access.read_databases.contains("other"));
    assert!(!access.replayable);
    for sql in ["BEGIN", "COMMIT", "ROLLBACK"] {
        assert!(
            analyze_mysql(sql, &SqlContext::default())
                .unwrap()
                .objects
                .is_empty()
        );
    }
    let batch = analyze_mysql(
        "BEGIN; UPDATE t SET n=1; COMMIT; BEGIN; TRUNCATE TABLE t; ROLLBACK",
        &context(),
    )
    .unwrap();
    assert_eq!(
        batch.transaction_actions,
        ["begin", "commit", "begin", "implicit_commit", "rollback"]
    );
    assert!(analyze_mysql("ROLLBACK TO SAVEPOINT s", &context()).is_err());
    assert!(analyze_mysql("COMMIT AND CHAIN", &context()).is_err());
    assert!(analyze_mysql("SET sql_mode='ANSI_QUOTES'; SELECT * FROM t", &context()).is_err());
}

#[test]
fn unknown_scope_metadata_and_parser_differences_fail_closed() {
    for sql in [
        "SELECT * FROM information_schema.tables",
        "SELECT * FROM mysql.user",
        "SHOW DATABASES",
        "PREPARE s FROM 'SELECT 1'",
        "EXECUTE s",
        "CALL p()",
        "SELECT dangerous_function()",
        "SELECT @x",
        "SELECT * FROM t; SET @x=1",
        "SELECT * FROM t /*! UNION SELECT * FROM secret.t */",
        "SELECT * FROM JSON_TABLE('[]', '$' COLUMNS (n INT PATH '$')) AS j",
        "SELECT * FROM t INTO OUTFILE '/tmp/out'",
        "SELECT * FROM t WHERE id = \\",
    ] {
        assert!(analyze_mysql(sql, &context()).is_err(), "{sql}");
    }
    assert!(analyze_mysql("SELECT * FROM t", &SqlContext::default()).is_err());
    for ansi in [false, true] {
        let ctx = SqlContext {
            ansi_quotes: ansi,
            no_backslash_escapes: true,
            ..context()
        };
        assert!(analyze_mysql("SELECT * FROM \"secret\".t", &ctx).is_err());
    }
}

#[test]
fn names_follow_server_case_mode_and_explain_checks_target() {
    let ctx = SqlContext {
        lower_case_table_names: 1,
        ..context()
    };
    let access = analyze_mysql("SELECT * FROM Sales.Orders", &ctx).unwrap();
    assert!(access.read_databases.contains("sales"));
    assert_eq!(access.objects[0].name, "orders");
    let explain = analyze_mysql("EXPLAIN SELECT * FROM secret.t", &context()).unwrap();
    assert!(explain.read_databases.contains("secret"));
    assert!(!explain.replayable);
}
