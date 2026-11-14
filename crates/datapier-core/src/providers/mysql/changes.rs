use std::collections::{BTreeMap, HashSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value as Json, json};
use sqlx::{MySql, MySqlConnection, Row, mysql::MySqlArguments, query::Query};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{MysqlSession, catalog};
use crate::{
    ExecutionSink,
    model::preview::{
        ChangePreview, PreviewBudget, PreviewItem, PreviewStatement, preview_json, preview_page,
    },
    model::{AppError, Column, ResourceTarget, Result, RowChange, TableSchema, Value, param},
};

/// 令牌绑定打开时的表结构或视图映射，提交前重新证明。
#[derive(Clone)]
pub(super) struct EditToken {
    pub target: ResourceTarget,
    pub view_fingerprint: Option<String>,
    pub schema_fingerprint: Option<String>,
}

enum Bind {
    Null,
    Text(String),
    Bytes(Vec<u8>),
    Year(u16),
    Float32(f32),
    Float64(f64),
}

fn integer_fits(value: &str, base: &str, unsigned: bool) -> bool {
    let Ok(n) = value.parse::<i128>() else {
        return false;
    };
    if base == "year" {
        return n == 0 || (1901..=2155).contains(&n);
    }
    let bits = match base {
        "tinyint" => 8,
        "smallint" => 16,
        "mediumint" => 24,
        "int" | "integer" => 32,
        "bigint" => 64,
        _ => return false,
    };
    if unsigned {
        (0..=(1_i128 << bits) - 1).contains(&n)
    } else {
        (-(1_i128 << (bits - 1))..=(1_i128 << (bits - 1)) - 1).contains(&n)
    }
}

fn bit_width(ty: &str) -> Option<u32> {
    let rest = ty.strip_prefix("bit")?.trim();
    let width = if rest.is_empty() {
        1
    } else {
        rest.strip_prefix('(')?
            .strip_suffix(')')?
            .trim()
            .parse()
            .ok()?
    };
    (1..=64).contains(&width).then_some(width)
}

fn bit_bytes_fit(bytes: &[u8], width: u32) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let Some(index) = bytes.iter().position(|&byte| byte != 0) else {
        return true;
    };
    let significant = &bytes[index..];
    significant.len() <= 8
        && (significant.len() - 1) * 8 + (8 - significant[0].leading_zeros()) as usize
            <= width as usize
}

// 排除系数末尾的零后检查有效标度，允许等值表示但拒绝依赖服务端舍入。
fn decimal_fits(value: &str, ty: &str, unsigned: bool) -> bool {
    let Some((_, args)) = ty.split_once('(') else {
        return false;
    };
    let Some((args, _)) = args.split_once(')') else {
        return false;
    };
    let mut parts = args.split(',').map(str::trim);
    let (Some(precision), Some(scale), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    let (Ok(precision), Ok(scale)) = (precision.parse::<u32>(), scale.parse::<u32>()) else {
        return false;
    };
    if precision == 0 || precision > 65 || scale > 30 || scale > precision {
        return false;
    }
    let Ok(decimal) = value.parse::<bigdecimal::BigDecimal>() else {
        return false;
    };
    let (coefficient, exponent) = decimal.as_bigint_and_exponent();
    let digits = coefficient.to_string();
    let negative = digits.starts_with('-');
    let digits = digits.trim_start_matches('-').trim_start_matches('0');
    if digits.is_empty() {
        return true;
    }
    if unsigned && negative {
        return false;
    }
    let significant = digits.trim_end_matches('0');
    let trailing_zeros = digits.len() - significant.len();
    let effective_scale = i128::from(exponent) - trailing_zeros as i128;
    let integer_digits = significant.len() as i128 - effective_scale;
    effective_scale <= i128::from(scale) && integer_digits <= i128::from(precision - scale)
}

fn temporal_fits(value: &str, ty: &str) -> bool {
    let precision = match ty.split_once('(') {
        Some((_, rest)) => match rest
            .split_once(')')
            .and_then(|(n, _)| n.trim().parse::<usize>().ok())
        {
            Some(n) if n <= 6 => n,
            _ => return false,
        },
        None => 0,
    };
    match value.split_once('.') {
        None => true,
        Some((_, fraction)) => {
            !fraction.is_empty()
                && fraction.bytes().all(|c| c.is_ascii_digit())
                && fraction.bytes().skip(precision).all(|c| c == b'0')
        }
    }
}

// 精确数值按文本绑定以避免经浮点转换；浮点则按目标列精度绑定原生类型。
fn bind_value(value: &Value, column: &Column) -> Result<Bind> {
    let ty = column.type_name.to_ascii_lowercase();
    let base = ty
        .split(|c: char| c == '(' || c.is_ascii_whitespace())
        .next()
        .unwrap_or("");
    let unsigned = ty
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|part| matches!(part, "unsigned" | "zerofill"));
    let validation_error = |message: String, validation: &str| {
        AppError::invalid(message).with_details(json!({
            "validation": validation, "column": column.name, "type": column.type_name
        }))
    };
    // 严格模式仍允许截去超出 CHAR/VARCHAR 长度的尾随空格，必须在发送前拒绝。
    if let Value::Text(text) = value
        && matches!(base, "char" | "varchar")
        && let Some(length) = ty
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .and_then(|(length, _)| length.parse::<usize>().ok())
        && text.chars().count() > length
    {
        return Err(AppError::invalid(format!(
            "列 {} 超过字符长度 {}",
            column.name, length
        )).with_details(json!({"validation":"character_length", "column":column.name, "type":column.type_name, "limit":length})));
    }
    let valid = match value {
        Value::Null if column.nullable => return Ok(Bind::Null),
        Value::Null => {
            return Err(validation_error(
                format!("列 {} 不允许 NULL", column.name),
                "null_not_allowed",
            ));
        }
        Value::Bytes(v) if base == "bit" => {
            let bytes = STANDARD
                .decode(v)
                .map_err(|_| validation_error("无效的 Base64 二进制值".into(), "invalid_base64"))?;
            if !bit_width(&ty).is_some_and(|width| bit_bytes_fit(&bytes, width)) {
                return Err(validation_error(
                    format!(
                        "列 {} ({}) 的二进制值超出位宽或类型无效",
                        column.name, column.type_name
                    ),
                    "bit_width",
                ));
            }
            return Ok(Bind::Bytes(bytes));
        }
        Value::Bool(_) => matches!(base, "tinyint" | "bool" | "boolean" | "bit"),
        Value::Integer(v) => v.parse::<i64>().is_ok() && integer_fits(v, base, unsigned),
        Value::Unsigned(v) => v.parse::<u64>().is_ok_and(|n| {
            if base == "bit" {
                bit_width(&ty).is_some_and(|width| width == 64 || n < (1_u64 << width))
            } else {
                integer_fits(v, base, unsigned)
            }
        }),
        Value::Decimal(v) => {
            matches!(base, "decimal" | "numeric") && decimal_fits(v, &ty, unsigned)
        }
        Value::Float(v) => {
            matches!(base, "float" | "double" | "real")
                && v.parse::<f64>().is_ok_and(f64::is_finite)
        }
        Value::Text(_) => matches!(
            base,
            "char" | "varchar" | "tinytext" | "text" | "mediumtext" | "longtext" | "enum" | "set"
        ),
        Value::Bytes(_) => matches!(
            base,
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob" | "bit"
        ),
        Value::Date(_) => base == "date",
        Value::Time(v) => base == "time" && temporal_fits(v, &ty),
        Value::DateTime(v) => matches!(base, "datetime" | "timestamp") && temporal_fits(v, &ty),
        Value::Json(_) => base == "json",
        _ => false,
    };
    if !valid {
        let reason = if matches!(
            value,
            Value::Integer(_) | Value::Unsigned(_) | Value::Decimal(_)
        ) {
            "数值无效、超出范围或无法按目标精度和标度无损表示"
        } else {
            "值类型不受支持或不匹配"
        };
        return Err(validation_error(
            format!("列 {} ({})：{}", column.name, column.type_name, reason),
            if matches!(
                value,
                Value::Integer(_) | Value::Unsigned(_) | Value::Decimal(_)
            ) {
                "numeric_range"
            } else {
                "type_mismatch"
            },
        ));
    }
    Ok(match value {
        // BIT 接收二进制字符串时按字节值解释，绑定十进制文本会写入 ASCII 字节。
        Value::Bool(v) if base == "bit" => Bind::Bytes(vec![u8::from(*v)]),
        Value::Unsigned(v) if base == "bit" => {
            let n = v
                .parse::<u64>()
                .map_err(|_| validation_error("无效的位值".into(), "bit_width"))?;
            let width =
                bit_width(&ty).ok_or_else(|| validation_error("无效的位宽".into(), "bit_width"))?;
            let bytes = n.to_be_bytes();
            Bind::Bytes(bytes[8 - width.div_ceil(8) as usize..].to_vec())
        }
        // MySQL 将字符串 '0' 当作 2000 年，整数 0 才表示零年。
        Value::Integer(v) | Value::Unsigned(v) if base == "year" => Bind::Year(
            v.parse::<u16>()
                .map_err(|_| validation_error("无效的年份".into(), "numeric_range"))?,
        ),
        Value::Bool(v) => Bind::Text(if *v { "1" } else { "0" }.into()),
        Value::Time(v) | Value::DateTime(v) => {
            let value = if let Some((whole, fraction)) = v.split_once('.') {
                let fraction = fraction.trim_end_matches('0');
                if fraction.is_empty() {
                    whole.to_owned()
                } else {
                    format!("{whole}.{fraction}")
                }
            } else {
                v.clone()
            };
            Bind::Text(value)
        }
        Value::Integer(v)
        | Value::Unsigned(v)
        | Value::Decimal(v)
        | Value::Text(v)
        | Value::Date(v) => Bind::Text(v.clone()),
        Value::Float(v) if base == "float" => {
            let n = v
                .parse::<f32>()
                .map_err(|_| validation_error("无效的单精度浮点数".into(), "numeric_range"))?;
            if !n.is_finite() {
                return Err(validation_error("浮点数超出范围".into(), "numeric_range"));
            }
            Bind::Float32(n)
        }
        Value::Float(v) => Bind::Float64(
            v.parse::<f64>()
                .map_err(|_| validation_error("无效的浮点数".into(), "numeric_range"))?,
        ),
        Value::Bytes(v) => Bind::Bytes(
            STANDARD
                .decode(v)
                .map_err(|_| validation_error("无效的 Base64 二进制值".into(), "invalid_base64"))?,
        ),
        Value::Json(v) => Bind::Text(serde_json::to_string(v)?),
        _ => unreachable!(),
    })
}

/// 表单和导入预检复用写入绑定规则；生成列、类型不匹配和无法无损表示的值返回错误。
pub fn validate_import_value(value: &Value, column: &Column) -> Result<()> {
    if column.generated {
        return Err(AppError::invalid(format!("生成列不可写: {}", column.name)).with_details(json!({"validation":"generated_column", "column":column.name, "type":column.type_name})));
    }
    bind_value(value, column).map(|_| ())
}

struct StatementPlan {
    text: String,
    parameters: Vec<Bind>,
}
impl StatementPlan {
    fn new(text: &str) -> Self {
        Self {
            text: text.into(),
            parameters: Vec::new(),
        }
    }
    fn push(&mut self, text: impl AsRef<str>) -> &mut Self {
        self.text.push_str(text.as_ref());
        self
    }
    fn push_bind(&mut self, value: Bind) {
        self.text.push('?');
        self.parameters.push(value);
    }
    fn query(&self) -> Query<'_, MySql, MySqlArguments> {
        let mut q = sqlx::query(sqlx::AssertSqlSafe(self.text.as_str()));
        for value in &self.parameters {
            q = match value {
                Bind::Null => q.bind(Option::<&str>::None),
                Bind::Text(v) => q.bind(v.as_str()),
                Bind::Bytes(v) => q.bind(v.as_slice()),
                Bind::Year(v) => q.bind(*v),
                Bind::Float32(v) => q.bind(*v),
                Bind::Float64(v) => q.bind(*v),
            };
        }
        q
    }
    fn preview(&self, role: &'static str, budget: &mut PreviewBudget) -> PreviewStatement {
        let parameters = self
            .parameters
            .iter()
            .map(|bind| match bind {
                Bind::Null => budget.parameter("null", b"NULL", false),
                Bind::Text(v) => budget.parameter("text", v.as_bytes(), false),
                Bind::Bytes(v) => budget.parameter("bytes", v, false),
                Bind::Year(v) => {
                    let mut p = budget.parameter("year", v.to_string().as_bytes(), false);
                    p.byte_len = Some(2);
                    p
                }
                Bind::Float32(v) => {
                    let mut p = budget.parameter("float32", v.to_string().as_bytes(), false);
                    p.byte_len = Some(4);
                    p
                }
                Bind::Float64(v) => {
                    let mut p = budget.parameter("float64", v.to_string().as_bytes(), false);
                    p.byte_len = Some(8);
                    p
                }
            })
            .map(|mut p| {
                if p.kind == "null" {
                    p.byte_len = Some(0);
                }
                p
            })
            .collect();
        PreviewStatement {
            role,
            text: self.text.clone(),
            parameters,
        }
    }
}
fn push_bind(q: &mut StatementPlan, value: Bind) {
    q.push_bind(value);
}

fn table(schema: &TableSchema) -> Result<String> {
    let db = schema
        .target
        .database
        .as_deref()
        .ok_or_else(|| AppError::invalid("缺少数据库名称"))?;
    Ok(format!(
        "{}.{}",
        super::quote_ident(db)?,
        super::quote_ident(&schema.target.name)?
    ))
}

fn column<'a>(schema: &'a TableSchema, name: &str) -> Result<&'a Column> {
    schema
        .columns
        .iter()
        .find(|c| c.name == name)
        .ok_or_else(|| AppError::invalid(format!("列不存在: {name}")))
}

fn validate(schema: &TableSchema, change: &RowChange) -> Result<()> {
    if !matches!(change.operation.as_str(), "insert" | "update" | "delete") {
        return Err(AppError::invalid("未知行变更操作"));
    }
    if schema.target.kind == "view" {
        let allowed = schema
            .details
            .get("view_mapping")
            .and_then(|v| v.get(&change.operation))
            .and_then(Json::as_bool)
            .unwrap_or(false);
        if !allowed {
            return Err(AppError::invalid(format!(
                "视图不支持{}操作",
                change.operation
            )));
        }
    }
    for (name, value) in &change.values {
        let c = column(schema, name)?;
        if c.generated {
            return Err(AppError::invalid(format!("生成列不可写: {name}")));
        }
        bind_value(value, c)?;
    }
    if change.operation == "insert" {
        if schema.target.kind == "view" {
            let required = schema.details["view_mapping"]["required_insert_columns"]
                .as_array()
                .ok_or_else(|| AppError::invalid("视图必填列信息不可用"))?;
            for name in required {
                let name = name
                    .as_str()
                    .ok_or_else(|| AppError::invalid("视图必填列信息无效"))?;
                if !change.values.contains_key(name) {
                    return Err(AppError::invalid(format!("缺少视图必填列: {name}")));
                }
            }
        }
        return Ok(());
    }
    if change.operation == "update" && change.values.is_empty() {
        return Err(AppError::invalid("更新值不能为空"));
    }
    // 更新和删除必须同时提供完整可靠键及所有列的原值，才能检测并发修改。
    if schema.keys.is_empty() {
        return Err(AppError::invalid("表没有可靠键，不能编辑行"));
    }
    if change.before.len() != schema.columns.len() {
        return Err(AppError::invalid("必须提供全部列的原值"));
    }
    for c in &schema.columns {
        let v = change
            .before
            .get(&c.name)
            .ok_or_else(|| AppError::invalid(format!("缺少列 {} 的原值", c.name)))?;
        bind_value(v, c)?;
    }
    let key = schema
        .keys
        .iter()
        .find(|k| k.len() == change.key.len() && k.iter().all(|n| change.key.contains_key(n)))
        .ok_or_else(|| AppError::invalid("必须提供完整可靠键"))?;
    for name in key {
        let v = &change.key[name];
        if matches!(v, Value::Null) || change.before.get(name) != Some(v) {
            return Err(AppError::invalid("键值必须非空且与原值一致"));
        }
    }
    Ok(())
}

// 使用二进制比较避免排序规则、大小写和尾随空格被视为相等；JSON 先由
// MySQL 规范化再比较其可观察序列化结果。NULL、浮点、年份和时间按原生语义比较。
fn predicate(
    q: &mut StatementPlan,
    schema: &TableSchema,
    values: &BTreeMap<String, Value>,
) -> Result<()> {
    for (i, (name, value)) in values.iter().enumerate() {
        if i > 0 {
            q.push(" AND ");
        }
        let c = column(schema, name)?;
        let ident = super::quote_ident(name)?;
        if matches!(value, Value::Null) {
            q.push(ident).push(" IS NULL");
            continue;
        }
        if let Value::Decimal(text) = value {
            // 校验后以目标标度转成普通十进制文本，避免科学计数法经浮点解析。
            bind_value(value, c)?;
            let args = c
                .type_name
                .split_once('(')
                .and_then(|(_, s)| s.split_once(')'))
                .map(|(args, _)| args)
                .ok_or_else(|| AppError::invalid("无效 DECIMAL 类型"))?;
            let (precision, scale) = args
                .split_once(',')
                .ok_or_else(|| AppError::invalid("无效 DECIMAL 类型"))?;
            let precision: u32 = precision
                .trim()
                .parse()
                .map_err(|_| AppError::invalid("无效 DECIMAL 精度"))?;
            let scale: u32 = scale
                .trim()
                .parse()
                .map_err(|_| AppError::invalid("无效 DECIMAL 标度"))?;
            let decimal = text
                .parse::<bigdecimal::BigDecimal>()
                .map_err(|_| AppError::invalid("无效 DECIMAL 值"))?;
            q.push(ident).push(" <=> CAST(");
            push_bind(
                q,
                Bind::Text(decimal.with_scale(i64::from(scale)).to_plain_string()),
            );
            q.push(format!(" AS DECIMAL({precision},{scale}))"));
            continue;
        }
        if matches!(value, Value::Float(_))
            || c.type_name
                .split(|c: char| c == '(' || c.is_ascii_whitespace())
                .next()
                .is_some_and(|ty| ty.eq_ignore_ascii_case("year"))
        {
            q.push(ident).push(" <=> ");
            push_bind(q, bind_value(value, c)?);
            continue;
        }
        let cast = match value {
            Value::Date(_) => Some("DATE"),
            Value::Time(_) => Some("TIME(6)"),
            Value::DateTime(_) => Some("DATETIME(6)"),
            _ => None,
        };
        if let Some(ty) = cast {
            q.push(ident).push(" <=> CAST(");
            push_bind(q, bind_value(value, c)?);
            q.push(" AS ").push(ty).push(")");
        } else if matches!(value, Value::Text(_)) {
            q.push("BINARY CONVERT(")
                .push(ident)
                .push(" USING utf8mb4) <=> BINARY ");
            push_bind(q, bind_value(value, c)?);
        } else {
            q.push("BINARY ").push(ident).push(" <=> BINARY ");
            if matches!(value, Value::Json(_)) {
                q.push("CAST(");
                push_bind(q, bind_value(value, c)?);
                q.push(" AS JSON)");
            } else {
                push_bind(q, bind_value(value, c)?);
            }
        }
    }
    Ok(())
}

struct ChangePlan {
    check: Option<StatementPlan>,
    write: Option<StatementPlan>,
    insert: bool,
}

fn plan_change(
    schema: &TableSchema,
    change: &RowChange,
    transactional: bool,
) -> Result<ChangePlan> {
    let table = table(schema)?;
    let unchanged = change.operation == "update"
        && change
            .values
            .iter()
            .all(|(name, value)| change.before.get(name) == Some(value));
    let locking = transactional && schema.target.kind != "view";
    // 实际写入仍用锁或完整 WHERE 保护；同值项不写入，但必须确认当前完整基线仍存在。
    // 视图只查询视图自身，不扩大为对基表的直接读取或锁权限。
    let check = if change.operation != "insert" && (locking || unchanged) {
        let mut check = StatementPlan::new("SELECT 1 FROM ");
        check.push(&table).push(" WHERE ");
        predicate(&mut check, schema, &change.before)?;
        if locking {
            check.push(" FOR UPDATE");
        }
        Some(check)
    } else {
        None
    };
    if unchanged {
        return Ok(ChangePlan {
            check,
            write: None,
            insert: false,
        });
    }
    let mut q = StatementPlan::new("");
    match change.operation.as_str() {
        "insert" => {
            q.push("INSERT INTO ").push(&table).push(" (");
            for (i, name) in change.values.keys().enumerate() {
                if i > 0 {
                    q.push(", ");
                }
                q.push(super::quote_ident(name)?);
            }
            q.push(") VALUES (");
            for (i, (name, value)) in change.values.iter().enumerate() {
                if i > 0 {
                    q.push(", ");
                }
                push_bind(&mut q, bind_value(value, column(schema, name)?)?);
            }
            q.push(")");
        }
        "update" => {
            q.push("UPDATE ").push(&table).push(" SET ");
            for (i, (name, value)) in change.values.iter().enumerate() {
                if i > 0 {
                    q.push(", ");
                }
                q.push(super::quote_ident(name)?).push(" = ");
                push_bind(&mut q, bind_value(value, column(schema, name)?)?);
            }
            q.push(" WHERE ");
            predicate(
                &mut q,
                schema,
                if transactional && schema.target.kind != "view" {
                    &change.key
                } else {
                    &change.before
                },
            )?;
        }
        "delete" => {
            q.push("DELETE FROM ").push(&table).push(" WHERE ");
            predicate(
                &mut q,
                schema,
                if transactional && schema.target.kind != "view" {
                    &change.key
                } else {
                    &change.before
                },
            )?;
        }
        _ => return Err(AppError::invalid("未知行变更操作")),
    }
    Ok(ChangePlan {
        check,
        write: Some(q),
        insert: change.operation == "insert",
    })
}

async fn run_change(
    conn: &mut MySqlConnection,
    plan: &ChangePlan,
    transactional: bool,
) -> Result<()> {
    if let Some(check) = &plan.check
        && check
            .query()
            .fetch_optional(&mut *conn)
            .await
            .map_err(super::errors::mysql_error)?
            .is_none()
    {
        return Err(AppError::conflict("行已变更或不存在"));
    }
    let Some(write) = &plan.write else {
        return Ok(());
    };
    let result = write.query().execute(&mut *conn).await.map_err(|e| {
        if matches!(e, sqlx::Error::Database(_)) {
            let original = super::errors::mysql_error(e);
            if transactional {
                original
            } else {
                let mut unknown =
                    AppError::unknown(format!("非事务写入可能部分生效: {}", original.message));
                unknown.database_code = original.database_code.clone();
                let mut details = original.details.as_object().cloned().unwrap_or_default();
                details.insert("original_error".into(), json!(original));
                unknown.with_details(Json::Object(details))
            }
        } else {
            AppError::unknown(format!("写入结果无法确认: {e}"))
        }
    })?;
    if !plan.insert && result.rows_affected() == 0 {
        return Err(AppError::conflict("行已变更或不存在"));
    }
    Ok(())
}

fn editable(schema: &TableSchema) -> Result<()> {
    if schema.target.kind == "view" && schema.details.get("view_mapping").is_none() {
        return Err(AppError::invalid("视图映射无法证明，不能编辑"));
    }
    Ok(())
}

pub(super) async fn edit_schema(
    s: &mut MysqlSession,
    token: &EditToken,
    cancel: &CancellationToken,
) -> Result<TableSchema> {
    let raw = catalog::schema_raw(s, token.target.clone(), cancel).await?;
    if raw.target.kind != "view" {
        if token.view_fingerprint.is_some() {
            return Err(AppError::conflict("视图类型已变化"));
        }
        if token.schema_fingerprint.as_deref() != Some(catalog::schema_fingerprint(&raw)?.as_str())
        {
            return Err(AppError::conflict("表结构已变化"));
        }
        return Ok(raw);
    }
    let mapping = super::view::resolve(s, raw, cancel).await?;
    if token.view_fingerprint.as_deref() != Some(mapping.fingerprint.as_str()) {
        return Err(AppError::conflict("视图映射或依赖结构已变化"));
    }
    Ok(mapping.schema)
}

pub(super) async fn open(
    s: &mut MysqlSession,
    params: Json,
    cancel: &CancellationToken,
) -> Result<Json> {
    s.profile.ensure_write()?;
    let target: ResourceTarget = param(&params, "target")?;
    let raw = catalog::schema_raw(s, target, cancel).await?;
    let (schema, view_fingerprint) = if raw.target.kind == "view" {
        let mapping = super::view::resolve(s, raw, cancel).await?;
        (mapping.schema, Some(mapping.fingerprint))
    } else {
        (raw, None)
    };
    editable(&schema)?;
    let schema_fingerprint = if view_fingerprint.is_none() {
        Some(catalog::schema_fingerprint(&schema)?)
    } else {
        None
    };
    if schema.keys.is_empty() {
        return Err(AppError::invalid("表没有可靠键，不能编辑行"));
    }
    let token = Uuid::new_v4();
    s.edits.insert(
        token,
        EditToken {
            target: schema.target.clone(),
            view_fingerprint,
            schema_fingerprint,
        },
    );
    Ok(json!({"token":token,"schema":schema}))
}

pub(super) async fn discard(s: &mut MysqlSession, params: Json) -> Result<Json> {
    let token: Uuid = param(&params, "token")?;
    if s.edits.remove(&token).is_none() {
        return Err(AppError::invalid("编辑令牌已失效"));
    }
    Ok(json!({"discarded":true}))
}

async fn resolve_and_validate(
    schema: &TableSchema,
    change: &mut RowChange,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<()> {
    for value in change
        .key
        .values_mut()
        .chain(change.before.values_mut())
        .chain(change.values.values_mut())
    {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled());
        }
        if let Value::Large { id, .. } = value {
            let resolved = sink.store.read_value(*id).await?;
            if matches!(resolved, Value::Large { .. }) {
                return Err(AppError::invalid("大字段引用未解析为原始值"));
            }
            *value = resolved;
        }
    }
    validate(schema, change)
}

pub(super) async fn execute_batch(
    s: &mut MysqlSession,
    schema: &TableSchema,
    mut changes: Vec<RowChange>,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    let mut items: Vec<Json> = (0..changes.len())
        .map(|index| json!({"index":index,"status":"not_executed"}))
        .collect();
    if changes.is_empty() {
        return Ok(json!({"items":items,"affected_rows":0}));
    }
    for (index, change) in changes.iter_mut().enumerate() {
        if let Err(e) = resolve_and_validate(schema, change, sink, cancel).await {
            items[index] = json!({"index":index,"status":"failed","error":e});
            return Err(e.with_details(json!({"items":items})));
        }
    }
    if cancel.is_cancelled() {
        return Err(AppError::cancelled().with_details(json!({"items":items})));
    }
    // 只有 InnoDB 批次使用事务；其他引擎逐条生效，后续失败不代表前项回滚。
    let transactional = schema.engine.eq_ignore_ascii_case("innodb");
    let mut conn = s.new_connection().await?;
    // STRICT_ALL_TABLES 在非事务表也拒绝有损转换；独立连接不改变查询标签的 SQL 模式。
    sqlx::raw_sql("SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, IF(@@SESSION.sql_mode = '', '', ','), 'STRICT_ALL_TABLES')")
        .execute(&mut conn)
        .await
        .map_err(super::errors::mysql_error)?;
    if transactional {
        if schema.target.kind == "view" {
            // 无锁的视图同值校验须读取本次语句的新快照，不能复用批次前项的旧读视图。
            sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL READ COMMITTED")
                .execute(&mut conn)
                .await
                .map_err(super::errors::mysql_error)?;
        }
        sqlx::raw_sql("START TRANSACTION")
            .execute(&mut conn)
            .await
            .map_err(super::errors::mysql_error)?;
    }
    let mut failure: Option<(usize, AppError)> = None;
    let mut affected = 0usize;
    for (index, change) in changes.iter().enumerate() {
        if cancel.is_cancelled() {
            failure = Some((index, AppError::cancelled()));
            break;
        }
        let plan = match plan_change(schema, change, transactional) {
            Ok(plan) => plan,
            Err(error) => {
                failure = Some((index, error));
                break;
            }
        };
        if plan.write.is_some()
            && let Err(e) = sink.mark_sent(true).await
        {
            failure = Some((index, e));
            break;
        }
        match run_change(&mut conn, &plan, transactional).await {
            Ok(()) => {
                if plan.write.is_some() {
                    items[index] = json!({"index":index,"status":"applied"});
                    affected += 1;
                } else {
                    items[index] = json!({"index":index,"status":"unchanged"});
                }
            }
            Err(e) => {
                failure = Some((index, e));
                break;
            }
        }
    }
    // 最后一条语句执行期间也可能收到取消；COMMIT 前仍可安全回滚。
    if failure.is_none() && transactional && cancel.is_cancelled() {
        failure = Some((changes.len(), AppError::cancelled()));
    }
    if let Some((index, mut error)) = failure {
        let unknown = error.code == "outcome_unknown";
        if index < items.len() {
            items[index] = json!({"index":index,"status":if unknown {"unknown"} else if error.code == "cancelled" {"not_executed"} else if error.code == "conflict" {"conflict"} else {"failed"},"error":error});
        }
        // 事务回滚成功仍可能留下非事务表的修改，须在同一连接读取回滚诊断。
        if transactional {
            let rolled_back = if sqlx::raw_sql("ROLLBACK").execute(&mut conn).await.is_ok() {
                // COUNT 不受 max_error_count 限制；原始查询避免预处理覆盖诊断区。
                sqlx::raw_sql("SHOW COUNT(*) WARNINGS")
                    .fetch_optional(&mut conn)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|row| {
                        row.try_get::<u64, _>(0).is_ok_and(|count| count == 0)
                            || row.try_get::<i64, _>(0).is_ok_and(|count| count == 0)
                    })
            } else {
                false
            };
            for item in &mut items[..index] {
                if item["status"] == "unchanged" {
                    continue;
                }
                item["status"] = json!(if rolled_back {
                    "rolled_back"
                } else {
                    "unknown"
                });
            }
            if unknown && rolled_back && index < items.len() {
                items[index]["status"] = json!("rolled_back");
            }
            if !rolled_back {
                error = AppError::unknown("事务回滚结果无法确认");
            }
        }
        let mut details = match error.details.as_ref() {
            Json::Object(map) => map.clone(),
            _ => serde_json::Map::new(),
        };
        details.insert("items".into(), json!(items));
        return Err(error.with_details(Json::Object(details)));
    }
    // COMMIT 失败无法区分提交未发生和响应丢失，不自动重放。
    if transactional && let Err(e) = sqlx::raw_sql("COMMIT").execute(&mut conn).await {
        for item in &mut items {
            item["status"] = json!("unknown");
        }
        return Err(AppError::unknown(format!("事务提交结果无法确认: {e}"))
            .with_details(json!({"items":items})));
    }
    Ok(json!({"items":items,"affected_rows":affected}))
}

pub(super) async fn preview(
    s: &mut MysqlSession,
    params: Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    s.profile.ensure_write()?;
    let token: Uuid = param(&params, "token")?;
    let mut changes: Vec<RowChange> = param(&params, "rows")?;
    let edit = s
        .edits
        .get(&token)
        .cloned()
        .ok_or_else(|| AppError::invalid("编辑令牌已失效"))?;
    let schema = edit_schema(s, &edit, cancel).await?;
    editable(&schema)?;
    for change in &mut changes {
        resolve_and_validate(&schema, change, sink, cancel).await?;
    }
    if cancel.is_cancelled() {
        return Err(AppError::cancelled());
    }
    let transactional = schema.engine.eq_ignore_ascii_case("innodb");
    let total = changes.len();
    let (offset, end, next_offset) = preview_page(&params, total)?;
    let mut budget = PreviewBudget::new();
    let mut items = Vec::new();
    for (index, change) in changes
        .iter()
        .enumerate()
        .skip(offset)
        .take(end.saturating_sub(offset))
    {
        if cancel.is_cancelled() {
            return Err(AppError::cancelled());
        }
        let mut statements = Vec::new();
        let plan = plan_change(&schema, change, transactional)?;
        if let Some(check) = &plan.check {
            statements.push(check.preview("check", &mut budget));
        }
        if let Some(write) = &plan.write {
            statements.push(write.preview("write", &mut budget));
        }
        items.push(PreviewItem {
            index: Some(index),
            operation: change.operation.clone(),
            statements,
        });
    }
    preview_json(ChangePreview {
        provider: "mysql",
        transactional,
        total_changes: total,
        offset,
        next_offset,
        items,
        notes: Vec::new(),
    })
}

pub(super) async fn submit(
    s: &mut MysqlSession,
    params: Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    s.profile.ensure_write()?;
    let token: Uuid = param(&params, "token")?;
    let changes: Vec<RowChange> = param(&params, "rows")?;
    // 提交时消费令牌并重新读取 schema，失败后也不能复用旧编辑基线。
    let edit = s
        .edits
        .remove(&token)
        .ok_or_else(|| AppError::invalid("编辑令牌已失效"))?;
    let schema = edit_schema(s, &edit, cancel).await?;
    editable(&schema)?;
    execute_batch(s, &schema, changes, sink, cancel).await
}

pub(super) async fn import(
    s: &mut MysqlSession,
    params: Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    s.profile.ensure_write()?;
    let target: ResourceTarget = param(&params, "target")?;
    let columns: Vec<String> = param(&params, "columns")?;
    let rows: Vec<Vec<Value>> = param(&params, "rows")?;
    if rows.len() > 500 {
        return Err(AppError::invalid("导入批次最多 500 行"));
    }
    let mut seen = HashSet::new();
    if columns.iter().any(|c| !seen.insert(c)) {
        return Err(AppError::invalid("导入列重复"));
    }
    let schema = catalog::schema(s, target, cancel).await?;
    editable(&schema)?;
    let mut changes = Vec::with_capacity(rows.len());
    for row in rows {
        if row.len() != columns.len() {
            return Err(AppError::invalid("导入行的列数不匹配"));
        }
        changes.push(RowChange {
            operation: "insert".into(),
            key: BTreeMap::new(),
            before: BTreeMap::new(),
            values: columns.iter().cloned().zip(row).collect(),
        });
    }
    execute_batch(s, &schema, changes, sink, cancel).await
}

#[cfg(test)]
#[path = "../../../tests/unit/mysql/changes.rs"]
mod tests;

#[cfg(test)]
#[path = "../../../tests/unit/mysql/changes_view.rs"]
mod view_tests;
