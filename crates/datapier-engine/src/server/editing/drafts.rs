//! 父变更草稿中的单值文件引用：授权先于读取，提交预算覆盖解码和构造参数。
use super::*;
use datapier_core::model::{
    Value as TypedValue,
    editing::{EditFilePurpose, EditFileState},
};
use std::collections::BTreeMap;

#[derive(Clone, Copy)]
enum ChangeValueRead {
    Draft { revision: u64, budget: usize },
    Cached { budget: usize },
}

fn declaration(value: &Value, out: &mut BTreeMap<Uuid, u64>) -> Result<()> {
    if value.get("edit_file_id").is_none() && value.get("edit_file_revision").is_none() {
        return Ok(());
    }
    let id: Uuid = model::param(value, "edit_file_id")?;
    let revision: u64 = model::param(value, "edit_file_revision")?;
    if revision == 0 || out.insert(id, revision).is_some_and(|old| old != revision) {
        return Err(AppError::invalid("草稿文件引用版本不一致"));
    }
    Ok(())
}
fn value_references(value: &Value, out: &mut BTreeMap<Uuid, usize>) -> Result<()> {
    match value {
        Value::Object(fields) if fields.get("type").and_then(Value::as_str) == Some("json") => (),
        Value::Object(fields) if fields.get("type").and_then(Value::as_str) == Some("large") => {
            let body = fields
                .get("value")
                .ok_or_else(|| AppError::invalid("大值引用缺少内容"))?;
            let count = out.entry(model::param(body, "id")?).or_default();
            *count = count
                .checked_add(1)
                .ok_or_else(|| AppError::new("resource_limit", "编辑引用数量溢出"))?;
        }
        Value::Object(fields) => {
            for value in fields.values() {
                value_references(value, out)?;
            }
        }
        Value::Array(values) => {
            for value in values {
                value_references(value, out)?;
            }
        }
        _ => (),
    }
    Ok(())
}
fn charge_reference(bytes: &mut usize, size: usize, count: usize) -> Result<()> {
    *bytes = size
        .checked_mul(count)
        .and_then(|size| bytes.checked_add(size))
        .ok_or_else(|| AppError::new("resource_limit", "编辑引用预算溢出"))?;
    Ok(())
}

impl Engine {
    async fn resolve_change_file_value(
        &self,
        value: &mut Value,
        reads: &BTreeMap<Uuid, ChangeValueRead>,
    ) -> Result<()> {
        if value.get("type").and_then(Value::as_str) != Some("large") {
            return Ok(());
        }
        let id = value
            .pointer("/value/id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
            .ok_or_else(|| AppError::invalid("大值引用标识无效"))?;
        let Some(read) = reads.get(&id) else {
            return Ok(());
        };
        let resolved: TypedValue = match *read {
            ChangeValueRead::Draft { revision, budget } => {
                self.store
                    .read_draft_value_bounded(id, revision, budget)
                    .await?
            }
            ChangeValueRead::Cached { budget } => {
                self.store.read_cached_value_bounded(id, budget).await?
            }
        };
        *value = serde_json::to_value(resolved)?;
        Ok(())
    }

    /// 返回需在 provider 调用结束后释放的预算句柄。
    pub(in crate::server) async fn prepare_change_files(
        &self,
        db: &Database,
        params: &mut Value,
    ) -> Result<Option<Uuid>> {
        let mut declarations = BTreeMap::new();
        if let Some(files) = params.get("edit_files") {
            for file in files
                .as_array()
                .ok_or_else(|| AppError::invalid("edit_files 必须是数组"))?
            {
                declaration(file, &mut declarations)?;
            }
        }
        if let Some(changes) = params.get("changes").and_then(Value::as_array) {
            for change in changes {
                declaration(change, &mut declarations)?;
                for key in [
                    "identity_edit_file",
                    "new_field_edit_file",
                    "new_key_edit_file",
                ] {
                    if let Some(descriptor) = change.get(key) {
                        declaration(descriptor, &mut declarations)?;
                    }
                }
            }
        }
        let mut ids = BTreeMap::new();
        if let Some(rows) = params.get("rows").and_then(Value::as_array) {
            for row in rows {
                for field in ["key", "before", "values"] {
                    if let Some(values) = row.get(field) {
                        value_references(values, &mut ids)?;
                    }
                }
            }
        }
        if let Some(changes) = params.get("changes").and_then(Value::as_array) {
            for change in changes {
                for key in [
                    "value",
                    "field",
                    "member",
                    "new_member",
                    "new_field",
                    "new_key_b64",
                ] {
                    if let Some(value) = change.get(key) {
                        value_references(value, &mut ids)?;
                    }
                }
            }
        }
        if ids.is_empty() && declarations.is_empty() {
            return Ok(None);
        }
        self.ensure_editing_client(db.owner)?;
        let workspace = self
            .state
            .lock()
            .unwrap()
            .contexts
            .get(&db.owner)
            .map(|context| context.workspace.clone())
            .ok_or_else(|| AppError::new("disconnected", "客户端会话已关闭"))?;
        let target = params
            .get("target")
            .ok_or_else(|| AppError::invalid("草稿缺少目标"))?;
        let current = self.store.profile(db.profile.id).await?;
        if current.revision != db.profile.revision {
            return Err(AppError::conflict("连接配置已变化，请重新连接"));
        }
        let session_id: Uuid = model::param(params, "session_id")?;
        let mut reads = BTreeMap::new();
        let mut bytes = 0usize;
        for (id, count) in ids {
            let record = match self.store.edit_file(id).await {
                Ok(record) => record,
                Err(error) if error.code == "not_found" => {
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
                            "字段引用不属于当前数据库会话",
                        ));
                    }
                    let bound = self.store.cached_value_memory_bound(id).await?;
                    charge_reference(&mut bytes, bound, count)?;
                    reads.insert(id, ChangeValueRead::Cached { budget: bound });
                    continue;
                }
                Err(error) => return Err(error),
            };
            let revision = declarations
                .get(&id)
                .copied()
                .ok_or_else(|| AppError::invalid("用户新值缺少文件 revision"))?;
            if record.revision != revision
                || record.purpose != EditFilePurpose::Draft
                || record.state != EditFileState::Ready
                || record.workspace_id != workspace
                || record.profile_id != db.profile.id.to_string()
                || &record.target != target
            {
                return Err(AppError::conflict("草稿文件版本、用途或编辑目标不匹配"));
            }
            if !self.editing_owns_file(db.owner, id)
                && !self
                    .store
                    .edit_file_referenced_in_workspace(id, &workspace)
                    .await?
            {
                return Err(AppError::new(
                    "permission_denied",
                    "草稿文件不属于当前客户端或工作区",
                ));
            }
            let bound = self.store.draft_value_memory_bound(id, revision).await?;
            charge_reference(&mut bytes, bound, count)?;
            reads.insert(
                id,
                ChangeValueRead::Draft {
                    revision,
                    budget: bound,
                },
            );
        }
        if reads.is_empty() {
            return Ok(None);
        }
        let handle = Uuid::new_v4();
        self.store
            .reserve_auxiliary_for_owner(db.owner, handle, bytes)
            .await?;
        let resolved = async {
            if db.profile.provider == model::ProviderKind::Mysql
                && let Some(rows) = params.get_mut("rows").and_then(Value::as_array_mut)
            {
                for row in rows {
                    for field in ["key", "before", "values"] {
                        if let Some(values) = row.get_mut(field).and_then(Value::as_object_mut) {
                            for value in values.values_mut() {
                                self.resolve_change_file_value(value, &reads).await?;
                            }
                        }
                    }
                }
            }
            if db.profile.provider == model::ProviderKind::Redis
                && let Some(changes) = params.get_mut("changes").and_then(Value::as_array_mut)
            {
                for change in changes {
                    for key in [
                        "value",
                        "field",
                        "member",
                        "new_member",
                        "new_field",
                        "new_key_b64",
                    ] {
                        let Some(value) = change.get_mut(key) else {
                            continue;
                        };
                        let was_large = value.get("type").and_then(Value::as_str) == Some("large");
                        self.resolve_change_file_value(value, &reads).await?;
                        if key == "new_key_b64" && was_large {
                            if value.get("type").and_then(Value::as_str) != Some("text") {
                                return Err(AppError::invalid("新 key 草稿必须是文本"));
                            }
                            let text = value
                                .get_mut("value")
                                .map(Value::take)
                                .filter(Value::is_string)
                                .ok_or_else(|| AppError::invalid("新 key 草稿必须是文本"))?;
                            *value = text;
                        }
                    }
                }
            }
            Ok(Some(handle))
        }
        .await;
        if resolved.is_err() {
            self.store.release_auxiliary(handle).await?;
        }
        resolved
    }
}
