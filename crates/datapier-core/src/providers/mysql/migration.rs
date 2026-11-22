//! MySQL 迁移使用独立连接、单次流式读取和有界批次；数据不经过结果缓存。
mod offline;
use super::{MysqlSession, catalog, quote_ident};
use crate::{
    ExecutionSink,
    model::{
        AppError, Column, ResourceTarget, Result, TableSchema, Value, ensure_not_cancelled,
        transfer::{
            ConflictPolicy, FilterOperator, MAX_BATCH_BYTES, MAX_BATCH_ROWS, MAX_VALUE_BYTES,
            ReadConsistency, TransferObject, TransferPlanItem, TransferStepState,
        },
    },
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::TryStreamExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::mysql::{MySqlArguments, MySqlConnection, MySqlRow};
use sqlx::{Arguments, Column as _, Row, TypeInfo};
use std::str::FromStr;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 包含目录事实及服务端原始建表语句的迁移基线。
#[derive(Debug, Clone)]
pub struct MigrationTable {
    pub schema: TableSchema,
    pub version: String,
    pub sql_mode: String,
    pub fingerprint: String,
    pub collations: std::collections::BTreeMap<String, (Option<String>, Option<String>)>,
    pub time_zone: String,
    pub snapshot: Option<crate::model::schema::SchemaSnapshot>,
}
/// 对象序号和移动所有权的类型化数据批次。
#[derive(Debug)]
pub struct MigrationBatch {
    pub object: usize,
    pub rows: Vec<Vec<Value>>,
    pub bytes: usize,
}
/// 批次逐项状态；失败时错误详情也包含已知逐项状态。
#[derive(Debug)]
pub struct MigrationWriteOutcome {
    pub states: Vec<TransferStepState>,
    pub affected: u64,
}

fn qualified(t: &ResourceTarget) -> Result<String> {
    let db = t
        .database
        .as_deref()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| AppError::invalid("迁移必须显式指定数据库"))?;
    Ok(format!("{}.{}", quote_ident(db)?, quote_ident(&t.name)?))
}
fn hash(ddl: &str, mode: &str) -> String {
    // AUTO_INCREMENT 是运行时计数，并非结构；普通插入不应使后续批次失效。
    let normalized = if let Ok((parts, options)) = ddl_parts(ddl) {
        let mut options = options;
        if let Some(start) = options.find(" AUTO_INCREMENT=") {
            let number = start + " AUTO_INCREMENT=".len();
            let end = options[number..]
                .bytes()
                .take_while(u8::is_ascii_digit)
                .count()
                + number;
            if end > number {
                options.replace_range(start..end, "");
            }
        }
        format!("{}\0{options}", parts.join(","))
    } else {
        ddl.to_string()
    };
    format!(
        "{:x}",
        base16ct::HexDisplay(&Sha256::digest(format!("{normalized}\0{mode}").as_bytes()))
    )
}
async fn fingerprint(conn: &mut MySqlConnection, target: &ResourceTarget) -> Result<String> {
    let sql = format!("SHOW CREATE TABLE {}", qualified(target)?);
    let ddl: String = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_one(&mut *conn)
        .await?
        .try_get(1)?;
    let mode: String = sqlx::query_scalar("SELECT @@SESSION.sql_mode")
        .fetch_one(&mut *conn)
        .await?;
    Ok(hash(&ddl, &mode))
}
fn source_columns<'a>(
    table: &'a MigrationTable,
    object: &TransferObject,
) -> Result<Vec<&'a Column>> {
    if object.columns.is_empty() {
        return Ok(table
            .schema
            .columns
            .iter()
            .filter(|c| !c.generated)
            .collect());
    }
    let mut seen = std::collections::HashSet::new();
    object
        .columns
        .iter()
        .map(|m| {
            if !seen.insert(&m.source) {
                return Err(AppError::invalid("源列映射重复"));
            }
            table
                .schema
                .columns
                .iter()
                .find(|c| c.name == m.source)
                .ok_or_else(|| AppError::invalid(format!("源列不存在: {}", m.source)))
        })
        .collect()
}
fn add_arg(args: &mut MySqlArguments, v: &Value) -> Result<()> {
    let result = match v {
        Value::Bool(v) => args.add(*v),
        Value::Integer(v) => args.add(
            v.parse::<i64>()
                .map_err(|_| AppError::invalid("整数无效"))?,
        ),
        Value::Unsigned(v) => args.add(
            v.parse::<u64>()
                .map_err(|_| AppError::invalid("无符号整数无效"))?,
        ),
        Value::Decimal(v) => args
            .add(bigdecimal::BigDecimal::from_str(v).map_err(|_| AppError::invalid("小数无效"))?),
        Value::Float(v) => {
            let n = v
                .parse::<f64>()
                .map_err(|_| AppError::invalid("浮点数无效"))?;
            if !n.is_finite() {
                return Err(AppError::invalid("浮点数必须有限"));
            }
            args.add(n)
        }
        Value::Bytes(v) => args.add(
            STANDARD
                .decode(v)
                .map_err(|_| AppError::invalid("Base64 无效"))?,
        ),
        Value::Text(v) | Value::Date(v) | Value::DateTime(v) | Value::Time(v) => {
            args.add(v.clone())
        }
        Value::Json(v) => args.add(v.to_string()),
        _ => return Err(AppError::invalid("不支持的绑定值")),
    };
    result.map_err(|e| AppError::invalid(format!("绑定参数失败: {e}")))
}
fn select_statement(
    table: &MigrationTable,
    object: &TransferObject,
) -> Result<(String, MySqlArguments)> {
    let cols = source_columns(table, object)?;
    if cols.is_empty() {
        return Err(AppError::invalid("未选择源列"));
    }
    let mut sql = format!(
        "SELECT {} FROM {}",
        cols.iter()
            .map(|c| quote_ident(&c.name))
            .collect::<Result<Vec<_>>>()?
            .join(", "),
        qualified(&table.schema.target)?
    );
    let mut args = MySqlArguments::default();
    for (i, f) in object.filters.iter().enumerate() {
        let column = table
            .schema
            .columns
            .iter()
            .find(|c| c.name == f.column)
            .ok_or_else(|| AppError::invalid(format!("过滤列不存在: {}", f.column)))?;
        sql.push_str(if i == 0 { " WHERE " } else { " AND " });
        sql.push_str(&quote_ident(&column.name)?);
        let op = match f.operator {
            FilterOperator::Eq => " = ",
            FilterOperator::Ne => " <> ",
            FilterOperator::Lt => " < ",
            FilterOperator::Le => " <= ",
            FilterOperator::Gt => " > ",
            FilterOperator::Ge => " >= ",
            FilterOperator::Like => " LIKE ",
            FilterOperator::IsNull => " IS NULL",
            FilterOperator::IsNotNull => " IS NOT NULL",
        };
        sql.push_str(op);
        if matches!(
            f.operator,
            FilterOperator::IsNull | FilterOperator::IsNotNull
        ) {
            if f.value.is_some() {
                return Err(AppError::invalid("NULL 过滤不能携带参数"));
            }
        } else {
            let value = f
                .value
                .as_ref()
                .ok_or_else(|| AppError::invalid("过滤缺少参数"))?;
            let mut filter_column = column.clone();
            filter_column.generated = false;
            super::changes::validate_import_value(value, &filter_column)?;
            add_arg(&mut args, value)?;
            sql.push('?');
        }
    }
    Ok((sql, args))
}
fn decode_row(row: &MySqlRow) -> Result<Vec<Value>> {
    row.columns()
        .iter()
        .enumerate()
        .map(|(i, c)| {
            if super::results::mysql_value_is_null(row, i)? {
                return Ok(Value::Null);
            }
            let ty = c.type_info().name();
            Ok(match ty {
                "BOOLEAN" | "TINYINT" => {
                    Value::Integer(row.try_get_unchecked::<i8, _>(i)?.to_string())
                }
                "SMALLINT" => Value::Integer(row.try_get_unchecked::<i16, _>(i)?.to_string()),
                "INT" | "MEDIUMINT" => {
                    Value::Integer(row.try_get_unchecked::<i32, _>(i)?.to_string())
                }
                "BIGINT" => Value::Integer(row.try_get_unchecked::<i64, _>(i)?.to_string()),
                "TINYINT UNSIGNED" => {
                    Value::Unsigned(row.try_get_unchecked::<u8, _>(i)?.to_string())
                }
                "SMALLINT UNSIGNED" | "YEAR" => {
                    Value::Unsigned(row.try_get_unchecked::<u16, _>(i)?.to_string())
                }
                "INT UNSIGNED" | "MEDIUMINT UNSIGNED" => {
                    Value::Unsigned(row.try_get_unchecked::<u32, _>(i)?.to_string())
                }
                "BIGINT UNSIGNED" => {
                    Value::Unsigned(row.try_get_unchecked::<u64, _>(i)?.to_string())
                }
                "DECIMAL" => Value::Decimal(
                    row.try_get_unchecked::<bigdecimal::BigDecimal, _>(i)?
                        .to_string(),
                ),
                "FLOAT" => Value::Float(row.try_get_unchecked::<f32, _>(i)?.to_string()),
                "DOUBLE" => Value::Float(row.try_get_unchecked::<f64, _>(i)?.to_string()),
                "DATE" => Value::Date(
                    row.try_get_unchecked::<chrono::NaiveDate, _>(i)?
                        .to_string(),
                ),
                "DATETIME" | "TIMESTAMP" => Value::DateTime(
                    row.try_get_unchecked::<chrono::NaiveDateTime, _>(i)?
                        .to_string(),
                ),
                "TIME" => Value::Time(
                    row.try_get_unchecked::<sqlx::mysql::types::MySqlTime, _>(i)?
                        .to_string(),
                ),
                "JSON" => Value::Json(row.try_get_unchecked::<serde_json::Value, _>(i)?),
                "BINARY" | "VARBINARY" | "TINYBLOB" | "BLOB" | "MEDIUMBLOB" | "LONGBLOB"
                | "BIT" | "GEOMETRY" => {
                    Value::Bytes(STANDARD.encode(row.try_get_unchecked::<Vec<u8>, _>(i)?))
                }
                "CHAR" | "VARCHAR" | "TINYTEXT" | "TEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM"
                | "SET" => Value::Text(row.try_get_unchecked::<String, _>(i)?),
                _ => {
                    return Err(AppError::new(
                        "unsupported_type",
                        format!("不支持无损读取 MySQL 类型 {ty}"),
                    ));
                }
            })
        })
        .collect()
}
fn row_bytes(row: &[Value]) -> Result<usize> {
    let mut total = 0usize;
    for value in row {
        let size = crate::execution::migration::encoded_size(value)?;
        if size > MAX_VALUE_BYTES {
            return Err(AppError::new("value_too_large", "单值超过 16 MiB"));
        }
        total = total.saturating_add(size);
    }
    Ok(total)
}

impl MysqlSession {
    /// 从 catalog 和 SHOW CREATE 读取完整对象；权限不足不能解释为不存在。
    pub async fn describe_migration(
        &mut self,
        target: ResourceTarget,
        cancel: &CancellationToken,
    ) -> Result<MigrationTable> {
        let schema = catalog::schema(self, target, cancel).await?;
        if schema.target.kind != "table" {
            return Err(AppError::invalid("直接迁移只接受基表"));
        }
        let mut conn = self.new_connection().await?;
        sqlx::raw_sql("SET SESSION time_zone = '+00:00'")
            .execute(&mut conn)
            .await?;
        let row = sqlx::query("SELECT VERSION(), @@SESSION.sql_mode, @@SESSION.time_zone")
            .fetch_one(&mut conn)
            .await?;
        let version: String = row.try_get(0)?;
        let sql_mode: String = row.try_get(1)?;
        let time_zone: String = row.try_get(2)?;
        ensure_version(&version)?;
        let db = schema
            .target
            .database
            .as_deref()
            .ok_or_else(|| AppError::invalid("必须指定数据库"))?;
        let rows = sqlx::query("SELECT COLUMN_NAME, CHARACTER_SET_NAME, COLLATION_NAME FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION").bind(db).bind(&schema.target.name).fetch_all(&mut conn).await?;
        let mut collations = std::collections::BTreeMap::new();
        for row in rows {
            collations.insert(
                row.try_get("COLUMN_NAME")?,
                (
                    row.try_get("CHARACTER_SET_NAME")?,
                    row.try_get("COLLATION_NAME")?,
                ),
            );
        }
        if collations.len() != schema.columns.len() {
            return Err(AppError::new(
                "schema_incomplete",
                "无法完整读取列字符集与排序规则",
            ));
        }
        let fingerprint = hash(&schema.ddl, &sql_mode);
        let snapshot = super::schema::describe_on(&mut conn, schema.target.clone())
            .await?
            .ok_or_else(|| AppError::conflict("读取期间目标表已消失"))?;
        if snapshot.ddl != schema.ddl || snapshot.sql_mode != sql_mode {
            return Err(AppError::conflict("读取期间表结构或 SQL mode 已变化"));
        }
        Ok(MigrationTable {
            schema,
            version,
            sql_mode,
            fingerprint,
            collations,
            time_zone,
            snapshot: Some(snapshot),
        })
    }
    /// 获取服务端物理身份，用于跨 profile 自拷贝检查。
    pub async fn server_identity(&self) -> Result<String> {
        let mut conn = self.new_connection().await?;
        let identity: String = sqlx::query_scalar("SELECT @@server_uuid")
            .fetch_one(&mut conn)
            .await?;
        if identity.is_empty() {
            return Err(AppError::new(
                "identity_unknown",
                "无法确定 MySQL 服务端身份",
            ));
        }
        Ok(identity)
    }

    /// 同一服务端内以 catalog 返回的物理表身份判断，遵循 lower_case_table_names。
    pub async fn same_table_as(
        &self,
        other: &MysqlSession,
        source: &ResourceTarget,
        target: &ResourceTarget,
    ) -> Result<bool> {
        if self.server_identity().await? != other.server_identity().await? {
            return Ok(false);
        }
        let mut conn = self.new_connection().await?;
        let mut identities = Vec::with_capacity(2);
        for table in [source, target] {
            let db = table
                .database
                .as_deref()
                .ok_or_else(|| AppError::invalid("表缺少数据库"))?;
            let rows = sqlx::query("SELECT TABLE_SCHEMA, TABLE_NAME FROM information_schema.TABLES WHERE BINARY IF(@@lower_case_table_names = 0, TABLE_SCHEMA, LOWER(TABLE_SCHEMA)) = BINARY IF(@@lower_case_table_names = 0, ?, LOWER(?)) AND BINARY IF(@@lower_case_table_names = 0, TABLE_NAME, LOWER(TABLE_NAME)) = BINARY IF(@@lower_case_table_names = 0, ?, LOWER(?))")
                .bind(db).bind(db).bind(&table.name).bind(&table.name)
                .fetch_all(&mut conn).await?;
            if rows.len() > 1 {
                return Err(AppError::new("identity_unknown", "catalog 表身份不唯一"));
            }
            identities.push(
                rows.first()
                    .map(|row| -> Result<(String, String)> {
                        Ok((row.try_get(0)?, row.try_get(1)?))
                    })
                    .transpose()?,
            );
        }
        Ok(identities[0].is_some() && identities[0] == identities[1])
    }
    /// 校验两端映射列的目录信息；不能静默改变字符集、排序规则或时间戳会话时区。
    pub fn validate_migration_mapping(
        source: &MigrationTable,
        target: &MigrationTable,
        object: &TransferObject,
    ) -> Result<()> {
        let sources = source_columns(source, object)?;
        let targets = mapped_columns(target, object)?;
        if sources.len() != targets.len() {
            return Err(AppError::invalid("源和目标映射列数量不同"));
        }
        for (s, t) in sources.iter().zip(targets.iter()) {
            if object.columns.is_empty() && s.name != t.name {
                return Err(AppError::invalid(format!(
                    "源列 {} 与目标列 {} 不同，必须显式映射",
                    s.name, t.name
                )));
            }
            if source.collations.get(&s.name) != target.collations.get(&t.name) {
                return Err(AppError::new(
                    "incompatible_collation",
                    format!("列 {} → {} 的字符集或排序规则不一致", s.name, t.name),
                ));
            }
            if (s.type_name.to_ascii_lowercase().starts_with("timestamp")
                || t.type_name.to_ascii_lowercase().starts_with("timestamp"))
                && (source.time_zone != target.time_zone
                    || source.time_zone.eq_ignore_ascii_case("SYSTEM"))
            {
                return Err(AppError::new(
                    "incompatible_timezone",
                    format!(
                        "列 {} → {} 的时间戳时区无法证明一致，请将两端会话时区设置为相同明确值",
                        s.name, t.name
                    ),
                ));
            }
        }
        Ok(())
    }
    /// 准备阶段只采样一条有 LIMIT 的查询；不创建结果缓存。
    pub async fn sample_migration(
        &mut self,
        object: &TransferObject,
        max_rows: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<Vec<Value>>> {
        let table = self
            .describe_migration(object.source.clone(), cancel)
            .await?;
        let (mut sql, mut args) = select_statement(&table, object)?;
        let limit = max_rows.min(1000) as u64;
        sql.push_str(" LIMIT ?");
        args.add(limit)
            .map_err(|e| AppError::invalid(e.to_string()))?;
        let mut conn = self.new_connection().await?;
        sqlx::raw_sql("SET SESSION time_zone = '+00:00'")
            .execute(&mut conn)
            .await?;
        let mut stream = sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch(&mut conn);
        let mut rows = Vec::new();
        let mut bytes = 0usize;
        loop {
            let row = tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), row = stream.try_next() => row? };
            let Some(row) = row else { break };
            let row = decode_row(&row)?;
            let size = row_bytes(&row)?;
            if !rows.is_empty() && bytes.saturating_add(size) > MAX_BATCH_BYTES {
                break;
            }
            bytes += size;
            rows.push(row);
        }
        Ok(rows)
    }
    /// 所有对象在同一独立物理连接和同一个只读快照中顺序读取，通道容量为两批。
    pub async fn open_source_stream(
        &self,
        objects: Vec<(MigrationTable, TransferObject)>,
        consistency: ReadConsistency,
        cancel: CancellationToken,
    ) -> Result<mpsc::Receiver<Result<MigrationBatch>>> {
        for (table, object) in &objects {
            if consistency == ReadConsistency::Transaction
                && !table.schema.engine.eq_ignore_ascii_case("innodb")
            {
                return Err(AppError::new(
                    "inconsistent_engine",
                    format!(
                        "表 {} 非 InnoDB，需明确选择尽力读取",
                        table.schema.target.name
                    ),
                ));
            }
            select_statement(table, object)?;
        }
        let mut conn = self.new_connection().await?;
        sqlx::raw_sql("SET SESSION time_zone = '+00:00'")
            .execute(&mut conn)
            .await?;
        sqlx::raw_sql("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .execute(&mut conn)
            .await?;
        sqlx::raw_sql("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")
            .execute(&mut conn)
            .await?;
        let (tx, rx) = mpsc::channel(2);
        tokio::spawn(async move {
            let result: Result<()> = async {
                for (index, (table, object)) in objects.iter().enumerate() {
                    ensure_not_cancelled(&cancel)?;
                    if fingerprint(&mut conn, &table.schema.target).await? != table.fingerprint { return Err(AppError::new("conflict", "源表结构已变化")); }
                    let (sql, args) = select_statement(table, object)?;
                    let mut stream = sqlx::query_with(sqlx::AssertSqlSafe(sql), args).fetch(&mut conn);
                    let mut rows = Vec::new(); let mut bytes = 0usize;
                    loop {
                        let row = tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), _ = tx.closed() => return Ok(()), r = stream.try_next() => r? };
                        let Some(row) = row else { break }; let row = decode_row(&row)?; let size = row_bytes(&row)?;
                        if !rows.is_empty() && (rows.len() >= MAX_BATCH_ROWS || bytes.saturating_add(size) > MAX_BATCH_BYTES) {
                            let batch = MigrationBatch { object: index, rows: std::mem::take(&mut rows), bytes };
                            tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), r = tx.send(Ok(batch)) => if r.is_err() { return Ok(()) } }
                            bytes = 0;
                        }
                        bytes += size; rows.push(row);
                    }
                    drop(stream);
                    if fingerprint(&mut conn, &table.schema.target).await? != table.fingerprint { return Err(AppError::new("conflict", "读取期间源表结构已变化")); }
                    if !rows.is_empty() { tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), r = tx.send(Ok(MigrationBatch { object: index, rows, bytes })) => if r.is_err() { return Ok(()) } } }
                }
                Ok(())
            }.await;
            if let Err(e) = result {
                let _ = tx.send(Err(e)).await;
            }
            let _ = sqlx::raw_sql("ROLLBACK").execute(&mut conn).await;
        });
        Ok(rx)
    }
}

fn mapped_columns<'a>(
    table: &'a MigrationTable,
    object: &TransferObject,
) -> Result<Vec<&'a Column>> {
    let names: Vec<&str> = if object.columns.is_empty() {
        table
            .schema
            .columns
            .iter()
            .filter(|c| !c.generated)
            .map(|c| c.name.as_str())
            .collect()
    } else {
        object.columns.iter().map(|m| m.target.as_str()).collect()
    };
    let mut seen = std::collections::HashSet::new();
    names
        .into_iter()
        .map(|name| {
            if !seen.insert(name) {
                return Err(AppError::invalid("目标列映射重复"));
            }
            let column = table
                .schema
                .columns
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| AppError::invalid(format!("目标列不存在: {name}")))?;
            if column.generated {
                return Err(AppError::invalid(format!("生成列不可写: {name}")));
            }
            Ok(column)
        })
        .collect()
}
fn bind_row(
    args: &mut MySqlArguments,
    columns: &[&Column],
    row: &[Value],
    indices: &[usize],
) -> Result<()> {
    for &i in indices {
        let v = &row[i];
        if matches!(v, Value::Null) {
            args.add(Option::<String>::None)
                .map_err(|e| AppError::invalid(e.to_string()))?;
        } else if columns[i]
            .type_name
            .to_ascii_lowercase()
            .starts_with("year")
        {
            let n = match v {
                Value::Integer(s) | Value::Unsigned(s) => s
                    .parse::<u16>()
                    .map_err(|_| AppError::invalid("年份无效"))?,
                _ => return Err(AppError::invalid("年份类型无效")),
            };
            args.add(n).map_err(|e| AppError::invalid(e.to_string()))?;
        } else {
            add_arg(args, v)?;
        }
    }
    Ok(())
}
pub(super) enum MigrationBatchSource<'a> {
    Transfer(&'a CancellationToken),
    SqlRestore(&'a [String]),
}

impl MysqlSession {
    /// 写入一个有界批次。调用方在调用前后登记批次日志；每次发送由 sink 保护。
    pub async fn write_batch(
        &self,
        table: &MigrationTable,
        object: &TransferObject,
        policy: ConflictPolicy,
        rows: Vec<Vec<Value>>,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<MigrationWriteOutcome> {
        self.write_batch_with_context(
            table,
            object,
            policy,
            rows,
            sink,
            MigrationBatchSource::Transfer(cancel),
        )
        .await
    }

    pub(super) async fn write_batch_with_context(
        &self,
        table: &MigrationTable,
        object: &TransferObject,
        policy: ConflictPolicy,
        rows: Vec<Vec<Value>>,
        sink: &ExecutionSink,
        source: MigrationBatchSource<'_>,
    ) -> Result<MigrationWriteOutcome> {
        let (settings, cancel) = match source {
            MigrationBatchSource::Transfer(cancel) => (&[][..], Some(cancel)),
            MigrationBatchSource::SqlRestore(settings) => (settings, None),
        };
        self.profile.ensure_write()?;
        if !table.schema.engine.eq_ignore_ascii_case("innodb") && rows.len() > 1 {
            return Err(AppError::invalid("非事务表必须逐行登记和写入"));
        }
        if rows.len() > MAX_BATCH_ROWS {
            return Err(AppError::invalid("批次超过 500 行"));
        }
        let columns = mapped_columns(table, object)?;
        if columns.is_empty() {
            return Err(AppError::invalid("目标列为空"));
        }
        let mut bytes = 0usize;
        for row in &rows {
            if row.len() != columns.len() {
                return Err(AppError::invalid("数据列数与映射不一致"));
            }
            bytes = bytes.saturating_add(row_bytes(row)?);
            for (v, c) in row.iter().zip(&columns) {
                super::changes::validate_import_value(v, c)?;
            }
        }
        if rows.len() > 1 && bytes > MAX_BATCH_BYTES {
            return Err(AppError::invalid("批次超过 1 MiB"));
        }
        let key: Vec<usize> = if policy == ConflictPolicy::Insert {
            Vec::new()
        } else {
            if !table.schema.engine.eq_ignore_ascii_case("innodb") {
                return Err(AppError::new(
                    "unsupported_engine",
                    "跳过和覆盖需要 InnoDB 行锁",
                ));
            }
            if object.locator_key.is_empty()
                || !table.schema.keys.iter().any(|k| k == &object.locator_key)
            {
                return Err(AppError::invalid("必须显式选择完整可靠唯一键"));
            }
            object
                .locator_key
                .iter()
                .map(|name| {
                    columns
                        .iter()
                        .position(|c| &c.name == name)
                        .ok_or_else(|| AppError::invalid(format!("定位键未映射: {name}")))
                })
                .collect::<Result<Vec<_>>>()?
        };
        let mut conn = self.new_connection().await?;
        sqlx::raw_sql("SET SESSION time_zone = '+00:00'")
            .execute(&mut conn)
            .await?;
        // 恢复字面量按 dump 的会话时区解释；直接复制仍使用 UTC。
        for setting in settings {
            sqlx::raw_sql(sqlx::AssertSqlSafe(setting.as_str()))
                .execute(&mut conn)
                .await?;
        }
        if fingerprint(&mut conn, &table.schema.target).await? != table.fingerprint {
            return Err(AppError::conflict("目标表结构已变化"));
        }
        // 保留显式源值 0；由目标生成自增值时已省略该列，不能借 MySQL 的零值规则悄悄换键。
        sqlx::raw_sql("SET SESSION sql_mode = CONCAT(@@SESSION.sql_mode, IF(@@SESSION.sql_mode = '', '', ','), 'STRICT_ALL_TABLES,NO_AUTO_VALUE_ON_ZERO')").execute(&mut conn).await?;
        let transactional = table.schema.engine.eq_ignore_ascii_case("innodb");
        if transactional {
            sqlx::raw_sql("START TRANSACTION")
                .execute(&mut conn)
                .await?;
        }
        let target = qualified(&table.schema.target)?;
        let names = columns
            .iter()
            .map(|c| quote_ident(&c.name))
            .collect::<Result<Vec<_>>>()?;
        let all: Vec<usize> = (0..columns.len()).collect();
        let mut states = vec![TransferStepState::NotSent; rows.len()];
        let mut marked_sent = false;
        let check_cancelled = || {
            sink.check_cancelled()?;
            cancel.map_or(Ok(()), crate::model::ensure_not_cancelled)
        };
        // 最后一轮只检查提交边界；任何取消都沿相同的回滚确认路径处理。
        for index in 0..=rows.len() {
            let result: Result<Option<bool>> = async {
                check_cancelled()?;
                let Some(row) = rows.get(index) else {
                    return Ok(None);
                };
                let exists = if key.is_empty() {
                    false
                } else {
                    if key.iter().any(|i| matches!(row[*i], Value::Null)) {
                        return Err(AppError::invalid("定位键不能为 NULL"));
                    }
                    let predicates = key
                        .iter()
                        .map(|i| format!("{} = ?", names[*i]))
                        .collect::<Vec<_>>()
                        .join(" AND ");
                    let mut args = MySqlArguments::default();
                    bind_row(&mut args, &columns, row, &key)?;
                    let sql = format!("SELECT 1 FROM {target} WHERE {predicates} FOR UPDATE");
                    sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                        .fetch_optional(&mut conn)
                        .await?
                        .is_some()
                };
                if exists && policy == ConflictPolicy::Skip {
                    return Ok(Some(false));
                }
                let mut args = MySqlArguments::default();
                let sql = if exists {
                    let update: Vec<usize> =
                        all.iter().copied().filter(|i| !key.contains(i)).collect();
                    if update.is_empty() {
                        return Ok(Some(false));
                    }
                    bind_row(&mut args, &columns, row, &update)?;
                    bind_row(&mut args, &columns, row, &key)?;
                    format!(
                        "UPDATE {target} SET {} WHERE {}",
                        update
                            .iter()
                            .map(|i| format!("{} = ?", names[*i]))
                            .collect::<Vec<_>>()
                            .join(", "),
                        key.iter()
                            .map(|i| format!("{} = ?", names[*i]))
                            .collect::<Vec<_>>()
                            .join(" AND ")
                    )
                } else {
                    bind_row(&mut args, &columns, row, &all)?;
                    format!(
                        "INSERT INTO {target} ({}) VALUES ({})",
                        names.join(", "),
                        vec!["?"; names.len()].join(", ")
                    )
                };
                if !marked_sent {
                    sink.mark_sent(true).await?;
                    marked_sent = true;
                }
                // 上方的锁定读取或发送登记可能等待，真正派发前再次检查。
                check_cancelled()?;
                sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
                    .execute(&mut conn)
                    .await?;
                Ok(Some(true))
            }
            .await;
            match result {
                Ok(Some(true)) => states[index] = TransferStepState::Applied,
                Ok(Some(false)) => states[index] = TransferStepState::Skipped,
                Ok(None) => break,
                Err(mut error) => {
                    if error.code != "cancelled"
                        && let Some(state) = states.get_mut(index)
                    {
                        *state = TransferStepState::Failed;
                    }
                    if transactional {
                        let rollback = sqlx::raw_sql("ROLLBACK").execute(&mut conn).await;
                        let confirmed = if rollback.is_ok() {
                            sqlx::raw_sql("SHOW COUNT(*) WARNINGS")
                                .fetch_one(&mut conn)
                                .await
                                .ok()
                                .is_some_and(|r| {
                                    r.try_get::<u64, _>(0).is_ok_and(|v| v == 0)
                                        || r.try_get::<i64, _>(0).is_ok_and(|v| v == 0)
                                })
                        } else {
                            false
                        };
                        for state in &mut states[..index] {
                            if *state == TransferStepState::Applied {
                                *state = if confirmed {
                                    TransferStepState::RolledBack
                                } else {
                                    TransferStepState::OutcomeUnknown
                                };
                            }
                        }
                        if !confirmed {
                            if error.code != "cancelled"
                                && let Some(state) = states.get_mut(index)
                            {
                                *state = TransferStepState::OutcomeUnknown;
                            }
                            error = AppError::unknown(format!("回滚结果无法确认: {error}"));
                        }
                    } else if error.code == "database" && error.database_code.is_none() {
                        states[index] = TransferStepState::OutcomeUnknown;
                        error = AppError::unknown(format!("写入结果无法确认: {error}"));
                    }
                    return Err(error.with_details(json!({"index":index,"states":states})));
                }
            }
        }
        if transactional && let Err(e) = sqlx::raw_sql("COMMIT").execute(&mut conn).await {
            for state in &mut states {
                if *state == TransferStepState::Applied {
                    *state = TransferStepState::OutcomeUnknown;
                }
            }
            return Err(AppError::unknown(format!("事务提交结果无法确认: {e}"))
                .with_details(json!({"states":states})));
        }
        let affected = states
            .iter()
            .filter(|s| **s == TransferStepState::Applied)
            .count() as u64;
        Ok(MigrationWriteOutcome { states, affected })
    }
}

/// SHOW CREATE 的顶层组成部分；括号、注释及字符串中的逗号不拆分。
fn ddl_parts(ddl: &str) -> Result<(Vec<String>, String)> {
    let start = ddl
        .find('(')
        .ok_or_else(|| AppError::new("schema_incomplete", "建表语句缺少列定义"))?;
    let bytes = ddl.as_bytes();
    let mut pos = start + 1;
    let mut begin = pos;
    let mut depth = 1usize;
    let mut quote = 0u8;
    let mut parts = Vec::new();
    while pos < bytes.len() {
        let c = bytes[pos];
        if quote != 0 {
            if c == b'\\' && quote != b'`' {
                pos += 2;
                continue;
            }
            if c == quote {
                if bytes.get(pos + 1) == Some(&quote) {
                    pos += 2;
                    continue;
                }
                quote = 0;
            }
        } else {
            match c {
                b'\'' | b'"' | b'`' => quote = c,
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        let part = ddl[begin..pos].trim();
                        if !part.is_empty() {
                            parts.push(part.to_string());
                        }
                        let suffix = ddl[pos + 1..]
                            .trim()
                            .strip_suffix(';')
                            .unwrap_or(ddl[pos + 1..].trim())
                            .trim();
                        return Ok((parts, suffix.to_string()));
                    }
                }
                b',' if depth == 1 => {
                    parts.push(ddl[begin..pos].trim().to_string());
                    begin = pos + 1;
                }
                _ => (),
            }
        }
        pos += 1;
    }
    Err(AppError::new("schema_incomplete", "无法完整解析建表语句"))
}
fn identifier(s: &str) -> Option<(String, usize)> {
    let b = s.as_bytes();
    if b.first() != Some(&b'`') {
        return None;
    }
    let mut end = 1;
    let mut name = String::new();
    let mut start = 1;
    while end < b.len() {
        if b[end] == b'`' {
            name.push_str(&s[start..end]);
            if b.get(end + 1) == Some(&b'`') {
                name.push('`');
                end += 2;
                start = end;
                continue;
            }
            return Some((name, end + 1));
        }
        end += 1;
    }
    None
}
fn part_key(part: &str) -> Option<(String, String)> {
    if let Some((name, _)) = identifier(part) {
        return Some(("column".into(), name));
    }
    if part.starts_with("PRIMARY KEY ") || part.starts_with("PRIMARY KEY(") {
        return Some(("primary".into(), "PRIMARY".into()));
    }
    for (prefix, kind) in [
        ("UNIQUE KEY ", "index"),
        ("FULLTEXT KEY ", "index"),
        ("SPATIAL KEY ", "index"),
        ("KEY ", "index"),
        ("CONSTRAINT ", "constraint"),
    ] {
        if let Some(rest) = part.strip_prefix(prefix) {
            return identifier(rest).map(|(name, _)| (kind.into(), name));
        }
    }
    None
}
fn rename_identifiers(part: &str, object: &TransferObject) -> Result<String> {
    let mut out = String::new();
    let mut pos = 0;
    // SHOW CREATE 的字符串文字保持原样，仅改写独立的反引号标识符。
    let b = part.as_bytes();
    let mut string = 0u8;
    while pos < b.len() {
        let c = b[pos];
        if string != 0 {
            let start = pos;
            if c == b'\\' && pos + 1 < b.len() {
                pos += 1;
                pos += part[pos..].chars().next().unwrap().len_utf8();
            } else if c == string && b.get(pos + 1) == Some(&string) {
                pos += 2;
            } else {
                let ch = part[pos..].chars().next().unwrap();
                pos += ch.len_utf8();
                if c == string {
                    string = 0;
                }
            }
            out.push_str(&part[start..pos]);
            continue;
        }
        if b[pos..].starts_with(b"/*") {
            let end = part[pos + 2..]
                .find("*/")
                .map(|n| pos + 2 + n + 2)
                .ok_or_else(|| AppError::new("schema_incomplete", "结构定义的注释未闭合"))?;
            out.push_str(&part[pos..end]);
            pos = end;
            continue;
        }
        if c == b'#'
            || (b[pos..].starts_with(b"--") && b.get(pos + 2).is_none_or(u8::is_ascii_whitespace))
        {
            let end = part[pos..]
                .find('\n')
                .map(|n| pos + n)
                .unwrap_or(part.len());
            out.push_str(&part[pos..end]);
            pos = end;
            continue;
        }
        if c == b'\'' || c == b'"' {
            string = c;
            out.push(c as char);
            pos += 1;
            continue;
        }
        if c == b'`' {
            let (name, n) = identifier(&part[pos..])
                .ok_or_else(|| AppError::new("schema_incomplete", "无效的标识符引用"))?;
            let after = skip_space(part, pos + n);
            let is_object_name = (!part.starts_with('`')
                && pos < part.find('(').unwrap_or(part.len()))
                || b.get(after) == Some(&b'.');
            let mapped = if is_object_name {
                name.as_str()
            } else {
                object
                    .columns
                    .iter()
                    .find(|m| m.source == name)
                    .map(|m| m.target.as_str())
                    .unwrap_or(&name)
            };
            out.push_str(&quote_ident(mapped)?);
            pos += n;
            continue;
        }
        let ch = part[pos..]
            .chars()
            .next()
            .ok_or_else(|| AppError::invalid("无效的结构文本"))?;
        out.push(ch);
        pos += ch.len_utf8();
    }
    Ok(out)
}
/// 在 SQL 字面量、标识符和注释之外定位关键字，避免把列默认值当作结构。
fn keyword_position(text: &str, keyword: &str) -> Option<usize> {
    let b = text.as_bytes();
    let k = keyword.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if matches!(b[i], b'\'' | b'"' | b'`') {
            let q = b[i];
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' && q != b'`' {
                    i += 2;
                    continue;
                }
                if b[i] == q {
                    if b.get(i + 1) == Some(&q) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            i += 2;
            while i + 1 < b.len() && !b[i..].starts_with(b"*/") {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else if b[i] == b'#'
            || (b[i..].starts_with(b"--") && b.get(i + 2).is_none_or(u8::is_ascii_whitespace))
        {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if i + k.len() <= b.len()
            && b[i..i + k.len()].eq_ignore_ascii_case(k)
            && (i == 0 || !(b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_'))
            && (i + k.len() == b.len()
                || !(b[i + k.len()].is_ascii_alphanumeric() || b[i + k.len()] == b'_'))
        {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}
fn ddl_name(text: &str) -> Option<(String, usize)> {
    if text.starts_with('`') {
        return identifier(text);
    }
    let end = text
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'$')
        .count();
    (end > 0).then(|| (text[..end].to_string(), end))
}
fn skip_space(text: &str, mut i: usize) -> usize {
    while text.as_bytes().get(i).is_some_and(u8::is_ascii_whitespace) {
        i += 1;
    }
    i
}

fn desired_part(part: &str, object: &TransferObject) -> Result<String> {
    let rewritten = rename_identifiers(part, object)?;
    let Some((name, end)) = identifier(&rewritten) else {
        return Ok(rewritten);
    };
    let Some(column) = object.target_columns.iter().find(|c| c.name == name) else {
        return Ok(rewritten);
    };
    let ty = column.type_name.trim();
    let words = ty.split_whitespace().collect::<Vec<_>>();
    let native = words.first().copied().unwrap_or("");
    let base = native.split('(').next().unwrap_or("").to_ascii_lowercase();
    let allowed = [
        "tinyint",
        "smallint",
        "mediumint",
        "int",
        "integer",
        "bigint",
        "decimal",
        "numeric",
        "float",
        "double",
        "real",
        "bit",
        "date",
        "datetime",
        "timestamp",
        "time",
        "year",
        "char",
        "varchar",
        "binary",
        "varbinary",
        "tinytext",
        "text",
        "mediumtext",
        "longtext",
        "tinyblob",
        "blob",
        "mediumblob",
        "longblob",
        "json",
    ];
    let tail = rewritten[end..].trim_start();
    let bytes = tail.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    let mut quote = 0u8;
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == b'\\' {
                i += 2;
                continue;
            }
            if c == quote {
                quote = 0;
            }
        } else if c == b'\'' || c == b'"' {
            quote = c;
        } else if c == b'(' {
            depth += 1;
        } else if c == b')' {
            depth = depth.saturating_sub(1);
        } else if c.is_ascii_whitespace() && depth == 0 {
            break;
        }
        i += 1;
    }
    if i == 0 || depth != 0 || quote != 0 {
        return Err(AppError::new(
            "schema_incomplete",
            format!("无法识别列 {name} 的源类型"),
        ));
    }
    if !ty.eq_ignore_ascii_case(&tail[..i])
        && (!allowed.contains(&base.as_str())
            || !native
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"(),".contains(&b))
            || words.iter().skip(1).any(|s| {
                !s.eq_ignore_ascii_case("unsigned") && !s.eq_ignore_ascii_case("zerofill")
            }))
    {
        return Err(AppError::new(
            "schema_incomplete",
            format!("列 {name} 的候选类型包含无法安全生成的属性: {ty}"),
        ));
    }
    let mut rest = tail[i..].to_string();
    // UNSIGNED / ZEROFILL 属于类型；候选类型覆盖源类型时不能残留旧修饰符。
    for modifier in [" unsigned", " zerofill"] {
        if rest.to_ascii_lowercase().starts_with(modifier) {
            rest.drain(..modifier.len());
        }
    }
    let upper = rest.to_ascii_uppercase();
    let boundary = [
        " DEFAULT",
        " COMMENT",
        " GENERATED",
        " AUTO_INCREMENT",
        " ON UPDATE",
    ]
    .iter()
    .filter_map(|s| upper.find(s))
    .min()
    .unwrap_or(rest.len());
    let null_clause = upper[..boundary]
        .find(" NOT NULL")
        .map(|p| (p, 9))
        .or_else(|| upper[..boundary].find(" NULL").map(|p| (p, 5)));
    if let Some((start, len)) = null_clause {
        rest.replace_range(
            start..start + len,
            if column.nullable {
                " NULL"
            } else {
                " NOT NULL"
            },
        );
    } else if !column.nullable {
        rest.insert_str(0, " NOT NULL");
    }
    Ok(format!("{} {}{}", &rewritten[..end], ty, rest))
}
fn schema_item(
    object: &TransferObject,
    id: String,
    action: &str,
    sql: Option<String>,
    destructive: bool,
    precondition: Option<String>,
    notes: Vec<String>,
) -> TransferPlanItem {
    // 长度前缀避免不同库、表、步骤和 SQL 拼接得到相同输入。
    let mut hash = Sha256::new();
    for value in [
        object.target.database.as_deref().unwrap_or(""),
        object.target.name.as_str(),
        id.as_str(),
        sql.as_deref().unwrap_or(""),
    ] {
        hash.update((value.len() as u64).to_be_bytes());
        hash.update(value.as_bytes());
    }
    let id = format!("schema:{:x}", base16ct::HexDisplay(&hash.finalize()));
    let selected = object.selected_schema_items.contains(&id);
    TransferPlanItem {
        id,
        phase: "schema".into(),
        action: action.into(),
        source: Some(object.source.clone()),
        target: Some(object.target.clone()),
        sql,
        destructive,
        selected,
        precondition,
        notes,
    }
}
/// 只生成输入显式声明列的结构变化；未输入的目标列及索引不进入差异。
fn input_schema_items(
    current: &MigrationTable,
    columns: &[Column],
    mapping: &std::collections::BTreeMap<String, String>,
    selected: &[String],
) -> Result<Vec<TransferPlanItem>> {
    if current
        .sql_mode
        .split(',')
        .any(|mode| mode.trim().eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES"))
    {
        return Err(AppError::new(
            "schema_incomplete",
            "目标 SQL mode 使用 NO_BACKSLASH_ESCAPES，无法安全解析列定义",
        ));
    }
    let (parts, _) = ddl_parts(&current.schema.ddl)?;
    let definitions: std::collections::BTreeMap<String, &str> = parts
        .iter()
        .filter_map(|part| identifier(part).map(|(name, _)| (name, part.as_str())))
        .collect();
    let mut object = TransferObject {
        source: current.schema.target.clone(),
        target: current.schema.target.clone(),
        columns: Vec::new(),
        target_columns: Vec::new(),
        filters: Vec::new(),
        locator_key: Vec::new(),
        create_if_missing: false,
        sync_schema: true,
        selected_schema_items: selected.to_vec(),
    };
    let mut seen = std::collections::HashSet::new();
    for column in columns {
        let name = mapping
            .get(&column.name)
            .map(String::as_str)
            .unwrap_or(&column.name);
        if name.is_empty() || !seen.insert(name.to_owned()) {
            return Err(AppError::invalid(format!("输入目标列为空或重复: {name}")));
        }
        let mut candidate = column.clone();
        candidate.name = name.to_owned();
        object.target_columns.push(candidate);
    }
    let target = qualified(&current.schema.target)?;
    let mut items = Vec::new();
    for column in &object.target_columns {
        let (action, clause, destructive) = if let Some(previous) = definitions.get(&column.name) {
            let desired = desired_part(previous, &object)?;
            if desired == *previous {
                continue;
            }
            ("modify_column", format!("MODIFY COLUMN {desired}"), true)
        } else {
            if column.generated || column.auto_increment {
                return Err(AppError::new(
                    "schema_incomplete",
                    format!(
                        "新增列 {} 的生成表达式或自增属性无法从文件列类型安全构建",
                        column.name
                    ),
                ));
            }
            let base = format!("{} int", quote_ident(&column.name)?);
            let desired = desired_part(&base, &object)?;
            ("add_column", format!("ADD COLUMN {desired}"), false)
        };
        let mut item = schema_item(
            &object,
            format!("input:column:{}", column.name),
            action,
            Some(format!("ALTER TABLE {target} {clause}")),
            destructive,
            Some(current.fingerprint.clone()),
            vec!["仅改变显式指定的列类型和 NULL 属性；ALTER 可能锁表或重建表".into()],
        );
        item.source = None;
        items.push(item);
    }
    Ok(items)
}
fn version_number(version: &str) -> (u32, u32, u32) {
    let mut p = version.split('.').map(|s| {
        s.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse()
            .unwrap_or(0)
    });
    (
        p.next().unwrap_or(0),
        p.next().unwrap_or(0),
        p.next().unwrap_or(0),
    )
}
fn ensure_version(version: &str) -> Result<()> {
    let v = version_number(version);
    if version.to_ascii_lowercase().contains("mariadb")
        || !matches!((v.0, v.1), (5, 7) | (8, 0) | (8, 4))
    {
        return Err(AppError::new(
            "unsupported_version",
            format!("迁移不支持服务端版本 {version}；支持 MySQL 5.7、8.0、8.4"),
        ));
    }
    Ok(())
}
fn strip_sql_keyword<'a>(text: &'a str, keyword: &str) -> Option<&'a str> {
    let text = text.trim_start();
    let prefix = text.get(..keyword.len())?;
    if !prefix.eq_ignore_ascii_case(keyword) {
        return None;
    }
    let rest = &text[keyword.len()..];
    if rest
        .as_bytes()
        .first()
        .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_')
    {
        return None;
    }
    Some(rest.trim_start())
}
fn strip_qualified_name(text: &str) -> Result<&str> {
    let text = text.trim_start();
    let (_, n) = ddl_name(text)
        .ok_or_else(|| AppError::new("schema_incomplete", "结构语句的表名无法解析"))?;
    let mut rest = text[n..].trim_start();
    if let Some(next) = rest.strip_prefix('.') {
        let next = next.trim_start();
        let (_, n) = ddl_name(next)
            .ok_or_else(|| AppError::new("schema_incomplete", "结构语句的限定表名无法解析"))?;
        rest = next[n..].trim_start();
    }
    Ok(rest)
}
fn canonical_restore_definition(text: &str) -> Result<String> {
    let text = text.trim();
    if part_key(text).is_some() {
        return Ok(text.to_string());
    }
    for (prefix, canonical) in [
        ("UNIQUE INDEX", "UNIQUE KEY"),
        ("UNIQUE KEY", "UNIQUE KEY"),
        ("FULLTEXT INDEX", "FULLTEXT KEY"),
        ("FULLTEXT KEY", "FULLTEXT KEY"),
        ("SPATIAL INDEX", "SPATIAL KEY"),
        ("SPATIAL KEY", "SPATIAL KEY"),
        ("INDEX", "KEY"),
        ("KEY", "KEY"),
        ("CONSTRAINT", "CONSTRAINT"),
    ] {
        if let Some(rest) = strip_sql_keyword(text, prefix) {
            let (name, n) = ddl_name(rest)
                .ok_or_else(|| AppError::new("schema_incomplete", format!("{prefix} 缺少名称")))?;
            return Ok(format!("{canonical} {}{}", quote_ident(&name)?, &rest[n..]));
        }
    }
    // 文件中的普通列名可以不加引号，但 SQL 关键字和不支持的表属性不能当成列。
    let (name, n) = ddl_name(text)
        .ok_or_else(|| AppError::new("schema_incomplete", format!("结构定义无法识别: {text}")))?;
    let rest = text[n..].trim_start();
    if rest.is_empty()
        || [
            "CONSTRAINT",
            "PRIMARY",
            "FOREIGN",
            "CHECK",
            "UNIQUE",
            "INDEX",
            "KEY",
            "FULLTEXT",
            "SPATIAL",
        ]
        .iter()
        .any(|word| name.eq_ignore_ascii_case(word))
    {
        return Err(AppError::new(
            "schema_incomplete",
            format!("结构定义无法安全识别: {text}"),
        ));
    }
    Ok(format!("{} {rest}", quote_ident(&name)?))
}
fn restore_schema_sql(source: &MigrationTable, sql: &str) -> Result<String> {
    let text = sql.trim().strip_suffix(';').unwrap_or(sql.trim()).trim();
    if text.len() > MAX_VALUE_BYTES {
        return Err(AppError::new("resource_limit", "结构语句超过 16 MiB"));
    }
    // reader 应只传一条结构语句；再次检查未加引号的分号，避免把额外语句拼入计划。
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut quote = 0u8;
    while i < bytes.len() {
        let c = bytes[i];
        if quote != 0 {
            if c == b'\\' && quote != b'`' {
                i += 2;
                continue;
            }
            if c == quote {
                if bytes.get(i + 1) == Some(&quote) {
                    i += 2;
                    continue;
                }
                quote = 0;
            }
        } else if matches!(c, b'\'' | b'"' | b'`') {
            quote = c;
        } else if c == b';' {
            return Err(AppError::invalid("结构输入必须恰好包含一条语句"));
        }
        i += 1;
    }
    if quote != 0 {
        return Err(AppError::new("schema_incomplete", "结构语句的引号未闭合"));
    }
    let (mut parts, options) = ddl_parts(&source.schema.ddl)?;
    let table_start = source
        .schema
        .ddl
        .find('(')
        .ok_or_else(|| AppError::new("schema_incomplete", "建表基线缺少字段列表"))?;
    if let Some(after_create) = strip_sql_keyword(text, "CREATE") {
        let (kind, after_kind) = if let Some(rest) = strip_sql_keyword(after_create, "UNIQUE") {
            ("UNIQUE KEY", rest)
        } else if let Some(rest) = strip_sql_keyword(after_create, "FULLTEXT") {
            ("FULLTEXT KEY", rest)
        } else if let Some(rest) = strip_sql_keyword(after_create, "SPATIAL") {
            ("SPATIAL KEY", rest)
        } else {
            ("KEY", after_create)
        };
        let after_index = strip_sql_keyword(after_kind, "INDEX").ok_or_else(|| {
            AppError::new("schema_incomplete", "仅支持可比较的 CREATE INDEX 结构语句")
        })?;
        let (name, n) = ddl_name(after_index)
            .ok_or_else(|| AppError::new("schema_incomplete", "CREATE INDEX 缺少索引名称"))?;
        let after_on = strip_sql_keyword(&after_index[n..], "ON")
            .ok_or_else(|| AppError::new("schema_incomplete", "CREATE INDEX 缺少 ON"))?;
        let definition = strip_qualified_name(after_on)?;
        if !definition.starts_with('(') {
            return Err(AppError::new(
                "schema_incomplete",
                "CREATE INDEX 不支持的索引定义",
            ));
        }
        let part = format!("{kind} {} {definition}", quote_ident(&name)?);
        if parts
            .iter()
            .any(|p| part_key(p) == Some(("index".into(), name.clone())))
        {
            return Err(AppError::conflict(format!("索引 {name} 在输入中重复创建")));
        }
        parts.push(part);
    } else if let Some(after_alter) =
        strip_sql_keyword(text, "ALTER").and_then(|s| strip_sql_keyword(s, "TABLE"))
    {
        let clauses = strip_qualified_name(after_alter)?;
        // 复用顶层括号解析，确保函数、类型参数和字符串中的逗号不拆成步骤。
        let wrapper = format!("CREATE TABLE `_` ({clauses}) ENGINE=InnoDB");
        let (clauses, _) = ddl_parts(&wrapper)?;
        for clause in clauses {
            if let Some(add) = strip_sql_keyword(&clause, "ADD") {
                let add = strip_sql_keyword(add, "COLUMN").unwrap_or(add);
                let definition = canonical_restore_definition(add)?;
                let key = part_key(&definition).ok_or_else(|| {
                    AppError::new("schema_incomplete", format!("无法识别 ADD 定义: {clause}"))
                })?;
                if keyword_position(&definition, "AFTER").is_some()
                    || keyword_position(&definition, "FIRST").is_some()
                {
                    return Err(AppError::new(
                        "schema_incomplete",
                        format!("列顺序调整需单独解析: {clause}"),
                    ));
                }
                if parts.iter().any(|p| part_key(p) == Some(key.clone())) {
                    return Err(AppError::conflict(format!("输入重复添加结构项 {}", key.1)));
                }
                parts.push(definition);
            } else if let Some(change) = strip_sql_keyword(&clause, "MODIFY") {
                let change = strip_sql_keyword(change, "COLUMN").unwrap_or(change);
                let definition = canonical_restore_definition(change)?;
                let key = part_key(&definition)
                    .filter(|k| k.0 == "column")
                    .ok_or_else(|| {
                        AppError::new("schema_incomplete", format!("无法识别 MODIFY 列: {clause}"))
                    })?;
                if keyword_position(&definition, "AFTER").is_some()
                    || keyword_position(&definition, "FIRST").is_some()
                {
                    return Err(AppError::new(
                        "schema_incomplete",
                        format!("列顺序调整需单独解析: {clause}"),
                    ));
                }
                let slot = parts
                    .iter_mut()
                    .find(|p| part_key(p) == Some(key.clone()))
                    .ok_or_else(|| {
                        AppError::new(
                            "schema_incomplete",
                            format!("输入修改的列 {} 不存在", key.1),
                        )
                    })?;
                *slot = definition;
            } else if let Some(drop) = strip_sql_keyword(&clause, "DROP") {
                let (kind, rest) = if let Some(rest) = strip_sql_keyword(drop, "COLUMN") {
                    ("column", rest)
                } else if let Some(rest) =
                    strip_sql_keyword(drop, "INDEX").or_else(|| strip_sql_keyword(drop, "KEY"))
                {
                    ("index", rest)
                } else if let Some(rest) =
                    strip_sql_keyword(drop, "FOREIGN").and_then(|s| strip_sql_keyword(s, "KEY"))
                {
                    ("constraint", rest)
                } else if let Some(rest) =
                    strip_sql_keyword(drop, "PRIMARY").and_then(|s| strip_sql_keyword(s, "KEY"))
                {
                    ("primary", rest)
                } else {
                    ("column", drop)
                };
                let name = if kind == "primary" {
                    if !rest.is_empty() {
                        return Err(AppError::new(
                            "schema_incomplete",
                            format!("不支持的 DROP PRIMARY KEY 属性: {rest}"),
                        ));
                    }
                    "PRIMARY".to_string()
                } else {
                    let (name, n) = ddl_name(rest).ok_or_else(|| {
                        AppError::new("schema_incomplete", format!("DROP 名称无法解析: {clause}"))
                    })?;
                    if !rest[n..].trim().is_empty() {
                        return Err(AppError::new(
                            "schema_incomplete",
                            format!("不支持的 DROP 属性: {clause}"),
                        ));
                    }
                    name
                };
                if kind == "column"
                    && parts.iter().any(|p| {
                        part_key(p).is_none_or(|k| k != ("column".into(), name.clone()))
                            && p.contains(&quote_ident(&name).unwrap_or_default())
                    })
                {
                    return Err(AppError::new(
                        "schema_incomplete",
                        format!("删除列 {name} 会影响其他列、索引或约束，需显式处理依赖"),
                    ));
                }
                let index = parts
                    .iter()
                    .position(|p| part_key(p) == Some((kind.into(), name.clone())))
                    .ok_or_else(|| {
                        AppError::new(
                            "schema_incomplete",
                            format!("输入删除的结构项 {name} 不存在"),
                        )
                    })?;
                parts.remove(index);
            } else {
                return Err(AppError::new(
                    "schema_incomplete",
                    format!("不支持的 ALTER TABLE 子句: {clause}"),
                ));
            }
        }
    } else {
        return Err(AppError::new(
            "schema_incomplete",
            "结构语句不是 CREATE INDEX 或 ALTER TABLE",
        ));
    }
    Ok(format!(
        "{}(\n  {}\n) {options}",
        &source.schema.ddl[..table_start],
        parts.join(",\n  ")
    ))
}

impl MysqlSession {
    /// 将预检确认归属同一源表的结构语句合并到输入基线；不会执行输入 SQL。
    pub fn apply_restore_structure(
        &self,
        source: &MigrationTable,
        sql: &str,
    ) -> Result<MigrationTable> {
        let ddl = restore_schema_sql(source, sql)?;
        let mut result =
            self.from_restore_ddl(source.schema.target.clone(), &ddl, &source.sql_mode)?;
        result.version = source.version.clone();
        result.time_zone = source.time_zone.clone();
        Ok(result)
    }
}
impl MysqlSession {
    /// 将恢复文件中的 CREATE TABLE 转为同一迁移结构模型；不会连接或写入目标。
    pub fn from_restore_ddl(
        &self,
        target: ResourceTarget,
        ddl: &str,
        sql_mode: &str,
    ) -> Result<MigrationTable> {
        use crate::model::schema::{ColumnDefault, IndexKind, SchemaDefinition};
        let snapshot = offline::parse_ddl(target.clone(), ddl, sql_mode)?;
        let SchemaDefinition::Table(table) = &snapshot.definition else {
            return Err(AppError::new("schema_incomplete", "恢复结构不是表"));
        };
        let mut columns: Vec<Column> = table
            .columns
            .iter()
            .map(|column| Column {
                name: column.name.clone(),
                type_name: column.native_type.clone(),
                nullable: column.nullable,
                generated: column.generated.is_some(),
                auto_increment: column.auto_increment,
                primary_key: false,
                default_value: match &column.default {
                    ColumnDefault::Unspecified => None,
                    ColumnDefault::Null => Some("NULL".into()),
                    ColumnDefault::Literal(value) | ColumnDefault::Expression(value) => {
                        Some(value.clone())
                    }
                },
                comment: (!column.comment.is_empty()).then(|| column.comment.clone()),
            })
            .collect();
        let mut keys = Vec::new();
        for index in &table.indexes {
            if !matches!(&index.kind, IndexKind::Primary | IndexKind::Unique) {
                continue;
            }
            let names: Option<Vec<String>> = index
                .parts
                .iter()
                .map(|part| {
                    if part.expression.is_none() && part.prefix_length.is_none() {
                        part.column.clone()
                    } else {
                        None
                    }
                })
                .collect();
            if let Some(names) = names
                && !names.is_empty()
                && names.iter().all(|name| {
                    columns
                        .iter()
                        .any(|column| &column.name == name && !column.nullable)
                })
            {
                if index.kind == IndexKind::Primary {
                    for column in &mut columns {
                        if names.contains(&column.name) {
                            column.primary_key = true;
                        }
                    }
                }
                keys.push(names);
            }
        }
        let engine = table.engine.clone();
        let fingerprint = hash(ddl, sql_mode);
        Ok(MigrationTable {
            schema: TableSchema {
                target,
                columns,
                keys,
                ddl: ddl.to_string(),
                engine,
                details: json!({}),
            },
            version: "unknown".into(),
            sql_mode: sql_mode.into(),
            fingerprint,
            collations: Default::default(),
            time_zone: "+00:00".into(),
            snapshot: Some(snapshot),
        })
    }
}
impl MysqlSession {
    /// 根据文件显式列意图预览既有表调整，不更改数据库或未映射对象。
    pub async fn prepare_input_schema(
        &self,
        current: &MigrationTable,
        columns: &[Column],
        mapping: &std::collections::BTreeMap<String, String>,
        selected: &[String],
    ) -> Result<Vec<TransferPlanItem>> {
        input_schema_items(current, columns, mapping, selected)
    }
    /// 使用同一类型化结构规划器处理离线恢复及跨连接映射。
    pub async fn prepare_schema(
        &self,
        source: &MigrationTable,
        target: Option<&MigrationTable>,
        object: &TransferObject,
        all_objects: &[TransferObject],
    ) -> Result<Vec<TransferPlanItem>> {
        let mut conn = self.new_connection().await?;
        let row = sqlx::query("SELECT VERSION(), @@SESSION.sql_mode")
            .fetch_one(&mut conn)
            .await?;
        let version: String = row.try_get(0)?;
        let mode: String = row.try_get(1)?;
        ensure_version(&version)?;
        let source = source
            .snapshot
            .as_ref()
            .ok_or_else(|| AppError::new("schema_incomplete", "源结构快照不可用"))?;
        let target = match target {
            Some(table) => Some(
                table
                    .snapshot
                    .as_ref()
                    .ok_or_else(|| AppError::new("schema_incomplete", "目标结构快照不可用"))?,
            ),
            None => None,
        };
        let items =
            super::schema::plan_transfer(source, target, object, all_objects, version, mode)?;
        if let Some(item) = items.iter().find(|item| item.action == "unknown_attribute") {
            return Err(AppError::new("schema_incomplete", item.notes.join("; ")));
        }
        Ok(items)
    }
    /// 在发送前复核目标指纹并读回，调用者将返回指纹用于下一步。
    pub async fn apply_schema(
        &self,
        target: &ResourceTarget,
        item: &TransferPlanItem,
        expected: Option<&str>,
        sink: &ExecutionSink,
    ) -> Result<String> {
        self.profile.ensure_write()?;
        if !item.selected {
            return Err(AppError::invalid("结构步骤尚未选择"));
        }
        let planned = item
            .target
            .as_ref()
            .ok_or_else(|| AppError::invalid("结构步骤缺少目标"))?;
        if planned.database != target.database || planned.name != target.name {
            return Err(AppError::invalid("结构步骤与目标不一致"));
        }
        let sql = item
            .sql
            .as_deref()
            .ok_or_else(|| AppError::new("schema_incomplete", "该结构项没有可安全执行的 DDL"))?;
        let mut conn = self.new_connection().await?;
        match expected {
            Some(expected) if expected.starts_with("snapshot:") => {
                let current = super::schema::describe_on(&mut conn, target.clone())
                    .await?
                    .ok_or_else(|| AppError::conflict("目标结构已消失"))?;
                if current.fingerprint != expected["snapshot:".len()..] {
                    return Err(AppError::conflict("目标结构已变化"));
                }
            }
            Some(expected) => {
                if fingerprint(&mut conn, target).await? != expected {
                    return Err(AppError::conflict("目标结构已变化"));
                }
            }
            None => {
                let db = target
                    .database
                    .as_deref()
                    .ok_or_else(|| AppError::invalid("必须指定目标库"))?;
                let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.TABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?").bind(db).bind(&target.name).fetch_one(&mut conn).await?;
                if count != 0 {
                    return Err(AppError::conflict("目标表已存在"));
                }
            }
        }
        sink.mark_sent(true).await?;
        if let Err(e) = sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&mut conn)
            .await
        {
            return Err(if e.as_database_error().is_some() {
                e.into()
            } else {
                AppError::unknown(format!("DDL 结果无法确认: {e}"))
            });
        }
        let warnings = sqlx::raw_sql("SHOW COUNT(*) WARNINGS")
            .fetch_one(&mut conn)
            .await
            .map_err(|e| AppError::unknown(format!("DDL 已发送，无法读取警告: {e}")))?;
        let count = warnings
            .try_get::<u64, _>(0)
            .or_else(|_| warnings.try_get::<i64, _>(0).map(|v| v.max(0) as u64))
            .map_err(|e| AppError::unknown(format!("DDL 已发送，无法解码警告数: {e}")))?;
        let typed = expected.is_some_and(|value| value.starts_with("snapshot:"))
            || (expected.is_none() && item.source.is_some() && item.action == "create_table");
        let current = if typed {
            let snapshot = super::schema::describe_on(&mut conn, target.clone())
                .await
                .map_err(|e| AppError::unknown(format!("DDL 已发送，读回失败: {e}")))?
                .ok_or_else(|| AppError::unknown("DDL 已发送，但目标表不存在"))?;
            format!("snapshot:{}", snapshot.fingerprint)
        } else {
            fingerprint(&mut conn, target)
                .await
                .map_err(|e| AppError::unknown(format!("DDL 已发送，读回失败: {e}")))?
        };
        if count != 0 {
            return Err(AppError::new(
                "schema_warning",
                "DDL 已执行但服务端返回警告，不能确认结构语义等价",
            )
            .with_details(json!({"applied":true,"warnings":count,"fingerprint":current})));
        }
        Ok(current)
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/mysql/migration.rs"]
mod tests;
