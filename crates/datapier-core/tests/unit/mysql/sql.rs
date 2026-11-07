use super::*;
#[test]
fn transaction_control_requires_a_single_complete_statement() {
    for (sql, expected) in [
        ("BEGIN WORK;", TransactionControl::Begin),
        ("start transaction", TransactionControl::Begin),
        (" COMMIT\tWORK ; ", TransactionControl::Commit),
        ("ROLLBACK WORK", TransactionControl::Rollback),
        ("SET autocommit=1", TransactionControl::AutocommitOn),
        (
            "set SESSION autocommit = ON;",
            TransactionControl::AutocommitOn,
        ),
        (
            "SET @@session.autocommit=1",
            TransactionControl::AutocommitOn,
        ),
    ] {
        assert_eq!(transaction_control(sql), Some(expected), "{sql}");
    }
    for sql in [
        "COMMIT; INSERT INTO t VALUES (1)",
        "ROLLBACK; DROP TABLE t",
        "COMMIT;;",
        "COMMIT AND CHAIN",
        "/*! COMMIT */",
        "SET autocommit=1; DROP TABLE t",
        "SET GLOBAL autocommit=1",
        "SET autocommit=0",
        "SET autocommit=1, sql_mode=''",
        "SET autocommit=ON /* comment */",
    ] {
        assert_eq!(transaction_control(sql), None, "{sql}");
    }
}
#[test]
fn quoted_delimiters_and_comments() {
    let (first, rest) = next_statement("SELECT '中;😀', `x;y` /* ; */; SELECT 2", "").unwrap();
    assert_eq!(first, "SELECT '中;😀', `x;y` /* ; */");
    assert_eq!(rest.trim(), "SELECT 2");
    assert!(next_statement("SELECT 'unterminated", "").is_err());
}
#[test]
fn program_definitions_keep_the_full_input() {
    for sql in [
        "/* intro */ CREATE\nPROCEDURE p() BEGIN SELECT 1; SELECT 2; END;",
        "-- intro\nCREATE DEFINER=CURRENT_USER PROCEDURE p() BEGIN SELECT 1; END;",
        "# intro\rCREATE DEFINER = CURRENT_USER() FUNCTION f() RETURNS INT BEGIN RETURN 1; END;",
        "CREATE /*!50017 DEFINER=`root`@`localhost`*/ TRIGGER t BEFORE INSERT ON x FOR EACH ROW BEGIN SET @x=1; END;",
        "/*!50000 CREATE */ DEFINER='root'@'%' EVENT e ON SCHEDULE EVERY 1 DAY DO BEGIN SELECT 1; END;",
        "CREATE DEFINER = root @ db.example EVENT e ON SCHEDULE EVERY 1 DAY DO BEGIN SELECT 1; END;",
    ] {
        assert_eq!(next_statement(sql, "").unwrap(), (sql, ""), "{sql}");
    }
    let (first, rest) =
        next_statement("/* intro */ CREATE TABLE t (id INT); SELECT 2", "").unwrap();
    assert_eq!(first, "/* intro */ CREATE TABLE t (id INT)");
    assert_eq!(rest.trim(), "SELECT 2");
    assert!(next_statement("/* intro */ DELIMITER $$", "").is_err());
}
#[test]
fn dynamic_detection_skips_comments_but_reads_executable_sql() {
    for sql in [
        "/* report */ CALL p()",
        "-- report\nexecute stmt",
        "# report\rCALL p()",
        "/*!50000 CALL p() */",
        "/* report */ /*!50000 EXECUTE stmt */",
    ] {
        assert!(is_dynamic(sql), "{sql}");
    }
    for sql in [
        "/* CALL p() */ SELECT 1",
        "/*!50000 SELECT 1 */ CALL p()",
        "CALLER p()",
        "'CALL'",
        "/* unfinished",
    ] {
        assert!(!is_dynamic(sql), "{sql}");
    }
    assert_eq!(
        first_keyword("/* report */ begin"),
        Some("BEGIN".to_owned())
    );
    assert_eq!(first_keyword("# only a comment"), None);
}
#[test]
fn readonly_rejects_side_effects() {
    assert!(readonly("SELECT count(*) FROM t WHERE id = 1"));
    assert!(!readonly("SELECT secret_function()"));
    assert!(!readonly("SELECT 1 INTO OUTFILE '/tmp/a'"));
    assert!(!readonly("SELECT /*!50000 SLEEP(1) */ 1"));
    assert!(!readonly("SELECT @x := 1"));
}
#[test]
fn readonly_metadata_whitelist() {
    for sql in [
        "SHOW DATABASES",
        "SHOW SCHEMAS LIKE 'app%'",
        "SHOW FULL TABLES FROM app",
        "SHOW COLUMNS FROM app.users",
        "SHOW CREATE TABLE app.users",
        "SHOW CREATE VIEW `app`.`users`",
        "SHOW SESSION VARIABLES LIKE 'sql_mode'",
        "SHOW STATUS",
        "SHOW COLLATION",
        "SHOW CHARACTER SET",
        "SHOW PROCESSLIST",
        "DESCRIBE app.users",
        "DESC `app`.`users`",
        "EXPLAIN app.users",
        "EXPLAIN SELECT count(*) FROM users",
    ] {
        assert!(readonly(sql), "should allow: {sql}");
    }
    for sql in [
        "SHOW CREATE PROCEDURE p",
        "SHOW CREATE FUNCTION f",
        "SHOW CREATE TABLE f()",
        "SHOW TABLES FROM db LIMIT 1",
        "SHOW COLUMNS",
        "SHOW TABLES WHERE SLEEP(1)",
        "SHOW VARIABLES WHERE secret_function()",
        "SHOW VARIABLES WHERE @x := 1",
        "SHOW GRANTS FOR CURRENT_USER",
        "SHOW TABLES; DELETE FROM users",
        "SHOW TABLES /*!50000 INTO OUTFILE '/tmp/a' */",
        "DESCRIBE f()",
        "DESCRIBE app.users extra",
        "EXPLAIN ANALYZE SELECT 1",
        "EXPLAIN ANALYZE users",
        "EXPLAIN UPDATE users SET x = 1",
        "EXPLAIN ANALYZE DELETE FROM users",
        "EXPLAIN SELECT secret_function()",
        "EXPLAIN ANALYZE SELECT SLEEP(1)",
        "EXPLAIN SELECT /*!50000 SLEEP(1) */ 1",
        "EXPLAIN SELECT 1; DELETE FROM users",
        "EXPLAIN SELECT 1 INTO OUTFILE '/tmp/a'",
    ] {
        assert!(!readonly(sql), "should reject: {sql}");
    }
}

#[test]
fn stateless_ranges_and_nested_callable_safety() {
    let text = "  SELECT '中;😀'; ; SELECT count(*) FROM `表`  ";
    let ranges = stateless_statement_ranges(text).unwrap();
    assert_eq!(
        ranges.iter().map(|&(a, b)| &text[a..b]).collect::<Vec<_>>(),
        vec!["SELECT '中;😀'", "SELECT count(*) FROM `表`"]
    );
    for sql in [
        "SELECT \\\'x'",
        "SELECT \"x\"",
        "SELECT 1 /* x */",
        "SELECT 1 -- x",
        "DELIMITER $$",
    ] {
        assert!(stateless_statement_ranges(sql).is_err(), "{sql}");
    }
    for sql in [
        "SELECT 函数()",
        "SELECT `sleep`(1)",
        "SELECT db.count(*)",
        "WITH x AS (SELECT SLEEP(1)) SELECT * FROM x",
        "SELECT (SELECT secret())",
        "SELECT * FROM func()",
        "SELECT 1 FOR UPDATE",
        "EXPLAIN ANALYZE SELECT 1",
    ] {
        assert!(validate_readonly(sql).is_err(), "{sql}");
    }
    assert!(validate_readonly("WITH x AS (SELECT 1 AS n) SELECT count(*) FROM x").is_ok());
    assert!(validate_readonly("SHOW INDEX FROM `db`.`表`").is_ok());
    for sql in [
        "SHOW INDEX FROM `db#1`.`t@x--/*\"`",
        "SHOW FULL COLUMNS FROM `db#1`.`t@x--/*\"`",
        "SELECT '# @ -- /* \"'",
        "SELECT `a``#@` FROM `t--x`",
    ] {
        assert!(validate_readonly(sql).is_ok(), "{sql}");
    }
    for sql in [
        "SELECT 1 # comment",
        "SELECT @x",
        "SELECT 1 /* comment */",
        "SELECT \"x\"",
    ] {
        assert!(validate_readonly(sql).is_err(), "{sql}");
    }
    assert!(validate_readonly("SHOW INDEX FROM t WHERE SLEEP(1)").is_err());
    assert!(validate_readonly("SELECT 1; DELETE FROM t").is_err());
}

#[test]
fn readonly_rejects_mode_sensitive_function_spacing() {
    for sql in [
        "SELECT COUNT (1)",
        "SELECT COUNT\t(1)",
        "WITH x AS (SELECT TRIM\n(1)) SELECT * FROM x",
        "SELECT (SELECT MAX (v) FROM t)",
        "SHOW TABLES WHERE COUNT (1) = 1",
        "EXPLAIN SELECT SUM (1)",
    ] {
        assert!(validate_readonly(sql).is_err(), "should reject: {sql}");
    }
    for sql in [
        "SELECT COUNT(1), TRIM(1) FROM t",
        "WITH x AS (SELECT 1 AS v) SELECT COUNT(*) FROM x",
        "SELECT(1) AS v WHERE 1 IN (1, 2)",
        "SELECT 'COUNT (1)', `COUNT (1)` FROM t",
    ] {
        assert!(validate_readonly(sql).is_ok(), "should allow: {sql}");
    }
}
