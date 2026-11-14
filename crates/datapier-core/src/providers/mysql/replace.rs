//! MySQL 替换计划：单次流式冻结完整原值，提交时复用行变更 CAS。
use std::collections::{BTreeMap, HashSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::TryStreamExt;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::mysql::MySqlArguments;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{MysqlSession, catalog, changes, results};
use crate::{
    ExecutionSink,
    execution::replace::CompiledReplacement,
    model::replace::{ReplaceScope, ReplaceSpec},
    model::{AppError, ResourceTarget, Result, RowChange, TableSchema, Value, param},
};

const MAX_ROWS: usize = 10_000;
const MAX_BYTES: usize = 32 * 1024 * 1024;
const CLIENT_MAX: usize = 64 * 1024 * 1024;

struct Item {
    id: usize,
    change: RowChange,
    matches: usize,
    excluded: bool,
}

// BTreeMap 的节点容量不公开；按每项附加节点开销估算，避免只统计序列化长度。
fn json_heap(value: &Json) -> usize {
    match value {
        Json::String(s) => s.capacity(),
        Json::Array(values) => values
            .capacity()
            .saturating_mul(std::mem::size_of::<Json>())
            .saturating_add(values.iter().map(json_heap).sum()),
        Json::Object(values) => values
            .iter()
            .map(|(k, v)| {
                256usize
                    .saturating_add(k.capacity())
                    .saturating_add(json_heap(v))
            })
            .sum(),
        _ => 0,
    }
}

fn value_heap(value: &Value) -> usize {
    match value {
        Value::Integer(v)
        | Value::Unsigned(v)
        | Value::Decimal(v)
        | Value::Float(v)
        | Value::Text(v)
        | Value::Bytes(v)
        | Value::Date(v)
        | Value::Time(v)
        | Value::DateTime(v) => v.capacity(),
        Value::Json(v) => json_heap(v),
        Value::Array(v) => v
            .capacity()
            .saturating_mul(std::mem::size_of::<Value>())
            .saturating_add(v.iter().map(value_heap).sum()),
        Value::Map(v) => v
            .capacity()
            .saturating_mul(std::mem::size_of::<(Value, Value)>())
            .saturating_add(
                v.iter()
                    .map(|(k, v)| value_heap(k).saturating_add(value_heap(v)))
                    .sum(),
            ),
        Value::Large {
            original_type,
            preview,
            ..
        } => original_type.capacity().saturating_add(preview.capacity()),
        _ => 0,
    }
}

fn map_heap(map: &BTreeMap<String, Value>) -> usize {
    map.iter()
        .map(|(key, value)| {
            256usize
                .saturating_add(key.capacity())
                .saturating_add(value_heap(value))
        })
        .sum()
}

fn schema_heap(schema: &TableSchema) -> usize {
    let target = &schema.target;
    let mut bytes = target.name.capacity()
        + target.kind.capacity()
        + target.database.as_ref().map_or(0, String::capacity)
        + target.key_b64.as_ref().map_or(0, String::capacity)
        + schema.ddl.capacity()
        + schema.engine.capacity();
    bytes = bytes.saturating_add(
        schema
            .columns
            .capacity()
            .saturating_mul(std::mem::size_of::<crate::model::Column>()),
    );
    for col in &schema.columns {
        bytes = bytes.saturating_add(
            col.name.capacity()
                + col.type_name.capacity()
                + col.default_value.as_ref().map_or(0, String::capacity)
                + col.comment.as_ref().map_or(0, String::capacity),
        );
    }
    bytes = bytes.saturating_add(
        schema
            .keys
            .capacity()
            .saturating_mul(std::mem::size_of::<Vec<String>>()),
    );
    for key in &schema.keys {
        bytes = bytes.saturating_add(
            key.capacity().saturating_mul(std::mem::size_of::<String>())
                + key.iter().map(String::capacity).sum::<usize>(),
        );
    }
    bytes.saturating_add(json_heap(&schema.details))
}

fn item_heap(item: &Item) -> usize {
    item.change
        .operation
        .capacity()
        .saturating_add(map_heap(&item.change.key))
        .saturating_add(map_heap(&item.change.before))
        .saturating_add(map_heap(&item.change.values))
}

// 顺序序列化并只保留请求窗口，避免每次分页复制完整原值。
fn json_window<T: serde::Serialize>(
    value: &T,
    offset: usize,
    limit: usize,
) -> Result<(Vec<u8>, usize)> {
    struct Window {
        offset: usize,
        limit: usize,
        pos: usize,
        bytes: Vec<u8>,
    }
    impl std::io::Write for Window {
        fn write(&mut self, input: &[u8]) -> std::io::Result<usize> {
            let end = self.pos.saturating_add(input.len());
            let start = self.offset.saturating_sub(self.pos).min(input.len());
            let stop = self
                .offset
                .saturating_add(self.limit)
                .saturating_sub(self.pos)
                .min(input.len());
            if stop > start {
                self.bytes.extend_from_slice(&input[start..stop]);
            }
            self.pos = end;
            Ok(input.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut window = Window {
        offset,
        limit,
        pos: 0,
        bytes: Vec::new(),
    };
    serde_json::to_writer(&mut window, value)?;
    Ok((window.bytes, window.pos))
}
/// 只保留实际变化的完整基线；未完成的流永远不能提交。
pub(super) struct ReplacePlan {
    schema: TableSchema,
    fingerprint: Option<String>,
    selection_digest: String,
    items_heap: usize,
    items: Vec<Item>,
    revision: u64,
    scanned: usize,
    matched: usize,
    ready: bool,
    reason: Option<String>,
}

impl ReplacePlan {
    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(schema_heap(&self.schema))
            .saturating_add(self.fingerprint.as_ref().map_or(0, String::capacity))
            .saturating_add(self.selection_digest.capacity())
            .saturating_add(self.reason.as_ref().map_or(0, String::capacity))
            .saturating_add(
                self.items
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Item>()),
            )
            .saturating_add(self.items_heap)
    }
    fn digest(&self) -> Result<String> {
        struct HashWriter<'a>(&'a mut Sha256);
        impl std::io::Write for HashWriter<'_> {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.update(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut hash = Sha256::new();
        serde_json::to_writer(
            HashWriter(&mut hash),
            &(
                &self.schema.target,
                &self.fingerprint,
                &self.selection_digest,
                self.revision,
                self.ready,
            ),
        )?;
        for item in &self.items {
            serde_json::to_writer(
                HashWriter(&mut hash),
                &(item.id, &item.change, item.matches, item.excluded),
            )?;
        }
        Ok(format!("{:x}", hash.finalize()))
    }
    fn summary(&self, id: Uuid) -> Result<Json> {
        let digest = self.digest()?;
        Ok(
            json!({"id":id,"plan_id":id,"revision":self.revision,"plan_digest":digest,"digest":digest,"retained_bytes":self.retained_bytes(),"scanned":self.scanned,"matched":self.matched,"ready":self.ready,"complete":self.ready,"state":if self.ready {"ready"} else {"incomplete"},"total_items":self.items.len(),"submittable":self.ready && self.items.iter().any(|item| !item.excluded),"reason":self.reason,"transactional":self.schema.engine.eq_ignore_ascii_case("innodb")}),
        )
    }
}

fn allowed_text(ty: &str) -> bool {
    let base = ty
        .split(|c: char| c == '(' || c.is_ascii_whitespace())
        .next()
        .unwrap_or("");
    [
        "char",
        "varchar",
        "tinytext",
        "text",
        "mediumtext",
        "longtext",
    ]
    .iter()
    .any(|v| base.eq_ignore_ascii_case(v))
}

fn add_filters(
    sql: &mut String,
    args: &mut MySqlArguments,
    schema: &TableSchema,
    filters: &[Json],
    has_where: &mut bool,
) -> Result<()> {
    for filter in filters {
        let name = filter
            .get("column")
            .and_then(Json::as_str)
            .ok_or_else(|| AppError::invalid("过滤条件缺少列名"))?;
        if !schema.columns.iter().any(|c| c.name == name) {
            return Err(AppError::invalid("过滤列不存在"));
        }
        let op = filter
            .get("op")
            .and_then(Json::as_str)
            .unwrap_or("eq")
            .to_ascii_lowercase();
        let operator = match op.as_str() {
            "eq" | "=" => "=",
            "ne" | "!=" | "<>" => "<>",
            "lt" | "<" => "<",
            "lte" | "<=" => "<=",
            "gt" | ">" => ">",
            "gte" | ">=" => ">=",
            "like" => "LIKE",
            "not_like" => "NOT LIKE",
            "is_null" => "IS NULL",
            "is_not_null" => "IS NOT NULL",
            _ => return Err(AppError::invalid("不支持的过滤运算符")),
        };
        sql.push_str(if *has_where { " AND " } else { " WHERE " });
        *has_where = true;
        sql.push_str(&super::quote_ident(name)?);
        sql.push(' ');
        sql.push_str(operator);
        if !matches!(operator, "IS NULL" | "IS NOT NULL") {
            let value = filter
                .get("value")
                .ok_or_else(|| AppError::invalid("过滤条件缺少值"))?;
            if value.is_null() || value.get("type").and_then(Json::as_str) == Some("null") {
                return Err(AppError::invalid("NULL 过滤请使用 is_null 或 is_not_null"));
            }
            catalog::add_filter_arg(args, value)?;
            sql.push_str(" ?");
        }
    }
    Ok(())
}

fn append_key(
    sql: &mut String,
    args: &mut MySqlArguments,
    key: &[String],
    values: &BTreeMap<String, Value>,
) -> Result<()> {
    if values.len() != key.len()
        || key.iter().any(|name| {
            !values.contains_key(name)
                || matches!(values.get(name), Some(Value::Null | Value::Missing))
        })
    {
        return Err(AppError::invalid("必须提供完整非空可靠键"));
    }
    sql.push('(');
    for (part, name) in key.iter().enumerate() {
        if part > 0 {
            sql.push_str(" AND ");
        }
        sql.push_str(&super::quote_ident(name)?);
        sql.push_str(" <=> ?");
        catalog::add_filter_arg(args, &serde_json::to_value(&values[name])?)?;
    }
    sql.push(')');
    Ok(())
}

/// 按完整可靠键读取完整原值；数据库在同一语句中先限制每列的编码体积。
/// 预算不足时不返回截断基线，视图指纹供后续写入前核对。
pub(super) async fn snapshot(
    s: &mut MysqlSession,
    target: ResourceTarget,
    values: BTreeMap<String, Value>,
    max_bytes: usize,
    cancel: &CancellationToken,
) -> Result<(TableSchema, BTreeMap<String, Value>, Option<String>)> {
    let raw = catalog::schema_raw(s, target, cancel).await?;
    let (schema, fingerprint) = if raw.target.kind == "view" {
        let mapping = super::view::resolve(s, raw, cancel).await?;
        if !mapping.update {
            return Err(AppError::invalid("视图不能更新"));
        }
        (mapping.schema, Some(mapping.fingerprint))
    } else {
        (raw, None)
    };
    let key = schema
        .keys
        .iter()
        .find(|key| key.len() == values.len() && key.iter().all(|name| values.contains_key(name)))
        .ok_or_else(|| AppError::invalid("必须提供完整可靠键"))?;
    let db = schema
        .target
        .database
        .as_deref()
        .ok_or_else(|| AppError::invalid("缺少数据库"))?;
    let mut sql = format!(
        "SELECT {} FROM {}.{} WHERE ",
        schema
            .columns
            .iter()
            .map(|c| super::quote_ident(&c.name))
            .collect::<Result<Vec<_>>>()?
            .join(","),
        super::quote_ident(db)?,
        super::quote_ident(&schema.target.name)?
    );
    let mut args = MySqlArguments::default();
    append_key(&mut sql, &mut args, key, &values)?;
    // 二进制与文本解码需要额外副本；JSON 解析树可能远大于原文。使用保守系数，
    // 并在同一 SELECT 中判断，非事务表不会在大小检查与读取之间换成大行。
    let overhead = schema
        .columns
        .iter()
        .try_fold(0usize, |sum, c| sum.checked_add(1024 + c.name.len()))
        .ok_or_else(|| AppError::new("resource_limit", "行元数据超过预算"))?;
    let remaining = max_bytes
        .checked_sub(overhead)
        .ok_or_else(|| AppError::new("resource_limit", "行元数据超过预算"))?;
    sql.push_str(" AND (");
    for (index, column) in schema.columns.iter().enumerate() {
        if index > 0 {
            sql.push_str(" + ");
        }
        let factor = if column.type_name.eq_ignore_ascii_case("json") {
            128
        } else {
            8
        };
        sql.push_str(&format!(
            "COALESCE(OCTET_LENGTH(CAST({} AS BINARY)), 0) * {factor}",
            super::quote_ident(&column.name)?
        ));
    }
    sql.push_str(&format!(") <= {}", remaining.min(CLIENT_MAX)));
    let mut conn = s.new_connection().await?;
    let row = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(AppError::cancelled()),
        row = sqlx::query_with(sqlx::AssertSqlSafe(sql.as_str()), args).fetch_optional(&mut conn) => row?,
    }.ok_or_else(|| AppError::new("resource_limit_or_conflict", "行不存在、不再满足视图条件或完整原值超过预算"))?;
    let decoded = results::decode_row(&row)?;
    if decoded.len() != schema.columns.len() {
        return Err(AppError::conflict("读取期间表结构已变化"));
    }
    let before = schema
        .columns
        .iter()
        .map(|column| column.name.clone())
        .zip(decoded)
        .collect();
    Ok((schema, before, fingerprint))
}

/// 一次 SELECT 收集范围；选中行也按可靠键重新读取，绝不信任客户端原值。
pub(super) async fn prepare(
    s: &mut MysqlSession,
    params: Json,
    _sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    s.profile.ensure_write()?;
    let target: ResourceTarget = param(&params, "target")?;
    let spec: ReplaceSpec = param(&params, "spec")?;
    let compiled = CompiledReplacement::new(&spec)?;
    let raw = catalog::schema_raw(s, target, cancel).await?;
    let (schema, fingerprint) = if raw.target.kind == "view" {
        let mapped = super::view::resolve(s, raw, cancel).await?;
        if !mapped.update {
            return Err(AppError::invalid("视图不能更新"));
        }
        (mapped.schema, Some(mapped.fingerprint))
    } else {
        (raw, None)
    };
    let selected_keys: Option<Vec<BTreeMap<String, Value>>> =
        if spec.scope == ReplaceScope::Filtered {
            None
        } else {
            Some(param(&params, "keys")?)
        };
    let key = if let Some(keys) = &selected_keys {
        let first = keys.first().ok_or_else(|| AppError::invalid("请选择行"))?;
        schema
            .keys
            .iter()
            .find(|key| key.len() == first.len() && key.iter().all(|name| first.contains_key(name)))
    } else {
        schema.keys.first()
    }
    .cloned()
    .ok_or_else(|| AppError::invalid("目标没有匹配的可靠键"))?;
    if spec.columns.is_empty() {
        return Err(AppError::invalid("必须选择替换列"));
    }
    let mut selected = HashSet::new();
    for name in &spec.columns {
        let col = schema
            .columns
            .iter()
            .find(|c| &c.name == name)
            .ok_or_else(|| AppError::invalid("替换列不存在"))?;
        if !selected.insert(name.clone()) || col.generated || !allowed_text(&col.type_name) {
            return Err(AppError::invalid(format!("列 {name} 不是可写文本列或重复")));
        }
    }
    let available = params
        .get("available_bytes")
        .map(|v| {
            serde_json::from_value::<usize>(v.clone())
                .map_err(|_| AppError::invalid("无效客户端内存预算"))
        })
        .transpose()?
        .unwrap_or(CLIENT_MAX);
    let limit = params
        .get("_max_plan_bytes")
        .map(|v| {
            serde_json::from_value::<usize>(v.clone())
                .map_err(|_| AppError::invalid("无效计划内存预算"))
        })
        .transpose()?
        .unwrap_or(MAX_BYTES);
    let max_items = params
        .get("_max_plan_items")
        .map(|v| {
            serde_json::from_value::<usize>(v.clone())
                .map_err(|_| AppError::invalid("无效计划行数预算"))
        })
        .transpose()?
        .unwrap_or(MAX_ROWS)
        .min(MAX_ROWS);
    let budget = available.min(CLIENT_MAX).min(MAX_BYTES).min(limit);
    if budget == 0 {
        return Err(AppError::new("resource_limit", "替换计划内存预算为零"));
    }
    let db = schema
        .target
        .database
        .as_deref()
        .ok_or_else(|| AppError::invalid("缺少数据库"))?;
    // 每行返回完整值或 NULL 加超限标记；不能在 WHERE 中丢弃宽行。
    // JSON 解析树和 Base64 等解码副本需要比网络字节更多的空间。
    let row_overhead = schema.columns.iter().fold(0usize, |sum, column| {
        sum.saturating_add(1024 + column.name.len())
    });
    let row_size = schema
        .columns
        .iter()
        .map(|column| {
            let factor = if column.type_name.eq_ignore_ascii_case("json") {
                128
            } else {
                8
            };
            Ok(format!(
                "COALESCE(OCTET_LENGTH(CAST({} AS BINARY)), 0) * {factor}",
                super::quote_ident(&column.name)?
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .join(" + ");
    let row_size = format!("({row_overhead} + {row_size})");
    let mut sql = format!(
        " FROM {}.{}",
        super::quote_ident(db)?,
        super::quote_ident(&schema.target.name)?
    );
    let mut args = MySqlArguments::default();
    let mut has_where = false;
    let mut requested_keys = None;
    if let Some(keys) = &selected_keys {
        if keys.is_empty() || keys.len() > MAX_ROWS {
            return Err(AppError::invalid("选中键数量必须在 1 到 10000 之间"));
        }
        if keys.len().saturating_mul(key.len()) > 60_000 {
            return Err(AppError::invalid("选中键参数过多"));
        }
        requested_keys = Some(keys.len());
        let mut seen = HashSet::new();
        sql.push_str(" WHERE (");
        has_where = true;
        for (index, values) in keys.iter().enumerate() {
            if !seen.insert(serde_json::to_string(values)?) {
                return Err(AppError::invalid("选中键重复"));
            }
            if index > 0 {
                sql.push_str(" OR ");
            }
            append_key(&mut sql, &mut args, &key, values)?;
        }
        sql.push(')');
    }
    let filters = match params.get("filters") {
        None | Some(Json::Null) => &[][..],
        Some(value) => value
            .as_array()
            .ok_or_else(|| AppError::invalid("filters 必须是数组"))?
            .as_slice(),
    };
    add_filters(&mut sql, &mut args, &schema, filters, &mut has_where)?;
    let selection_digest = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(&spec, filters, params.get("keys")))?)
    );
    let mut plan = ReplacePlan {
        schema,
        fingerprint,
        selection_digest,
        items_heap: 0,
        items: Vec::new(),
        revision: 1,
        scanned: 0,
        matched: 0,
        ready: false,
        reason: None,
    };
    if plan.retained_bytes() > budget {
        return Err(AppError::new("resource_limit", "替换计划结构超过内存预算"));
    }
    // SELECT 是单次流式执行，无法随已保留项目调整服务端上限；预留一半剩余
    // 空间给当前行的传输和解码，并限制计划增长，保证两者同时存在时仍有界。
    let row_limit = budget.saturating_sub(plan.retained_bytes()) / 2;
    let retained_limit = budget.saturating_sub(row_limit);
    let columns = plan
        .schema
        .columns
        .iter()
        .map(|column| {
            let name = super::quote_ident(&column.name)?;
            Ok(format!(
                "CASE WHEN {row_size} <= {row_limit} THEN {name} ELSE NULL END AS {name}"
            ))
        })
        .collect::<Result<Vec<_>>>()?
        .join(",");
    sql = format!("SELECT {columns}, CAST({row_size} AS UNSIGNED) AS `__dp_replace_size`{sql}");
    let mut conn = s.new_connection().await?;
    let transactional = plan.schema.engine.eq_ignore_ascii_case("innodb");
    if transactional {
        sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut conn)
            .await?;
        sqlx::raw_sql("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")
            .execute(&mut conn)
            .await?;
    }
    let mut used = plan.retained_bytes();
    let timeout_ms = params
        .get("timeout_ms")
        .and_then(Json::as_u64)
        .unwrap_or(30_000)
        .clamp(1, 300_000);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    {
        let mut rows = sqlx::query_with(sqlx::AssertSqlSafe(sql.as_str()), args).fetch(&mut conn);
        loop {
            let next = tokio::select! { biased; _ = cancel.cancelled() => { plan.reason = Some("收集已取消".into()); break; }, _ = tokio::time::sleep_until(deadline) => { plan.reason = Some("收集超时".into()); break; }, row = rows.try_next() => row };
            let row = match next {
                Ok(Some(row)) => row,
                Ok(None) => {
                    plan.ready = true;
                    break;
                }
                Err(e) => {
                    plan.reason = Some(format!("读取中断: {e}"));
                    break;
                }
            };
            plan.scanned += 1;
            let row_size: u64 = match row.try_get_unchecked(plan.schema.columns.len()) {
                Ok(size) => size,
                Err(e) => {
                    plan.reason = Some(format!("无法读取行大小: {e}"));
                    break;
                }
            };
            if row_size > row_limit as u64 || row_size > budget.saturating_sub(used) as u64 {
                plan.reason = Some("完整原值超过字节预算，请缩小范围".into());
                break;
            }
            let values = match results::decode_row(&row) {
                Ok(mut v) => {
                    v.pop(); // 最后一列是大小标记，不属于原始行。
                    v
                }
                Err(e) => {
                    plan.reason = Some(format!("无法完整读取原值: {e}"));
                    break;
                }
            };
            if values.len() != plan.schema.columns.len() {
                plan.reason = Some("读取期间表结构已变化".into());
                break;
            }
            let before: BTreeMap<_, _> = plan
                .schema
                .columns
                .iter()
                .map(|c| c.name.clone())
                .zip(values)
                .collect();
            let mut after = BTreeMap::new();
            let mut matches = 0;
            let mut after_bytes = 0usize;
            for name in &spec.columns {
                if let Some(Value::Text(text)) = before.get(name) {
                    let output_budget = budget
                        .saturating_sub(used)
                        .saturating_sub(row_size as usize)
                        .saturating_sub(after_bytes)
                        / 2;
                    match compiled.apply(text, output_budget) {
                        Ok(result) if result.changed => {
                            matches += result.matches;
                            after_bytes = after_bytes
                                .saturating_add(256 + name.len())
                                .saturating_add(result.value.capacity());
                            after.insert(name.clone(), Value::Text(result.value));
                        }
                        Ok(_) => {}
                        Err(e) => {
                            plan.reason = Some(format!("替换超限或无效: {e}"));
                            break;
                        }
                    }
                }
            }
            if plan.reason.is_some() {
                break;
            }
            if after.is_empty() {
                continue;
            }
            plan.matched += 1;
            let key_values = key
                .iter()
                .map(|name| (name.clone(), before[name].clone()))
                .collect();
            let change = RowChange {
                operation: "update".into(),
                key: key_values,
                before,
                values: after,
            };
            if plan.items.len() >= max_items {
                plan.reason = Some("替换计划超过行数预算，请缩小范围".into());
                break;
            }
            let item = Item {
                id: plan.items.len(),
                change,
                matches,
                excluded: false,
            };
            let size = item_heap(&item);
            plan.items_heap = plan.items_heap.saturating_add(size);
            plan.items.push(item);
            if plan.retained_bytes() > retained_limit {
                plan.items.pop();
                plan.items_heap = plan.items_heap.saturating_sub(size);
                plan.items.shrink_to_fit();
                plan.reason = Some("替换计划超过字节预算，请缩小范围".into());
                break;
            }
            used = plan.retained_bytes();
        }
    }
    if plan.ready && requested_keys.is_some_and(|count| count != plan.scanned) {
        plan.ready = false;
        plan.reason = Some("部分选中行已不存在或不再满足筛选条件".into());
    }
    if transactional
        && plan.ready
        && let Err(e) = sqlx::raw_sql("ROLLBACK").execute(&mut conn).await
    {
        plan.ready = false;
        plan.reason = Some(format!("结束读取事务失败: {e}"));
    }
    if plan.retained_bytes() > budget {
        return Err(AppError::new("resource_limit", "替换计划超过内存预算"));
    }
    let id = Uuid::new_v4();
    let retained_bytes = plan.retained_bytes();
    let summary = plan.summary(id)?;
    let response = json!({"summary":summary,"plan_id":id,"revision":plan.revision,"digest":plan.digest()?,"retained_bytes":retained_bytes});
    s.replace_plans.insert(id, plan);
    Ok(response)
}

/// 分页读取冻结项目；指定 item_id 获取单项完整值。
pub(super) async fn read(s: &mut MysqlSession, params: Json) -> Result<Json> {
    let id: Uuid = param(&params, "plan_id")?;
    let plan = s
        .replace_plans
        .get(&id)
        .ok_or_else(|| AppError::invalid("替换计划已失效"))?;
    if params
        .get("revision")
        .or_else(|| params.get("expected_revision"))
        .and_then(Json::as_u64)
        != Some(plan.revision)
    {
        return Err(AppError::conflict("替换计划版本已变化"));
    }
    if let Some(item_id) = params.get("item_id").and_then(Json::as_u64) {
        let i = plan
            .items
            .get(item_id as usize)
            .ok_or_else(|| AppError::invalid("替换项目不存在"))?;
        let offset = params.get("offset").and_then(Json::as_u64).unwrap_or(0) as usize;
        let (bytes, total) = if params.get("side").and_then(Json::as_str) == Some("after") {
            json_window(&i.change.values, offset, 256 * 1024)?
        } else {
            json_window(&i.change.before, offset, 256 * 1024)?
        };
        if offset > total {
            return Err(AppError::invalid("无效偏移"));
        }
        let end = offset.saturating_add(bytes.len());
        return Ok(
            json!({"summary":plan.summary(id)?,"revision":plan.revision,"digest":plan.digest()?,"item_id":item_id,"offset":offset,"base64":STANDARD.encode(&bytes),"next_offset":(end < total).then_some(end),"encoding":"typed_json"}),
        );
    }
    let offset = params.get("offset").and_then(Json::as_u64).unwrap_or(0) as usize;
    let limit = params
        .get("limit")
        .and_then(Json::as_u64)
        .unwrap_or(50)
        .clamp(1, 100) as usize;
    let end = offset.saturating_add(limit).min(plan.items.len());
    if offset > plan.items.len() {
        return Err(AppError::invalid("分页偏移超出范围"));
    }
    let items: Vec<_> = plan.items[offset..end].iter().map(|i| -> Result<Json> {
        let (before, before_len) = json_window(&i.change.before, 0, 768)?;
        let (after, after_len) = json_window(&i.change.values, 0, 768)?;
        let brief = |v: &[u8], total: usize| json!({"base64":STANDARD.encode(v),"byte_len":total,"complete":v.len() == total,"encoding":"typed_json"});
        Ok(json!({"id":i.id,"key":i.change.key,"before":brief(&before, before_len),"after":brief(&after, after_len),"matches":i.matches,"selected":!i.excluded,"collision":false}))
    }).collect::<Result<_>>()?;
    Ok(
        json!({"summary":plan.summary(id)?,"items":items,"offset":offset,"next_offset":(end < plan.items.len()).then_some(end),"revision":plan.revision,"digest":plan.digest()?,"plan_digest":plan.digest()?}),
    )
}

/// 更新排除集合并使旧摘要失效。
pub(super) async fn update(s: &mut MysqlSession, params: Json) -> Result<Json> {
    let id: Uuid = param(&params, "plan_id")?;
    let revision = params
        .get("revision")
        .or_else(|| params.get("expected_revision"))
        .and_then(Json::as_u64)
        .ok_or_else(|| AppError::invalid("缺少 revision"))?;
    let selections = params
        .get("selections")
        .and_then(Json::as_array)
        .ok_or_else(|| AppError::invalid("缺少 selections"))?;
    let plan = s
        .replace_plans
        .get_mut(&id)
        .ok_or_else(|| AppError::invalid("替换计划已失效"))?;
    if plan.revision != revision {
        return Err(AppError::conflict("替换计划版本已变化"));
    }
    let mut seen = HashSet::new();
    let mut changes = Vec::new();
    for selection in selections {
        let index = selection
            .get("id")
            .and_then(Json::as_u64)
            .ok_or_else(|| AppError::invalid("缺少条目 id"))? as usize;
        let selected = selection
            .get("selected")
            .and_then(Json::as_bool)
            .ok_or_else(|| AppError::invalid("缺少 selected"))?;
        if index >= plan.items.len() || !seen.insert(index) {
            return Err(AppError::invalid("条目不存在或重复"));
        }
        changes.push((index, selected));
    }
    for (index, selected) in changes {
        plan.items[index].excluded = !selected;
    }
    plan.revision = plan
        .revision
        .checked_add(1)
        .ok_or_else(|| AppError::invalid("替换版本溢出"))?;
    Ok(json!({"summary":plan.summary(id)?,"revision":plan.revision,"digest":plan.digest()?}))
}

/// 消费计划后核对视图映射，提交固定行，绝不再次执行筛选。
pub(super) async fn submit(
    s: &mut MysqlSession,
    params: Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Json> {
    s.profile.ensure_write()?;
    let id: Uuid = param(&params, "plan_id")?;
    let revision: u64 = param(&params, "revision")?;
    let digest: String = param(&params, "digest")?;
    let plan = s
        .replace_plans
        .get(&id)
        .ok_or_else(|| AppError::invalid("替换计划已失效"))?;
    if !plan.ready || plan.revision != revision || plan.digest()? != digest {
        return Err(AppError::conflict("替换计划未完成或版本已变化"));
    }
    let plan = s.replace_plans.remove(&id).expect("已验证的替换计划存在");
    let raw = catalog::schema_raw(s, plan.schema.target.clone(), cancel).await?;
    let schema = if let Some(expected) = &plan.fingerprint {
        if raw.target.kind != "view" {
            return Err(AppError::conflict("视图类型已变化"));
        }
        let mapping = super::view::resolve(s, raw, cancel).await?;
        if &mapping.fingerprint != expected {
            return Err(AppError::conflict("视图映射或依赖结构已变化"));
        }
        mapping.schema
    } else {
        if catalog::schema_fingerprint(&raw)? != catalog::schema_fingerprint(&plan.schema)? {
            return Err(AppError::conflict("表结构已变化"));
        }
        raw
    };
    let changes = plan
        .items
        .into_iter()
        .filter(|i| !i.excluded)
        .map(|i| i.change)
        .collect();
    changes::execute_batch(s, &schema, changes, sink, cancel).await
}

/// 丢弃内存中的计划。
pub(super) async fn discard(s: &mut MysqlSession, params: Json) -> Result<Json> {
    let id: Uuid = param(&params, "plan_id")?;
    Ok(json!({"discarded":s.replace_plans.remove(&id).is_some()}))
}
