//! MySQL 复制的源快照、结构阶段和批次状态编排。
use super::*;
use crate::{
    model::{ResourceTarget, Value},
    providers::mysql::{MysqlSession, migration::MigrationTable},
};

fn target_key(target: &ResourceTarget) -> (Option<String>, String) {
    (target.database.clone(), target.name.clone())
}

pub(super) async fn apply_schema_phase(
    prepared: &PreparedTransfer,
    target: &mut MysqlSession,
    phase: &str,
    journal: &mut TransferJournal,
    fingerprints: &mut BTreeMap<(Option<String>, String), String>,
    cancel: &CancellationToken,
) -> Result<()> {
    for item in prepared
        .items
        .iter()
        .filter(|item| item.selected && item.phase == phase)
    {
        crate::model::ensure_not_cancelled(cancel)?;
        let resource = item
            .target
            .as_ref()
            .ok_or_else(|| AppError::invalid("结构步骤缺少目标"))?;
        let key = target_key(resource);
        let expected = fingerprints.get(&key).map(String::as_str).or_else(|| {
            item.precondition
                .as_deref()
                .filter(|value| *value != "absent")
        });
        let mut ticket = journal
            .begin(
                "schema",
                TransferLocator {
                    object: Some(format!(
                        "{}.{}",
                        resource.database.as_deref().unwrap_or(""),
                        resource.name
                    )),
                    ..Default::default()
                },
                digest(&(resource, &item.sql))?,
            )
            .await?;
        let result = target
            .apply_schema(resource, item, expected, &ticket.sink)
            .await;
        match result {
            Ok(current) => {
                fingerprints.insert(key, current);
                journal
                    .finish(
                        ticket,
                        TransferStepState::Applied,
                        1,
                        Vec::new(),
                        None,
                        None,
                    )
                    .await?;
            }
            Err(error) => {
                let state = if error.code == "outcome_unknown" {
                    TransferStepState::OutcomeUnknown
                } else if error
                    .details
                    .get("applied")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                {
                    TransferStepState::Applied
                } else {
                    TransferStepState::Failed
                };
                // 发送前的本地校验失败也需要一个确定的失败记录。
                if state == TransferStepState::Failed {
                    ticket.outcome.error_code = Some(error.code.clone());
                }
                journal
                    .finish(ticket, state, 0, Vec::new(), Some(&error), None)
                    .await?;
                return Err(error);
            }
        }
        journal.checkpoint().await?;
    }
    Ok(())
}

pub(super) struct RowBatch {
    pub rows: Vec<Vec<Value>>,
    pub locator: TransferLocator,
}

/// 批次 journal 的每项状态对应输入顺序，不持久保存任何值。
pub(super) async fn write_rows(
    target: &MysqlSession,
    table: &MigrationTable,
    object: &TransferObject,
    policy: ConflictPolicy,
    batch: RowBatch,
    journal: &mut TransferJournal,
    cancel: &CancellationToken,
) -> Result<()> {
    let RowBatch { rows, mut locator } = batch;
    crate::model::ensure_not_cancelled(cancel)?;
    if rows.is_empty() {
        return Ok(());
    }
    if !table.schema.engine.eq_ignore_ascii_case("innodb") && rows.len() > 1 {
        let first = locator.first_record;
        for (index, row) in rows.into_iter().enumerate() {
            let mut location = locator.clone();
            location.first_record = first.map(|record| record + index as u64);
            location.last_record = location.first_record;
            write_one_batch(target, table, object, policy, vec![row], location, journal).await?;
        }
        return Ok(());
    }
    if locator.object.is_none() {
        locator.object = Some(format!(
            "{}.{}",
            table.schema.target.database.as_deref().unwrap_or(""),
            table.schema.target.name
        ));
    }
    write_one_batch(target, table, object, policy, rows, locator, journal).await
}

async fn write_one_batch(
    target: &MysqlSession,
    table: &MigrationTable,
    object: &TransferObject,
    policy: ConflictPolicy,
    rows: Vec<Vec<Value>>,
    locator: TransferLocator,
    journal: &mut TransferJournal,
) -> Result<()> {
    let count = rows.len() as u64;
    let row_digest = digest(&rows)?;
    let first_record = locator.first_record;
    let ticket = journal.begin("data", locator, row_digest).await?;
    match target
        .write_batch(table, object, policy, rows, &ticket.sink)
        .await
    {
        Ok(outcome) => {
            let skipped = outcome
                .states
                .iter()
                .filter(|state| **state == TransferStepState::Skipped)
                .count() as u64;
            let applied = outcome
                .states
                .iter()
                .filter(|state| **state == TransferStepState::Applied)
                .count() as u64;
            journal.progress.written_rows += applied;
            journal.progress.skipped += skipped;
            let state = if applied == 0 && skipped == count {
                TransferStepState::Skipped
            } else {
                TransferStepState::Applied
            };
            journal
                .finish(ticket, state, outcome.affected, outcome.states, None, None)
                .await?;
        }
        Err(error) => {
            let states: Vec<TransferStepState> = error
                .details
                .get("states")
                .cloned()
                .map(serde_json::from_value)
                .transpose()?
                .unwrap_or_default();
            let applied = states
                .iter()
                .filter(|state| **state == TransferStepState::Applied)
                .count() as u64;
            let rolled_back = states
                .iter()
                .filter(|state| **state == TransferStepState::RolledBack)
                .count() as u64;
            let skipped = states
                .iter()
                .filter(|state| **state == TransferStepState::Skipped)
                .count() as u64;
            let unknown = states.contains(&TransferStepState::OutcomeUnknown)
                || error.code == "outcome_unknown";
            journal.progress.written_rows += applied;
            journal.progress.rolled_back += rolled_back;
            journal.progress.skipped += skipped;
            journal.progress.failed += 1;
            if unknown {
                journal.progress.outcome_unknown += 1;
            }
            let state = if unknown {
                TransferStepState::OutcomeUnknown
            } else if rolled_back > 0 {
                TransferStepState::RolledBack
            } else {
                TransferStepState::Failed
            };
            let error_record = error
                .details
                .get("index")
                .or_else(|| error.details.get("failed_index"))
                .and_then(serde_json::Value::as_u64)
                .and_then(|index| first_record.map(|first| first + index));
            journal
                .finish(ticket, state, applied, states, Some(&error), error_record)
                .await?;
            return Err(error);
        }
    }
    journal.checkpoint().await
}

pub(super) async fn copy(
    prepared: &PreparedTransfer,
    source: &mut MysqlSession,
    target: &mut MysqlSession,
    journal: &mut TransferJournal,
    cancel: &CancellationToken,
) -> Result<()> {
    for object in &prepared.spec.objects {
        if source
            .same_table_as(target, &object.source, &object.target)
            .await?
        {
            return Err(AppError::invalid("源目标实际为同一张表"));
        }
    }
    for (index, object) in prepared.spec.objects.iter().enumerate() {
        let expected = prepared
            .source_schema_fingerprints
            .get(&index)
            .ok_or_else(|| AppError::conflict("源结构指纹缺失，请重新准备"))?;
        let snapshot: crate::model::schema::SchemaSnapshot = serde_json::from_value(
            source
                .schema_describe(serde_json::json!({"target": object.source}), cancel)
                .await?,
        )?;
        if &snapshot.fingerprint != expected {
            return Err(AppError::conflict("源结构已变化，请重新准备"));
        }
    }
    let mut fingerprints = BTreeMap::new();
    if prepared.spec.options.include_schema {
        apply_schema_phase(
            prepared,
            target,
            "schema",
            journal,
            &mut fingerprints,
            cancel,
        )
        .await?;
    }
    if !prepared.spec.options.include_data {
        if prepared.spec.options.include_schema {
            apply_schema_phase(
                prepared,
                target,
                "after_data",
                journal,
                &mut fingerprints,
                cancel,
            )
            .await?;
        }
        return Ok(());
    }
    let mut source_tables = Vec::with_capacity(prepared.spec.objects.len());
    let mut targets = Vec::with_capacity(prepared.spec.objects.len());
    for (index, object) in prepared.spec.objects.iter().enumerate() {
        let source_table = source
            .describe_migration(object.source.clone(), cancel)
            .await?;
        let source_fingerprint = source_table
            .snapshot
            .as_ref()
            .map(|snapshot| &snapshot.fingerprint)
            .ok_or_else(|| AppError::conflict("源结构快照不可用"))?;
        if prepared.source_schema_fingerprints.get(&index) != Some(source_fingerprint) {
            return Err(AppError::conflict("源结构已变化，请重新准备"));
        }
        let target_table = target
            .describe_migration(object.target.clone(), cancel)
            .await?;
        MysqlSession::validate_migration_mapping(&source_table, &target_table, object)?;
        fingerprints.insert(target_key(&object.target), target_table.fingerprint.clone());
        source_tables.push((source_table, object.clone()));
        targets.push(target_table);
    }
    if prepared.spec.options.include_data {
        let child_cancel = cancel.child_token();
        let mut stream = source
            .open_source_stream(
                source_tables,
                prepared.spec.options.consistency,
                child_cancel.clone(),
            )
            .await?;
        let mut records = vec![0u64; targets.len()];
        let result = async {
            while let Some(batch) = tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), item = stream.recv() => item } {
                let batch = batch?;
                let table = targets.get(batch.object).ok_or_else(|| AppError::new("internal", "源批次对象序号无效"))?;
                let object = &prepared.spec.objects[batch.object];
                let first = records[batch.object] + 1;
                records[batch.object] += batch.rows.len() as u64;
                journal.progress.read_rows += batch.rows.len() as u64;
                journal.progress.processed_bytes = journal.progress.processed_bytes.saturating_add(batch.bytes as u64);
                journal.progress.current_object = Some(object.source.name.clone());
                journal.progress.phase = "data".into();
                write_rows(target, table, object, prepared.spec.options.conflict, RowBatch { rows: batch.rows, locator: TransferLocator { object: Some(object.target.name.clone()), first_record: Some(first), last_record: Some(records[batch.object]), ..Default::default() } }, journal, cancel).await?;
            }
            Ok(())
        }.await;
        child_cancel.cancel();
        drop(stream);
        result?;
    }
    if prepared.spec.options.include_schema {
        apply_schema_phase(
            prepared,
            target,
            "after_data",
            journal,
            &mut fingerprints,
            cancel,
        )
        .await?;
    }
    Ok(())
}
