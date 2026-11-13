use super::*;
use crate::model::ResourceTarget;

#[test]
fn mysql_engine_is_typed_and_other_table_options_are_preserved() {
    use sqlparser::{ast::Statement, dialect::MySqlDialect, parser::Parser};
    let mut parsed = Parser::parse_sql(
        &MySqlDialect {},
        "CREATE TABLE t (id INT NOT NULL) ENGINE=InnoDB ROW_FORMAT=COMPRESSED",
    )
    .unwrap();
    let Statement::CreateTable(table) = parsed.remove(0) else {
        panic!("expected table")
    };
    let options = super::super::catalog::table_options(table.table_options).unwrap();
    assert!(
        options
            .iter()
            .all(|option| !option.to_ascii_uppercase().starts_with("ENGINE")),
        "{options:?}"
    );
    assert!(
        options
            .iter()
            .any(|option| option.to_ascii_uppercase().starts_with("ROW_FORMAT")),
        "{options:?}"
    );
}
fn target(kind: &str) -> ResourceTarget {
    ResourceTarget {
        database: Some("库".into()),
        name: "a`b".into(),
        key_b64: None,
        kind: kind.into(),
    }
}
#[test]
fn database_creation_quotes_name_and_binds_authorization_target() {
    let mut intent = SchemaEditIntent {
        target: ResourceTarget {
            database: Some("a`b".into()),
            name: "a`b".into(),
            key_b64: None,
            kind: "database".into(),
        },
        baseline: None,
        definition: Some(SchemaDefinition::Database(DatabaseDefinition {
            charset: None,
            collation: None,
        })),
        profile_revision: "r1".into(),
        algorithm: None,
        lock: None,
        server_version: "8.0.36".into(),
        sql_mode: String::new(),
    };
    let plan = build_plan(&intent).unwrap();
    assert_eq!(plan.steps[0].sql, "CREATE DATABASE `a``b`");
    assert_eq!(plan.steps[0].precondition, SchemaPrecondition::Absent);
    intent.target.database = Some("other".into());
    assert!(build_plan(&intent).is_err());
}

fn view() -> ViewDefinition {
    ViewDefinition {
        select: "SELECT 1 AS x".into(),
        columns: vec!["x".into()],
        algorithm: "MERGE".into(),
        definer: Some("u@host".into()),
        security: "INVOKER".into(),
        check_option: Some("LOCAL".into()),
        character_set_client: None,
        collation_connection: None,
    }
}
#[test]
fn view_creation_quotes_target_and_preserves_attributes() {
    let intent = SchemaEditIntent {
        target: target("view"),
        baseline: None,
        definition: Some(SchemaDefinition::View(view())),
        profile_revision: "r1".into(),
        algorithm: None,
        lock: None,
        server_version: "8.0.36".into(),
        sql_mode: String::new(),
    };
    let plan = build_plan(&intent).unwrap();
    assert_eq!(plan.steps.len(), 1);
    assert!(plan.steps[0].sql.contains("`库`.`a``b`"));
    assert!(plan.steps[0].sql.contains("SQL SECURITY INVOKER"));
    assert!(plan.steps[0].sql.ends_with("WITH LOCAL CHECK OPTION"));
    assert_eq!(plan.plan_digest, build_plan(&intent).unwrap().plan_digest);
}
#[test]
fn incomplete_baseline_never_produces_ddl() {
    let t = target("view");
    let baseline = SchemaSnapshot {
        target: t.clone(),
        server_version: "8.0.36".into(),
        sql_mode: String::new(),
        definition: SchemaDefinition::View(view()),
        ddl: "CREATE VIEW v AS SELECT 1".into(),
        dependencies: vec![],
        completeness: Completeness {
            state: CompletenessState::Incomplete,
            reasons: vec!["no metadata".into()],
        },
        fingerprint: "a".into(),
        capabilities: Default::default(),
    };
    let intent = SchemaEditIntent {
        target: t,
        baseline: Some(baseline),
        definition: Some(SchemaDefinition::View(view())),
        profile_revision: "r1".into(),
        algorithm: None,
        lock: None,
        server_version: "8.0.36".into(),
        sql_mode: String::new(),
    };
    assert!(build_plan(&intent).is_err());
}

#[test]
fn rename_keeps_column_identity_and_uses_alter() {
    let column = ColumnDefinition {
        id: "column-1".into(),
        name: "old".into(),
        native_type: "varchar(20)".into(),
        nullable: false,
        default: ColumnDefault::Literal("中'文".into()),
        on_update: None,
        auto_increment: false,
        generated: None,
        charset: None,
        collation: None,
        visible: true,
        comment: "note".into(),
        extra: vec![],
    };
    let table = TableDefinition {
        columns: vec![column],
        indexes: vec![],
        constraints: vec![],
        engine: "InnoDB".into(),
        charset: None,
        collation: None,
        comment: String::new(),
        options: vec![],
    };
    let mut changed = table.clone();
    changed.columns[0].name = "new".into();
    let target = target("table");
    let baseline = SchemaSnapshot {
        target: target.clone(),
        server_version: "8.0.36".into(),
        sql_mode: String::new(),
        definition: SchemaDefinition::Table(table),
        ddl: "CREATE TABLE t (old varchar(20))".into(),
        dependencies: vec![],
        completeness: Completeness {
            state: CompletenessState::Complete,
            reasons: vec![],
        },
        fingerprint: "abc".into(),
        capabilities: Default::default(),
    };
    let intent = SchemaEditIntent {
        target,
        baseline: Some(baseline),
        definition: Some(SchemaDefinition::Table(changed)),
        profile_revision: "r".into(),
        algorithm: None,
        lock: None,
        server_version: "8.0.36".into(),
        sql_mode: String::new(),
    };
    let plan = build_plan(&intent).unwrap();
    assert!(plan.steps[0].sql.contains("CHANGE COLUMN `old` `new`"));
    assert!(plan.steps[0].sql.contains("DEFAULT '中''文'"));
    assert!(!plan.steps[0].sql.contains("DROP COLUMN"));
}

#[test]
fn fingerprint_ignores_sequence_but_preserves_literal_whitespace() {
    let ddl = "CREATE TABLE `t` (`c` varchar(20) DEFAULT 'a  b') ENGINE=InnoDB AUTO_INCREMENT=12";
    let changed_counter = ddl.replace("AUTO_INCREMENT=12", "AUTO_INCREMENT=13");
    let changed_default = ddl.replace("a  b", "a b");
    assert_eq!(
        super::super::fingerprint(ddl, ""),
        super::super::fingerprint(&changed_counter, "")
    );
    assert_ne!(
        super::super::fingerprint(ddl, ""),
        super::super::fingerprint(&changed_default, "")
    );
}

#[test]
fn functional_index_requires_mysql_8013() {
    let column = ColumnDefinition {
        id: "c".into(),
        name: "c".into(),
        native_type: "int".into(),
        nullable: false,
        default: ColumnDefault::Unspecified,
        on_update: None,
        auto_increment: false,
        generated: None,
        charset: None,
        collation: None,
        visible: true,
        comment: String::new(),
        extra: vec![],
    };
    let index = IndexDefinition {
        id: "i".into(),
        name: "i".into(),
        kind: IndexKind::Normal,
        parts: vec![IndexPart {
            column: None,
            expression: Some("c + 1".into()),
            prefix_length: None,
            descending: false,
        }],
        visible: true,
        method: None,
        options: vec![],
    };
    let table = TableDefinition {
        columns: vec![column],
        indexes: vec![index],
        constraints: vec![],
        engine: "InnoDB".into(),
        charset: None,
        collation: None,
        comment: String::new(),
        options: vec![],
    };
    let mut intent = SchemaEditIntent {
        target: target("table"),
        baseline: None,
        definition: Some(SchemaDefinition::Table(table)),
        profile_revision: "r".into(),
        algorithm: None,
        lock: None,
        server_version: "8.0.12".into(),
        sql_mode: String::new(),
    };
    assert!(build_plan(&intent).is_err());
    intent.server_version = "8.0.13".into();
    assert!(build_plan(&intent).is_ok());
}

#[test]
fn version_comments_expand_only_when_applicable() {
    let ddl = "CREATE TABLE t (c int, CONSTRAINT ck CHECK (c > 0) /*!80016 NOT ENFORCED */) COMMENT='/*!80016 literal */'";
    let old = super::super::catalog::expand_version_comments(ddl, "5.7.44", "").unwrap();
    let new = super::super::catalog::expand_version_comments(ddl, "8.0.36", "").unwrap();
    assert!(!old.contains("NOT ENFORCED"));
    assert!(new.contains("NOT ENFORCED"));
    assert!(new.contains("'/*!80016 literal */'"));
}

#[test]
fn no_backslash_mode_keeps_literal_backslashes_for_parser() {
    let ddl = "CREATE TABLE t (c varchar(20) DEFAULT 'a\\b')";
    let expanded =
        super::super::catalog::expand_version_comments(ddl, "8.0.36", "NO_BACKSLASH_ESCAPES")
            .unwrap();
    assert!(expanded.contains("'a\\\\b'"));
}

#[test]
fn view_readback_normalizes_server_qualification_but_not_semantics() {
    let mut expected = view();
    expected.select = "SELECT t.id FROM t WHERE t.id > 1".into();
    expected.columns = vec!["id".into()];
    let mut actual = expected.clone();
    actual.select = "select `db`.`t`.`id` AS `id` from `db`.`t` where (`db`.`t`.`id` > 1)".into();
    assert!(super::super::verify::views_equal(&expected, &actual, "db"));
    actual.select = actual.select.replace("> 1", "> 2");
    assert!(!super::super::verify::views_equal(&expected, &actual, "db"));
}

#[test]
fn index_comment_preserves_sql_punctuation_without_extra_statements() {
    let index = IndexDefinition {
        id: "i".into(),
        name: "i".into(),
        kind: IndexKind::Normal,
        parts: vec![IndexPart {
            column: Some("c".into()),
            expression: None,
            prefix_length: Some(5),
            descending: true,
        }],
        visible: false,
        method: Some("BTREE".into()),
        options: vec!["COMMENT 'tag # -- ''quoted'''".into()],
    };
    let rendered = super::index(&index, "").unwrap();
    assert!(rendered.contains("`c`(5) DESC"));
    assert!(rendered.contains("INVISIBLE COMMENT 'tag # -- ''quoted'''"));
    let mut invalid = index;
    invalid.options = vec!["COMMENT 'ok' DROP INDEX other".into()];
    assert!(super::index(&invalid, "").is_err());
}

#[test]
fn trigger_readback_accepts_server_defaults_but_checks_explicit_properties() {
    let mut expected = TriggerDefinition {
        database: "db".into(),
        name: "tr".into(),
        table: "t".into(),
        timing: "BEFORE".into(),
        event: "INSERT".into(),
        body: "SET NEW.a = 1".into(),
        definer: None,
        sql_mode: String::new(),
        character_set_client: None,
        collation_connection: None,
        order: None,
    };
    let mut actual = expected.clone();
    actual.definer = Some("u@localhost".into());
    actual.character_set_client = Some("utf8mb4".into());
    actual.collation_connection = Some("utf8mb4_general_ci".into());
    assert!(super::super::verify::triggers_equal(&expected, &actual));
    expected.definer = Some("other@localhost".into());
    assert!(!super::super::verify::triggers_equal(&expected, &actual));
    expected.definer = actual.definer.clone();
    actual.body = "SET NEW.a = 2".into();
    assert!(!super::super::verify::triggers_equal(&expected, &actual));
}

#[test]
fn unknown_server_context_allows_only_conservative_local_drafts() {
    let intent = SchemaEditIntent {
        target: target("view"),
        baseline: None,
        definition: Some(SchemaDefinition::View(view())),
        profile_revision: "r".into(),
        algorithm: None,
        lock: None,
        server_version: String::new(),
        sql_mode: String::new(),
    };
    let plan = build_plan(&intent).unwrap();
    assert_eq!(
        plan.steps[0].sql,
        "CREATE ALGORITHM = MERGE DEFINER = `u`@`host` SQL SECURITY INVOKER VIEW `库`.`a``b` (`x`) AS SELECT 1 AS x\nWITH LOCAL CHECK OPTION"
    );
    let definition = TableDefinition {
        columns: vec![],
        indexes: vec![],
        constraints: vec![ConstraintDefinition::Check {
            id: "c".into(),
            name: "c".into(),
            expression: "1 > 0".into(),
            enforced: true,
        }],
        engine: "InnoDB".into(),
        charset: None,
        collation: None,
        comment: String::new(),
        options: vec![],
    };
    assert!(validate_features(&SchemaDefinition::Table(definition), "").is_err());
}

#[test]
fn table_readback_uses_creation_defaults_and_preserves_differences() {
    let column = ColumnDefinition {
        id: "draft".into(),
        name: "id".into(),
        native_type: "int".into(),
        nullable: true,
        default: ColumnDefault::Unspecified,
        on_update: None,
        auto_increment: false,
        generated: None,
        charset: None,
        collation: None,
        visible: true,
        comment: String::new(),
        extra: vec![],
    };
    let expected = TableDefinition {
        columns: vec![column],
        indexes: vec![],
        constraints: vec![],
        engine: "InnoDB".into(),
        charset: None,
        collation: None,
        comment: String::new(),
        options: vec![],
    };
    let defaults: (String, String) = ("utf8mb4".into(), "utf8mb4_general_ci".into());
    let mut actual = expected.clone();
    actual.charset = Some(defaults.0.clone());
    actual.collation = Some(defaults.1.clone());
    actual.columns[0].native_type = "INT(11)".into();
    actual.columns[0].default = ColumnDefault::Null;
    let compare = |a: &TableDefinition, b: &TableDefinition| {
        super::super::equivalent(
            &SchemaDefinition::Table(a.clone()),
            &SchemaDefinition::Table(b.clone()),
            "db",
            Some(&defaults),
        )
    };
    assert!(compare(&expected, &actual));
    let mut explicit = expected.clone();
    explicit.charset = Some("latin1".into());
    assert!(!compare(&explicit, &actual));
    explicit = expected.clone();
    explicit.collation = Some("utf8mb4_bin".into());
    assert!(!compare(&explicit, &actual));
    actual.columns[0].default = ColumnDefault::Literal("1".into());
    assert!(!compare(&expected, &actual));
    actual.columns[0].default = ColumnDefault::Null;
    actual.options.push("ROW_FORMAT=COMPRESSED".into());
    assert!(!compare(&expected, &actual));
    assert_eq!(
        super::super::verify::normalized_type("INT(11) UNSIGNED"),
        "int unsigned"
    );
    assert_eq!(super::super::verify::normalized_type("integer"), "int");
    assert_ne!(
        super::super::verify::normalized_type("int(5) zerofill"),
        super::super::verify::normalized_type("int(11) zerofill")
    );
    assert_ne!(
        super::super::verify::normalized_type("varchar(5)"),
        super::super::verify::normalized_type("varchar(11)")
    );
}
