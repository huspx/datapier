//! 直接迁移结构选择与数据映射约束回归。
use super::*;
use crate::model::{
    TableSchema,
    schema::{
        ColumnDefault, ColumnDefinition, Completeness, CompletenessState, SchemaDefinition,
        SchemaSnapshot, TableDefinition,
    },
};
use crate::providers::mysql::migration::MigrationTable;

fn table(names: &[(&str, &str)]) -> MigrationTable {
    let target = ResourceTarget {
        database: Some("db".into()),
        name: "items".into(),
        kind: "table".into(),
        key_b64: None,
    };
    let columns: Vec<_> = names
        .iter()
        .map(|(name, ty)| Column::new(*name, *ty))
        .collect();
    let definition = TableDefinition {
        columns: names
            .iter()
            .map(|(name, ty)| ColumnDefinition {
                id: (*name).into(),
                name: (*name).into(),
                native_type: (*ty).into(),
                nullable: true,
                default: ColumnDefault::Unspecified,
                on_update: None,
                auto_increment: false,
                generated: None,
                charset: None,
                collation: None,
                visible: true,
                comment: String::new(),
                extra: Vec::new(),
            })
            .collect(),
        indexes: Vec::new(),
        constraints: Vec::new(),
        engine: "InnoDB".into(),
        charset: Some("utf8mb4".into()),
        collation: Some("utf8mb4_general_ci".into()),
        comment: String::new(),
        options: Vec::new(),
    };
    let collations = names
        .iter()
        .map(|(name, ty)| {
            (
                (*name).into(),
                if *ty == "text" {
                    (Some("utf8mb4".into()), Some("utf8mb4_general_ci".into()))
                } else {
                    (None, None)
                },
            )
        })
        .collect();
    MigrationTable {
        schema: TableSchema {
            target: target.clone(),
            columns,
            keys: Vec::new(),
            ddl: "baseline".into(),
            engine: "InnoDB".into(),
            details: serde_json::json!({}),
        },
        version: "8.0.40".into(),
        sql_mode: String::new(),
        fingerprint: "baseline".into(),
        collations,
        time_zone: "+00:00".into(),
        snapshot: Some(SchemaSnapshot {
            target,
            server_version: "8.0.40".into(),
            sql_mode: String::new(),
            definition: SchemaDefinition::Table(definition),
            ddl: "baseline".into(),
            dependencies: Vec::new(),
            completeness: Completeness {
                state: CompletenessState::Complete,
                reasons: Vec::new(),
            },
            fingerprint: "baseline".into(),
            capabilities: Default::default(),
        }),
    }
}

#[test]
fn copy_schema_selection_validates_the_resulting_columns() {
    let source = table(&[("id", "int"), ("note", "text"), ("amount", "decimal(20,6)")]);
    let current = table(&[("id", "int")]);
    let mut object = TransferObject {
        source: source.schema.target.clone(),
        target: current.schema.target.clone(),
        columns: source
            .schema
            .columns
            .iter()
            .map(|column| ColumnMapping {
                source: column.name.clone(),
                target: column.name.clone(),
            })
            .collect(),
        target_columns: Vec::new(),
        filters: Vec::new(),
        locator_key: Vec::new(),
        create_if_missing: false,
        sync_schema: true,
        selected_schema_items: Vec::new(),
    };
    let source_snapshot = source.snapshot.as_ref().unwrap();
    let target_snapshot = current.snapshot.as_ref().unwrap();
    let plan = |object: &TransferObject| {
        crate::providers::mysql::schema::plan_transfer_with_definition(
            source_snapshot,
            Some(target_snapshot),
            object,
            std::slice::from_ref(object),
            "8.0.40".into(),
            String::new(),
        )
        .unwrap()
    };
    let (items, desired) = plan(&object);
    assert_eq!(items.len(), 1);
    assert!(!items[0].selected);
    assert!(validate_copy_mapping(&source, &current, &object, &items, desired.as_ref()).is_err());
    object.selected_schema_items.push(items[0].id.clone());
    let (selected, desired) = plan(&object);
    assert!(validate_copy_mapping(&source, &current, &object, &selected, desired.as_ref()).is_ok());
    let mut partial = desired.unwrap();
    partial.columns.retain(|column| column.name != "amount");
    assert!(validate_copy_mapping(&source, &current, &object, &selected, Some(&partial)).is_err());
    let mut disabled = selected.clone();
    disabled[0].selected = false;
    assert!(validate_copy_mapping(&source, &current, &object, &disabled, Some(&partial)).is_err());
    partial.columns.push(match &source_snapshot.definition {
        SchemaDefinition::Table(table) => table.columns[2].clone(),
        _ => unreachable!(),
    });
    partial.columns[1].generated = Some(crate::model::schema::GeneratedColumn {
        expression: "1".into(),
        stored: false,
    });
    assert!(validate_copy_mapping(&source, &current, &object, &selected, Some(&partial)).is_err());

    object.columns[1].target = "renamed_note".into();
    object.selected_schema_items.clear();
    let (renamed, _) = plan(&object);
    object.selected_schema_items.push(renamed[0].id.clone());
    let (renamed, desired) = plan(&object);
    assert!(validate_copy_mapping(&source, &current, &object, &renamed, desired.as_ref()).is_ok());
}

#[test]
fn restore_validates_foreign_keys_and_defers_new_constraints() {
    use crate::model::schema::{ConstraintDefinition, IndexDefinition, IndexKind, IndexPart};
    let mut child = table(&[("id", "int"), ("parent_id", "int")]);
    child.schema.target.name = "child".into();
    let child_snapshot = child.snapshot.as_mut().unwrap();
    child_snapshot.target.name = "child".into();
    child_snapshot.server_version.clear();
    let SchemaDefinition::Table(child_definition) = &mut child_snapshot.definition else {
        unreachable!()
    };
    child_definition
        .constraints
        .push(ConstraintDefinition::ForeignKey {
            id: "fk_parent".into(),
            name: "fk_parent".into(),
            columns: vec!["parent_id".into()],
            referenced_database: "db".into(),
            referenced_table: "parent".into(),
            referenced_columns: vec!["id".into()],
            on_update: "RESTRICT".into(),
            on_delete: "RESTRICT".into(),
        });
    let mut parent = table(&[("id", "int")]);
    parent.schema.target.name = "parent".into();
    let parent_snapshot = parent.snapshot.as_mut().unwrap();
    parent_snapshot.target.name = "parent".into();
    let SchemaDefinition::Table(parent_definition) = &mut parent_snapshot.definition else {
        unreachable!()
    };
    parent_definition.indexes.push(IndexDefinition {
        id: "PRIMARY".into(),
        name: "PRIMARY".into(),
        kind: IndexKind::Primary,
        parts: vec![IndexPart {
            column: Some("id".into()),
            expression: None,
            prefix_length: None,
            descending: false,
        }],
        visible: true,
        method: None,
        options: Vec::new(),
    });
    let object = |table: &MigrationTable| TransferObject {
        source: table.schema.target.clone(),
        target: table.schema.target.clone(),
        columns: Vec::new(),
        target_columns: Vec::new(),
        filters: Vec::new(),
        locator_key: Vec::new(),
        create_if_missing: true,
        sync_schema: true,
        selected_schema_items: Vec::new(),
    };
    let objects = vec![object(&child), object(&parent)];
    let mut tables = BTreeMap::from([
        (("db".into(), "child".into()), child.clone()),
        (("db".into(), "parent".into()), parent.clone()),
    ]);
    assert!(
        crate::providers::mysql::schema::validate_restore_dependencies(&tables, &objects).is_ok()
    );
    let mut missing = tables.clone();
    missing.remove(&("db".into(), "parent".into()));
    assert!(
        crate::providers::mysql::schema::validate_restore_dependencies(&missing, &objects).is_err()
    );
    let SchemaDefinition::Table(definition) = &mut tables
        .get_mut(&("db".into(), "parent".into()))
        .unwrap()
        .snapshot
        .as_mut()
        .unwrap()
        .definition
    else {
        unreachable!()
    };
    definition.indexes.clear();
    assert!(
        crate::providers::mysql::schema::validate_restore_dependencies(&tables, &objects).is_err()
    );
    let mut current = table(&[("id", "int"), ("parent_id", "int")]);
    current.schema.target = child.schema.target.clone();
    current.snapshot.as_mut().unwrap().target = child.schema.target.clone();
    let (items, _) = crate::providers::mysql::schema::plan_transfer_with_definition(
        child.snapshot.as_ref().unwrap(),
        Some(current.snapshot.as_ref().unwrap()),
        &objects[0],
        &objects,
        "8.0.40".into(),
        String::new(),
    )
    .unwrap();
    assert!(
        items
            .iter()
            .any(|item| item.action == "add_foreign_key" && item.phase == "after_data")
    );
    assert!(!items.iter().any(|item| {
        item.phase == "schema"
            && item
                .sql
                .as_deref()
                .is_some_and(|sql| sql.contains("FOREIGN KEY"))
    }));
}
