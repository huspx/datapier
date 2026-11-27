use super::*;
use tokio::sync::oneshot;

/// 让持久化收尾独立于请求 future 的生命周期。
pub(super) struct ScopeTask {
    pub(super) id: Uuid,
    result: oneshot::Sender<Result<Value>>,
    finished: oneshot::Receiver<Result<Value>>,
}

impl ScopeTask {
    pub(super) async fn start(
        store: Store,
        owner: Uuid,
        workspace: String,
        params: Value,
    ) -> Result<Self> {
        let (ready_tx, ready_rx) = oneshot::channel();
        let (result_tx, result_rx) = oneshot::channel();
        let (finished_tx, finished_rx) = oneshot::channel();
        tokio::spawn(async move {
            let record = match store
                .create_task(owner, &workspace, "mcp.scope.inspect", params, false)
                .await
            {
                Ok(record) => record,
                Err(error) => {
                    let _ = ready_tx.send(Err(error));
                    return;
                }
            };
            if let Err(error) = store
                .update_task(record.id, "running", Value::Null, None)
                .await
            {
                let _ = store
                    .update_task(record.id, "failed", Value::Null, Some(error.clone()))
                    .await;
                let _ = ready_tx.send(Err(error));
                return;
            }
            let result = if ready_tx.send(Ok(record.id)).is_ok() {
                result_rx
                    .await
                    .unwrap_or_else(|_| Err(AppError::cancelled()))
            } else {
                Err(AppError::cancelled())
            };
            let summary = store.task(record.id).await.map(|task| task.summary);
            let (state, error) = match (&result, &summary) {
                (_, Err(error)) => ("failed", Some(error.clone())),
                (Ok(_), Ok(_)) => ("succeeded", None),
                (Err(error), Ok(_)) if error.code == "cancelled" => {
                    ("cancelled", Some(error.clone()))
                }
                (Err(error), Ok(_)) => ("failed", Some(error.clone())),
            };
            let saved = store
                .update_task(
                    record.id,
                    state,
                    summary.as_ref().cloned().unwrap_or(Value::Null),
                    error,
                )
                .await;
            let finished = match (saved, summary) {
                (Err(error), _) | (_, Err(error)) => Err(error),
                (Ok(()), Ok(_)) => result,
            };
            let _ = finished_tx.send(finished);
        });
        let id = ready_rx
            .await
            .map_err(|_| AppError::new("internal", "对象检查任务意外停止"))??;
        Ok(Self {
            id,
            result: result_tx,
            finished: finished_rx,
        })
    }

    pub(super) async fn finish(self, result: Result<Value>) -> Result<Value> {
        self.result
            .send(result)
            .map_err(|_| AppError::new("internal", "对象检查任务意外停止"))?;
        self.finished
            .await
            .map_err(|_| AppError::new("internal", "对象检查任务意外停止"))?
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/server/scope_task.rs"]
mod tests;
