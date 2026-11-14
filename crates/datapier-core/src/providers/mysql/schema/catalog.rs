//! 从 SHOW CREATE 和 information_schema 联合读取表结构。
use crate::model::{AppError, Result, schema::*};
use sqlparser::{
    ast::{ColumnOption, Expr, GeneratedExpressionMode, Statement, TableConstraint, Value},
    dialect::MySqlDialect,
    parser::Parser,
};
use sqlx::{MySqlConnection, Row};

// 版本注释是可执行语法，不能由通用 SQL 解析器按普通注释丢弃。
pub(super) fn expand_version_comments(ddl: &str, version: &str, mode: &str) -> Result<String> {
    let (major, minor, patch) = super::plan::version(version);
    let current = major
        .saturating_mul(10000)
        .saturating_add(minor.saturating_mul(100))
        .saturating_add(patch);
    let bytes = ddl.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    let mut quote = 0;
    let no_backslash = mode
        .split(',')
        .any(|m| m.trim().eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"));
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            out.push(c);
            i += 1;
            // sqlparser 的 MySQL 词法器始终启用反斜线转义；加倍可保留当前模式下的原始值。
            if c == b'\\' && quote != b'`' && no_backslash {
                out.push(c);
            }
            if c == b'\\' && quote != b'`' && !no_backslash && i < bytes.len() {
                out.push(bytes[i]);
                i += 1;
            } else if c == quote {
                if bytes.get(i) == Some(&quote) {
                    out.push(quote);
                    i += 1;
                } else {
                    quote = 0;
                }
            }
            continue;
        }
        if c == b'\'' || c == b'"' || c == b'`' {
            quote = c;
            out.push(c);
            i += 1;
            continue;
        }
        if bytes[i..].starts_with(b"/*!") {
            let start = i + 3;
            let end = ddl[start..]
                .find("*/")
                .map(|n| start + n)
                .ok_or_else(|| AppError::invalid("SHOW CREATE 版本注释未闭合"))?;
            let body = &ddl[start..end];
            let count = body.bytes().take_while(u8::is_ascii_digit).count();
            let required = if count == 0 {
                0
            } else {
                body[..count]
                    .parse::<u32>()
                    .map_err(|_| AppError::invalid("版本注释版本无效"))?
            };
            if current >= required {
                let sql = body[count..].trim();
                // 索引可见性由 STATISTICS.IS_VISIBLE 读取，sqlparser 不支持该选项。
                let line = out.rsplit(|b| *b == b'\n').next().unwrap_or_default();
                let line = std::str::from_utf8(line)
                    .unwrap_or("")
                    .trim_start()
                    .to_ascii_uppercase();
                let index_line = ["KEY ", "UNIQUE ", "PRIMARY KEY", "FULLTEXT ", "SPATIAL "]
                    .iter()
                    .any(|prefix| line.starts_with(prefix));
                if !(index_line
                    && (sql.eq_ignore_ascii_case("INVISIBLE")
                        || sql.eq_ignore_ascii_case("VISIBLE")))
                {
                    out.push(b' ');
                    out.extend_from_slice(sql.as_bytes());
                    out.push(b' ');
                }
            }
            i = end + 2;
        } else if bytes[i..].starts_with(b"/*") {
            let start = i + 2;
            let end = ddl[start..]
                .find("*/")
                .map(|n| start + n + 2)
                .ok_or_else(|| AppError::invalid("SHOW CREATE 注释未闭合"))?;
            out.extend_from_slice(&bytes[i..end]);
            i = end;
        } else {
            out.push(c);
            i += 1;
        }
    }
    String::from_utf8(out).map_err(|_| AppError::invalid("SHOW CREATE 编码无效"))
}

fn index_options(
    options: Vec<sqlparser::ast::IndexOption>,
    mode: &str,
) -> (Option<String>, Vec<String>) {
    let mut method = None;
    let mut preserved = Vec::new();
    for option in options {
        match option {
            sqlparser::ast::IndexOption::Using(value) => method = Some(value.to_string()),
            sqlparser::ast::IndexOption::Comment(value) => {
                preserved.push(format!("COMMENT {}", super::plan::literal(&value, mode)))
            }
        }
    }
    (method, preserved)
}

pub(super) fn table_options(options: sqlparser::ast::CreateTableOptions) -> Result<Vec<String>> {
    use sqlparser::ast::{CreateTableOptions, SqlOption};
    let values = match options {
        CreateTableOptions::None => return Ok(Vec::new()),
        CreateTableOptions::Plain(values) => values,
        _ => return Err(AppError::invalid("不支持此表选项格式")),
    };
    let mut preserved = Vec::new();
    for option in values {
        match &option {
            SqlOption::KeyValue { key, .. }
                if [
                    "ENGINE",
                    "CHARSET",
                    "DEFAULT CHARSET",
                    "CHARACTER SET",
                    "DEFAULT CHARACTER SET",
                    "COLLATE",
                    "DEFAULT COLLATE",
                    "AUTO_INCREMENT",
                    "COMMENT",
                ]
                .iter()
                .any(|v| key.value.eq_ignore_ascii_case(v)) => {}
            // sqlparser 表示 MySQL 的 ENGINE=InnoDB 为具名列表，实际引擎已从元数据读取。
            SqlOption::NamedParenthesizedList(value)
                if value.key.value.eq_ignore_ascii_case("ENGINE")
                    && value.name.is_some()
                    && value.values.is_empty() => {}
            SqlOption::Comment(_) => (),
            _ => preserved.push(option.to_string()),
        }
    }
    Ok(preserved)
}
// CREATE TABLE 的省略项由目标数据库或显式字符集决定，而非连接默认值。
pub(super) async fn creation_defaults(
    conn: &mut MySqlConnection,
    db: &str,
    table: &TableDefinition,
) -> Result<(String, String)> {
    let row = sqlx::query("SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?")
        .bind(db)
        .fetch_one(&mut *conn)
        .await?;
    let database_charset: String = row.try_get(0)?;
    let database_collation: String = row.try_get(1)?;
    if let Some(collation) = &table.collation {
        let charset: String = sqlx::query_scalar(
            "SELECT CHARACTER_SET_NAME FROM information_schema.COLLATIONS WHERE COLLATION_NAME = ?",
        )
        .bind(collation)
        .fetch_one(&mut *conn)
        .await?;
        return Ok((table.charset.clone().unwrap_or(charset), collation.clone()));
    }
    if let Some(charset) = &table.charset {
        let collation: String = sqlx::query_scalar("SELECT DEFAULT_COLLATE_NAME FROM information_schema.CHARACTER_SETS WHERE CHARACTER_SET_NAME = ?")
            .bind(charset)
            .fetch_one(&mut *conn)
            .await?;
        return Ok((charset.clone(), collation));
    }
    Ok((database_charset, database_collation))
}

// sqlparser 按普通 MySQL 模式解码字符串；将无反斜线转义模式转换为等值输入。
pub(super) fn parser_input(ddl: &str, mode: &str) -> String {
    if !mode
        .split(',')
        .any(|m| m.trim().eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"))
    {
        return ddl.to_owned();
    }
    let ansi_quotes = mode
        .split(',')
        .any(|m| m.trim().eq_ignore_ascii_case("ANSI_QUOTES"));
    let mut out = String::with_capacity(ddl.len());
    let mut quote = None;
    let mut chars = ddl.chars().peekable();
    while let Some(c) = chars.next() {
        out.push(c);
        if let Some(delimiter) = quote {
            if c == delimiter {
                if chars.peek() == Some(&delimiter) {
                    out.push(chars.next().unwrap());
                } else {
                    quote = None;
                }
            } else if c == '\\' && delimiter != '`' && !(delimiter == '"' && ansi_quotes) {
                out.push('\\');
            }
        } else if matches!(c, '\'' | '"' | '`') {
            quote = Some(c);
        }
    }
    out
}

pub(super) fn parsed_native_type(ty: &sqlparser::ast::DataType, mode: &str) -> Result<String> {
    use sqlparser::ast::{DataType, EnumMember};
    let (name, members): (&str, Vec<&str>) = match ty {
        DataType::Enum(values, None) => (
            "ENUM",
            values
                .iter()
                .map(|v| match v {
                    EnumMember::Name(s) => Ok(s.as_str()),
                    _ => Err(AppError::invalid("ENUM 成员含不受支持的值")),
                })
                .collect::<Result<_>>()?,
        ),
        DataType::Set(values) => ("SET", values.iter().map(String::as_str).collect()),
        _ => return Ok(ty.to_string()),
    };
    Ok(format!(
        "{}({})",
        name,
        members
            .iter()
            .map(|v| super::plan::literal(v, mode))
            .collect::<Vec<_>>()
            .join(", ")
    ))
}

pub(super) async fn read_table(
    conn: &mut MySqlConnection,
    db: &str,
    name: &str,
    ddl: &str,
    version: &str,
    mode: &str,
) -> Result<(TableDefinition, Completeness)> {
    let expanded = expand_version_comments(ddl, version, mode)?;
    let parser_ddl = parser_input(&expanded, mode);
    let mut statements = Parser::parse_sql(&MySqlDialect {}, &parser_ddl)
        .map_err(|e| AppError::invalid(format!("SHOW CREATE 无法完整解析: {e}")))?;
    if statements.len() != 1 {
        return Err(AppError::invalid("SHOW CREATE 不是单条语句"));
    }
    let Statement::CreateTable(table) = statements.remove(0) else {
        return Err(AppError::invalid("SHOW CREATE 不是表定义"));
    };
    let preserved_table_options = table_options(table.table_options.clone())?;
    let mut reasons = Vec::new();
    let mut columns = Vec::new();
    for c in table.columns {
        let mut out = ColumnDefinition {
            id: c.name.value.clone(),
            name: c.name.value,
            native_type: parsed_native_type(&c.data_type, mode)?,
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
        for opt in c.options {
            if opt.name.is_some() {
                reasons.push(format!("字段 {} 的具名选项不能保留", out.name));
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
                    };
                }
                ColumnOption::OnUpdate(e) => out.on_update = Some(e.to_string()),
                ColumnOption::Invisible => out.visible = false,
                ColumnOption::Comment(v) => out.comment = v,
                ColumnOption::CharacterSet(v) => {
                    out.charset = Some(v.to_string().trim_matches('`').to_owned())
                }
                ColumnOption::Collation(v) => {
                    out.collation = Some(v.to_string().trim_matches('`').to_owned())
                }
                ColumnOption::Generated {
                    generation_expr: Some(expr),
                    generation_expr_mode,
                    ..
                } => {
                    out.generated = Some(GeneratedColumn {
                        expression: expr.to_string(),
                        stored: matches!(
                            generation_expr_mode,
                            Some(GeneratedExpressionMode::Stored)
                        ),
                    })
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
                other => {
                    reasons.push(format!("字段 {} 未识别属性 {}", out.name, other));
                }
            }
        }
        columns.push(out);
    }
    let mut constraints = Vec::new();
    let mut special_indexes = std::collections::HashMap::new();
    let mut index_attributes = std::collections::HashMap::new();
    for c in table.constraints {
        match c {
            TableConstraint::ForeignKey(f) => {
                let Some(name) = f.name.map(|n| n.value) else {
                    reasons.push("外键缺少名称".into());
                    continue;
                };
                if f.match_kind.is_some() || f.characteristics.is_some() {
                    reasons.push(format!("外键 {name} 含未知选项"));
                }
                let parts: Vec<_> = f
                    .foreign_table
                    .to_string()
                    .split('.')
                    .map(|s| s.trim_matches('`').to_owned())
                    .collect();
                let (referenced_database, referenced_table) = match parts.as_slice() {
                    [table] => (db.to_owned(), table.clone()),
                    [database, table] => (database.clone(), table.clone()),
                    _ => {
                        reasons.push(format!("外键 {name} 引用无法解析"));
                        continue;
                    }
                };
                constraints.push(ConstraintDefinition::ForeignKey {
                    id: name.clone(),
                    name,
                    columns: f.columns.into_iter().map(|x| x.value).collect(),
                    referenced_database,
                    referenced_table,
                    referenced_columns: f.referred_columns.into_iter().map(|x| x.value).collect(),
                    on_update: f
                        .on_update
                        .map(|x| x.to_string())
                        .unwrap_or_else(|| "RESTRICT".into()),
                    on_delete: f
                        .on_delete
                        .map(|x| x.to_string())
                        .unwrap_or_else(|| "RESTRICT".into()),
                });
            }
            TableConstraint::Check(c) => {
                if let Some(name) = c.name {
                    constraints.push(ConstraintDefinition::Check {
                        id: name.value.clone(),
                        name: name.value,
                        expression: c.expr.to_string(),
                        enforced: c.enforced.unwrap_or(true),
                    });
                } else {
                    reasons.push("CHECK 缺少名称".into());
                }
            }
            TableConstraint::PrimaryKey(i) => {
                if i.characteristics.is_some() {
                    reasons.push("主键包含未识别约束属性".into());
                }
                let (method, options) = index_options(i.index_options, mode);
                index_attributes.insert(
                    "PRIMARY".to_owned(),
                    (
                        method.or_else(|| i.index_type.map(|v| v.to_string())),
                        options,
                    ),
                );
            }
            TableConstraint::Unique(i) => {
                if i.characteristics.is_some() {
                    reasons.push("唯一索引包含未识别约束属性".into());
                }
                if let Some(name) = i.index_name.or(i.name) {
                    let (method, options) = index_options(i.index_options, mode);
                    index_attributes.insert(
                        name.value,
                        (
                            method.or_else(|| i.index_type.map(|v| v.to_string())),
                            options,
                        ),
                    );
                } else {
                    reasons.push("唯一索引缺少服务端名称".into());
                }
            }
            TableConstraint::Index(i) => {
                if let Some(name) = i.name {
                    let (method, options) = index_options(i.index_options, mode);
                    index_attributes.insert(
                        name.value,
                        (
                            method.or_else(|| i.index_type.map(|v| v.to_string())),
                            options,
                        ),
                    );
                } else {
                    reasons.push("索引缺少服务端名称".into());
                }
            }
            TableConstraint::FulltextOrSpatial(i) => {
                if let Some(name) = i.opt_index_name {
                    special_indexes.insert(
                        name.value,
                        if i.fulltext {
                            IndexKind::Fulltext
                        } else {
                            IndexKind::Spatial
                        },
                    );
                }
            }
            _ => reasons.push("未知表约束".into()),
        }
    }
    let v = super::plan::version(version);
    let query = if v >= (8, 0, 13) {
        "SELECT INDEX_NAME, CAST(NON_UNIQUE AS SIGNED) AS NON_UNIQUE, COLUMN_NAME, CAST(SUB_PART AS SIGNED) AS SUB_PART, EXPRESSION, INDEX_TYPE, COLLATION, IS_VISIBLE FROM information_schema.STATISTICS WHERE TABLE_SCHEMA=? AND TABLE_NAME=? ORDER BY INDEX_NAME, SEQ_IN_INDEX"
    } else if v >= (8, 0, 0) {
        "SELECT INDEX_NAME, CAST(NON_UNIQUE AS SIGNED) AS NON_UNIQUE, COLUMN_NAME, CAST(SUB_PART AS SIGNED) AS SUB_PART, NULL AS EXPRESSION, INDEX_TYPE, COLLATION, IS_VISIBLE FROM information_schema.STATISTICS WHERE TABLE_SCHEMA=? AND TABLE_NAME=? ORDER BY INDEX_NAME, SEQ_IN_INDEX"
    } else {
        "SELECT INDEX_NAME, CAST(NON_UNIQUE AS SIGNED) AS NON_UNIQUE, COLUMN_NAME, CAST(SUB_PART AS SIGNED) AS SUB_PART, NULL AS EXPRESSION, INDEX_TYPE, COLLATION, 'YES' AS IS_VISIBLE FROM information_schema.STATISTICS WHERE TABLE_SCHEMA=? AND TABLE_NAME=? ORDER BY INDEX_NAME, SEQ_IN_INDEX"
    };
    let rows = sqlx::query(query)
        .bind(db)
        .bind(name)
        .fetch_all(&mut *conn)
        .await?;
    let mut indexes: Vec<IndexDefinition> = Vec::new();
    for row in rows {
        let n: String = row.try_get("INDEX_NAME")?;
        let method: String = row.try_get("INDEX_TYPE")?;
        if indexes.last().is_none_or(|i| i.name != n) {
            let unique: i64 = row.try_get("NON_UNIQUE")?;
            let kind = if n == "PRIMARY" {
                IndexKind::Primary
            } else if let Some(kind) = special_indexes.get(&n) {
                kind.clone()
            } else if method.eq_ignore_ascii_case("FULLTEXT") {
                IndexKind::Fulltext
            } else if unique == 0 {
                IndexKind::Unique
            } else {
                IndexKind::Normal
            };
            let (explicit_method, options) = index_attributes.remove(&n).unwrap_or_default();
            indexes.push(IndexDefinition {
                id: n.clone(),
                name: n,
                kind,
                parts: vec![],
                visible: row.try_get::<String, _>("IS_VISIBLE")? == "YES",
                method: explicit_method.or_else(|| {
                    if ["BTREE", "FULLTEXT", "SPATIAL", "RTREE"]
                        .iter()
                        .any(|m| method.eq_ignore_ascii_case(m))
                    {
                        None
                    } else {
                        Some(method)
                    }
                }),
                options,
            });
        }
        let part = IndexPart {
            column: row.try_get("COLUMN_NAME")?,
            expression: row.try_get("EXPRESSION")?,
            prefix_length: row.try_get::<Option<i64>, _>("SUB_PART")?.map(|x| x as u32),
            descending: row.try_get::<Option<String>, _>("COLLATION")?.as_deref() == Some("D"),
        };
        indexes.last_mut().expect("index exists").parts.push(part);
    }
    let row = sqlx::query("SELECT ENGINE,TABLE_COLLATION,TABLE_COMMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA=? AND TABLE_NAME=?").bind(db).bind(name).fetch_one(&mut *conn).await?;
    let collation: Option<String> = row.try_get("TABLE_COLLATION")?;
    let charset: Option<String> = if let Some(c) = &collation {
        sqlx::query_scalar(
            "SELECT CHARACTER_SET_NAME FROM information_schema.COLLATIONS WHERE COLLATION_NAME=?",
        )
        .bind(c)
        .fetch_optional(&mut *conn)
        .await?
    } else {
        None
    };
    let state = if reasons.is_empty() {
        CompletenessState::Complete
    } else {
        CompletenessState::Incomplete
    };
    Ok((
        TableDefinition {
            columns,
            indexes,
            constraints,
            engine: row
                .try_get::<Option<String>, _>("ENGINE")?
                .unwrap_or_default(),
            charset,
            collation,
            comment: row.try_get("TABLE_COMMENT")?,
            options: preserved_table_options,
        },
        Completeness { state, reasons },
    ))
}
