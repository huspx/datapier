use super::column_flags;

fn grant_schema_matches(scope: &str, database: &str, case_insensitive: bool) -> bool {
    super::grant_schema_matches(scope, database, case_insensitive, false)
}

#[test]
fn auto_increment_and_generated_are_independent() {
    assert_eq!(column_flags("auto_increment", None), (true, false));
    assert_eq!(
        column_flags("VIRTUAL GENERATED", Some("`a` + 1")),
        (false, true)
    );
    assert_eq!(
        column_flags("DEFAULT_GENERATED on update CURRENT_TIMESTAMP", None),
        (false, false)
    );
    assert_eq!(
        column_flags("auto_increment DEFAULT_GENERATED", None),
        (true, false)
    );
}

#[test]
fn schema_grants_preserve_escaped_wildcards_and_identifier_quotes() {
    assert!(grant_schema_matches(
        r"`datapier\_fixture`.*",
        "datapier_fixture",
        false
    ));
    assert!(!grant_schema_matches(
        r"`datapier\_fixture`.*",
        "datapierXfixture",
        false
    ));
    assert!(grant_schema_matches(r"`a\%b`.*", "a%b", false));
    assert!(!grant_schema_matches(r"`a\%b`.*", "axxb", false));
    assert!(grant_schema_matches("`tenant_%`.*", "tenant_audit", false));
    assert!(!grant_schema_matches("`tenant_%`.*", "tenant", false));
    assert!(grant_schema_matches(
        "`quoted``name`.*",
        "quoted`name",
        false
    ));
    assert!(!grant_schema_matches(
        "`quoted`name`.*",
        "quoted`name",
        false
    ));
    assert!(!grant_schema_matches(
        r"`unfinished\`.*",
        "unfinished",
        false
    ));
    assert!(!grant_schema_matches("`Fixture`.*", "fixture", false));
    assert!(grant_schema_matches("`Fixture`.*", "fixture", true));
    assert!(!grant_schema_matches("`Fixture`.*", "fixture_extra", true));
    assert!(!grant_schema_matches("`db`.`table`", "db", false));
}

#[test]
fn partial_revokes_treats_wildcards_as_literal_database_names() {
    assert!(super::grant_schema_matches(
        "`tenant_%`.*",
        "tenant_%",
        false,
        true
    ));
    assert!(!super::grant_schema_matches(
        "`tenant_%`.*",
        "tenant_audit",
        false,
        true
    ));
    assert!(super::grant_schema_matches(r"`a\b`.*", r"a\b", false, true));
}
