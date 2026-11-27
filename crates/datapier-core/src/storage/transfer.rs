//! 数据交换配置和执行事实；这里只保存定位、摘要和状态，不保存迁移数据。
use super::*;
use crate::model::transfer::{
    TransferInputIdentity, TransferLocator, TransferProgress, TransferSpec, TransferStepOutcome,
    TransferStepState,
};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};

const PAGE_BYTES: usize = 1024 * 1024;
const ITEM_BYTES: usize = 64 * 1024;

fn bounded(text: &str) -> Result<()> {
    if text.is_empty() || text.len() > ITEM_BYTES {
        return Err(AppError::invalid("交换元数据长度无效"));
    }
    Ok(())
}
fn digest(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(AppError::invalid("SHA-256 摘要无效"));
    }
    Ok(())
}
fn integer(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| AppError::invalid("数值超出存储范围"))
}
fn json_bounded<T: Serialize>(value: &T) -> Result<String> {
    let value = serde_json::to_string(value)?;
    if value.len() > ITEM_BYTES {
        return Err(AppError::new("resource_limit", "交换元数据超限"));
    }
    Ok(value)
}
fn conflict() -> AppError {
    AppError::new("conflict", "记录不存在或版本已变化")
}
fn now_ms() -> Result<i64> {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| AppError::new("storage", "系统时间早于 Unix epoch"))?
        .as_millis();
    i64::try_from(ms).map_err(|_| AppError::new("storage", "系统时间超出范围"))
}

/// 运行上下文仅包括可审计的标识与摘要，不包含计划 SQL 或预览数据。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferRunContext {
    pub plan_id: Option<Uuid>,
    pub plan_revision: Option<u64>,
    pub plan_digest: Option<String>,
    pub source_profile: Option<Uuid>,
    pub destination_profile: Option<Uuid>,
    #[serde(default)]
    pub inputs: Vec<TransferInputIdentity>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferRun {
    pub id: Uuid,
    pub execution_id: Uuid,
    pub workspace_id: String,
    pub spec_digest: String,
    pub context: TransferRunContext,
    pub state: String,
    pub progress: TransferProgress,
    pub retry_of: Option<Uuid>,
    pub revision: u64,
    pub created_at_ms: i64,
    pub finished_at_ms: Option<i64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPreset {
    pub id: Uuid,
    pub workspace_id: String,
    pub revision: u64,
    pub name: String,
    pub spec: TransferSpec,
    pub updated_at_ms: i64,
}

/// 本机绑定不进入共享项目；路径覆盖按具名参数保存。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindingOverrides {
    pub database: Option<String>,
    pub redis_db: Option<u32>,
    pub paths: std::collections::BTreeMap<String, std::path::PathBuf>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectBinding {
    pub project_id: Uuid,
    pub binding_group: String,
    pub connection_alias: String,
    pub profile_id: Uuid,
    pub overrides: BindingOverrides,
    pub revision: u64,
}

/// 登记的是资源身份和导出所需元信息，不是文件内容或 Redis key。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactIdentity {
    #[serde(default)]
    pub task_id: Option<Uuid>,
    pub path: Option<std::path::PathBuf>,
    pub canonical_path: Option<std::path::PathBuf>,
    pub file_id: Option<String>,
    pub size: Option<u64>,
    pub sha256: Option<String>,
    pub profile_id: Option<Uuid>,
    pub redis_db: Option<u32>,
    pub ownership_digest: Option<String>,
    pub temp_b64: Option<String>,
    pub marker_b64: Option<String>,
    pub pending_marker_b64: Option<String>,
    pub kind: Option<String>,
    pub export_name: Option<String>,
    pub media_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stage_dir_identity: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferArtifact {
    pub id: Uuid,
    pub run_id: Option<Uuid>,
    pub owner_client_id: Option<Uuid>,
    pub kind: String,
    pub state: String,
    pub identity: ArtifactIdentity,
    pub revision: u64,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyReservation {
    Reserved,
    Duplicate,
    TargetConflict,
}

fn validate_locator(locator: &TransferLocator) -> Result<()> {
    for value in [&locator.source_digest, &locator.target_digest]
        .into_iter()
        .flatten()
    {
        digest(value)?;
    }
    if locator
        .first_record
        .zip(locator.last_record)
        .is_some_and(|(a, b)| a > b)
        || locator
            .byte_start
            .zip(locator.byte_end)
            .is_some_and(|(a, b)| a > b)
    {
        return Err(AppError::invalid("定位范围无效"));
    }
    if locator.record_offsets.len() > 500 {
        return Err(AppError::new("resource_limit", "批次定位超过 500 项"));
    }
    if locator
        .record_offsets
        .windows(2)
        .any(|pair| pair[0] >= pair[1])
        || locator
            .record_offsets
            .first()
            .is_some_and(|first| locator.byte_start != Some(*first))
        || locator
            .record_offsets
            .last()
            .is_some_and(|last| locator.byte_end != Some(*last))
    {
        return Err(AppError::invalid("批次定位范围无效"));
    }
    if let Some(object) = &locator.object {
        bounded(object)?;
    }
    Ok(())
}
fn validate_outcome(outcome: &TransferStepOutcome) -> Result<()> {
    integer(outcome.ordinal)?;
    integer(outcome.affected)?;
    digest(&outcome.digest)?;
    bounded(&outcome.phase)?;
    if outcome.item_states.len() > 500 {
        return Err(AppError::new("resource_limit", "批次结果超过 500 项"));
    }
    if let Some(record) = outcome.error_record {
        integer(record)?;
    }
    validate_locator(&outcome.locator)?;
    // 错误只允许稳定错误码，不能把数据库返回的正文混入日志。
    for code in [&outcome.error_code, &outcome.database_code]
        .into_iter()
        .flatten()
    {
        if code.is_empty()
            || code.len() > 128
            || !code
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
        {
            return Err(AppError::invalid("错误码无效"));
        }
    }
    Ok(())
}
fn validate_identity(identity: &ArtifactIdentity) -> Result<()> {
    for d in [&identity.sha256, &identity.ownership_digest]
        .into_iter()
        .flatten()
    {
        digest(d)?;
    }
    json_bounded(identity)?;
    Ok(())
}
fn run_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<TransferRun> {
    let retry: Option<String> = row.try_get("retry_of")?;
    Ok(TransferRun {
        id: parse_id(row.try_get("id")?)?,
        execution_id: parse_id(row.try_get("execution_id")?)?,
        workspace_id: row.try_get("workspace_id")?,
        spec_digest: row.try_get("spec_digest")?,
        context: serde_json::from_str(row.try_get("context_json")?)?,
        state: row.try_get("state")?,
        progress: serde_json::from_str(row.try_get("progress_json")?)?,
        retry_of: retry.as_deref().map(parse_id).transpose()?,
        revision: row.try_get::<i64, _>("revision")? as u64,
        created_at_ms: row.try_get("created_at_ms")?,
        finished_at_ms: row.try_get("finished_at_ms")?,
    })
}
fn artifact_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<TransferArtifact> {
    let run: Option<String> = row.try_get("run_id")?;
    let owner: Option<String> = row.try_get("owner_client_id")?;
    Ok(TransferArtifact {
        id: parse_id(row.try_get("id")?)?,
        run_id: run.as_deref().map(parse_id).transpose()?,
        owner_client_id: owner.as_deref().map(parse_id).transpose()?,
        kind: row.try_get("kind")?,
        state: row.try_get("state")?,
        identity: serde_json::from_str(row.try_get("identity_json")?)?,
        revision: row.try_get::<i64, _>("revision")? as u64,
    })
}

impl Store {
    /// execution 必须已登记且属于同一工作区；首次创建即冻结摘要。
    pub async fn transfer_create_run(
        &self,
        id: Uuid,
        execution_id: Uuid,
        workspace: &str,
        spec_digest: &str,
        context: &TransferRunContext,
        retry_of: Option<Uuid>,
    ) -> Result<()> {
        bounded(workspace)?;
        digest(spec_digest)?;
        if let Some(d) = &context.plan_digest {
            digest(d)?;
        }
        let context = json_bounded(context)?;
        let progress = json_bounded(&TransferProgress {
            checkpoint_ordinal: Some(0),
            ..Default::default()
        })?;
        let result = sqlx::query("INSERT INTO transfer_runs(id,execution_id,workspace_id,spec_digest,context_json,state,progress_json,retry_of,revision,created_at_ms) SELECT ?,id,?,?,?,'running',?,?,1,? FROM executions WHERE id=? AND workspace_id=?")
            .bind(id.to_string()).bind(workspace).bind(spec_digest).bind(context).bind(progress).bind(retry_of.map(|x| x.to_string())).bind(now_ms()?).bind(execution_id.to_string()).bind(workspace).execute(&mut *self.pool.acquire().await?).await?;
        if result.rows_affected() != 1 {
            return Err(AppError::new("not_found", "执行记录不存在或工作区不匹配"));
        }
        Ok(())
    }
    pub async fn transfer_run(&self, id: Uuid) -> Result<TransferRun> {
        let row = sqlx::query("SELECT * FROM transfer_runs WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&mut *self.pool.acquire().await?)
            .await?
            .ok_or_else(|| AppError::new("not_found", "交换任务不存在"))?;
        run_from_row(&row)
    }
    /// 单页按序列化字节预算截断，offset 为记录数。
    pub async fn transfer_runs(
        &self,
        workspace: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<TransferRun>> {
        if limit == 0 || limit > 1000 {
            return Err(AppError::invalid("分页参数无效"));
        }
        let mut connection = self.pool.acquire().await?;
        let mut rows = sqlx::query("SELECT * FROM transfer_runs WHERE workspace_id=? ORDER BY created_at_ms DESC,id DESC LIMIT ? OFFSET ?").bind(workspace).bind(integer(limit)?).bind(integer(offset)?).fetch(&mut *connection);
        let mut result = Vec::new();
        let mut bytes = 2usize;
        while let Some(row) = rows.try_next().await? {
            let item = run_from_row(&row)?;
            let n = serde_json::to_vec(&item)?.len() + 1;
            if bytes + n > PAGE_BYTES {
                break;
            }
            bytes += n;
            result.push(item);
        }
        Ok(result)
    }
    /// 更新进度和终态采用版本 CAS；终态不可重新启动。
    pub async fn transfer_update_progress(
        &self,
        run_id: Uuid,
        expected_revision: u64,
        state: &str,
        progress: &TransferProgress,
    ) -> Result<u64> {
        if !matches!(
            state,
            "running" | "cancelling" | "succeeded" | "failed" | "cancelled" | "outcome_unknown"
        ) {
            return Err(AppError::invalid("交换状态无效"));
        }
        let next = integer(expected_revision.checked_add(1).ok_or_else(conflict)?)?;
        let finished = if matches!(
            state,
            "succeeded" | "failed" | "cancelled" | "outcome_unknown"
        ) {
            Some(now_ms()?)
        } else {
            None
        };
        let result=sqlx::query("UPDATE transfer_runs SET state=?,progress_json=?,revision=?,finished_at_ms=? WHERE id=? AND revision=? AND state IN ('running','cancelling') AND NOT(state='cancelling' AND ?='running') AND (? IN ('running','cancelling','outcome_unknown') OR NOT EXISTS(SELECT 1 FROM transfer_steps WHERE run_id=transfer_runs.id AND state IN ('sent','outcome_unknown')))")
            .bind(state).bind(json_bounded(progress)?).bind(next).bind(finished).bind(run_id.to_string()).bind(integer(expected_revision)?).bind(state).bind(state).execute(&mut *self.pool.acquire().await?).await?;
        if result.rows_affected() != 1 {
            return Err(conflict());
        }
        Ok(next as u64)
    }
    /// 新步骤必须未发送；终态只能由已登记的步骤推进。
    pub async fn transfer_put_step(
        &self,
        run_id: Uuid,
        outcome: &TransferStepOutcome,
    ) -> Result<()> {
        validate_outcome(outcome)?;
        let ordinal = integer(outcome.ordinal)?;
        let locator = json_bounded(&outcome.locator)?;
        let payload = json_bounded(outcome)?;
        let result = if outcome.state == TransferStepState::NotSent {
            sqlx::query("INSERT INTO transfer_steps(run_id,ordinal,locator_json,phase,state,digest,outcome_json) SELECT ?,?,?,?,'not_sent',?,? WHERE EXISTS(SELECT 1 FROM transfer_runs WHERE id=? AND state='running')")
                .bind(run_id.to_string()).bind(ordinal).bind(locator).bind(&outcome.phase).bind(&outcome.digest).bind(payload).bind(run_id.to_string()).execute(&mut *self.pool.acquire().await?).await?
        } else {
            if outcome.state == TransferStepState::Sent {
                return Err(AppError::invalid("发送状态必须原子登记"));
            }
            sqlx::query("UPDATE transfer_steps SET state=?,outcome_json=? WHERE run_id=? AND ordinal=? AND locator_json=? AND phase=? AND digest=? AND ((state='sent' AND ? IN ('applied','rolled_back','failed','skipped','outcome_unknown')) OR (state='not_sent' AND ? IN ('failed','skipped'))) ")
                .bind(outcome.state.as_str()).bind(payload).bind(run_id.to_string()).bind(ordinal).bind(locator).bind(&outcome.phase).bind(&outcome.digest).bind(outcome.state.as_str()).bind(outcome.state.as_str()).execute(&mut *self.pool.acquire().await?).await?
        };
        if result.rows_affected() != 1 {
            return Err(conflict());
        }
        Ok(())
    }
    /// 只能由持有 ExecutionSink 取消门的发送路径调用；两个发送事实同事务提交。
    pub async fn transfer_mark_sent(
        &self,
        execution_id: Uuid,
        run_id: Uuid,
        ordinal: u64,
        may_write: bool,
    ) -> Result<()> {
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin().await?;
        let step=sqlx::query("UPDATE transfer_steps SET state='sent',outcome_json=json_set(outcome_json,'$.state','sent') WHERE run_id=? AND ordinal=? AND state IN ('not_sent','sent') AND EXISTS(SELECT 1 FROM transfer_runs WHERE id=? AND execution_id=? AND state='running')")
            .bind(run_id.to_string()).bind(integer(ordinal)?).bind(run_id.to_string()).bind(execution_id.to_string()).execute(&mut *tx).await?;
        if step.rows_affected() != 1 {
            return Err(conflict());
        }
        let execution=sqlx::query("UPDATE executions SET sent=1,may_write=MAX(may_write,?),summary=CASE WHEN json_extract(params,'$._task_context.source')='archery' OR json_type(summary,'$.remote_completion') IS NOT NULL THEN json_set(CASE WHEN json_type(summary)='object' THEN summary ELSE '{}' END,'$.remote_completion','unknown') ELSE summary END,updated_at=unixepoch('subsec') WHERE id=?")
            .bind(i64::from(may_write)).bind(execution_id.to_string()).execute(&mut *tx).await?;
        if execution.rows_affected() != 1 {
            return Err(conflict());
        }
        tx.commit().await?;
        Ok(())
    }
    pub async fn transfer_items(
        &self,
        run_id: Uuid,
        after: Option<u64>,
        limit: u64,
    ) -> Result<Vec<TransferStepOutcome>> {
        if limit == 0 || limit > 1000 {
            return Err(AppError::invalid("分页参数无效"));
        }
        let after = after.map(integer).transpose()?.unwrap_or(-1);
        let mut connection = self.pool.acquire().await?;
        let mut rows=sqlx::query("SELECT outcome_json FROM transfer_steps WHERE run_id=? AND ordinal>? ORDER BY ordinal LIMIT ?").bind(run_id.to_string()).bind(after).bind(integer(limit)?).fetch(&mut *connection);
        let mut items = Vec::new();
        let mut bytes = 2;
        while let Some(row) = rows.try_next().await? {
            let s: &str = row.try_get(0)?;
            if bytes + s.len() + 1 > PAGE_BYTES {
                break;
            }
            let item: TransferStepOutcome = serde_json::from_str(s)?;
            validate_outcome(&item)?;
            bytes += s.len() + 1;
            items.push(item);
        }
        Ok(items)
    }
    /// 查询整条续作链的输入字节位置；未知或已生效事实优先，不能因后续运行跳过而丢失。
    pub async fn transfer_prior_input_state(
        &self,
        run_id: Uuid,
        input: usize,
        byte_start: u64,
        phase: &str,
    ) -> Result<Option<TransferStepState>> {
        bounded(phase)?;
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin().await?;
        // UNION ALL 保留路径，以便检测循环；超过安全深度或缺失祖先都不能当成无旧事实。
        let invalid: i64=sqlx::query_scalar("WITH RECURSIVE chain(id,depth,path,cycle) AS (SELECT ?,0,',' || ? || ',',0 UNION ALL SELECT r.retry_of,c.depth+1,c.path || r.retry_of || ',',instr(c.path,',' || r.retry_of || ',')>0 FROM chain c JOIN transfer_runs r ON r.id=c.id WHERE r.retry_of IS NOT NULL AND c.cycle=0 AND c.depth<1024) SELECT EXISTS(SELECT 1 FROM chain c LEFT JOIN transfer_runs r ON r.id=c.id WHERE c.cycle=1 OR c.depth=1024 OR r.id IS NULL)")
            .bind(run_id.to_string()).bind(run_id.to_string()).fetch_one(&mut *tx).await?;
        if invalid != 0 {
            return Err(AppError::new(
                "storage",
                "续作祖先链缺失、循环或超出深度限制",
            ));
        }
        let state: Option<String> = sqlx::query_scalar("WITH RECURSIVE lineage(id) AS (SELECT id FROM transfer_runs WHERE id=?1 UNION SELECT r.retry_of FROM transfer_runs r JOIN lineage l ON r.id=l.id WHERE r.retry_of IS NOT NULL), matches AS (SELECT s.ordinal, CASE WHEN s.state IN ('sent','outcome_unknown') THEN s.state ELSE COALESCE(json_extract(s.outcome_json,'$.item_states[' || offsets.key || ']'),s.state) END AS effective_state FROM transfer_steps s JOIN json_each(s.locator_json,'$.record_offsets') offsets WHERE s.run_id IN (SELECT id FROM lineage) AND json_extract(s.locator_json,'$.input')=?2 AND offsets.value=?3 AND s.phase=?4 UNION ALL SELECT s.ordinal,s.state FROM transfer_steps s WHERE s.run_id IN (SELECT id FROM lineage) AND json_extract(s.locator_json,'$.input')=?2 AND COALESCE(json_array_length(s.locator_json,'$.record_offsets'),0)=0 AND json_extract(s.locator_json,'$.byte_start')=?3 AND s.phase=?4) SELECT effective_state FROM matches ORDER BY CASE effective_state WHEN 'outcome_unknown' THEN 0 WHEN 'sent' THEN 1 WHEN 'applied' THEN 2 WHEN 'skipped' THEN 3 ELSE 4 END,ordinal DESC LIMIT 1")
            .bind(run_id.to_string()).bind(integer(u64::try_from(input).map_err(|_|AppError::invalid("输入索引超限"))?)?).bind(integer(byte_start)?).bind(phase).fetch_optional(&mut *tx).await?;
        tx.commit().await?;
        state
            .map(|state| match state.as_str() {
                "not_sent" => Ok(TransferStepState::NotSent),
                "sent" => Ok(TransferStepState::Sent),
                "applied" => Ok(TransferStepState::Applied),
                "rolled_back" => Ok(TransferStepState::RolledBack),
                "failed" => Ok(TransferStepState::Failed),
                "skipped" => Ok(TransferStepState::Skipped),
                "outcome_unknown" => Ok(TransferStepState::OutcomeUnknown),
                _ => Err(AppError::new("storage", "交换步骤状态无效")),
            })
            .transpose()
    }
    /// 仅供 Direct SCAN；文件输入必须逐记录执行，不调用此接口。
    pub async fn transfer_reserve_key(
        &self,
        run_id: Uuid,
        ordinal: u64,
        source_digest: &str,
        target_digest: &str,
    ) -> Result<KeyReservation> {
        digest(source_digest)?;
        digest(target_digest)?;
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin().await?;
        let result=sqlx::query("INSERT INTO transfer_key_ops(run_id,source_digest,target_digest,step_ordinal) VALUES(?,?,?,?) ON CONFLICT DO NOTHING")
            .bind(run_id.to_string()).bind(source_digest).bind(target_digest).bind(integer(ordinal)?).execute(&mut *tx).await?;
        let reservation = if result.rows_affected() == 1 {
            KeyReservation::Reserved
        } else {
            let target: Option<String> = sqlx::query_scalar(
                "SELECT target_digest FROM transfer_key_ops WHERE run_id=? AND source_digest=?",
            )
            .bind(run_id.to_string())
            .bind(source_digest)
            .fetch_optional(&mut *tx)
            .await?;
            match target {
                Some(existing) if existing == target_digest => KeyReservation::Duplicate,
                _ => KeyReservation::TargetConflict,
            }
        };
        tx.commit().await?;
        Ok(reservation)
    }
    /// 仅恢复持久状态；绝不重放写入或清理未知结果。
    pub async fn transfer_recover(&self) -> Result<()> {
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin().await?;
        let runs=sqlx::query("SELECT id,execution_id,progress_json FROM transfer_runs WHERE state IN ('running','cancelling')").fetch_all(&mut *tx).await?;
        for run in runs {
            let run_id: &str = run.try_get("id")?;
            let execution_id: &str = run.try_get("execution_id")?;
            let mut progress: TransferProgress =
                serde_json::from_str(run.try_get("progress_json")?)?;
            let mut written_rows = 0u64;
            let mut written_keys = 0u64;
            let mut skipped = 0u64;
            let mut unknown = 0u64;
            let mut uncertain = false;
            {
                let mut steps =
                    sqlx::query("SELECT outcome_json FROM transfer_steps WHERE run_id=?")
                        .bind(run_id)
                        .fetch(&mut *tx);
                while let Some(row) = steps.try_next().await? {
                    let outcome: TransferStepOutcome = serde_json::from_str(row.try_get(0)?)?;
                    validate_outcome(&outcome)?;
                    let is_uncertain = matches!(
                        outcome.state,
                        TransferStepState::Sent | TransferStepState::OutcomeUnknown
                    ) || outcome.item_states.iter().any(|state| {
                        matches!(
                            state,
                            TransferStepState::Sent | TransferStepState::OutcomeUnknown
                        )
                    });
                    uncertain |= is_uncertain;
                    // 发送中步骤尚未完成，即使其序号早于水位也未计入未知结果。
                    if progress
                        .checkpoint_ordinal
                        .is_some_and(|ordinal| outcome.ordinal < ordinal)
                        && outcome.state != TransferStepState::Sent
                    {
                        continue;
                    }
                    if matches!(
                        outcome.state,
                        TransferStepState::Sent | TransferStepState::OutcomeUnknown
                    ) {
                        uncertain = true;
                        let count = if !outcome.locator.record_offsets.is_empty() {
                            outcome.locator.record_offsets.len() as u64
                        } else if outcome.phase == "data" {
                            outcome
                                .locator
                                .first_record
                                .zip(outcome.locator.last_record)
                                .and_then(|(first, last)| last.checked_sub(first)?.checked_add(1))
                                .unwrap_or_else(|| (outcome.item_states.len() as u64).max(1))
                        } else {
                            1
                        };
                        unknown = unknown.saturating_add(count);
                    } else if outcome.phase == "data" {
                        let uncertain_items = outcome
                            .item_states
                            .iter()
                            .filter(|state| {
                                matches!(
                                    state,
                                    TransferStepState::Sent | TransferStepState::OutcomeUnknown
                                )
                            })
                            .count() as u64;
                        if uncertain_items > 0 {
                            uncertain = true;
                            unknown = unknown.saturating_add(uncertain_items);
                        }
                        if outcome.item_states.is_empty() {
                            if outcome.state == TransferStepState::Applied {
                                written_rows = written_rows.saturating_add(outcome.affected);
                            }
                            if outcome.state == TransferStepState::Skipped {
                                skipped = skipped.saturating_add(1);
                            }
                        } else {
                            written_rows = written_rows.saturating_add(
                                outcome
                                    .item_states
                                    .iter()
                                    .filter(|state| **state == TransferStepState::Applied)
                                    .count() as u64,
                            );
                            skipped = skipped.saturating_add(
                                outcome
                                    .item_states
                                    .iter()
                                    .filter(|state| **state == TransferStepState::Skipped)
                                    .count() as u64,
                            );
                        }
                    } else if matches!(
                        outcome.phase.as_str(),
                        "redis_key" | "redis_import" | "redis_export_key"
                    ) {
                        if outcome.phase != "redis_export_key"
                            && outcome.state == TransferStepState::Applied
                        {
                            written_keys = written_keys.saturating_add(1);
                        }
                        if outcome.state == TransferStepState::Skipped {
                            skipped = skipped.saturating_add(1);
                        }
                    } else if outcome.phase == "conversion"
                        && outcome.state == TransferStepState::Skipped
                    {
                        skipped = skipped.saturating_add(1);
                    }
                }
            }
            if progress.checkpoint_ordinal.is_some() {
                progress.written_rows = progress.written_rows.saturating_add(written_rows);
                progress.written_keys = progress.written_keys.saturating_add(written_keys);
                progress.skipped = progress.skipped.saturating_add(skipped);
                progress.outcome_unknown = progress.outcome_unknown.saturating_add(unknown);
            } else {
                progress.written_rows = progress.written_rows.max(written_rows);
                progress.written_keys = progress.written_keys.max(written_keys);
                progress.skipped = progress.skipped.max(skipped);
                progress.outcome_unknown = progress.outcome_unknown.max(unknown);
            }
            let state = if uncertain {
                "outcome_unknown"
            } else {
                "cancelled"
            };
            let error = if uncertain {
                AppError::unknown("Engine 已中断，数据库写入结果需人工核对")
            } else {
                AppError::cancelled()
            };
            let summary =
                serde_json::to_string(&serde_json::json!({"run_id":run_id,"progress":progress}))?;
            sqlx::query("UPDATE executions SET state=?,summary=?,error=?,updated_at=unixepoch('subsec') WHERE id=?")
                .bind(state).bind(summary).bind(serde_json::to_string(&error)?).bind(execution_id).execute(&mut *tx).await?;
            sqlx::query("UPDATE transfer_runs SET progress_json=?,state=?,revision=revision+1,finished_at_ms=? WHERE id=?")
                .bind(json_bounded(&progress)?).bind(state).bind(now_ms()?).bind(run_id).execute(&mut *tx).await?;
            sqlx::query("UPDATE transfer_steps SET state='outcome_unknown',outcome_json=json_set(outcome_json,'$.state','outcome_unknown') WHERE run_id=? AND state='sent'").bind(run_id).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

impl Store {
    /// 保存用户意图；调用者只可传入用户选择的 key，不能传入扫描结果。
    pub async fn transfer_save_preset(
        &self,
        id: Uuid,
        workspace: &str,
        name: &str,
        spec: &TransferSpec,
        expected_revision: u64,
    ) -> Result<u64> {
        bounded(workspace)?;
        bounded(name)?;
        spec.validate()?;
        let payload = json_bounded(spec)?;
        let next = integer(expected_revision.checked_add(1).ok_or_else(conflict)?)?;
        let result = if expected_revision == 0 {
            sqlx::query("INSERT INTO transfer_presets(id,workspace_id,revision,name,spec_json,updated_at_ms) VALUES(?,?,?,?,?,?) ON CONFLICT(id) DO NOTHING")
                .bind(id.to_string()).bind(workspace).bind(next).bind(name).bind(payload).bind(now_ms()?).execute(&mut *self.pool.acquire().await?).await?
        } else {
            sqlx::query("UPDATE transfer_presets SET revision=?,name=?,spec_json=?,updated_at_ms=? WHERE id=? AND workspace_id=? AND revision=?")
                .bind(next).bind(name).bind(payload).bind(now_ms()?).bind(id.to_string()).bind(workspace).bind(integer(expected_revision)?).execute(&mut *self.pool.acquire().await?).await?
        };
        if result.rows_affected() != 1 {
            return Err(conflict());
        }
        Ok(next as u64)
    }
    pub async fn transfer_delete_preset(
        &self,
        id: Uuid,
        workspace: &str,
        expected_revision: u64,
    ) -> Result<()> {
        let result = sqlx::query(
            "DELETE FROM transfer_presets WHERE id=? AND workspace_id=? AND revision=?",
        )
        .bind(id.to_string())
        .bind(workspace)
        .bind(integer(expected_revision)?)
        .execute(&mut *self.pool.acquire().await?)
        .await?;
        if result.rows_affected() != 1 {
            return Err(conflict());
        }
        Ok(())
    }
    pub async fn transfer_presets(
        &self,
        workspace: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<TransferPreset>> {
        if limit == 0 || limit > 1000 {
            return Err(AppError::invalid("分页参数无效"));
        }
        let mut connection = self.pool.acquire().await?;
        let mut rows = sqlx::query(
            "SELECT * FROM transfer_presets WHERE workspace_id=? ORDER BY name,id LIMIT ? OFFSET ?",
        )
        .bind(workspace)
        .bind(integer(limit)?)
        .bind(integer(offset)?)
        .fetch(&mut *connection);
        let mut items = Vec::new();
        let mut bytes = 2;
        while let Some(row) = rows.try_next().await? {
            let item = TransferPreset {
                id: parse_id(row.try_get("id")?)?,
                workspace_id: row.try_get("workspace_id")?,
                revision: row.try_get::<i64, _>("revision")? as u64,
                name: row.try_get("name")?,
                spec: serde_json::from_str(row.try_get("spec_json")?)?,
                updated_at_ms: row.try_get("updated_at_ms")?,
            };
            let size = serde_json::to_vec(&item)?.len() + 1;
            if bytes + size > PAGE_BYTES {
                break;
            }
            bytes += size;
            items.push(item);
        }
        Ok(items)
    }
    /// 一次绑定操作中的所有 alias 共同提交，任意 revision 冲突回滚整组。
    pub async fn project_save_bindings(
        &self,
        bindings: &[ProjectBinding],
    ) -> Result<Vec<ProjectBinding>> {
        if bindings.len() > 1000 {
            return Err(AppError::invalid("一次最多保存 1000 个连接绑定"));
        }
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin().await?;
        let mut seen = std::collections::BTreeSet::new();
        let mut saved = Vec::with_capacity(bindings.len());
        for binding in bindings {
            bounded(&binding.binding_group)?;
            bounded(&binding.connection_alias)?;
            if !seen.insert((
                binding.project_id,
                &binding.binding_group,
                &binding.connection_alias,
            )) {
                return Err(AppError::invalid("本次保存包含重复连接绑定"));
            }
            let payload = json_bounded(&binding.overrides)?;
            let next = integer(binding.revision.checked_add(1).ok_or_else(conflict)?)?;
            let affected = if binding.revision == 0 {
                sqlx::query("INSERT INTO project_bindings(project_id,binding_group,connection_alias,profile_id,database_json,revision) VALUES(?,?,?,?,?,?) ON CONFLICT DO NOTHING")
                    .bind(binding.project_id.to_string()).bind(&binding.binding_group).bind(&binding.connection_alias).bind(binding.profile_id.to_string()).bind(&payload).bind(next).execute(&mut *tx).await?.rows_affected()
            } else {
                sqlx::query("UPDATE project_bindings SET profile_id=?,database_json=?,revision=? WHERE project_id=? AND binding_group=? AND connection_alias=? AND revision=?")
                    .bind(binding.profile_id.to_string()).bind(&payload).bind(next).bind(binding.project_id.to_string()).bind(&binding.binding_group).bind(&binding.connection_alias).bind(integer(binding.revision)?).execute(&mut *tx).await?.rows_affected()
            };
            if affected != 1 {
                return Err(conflict());
            }
            let mut current = binding.clone();
            current.revision = next as u64;
            saved.push(current);
        }
        tx.commit().await?;
        Ok(saved)
    }
    pub async fn project_bindings(
        &self,
        project_id: Uuid,
        group: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<ProjectBinding>> {
        if limit == 0 || limit > 1000 {
            return Err(AppError::invalid("分页参数无效"));
        }
        let mut connection = self.pool.acquire().await?;
        let mut rows=sqlx::query("SELECT * FROM project_bindings WHERE project_id=? AND binding_group=? ORDER BY connection_alias LIMIT ? OFFSET ?").bind(project_id.to_string()).bind(group).bind(integer(limit)?).bind(integer(offset)?).fetch(&mut *connection);
        let mut items = Vec::new();
        let mut bytes = 2;
        while let Some(row) = rows.try_next().await? {
            let item = ProjectBinding {
                project_id,
                binding_group: row.try_get("binding_group")?,
                connection_alias: row.try_get("connection_alias")?,
                profile_id: parse_id(row.try_get("profile_id")?)?,
                overrides: serde_json::from_str(row.try_get("database_json")?)?,
                revision: row.try_get::<i64, _>("revision")? as u64,
            };
            let size = serde_json::to_vec(&item)?.len() + 1;
            if bytes + size > PAGE_BYTES {
                break;
            }
            bytes += size;
            items.push(item);
        }
        Ok(items)
    }
    /// 确认只属于本机规范目录和当前绑定内容，绑定变化后摘要自然失效。
    pub async fn project_binding_confirmed(
        &self,
        project: Uuid,
        root: &str,
        group: &str,
        digest: &str,
    ) -> Result<bool> {
        let saved: Option<String> = sqlx::query_scalar("SELECT binding_digest FROM project_binding_confirmations WHERE project_id=? AND root=? AND binding_group=?")
            .bind(project.to_string()).bind(root).bind(group)
            .fetch_optional(&mut *self.pool.acquire().await?).await?;
        Ok(saved.as_deref() == Some(digest))
    }

    pub async fn project_confirm_binding(
        &self,
        project: Uuid,
        root: &str,
        group: &str,
        binding_digest: &str,
    ) -> Result<()> {
        bounded(root)?;
        bounded(group)?;
        digest(binding_digest)?;
        sqlx::query("INSERT INTO project_binding_confirmations(project_id,root,binding_group,binding_digest) VALUES(?,?,?,?) ON CONFLICT(project_id,root,binding_group) DO UPDATE SET binding_digest=excluded.binding_digest")
            .bind(project.to_string()).bind(root).bind(group).bind(binding_digest)
            .execute(&mut *self.pool.acquire().await?).await?;
        Ok(())
    }
    pub async fn transfer_register_artifact(
        &self,
        owner: Uuid,
        run_id: Option<Uuid>,
        kind: &str,
        identity: &Json,
    ) -> Result<Uuid> {
        let identity: ArtifactIdentity = serde_json::from_value(identity.clone())?;
        validate_artifact(kind, &identity)?;
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO transfer_artifacts(id,run_id,owner_client_id,kind,state,identity_json,revision) VALUES(?,?,?,?,'preparing',?,1)")
            .bind(id.to_string()).bind(run_id.map(|v| v.to_string())).bind(owner.to_string()).bind(kind).bind(json_bounded(&identity)?).execute(&mut *self.pool.acquire().await?).await?;
        Ok(id)
    }
    pub async fn transfer_artifact(&self, id: Uuid) -> Result<TransferArtifact> {
        let row = sqlx::query("SELECT * FROM transfer_artifacts WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&mut *self.pool.acquire().await?)
            .await?
            .ok_or_else(|| AppError::new("not_found", "资源登记不存在"))?;
        artifact_from_row(&row)
    }
    /// 活跃性由 Engine 在持有客户端生命周期门时判定；版本 CAS 防止并发领取。
    pub async fn transfer_claim_artifact(
        &self,
        id: Uuid,
        new_owner: Uuid,
        expected_revision: u64,
        observed_owner: Option<Uuid>,
        owner_active: bool,
    ) -> Result<u64> {
        if owner_active && observed_owner != Some(new_owner) {
            return Err(AppError::new("busy", "资源所有者仍活跃"));
        }
        let next = integer(expected_revision.checked_add(1).ok_or_else(conflict)?)?;
        let result=sqlx::query("UPDATE transfer_artifacts SET owner_client_id=?,revision=? WHERE id=? AND revision=? AND owner_client_id IS ?")
            .bind(new_owner.to_string()).bind(next).bind(id.to_string()).bind(integer(expected_revision)?).bind(observed_owner.map(|x| x.to_string())).execute(&mut *self.pool.acquire().await?).await?;
        if result.rows_affected() != 1 {
            return Err(conflict());
        }
        Ok(next as u64)
    }
    /// 状态只登记清理／发布事实；文件删除和 Redis 清理须由调用者验证实际身份。
    pub async fn transfer_update_artifact(
        &self,
        id: Uuid,
        owner: Uuid,
        expected_revision: Option<u64>,
        state: &str,
        identity: Option<&Json>,
    ) -> Result<u64> {
        if !matches!(
            state,
            "preparing"
                | "ready"
                | "published"
                | "cleanup_pending"
                | "cleaned"
                | "outcome_unknown"
                | "recovery_conflict"
        ) {
            return Err(AppError::invalid("资源状态无效"));
        }
        let current = self.transfer_artifact(id).await?;
        if current.owner_client_id != Some(owner)
            || expected_revision.is_some_and(|revision| revision != current.revision)
        {
            return Err(conflict());
        }
        if (current.state == "outcome_unknown"
            || (current.state == "published" && current.kind != "project_stage"))
            && matches!(state, "cleanup_pending" | "cleaned")
        {
            return Err(AppError::invalid("已发布或结果未知的资源不能直接清理"));
        }
        let identity = if let Some(value) = identity {
            let parsed: ArtifactIdentity = serde_json::from_value(value.clone())?;
            validate_artifact(&current.kind, &parsed)?;
            parsed
        } else {
            current.identity
        };
        let next = integer(current.revision.checked_add(1).ok_or_else(conflict)?)?;
        let result=sqlx::query("UPDATE transfer_artifacts SET state=?,identity_json=?,revision=? WHERE id=? AND owner_client_id=? AND revision=?")
            .bind(state).bind(json_bounded(&identity)?).bind(next).bind(id.to_string()).bind(owner.to_string()).bind(integer(current.revision)?).execute(&mut *self.pool.acquire().await?).await?;
        if result.rows_affected() != 1 {
            return Err(conflict());
        }
        Ok(next as u64)
    }
    /// 仅确认本次显式 Redis 清理的明确回包；未知发布不能借此绕过普通清理门。
    /// 调用者须先登记待确认，再原子验证临时对象归属并取得 EVAL 响应；断连不能调用。
    pub async fn transfer_confirm_redis_cleanup(
        &self,
        id: Uuid,
        owner: Uuid,
        expected_revision: u64,
        state: &str,
    ) -> Result<u64> {
        if !matches!(state, "cleaned" | "recovery_conflict") {
            return Err(AppError::invalid("Redis 清理确认状态无效"));
        }
        let next = integer(expected_revision.checked_add(1).ok_or_else(conflict)?)?;
        let result = sqlx::query("UPDATE transfer_artifacts SET state=?,revision=? WHERE id=? AND owner_client_id=? AND revision=? AND kind='redis_temporary' AND state='outcome_unknown'")
            .bind(state).bind(next).bind(id.to_string()).bind(owner.to_string()).bind(integer(expected_revision)?)
            .execute(&mut *self.pool.acquire().await?).await?;
        if result.rows_affected() != 1 {
            return Err(conflict());
        }
        Ok(next as u64)
    }
    pub async fn transfer_artifacts(
        &self,
        owner: Option<Uuid>,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<TransferArtifact>> {
        if limit == 0 || limit > 1000 {
            return Err(AppError::invalid("分页参数无效"));
        }
        let mut connection = self.pool.acquire().await?;
        let mut rows=sqlx::query("SELECT * FROM transfer_artifacts WHERE (? IS NULL OR owner_client_id=?) ORDER BY id LIMIT ? OFFSET ?")
            .bind(owner.map(|x|x.to_string())).bind(owner.map(|x|x.to_string())).bind(integer(limit)?).bind(integer(offset)?).fetch(&mut *connection);
        let mut items = Vec::new();
        let mut bytes = 2;
        while let Some(row) = rows.try_next().await? {
            let item = artifact_from_row(&row)?;
            let size = serde_json::to_vec(&item)?.len() + 1;
            if bytes + size > PAGE_BYTES {
                break;
            }
            bytes += size;
            items.push(item);
        }
        Ok(items)
    }
}

fn validate_artifact(kind: &str, identity: &ArtifactIdentity) -> Result<()> {
    if !matches!(
        kind,
        "export_partial" | "report" | "project_stage" | "redis_temporary" | "input_index"
    ) {
        return Err(AppError::invalid("资源类型无效"));
    }
    if kind != "redis_temporary"
        && (identity.temp_b64.is_some()
            || identity.marker_b64.is_some()
            || identity.pending_marker_b64.is_some())
    {
        return Err(AppError::invalid("临时 Redis 身份与资源类型不符"));
    }
    for value in [
        &identity.temp_b64,
        &identity.marker_b64,
        &identity.pending_marker_b64,
    ]
    .into_iter()
    .flatten()
    {
        STANDARD
            .decode(value)
            .map_err(|_| AppError::invalid("临时对象身份编码无效"))?;
    }
    validate_identity(identity)
}

#[cfg(test)]
#[path = "../../tests/unit/storage/transfer.rs"]
mod tests;
