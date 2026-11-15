//! SQL 恢复选择、依赖与冻结范围回归。
use super::*;

#[test]
fn excluded_dependency_and_database_only_selection_are_rejected() {
    let path = std::env::temp_dir().join(format!(
        "datapier-restore-selection-{}.sql",
        uuid::Uuid::new_v4()
    ));
    std::fs::write(&path, "USE `db`; CREATE TABLE `parent` (`id` INT PRIMARY KEY); CREATE TABLE `child` (`id` INT, `parent_id` INT, FOREIGN KEY (`parent_id`) REFERENCES `parent` (`id`));").unwrap();
    let index = sql_restore::inspect(&path).unwrap();
    let spec: TransferSpec = serde_json::from_value(serde_json::json!({"mode":"sql_restore", "provider":"mysql", "destination":{"database":"db"}})).unwrap();
    let mut entries = Vec::new();
    for (number, statement) in index
        .statements
        .iter()
        .enumerate()
        .filter(|(_, statement)| statement.kind == SqlKind::Table && statement.is_create)
    {
        let object = mapping(&spec, statement, 0).unwrap();
        let mut scope = item(0, number, "object_scope", &object, None);
        scope.id.push_str(":object-scope");
        entries.push(scope);
    }
    assert_eq!(entries.len(), 2);
    let sources = vec![RestoreInput {
        path: path.clone(),
        index: std::sync::Arc::new(index),
    }];
    assert!(validate_restore_selection(&entries, &sources, &spec).is_ok());
    entries
        .iter_mut()
        .find(|entry| entry.source.as_ref().unwrap().name == "parent")
        .unwrap()
        .selected = false;
    assert_eq!(
        validate_restore_selection(&entries, &sources, &spec)
            .unwrap_err()
            .code,
        "unresolved_reference"
    );
    entries.iter_mut().for_each(|entry| entry.selected = false);
    let mut database = entries[0].clone();
    database.source = None;
    database.phase = "database".into();
    database.selected = true;
    entries.push(database);
    assert!(validate_restore_selection(&entries, &sources, &spec).is_err());
    std::fs::remove_file(path).unwrap();
}

#[test]
fn comma_from_dependencies_are_indexed_and_selection_checked() {
    let input = BudgetInput::new(
        "USE `db`; CREATE TABLE `A` (`id` INT); CREATE TABLE `a` (`id` INT); CREATE VIEW `v` AS SELECT concat('FROM ignored, x', x.id), y.id FROM `A` AS x, (SELECT * FROM `a`) AS y;",
    );
    let index = sql_restore::inspect(&input.0).unwrap();
    let view = index
        .statements
        .iter()
        .find(|statement| statement.kind == SqlKind::View)
        .unwrap();
    assert!(view.references.iter().any(|name| name == "A"));
    assert!(view.references.iter().any(|name| name == "a"));
    assert_eq!(view.references.len(), 2);
    assert!(!view.unresolved_references);
    let spec: TransferSpec = serde_json::from_value(serde_json::json!({"mode":"sql_restore", "provider":"mysql", "destination":{"database":"db"}})).unwrap();
    let mut entries = Vec::new();
    for (number, statement) in index
        .statements
        .iter()
        .enumerate()
        .filter(|(_, statement)| {
            matches!(statement.kind, SqlKind::Table | SqlKind::View) && statement.is_create
        })
    {
        let object = mapping(&spec, statement, 0).unwrap();
        let mut scope = item(0, number, "object_scope", &object, None);
        scope.id.push_str(":object-scope");
        if object.source.name == "a" {
            scope.selected = false;
        }
        entries.push(scope);
    }
    let sources = vec![RestoreInput {
        path: input.0.clone(),
        index: std::sync::Arc::new(index),
    }];
    assert_eq!(
        validate_restore_selection(&entries, &sources, &spec)
            .unwrap_err()
            .code,
        "unresolved_reference"
    );
}

#[test]
fn uncertain_parenthesized_table_scope_is_flagged() {
    let input = BudgetInput::new("USE `db`; CREATE VIEW `v` AS SELECT * FROM (`a`, `b`);");
    let index = sql_restore::inspect(&input.0).unwrap();
    let view = index
        .statements
        .iter()
        .find(|statement| statement.kind == SqlKind::View)
        .unwrap();
    assert!(view.unresolved_references);
}

struct BudgetInput(std::path::PathBuf);

impl BudgetInput {
    fn new(text: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("datapier-sql-budget-{}.sql", uuid::Uuid::new_v4()));
        std::fs::write(&path, text).unwrap();
        Self(path)
    }
}

impl Drop for BudgetInput {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn index_budget_limits_small_statements_and_context_snapshots() {
    let cancel = CancellationToken::new();
    let inserts = BudgetInput::new(&"INSERT INTO t VALUES (1);\n".repeat(2_000));
    let error = sql_restore::inspect_with_budget(
        &inserts.0,
        "utf-8",
        &cancel,
        &mut sql_restore::IndexBudget::with_limit(64 * 1024),
    )
    .unwrap_err();
    assert_eq!(error.code, "resource_limit");

    let contexts = BudgetInput::new(&"SET SQL_MODE='';\n".repeat(1_000));
    let error = sql_restore::inspect_with_budget(
        &contexts.0,
        "utf-8",
        &cancel,
        &mut sql_restore::IndexBudget::with_limit(64 * 1024),
    )
    .unwrap_err();
    assert_eq!(error.code, "resource_limit");
}

#[test]
fn index_budget_does_not_limit_streamed_row_bytes() {
    let text = format!("INSERT INTO t VALUES {};", vec!["(1)"; 20_000].join(","));
    let input = BudgetInput::new(&text);
    let index = sql_restore::inspect_with_budget(
        &input.0,
        "utf-8",
        &CancellationToken::new(),
        &mut sql_restore::IndexBudget::with_limit(64 * 1024),
    )
    .unwrap();
    let mut rows = 0;
    sql_restore::read_inserts(&input.0, &index, "t", |_, _, _| {
        rows += 1;
        Ok(())
    })
    .unwrap();
    assert_eq!(rows, 20_000);
}

#[tokio::test]
async fn multiple_restore_inputs_share_index_budget() {
    let input = BudgetInput::new(&"INSERT INTO t VALUES (1);\n".repeat(100));
    let cancel = CancellationToken::new();
    let mut measured = sql_restore::IndexBudget::new();
    let index =
        sql_restore::inspect_with_budget(&input.0, "utf-8", &cancel, &mut measured).unwrap();
    drop(index);
    let limit = measured.used() * 2 - 1;
    let spec: TransferSpec = serde_json::from_value(serde_json::json!({
        "mode": "sql_restore", "provider": "mysql",
        "inputs": [{"path": input.0}, {"path": input.0}]
    }))
    .unwrap();
    let root = std::env::temp_dir().join(format!("datapier-index-budget-{}", uuid::Uuid::new_v4()));
    let store = crate::Store::open(&root).await.unwrap();
    let task = store
        .create_task(
            uuid::Uuid::new_v4(),
            "fixture",
            "transfer.prepare",
            serde_json::json!({}),
            false,
        )
        .await
        .unwrap();
    let sink = crate::ExecutionSink::new(store.clone(), task.id);
    let mut single = spec.clone();
    single.inputs.truncate(1);
    assert!(
        inputs_with_budget(
            &single,
            &sink,
            &cancel,
            sql_restore::IndexBudget::with_limit(limit)
        )
        .await
        .is_ok()
    );
    let error = inputs_with_budget(
        &spec,
        &sink,
        &cancel,
        sql_restore::IndexBudget::with_limit(limit),
    )
    .await
    .err()
    .unwrap();
    assert_eq!(error.code, "resource_limit");
    drop(sink);
    store.pool.close().await;
    drop(store);
    std::fs::remove_dir_all(root).unwrap();
}

fn rewrite_view_for_selection(
    sql: &str,
    mappings: serde_json::Value,
) -> crate::model::Result<String> {
    use crate::model::transfer::{DefinerMapping, TransferObject};
    let input = BudgetInput::new(&format!("USE `db`; {sql};"));
    let index = sql_restore::inspect(&input.0)?;
    let (number, statement) = index
        .statements
        .iter()
        .enumerate()
        .find(|(_, statement)| statement.kind == SqlKind::View)
        .unwrap();
    let ddl = sql_restore::read_ddl(&input.0, &index, number)?;
    let mappings: Vec<TransferObject> = serde_json::from_value(mappings).unwrap();
    sql_restore::rewrite_ddl(&ddl, statement, &mappings, &DefinerMapping::Preserve)
}

#[test]
fn comma_from_factors_are_mapped_and_checked() {
    let mappings = serde_json::json!([
        {"source":{"database":"db","name":"v","kind":"view"},"target":{"database":"out","name":"v2","kind":"view"}},
        {"source":{"database":"db","name":"a","kind":"table"},"target":{"database":"out","name":"a2","kind":"table"}},
        {"source":{"database":"db","name":"b","kind":"table"},"target":{"database":"out","name":"b2","kind":"table"}}
    ]);
    let sql = "CREATE VIEW `v` AS SELECT `a`.`id`, x.id FROM `a`, `b` AS x";
    let result = rewrite_view_for_selection(sql, mappings.clone()).unwrap();
    assert!(result.contains("FROM `out`.`a2`, `out`.`b2` AS x"));
    let nested = rewrite_view_for_selection(
        "CREATE VIEW `v` AS SELECT * FROM `a`, (SELECT * FROM `a`, `b`) AS x",
        mappings.clone(),
    )
    .unwrap();
    assert!(nested.contains("FROM `out`.`a2`, (SELECT * FROM `out`.`a2`, `out`.`b2`) AS x"));
    assert!(result.contains("`out`.`a2`.`id`, x.id"));
    let mut missing = mappings.clone();
    missing.as_array_mut().unwrap().pop();
    assert!(rewrite_view_for_selection(sql, missing).is_err());
    let mut columns = mappings;
    columns[2]["columns"] = serde_json::json!([{"source":"id","target":"other"}]);
    assert!(rewrite_view_for_selection(sql, columns).is_err());
}

#[test]
fn ambiguous_case_and_nested_aliases_are_rejected() {
    let mappings = serde_json::json!([
        {"source":{"database":"db","name":"v","kind":"view"},"target":{"database":"out","name":"v","kind":"view"}},
        {"source":{"database":"db","name":"A","kind":"table"},"target":{"database":"out","name":"upper","kind":"table"}},
        {"source":{"database":"db","name":"a","kind":"table"},"target":{"database":"out","name":"lower","kind":"table"}}
    ]);
    assert!(
        rewrite_view_for_selection(
            "CREATE VIEW `v` AS SELECT * FROM `A`, `a`",
            mappings.clone()
        )
        .is_err()
    );
    let mut one = mappings;
    one.as_array_mut().unwrap().pop();
    assert!(
        rewrite_view_for_selection("CREATE VIEW `v` AS SELECT * FROM `a`", one.clone()).is_err()
    );
    assert!(
        rewrite_view_for_selection(
            "CREATE VIEW `v` AS SELECT `A`.id FROM `A` WHERE EXISTS (SELECT 1 FROM `A` AS `A`)",
            one
        )
        .is_err()
    );
}
