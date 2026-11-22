use super::*;

// 竖线标记光标，所有偏移量都按编辑器使用的 UTF-16 代码单元计算。
fn analyze_marked(marked: &str) -> Option<Analysis> {
    let marker = marked.find('|').expect("缺少光标标记");
    let cursor = marked[..marker].encode_utf16().count();
    let mut sql = marked.to_owned();
    sql.remove(marker);
    analyze(
        &sql,
        cursor,
        crate::editor::SqlContext::Stateless,
        Some("current_db"),
    )
}

fn relation<'a>(analysis: &'a Analysis, alias: &str) -> &'a Relation {
    analysis
        .relations
        .iter()
        .find(|relation| relation.alias == alias)
        .unwrap_or_else(|| panic!("找不到表别名 {alias}"))
}

#[test]
fn select_finds_relations_after_the_cursor_and_join_aliases() {
    let result =
        analyze_marked("SELECT u.na|me FROM users AS u JOIN orders o ON o.user_id = u.id").unwrap();

    assert!(matches!(result.position, Position::QualifiedColumn));
    assert_eq!(result.prefix, "na");
    assert_eq!(result.qualifier.as_deref(), Some("u"));
    assert_eq!(relation(&result, "u").name, "users");
    assert_eq!(relation(&result, "o").name, "orders");
}

#[test]
fn self_join_keeps_both_aliases_for_the_same_table() {
    let result = analyze_marked(
        "SELECT manager.| FROM employees employee JOIN employees manager ON manager.id = employee.manager_id",
    )
    .unwrap();

    assert!(matches!(result.position, Position::QualifiedColumn));
    assert_eq!(result.qualifier.as_deref(), Some("manager"));
    assert_eq!(relation(&result, "employee").name, "employees");
    assert_eq!(relation(&result, "manager").name, "employees");
}

#[test]
fn cross_database_quoted_relations_preserve_database_and_alias() {
    let result = analyze_marked(
        "SELECT `u`.| FROM `sales-db`.`order items` AS `u` JOIN `other`.`users` AS `v` ON 1 = 1",
    )
    .unwrap();

    assert!(matches!(result.position, Position::QualifiedColumn));
    assert_eq!(result.qualifier.as_deref(), Some("u"));
    assert_eq!(relation(&result, "u").database.as_deref(), Some("sales-db"));
    assert_eq!(relation(&result, "u").name, "order items");
    assert_eq!(relation(&result, "v").database.as_deref(), Some("other"));
    assert_eq!(relation(&result, "v").name, "users");
}

#[test]
fn replacement_range_covers_the_whole_identifier_in_utf16() {
    let result = analyze_marked("SELECT '😀', u.na|me FROM users u").unwrap();
    let sql = "SELECT '😀', u.name FROM users u";
    let start = sql[..sql.find("name").unwrap()].encode_utf16().count();

    assert_eq!(result.range, start..start + 4);
    assert_eq!(result.prefix, "na");
    assert_eq!(result.qualifier.as_deref(), Some("u"));
}

#[test]
fn identifies_relation_and_database_qualified_relation_positions() {
    let result = analyze_marked("SELECT * FROM use|rs").unwrap();
    assert!(matches!(result.position, Position::Relation));
    assert_eq!(result.prefix, "use");

    let result = analyze_marked("SELECT * FROM `sales-db`.ord|ers").unwrap();
    assert!(matches!(result.position, Position::QualifiedRelation));
    assert_eq!(result.qualifier.as_deref(), Some("sales-db"));
    assert_eq!(result.prefix, "ord");
}

#[test]
fn insert_column_position_excludes_columns_already_in_the_list() {
    let result = analyze_marked("INSERT INTO users (id, na|me) VALUES (1, 'x')").unwrap();

    assert!(matches!(result.position, Position::InsertColumn));
    assert_eq!(result.prefix, "na");
    assert!(result.excluded_columns.iter().any(|column| column == "id"));
    assert!(
        !result
            .excluded_columns
            .iter()
            .any(|column| column == "name")
    );
    assert!(
        result
            .relations
            .iter()
            .any(|relation| relation.name == "users")
    );
}

#[test]
fn update_and_delete_find_the_target_relation() {
    let update = analyze_marked("UPDATE users AS u SET u.na|me = 'x' WHERE u.id = 1").unwrap();
    assert!(matches!(update.position, Position::QualifiedColumn));
    assert_eq!(update.qualifier.as_deref(), Some("u"));
    assert_eq!(relation(&update, "u").name, "users");

    let delete = analyze_marked("DELETE FROM users AS u WHERE u.na|me = 'x'").unwrap();
    assert!(matches!(delete.position, Position::QualifiedColumn));
    assert_eq!(delete.qualifier.as_deref(), Some("u"));
    assert_eq!(relation(&delete, "u").name, "users");
}

#[test]
fn cte_and_derived_table_aliases_are_not_mapped_to_physical_tables() {
    let cte = analyze_marked("WITH recent AS (SELECT id FROM orders) SELECT r.| FROM recent AS r")
        .unwrap();
    assert!(cte.relations.iter().all(|relation| relation.alias != "r"));

    let derived =
        analyze_marked("SELECT d.| FROM (SELECT id FROM orders) AS d JOIN users u ON d.id = u.id")
            .unwrap();
    assert!(
        derived
            .relations
            .iter()
            .all(|relation| relation.alias != "d")
    );
    assert_eq!(relation(&derived, "u").name, "users");
}

#[test]
fn comments_and_string_literals_do_not_trigger_completion() {
    for sql in [
        "SELECT 'hel|lo' FROM users",
        "SELECT 'it''s |text' FROM users",
        "SELECT 1 -- comm|ent\nFROM users",
        "SELECT 1 /* comm|ent */ FROM users",
    ] {
        assert!(
            analyze_marked(sql).is_none(),
            "不应在注释或字符串中补全: {sql}"
        );
    }
}

#[test]
fn quoted_identifiers_and_escaped_backticks_are_decoded() {
    let result =
        analyze_marked("SELECT `odd``alias`.| FROM `order``items` AS `odd``alias`").unwrap();

    assert!(matches!(result.position, Position::QualifiedColumn));
    assert_eq!(result.qualifier.as_deref(), Some("odd`alias"));
    assert_eq!(relation(&result, "odd`alias").name, "order`items");
}

#[test]
fn quoted_identifier_replacement_includes_quotes_and_decodes_prefix() {
    let result = analyze_marked("SELECT `odd``na|me` FROM users").unwrap();
    let sql = "SELECT `odd``name` FROM users";
    let start = sql[..sql.find('`').unwrap()].encode_utf16().count();
    let end = start + "`odd``name`".encode_utf16().count();

    assert_eq!(result.range, start..end);
    assert_eq!(result.prefix, "odd`na");
}

#[test]
fn comments_and_strings_do_not_create_phantom_relations() {
    let result = analyze_marked(
        "SELECT na|me, 'FROM fake f' FROM users u /* JOIN hidden h */ WHERE u.id = 1",
    )
    .unwrap();

    assert!(matches!(result.position, Position::Expression));
    assert_eq!(relation(&result, "u").name, "users");
    assert!(
        result
            .relations
            .iter()
            .all(|relation| relation.name != "fake" && relation.name != "hidden")
    );
}
