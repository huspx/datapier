//! 将 Engine 确认的草稿文件登记合并到仍在使用的本地草稿。
use super::*;
use datapier_core::model::{RowChange, Value as DataValue};
use grid::EditFileRef;

fn descriptor(value: &Value) -> Option<EditFileRef> {
    Some(EditFileRef {
        edit_file_id: Uuid::parse_str(value.get("edit_file_id")?.as_str()?).ok()?,
        edit_file_revision: value.get("edit_file_revision")?.as_u64()?,
    })
}

fn collect_files(old: &Value, new: &Value, files: &mut BTreeMap<EditFileRef, EditFileRef>) {
    if old.get("type").and_then(Value::as_str) == Some("json") {
        return;
    }
    if let (Some(a), Some(b)) = (descriptor(old), descriptor(new)) {
        files.insert(a, b);
    }
    match (old, new) {
        (Value::Object(a), Value::Object(b)) => {
            for (key, value) in a {
                if let Some(other) = b.get(key) {
                    collect_files(value, other, files);
                }
            }
        }
        (Value::Array(a), Value::Array(b)) => {
            for (value, other) in a.iter().zip(b) {
                collect_files(value, other, files);
            }
        }
        _ => {}
    }
}

fn rebind_json(value: &mut Value, files: &BTreeMap<EditFileRef, EditFileRef>) {
    if value.get("type").and_then(Value::as_str) == Some("json") {
        return;
    }
    if let Some(old) = descriptor(value)
        && let Some(new) = files.get(&old)
    {
        value["edit_file_id"] = json!(new.edit_file_id);
        value["edit_file_revision"] = json!(new.edit_file_revision);
    }
    if value.get("type").and_then(Value::as_str) == Some("large") {
        if let Some(id) = value.pointer_mut("/value/id")
            && let Some(new) = id
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok())
                .and_then(|id| {
                    files
                        .iter()
                        .find(|(old, _)| old.edit_file_id == id)
                        .map(|(_, new)| new.edit_file_id)
                })
        {
            *id = json!(new);
        }
        return;
    }
    match value {
        Value::Object(map) => {
            for item in map.values_mut() {
                rebind_json(item, files);
            }
        }
        Value::Array(items) => {
            for item in items {
                rebind_json(item, files);
            }
        }
        _ => {}
    }
}

fn rebind_value(
    value: &mut DataValue,
    current: &[EditFileRef],
    files: &BTreeMap<EditFileRef, EditFileRef>,
) {
    if let DataValue::Large { id, .. } = value
        && let Some(new) = current
            .iter()
            .find(|file| file.edit_file_id == *id)
            .and_then(|file| files.get(file))
    {
        *id = new.edit_file_id;
    }
}

fn row(value: &Value) -> Option<RowChange> {
    let mut value = value.clone();
    value
        .as_object_mut()?
        .entry("before")
        .or_insert_with(|| json!({}));
    serde_json::from_value(value).ok()
}

fn merge_rows(
    rows: &mut [RowChange],
    files: &mut Vec<EditFileRef>,
    pairs: &[(RowChange, RowChange)],
    canonical_files: &[EditFileRef],
) {
    for current in rows {
        for (old, new) in pairs {
            if current.operation != old.operation || current.key != old.key {
                continue;
            }
            for (column, previous) in &old.values {
                let Some(next) = new.values.get(column) else {
                    continue;
                };
                if current.values.get(column) != Some(previous) || previous == next {
                    continue;
                }
                let DataValue::Large { id, .. } = next else {
                    continue;
                };
                let Some(file) = canonical_files.iter().find(|file| file.edit_file_id == *id)
                else {
                    continue;
                };
                current.values.insert(column.clone(), next.clone());
                files.push(*file);
            }
            break;
        }
    }
    files.sort();
    files.dedup();
}

fn merge_redis(current: &mut [Value], old: &[Value], new: &[Value]) {
    for (index, (before, after)) in old.iter().zip(new).enumerate() {
        let Some(change) = current.get_mut(index) else {
            continue;
        };
        if change.get("operation") != before.get("operation") {
            continue;
        }
        for name in ["field", "member", "new_member", "value"] {
            let (Some(previous), Some(next)) = (before.get(name), after.get(name)) else {
                continue;
            };
            if previous == next
                || change.get(name) != Some(previous)
                || next.get("type").and_then(Value::as_str) != Some("large")
            {
                continue;
            }
            let identity = name == "field"
                || (name == "member"
                    && matches!(
                        before.get("operation").and_then(Value::as_str),
                        Some(
                            "set_remove"
                                | "set_replace_member"
                                | "zset_remove"
                                | "zset_replace_member"
                        )
                    ));
            let descriptor_name = if identity {
                Some("identity_edit_file")
            } else {
                None
            };
            if let Some(key) = descriptor_name {
                if let Some(file) = after.get(key) {
                    change[key] = file.clone();
                }
            } else {
                for key in ["edit_file_id", "edit_file_revision"] {
                    if let Some(file) = after.get(key) {
                        change[key] = file.clone();
                    }
                }
            }
            change[name] = next.clone();
        }
    }
}

impl App {
    /// 副本确认前检查所有标签的异步编辑，避免切换时丢弃仍由旧工作区拥有的会话。
    pub(super) fn workspace_copy_has_active_editors(&self) -> bool {
        fn active(
            value: &advanced::value::ValueUi,
            file: &advanced::files::FileUi,
            schema: &Option<advanced::schema::SchemaEditor>,
            replace: &advanced::replace::ReplaceState,
            busy: bool,
        ) -> bool {
            value.session.is_some()
                || value.pending
                || value.request.is_some()
                || file.file_session.is_some()
                || file.pending
                || file.terminal_handoff.is_some()
                || schema.as_ref().is_some_and(|state| {
                    state.has_pending_request() || state.connect_nonce.is_some()
                })
                || replace.has_live_plan()
                || busy
        }
        active(
            &self.advanced_value,
            &self.advanced_files,
            &self.advanced_schema,
            &self.advanced_replace,
            self.task.is_some()
                || self.operation_pending
                || self.restore_rows.is_some()
                || self.file_open_pending.is_some()
                || self.workbench.inline.is_some(),
        ) || self.tabs.iter().enumerate().any(|(index, tab)| {
            if index == self.active {
                return false;
            }
            let state = &tab.runtime;
            active(
                &state.advanced_value,
                &state.advanced_files,
                &state.advanced_schema,
                &state.advanced_replace,
                state.task.is_some()
                    || state.operation_pending
                    || state.restore_rows.is_some()
                    || state.file_open_pending.is_some()
                    || state.workbench.inline.is_some(),
            )
        })
    }

    /// 只合并已发送快照所确认的引用；迟到响应不得覆盖之后发生的编辑。
    pub(super) fn rebind_workspace_drafts(
        &mut self,
        saved_snapshot: &Value,
        canonical_snapshot: &Value,
        is_copy: bool,
    ) {
        let mut files = BTreeMap::new();
        collect_files(saved_snapshot, canonical_snapshot, &mut files);
        let (Some(old_tabs), Some(new_tabs)) = (
            saved_snapshot.get("tabs").and_then(Value::as_array),
            canonical_snapshot.get("tabs").and_then(Value::as_array),
        ) else {
            return;
        };
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let id = json!(tab.id);
            let Some(old) = old_tabs
                .iter()
                .find(|item| item.get("id") == Some(&id))
                .and_then(|item| item.get("pending"))
            else {
                continue;
            };
            let Some(new) = new_tabs
                .iter()
                .find(|item| item.get("id") == Some(&id))
                .and_then(|item| item.get("pending"))
            else {
                continue;
            };
            let pairs: Vec<_> = old
                .get("rows")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .zip(
                    new.get("rows")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten(),
                )
                .filter_map(|(a, b)| Some((row(a)?, row(b)?)))
                .collect();
            let canonical_files: Vec<EditFileRef> = new
                .get("edit_files")
                .cloned()
                .and_then(|v| serde_json::from_value(v).ok())
                .unwrap_or_default();
            let (grid, rows, pending_files, redis, schema, value_ui) = if index == self.active {
                (
                    &mut self.grid,
                    &mut self.pending_rows,
                    &mut self.pending_edit_files,
                    &mut self.redis_changes,
                    &self.schema,
                    &mut self.advanced_value,
                )
            } else {
                (
                    &mut tab.runtime.grid,
                    &mut tab.runtime.pending_rows,
                    &mut tab.runtime.pending_edit_files,
                    &mut tab.runtime.redis_changes,
                    &tab.runtime.schema,
                    &mut tab.runtime.advanced_value,
                )
            };
            for row in rows.iter_mut() {
                for value in row.values.values_mut() {
                    rebind_value(value, pending_files, &files);
                }
            }
            for file in pending_files.iter_mut() {
                if let Some(new) = files.get(file) {
                    *file = *new;
                }
            }
            if let Some(grid) = grid {
                grid.rebind_draft_files(&files);
                let keys: Vec<String> = schema
                    .as_ref()
                    .and_then(|s| s.get("keys"))
                    .and_then(Value::as_array)
                    .and_then(|keys| keys.first())
                    .and_then(|v| serde_json::from_value(v.clone()).ok())
                    .unwrap_or_default();
                grid.normalize_draft_rows(&keys, &pairs, &canonical_files);
            }
            merge_rows(rows, pending_files, &pairs, &canonical_files);
            for change in redis.iter_mut() {
                rebind_json(change, &files);
            }
            if let (Some(a), Some(b)) = (
                old.get("redis").and_then(Value::as_array),
                new.get("redis").and_then(Value::as_array),
            ) {
                merge_redis(redis, a, b);
            }
            if let Some(source) = &mut value_ui.source {
                rebind_json(source, &files);
            }
            if tab.pending == *old {
                tab.pending = new.clone();
            } else {
                rebind_json(&mut tab.pending, &files);
            }
        }
        if is_copy {
            // 新工作区不能使用旧工作区的数据库基线。先捕获当前草稿，再清理运行态；
            // 迟到响应由 generation 拒绝，用户须重新读取并确认后才能提交。
            let snapshot = self.workspace_snapshot();
            for (tab, saved) in self.tabs.iter_mut().zip(snapshot.tabs) {
                tab.pending = saved.pending;
                tab.pending_live = false;
                if let Some(grid) = &mut tab.runtime.grid {
                    grid.discard();
                }
                tab.runtime.pending_rows.clear();
                tab.runtime.pending_edit_files.clear();
                tab.runtime.redis_changes.clear();
                tab.runtime.edit_token = None;
                tab.runtime.restore_rows = None;
                tab.runtime.preview = None;
                tab.runtime.preview_pending = None;
                tab.runtime.generation = tab.runtime.generation.wrapping_add(1);
                tab.runtime.advanced_value.writable = false;
                tab.runtime.advanced_files.file_session = None;
            }
            if let Some(grid) = &mut self.grid {
                grid.discard();
            }
            self.pending_rows.clear();
            self.pending_edit_files.clear();
            self.redis_changes.clear();
            self.edit_token = None;
            self.restore_rows = None;
            self.preview = None;
            self.preview_pending = None;
            self.generation = self.generation.wrapping_add(1);
            self.advanced_value.writable = false;
            self.advanced_files.file_session = None;
        }
    }
}
