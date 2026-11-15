//! SQL 输入索引的本机文件归属；数据库写入前必须完成索引封口和登记。
use super::RestoreInput;
use crate::{
    ExecutionSink,
    execution::{
        data_files::{self, AtomicOutput, OutputSnapshot},
        sql_restore::{self, IndexBudget, persist},
    },
    model::{AppError, Result, ensure_not_cancelled, transfer::TransferSpec},
};
use serde_json::{Value as Json, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

fn identity(snapshot: &OutputSnapshot, task_id: Uuid) -> Json {
    json!({
        "path": snapshot.canonical_path,
        "canonical_path": snapshot.canonical_path,
        "file_id": snapshot.file_id,
        "size": snapshot.size,
        "sha256": snapshot.sha256,
        "task_id": task_id,
        "export_name": "sql-input-index.json",
        "media_type": "application/json"
    })
}

pub(super) async fn inputs(
    spec: &TransferSpec,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Vec<RestoreInput>> {
    let reservation = uuid::Uuid::new_v4();
    let store = sink.store.clone();
    let execution_id = sink.execution_id;
    let runtime = tokio::runtime::Handle::current();
    let mut reserved = 0usize;
    let mut budget = IndexBudget::new();
    budget.set_tuple_reserver(Box::new(move |bytes| {
        if bytes > reserved {
            let next = bytes.saturating_add(1024 * 1024 - 1) / (1024 * 1024) * (1024 * 1024);
            runtime.block_on(store.reserve_auxiliary(execution_id, reservation, next))?;
            reserved = next;
        }
        Ok(())
    }));
    let result = inputs_with_budget(spec, sink, cancel, budget).await;
    sink.store.release_auxiliary(reservation).await?;
    result
}

pub(super) async fn inputs_with_budget(
    spec: &TransferSpec,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    mut budget: IndexBudget,
) -> Result<Vec<RestoreInput>> {
    if spec.inputs.is_empty() {
        return Err(AppError::invalid("恢复缺少 SQL 输入"));
    }
    let lease = std::sync::Arc::new(sink.store.input_index_work().await);
    let mut remaining = persist::MAX_INDEX_DISK_BYTES;
    let mut sources = Vec::new();
    for input in &spec.inputs {
        ensure_not_cancelled(cancel)?;
        let root = sink.store.root.join("transfer-index");
        let create_lease = lease.clone();
        let (output, baseline) = tokio::task::spawn_blocking(move || -> Result<_> {
            let _lease = create_lease;
            std::fs::create_dir_all(&root)?;
            // 唯一名称和 create_new 沿用已有 partial 机制；索引不发布为用户备份。
            let mut output = AtomicOutput::create(&root.join(format!("{}.json", Uuid::new_v4())))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    output.partial_path(),
                    std::fs::Permissions::from_mode(0o600),
                )?;
            }
            let initial = output.snapshot(true)?;
            Ok((output, initial))
        })
        .await
        .map_err(|error| AppError::new("io", format!("创建 SQL 索引任务退出: {error}")))??;
        let artifact = match sink
            .register_transfer_artifact("input_index", identity(&baseline, sink.execution_id))
            .await
        {
            Ok(id) => id,
            Err(error) => {
                let path = output.partial_path().to_owned();
                drop(output);
                // 尚未发送给 worker 的空文件仍属当前句柄；核对身份后才可移除。
                if let Ok(current) = data_files::output_snapshot(&path, true)
                    && current.file_id == baseline.file_id
                    && current.canonical_path == baseline.canonical_path
                    && current.sha256 == baseline.sha256
                    && let Err(cleanup) = tokio::fs::remove_file(&path).await
                {
                    tracing::error!(%cleanup, "清理未登记的 SQL 空索引失败");
                }
                return Err(error);
            }
        };
        let path = input.path.clone();
        let encoding = input.csv.encoding.clone();
        let scan_cancel = cancel.clone();
        let worker_lease = lease.clone();
        let (result, snapshot, next_budget, next_remaining) =
            tokio::task::spawn_blocking(move || {
                let _lease = worker_lease;
                let mut output = output;
                let result = (|| {
                    let index = sql_restore::inspect_with_budget(
                        &path,
                        &encoding,
                        &scan_cancel,
                        &mut budget,
                    )?;
                    // Serde 按 token 写入；固定缓冲避免为每个标点发起文件系统调用。
                    persist::write_index(
                        &index,
                        &mut std::io::BufWriter::with_capacity(64 * 1024, &mut output),
                        &scan_cancel,
                        &mut remaining,
                    )?;
                    Ok(RestoreInput {
                        path,
                        index: std::sync::Arc::new(index),
                    })
                })();
                // 失败也封存当前身份；登记失败时原 preparing 身份仍可由领取流程复核。
                let snapshot = output.snapshot(true);
                drop(output);
                (result, snapshot, budget, remaining)
            })
            .await
            .map_err(|error| AppError::new("io", format!("SQL 预检任务退出: {error}")))?;
        budget = next_budget;
        remaining = next_remaining;
        let snapshot = match snapshot {
            Ok(snapshot)
                if snapshot.file_id == baseline.file_id
                    && snapshot.canonical_path == baseline.canonical_path =>
            {
                snapshot
            }
            Ok(_) => return Err(AppError::new("recovery_conflict", "SQL 索引文件身份已变化")),
            Err(error) => {
                // 无法封口时保留 preparing 和原始文件身份，领取时必须重新核对摘要。
                tracing::error!(%artifact, %error, "SQL 索引封口失败");
                return Err(result.err().unwrap_or(error));
            }
        };
        sink.update_transfer_artifact(artifact, identity(&snapshot, sink.execution_id))
            .await?;
        if result.is_err() {
            sink.finish_transfer_artifact(artifact, "cleanup_pending")
                .await?;
        }
        sources.push(result?);
    }
    Ok(sources)
}
