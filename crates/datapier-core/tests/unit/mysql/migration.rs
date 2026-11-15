use super::*;

#[test]
fn offline_mysql_engine_options_preserve_transactional_engine() {
    for option in ["ENGINE=InnoDB", "ENGINE = InnoDB"] {
        let target = ResourceTarget {
            database: Some("app".into()),
            name: "items".into(),
            kind: "table".into(),
            key_b64: None,
        };
        let snapshot = offline::parse_ddl(target, &format!("CREATE TABLE items (id INT NOT NULL PRIMARY KEY) {option} DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_general_ci"), "").unwrap();
        let crate::model::schema::SchemaDefinition::Table(table) = snapshot.definition else {
            panic!("table definition required")
        };
        assert_eq!(table.engine, "InnoDB");
    }
}

#[test]
fn ddl_parts_keep_nested_expressions_and_quoted_commas() {
    let ddl = "CREATE TABLE `t` (`id` int NOT NULL, `s` enum('a,b','c') DEFAULT 'a,b', KEY `k` (`s`)) ENGINE=InnoDB";
    let (parts, options) = ddl_parts(ddl).unwrap();
    assert_eq!(parts.len(), 3);
    assert_eq!(part_key(&parts[1]), Some(("column".into(), "s".into())));
    assert_eq!(part_key(&parts[2]), Some(("index".into(), "k".into())));
    assert_eq!(options, "ENGINE=InnoDB");
}

#[test]
fn fingerprint_ignores_only_auto_increment_counter() {
    let a = "CREATE TABLE `t` (`id` int NOT NULL) ENGINE=InnoDB AUTO_INCREMENT=12 DEFAULT CHARSET=utf8mb4";
    let b = "CREATE TABLE `t` (`id` int NOT NULL) ENGINE=InnoDB AUTO_INCREMENT=345 DEFAULT CHARSET=utf8mb4";
    let c = "CREATE TABLE `t` (`id` bigint NOT NULL) ENGINE=InnoDB AUTO_INCREMENT=345 DEFAULT CHARSET=utf8mb4";
    assert_eq!(hash(a, "STRICT_ALL_TABLES"), hash(b, "STRICT_ALL_TABLES"));
    assert_ne!(hash(a, "STRICT_ALL_TABLES"), hash(c, "STRICT_ALL_TABLES"));
}

#[test]
fn file_schema_changes_preserve_defaults_unmapped_columns_and_indexes() {
    let target = ResourceTarget {
        database: Some("db".into()),
        name: "items".into(),
        kind: "table".into(),
        key_b64: None,
    };
    let ddl = "CREATE TABLE `items` (`id` int NOT NULL AUTO_INCREMENT, `name` varchar(20) NOT NULL DEFAULT 'kept', `other` int DEFAULT 4, PRIMARY KEY (`id`), KEY `other_idx` (`other`)) ENGINE=InnoDB";
    let table = MigrationTable {
        schema: TableSchema {
            target,
            columns: Vec::new(),
            keys: Vec::new(),
            ddl: ddl.into(),
            engine: "InnoDB".into(),
            details: json!({}),
        },
        version: "8.0.40".into(),
        sql_mode: String::new(),
        fingerprint: "baseline".into(),
        collations: Default::default(),
        time_zone: "+00:00".into(),
        snapshot: None,
    };
    let mut changed = Column::new("file_name", "varchar(80)");
    changed.nullable = false;
    let added = Column::new("created", "int");
    let mapping = [("file_name".into(), "name".into())].into_iter().collect();
    let items =
        input_schema_items(&table, &[changed.clone(), added.clone()], &mapping, &[]).unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(
        items[0].sql.as_deref(),
        Some("ALTER TABLE `db`.`items` MODIFY COLUMN `name` varchar(80) NOT NULL DEFAULT 'kept'")
    );
    assert_eq!(
        items[1].sql.as_deref(),
        Some("ALTER TABLE `db`.`items` ADD COLUMN `created` int")
    );
    assert!(items[0].destructive);
    assert!(!items[1].destructive);
    let selected = vec![items[0].id.clone()];
    let again = input_schema_items(&table, &[changed, added], &mapping, &selected).unwrap();
    assert!(again[0].selected);
    assert!(!again[1].selected);
}

#[test]
fn file_schema_preserves_generated_and_auto_increment_attributes() {
    let target = ResourceTarget {
        database: Some("db".into()),
        name: "t".into(),
        kind: "table".into(),
        key_b64: None,
    };
    let ddl = "CREATE TABLE `t` (`id` int NOT NULL AUTO_INCREMENT, `v` int GENERATED ALWAYS AS (`id` + 1) STORED, PRIMARY KEY (`id`)) ENGINE=InnoDB";
    let table = MigrationTable {
        schema: TableSchema {
            target,
            columns: Vec::new(),
            keys: Vec::new(),
            ddl: ddl.into(),
            engine: "InnoDB".into(),
            details: json!({}),
        },
        version: "8.0.40".into(),
        sql_mode: String::new(),
        fingerprint: "baseline".into(),
        collations: Default::default(),
        time_zone: "+00:00".into(),
        snapshot: None,
    };
    let mut id = Column::new("id", "bigint");
    id.nullable = false;
    let generated = Column::new("v", "bigint");
    let items = input_schema_items(&table, &[id, generated], &Default::default(), &[]).unwrap();
    assert_eq!(
        items[0].sql.as_deref(),
        Some("ALTER TABLE `db`.`t` MODIFY COLUMN `id` bigint NOT NULL AUTO_INCREMENT")
    );
    assert_eq!(
        items[1].sql.as_deref(),
        Some("ALTER TABLE `db`.`t` MODIFY COLUMN `v` bigint GENERATED ALWAYS AS (`id` + 1) STORED")
    );
}

#[test]
fn restore_alter_and_indexes_merge_into_one_table_definition() {
    let target = ResourceTarget {
        database: Some("db".into()),
        name: "t".into(),
        kind: "table".into(),
        key_b64: None,
    };
    let mut table = MigrationTable { schema: TableSchema { target, columns: Vec::new(), keys: Vec::new(), ddl: "CREATE TABLE `t` (`id` int NOT NULL, `value` varchar(20), PRIMARY KEY (`id`)) ENGINE=InnoDB".into(), engine: "InnoDB".into(), details: json!({}) }, version: "unknown".into(), sql_mode: String::new(), fingerprint: "baseline".into(), collations: Default::default(), time_zone: "+00:00".into(), snapshot: None };
    table.schema.ddl =
        restore_schema_sql(&table, "CREATE UNIQUE INDEX `idx` ON `db`.`t` (`value`)").unwrap();
    let (parts, _) = ddl_parts(&table.schema.ddl).unwrap();
    assert!(parts.contains(&"UNIQUE KEY `idx` (`value`)".into()));
    table.schema.ddl = restore_schema_sql(&table, "ALTER TABLE `t` ADD COLUMN `extra` decimal(9,2) DEFAULT 0, MODIFY COLUMN `value` varchar(40)").unwrap();
    let (parts, _) = ddl_parts(&table.schema.ddl).unwrap();
    assert!(parts.contains(&"`extra` decimal(9,2) DEFAULT 0".into()));
    assert!(parts.contains(&"`value` varchar(40)".into()));
    table.schema.ddl = restore_schema_sql(&table, "ALTER TABLE `t` DROP INDEX `idx`").unwrap();
    let (parts, _) = ddl_parts(&table.schema.ddl).unwrap();
    assert!(
        parts
            .iter()
            .all(|part| part_key(part) != Some(("index".into(), "idx".into())))
    );
    assert!(
        restore_schema_sql(
            &table,
            "ALTER TABLE `t` ADD COLUMN `bad` int; DROP TABLE `t`"
        )
        .is_err()
    );
}
