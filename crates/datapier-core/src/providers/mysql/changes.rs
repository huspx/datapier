use std::collections::{BTreeMap, HashSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value as Json, json};
use sqlx::{MySql, MySqlConnection, QueryBuilder};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{MysqlSession, catalog};
use crate::{
    ExecutionSink,
    model::{AppError, Column, ResourceTarget, Result, RowChange, TableSchema, Value, param},
};

#[derive(Clone)]
enum Bind {
    Null,
    Text(String),
    Bytes(Vec<u8>),
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
    let valid = match value {
        Value::Null if column.nullable => return Ok(Bind::Null),
        Value::Null => return Err(AppError::invalid(format!("列 {} 不允许 NULL", column.name))),
        Value::Bytes(v) if base == "bit" => {
            let bytes = STANDARD
                .decode(v)
                .map_err(|_| AppError::invalid("无效的 Base64 二进制值"))?;
            if !bit_width(&ty).is_some_and(|width| bit_bytes_fit(&bytes, width)) {
                return Err(AppError::invalid(format!(
                    "列 {} ({}) 的二进制值超出位宽或类型无效",
                    column.name, column.type_name
                )));
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
        Value::Time(_) => base == "time",
        Value::DateTime(_) => matches!(base, "datetime" | "timestamp"),
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
        return Err(AppError::invalid(format!(
            "列 {} ({})：{}",
            column.name, column.type_name, reason
        )));
    }
    Ok(match value {
        Value::Bool(v) => Bind::Text(if *v { "1" } else { "0" }.into()),
        Value::Integer(v)
        | Value::Unsigned(v)
        | Value::Decimal(v)
        | Value::Text(v)
        | Value::Date(v)
        | Value::Time(v)
        | Value::DateTime(v) => Bind::Text(v.clone()),
        Value::Float(v) if base == "float" => {
            let n = v
                .parse::<f32>()
                .map_err(|_| AppError::invalid("无效的单精度浮点数"))?;
            if !n.is_finite() {
                return Err(AppError::invalid("浮点数超出范围"));
            }
            Bind::Float32(n)
        }
        Value::Float(v) => Bind::Float64(
            v.parse::<f64>()
                .map_err(|_| AppError::invalid("无效的浮点数"))?,
        ),
        Value::Bytes(v) => Bind::Bytes(
            STANDARD
                .decode(v)
                .map_err(|_| AppError::invalid("无效的 Base64 二进制值"))?,
        ),
        Value::Json(v) => Bind::Text(serde_json::to_string(v)?),
        _ => unreachable!(),
    })
}

/// 导入预检复用写入绑定规则；生成列、类型不匹配和无法无损表示的值返回错误。
pub(crate) fn validate_import_value(value: &Value, column: &Column) -> Result<()> {
    if column.generated {
        return Err(AppError::invalid(format!("生成列不可写: {}", column.name)));
    }
    bind_value(value, column).map(|_| ())
}

fn push_bind(q: &mut QueryBuilder<MySql>, value: Bind) {
    match value {
        Bind::Null => {
            q.push_bind(Option::<String>::None);
        }
        Bind::Text(v) => {
            q.push_bind(v);
        }
        Bind::Bytes(v) => {
            q.push_bind(v);
        }
        Bind::Float32(v) => {
            q.push_bind(v);
        }
        Bind::Float64(v) => {
            q.push_bind(v);
        }
    }
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
    for (name, value) in &change.values {
        let c = column(schema, name)?;
        if c.generated {
            return Err(AppError::invalid(format!("生成列不可写: {name}")));
        }
        bind_value(value, c)?;
    }
    if change.operation == "insert" {
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
// MySQL 规范化再比较其可观察序列化结果。NULL、浮点和时间分别按原生语义比较。
fn predicate(
    q: &mut QueryBuilder<MySql>,
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
        if matches!(value, Value::Float(_)) {
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

async fn run_change(
    conn: &mut MySqlConnection,
    schema: &TableSchema,
    change: &RowChange,
    transactional: bool,
) -> std::result::Result<(), AppError> {
    let table = table(schema)?;
    // InnoDB 先锁住与完整原值匹配的行，再按可靠键写入；其他引擎直接在
    // 写语句中比较完整原值，避免独立检查与写入之间的竞态。
    if transactional && change.operation != "insert" {
        let mut check = QueryBuilder::<MySql>::new("SELECT 1 FROM ");
        check.push(&table).push(" WHERE ");
        predicate(&mut check, schema, &change.before)?;
        check.push(" FOR UPDATE");
        if check
            .build()
            .fetch_optional(&mut *conn)
            .await
            .map_err(super::errors::mysql_error)?
            .is_none()
        {
            return Err(AppError::conflict("行已变更或不存在"));
        }
    }
    let mut q = QueryBuilder::<MySql>::new("");
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
                if transactional {
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
                if transactional {
                    &change.key
                } else {
                    &change.before
                },
            )?;
        }
        _ => return Err(AppError::invalid("未知行变更操作")),
    }
    let result = q.build().execute(&mut *conn).await.map_err(|e| {
        if matches!(e, sqlx::Error::Database(_)) {
            super::errors::mysql_error(e)
        } else {
            AppError::unknown(format!("写入结果无法确认: {e}"))
        }
    })?;
    if change.operation != "insert" && result.rows_affected() == 0 {
        return Err(AppError::conflict("行已变更或不存在"));
    }
    Ok(())
}

fn editable(schema: &TableSchema) -> Result<()> {
    if schema.engine.is_empty() {
        return Err(AppError::invalid("视图不可编辑"));
    }
    Ok(())
}

pub(super) async fn open(s: &mut MysqlSession, params: Json) -> Result<Json> {
    s.profile.ensure_write()?;
    let target: ResourceTarget = param(&params, "target")?;
    let schema = catalog::schema(s, target).await?;
    editable(&schema)?;
    if schema.keys.is_empty() {
        return Err(AppError::invalid("表没有可靠键，不能编辑行"));
    }
    let token = Uuid::new_v4();
    s.edits.insert(token, schema.target.clone());
    Ok(json!({"token":token,"schema":schema}))
}

pub(super) async fn discard(s: &mut MysqlSession, params: Json) -> Result<Json> {
    let token: Uuid = param(&params, "token")?;
    if s.edits.remove(&token).is_none() {
        return Err(AppError::invalid("编辑令牌已失效"));
    }
    Ok(json!({"discarded":true}))
}

async fn execute_batch(
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
        for value in change
            .key
            .values_mut()
            .chain(change.before.values_mut())
            .chain(change.values.values_mut())
        {
            if cancel.is_cancelled() {
                return Err(AppError::cancelled().with_details(json!({"items":items})));
            }
            if let Value::Large { id, .. } = value {
                let id = *id;
                match sink.store.read_value(id).await {
                    Ok(resolved) if !matches!(resolved, Value::Large { .. }) => *value = resolved,
                    Ok(_) => {
                        let error = AppError::invalid("大字段引用未解析为原始值");
                        items[index] = json!({"index":index,"status":"failed","error":error});
                        return Err(error.with_details(json!({"items":items})));
                    }
                    Err(error) => {
                        items[index] = json!({"index":index,"status":"failed","error":error});
                        return Err(error.with_details(json!({"items":items})));
                    }
                }
            }
        }
        if let Err(e) = validate(schema, change) {
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
    if transactional {
        sqlx::raw_sql("START TRANSACTION")
            .execute(&mut conn)
            .await
            .map_err(super::errors::mysql_error)?;
    }
    let mut failure: Option<(usize, AppError)> = None;
    for (index, change) in changes.iter().enumerate() {
        if cancel.is_cancelled() {
            failure = Some((index, AppError::cancelled()));
            break;
        }
        if let Err(e) = sink.mark_sent(true).await {
            failure = Some((index, e));
            break;
        }
        match run_change(&mut conn, schema, change, transactional).await {
            Ok(()) => items[index] = json!({"index":index,"status":"applied"}),
            Err(e) => {
                failure = Some((index, e));
                break;
            }
        }
    }
    if let Some((index, mut error)) = failure {
        let unknown = error.code == "outcome_unknown";
        items[index] = json!({"index":index,"status":if unknown {"unknown"} else if error.code == "cancelled" {"not_executed"} else if error.code == "conflict" {"conflict"} else {"failed"},"error":error});
        // 事务回滚得到确认后才能标记已执行项为 rolled_back；失联则保持未知。
        if transactional {
            let rolled_back = sqlx::raw_sql("ROLLBACK").execute(&mut conn).await.is_ok();
            for item in &mut items[..index] {
                item["status"] = json!(if rolled_back {
                    "rolled_back"
                } else {
                    "unknown"
                });
            }
            if unknown && rolled_back {
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
    Ok(json!({"items":items,"affected_rows":changes.len()}))
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
    let target = s
        .edits
        .remove(&token)
        .ok_or_else(|| AppError::invalid("编辑令牌已失效"))?;
    let schema = catalog::schema(s, target).await?;
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
    let schema = catalog::schema(s, target).await?;
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
mod tests {
    use super::*;

    fn assert_consumers(ty: &str, value: Value, accepted: bool) {
        let column = Column::new("amount", ty);
        let schema = TableSchema {
            target: ResourceTarget {
                database: Some("db".into()),
                name: "t".into(),
                key_b64: None,
                kind: "table".into(),
            },
            columns: vec![column.clone()],
            keys: Vec::new(),
            ddl: String::new(),
            engine: "InnoDB".into(),
            details: Json::Null,
        };
        let change = RowChange {
            operation: "insert".into(),
            key: BTreeMap::new(),
            before: BTreeMap::new(),
            values: [(column.name.clone(), value.clone())].into_iter().collect(),
        };
        assert_eq!(
            validate_import_value(&value, &column).is_ok(),
            accepted,
            "import {ty}: {value:?}"
        );
        assert_eq!(
            validate(&schema, &change).is_ok(),
            accepted,
            "write {ty}: {value:?}"
        );
    }

    #[test]
    fn decimal_precision_and_scale_are_lossless_for_import_and_write() {
        for (ty, value, accepted) in [
            ("decimal(5,2)", "999.99", true),
            ("decimal(5,2)", "-999.9900", true),
            ("decimal(5,2)", "1.234", false),
            ("decimal(5,2)", "1000", false),
            ("decimal(5,2)", "0.0010", false),
            ("decimal(5,2)", "1e2", true),
            ("decimal(5,2)", "1e-3", false),
            ("decimal(2,2)", "0.99", true),
            ("decimal(2,2)", "1", false),
            ("decimal(5,2) unsigned", "-0.01", false),
            ("decimal(5,2) unsigned", "-0.000", true),
            ("decimal(5,2)", "invalid", false),
            ("decimal", "1", false),
            ("decimal(2,3)", "1", false),
        ] {
            assert_consumers(ty, Value::Decimal(value.into()), accepted);
        }
    }

    #[test]
    fn integer_width_sign_and_year_are_checked_for_import_and_write() {
        for (ty, value, accepted) in [
            ("tinyint", "-128", true),
            ("tinyint", "127", true),
            ("tinyint", "128", false),
            ("tinyint unsigned", "-1", false),
            ("tinyint unsigned", "255", true),
            ("tinyint unsigned", "256", false),
            ("smallint", "-32769", false),
            ("mediumint", "8388607", true),
            ("mediumint", "8388608", false),
            ("int unsigned", "4294967295", true),
            ("int unsigned", "4294967296", false),
            ("bigint", "-9223372036854775808", true),
            ("year", "0", true),
            ("year", "1901", true),
            ("year", "2155", true),
            ("year", "1900", false),
            ("year", "2156", false),
            ("year", "70", false),
        ] {
            assert_consumers(ty, Value::Integer(value.into()), accepted);
        }
        for (ty, value, accepted) in [
            ("bigint unsigned", "18446744073709551615", true),
            ("bigint", "18446744073709551615", false),
            ("int", "2147483647", true),
            ("int", "2147483648", false),
            ("mediumint unsigned", "16777215", true),
            ("mediumint unsigned", "16777216", false),
            ("year", "2026", true),
            ("year", "99", false),
            ("year", "2156", false),
        ] {
            assert_consumers(ty, Value::Unsigned(value.into()), accepted);
        }
    }

    #[test]
    fn bit_width_is_checked_for_import_and_write() {
        for (ty, value, accepted) in [
            ("bit(1)", "1", true),
            ("bit(1)", "2", false),
            ("bit(9)", "511", true),
            ("bit(9)", "512", false),
            ("bit(64)", "18446744073709551615", true),
            ("bit(65)", "1", false),
        ] {
            assert_consumers(ty, Value::Unsigned(value.into()), accepted);
        }
        for (ty, bytes, accepted) in [
            ("bit(1)", vec![1], true),
            ("bit(1)", vec![2], false),
            ("bit(9)", vec![1, 255], true),
            ("bit(9)", vec![2, 0], false),
            ("bit(9)", vec![0, 1, 255], true),
            ("bit(64)", vec![255; 8], true),
            ("bit(64)", vec![1; 9], false),
            ("bit(1)", vec![], false),
        ] {
            assert_consumers(ty, Value::Bytes(STANDARD.encode(bytes)), accepted);
        }
        assert_consumers("bit(8)", Value::Bytes("invalid!".into()), false);
    }
}
