//! MySQL 逻辑备份：准备时读取所选定义，执行时从单个只读快照流式读取数据。
mod restore;
use std::{collections::HashSet, io::Write, path::Path};

use sha2::{Digest, Sha256};
use sqlx::{Row, mysql::MySqlRow};
use tokio_util::sync::CancellationToken;

use super::{MysqlSession, migration::MigrationTable, quote_ident};
use crate::{
    execution::{
        data_files::AtomicOutput,
        sql_restore::{backup_footer, backup_header, encode_literal},
    },
    model::{
        AppError, ResourceTarget, Result, ensure_not_cancelled,
        transfer::{ReadConsistency, TransferObject},
    },
};

/// 单个已读取的对象定义及其创建时上下文。
#[derive(Debug, Clone)]
pub struct DumpObject {
    pub object: TransferObject,
    pub ddl: String,
    pub sql_mode: String,
    pub time_zone: Option<String>,
    pub character_set_client: String,
    pub collation_connection: String,
    pub database_collation: Option<String>,
    pub database_charset: Option<String>,
    pub default_collation: String,
    pub default_charset: String,
    pub table: Option<MigrationTable>,
}

/// 已确认的源对象范围。表数据只在执行阶段读取，不进入结果缓存。
#[derive(Debug, Clone)]
pub struct DumpPlan {
    pub objects: Vec<DumpObject>,
    pub include_schema: bool,
    pub include_data: bool,
    pub consistency: ReadConsistency,
    pub server_version: String,
    pub complete_database: bool,
}

fn token(s: &str) -> Result<&str> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(AppError::invalid("服务端返回了无效字符集或排序规则"));
    }
    Ok(s)
}
fn object_fingerprint(object: &DumpObject) -> Result<String> {
    let bytes = serde_json::to_vec(&(
        &object.ddl,
        &object.sql_mode,
        &object.time_zone,
        &object.character_set_client,
        &object.collation_connection,
        &object.database_collation,
        &object.database_charset,
        &object.default_charset,
        &object.default_collation,
    ))?;
    Ok(format!(
        "{:x}",
        base16ct::HexDisplay(&Sha256::digest(bytes))
    ))
}
fn hex(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        let _ = write!(out, "{b:02x}");
    }
    out
}
fn string_at(row: &MySqlRow, index: usize, what: &str) -> Result<String> {
    row.try_get::<Option<String>, _>(index)?.ok_or_else(|| {
        AppError::new(
            "schema_incomplete",
            format!("无法读取{what}，请检查对象定义权限"),
        )
    })
}
fn show_command(target: &ResourceTarget) -> Result<(String, usize)> {
    let kind = match target.kind.as_str() {
        "table" => ("TABLE", 1),
        "view" => ("VIEW", 1),
        "trigger" => ("TRIGGER", 2),
        "procedure" => ("PROCEDURE", 2),
        "function" => ("FUNCTION", 2),
        "event" => ("EVENT", 3),
        _ => {
            return Err(AppError::invalid(format!(
                "不支持备份对象类型 {}",
                target.kind
            )));
        }
    };
    let db = target
        .database
        .as_deref()
        .ok_or_else(|| AppError::invalid("备份对象缺少数据库"))?;
    Ok((
        format!(
            "SHOW CREATE {} {}.{}",
            kind.0,
            quote_ident(db)?,
            quote_ident(&target.name)?
        ),
        kind.1,
    ))
}

impl MysqlSession {
    /// 读取显式选择的每个对象和创建上下文；不可见或无权读取的对象使准备失败。
    pub async fn prepare_sql_dump(
        &mut self,
        objects: Vec<TransferObject>,
        include_schema: bool,
        include_data: bool,
        consistency: ReadConsistency,
        cancel: &CancellationToken,
    ) -> Result<DumpPlan> {
        if !include_schema && !include_data {
            return Err(AppError::invalid("备份必须选择结构或数据"));
        }
        let mut conn = self.new_connection().await?;
        let version: String = sqlx::query_scalar("SELECT VERSION()")
            .fetch_one(&mut conn)
            .await?;
        let mut seen = HashSet::new();
        let mut result = Vec::with_capacity(objects.len());
        for object in objects {
            ensure_not_cancelled(cancel)?;
            let target = &object.source;
            let db = target
                .database
                .as_deref()
                .ok_or_else(|| AppError::invalid("备份对象缺少数据库"))?;
            if self
                .profile
                .direct()?
                .visible_databases
                .as_ref()
                .is_some_and(|allowed| !allowed.iter().any(|name| name == db))
            {
                return Err(AppError::new(
                    "permission_denied",
                    format!("源数据库 {db} 不在连接可见范围内"),
                ));
            }
            if !seen.insert((db.to_owned(), target.kind.clone(), target.name.clone())) {
                return Err(AppError::invalid("备份对象重复"));
            }
            if target.kind != "table" && !include_schema {
                return Err(AppError::invalid("非表对象没有可单独导出的数据"));
            }
            let (command, ddl_index) = show_command(target)?;
            let row = sqlx::query(sqlx::AssertSqlSafe(command))
                .fetch_one(&mut conn)
                .await?;
            let ddl = string_at(&row, ddl_index, "对象定义")?;
            let defaults = sqlx::query("SELECT DEFAULT_CHARACTER_SET_NAME, DEFAULT_COLLATION_NAME FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?").bind(db).fetch_optional(&mut conn).await?.ok_or_else(|| AppError::new("schema_incomplete", format!("数据库 {db} 不存在或不可见")))?;
            let default_charset: String = defaults.try_get(0)?;
            let default_collation: String = defaults.try_get(1)?;
            let session = sqlx::query("SELECT @@SESSION.sql_mode, @@SESSION.character_set_client, @@SESSION.collation_connection").fetch_one(&mut conn).await?;
            let (sql_mode, charset, collation, db_collation) = match target.kind.as_str() {
                "trigger" | "procedure" | "function" => (
                    string_at(&row, 1, "SQL mode")?,
                    string_at(&row, 3, "客户端字符集")?,
                    string_at(&row, 4, "连接排序规则")?,
                    Some(string_at(&row, 5, "数据库排序规则")?),
                ),
                "event" => (
                    string_at(&row, 1, "SQL mode")?,
                    string_at(&row, 4, "客户端字符集")?,
                    string_at(&row, 5, "连接排序规则")?,
                    Some(string_at(&row, 6, "数据库排序规则")?),
                ),
                "view" => (
                    session.try_get(0)?,
                    string_at(&row, 2, "客户端字符集")?,
                    string_at(&row, 3, "连接排序规则")?,
                    None,
                ),
                _ => (
                    session.try_get(0)?,
                    session.try_get(1)?,
                    session.try_get(2)?,
                    None,
                ),
            };
            let time_zone = if target.kind == "event" {
                Some(string_at(&row, 2, "事件创建时区")?)
            } else {
                None
            };
            let db_charset = if let Some(collation) = &db_collation {
                Some(sqlx::query_scalar::<_, String>("SELECT CHARACTER_SET_NAME FROM information_schema.COLLATIONS WHERE COLLATION_NAME = ?").bind(collation).fetch_one(&mut conn).await?)
            } else {
                None
            };
            token(&charset)?;
            token(&collation)?;
            token(&default_charset)?;
            token(&default_collation)?;
            if let Some(s) = &db_charset {
                token(s)?;
            }
            if let Some(s) = &db_collation {
                token(s)?;
            }
            let table = if target.kind == "table" {
                let table = self.describe_migration(target.clone(), cancel).await?;
                if table.schema.ddl != ddl {
                    return Err(AppError::conflict(format!(
                        "准备期间表 {} 的定义发生变化",
                        target.name
                    )));
                }
                if consistency == ReadConsistency::Transaction
                    && include_data
                    && !table.schema.engine.eq_ignore_ascii_case("innodb")
                {
                    return Err(AppError::new(
                        "inconsistent_engine",
                        format!("表 {} 非 InnoDB，需要明确选择尽力读取", target.name),
                    ));
                }
                Some(table)
            } else {
                None
            };
            result.push(DumpObject {
                object,
                ddl,
                sql_mode,
                time_zone,
                character_set_client: charset,
                collation_connection: collation,
                database_collation: db_collation,
                database_charset: db_charset,
                default_collation,
                default_charset,
                table,
            });
        }
        Ok(DumpPlan {
            objects: result,
            include_schema,
            include_data,
            consistency,
            server_version: version,
            complete_database: false,
        })
    }

    /// 在所有定义及数据读取成功后 flush 并以不覆盖方式发布；失败保留 partial 文件。
    pub async fn execute_sql_dump(
        &self,
        plan: &DumpPlan,
        destination: &Path,
        journal: &mut crate::execution::migration::TransferJournal,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let mut output = AtomicOutput::create(destination)?;
        let partial_path = output.partial_path().to_owned();
        let initial = output.snapshot(true)?;
        let identity = |path: &Path,
                        snapshot: &crate::execution::data_files::OutputSnapshot|
         -> Result<serde_json::Value> {
            let file_id = snapshot
                .file_id
                .as_ref()
                .ok_or_else(|| AppError::new("recovery_conflict", "平台未提供临时文件身份"))?;
            let sha256 = snapshot
                .sha256
                .as_ref()
                .ok_or_else(|| AppError::new("recovery_conflict", "临时文件缺少摘要"))?;
            Ok(
                serde_json::json!({"path":path,"canonical_path":snapshot.canonical_path,"file_id":file_id,"size":snapshot.size,"sha256":sha256}),
            )
        };
        let artifact_sink = journal
            .sink
            .clone()
            .with_transfer_step(journal.run_id, journal.next_ordinal());
        let artifact = artifact_sink
            .register_transfer_artifact("export_partial", identity(&partial_path, &initial)?)
            .await?;
        let mut uncertain = false;
        let writing: Result<()> = async {
            let mut hash = Sha256::new();
            fn put(output: &mut AtomicOutput, hash: &mut Sha256, s: &str) -> Result<()> {
                output.write_all(s.as_bytes())?;
                hash.update(s.as_bytes());
                Ok(())
            }
            put(&mut output, &mut hash, backup_header())?;
            put(
                &mut output,
                &mut hash,
                &format!("-- datapier-objects count={}\n", plan.objects.len()),
            )?;
            let partial = plan
                .objects
                .iter()
                .any(|o| !o.object.filters.is_empty() || !o.object.columns.is_empty());
            put(
                &mut output,
                &mut hash,
                &format!(
                    "-- source-version={} scope={} snapshot={} partial={}\n",
                    hex(&plan.server_version),
                    if plan.complete_database { "database" } else { "selected_objects" },
                    if plan.consistency == ReadConsistency::Transaction {
                        "innodb-repeatable-read"
                    } else {
                        "best-effort"
                    },
                    u8::from(partial)
                ),
            )?;
            for o in &plan.objects {
                let db = o
                    .object
                    .source
                    .database
                    .as_deref()
                    .ok_or_else(|| AppError::invalid("备份对象缺少数据库"))?;
                put(
                    &mut output,
                    &mut hash,
                    &format!(
                        "-- datapier-object kind={} database={} name={} schema={} data={}\n",
                        o.object.source.kind,
                        hex(db),
                        hex(&o.object.source.name),
                        u8::from(plan.include_schema),
                        u8::from(plan.include_data && o.table.is_some())
                    ),
                )?;
                let selection = serde_json::to_string(&(&o.object.filters, &o.object.columns))?;
                let encoded = hex(&selection);
                for (part, chunk) in encoded.as_bytes().chunks(2048).enumerate() {
                    let chunk = std::str::from_utf8(chunk).map_err(|_| AppError::invalid("备份选择编码无效"))?;
                    put(&mut output, &mut hash, &format!("-- datapier-selection database={} name={} part={} json={}\n", hex(db), hex(&o.object.source.name), part, chunk))?;
                }
            }
            if plan.include_schema {
                for o in &plan.objects {
                    ensure_not_cancelled(cancel)?;
                    let db = o
                        .object
                        .source
                        .database
                        .as_deref()
                        .ok_or_else(|| AppError::invalid("备份对象缺少数据库"))?;
                    put(
                        &mut output,
                        &mut hash,
                        &format!(
                            "USE {};\nSET NAMES {} COLLATE {};\nSET SQL_MODE='{}';\n",
                            quote_ident(db)?,
                            o.character_set_client,
                            o.collation_connection,
                            o.sql_mode.replace('\'', "''")
                        ),
                    )?;
                    if let Some(zone) = &o.time_zone {
                        put(&mut output, &mut hash, &format!("SET TIME_ZONE='{}';\n", zone.replace('\'', "''")))?;
                    }
                    let changed = o
                        .database_collation
                        .as_deref()
                        .is_some_and(|c| c != o.default_collation);
                    if changed {
                        put(
                            &mut output,
                            &mut hash,
                            &format!(
                                "ALTER DATABASE {} CHARACTER SET {} COLLATE {};\n",
                                quote_ident(db)?,
                                o.database_charset.as_deref().ok_or_else(|| AppError::new(
                                    "schema_incomplete",
                                    "缺少数据库字符集"
                                ))?,
                                o.database_collation.as_deref().unwrap_or_default()
                            ),
                        )?;
                    }
                    // 控制在 MySQL 客户端可携带的 16 字节内，reader 与外部客户端使用同一分隔符。
                    let delimiter = format!(
                        "$DP{:012x}$",
                        uuid::Uuid::new_v4().as_u128() & 0xffff_ffff_ffff
                    );
                    if o.ddl.contains(&delimiter) {
                        return Err(AppError::invalid("定义包含备份分隔符"));
                    }
                    put(
                        &mut output,
                        &mut hash,
                        &format!("DELIMITER {delimiter}\n{}{delimiter}\nDELIMITER ;\n", o.ddl),
                    )?;
                    if changed {
                        put(
                            &mut output,
                            &mut hash,
                            &format!(
                                "ALTER DATABASE {} CHARACTER SET {} COLLATE {};\n",
                                quote_ident(db)?,
                                o.default_charset,
                                o.default_collation
                            ),
                        )?;
                    }
                    journal.progress.current_object = Some(o.object.source.name.clone());
                    journal.progress.phase = "schema".into();
                    journal.progress.processed_bytes = output.bytes_written();
                    journal.checkpoint().await?;
                }
            }
            if plan.include_data {
                // 数据读取连接固定为 UTC，导出字面量必须携带同一时区。
                put(&mut output, &mut hash, "SET TIME_ZONE='+00:00';\n")?;
                let tables: Vec<_> = plan
                    .objects
                    .iter()
                    .filter_map(|o| o.table.as_ref().map(|t| (t.clone(), o.object.clone())))
                    .collect();
                let mut stream = self
                    .open_source_stream(tables.clone(), plan.consistency, cancel.clone())
                    .await?;
                while let Some(batch) = tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), batch = stream.recv() => batch } {
                    ensure_not_cancelled(cancel)?;
                    let batch = batch?;
                    let (table, object) = tables
                        .get(batch.object)
                        .ok_or_else(|| AppError::new("internal", "源批次对象序号无效"))?;
                    let columns: Vec<&str> = if object.columns.is_empty() {
                        table
                            .schema
                            .columns
                            .iter()
                            .filter(|c| !c.generated)
                            .map(|c| c.name.as_str())
                            .collect()
                    } else {
                        object.columns.iter().map(|c| c.source.as_str()).collect()
                    };
                    let db = object
                        .source
                        .database
                        .as_deref()
                        .ok_or_else(|| AppError::invalid("备份对象缺少数据库"))?;
                    let names = columns
                        .iter()
                        .map(|s| quote_ident(s))
                        .collect::<Result<Vec<_>>>()?
                        .join(", ");
                    let mut statement = format!(
                        "INSERT INTO {}.{} ({names}) VALUES\n",
                        quote_ident(db)?,
                        quote_ident(&object.source.name)?
                    );
                    for (i, row) in batch.rows.iter().enumerate() {
                        if row.len() != columns.len() {
                            return Err(AppError::new("schema_incomplete", "源数据列数与定义不符"));
                        }
                        if i != 0 {
                            statement.push_str(",\n");
                        }
                        statement.push('(');
                        statement.push_str(
                            &row.iter()
                                .map(encode_literal)
                                .collect::<Result<Vec<_>>>()?
                                .join(", "),
                        );
                        statement.push(')');
                    }
                    statement.push_str(";\n");
                    put(&mut output, &mut hash, &statement)?;
                    journal.progress.current_object = Some(object.source.name.clone());
                    journal.progress.phase = "data".into();
                    journal.progress.read_rows += batch.rows.len() as u64;
                    journal.progress.written_rows += batch.rows.len() as u64;
                    journal.progress.processed_bytes = output.bytes_written();
                    journal.checkpoint().await?;
                }
            }
            // 定义可在快照之外变化，发布前必须再次逐对象核对。
            let mut conn = self.new_connection().await?;
            for o in &plan.objects {
                ensure_not_cancelled(cancel)?;
                let (command, index) = show_command(&o.object.source)?;
                let row = sqlx::query(sqlx::AssertSqlSafe(command))
                    .fetch_one(&mut conn)
                    .await?;
                if string_at(&row, index, "对象定义")? != o.ddl
                    || (o.time_zone.is_some() && Some(string_at(&row, 2, "事件创建时区")?) != o.time_zone)
                {
                    return Err(AppError::conflict(format!(
                        "读取期间对象 {} 的定义发生变化",
                        o.object.source.name
                    )));
                }
            }
            output.write_all(backup_footer(&hash.finalize()).as_bytes())?;
            journal.progress.phase = "verifying".into();
            journal.progress.processed_bytes = output.bytes_written();
            journal.checkpoint().await?;
            ensure_not_cancelled(cancel)?;
            let snapshot = output.snapshot(true)?;
            if initial.file_id.is_none()
                || initial.file_id != snapshot.file_id
                || initial.canonical_path != snapshot.canonical_path
            {
                return Err(AppError::new("recovery_conflict", "备份临时文件身份已变化"));
            }
            artifact_sink
                .update_transfer_artifact(artifact, identity(&partial_path, &snapshot)?)
                .await?;
            let ticket = journal
                .begin(
                    "sql_dump_publish",
                    crate::model::transfer::TransferLocator::default(),
                    format!("{:x}", base16ct::HexDisplay(&Sha256::digest(destination.to_string_lossy().as_bytes()))),
                )
                .await?;
            if let Err(error) = ticket.sink.mark_sent(true).await {
                journal
                    .finish(
                        ticket,
                        crate::model::transfer::TransferStepState::NotSent,
                        0,
                        Vec::new(),
                        Some(&error),
                        None,
                    )
                    .await?;
                return Err(error);
            }
            uncertain = true;
            match output.publish() {
                Ok(_) => {
                    let published =
                        match crate::execution::data_files::output_snapshot(destination, true) {
                            Ok(published)
                                if published.file_id == snapshot.file_id
                                    && published.sha256 == snapshot.sha256
                                    && published.size == snapshot.size =>
                            {
                                published
                            }
                            _ => {
                                let error =
                                    AppError::unknown("备份已发布，但文件身份或摘要无法确认");
                                journal
                                    .sink
                                    .finish_transfer_artifact(artifact, "outcome_unknown")
                                    .await?;
                                journal
                                    .finish(
                                        ticket,
                                        crate::model::transfer::TransferStepState::OutcomeUnknown,
                                        0,
                                        Vec::new(),
                                        Some(&error),
                                        None,
                                    )
                                    .await?;
                                return Err(error);
                            }
                        };
                    journal
                        .finish(
                            ticket,
                            crate::model::transfer::TransferStepState::Applied,
                            1,
                            Vec::new(),
                            None,
                            None,
                        )
                        .await?;
                    artifact_sink
                        .update_transfer_artifact(artifact, identity(destination, &published)?)
                        .await?;
                    journal
                        .sink
                        .finish_transfer_artifact(artifact, "published")
                        .await?;
                    uncertain = false;
                }
                Err(error) => {
                    use crate::execution::data_files::{PublicationState, publication_state};
                    let publication = publication_state(&error);
                    uncertain = publication == PublicationState::Unknown;
                    let mut error = error;
                    if uncertain {
                        error.code = "outcome_unknown".into();
                        journal
                            .sink
                            .finish_transfer_artifact(artifact, "outcome_unknown")
                            .await?;
                    }
                    let state = match publication {
                        PublicationState::NotPublished => {
                            crate::model::transfer::TransferStepState::Failed
                        }
                        PublicationState::Published => {
                            crate::model::transfer::TransferStepState::Applied
                        }
                        PublicationState::Unknown => {
                            crate::model::transfer::TransferStepState::OutcomeUnknown
                        }
                    };
                    journal
                        .finish(
                            ticket,
                            state,
                            u64::from(publication == PublicationState::Published),
                            Vec::new(),
                            Some(&error),
                            None,
                        )
                        .await?;
                    return Err(error);
                }
            }
            Ok(())
        }
        .await;
        if writing.is_err() && !uncertain {
            let snapshot = crate::execution::data_files::output_snapshot(&partial_path, true)?;
            if initial.file_id != snapshot.file_id
                || initial.canonical_path != snapshot.canonical_path
            {
                return Err(AppError::new("recovery_conflict", "备份临时文件身份已变化"));
            }
            artifact_sink
                .update_transfer_artifact(artifact, identity(&partial_path, &snapshot)?)
                .await?;
            journal
                .sink
                .finish_transfer_artifact(artifact, "cleanup_pending")
                .await?;
        }
        writing
    }
}

impl MysqlSession {
    async fn dump_objects(
        &self,
        spec: &crate::model::transfer::TransferSpec,
    ) -> Result<Vec<TransferObject>> {
        if !spec.objects.is_empty() {
            return Ok(spec.objects.clone());
        }
        let db = spec
            .source
            .as_ref()
            .and_then(|source| source.database.as_deref())
            .ok_or_else(|| AppError::invalid("库级备份需要明确源数据库"))?;
        let mut conn = self.new_connection().await?;
        let visible: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME=?",
        )
        .bind(db)
        .fetch_one(&mut conn)
        .await?;
        if visible == 0 {
            return Err(AppError::new(
                "schema_incomplete",
                format!("数据库 {db} 不存在或不可见"),
            ));
        }
        // 库级发现依赖授权可见性；只能在授权足以覆盖各类目录和存储程序正文时称为完整库。
        let grants = sqlx::query("SHOW GRANTS FOR CURRENT_USER()")
            .fetch_all(&mut conn)
            .await?;
        let db_scope = format!("{}.*", quote_ident(db)?.to_ascii_uppercase());
        let mut privileges: HashSet<String> = HashSet::new();
        let mut routine_visibility = false;
        for grant in grants {
            let text: String = grant.try_get(0)?;
            let upper = text.to_ascii_uppercase();
            let Some((left, right)) = upper.split_once(" ON ") else {
                continue;
            };
            let Some((scope, _)) = right.split_once(" TO ") else {
                continue;
            };
            let global = scope.trim() == "*.*";
            if !global && scope.trim() != db_scope {
                continue;
            }
            let Some(list) = left.strip_prefix("GRANT ") else {
                continue;
            };
            for privilege in list.split(',').map(str::trim) {
                if privilege == "ALL PRIVILEGES" || privilege == "ALL" {
                    privileges.extend(
                        ["SELECT", "SHOW VIEW", "TRIGGER", "EVENT", "EXECUTE"]
                            .into_iter()
                            .map(str::to_owned),
                    );
                    if global {
                        routine_visibility = true;
                    }
                } else {
                    if ["SELECT", "SHOW VIEW", "TRIGGER", "EVENT", "EXECUTE"].contains(&privilege) {
                        privileges.insert(privilege.to_owned());
                    }
                    if global && (privilege == "SHOW_ROUTINE" || privilege == "SELECT") {
                        routine_visibility = true;
                    }
                }
            }
        }
        let enough = privileges.contains("SELECT")
            && (!spec.options.include_schema
                || (routine_visibility
                    && ["SHOW VIEW", "TRIGGER", "EVENT", "EXECUTE"]
                        .iter()
                        .all(|p| privileges.contains(*p))));
        if !enough {
            return Err(AppError::new(
                "metadata_incomplete",
                "无法证明库级对象和存储程序定义完整可见；请补充数据库范围读取权限及全局 SHOW_ROUTINE 或 SELECT，或明确选择对象",
            ));
        }
        let rows = sqlx::query("SELECT TABLE_NAME AS name, CASE TABLE_TYPE WHEN 'BASE TABLE' THEN 'table' ELSE 'view' END AS kind FROM information_schema.TABLES WHERE TABLE_SCHEMA=? UNION ALL SELECT TRIGGER_NAME, 'trigger' FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA=? UNION ALL SELECT ROUTINE_NAME, LOWER(ROUTINE_TYPE) FROM information_schema.ROUTINES WHERE ROUTINE_SCHEMA=? UNION ALL SELECT EVENT_NAME, 'event' FROM information_schema.EVENTS WHERE EVENT_SCHEMA=? ORDER BY kind, name")
            .bind(db).bind(db).bind(db).bind(db).fetch_all(&mut conn).await?;
        let mut objects = Vec::new();
        for row in rows {
            let kind: String = row.try_get("kind")?;
            if kind != "table" && !spec.options.include_schema {
                continue;
            }
            let source = ResourceTarget {
                database: Some(db.to_owned()),
                name: row.try_get("name")?,
                key_b64: None,
                kind,
            };
            objects.push(TransferObject {
                source: source.clone(),
                target: source,
                columns: Vec::new(),
                target_columns: Vec::new(),
                filters: Vec::new(),
                locator_key: Vec::new(),
                create_if_missing: false,
                sync_schema: false,
                selected_schema_items: Vec::new(),
            });
        }
        Ok(objects)
    }
    /// 准备备份并返回可展示的对象范围和定义指纹。
    pub async fn prepare_dump(
        &mut self,
        spec: &crate::model::transfer::TransferSpec,
        cancel: &CancellationToken,
    ) -> Result<Vec<crate::model::transfer::TransferPlanItem>> {
        let objects = self.dump_objects(spec).await?;
        let mut plan = self
            .prepare_sql_dump(
                objects,
                spec.options.include_schema,
                spec.options.include_data,
                spec.options.consistency,
                cancel,
            )
            .await?;
        plan.complete_database = spec.objects.is_empty();
        plan.objects
            .iter()
            .map(|o| {
                Ok(crate::model::transfer::TransferPlanItem {
                    id: format!(
                        "dump:{}:{}:{}",
                        o.object.source.kind,
                        o.object.source.database.as_deref().unwrap_or_default(),
                        o.object.source.name
                    ),
                    phase: "dump".into(),
                    action: "read_object".into(),
                    source: Some(o.object.source.clone()),
                    target: None,
                    sql: None,
                    destructive: false,
                    selected: true,
                    precondition: Some(object_fingerprint(o)?),
                    notes: vec![format!(
                        "范围：{}；版本 {}；数据快照 {:?}；过滤或列裁剪：{}",
                        if plan.complete_database {
                            "经权限核对的数据库对象"
                        } else {
                            "所选对象"
                        },
                        plan.server_version,
                        plan.consistency,
                        !o.object.filters.is_empty() || !o.object.columns.is_empty()
                    )],
                })
            })
            .collect()
    }

    /// 执行前重新读取定义并与已确认计划比较，再发布完整 SQL 文件。
    pub async fn dump_sql(
        &mut self,
        prepared: &crate::model::transfer::PreparedTransfer,
        journal: &mut crate::execution::migration::TransferJournal,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let spec = &prepared.spec;
        let destination = &spec
            .output
            .as_ref()
            .ok_or_else(|| AppError::invalid("SQL 备份缺少输出路径"))?
            .path;
        let objects = self.dump_objects(spec).await?;
        let mut plan = self
            .prepare_sql_dump(
                objects,
                spec.options.include_schema,
                spec.options.include_data,
                spec.options.consistency,
                cancel,
            )
            .await?;
        plan.complete_database = spec.objects.is_empty();
        if plan.objects.len() != prepared.items.len() {
            return Err(AppError::conflict("备份对象清单已变化，请重新准备"));
        }
        for object in &plan.objects {
            let id = format!(
                "dump:{}:{}:{}",
                object.object.source.kind,
                object.object.source.database.as_deref().unwrap_or_default(),
                object.object.source.name
            );
            let fingerprint = object_fingerprint(object)?;
            if !prepared.items.iter().any(|item| {
                item.id == id
                    && item.selected
                    && item.precondition.as_deref() == Some(fingerprint.as_str())
            }) {
                return Err(AppError::conflict(format!(
                    "备份对象 {} 已变化，请重新准备",
                    object.object.source.name
                )));
            }
        }
        self.execute_sql_dump(&plan, destination, journal, cancel)
            .await?;
        journal.progress.phase = "dump".into();
        journal.checkpoint().await
    }
}
