//! 变更预览不写执行历史；高级值回填复用 Engine 冻结的完整行基线。
use super::*;
use datapier_core::model::Value as TypedValue;
use std::collections::BTreeSet;

#[derive(serde::Deserialize)]
struct ValueSource {
    value_session: Uuid,
    revision: u64,
    row_index: usize,
    column: String,
}

impl Engine {
    pub(in crate::server) async fn prepare_change_value_sources(
        &self,
        db: &Database,
        params: &mut Value,
    ) -> Result<Vec<Uuid>> {
        let sources: Vec<ValueSource> = optional(params, "value_sources")?.unwrap_or_default();
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        let session_id: Uuid = model::param(params, "session_id")?;
        if db.profile.provider != model::ProviderKind::Mysql {
            return Err(AppError::invalid("行基线只能用于 MySQL"));
        }
        let target = params
            .get("target")
            .cloned()
            .ok_or_else(|| AppError::invalid("变更缺少目标"))?;
        let rows = params
            .get_mut("rows")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| AppError::invalid("变更缺少行"))?;
        let mut handles = Vec::new();
        let result = async {
            let mut cells = BTreeSet::new();
            let mut checked_rows = BTreeSet::new();
            for source in sources {
                if !cells.insert((source.row_index, source.column.clone())) {
                    return Err(AppError::invalid("值来源重复"));
                }
                let row = rows
                    .get_mut(source.row_index)
                    .ok_or_else(|| AppError::invalid("值来源行不存在"))?;
                if row.get("operation").and_then(Value::as_str) != Some("update")
                    || row
                        .get("values")
                        .and_then(|values| values.get(&source.column))
                        .is_none()
                {
                    return Err(AppError::conflict("值来源与更新列不匹配"));
                }
                let (before, key, handle) = self
                    .editing_staged_mysql_baseline(
                        db.owner,
                        source.value_session,
                        source.revision,
                        session_id,
                        &target,
                        &source.column,
                    )
                    .await?;
                handles.push(handle);
                let mut supplied_key = row
                    .get("key")
                    .cloned()
                    .ok_or_else(|| AppError::invalid("更新行缺少可靠键"))?;
                let fields = supplied_key
                    .as_object_mut()
                    .ok_or_else(|| AppError::invalid("可靠键无效"))?;
                for value in fields.values_mut() {
                    if value.get("type").and_then(Value::as_str) == Some("large") {
                        let typed: TypedValue = serde_json::from_value(value.clone())?;
                        if let TypedValue::Large { id, .. } = typed {
                            let result_id = self.store.value_result_id(id).await?;
                            let meta = self.owned_result(db.owner, result_id).await?;
                            if self
                                .state
                                .lock()
                                .unwrap()
                                .result_sessions
                                .get(&meta.execution_id)
                                != Some(&session_id)
                            {
                                return Err(AppError::new(
                                    "permission_denied",
                                    "可靠键不属于当前数据库会话",
                                ));
                            }
                            *value = serde_json::to_value(self.store.read_value(id).await?)?;
                        }
                    }
                }
                if supplied_key != key {
                    return Err(AppError::conflict("值来源与更新行可靠键不匹配"));
                }
                if !checked_rows.insert(source.row_index) && row.get("before") != Some(&before) {
                    return Err(AppError::conflict("同一行的高级值来自不同基线"));
                }
                row["before"] = before;
                row["key"] = key;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            for handle in handles {
                self.store.release_auxiliary(handle).await?;
            }
            return Err(error);
        }
        Ok(handles)
    }

    pub(super) async fn preview_changes(&self, owner: Uuid, mut params: Value) -> Result<Value> {
        self.ensure_editing_client(owner)?;
        let db = self.database(owner, &params)?;
        db.profile
            .capabilities()
            .ensure_method("change.preview", &params)?;
        let cancel = self.editing_cancel(owner)?;
        let mut budgets = self.prepare_change_value_sources(&db, &mut params).await?;
        let result = async {
            let gate = (db.profile.provider == model::ProviderKind::Mysql && !db.profile.is_archery()).then(|| self.editing_gate(db.profile.id));
            let _guard = match gate.as_ref() {
                Some(gate) => Some(tokio::select! { _ = cancel.cancelled() => return Err(AppError::cancelled()), guard = gate.lock() => guard }),
                None => None,
            };
            let mut provider = tokio::select! {
                _ = cancel.cancelled() => return Err(AppError::cancelled()),
                provider = db.session.lock() => provider,
            };
            self.editing_apply_invalidation(model::param(&params, "session_id")?, &mut provider);
            if let ProviderSession::Mysql(mysql) = &*provider {
                let token: Uuid = model::param(&params, "token")?;
                let target = mysql.draft_target(token)?;
                Self::validate_submit_target(target, params.get("target"))?;
                params["target"] = serialize(target)?;
            }
            if let Some(handle) = self.prepare_change_files(&db, &mut params).await? {
                budgets.push(handle);
            }
            let sink = ExecutionSink::new(self.store.clone(), Uuid::nil());
            provider
                .call("change.preview", params, &sink, &cancel)
                .await
        }
        .await;
        for handle in budgets {
            self.store.release_auxiliary(handle).await?;
        }
        let preview = result?;
        if serde_json::to_vec(&preview)?.len() > self.store.settings().await?.limits.response_bytes
        {
            return Err(AppError::new(
                "resource_limit",
                "变更预览超过响应预算，请减少条目",
            ));
        }
        Ok(preview)
    }
}
