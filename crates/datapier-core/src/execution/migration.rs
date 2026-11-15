//! 跨文件／跨实例的数据交换编排。数据库行只在有界流中传递，日志只保存执行事实。

mod files;
mod mysql;
mod prepare;
mod redis;
mod run;

use crate::{
    AppError, ExecutionSink, Result, Store,
    model::{Profile, ProviderKind, transfer::*},
    providers::ProviderSession,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 登记输出文件时保留可验证的文件身份和内容摘要。
fn identity(
    path: &std::path::Path,
    snapshot: &crate::execution::data_files::OutputSnapshot,
) -> Result<serde_json::Value> {
    let file_id = snapshot
        .file_id
        .as_ref()
        .ok_or_else(|| AppError::new("recovery_conflict", "平台未提供 partial 文件身份"))?;
    let sha256 = snapshot
        .sha256
        .as_ref()
        .ok_or_else(|| AppError::new("recovery_conflict", "partial 文件缺少摘要"))?;
    Ok(
        serde_json::json!({"path":path,"canonical_path":snapshot.canonical_path,"file_id":file_id,"size":snapshot.size,"sha256":sha256}),
    )
}

fn same_identity(
    initial: &crate::execution::data_files::OutputSnapshot,
    current: &crate::execution::data_files::OutputSnapshot,
) -> Result<()> {
    if initial.file_id.is_none()
        || initial.file_id != current.file_id
        || initial.canonical_path != current.canonical_path
    {
        return Err(AppError::new("recovery_conflict", "partial 文件身份已变化"));
    }
    Ok(())
}

pub fn digest<T: serde::Serialize>(value: &T) -> Result<String> {
    struct HashWriter(Sha256);
    impl std::io::Write for HashWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.update(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut writer = HashWriter(Sha256::new());
    serde_json::to_writer(&mut writer, value)?;
    Ok(format!("{:x}", writer.0.finalize()))
}

pub fn encoded_size<T: serde::Serialize>(value: &T) -> Result<usize> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("编码大小溢出"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}

/// 从已保存的本机 profile 解析端点；共享项目 alias 必须先由 Engine 显式绑定。
pub async fn resolve_endpoint(
    store: &Store,
    endpoint: &TransferEndpoint,
    provider: ProviderKind,
    write: bool,
) -> Result<(Profile, String)> {
    let id = endpoint
        .profile_id
        .ok_or_else(|| AppError::new("binding_required", "请先将逻辑连接绑定到本机连接"))?;
    let mut profile = store
        .profiles()
        .await?
        .into_iter()
        .find(|profile| profile.id == id)
        .ok_or_else(|| AppError::new("not_found", "本机连接不存在"))?;
    if profile.is_archery() {
        return Err(AppError::new(
            "capability_unavailable",
            "Archery 不提供数据库迁移或 SQL 备份恢复",
        ));
    }
    if profile.provider != provider {
        return Err(AppError::invalid("源目标数据库类型与迁移配置不一致"));
    }
    if write {
        profile.ensure_write()?;
    }
    let fingerprint = digest(&profile)?;
    let direct = profile.direct_mut()?;
    if let Some(database) = &endpoint.database {
        if database.is_empty() {
            return Err(AppError::invalid("数据库名称不能为空"));
        }
        if direct
            .visible_databases
            .as_ref()
            .is_some_and(|allowed| !allowed.contains(database))
        {
            return Err(AppError::new(
                "permission_denied",
                "迁移数据库不在连接保存的可见范围内",
            ));
        }
        direct.database = Some(database.clone());
    }
    if let Some(db) = endpoint.redis_db {
        direct.redis_db = db;
    }
    profile.validate()?;
    Ok((profile, fingerprint))
}

async fn validate_databases(store: &Store, spec: &TransferSpec) -> Result<()> {
    if spec.provider != ProviderKind::Mysql {
        return Ok(());
    }
    for (endpoint, write) in [
        (spec.source.as_ref(), false),
        (spec.destination.as_ref(), true),
    ] {
        let Some(endpoint) = endpoint else { continue };
        let (profile, _) = resolve_endpoint(store, endpoint, spec.provider, write).await?;
        let Some(allowed) = &profile.direct()?.visible_databases else {
            continue;
        };
        let resources: Vec<&crate::model::ResourceTarget> = if write {
            spec.objects
                .iter()
                .map(|object| &object.target)
                .chain(spec.inputs.iter().filter_map(|input| input.target.as_ref()))
                .collect()
        } else {
            spec.objects.iter().map(|object| &object.source).collect()
        };
        if resources.iter().any(|resource| {
            resource
                .database
                .as_ref()
                .is_some_and(|database| !allowed.contains(database))
        }) {
            return Err(AppError::new(
                "permission_denied",
                "迁移数据库不在连接保存的可见范围内",
            ));
        }
    }
    Ok(())
}

pub(super) async fn connect(
    store: &Store,
    endpoint: &TransferEndpoint,
    provider: ProviderKind,
    write: bool,
    cancel: &CancellationToken,
) -> Result<(ProviderSession, String)> {
    let (profile, fingerprint) = resolve_endpoint(store, endpoint, provider, write).await?;
    let secret = tokio::select! {
        _ = cancel.cancelled() => return Err(AppError::cancelled()),
        secret = store.secret(profile.id) => secret?,
    };
    let session = tokio::select! {
        _ = cancel.cancelled() => return Err(AppError::cancelled()),
        result = ProviderSession::connect(profile, secret) => result?,
    };
    Ok((session, fingerprint))
}

pub async fn prepare(
    sink: &ExecutionSink,
    spec: TransferSpec,
    cancel: &CancellationToken,
) -> Result<PreparedTransfer> {
    prepare::prepare(sink, spec, None, cancel, None).await
}

/// 按原任务冻结的输入和配置准备 SQL 续作。
pub async fn prepare_retry(
    sink: &ExecutionSink,
    spec: TransferSpec,
    old_run: Uuid,
    cancel: &CancellationToken,
) -> Result<PreparedTransfer> {
    prepare::prepare(sink, spec, Some(old_run), cancel, None).await
}

/// 准备时通过 watch 报告最新状态；不将进度写入计划。
pub async fn prepare_with_progress(
    sink: &ExecutionSink,
    spec: TransferSpec,
    cancel: &CancellationToken,
    progress: &tokio::sync::watch::Sender<PreparationProgress>,
) -> Result<PreparedTransfer> {
    prepare::prepare(sink, spec, None, cancel, Some(progress)).await
}

/// 按原任务准备续作，同时报告最新准备状态。
pub async fn prepare_retry_with_progress(
    sink: &ExecutionSink,
    spec: TransferSpec,
    old_run: Uuid,
    cancel: &CancellationToken,
    progress: &tokio::sync::watch::Sender<PreparationProgress>,
) -> Result<PreparedTransfer> {
    prepare::prepare(sink, spec, Some(old_run), cancel, Some(progress)).await
}

pub use prepare::plan_digest;

/// 每次执行都重新解析权限及 profile 指纹，不将旧计划当成可重放的数据库命令。
pub async fn execute(
    prepared: &PreparedTransfer,
    sink: &ExecutionSink,
    run_id: Uuid,
    workspace: &str,
    retry_of: Option<Uuid>,
    destructive_confirmations: &[String],
    cancel: &CancellationToken,
) -> Result<TransferProgress> {
    run::execute(
        prepared,
        sink,
        run_id,
        workspace,
        retry_of,
        destructive_confirmations,
        cancel,
    )
    .await
}

/// provider 使用一个 ticket 记录一个 DDL／行批次／Redis key 的真实边界。
/// ticket 不包含输入行，只含可用于人工核对的定位和状态。
pub struct TransferTicket {
    pub sink: ExecutionSink,
    outcome: TransferStepOutcome,
}
impl TransferTicket {
    /// 借用已冻结的定位，避免 writer 为失败定位复制整批偏移。
    pub fn locator(&self) -> &TransferLocator {
        &self.outcome.locator
    }
}

pub struct TransferJournal {
    pub sink: ExecutionSink,
    pub run_id: Uuid,
    pub retry_of: Option<Uuid>,
    pub retry_states: Vec<TransferStepState>,
    pub progress: TransferProgress,
    revision: u64,
    ordinal: u64,
    pending: std::collections::BTreeSet<u64>,
}
impl TransferJournal {
    pub fn new(sink: ExecutionSink, run_id: Uuid) -> Self {
        let progress = TransferProgress {
            checkpoint_ordinal: Some(0),
            ..Default::default()
        };
        Self {
            sink,
            run_id,
            retry_of: None,
            retry_states: Vec::new(),
            progress,
            revision: 1,
            ordinal: 0,
            pending: std::collections::BTreeSet::new(),
        }
    }
    pub fn next_ordinal(&self) -> u64 {
        self.ordinal
    }
    pub async fn begin(
        &mut self,
        phase: &str,
        locator: TransferLocator,
        content_digest: String,
    ) -> Result<TransferTicket> {
        let ordinal = self.ordinal;
        self.ordinal = self
            .ordinal
            .checked_add(1)
            .ok_or_else(|| AppError::new("resource_limit", "执行步骤数超限"))?;
        let outcome = TransferStepOutcome {
            ordinal,
            locator,
            phase: phase.into(),
            state: TransferStepState::NotSent,
            digest: content_digest,
            affected: 0,
            error_code: None,
            database_code: None,
            item_states: Vec::new(),
            error_record: None,
        };
        self.sink
            .store
            .transfer_put_step(self.run_id, &outcome)
            .await?;
        self.progress.phase.clone_from(&outcome.phase);
        if outcome.locator.object.is_some() {
            self.progress
                .current_object
                .clone_from(&outcome.locator.object);
        }
        self.pending.insert(ordinal);
        Ok(TransferTicket {
            sink: self.sink.clone().with_transfer_step(self.run_id, ordinal),
            outcome,
        })
    }
    pub async fn finish(
        &mut self,
        mut ticket: TransferTicket,
        state: TransferStepState,
        affected: u64,
        item_states: Vec<TransferStepState>,
        error: Option<&AppError>,
        error_record: Option<u64>,
    ) -> Result<()> {
        ticket.outcome.state = state;
        ticket.outcome.affected = affected;
        ticket.outcome.item_states = item_states;
        ticket.outcome.error_record = error_record;
        ticket.outcome.error_code = error.map(|error| error.code.clone());
        ticket.outcome.database_code = error.and_then(|error| error.database_code.clone());
        self.sink
            .store
            .transfer_put_step(self.run_id, &ticket.outcome)
            .await?;
        // 行/key 由各 writer 按项计量；DDL 没有数据项，单独计入失败或待确认数量。
        let ddl = ticket.outcome.phase == "schema"
            || ticket.outcome.phase.starts_with("schema:")
            || ticket.outcome.phase.starts_with("object:")
            || ticket.outcome.phase.starts_with("database:")
            || ticket.outcome.phase == "database_context";
        if ddl {
            match state {
                TransferStepState::Failed => self.progress.failed += 1,
                TransferStepState::OutcomeUnknown => self.progress.outcome_unknown += 1,
                _ => (),
            }
        }
        self.pending.remove(&ticket.outcome.ordinal);
        Ok(())
    }
    pub async fn checkpoint(&mut self) -> Result<()> {
        self.progress.checkpoint_ordinal =
            Some(self.pending.first().copied().unwrap_or(self.ordinal));
        self.revision = self
            .sink
            .store
            .transfer_update_progress(self.run_id, self.revision, "running", &self.progress)
            .await?;
        Ok(())
    }
    pub async fn terminal(&mut self, state: &str) -> Result<()> {
        self.progress.checkpoint_ordinal =
            Some(self.pending.first().copied().unwrap_or(self.ordinal));
        self.revision = self
            .sink
            .store
            .transfer_update_progress(self.run_id, self.revision, state, &self.progress)
            .await?;
        Ok(())
    }
    pub async fn reserve_key(
        &self,
        ticket: &TransferTicket,
    ) -> Result<crate::storage::transfer::KeyReservation> {
        let locator = ticket.locator();
        let source = locator
            .source_digest
            .as_deref()
            .ok_or_else(|| AppError::invalid("缺少源 key 摘要"))?;
        let target = locator
            .target_digest
            .as_deref()
            .ok_or_else(|| AppError::invalid("缺少目标 key 摘要"))?;
        self.sink
            .store
            .transfer_reserve_key(self.run_id, ticket.outcome.ordinal, source, target)
            .await
    }
}

pub(super) fn source_columns(
    schema: &crate::model::TableSchema,
    object: &TransferObject,
) -> Result<Vec<crate::model::Column>> {
    if object.columns.is_empty() {
        return Ok(schema
            .columns
            .iter()
            .filter(|column| !column.generated)
            .cloned()
            .collect());
    }
    object
        .columns
        .iter()
        .map(|mapping| {
            schema
                .columns
                .iter()
                .find(|column| column.name == mapping.source)
                .cloned()
                .ok_or_else(|| AppError::invalid(format!("源列不存在：{}", mapping.source)))
        })
        .collect()
}

pub(super) fn selected_items(items: &[TransferPlanItem]) -> Result<()> {
    for item in items.iter().filter(|item| item.selected) {
        if item.action == "unknown_attribute" || item.sql.is_none() && item.phase == "schema" {
            return Err(AppError::new(
                "schema_incomplete",
                format!("结构项 {} 尚不能安全执行", item.id),
            ));
        }
    }
    Ok(())
}

pub(super) fn profile_digests() -> BTreeMap<Uuid, String> {
    BTreeMap::new()
}
