//! 离线恢复文件的表结构解析。源服务端目录和版本不可用。
use crate::model::{AppError, ResourceTarget, Result, schema::*};
use sqlparser::{
    ast::{
        ColumnOption, CommentDef, CreateTableOptions, Expr, FunctionArg, FunctionArgExpr,
        FunctionArguments, GeneratedExpressionMode, IndexColumn, IndexOption, ObjectName,
        ObjectNamePart, OrderBySort, SqlOption, Statement, TableConstraint, Value,
    },
    dialect::MySqlDialect,
    parser::Parser,
};

fn name_parts(name: &sqlparser::ast::ObjectName) -> Option<Vec<String>> {
    name.0
        .iter()
        .map(|part| match part {
            ObjectNamePart::Identifier(id) => Some(id.value.clone()),
            _ => None,
        })
        .collect()
}

fn option_key_is(key: &ObjectName, name: &str) -> bool {
    matches!(key.0.as_slice(), [ObjectNamePart::Identifier(id)] if id.value.eq_ignore_ascii_case(name))
}

fn index_parts(columns: Vec<IndexColumn>, reasons: &mut Vec<String>) -> Vec<IndexPart> {
    columns
        .into_iter()
        .map(|part| {
            if part.operator_class.is_some()
                || part.column.options.nulls_first.is_some()
                || matches!(part.column.options.sort, Some(OrderBySort::Using(_)))
                || part.column.with_fill.is_some()
            {
                reasons.push(format!("索引列含无法表达的属性: {part}"));
            }
            let descending = matches!(part.column.options.sort, Some(OrderBySort::Desc));
            let expr = part.column.expr;
            let prefix = if let Expr::Function(f) = &expr {
                if let (Some(names), FunctionArguments::List(args)) = (name_parts(&f.name), &f.args)
                {
                    if names.len() == 1
                        && args.duplicate_treatment.is_none()
                        && args.clauses.is_empty()
                        && matches!(f.parameters, FunctionArguments::None)
                        && f.filter.is_none()
                        && f.over.is_none()
                        && f.null_treatment.is_none()
                    {
                        if let [FunctionArg::Unnamed(FunctionArgExpr::Expr(Expr::Value(v)))] =
                            args.args.as_slice()
                        {
                            if let Value::Number(n, _) = &v.value {
                                n.parse::<u32>()
                                    .ok()
                                    .map(|length| (names[0].clone(), length))
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            } else {
                None
            };
            match expr {
                Expr::Identifier(id) => IndexPart {
                    column: Some(id.value),
                    expression: None,
                    prefix_length: None,
                    descending,
                },
                _ if prefix.is_some() => {
                    let (name, length) = prefix.unwrap();
                    IndexPart {
                        column: Some(name),
                        expression: None,
                        prefix_length: Some(length),
                        descending,
                    }
                }
                other => IndexPart {
                    column: None,
                    expression: Some(other.to_string()),
                    prefix_length: None,
                    descending,
                },
            }
        })
        .collect()
}

fn index(
    name: Option<String>,
    kind: IndexKind,
    columns: Vec<IndexColumn>,
    index_type: Option<String>,
    options: Vec<IndexOption>,
    reasons: &mut Vec<String>,
) -> IndexDefinition {
    let name = name.unwrap_or_else(|| {
        reasons.push("索引缺少服务端生成的名称".into());
        String::new()
    });
    let mut method = index_type;
    let mut preserved = Vec::new();
    for option in options {
        match option {
            IndexOption::Using(value) => method = Some(value.to_string()),
            IndexOption::Comment(value) => preserved.push(format!(
                "COMMENT '{}'",
                value.replace('\\', "\\\\").replace('\'', "''")
            )),
        }
    }
    IndexDefinition {
        id: name.clone(),
        name,
        kind,
        parts: index_parts(columns, reasons),
        visible: true,
        method,
        options: preserved,
    }
}

fn foreign_key(
    f: sqlparser::ast::ForeignKeyConstraint,
    database: &str,
    inline_column: Option<&str>,
    reasons: &mut Vec<String>,
) -> Option<ConstraintDefinition> {
    let name = f.name.map(|n| n.value).unwrap_or_else(|| {
        reasons.push("外键缺少服务端生成的名称".into());
        String::new()
    });
    if f.index_name.is_some() || f.match_kind.is_some() || f.characteristics.is_some() {
        reasons.push(format!("外键 {name} 含无法表达的属性"));
    }
    let Some(parts) = name_parts(&f.foreign_table) else {
        reasons.push(format!("外键 {name} 的引用表无法解析"));
        return None;
    };
    let (referenced_database, referenced_table) = match parts.as_slice() {
        [table] if !database.is_empty() => (database.to_owned(), table.clone()),
        [table] => {
            reasons.push(format!("外键 {name} 的引用数据库未知"));
            (String::new(), table.clone())
        }
        [db, table] => (db.clone(), table.clone()),
        _ => {
            reasons.push(format!("外键 {name} 的引用表无法解析"));
            return None;
        }
    };
    let columns = if f.columns.is_empty() {
        inline_column.into_iter().map(str::to_owned).collect()
    } else {
        f.columns.into_iter().map(|c| c.value).collect()
    };
    Some(ConstraintDefinition::ForeignKey {
        id: name.clone(),
        name,
        columns,
        referenced_database,
        referenced_table,
        referenced_columns: f.referred_columns.into_iter().map(|c| c.value).collect(),
        on_update: f
            .on_update
            .map(|v| v.to_string())
            .unwrap_or_else(|| "RESTRICT".into()),
        on_delete: f
            .on_delete
            .map(|v| v.to_string())
            .unwrap_or_else(|| "RESTRICT".into()),
    })
}

fn check(c: sqlparser::ast::CheckConstraint, reasons: &mut Vec<String>) -> ConstraintDefinition {
    let name = c.name.map(|n| n.value).unwrap_or_else(|| {
        reasons.push("CHECK 缺少服务端生成的名称".into());
        String::new()
    });
    ConstraintDefinition::Check {
        id: name.clone(),
        name,
        expression: c.expr.to_string(),
        enforced: c.enforced.unwrap_or(true),
    }
}

fn option_value(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(v) => Some(v.value.clone()),
        Expr::Value(v) => match &v.value {
            Value::SingleQuotedString(s) | Value::Number(s, _) => Some(s.clone()),
            _ => None,
        },
        _ => None,
    }
}

pub(super) fn parse_ddl(
    target: ResourceTarget,
    ddl: &str,
    sql_mode: &str,
) -> Result<SchemaSnapshot> {
    if sql_mode.split(',').any(|m| {
        [
            "NO_BACKSLASH_ESCAPES",
            "ANSI_QUOTES",
            "ANSI",
            "DB2",
            "MAXDB",
            "MSSQL",
            "ORACLE",
            "POSTGRESQL",
            "PIPES_AS_CONCAT",
            "HIGH_NOT_PRECEDENCE",
            "IGNORE_SPACE",
            "REAL_AS_FLOAT",
        ]
        .iter()
        .any(|v| m.trim().eq_ignore_ascii_case(v))
    }) {
        return Err(AppError::new(
            "schema_incomplete",
            "源 SQL mode 与离线解析器的词法或表达式规则不兼容",
        ));
    }
    let mut statements = Parser::parse_sql(&MySqlDialect {}, ddl)
        .map_err(|e| AppError::new("schema_incomplete", format!("恢复表定义无法解析: {e}")))?;
    if statements.len() != 1 {
        return Err(AppError::new(
            "schema_incomplete",
            "恢复结构必须是单条 CREATE TABLE",
        ));
    }
    let Statement::CreateTable(table) = statements.remove(0) else {
        return Err(AppError::new(
            "schema_incomplete",
            "恢复结构不是 CREATE TABLE",
        ));
    };
    if table.query.is_some()
        || table.like.is_some()
        || table.clone.is_some()
        || table.columns.is_empty()
    {
        return Err(AppError::new(
            "schema_incomplete",
            "恢复表定义缺少显式列或依赖其他表/查询",
        ));
    }
    let mut reasons = Vec::new();
    if ddl.contains("/*!") || ddl.contains("/*M!") {
        reasons.push("版本注释的执行取决于未知源服务端版本".into());
    }
    if table.temporary
        || table.or_replace
        || table.external
        || table.partition_by.is_some()
        || table.cluster_by.is_some()
        || !table.sqlite_table_options.options.is_empty()
        || table.comment.is_some()
    {
        reasons.push("表包含无法完整表达的创建属性".into());
    }
    let database = target.database.as_deref().unwrap_or("");
    let mut columns = Vec::new();
    let mut indexes = Vec::new();
    let mut constraints = Vec::new();
    for c in table.columns {
        let mut out = ColumnDefinition {
            id: c.name.value.clone(),
            name: c.name.value,
            native_type: c.data_type.to_string(),
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
        };
        for opt in c.options {
            let option_name = opt.name;
            if option_name.is_some()
                && !matches!(
                    &opt.option,
                    ColumnOption::ForeignKey(_) | ColumnOption::Check(_)
                )
            {
                reasons.push(format!("字段 {} 的具名属性无法完整保留", out.name));
            }
            match opt.option {
                ColumnOption::Null => out.nullable = true,
                ColumnOption::NotNull => out.nullable = false,
                ColumnOption::Default(expr) => {
                    out.default = match expr {
                        Expr::Value(v) => match v.value {
                            Value::Null => ColumnDefault::Null,
                            Value::SingleQuotedString(s) | Value::Number(s, _) => {
                                ColumnDefault::Literal(s)
                            }
                            v => ColumnDefault::Expression(v.to_string()),
                        },
                        e => ColumnDefault::Expression(e.to_string()),
                    }
                }
                ColumnOption::OnUpdate(expr) => out.on_update = Some(expr.to_string()),
                ColumnOption::Invisible => out.visible = false,
                ColumnOption::Comment(comment) => out.comment = comment,
                ColumnOption::CharacterSet(name) => {
                    out.charset = name_parts(&name).and_then(|v| {
                        if v.len() == 1 {
                            v.into_iter().next()
                        } else {
                            None
                        }
                    })
                }
                ColumnOption::Collation(name) => {
                    out.collation = name_parts(&name).and_then(|v| {
                        if v.len() == 1 {
                            v.into_iter().next()
                        } else {
                            None
                        }
                    })
                }
                ColumnOption::Generated {
                    generation_expr: Some(expr),
                    generation_expr_mode,
                    sequence_options,
                    ..
                } => {
                    if sequence_options.is_some() {
                        reasons.push(format!("字段 {} 生成属性含序列选项", out.name));
                    }
                    out.generated = Some(GeneratedColumn {
                        expression: expr.to_string(),
                        stored: matches!(
                            generation_expr_mode,
                            Some(GeneratedExpressionMode::Stored)
                        ),
                    });
                }
                ColumnOption::DialectSpecific(tokens) => {
                    let extra = tokens
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("");
                    if extra.eq_ignore_ascii_case("AUTO_INCREMENT") {
                        out.auto_increment = true;
                    } else {
                        reasons.push(format!("字段 {} 未识别属性 {extra}", out.name));
                        out.extra.push(extra);
                    }
                }
                ColumnOption::PrimaryKey(i) => {
                    if i.characteristics.is_some() || i.name.is_some() || i.index_name.is_some() {
                        reasons.push(format!("字段 {} 主键含无法表达的属性", out.name));
                    }
                    out.nullable = false;
                    let cols = vec![IndexColumn::from(sqlparser::ast::Ident::new(&out.name))];
                    indexes.push(index(
                        Some("PRIMARY".into()),
                        IndexKind::Primary,
                        cols,
                        i.index_type.map(|v| v.to_string()),
                        i.index_options,
                        &mut reasons,
                    ));
                }
                ColumnOption::Unique(i) => {
                    if i.characteristics.is_some() {
                        reasons.push(format!("字段 {} 唯一约束含无法表达的属性", out.name));
                    }
                    let cols = vec![IndexColumn::from(sqlparser::ast::Ident::new(&out.name))];
                    indexes.push(index(
                        i.index_name.or(i.name).map(|n| n.value),
                        IndexKind::Unique,
                        cols,
                        i.index_type.map(|v| v.to_string()),
                        i.index_options,
                        &mut reasons,
                    ));
                }
                ColumnOption::ForeignKey(mut f) => {
                    if let Some(name) = option_name {
                        if f.name.as_ref().is_some_and(|inner| inner != &name) {
                            reasons.push(format!("字段 {} 的外键名称冲突", out.name));
                        } else {
                            f.name = Some(name);
                        }
                    }
                    if let Some(f) = foreign_key(f, database, Some(&out.name), &mut reasons) {
                        constraints.push(f);
                    }
                }
                ColumnOption::Check(mut c) => {
                    if let Some(name) = option_name {
                        if c.name.as_ref().is_some_and(|inner| inner != &name) {
                            reasons.push(format!("字段 {} 的 CHECK 名称冲突", out.name));
                        } else {
                            c.name = Some(name);
                        }
                    }
                    constraints.push(check(c, &mut reasons));
                }
                other => {
                    let value = other.to_string();
                    reasons.push(format!("字段 {} 未识别属性 {value}", out.name));
                    out.extra.push(value);
                }
            }
        }
        columns.push(out);
    }
    for c in table.constraints {
        match c {
            TableConstraint::PrimaryKey(i) => {
                if i.characteristics.is_some() || i.name.is_some() || i.index_name.is_some() {
                    reasons.push("主键含无法表达的属性".into());
                }
                indexes.push(index(
                    Some("PRIMARY".into()),
                    IndexKind::Primary,
                    i.columns,
                    i.index_type.map(|v| v.to_string()),
                    i.index_options,
                    &mut reasons,
                ));
            }
            TableConstraint::Unique(i) => {
                if i.characteristics.is_some() {
                    reasons.push("唯一约束含无法表达的属性".into());
                }
                indexes.push(index(
                    i.index_name.or(i.name).map(|n| n.value),
                    IndexKind::Unique,
                    i.columns,
                    i.index_type.map(|v| v.to_string()),
                    i.index_options,
                    &mut reasons,
                ));
            }
            TableConstraint::Index(i) => indexes.push(index(
                i.name.map(|n| n.value),
                IndexKind::Normal,
                i.columns,
                i.index_type.map(|v| v.to_string()),
                i.index_options,
                &mut reasons,
            )),
            TableConstraint::FulltextOrSpatial(i) => indexes.push(index(
                i.opt_index_name.map(|n| n.value),
                if i.fulltext {
                    IndexKind::Fulltext
                } else {
                    IndexKind::Spatial
                },
                i.columns,
                None,
                Vec::new(),
                &mut reasons,
            )),
            TableConstraint::ForeignKey(f) => {
                if let Some(f) = foreign_key(f, database, None, &mut reasons) {
                    constraints.push(f);
                }
            }
            TableConstraint::Check(c) => constraints.push(check(c, &mut reasons)),
            other => reasons.push(format!("未识别表约束: {other}")),
        }
    }
    for idx in &indexes {
        if matches!(idx.kind, IndexKind::Primary) {
            for part in &idx.parts {
                if let Some(name) = &part.column
                    && let Some(column) = columns.iter_mut().find(|c| c.name == *name)
                {
                    column.nullable = false;
                }
            }
        }
    }
    if indexes.iter().any(|i| !i.visible || i.parts.iter().any(|p| p.descending || p.expression.is_some())) || columns.iter().any(|c| !c.visible || matches!(&c.default, ColumnDefault::Expression(value) if !value.split('(').next().unwrap_or("").trim().eq_ignore_ascii_case("CURRENT_TIMESTAMP"))) {
        reasons.push("DDL 包含版本敏感的索引、字段或表达式默认值，源服务端版本未知".into());
    }
    let mut engine = String::new();
    let mut charset = None;
    let mut collation = None;
    let mut comment = String::new();
    let mut options = Vec::new();
    let table_options = match table.table_options {
        CreateTableOptions::None => Vec::new(),
        CreateTableOptions::Plain(v) => v,
        other => {
            reasons.push(format!("未识别表选项格式: {other}"));
            Vec::new()
        }
    };
    for option in table_options {
        match &option {
            // sqlparser 将 MySQL ENGINE 也表示为具名选项，即使它没有括号参数。
            SqlOption::NamedParenthesizedList(value)
                if value.key.value.eq_ignore_ascii_case("ENGINE") && value.values.is_empty() =>
            {
                if let Some(name) = &value.name {
                    engine = name.value.clone();
                } else {
                    reasons.push(format!("无法解析存储引擎: {option}"));
                    options.push(option.to_string());
                }
            }
            SqlOption::KeyValue { key, value } if option_key_is(key, "ENGINE") => {
                if let Some(v) = option_value(value) {
                    engine = v;
                } else {
                    reasons.push(format!("无法解析存储引擎: {option}"));
                    options.push(option.to_string());
                }
            }
            SqlOption::KeyValue { key, value }
                if [
                    "CHARSET",
                    "DEFAULT CHARSET",
                    "CHARACTER SET",
                    "DEFAULT CHARACTER SET",
                ]
                .iter()
                .any(|s| option_key_is(key, s)) =>
            {
                if let Some(v) = option_value(value) {
                    charset = Some(v);
                } else {
                    reasons.push(format!("无法解析字符集: {option}"));
                    options.push(option.to_string());
                }
            }
            SqlOption::KeyValue { key, value }
                if ["COLLATE", "DEFAULT COLLATE"]
                    .iter()
                    .any(|s| option_key_is(key, s)) =>
            {
                if let Some(v) = option_value(value) {
                    collation = Some(v);
                } else {
                    reasons.push(format!("无法解析排序规则: {option}"));
                    options.push(option.to_string());
                }
            }
            SqlOption::Comment(CommentDef::WithEq(v) | CommentDef::WithoutEq(v)) => {
                comment = v.clone()
            }
            SqlOption::KeyValue { key, value } if option_key_is(key, "COMMENT") => {
                if let Some(v) = option_value(value) {
                    comment = v;
                } else {
                    reasons.push(format!("无法解析表注释: {option}"));
                    options.push(option.to_string());
                }
            }
            SqlOption::KeyValue { key, .. } if option_key_is(key, "AUTO_INCREMENT") => {}
            _ => {
                reasons.push(format!("未识别表选项: {option}"));
                options.push(option.to_string());
            }
        }
    }
    if engine.is_empty() {
        reasons.push("DDL 未指定存储引擎，源默认引擎未知".into());
    }
    let fingerprint = super::super::schema::fingerprint(ddl, sql_mode);
    let state = if reasons.is_empty() {
        CompletenessState::Complete
    } else {
        CompletenessState::Incomplete
    };
    Ok(SchemaSnapshot {
        target,
        server_version: String::new(),
        sql_mode: sql_mode.to_owned(),
        definition: SchemaDefinition::Table(TableDefinition {
            columns,
            indexes,
            constraints,
            engine,
            charset,
            collation,
            comment,
            options,
        }),
        ddl: ddl.to_owned(),
        dependencies: Vec::new(),
        completeness: Completeness { state, reasons },
        fingerprint,
        capabilities: Default::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_named_foreign_keys_and_checks_from_dump() {
        let target = ResourceTarget {
            database: Some("db".into()),
            name: "child".into(),
            kind: "table".into(),
            key_b64: None,
        };
        let snapshot = parse_ddl(target, "CREATE TABLE `child` (`id` int NOT NULL, `parent_id` int, PRIMARY KEY (`id`), KEY `parent_id` (`parent_id`), CONSTRAINT `fk_parent` FOREIGN KEY (`parent_id`) REFERENCES `parent` (`id`), CONSTRAINT `ck_id` CHECK ((`id` > 0))) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4", "").unwrap();
        assert_eq!(
            snapshot.completeness.state,
            CompletenessState::Complete,
            "{:?}",
            snapshot.completeness.reasons
        );
        let SchemaDefinition::Table(table) = snapshot.definition else {
            panic!("expected table")
        };
        assert!(table.constraints.iter().any(|constraint| matches!(constraint, ConstraintDefinition::ForeignKey { name, referenced_database, referenced_table, .. } if name == "fk_parent" && referenced_database == "db" && referenced_table == "parent")));
        assert!(table.constraints.iter().any(|constraint| matches!(constraint, ConstraintDefinition::Check { name, enforced: true, .. } if name == "ck_id")));
    }

    #[test]
    fn table_option_keys_must_be_single_identifiers() {
        use sqlparser::ast::Ident;
        let single = ObjectName::from(Ident::new("engine"));
        let qualified = ObjectName::from(vec![Ident::new("x"), Ident::new("ENGINE")]);
        assert!(option_key_is(&single, "ENGINE"));
        assert!(!option_key_is(&qualified, "ENGINE"));
    }

    #[test]
    fn preserves_index_direction_and_table_options() {
        let target = ResourceTarget {
            database: Some("db".into()),
            name: "t".into(),
            kind: "table".into(),
            key_b64: None,
        };
        let snapshot = parse_ddl(target, "CREATE TABLE t (a int, b int, KEY idx (a ASC, b DESC)) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 AUTO_INCREMENT=5", "").unwrap();
        assert_eq!(snapshot.completeness.state, CompletenessState::Incomplete);
        let SchemaDefinition::Table(table) = snapshot.definition else {
            panic!("expected table")
        };
        assert_eq!(table.engine, "InnoDB");
        assert_eq!(table.charset.as_deref(), Some("utf8mb4"));
        assert!(table.options.is_empty());
        assert!(!table.indexes[0].parts[0].descending);
        assert!(table.indexes[0].parts[1].descending);
    }
}
