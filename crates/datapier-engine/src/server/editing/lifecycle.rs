//! 释放失去客户端或超过空闲期限的运行期编辑基线。
use super::*;

impl Engine {
    pub(in crate::server) async fn editing_disconnect(&self, owner: Uuid) -> Result<()> {
        let mut failure = self.disconnect_workspace_uploads(owner).await.err();
        self.editing_forget_scopes(owner);
        if let Err(error) = self.editing_disconnect_values(owner).await {
            failure.get_or_insert(error);
        }
        if let Err(error) = self.editing_disconnect_files(owner).await {
            failure.get_or_insert(error);
        }
        let handles = {
            let mut state = self.state.lock().unwrap();
            let mut handles = Vec::new();
            state.editing.schemas.retain(|id, plan| {
                if plan.owner == owner {
                    handles.push(*id);
                    false
                } else {
                    true
                }
            });
            state.editing.replacements.retain(|_, plan| {
                if plan.owner == owner {
                    handles.push(plan.budget_handle);
                    false
                } else {
                    true
                }
            });
            handles
        };
        for handle in handles {
            if let Err(error) = self.store.release_auxiliary(handle).await {
                failure.get_or_insert(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(in crate::server) async fn editing_sweep(&self) -> Result<()> {
        let idle = Duration::from_secs(self.store.settings().await?.limits.result_idle_seconds);
        self.editing_prune_coordination();
        self.sweep_workspace_uploads(idle).await?;
        self.editing_expire_values(idle).await?;
        let (schemas, replacements) = {
            let state = self.state.lock().unwrap();
            let eligible = |execution: Option<Uuid>, touched: Instant| {
                touched.elapsed() >= idle
                    && execution.is_none_or(|id| !state.tasks.contains_key(&id))
            };
            let schemas: Vec<_> = state
                .editing
                .schemas
                .iter()
                .filter(|(_, plan)| eligible(plan.execution, plan.touched))
                .map(|(&id, plan)| (id, plan.gate.clone()))
                .collect();
            let replacements: Vec<_> = state
                .editing
                .replacements
                .iter()
                .filter(|(_, plan)| eligible(plan.execution, plan.touched))
                .map(|(&id, plan)| (id, plan.gate.clone()))
                .collect();
            (schemas, replacements)
        };
        for (id, gate) in schemas {
            let Ok(_guard) = gate.try_lock() else {
                continue;
            };
            let removed = {
                let mut state = self.state.lock().unwrap();
                if state.editing.schemas.get(&id).is_some_and(|plan| {
                    plan.touched.elapsed() >= idle
                        && plan
                            .execution
                            .is_none_or(|task| !state.tasks.contains_key(&task))
                }) {
                    state.editing.schemas.remove(&id).is_some()
                } else {
                    false
                }
            };
            if removed {
                self.store.release_auxiliary(id).await?;
            }
        }
        for (id, gate) in replacements {
            let Ok(_guard) = gate.try_lock() else {
                continue;
            };
            let binding = {
                let state = self.state.lock().unwrap();
                state
                    .editing
                    .replacements
                    .get(&id)
                    .filter(|plan| {
                        plan.touched.elapsed() >= idle
                            && plan
                                .execution
                                .is_none_or(|task| !state.tasks.contains_key(&task))
                    })
                    .map(|plan| (plan.owner, plan.session_id, plan.budget_handle))
            };
            let Some((owner, session_id, budget)) = binding else {
                continue;
            };
            let database = self
                .state
                .lock()
                .unwrap()
                .databases
                .get(&session_id)
                .cloned();
            if let Some(db) = database {
                let Ok(mut provider) = db.session.try_lock() else {
                    continue;
                };
                let sink = ExecutionSink::new(self.store.clone(), Uuid::nil());
                provider
                    .call(
                        "replace.discard",
                        json!({"plan_id":id}),
                        &sink,
                        &CancellationToken::new(),
                    )
                    .await?;
            }
            self.state.lock().unwrap().editing.replacements.remove(&id);
            self.store.release_auxiliary(budget).await?;
            self.emit_editing(owner, "edit_plan.changed", id, 0, "expired");
        }
        Ok(())
    }
}
