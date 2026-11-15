//! 执行时重新校验冻结配置；每个 provider 的提交边界分别记账。

use super::*;
use crate::storage::transfer::TransferRunContext;
use serde_json::json;
pub(super) async fn session_identity(session: &ProviderSession) -> Result<String> {
    match session {
        ProviderSession::Mysql(mysql) => mysql.server_identity().await,
        ProviderSession::Redis(redis) => redis.server_identity().await,
        ProviderSession::Archery(_) => Err(AppError::new(
            "capability_unavailable",
            "Archery 不支持迁移",
        )),
    }
}

pub(super) async fn execute(
    prepared: &PreparedTransfer,
    sink: &ExecutionSink,
    run_id: Uuid,
    workspace: &str,
    retry_of: Option<Uuid>,
    destructive_confirmations: &[String],
    cancel: &CancellationToken,
) -> Result<TransferProgress> {
    prepared.spec.validate()?;
    if retry_of != prepared.retry_of {
        return Err(AppError::conflict("续作任务与已准备计划不一致"));
    }
    if prepared.retry_states.iter().any(|state| {
        !matches!(
            state,
            TransferStepState::Failed | TransferStepState::RolledBack | TransferStepState::NotSent
        )
    }) {
        return Err(AppError::invalid("仅允许续作失败、已回滚或未发送的步骤"));
    }
    validate_databases(&sink.store, &prepared.spec).await?;
    if prepared.state != "ready"
        || prepared
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.blocking)
    {
        return Err(AppError::new("plan_not_ready", "迁移计划尚未准备完成"));
    }
    if super::prepare::plan_digest(prepared)? != prepared.digest {
        return Err(AppError::conflict("迁移计划内容已变化，请重新准备"));
    }
    for item in prepared
        .items
        .iter()
        .filter(|item| item.selected && item.destructive)
    {
        if !destructive_confirmations.contains(&item.id) {
            return Err(AppError::new(
                "confirmation_required",
                format!("结构项 {} 尚未单独确认", item.id),
            ));
        }
    }
    crate::model::ensure_not_cancelled(cancel)?;
    // 与结果、编辑计划及其他执行共享客户端额度；覆盖冻结计划和有界流缓冲。
    let reservation = Uuid::new_v4();
    let retained = encoded_size(prepared)?
        .saturating_mul(2)
        .saturating_add(if prepared.spec.mode == TransferMode::SqlRestore {
            0
        } else {
            MAX_VALUE_BYTES.saturating_mul(2)
        })
        .saturating_add(MAX_BATCH_BYTES.saturating_mul(4));
    sink.store
        .reserve_auxiliary(sink.execution_id, reservation, retained)
        .await?;
    let mut source = None;
    let mut target = None;
    let execution = async {
        if let Some(endpoint) = &prepared.spec.source {
            let (session, current) = connect(&sink.store, endpoint, prepared.spec.provider, false, cancel).await?;
            let matches = prepared.profile_digests.get(&session.profile().id) == Some(&current);
            source = Some(session);
            if !matches { return Err(AppError::conflict("源连接配置已变化")); }
            if prepared.source_identity.as_deref() != Some(session_identity(source.as_ref().unwrap()).await?.as_str()) {
                return Err(AppError::conflict("源服务端身份已变化，请重新准备迁移"));
            }
        }
        if let Some(endpoint) = &prepared.spec.destination {
            let (session, current) = connect(&sink.store, endpoint, prepared.spec.provider, true, cancel).await?;
            let matches = prepared.profile_digests.get(&session.profile().id) == Some(&current);
            target = Some(session);
            if !matches { return Err(AppError::conflict("目标连接配置已变化")); }
            if prepared.destination_identity.as_deref() != Some(session_identity(target.as_ref().unwrap()).await?.as_str()) {
                return Err(AppError::conflict("目标服务端身份已变化，请重新准备迁移"));
            }
        }
    sink.store.transfer_create_run(run_id, sink.execution_id, workspace, &digest(&prepared.spec)?, &TransferRunContext {
        plan_id: Some(prepared.id), plan_revision: Some(prepared.revision), plan_digest: Some(prepared.digest.clone()),
        source_profile: prepared.spec.source.as_ref().and_then(|endpoint| endpoint.profile_id),
        destination_profile: prepared.spec.destination.as_ref().and_then(|endpoint| endpoint.profile_id),
        inputs: prepared.inputs.iter().map(|input| TransferInputIdentity { path: input.path.clone(), bytes: input.bytes, sha256: input.sha256.clone(), identity: input.identity.clone(), modified_ns: input.modified_ns.clone() }).collect(),
    }, retry_of).await?;
    let mut journal = TransferJournal::new(sink.clone(), run_id);
    journal.retry_of = retry_of;
    journal.retry_states = prepared.retry_states.clone();
    journal.progress.total_bytes = if prepared.inputs.is_empty() { None } else { Some(prepared.inputs.iter().map(|input| input.bytes).sum()) };
    let result = match (prepared.spec.provider, prepared.spec.mode) {
        (ProviderKind::Mysql, TransferMode::SqlDump) => match source.as_mut() {
            Some(ProviderSession::Mysql(mysql)) => mysql.dump_sql(prepared, &mut journal, cancel).await,
            _ => Err(AppError::invalid("SQL 导出需要 MySQL 源连接")),
        },
        (ProviderKind::Mysql, TransferMode::SqlRestore) => match target.as_mut() {
            Some(ProviderSession::Mysql(mysql)) => mysql.restore_sql(prepared, &mut journal, cancel).await,
            _ => Err(AppError::invalid("SQL 恢复需要 MySQL 目标连接")),
        },
        (ProviderKind::Mysql, TransferMode::Copy) => match (source.as_mut(), target.as_mut()) {
            (Some(ProviderSession::Mysql(source)), Some(ProviderSession::Mysql(target))) => super::mysql::copy(prepared, source, target, &mut journal, cancel).await,
            _ => Err(AppError::invalid("直接迁移需要 MySQL 源和目标连接")),
        },
        (ProviderKind::Mysql, TransferMode::Import) => match target.as_mut() {
            Some(ProviderSession::Mysql(mysql)) => super::files::import_mysql(prepared, mysql, &mut journal, cancel).await,
            _ => Err(AppError::invalid("导入需要 MySQL 目标连接")),
        },
        (ProviderKind::Mysql, TransferMode::Export) => match source.as_mut() {
            Some(ProviderSession::Mysql(mysql)) => super::files::export_mysql(prepared, mysql, &mut journal, cancel).await,
            _ => Err(AppError::invalid("导出需要 MySQL 源连接")),
        },
        (ProviderKind::Redis, _) => {
            let source = match source.as_mut() { Some(ProviderSession::Redis(redis)) => Some(redis.as_mut()), _ => None };
            let target = match target.as_mut() { Some(ProviderSession::Redis(redis)) => Some(redis.as_mut()), _ => None };
            super::redis::execute(&prepared.spec, source, target, &prepared.inputs, &mut journal, cancel).await
        }
    };
    // provider 未能记录终态时，持久 sent 不能被任务错误猜成已回滚。
    let mut after = None;
    // 续作可能继承祖先任务的未知结果，当前步骤全部成功也不能消除它。
    let mut unknown = journal.progress.outcome_unknown > 0;
    loop {
        let items = sink.store.transfer_items(run_id, after, 1000).await?;
        if items.is_empty() { break; }
        for mut item in items {
            after = Some(item.ordinal);
            if item.state == TransferStepState::Sent {
                item.state = TransferStepState::OutcomeUnknown;
                item.error_code = result.as_ref().err().map(|error| error.code.clone());
                sink.store.transfer_put_step(run_id, &item).await?;
                journal.progress.outcome_unknown += 1;
                unknown = true;
            } else if item.state == TransferStepState::OutcomeUnknown { unknown = true; }
        }
    }
    let state = if unknown { "outcome_unknown" } else { match &result { Ok(()) => "succeeded", Err(error) if error.code == "cancelled" => "cancelled", Err(_) => "failed" } };
    journal.terminal(state).await?;
    match result {
        Ok(()) if !unknown => Ok(journal.progress),
        Ok(()) => Err(AppError::unknown("存在未确认的迁移步骤，请人工核对").with_details(json!({"run_id":run_id,"progress":journal.progress}))),
        Err(mut error) => {
            if unknown { error.code = "outcome_unknown".into(); }
            error.details = Box::new(json!({"run_id":run_id,"state":state,"progress":journal.progress,"cause":error.details}));
            Err(error)
        }
    }
    }.await;
    for session in [source.as_mut(), target.as_mut()].into_iter().flatten() {
        // 专属直连不回池；关闭等待有界，防止取消之后仍阻塞 Engine 收尾。
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.close()).await;
    }
    sink.store.release_auxiliary(reservation).await?;
    execution
}
