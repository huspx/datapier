//! 复合类型选中行的冻结确认、完整读取与原子暂存。
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::{AccessMode, Value as DataValue};

type RedisSelectedRow = (u64, Vec<DataValue>);

#[derive(Clone)]
pub(super) struct RedisRowPending {
    tab: Uuid,
    generation: u64,
    profile: Uuid,
    session: Value,
    target: ResourceTarget,
    database: Option<String>,
    result: Uuid,
    result_id: Option<String>,
    schema: Value,
    token: Value,
    revision: u64,
    rows: Vec<RedisSelectedRow>,
    resolved: Option<Vec<Vec<DataValue>>>,
    error: Option<String>,
    tag: Option<String>,
}

impl RedisRowPending {
    fn matches(&self, app: &App) -> bool {
        app.is_redis()
            && !app.operation_pending
            && app.task.is_none()
            && app.browse_result
            && app.page_token.is_none()
            && app
                .tabs
                .get(app.active)
                .is_some_and(|t| t.id == self.tab && (t.pending.is_null() || t.pending_live))
            && app.generation == self.generation
            && app
                .profile
                .as_ref()
                .is_some_and(|p| p.id == self.profile && p.access_mode == AccessMode::ReadWrite)
            && app.session.as_ref() == Some(&self.session)
            && app.target.as_ref().is_some_and(|t| {
                t.name == self.target.name
                    && t.kind == self.target.kind
                    && t.database == self.target.database
                    && t.key_b64 == self.target.key_b64
            })
            && app.database == self.database
            && app.result_id == self.result_id
            && app.schema.as_ref() == Some(&self.schema)
            && app.edit_token.as_ref() == Some(&self.token)
            && app.redis_changes_revision == self.revision
            && app.grid.as_ref().is_some_and(|g| {
                g.page.result.id == self.result
                    && self.rows.iter().all(|(i, row)| {
                        g.row_visible(*i) && g.original_row(*i) == Some(row.as_slice())
                    })
            })
    }
}

impl App {
    pub(super) fn redis_row_delete_resolution_pending(&self) -> bool {
        self.workbench
            .redis_row_pending
            .as_ref()
            .is_some_and(|pending| pending.tag.is_some())
    }

    pub(super) fn redis_row_delete_enabled(&self) -> bool {
        if self
            .tabs
            .get(self.active)
            .is_some_and(|tab| self.redis_key_delete_tab_pending(tab.id))
        {
            return false;
        }
        self.is_redis()
            && !self.operation_pending
            && self.task.is_none()
            && !self.redis_row_delete_resolution_pending()
            && self.browse_result
            && self.page_token.is_none()
            && self
                .profile
                .as_ref()
                .is_some_and(|p| p.access_mode == AccessMode::ReadWrite)
            && self.session.is_some()
            && self.target.is_some()
            && self.edit_token.is_some()
            && self
                .tabs
                .get(self.active)
                .is_some_and(|t| t.pending.is_null() || t.pending_live)
            && self
                .schema
                .as_ref()
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str)
                .is_some_and(|kind| matches!(kind, "hash" | "list" | "set" | "zset"))
            && self
                .grid
                .as_ref()
                .is_some_and(|g| g.selected_row_count() > 0)
    }

    pub(super) fn redis_row_delete_targets(
        &self,
    ) -> Option<(&ResourceTarget, &[RedisSelectedRow])> {
        self.workbench
            .redis_row_pending
            .as_ref()
            .map(|pending| (&pending.target, pending.rows.as_slice()))
    }
    pub(super) fn redis_row_delete_resolved(&self) -> Option<&[Vec<DataValue>]> {
        self.workbench
            .redis_row_pending
            .as_ref()?
            .resolved
            .as_deref()
    }

    pub(super) fn redis_row_delete_error(&self) -> Option<&str> {
        self.workbench.redis_row_pending.as_ref()?.error.as_deref()
    }

    pub(super) fn load_redis_row_delete_targets(&mut self, backend: &Backend) {
        let Some(mut pending) = self.workbench.redis_row_pending.take() else {
            return;
        };
        if pending.tag.is_some() || !pending.matches(self) {
            self.workbench.redis_row_pending = Some(pending);
            return;
        }
        let tag = format!("redis.rows.preview/{}", Uuid::new_v4());
        let routed = format!("{}:{}:{tag}", pending.tab, pending.generation);
        let rows = pending.rows.iter().map(|(_, row)| row.clone()).collect();
        pending.error = None;
        match backend.redis_selected_rows(routed, rows, None) {
            Ok(()) => pending.tag = Some(tag),
            Err(error) => pending.error = Some(error.to_string()),
        }
        self.workbench.redis_row_pending = Some(pending);
    }

    pub(super) fn redis_row_delete_count(&self) -> usize {
        self.workbench
            .redis_row_pending
            .as_ref()
            .map_or(0, |p| p.rows.len())
    }

    pub(super) fn request_redis_row_delete(&mut self) {
        if !self.redis_row_delete_enabled() {
            self.status = i18n::UiMessage::key("请选择可删除的 Redis 条目并打开编辑基线");
            return;
        }
        self.workbench.redis_row_pending = None;
        let grid = self.grid.as_ref().unwrap();
        let rows: Option<Vec<_>> = grid
            .selected_rows()
            .into_iter()
            .map(|i| Some((i, grid.original_row(i)?.to_vec())))
            .collect();
        let Some(rows) = rows.filter(|rows| !rows.is_empty()) else {
            self.status = i18n::UiMessage::key("选中行已失效，请重新选择");
            return;
        };
        self.workbench.redis_row_pending = Some(RedisRowPending {
            tab: self.tabs[self.active].id,
            generation: self.generation,
            profile: self.profile.as_ref().unwrap().id,
            session: self.session.clone().unwrap(),
            target: self.target.clone().unwrap(),
            database: self.database.clone(),
            result: grid.page.result.id,
            result_id: self.result_id.clone(),
            schema: self.schema.clone().unwrap(),
            token: self.edit_token.clone().unwrap(),
            revision: self.redis_changes_revision,
            rows,
            resolved: None,
            error: None,
            tag: None,
        });
        self.workbench.overlay = Some("redis-delete-rows".into());
    }

    pub(super) fn cancel_redis_row_delete(&mut self) {
        self.workbench.redis_row_pending = None;
        if self.workbench.overlay.as_deref() == Some("redis-delete-rows") {
            self.workbench.overlay = None;
        }
    }

    pub(super) fn confirm_redis_row_delete(&mut self, backend: &Backend) {
        if self.workbench.overlay.as_deref() != Some("redis-delete-rows") {
            return;
        }
        if self.redis_row_delete_resolution_pending() {
            return;
        }
        self.workbench.overlay = None;
        let Some(mut pending) = self.workbench.redis_row_pending.take() else {
            return;
        };
        if !pending.matches(self) {
            self.status = i18n::UiMessage::key("Redis 上下文已变化，请重新选择条目");
            return;
        }
        let tag = format!("redis.rows.delete/{}", Uuid::new_v4());
        let routed = format!("{}:{}:{tag}", pending.tab, pending.generation);
        let rows = pending.rows.iter().map(|(_, row)| row.clone()).collect();
        match backend.redis_selected_rows(routed, rows, None) {
            Ok(()) => {
                pending.tag = Some(tag);
                self.workbench.redis_row_pending = Some(pending);
                self.status = i18n::UiMessage::key("正在读取完整 Redis 条目…");
            }
            Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
        }
    }

    pub(super) fn redis_row_delete_response(
        &mut self,
        tag: &str,
        result: &datapier_core::Result<Value>,
    ) -> bool {
        let preview = tag.starts_with("redis.rows.preview/");
        if !preview && !tag.starts_with("redis.rows.delete/") {
            return false;
        }
        if self
            .workbench
            .redis_row_pending
            .as_ref()
            .and_then(|p| p.tag.as_deref())
            != Some(tag)
        {
            return true;
        }
        let mut pending = self.workbench.redis_row_pending.take().unwrap();
        if !pending.matches(self) {
            self.status = i18n::UiMessage::key("Redis 上下文已变化，请重新选择条目");
            return true;
        }
        let rows: Vec<Vec<DataValue>> = match result {
            Ok(value) => match serde_json::from_value(value.clone()) {
                Ok(rows) => rows,
                Err(error) => {
                    self.status = i18n::UiMessage::raw(error.to_string());
                    if preview {
                        pending.tag = None;
                        pending.error = Some(error.to_string());
                        self.workbench.redis_row_pending = Some(pending);
                    }
                    return true;
                }
            },
            Err(error) => {
                self.status = i18n::UiMessage::raw(error.to_string());
                if preview {
                    pending.tag = None;
                    pending.error = Some(error.to_string());
                    self.workbench.redis_row_pending = Some(pending);
                }
                return true;
            }
        };
        if preview {
            pending.tag = None;
            pending.resolved = Some(rows);
            pending.error = None;
            self.workbench.redis_row_pending = Some(pending);
            return true;
        }
        let staged = self.redis_row_deletions(&pending, rows);
        match staged {
            Ok(changes) => {
                let start = self.redis_changes.len();
                self.redis_changes.extend(changes);
                self.redis_changes_revision = self.redis_changes_revision.wrapping_add(1);
                self.draft_revision = self.draft_revision.wrapping_add(1);
                self.workbench
                    .redis_undo_groups
                    .retain(|&(_, end)| end <= start);
                self.workbench
                    .redis_undo_groups
                    .push((start, self.redis_changes.len()));
                self.detail = serde_json::to_string_pretty(&self.redis_changes).unwrap_or_default();
                self.set_detail_context("草稿详情");
                self.dirty = true;
            }
            Err(error) => self.status = error,
        }
        true
    }

    // 跟踪已暂存的同身份操作；删除后重新添加时，条目再次可删除。
    fn redis_row_identity_present(
        &self,
        kind: &str,
        identity: &[u8],
    ) -> std::result::Result<bool, i18n::UiMessage> {
        let invalid = || i18n::UiMessage::key("已暂存 Redis 条目身份无效");
        let (field, add, remove) = match kind {
            "hash" => ("field", "hash_set", "hash_delete"),
            "set" => ("member", "set_add", "set_remove"),
            "zset" => ("member", "zset_set", "zset_remove"),
            _ => return Err(invalid()),
        };
        let mut present = true;
        for change in &self.redis_changes {
            let op = change
                .get("operation")
                .and_then(Value::as_str)
                .ok_or_else(invalid)?;
            if matches!(op, "delete_key" | "rename") {
                return Err(i18n::UiMessage::key(
                    "已暂存键删除或重命名，不能继续删除条目",
                ));
            }
            if matches!(
                (kind, op),
                ("set", "set_replace_member") | ("zset", "zset_replace_member")
            ) {
                // 大身份需由 Engine 解引用；本地不能用显示摘要判断是否为同一成员。
                let decode = |field: &str| -> std::result::Result<Vec<u8>, i18n::UiMessage> {
                    let value: DataValue =
                        serde_json::from_value(change.get(field).cloned().ok_or_else(invalid)?)
                            .map_err(|_| invalid())?;
                    match value {
                        DataValue::Bytes(encoded) => {
                            STANDARD.decode(encoded).map_err(|_| invalid())
                        }
                        DataValue::Text(text) => Ok(text.into_bytes()),
                        _ => Err(invalid()),
                    }
                };
                let old = decode("member")?;
                let new = decode("new_member")?;
                if old == identity {
                    present = false;
                }
                if new == identity {
                    present = true;
                }
                continue;
            }
            if op != add && op != remove {
                continue;
            }
            let value: DataValue =
                serde_json::from_value(change.get(field).cloned().ok_or_else(invalid)?)
                    .map_err(|_| invalid())?;
            let bytes = match value {
                DataValue::Bytes(encoded) => STANDARD.decode(encoded).map_err(|_| invalid())?,
                DataValue::Text(text) => text.into_bytes(),
                _ => return Err(invalid()),
            };
            if bytes == identity {
                present = op == add;
            }
        }
        Ok(present)
    }

    fn redis_row_deletions(
        &self,
        pending: &RedisRowPending,
        rows: Vec<Vec<DataValue>>,
    ) -> std::result::Result<Vec<Value>, i18n::UiMessage> {
        let invalid = || i18n::UiMessage::key("Redis 选中条目类型无效");
        if rows.len() != pending.rows.len() {
            return Err(invalid());
        }
        let kind = pending
            .schema
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(invalid)?;
        let mut changes = Vec::with_capacity(rows.len());
        let mut identities = std::collections::BTreeSet::new();
        if self.redis_changes.iter().any(|change| {
            matches!(
                change.get("operation").and_then(Value::as_str),
                Some("delete_key" | "rename")
            )
        }) {
            return Err(i18n::UiMessage::key(
                "已暂存键删除或重命名，不能继续删除条目",
            ));
        }
        for row in rows {
            let change = match (kind, row.as_slice()) {
                (
                    "list",
                    [
                        DataValue::Integer(index),
                        DataValue::Bytes(_) | DataValue::Large { .. },
                    ],
                ) => {
                    let original = index.parse::<i64>().map_err(|_| invalid())?;
                    json!({"operation":"list_delete","index":self.redis_list_index(original, false)?})
                }
                (
                    "hash",
                    [
                        DataValue::Bytes(field),
                        DataValue::Bytes(_) | DataValue::Large { .. },
                    ],
                ) => {
                    let identity = STANDARD.decode(field).map_err(|_| invalid())?;
                    if !identities.insert(identity.clone())
                        || !self.redis_row_identity_present(kind, &identity)?
                    {
                        return Err(i18n::UiMessage::key("选中的 Redis 条目已暂存删除"));
                    }
                    json!({"operation":"hash_delete","field":{"type":"bytes","value":field}})
                }
                ("set", [DataValue::Bytes(member)])
                | (
                    "zset",
                    [
                        DataValue::Bytes(member),
                        DataValue::Float(_) | DataValue::Large { .. },
                    ],
                ) => {
                    let identity = STANDARD.decode(member).map_err(|_| invalid())?;
                    if !identities.insert(identity.clone())
                        || !self.redis_row_identity_present(kind, &identity)?
                    {
                        return Err(i18n::UiMessage::key("选中的 Redis 条目已暂存删除"));
                    }
                    json!({"operation":if kind == "set" {"set_remove"} else {"zset_remove"},"member":{"type":"bytes","value":member}})
                }
                _ => return Err(invalid()),
            };
            changes.push(change);
        }
        if kind == "list" {
            changes.sort_by(|a, b| b["index"].as_i64().cmp(&a["index"].as_i64()));
            if changes
                .windows(2)
                .any(|pair| pair[0]["index"] == pair[1]["index"])
            {
                return Err(invalid());
            }
        }
        Ok(changes)
    }
}

#[cfg(test)]
#[path = "../tests/unit/redis_rows.rs"]
mod tests;
