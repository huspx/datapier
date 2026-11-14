//! 不可变用户新值文件；数据库结果与原值仍只从运行期注册表读取。
use super::*;
use std::io::{Seek, SeekFrom};

/// 扫描编码而不分配完整副本，计入现有树和两份可能同时存活的编码。
#[derive(Default)]
struct SaveEstimate {
    json: crate::model::editing::JsonMemoryEstimate,
    bytes: usize,
}
impl std::io::Write for SaveEstimate {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.saturating_add(bytes.len());
        self.json.feed(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn save_estimate(value: &Json) -> Result<SaveEstimate> {
    let mut estimate = SaveEstimate::default();
    serde_json::to_writer(&mut estimate, value)?;
    Ok(estimate)
}
fn save_charge(value: &Json) -> Result<usize> {
    let estimate = save_estimate(value)?;
    Ok(estimate
        .json
        .heap_bound()
        .saturating_add(estimate.bytes.saturating_mul(2)))
}
fn check_save_budget(charged: usize, budget: usize) -> Result<()> {
    if charged > budget {
        Err(AppError::new("resource_limit", "工作区保存超过内存预算"))
    } else {
        Ok(())
    }
}

fn has_reference(value: &Value) -> bool {
    match value {
        Value::Large { .. } | Value::Missing => true,
        Value::Array(values) => values.iter().any(has_reference),
        Value::Map(values) => values
            .iter()
            .any(|(key, value)| has_reference(key) || has_reference(value)),
        _ => false,
    }
}
type FileMap = std::collections::BTreeMap<Uuid, (Uuid, u64)>;

fn rewrite_descriptor(value: &mut Json, copies: &FileMap) {
    if let Some(map) = value.as_object_mut()
        && let Some((id, revision)) = map
            .get("edit_file_id")
            .and_then(Json::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
            .and_then(|id| copies.get(&id))
    {
        map.insert("edit_file_id".into(), serde_json::json!(id));
        map.insert("edit_file_revision".into(), serde_json::json!(revision));
    }
}
fn rewrite_values(value: &mut Json, copies: &FileMap) {
    match value {
        Json::Object(map) => {
            if map.get("type").and_then(Json::as_str) == Some("json") && map.contains_key("value") {
                return;
            }
            if map.get("type").and_then(Json::as_str) == Some("large") {
                if let Some(details) = map.get_mut("value").and_then(Json::as_object_mut)
                    && let Some((id, _)) = details
                        .get("id")
                        .and_then(Json::as_str)
                        .and_then(|id| Uuid::parse_str(id).ok())
                        .and_then(|id| copies.get(&id))
                {
                    details.insert("id".into(), serde_json::json!(id));
                }
                return;
            }
            for child in map.values_mut() {
                rewrite_values(child, copies);
            }
        }
        Json::Array(items) => {
            for child in items {
                rewrite_values(child, copies);
            }
        }
        _ => (),
    }
}
fn rewrite_copied_references(value: &mut Json, copies: &FileMap) {
    if let Some(files) = value.get_mut("value_drafts").and_then(Json::as_array_mut) {
        for file in files {
            rewrite_descriptor(file, copies);
        }
    }
    if let Some(tabs) = value.get_mut("tabs").and_then(Json::as_array_mut) {
        for tab in tabs {
            for kind in ["schema_draft", "replace_draft"] {
                if let Some(marker) = tab
                    .get_mut(kind)
                    .and_then(|v| v.get_mut("$datapier_intent"))
                {
                    rewrite_descriptor(marker, copies);
                }
            }
            if let Some(pending) = tab.get_mut("pending") {
                for key in ["edit_files", "value_drafts"] {
                    if let Some(files) = pending.get_mut(key).and_then(Json::as_array_mut) {
                        for file in files {
                            rewrite_descriptor(file, copies);
                        }
                    }
                }
                if let Some(rows) = pending.get_mut("rows").and_then(Json::as_array_mut) {
                    for row in rows {
                        for field in ["values", "key"] {
                            if let Some(values) = row.get_mut(field) {
                                rewrite_values(values, copies);
                            }
                        }
                    }
                }
                if let Some(changes) = pending.get_mut("redis").and_then(Json::as_array_mut) {
                    for change in changes {
                        rewrite_descriptor(change, copies);
                        for key in [
                            "identity_edit_file",
                            "new_field_edit_file",
                            "new_key_edit_file",
                        ] {
                            if let Some(descriptor) = change.get_mut(key) {
                                rewrite_descriptor(descriptor, copies);
                            }
                        }
                        for field in [
                            "value",
                            "field",
                            "member",
                            "new_member",
                            "new_field",
                            "new_key_b64",
                        ] {
                            if let Some(value) = change.get_mut(field) {
                                rewrite_values(value, copies);
                            }
                        }
                    }
                }
            }
        }
    }
}

// 只统计和外置用户新值，不触碰用于重新定位的键或数据库原值。
fn inline_values(pending: &Json) -> Vec<(String, bool)> {
    let mut paths = Vec::new();
    if let Some(rows) = pending.get("rows").and_then(Json::as_array) {
        for (row, item) in rows.iter().enumerate() {
            if let Some(values) = item.get("values").and_then(Json::as_object) {
                for key in values.keys() {
                    paths.push((
                        format!(
                            "/rows/{row}/values/{}",
                            key.replace('~', "~0").replace('/', "~1")
                        ),
                        false,
                    ));
                }
            }
        }
    }
    if let Some(changes) = pending.get("redis").and_then(Json::as_array) {
        for (index, change) in changes.iter().enumerate() {
            for field in [
                "field",
                "member",
                "new_member",
                "new_field",
                "new_key_b64",
                "value",
            ] {
                if change.get(field).is_some() {
                    let operation = change.get("operation").and_then(Json::as_str).unwrap_or("");
                    let identity = field == "field"
                        || (field == "member"
                            && matches!(
                                operation,
                                "set_remove"
                                    | "set_replace_member"
                                    | "zset_remove"
                                    | "zset_replace_member"
                            ));
                    paths.push((format!("/redis/{index}/{field}"), identity));
                }
            }
        }
    }
    paths
}

impl Store {
    /// 发布超额用户新值并在同一工作区事务中切换引用。调用方须持有文件操作锁。
    pub async fn workspace_save_prepared(
        &self,
        id: &str,
        expected_revision: u64,
        mut state: Json,
        copy_from: Option<&str>,
        memory_budget: usize,
    ) -> Result<(u64, Json)> {
        let initial_charge = save_charge(&state)?;
        check_save_budget(initial_charge, memory_budget)?;
        if let Some(tabs) = state.get("tabs").and_then(Json::as_array) {
            for tab in tabs {
                if let Some(pending) = tab.get("pending") {
                    for group in ["rows", "redis"] {
                        if pending
                            .get(group)
                            .and_then(Json::as_array)
                            .is_some_and(|items| {
                                items.iter().any(|item| {
                                    item.get("before").is_some_and(|before| {
                                        !before.is_null()
                                            && before
                                                .as_object()
                                                .is_none_or(|value| !value.is_empty())
                                    })
                                })
                            })
                        {
                            return Err(AppError::invalid("工作区草稿不能包含数据库原值"));
                        }
                    }
                }
            }
        }
        if let Some(source) = copy_from {
            if source == id || expected_revision != 0 {
                return Err(AppError::invalid("工作区副本目标无效"));
            }
            let mut copies = FileMap::new();
            for (file_id, revision) in refs(&state)? {
                let old = self.edit_file(file_id).await?;
                if old.workspace_id != source
                    || old.purpose != EditFilePurpose::Draft
                    || old.state != EditFileState::Ready
                    || old.revision != revision
                {
                    return Err(AppError::conflict("源工作区草稿文件版本或归属已变化"));
                }
                let new = self
                    .register_edit_file(EditFileRegistration {
                        workspace_id: id,
                        profile_id: &old.profile_id,
                        purpose: EditFilePurpose::Draft,
                        target: old.target.clone(),
                        format: &old.format,
                        baseline_digest: &old.baseline_digest,
                        draft_revision: old.draft_revision,
                    })
                    .await?;
                let dir = edit_dir(&self.retired_cache_root, &new, true)?;
                let pending = format!("{}.pending", new.id);
                let dest = format!("{}.{}", new.id, new.format);
                let mut output = new_edit_file(&dir, &pending)?;
                // 流式复制避免一个很小的引用触发完整大值分配。
                read_stable_with(
                    &file_path(&self.retired_cache_root, &old)?,
                    usize::MAX,
                    |input| {
                        std::io::copy(input, &mut output)?;
                        output.sync_all()?;
                        Ok(())
                    },
                )?;
                drop(output);
                dir.hard_link(&pending, &dir, &dest)?;
                dir.remove_file(&pending)?;
                let new = self
                    .transition_edit_file(new.id, new.revision, EditFileState::Ready)
                    .await?;
                copies.insert(old.id, (new.id, new.revision));
            }
            rewrite_copied_references(&mut state, &copies);
        }
        let mut total = 0usize;
        if let Some(tabs) = state.get("tabs").and_then(Json::as_array) {
            for tab in tabs {
                for kind in ["schema_draft", "replace_draft"] {
                    if let Some(intent) = tab.get(kind).filter(|v| !v.is_null())
                        && intent_marker(intent, kind)?.is_none()
                    {
                        total = total.saturating_add(save_estimate(intent)?.bytes);
                    }
                }
                if let Some(pending) = tab.get("pending") {
                    for (path, _) in inline_values(pending) {
                        if let Some(value) = pending.pointer(&path)
                            && (path.ends_with("/new_key_b64") && value.is_string()
                                || value
                                    .get("type")
                                    .and_then(Json::as_str)
                                    .is_some_and(|kind| kind != "large"))
                        {
                            total = total.saturating_add(save_estimate(value)?.bytes);
                        }
                    }
                }
            }
        }
        // 每个外置值产生 Large 和文件描述符；在分配前预留结构、编码和登记元数据。
        let mut growth = 0usize;
        if total > 256 * 1024
            && let Some(tabs) = state.get("tabs").and_then(Json::as_array)
        {
            for tab in tabs {
                for kind in ["schema_draft", "replace_draft"] {
                    if tab.get(kind).is_some_and(|section| !section.is_null()) {
                        growth = growth.saturating_add(4096);
                    }
                }
                if let Some(pending) = tab.get("pending") {
                    let target = pending
                        .get("target")
                        .map(save_charge)
                        .transpose()?
                        .unwrap_or(0);
                    for (path, _) in inline_values(pending) {
                        if pending.pointer(&path).is_some_and(|v| {
                            (path.ends_with("/new_key_b64") && v.is_string())
                                || v.get("type")
                                    .and_then(Json::as_str)
                                    .is_some_and(|kind| kind != "large" && kind != "missing")
                        }) {
                            growth = growth
                                .saturating_add(4096)
                                .saturating_add(target.saturating_mul(2));
                        }
                    }
                }
            }
        }
        check_save_budget(initial_charge.saturating_add(growth), memory_budget)?;
        if total > 256 * 1024 {
            // 类型化 JSON 的转义或 Base64 编码可能比原值大；与读取上限一致。
            let limit = self
                .settings()
                .await?
                .editing
                .max_value_bytes
                .saturating_mul(6)
                .saturating_add(1024);
            if let Some(tabs) = state.get_mut("tabs").and_then(Json::as_array_mut) {
                for tab in tabs {
                    let profile = tab
                        .get("profile_id")
                        .and_then(Json::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let Some(pending) = tab.get_mut("pending").filter(|p| p.is_object()) else {
                        continue;
                    };
                    let target = pending.get("target").cloned().unwrap_or(Json::Null);
                    for (path, identity) in inline_values(pending) {
                        let Some(value) = pending.pointer(&path) else {
                            continue;
                        };
                        let is_key = path.ends_with("/new_key_b64") && value.is_string();
                        let kind = if is_key {
                            "text".to_owned()
                        } else {
                            let Some(kind) = value.get("type").and_then(Json::as_str) else {
                                continue;
                            };
                            if kind == "large" || kind == "missing" {
                                continue;
                            }
                            kind.to_owned()
                        };
                        let bytes = if is_key {
                            let mut bytes = Vec::new();
                            bytes.extend_from_slice(b"{\"type\":\"text\",\"value\":");
                            serde_json::to_writer(&mut bytes, value)?;
                            bytes.push(b'}');
                            bytes
                        } else {
                            serde_json::to_vec(value)?
                        };
                        let record = self
                            .register_edit_file(EditFileRegistration {
                                workspace_id: id,
                                profile_id: &profile,
                                purpose: EditFilePurpose::Draft,
                                target: target.clone(),
                                format: "json",
                                baseline_digest: "",
                                draft_revision: 1,
                            })
                            .await?;
                        let record = self
                            .publish_edit_file(record.id, record.revision, &bytes, limit)
                            .await?;
                        *pending.pointer_mut(&path).expect("已枚举的草稿路径") = serde_json::json!({"type":"large","value":{"id":record.id,"original_type":kind,"bytes":bytes.len(),"preview":""}});
                        let descriptor = serde_json::json!({"edit_file_id":record.id,"edit_file_revision":record.revision});
                        if path.starts_with("/rows/") {
                            let object = pending.as_object_mut().expect("pending 是对象");
                            object
                                .entry("edit_files")
                                .or_insert_with(|| serde_json::json!([]))
                                .as_array_mut()
                                .ok_or_else(|| AppError::invalid("草稿文件列表无效"))?
                                .push(descriptor);
                        } else {
                            let change_path = path.rsplit_once('/').expect("Redis 路径包含字段").0;
                            let change = pending
                                .pointer_mut(change_path)
                                .and_then(Json::as_object_mut)
                                .ok_or_else(|| AppError::invalid("Redis 草稿无效"))?;
                            if path.ends_with("/new_field") {
                                change.insert("new_field_edit_file".into(), descriptor);
                            } else if path.ends_with("/new_key_b64") {
                                change.insert("new_key_edit_file".into(), descriptor);
                            } else if identity {
                                change.insert("identity_edit_file".into(), descriptor);
                            } else {
                                change.insert("edit_file_id".into(), serde_json::json!(record.id));
                                change.insert(
                                    "edit_file_revision".into(),
                                    serde_json::json!(record.revision),
                                );
                            }
                        }
                    }
                }
            }
        }
        // 恢复已有 marker 必须先验证文件和预算，且在 CAS 之前完成。
        let mut markers = Vec::new();
        if let Some(tabs) = state.get("tabs").and_then(Json::as_array) {
            for (index, tab) in tabs.iter().enumerate() {
                for kind in ["schema_draft", "replace_draft"] {
                    if let Some(section) = tab.get(kind)
                        && intent_marker(section, kind)?.is_some()
                    {
                        markers.push((index, kind, section.clone()));
                    }
                }
            }
        }
        let marker_charge = markers.len().saturating_mul(4096);
        if !markers.is_empty() {
            let restore_budget = memory_budget
                .saturating_sub(growth)
                .saturating_sub(marker_charge)
                / 2;
            let charged = save_charge(&state)?;
            check_save_budget(charged, restore_budget)?;
            self.restore_workspace_intents(id, &mut state, charged, restore_budget)
                .await?;
        }
        let charged = save_charge(&state)?
            .saturating_add(growth)
            .saturating_add(marker_charge);
        check_save_budget(charged, memory_budget)?;

        // 暂时替换 section，保留移动出来的原值供事务后返回，无需复制完整树。
        let mut replacements = Vec::new();
        if let Some(tabs) = state.get_mut("tabs").and_then(Json::as_array_mut) {
            for (index, tab) in tabs.iter_mut().enumerate() {
                let profile = tab
                    .get("profile_id")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_owned();
                for kind in ["schema_draft", "replace_draft"] {
                    let marker = if let Some((_, _, marker)) =
                        markers.iter().find(|(i, k, _)| *i == index && *k == kind)
                    {
                        Some(marker.clone())
                    } else if total > 256 * 1024 {
                        if let Some(intent) = tab.get(kind).filter(|v| !v.is_null()) {
                            let bytes = serde_json::to_vec(intent)?;
                            let record = self
                                .register_edit_file(EditFileRegistration {
                                    workspace_id: id,
                                    profile_id: &profile,
                                    purpose: EditFilePurpose::Draft,
                                    target: serde_json::json!({"intent_kind":kind}),
                                    format: "intent",
                                    baseline_digest: "",
                                    draft_revision: 1,
                                })
                                .await?;
                            let record = self
                                .publish_edit_file(
                                    record.id,
                                    record.revision,
                                    &bytes,
                                    memory_budget,
                                )
                                .await?;
                            Some(serde_json::json!({"$datapier_intent":{
                                "version":1,"kind":kind,"edit_file_id":record.id,"edit_file_revision":record.revision
                            }}))
                        } else {
                            None
                        }
                    } else {
                        None
                    };
                    if let Some(marker) = marker {
                        let section = tab.get_mut(kind).expect("已检查的意图 section");
                        replacements.push((index, kind, std::mem::replace(section, marker)));
                    }
                }
            }
        }
        let revision = self
            .workspace_save_with_edit_files_bounded(
                id,
                expected_revision,
                &state,
                memory_budget.saturating_sub(charged),
            )
            .await?;
        if let Some(tabs) = state.get_mut("tabs").and_then(Json::as_array_mut) {
            for (index, kind, section) in replacements {
                tabs[index][kind] = section;
            }
        }
        Ok((revision, state))
    }

    /// 在总内存预算内恢复工作区和已登记的不可变用户意图。
    pub async fn workspace_load_prepared(&self, id: &str, memory_budget: usize) -> Result<Json> {
        use crate::model::editing::JsonMemoryEstimate;
        let limit = i64::try_from(memory_budget).unwrap_or(i64::MAX);
        // 条件在 SQLite 取得 payload 前执行，避免先分配超出预算的字符串。
        let row = sqlx::query("SELECT revision,payload FROM workspaces WHERE id=? AND length(CAST(payload AS BLOB))<=?")
            .bind(id).bind(limit).fetch_optional(&mut *self.pool.acquire().await?).await?;
        let Some(row) = row else {
            let exists: i64 =
                sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM workspaces WHERE id=?)")
                    .bind(id)
                    .fetch_one(&mut *self.pool.acquire().await?)
                    .await?;
            return if exists == 0 {
                Ok(serde_json::json!({"revision":0,"state":{}}))
            } else {
                Err(AppError::new("resource_limit", "工作区超过恢复内存预算"))
            };
        };
        let revision: i64 = row.get(0);
        let payload: &str = row.get(1);
        let mut estimate = JsonMemoryEstimate::default();
        estimate.feed(payload.as_bytes());
        let charged = payload.len().saturating_add(estimate.heap_bound());
        if charged > memory_budget {
            return Err(AppError::new("resource_limit", "工作区超过恢复内存预算"));
        }
        let mut state: Json = serde_json::from_str(payload)?;
        drop(row);
        self.restore_workspace_intents(id, &mut state, charged, memory_budget)
            .await?;
        Ok(serde_json::json!({"revision":revision,"state":state}))
    }

    async fn restore_workspace_intents(
        &self,
        id: &str,
        state: &mut Json,
        mut charged: usize,
        memory_budget: usize,
    ) -> Result<()> {
        use crate::model::editing::JsonMemoryEstimate;
        if let Some(tabs) = state.get_mut("tabs").and_then(Json::as_array_mut) {
            for tab in tabs {
                let profile = tab
                    .get("profile_id")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_owned();
                for kind in ["schema_draft", "replace_draft"] {
                    let Some(section) = tab.get_mut(kind) else {
                        continue;
                    };
                    let Some(marker) = intent_marker(section, kind)? else {
                        continue;
                    };
                    let mut marker_refs = std::collections::BTreeMap::new();
                    references(marker, &mut marker_refs)?;
                    let (&file_id, &file_revision) = marker_refs
                        .iter()
                        .next()
                        .ok_or_else(|| AppError::invalid("意图文件引用无效"))?;
                    if marker_refs.len() != 1 {
                        return Err(AppError::invalid("意图文件引用无效"));
                    }
                    let record = self.edit_file(file_id).await?;
                    if record.workspace_id != id
                        || record.profile_id != profile
                        || record.purpose != EditFilePurpose::Draft
                        || record.state != EditFileState::Ready
                        || record.revision != file_revision
                        || record.format != "intent"
                        || record.target.get("intent_kind").and_then(Json::as_str) != Some(kind)
                    {
                        return Err(AppError::conflict("意图文件归属、类型或版本无效"));
                    }
                    let path = file_path(&self.retired_cache_root, &record)?;
                    let remaining = memory_budget
                        .saturating_sub(charged)
                        .checked_sub(8192)
                        .ok_or_else(|| {
                            AppError::new("resource_limit", "工作区意图超过恢复内存预算")
                        })?;
                    let (value, cost) = read_stable_with(&path, remaining, |file| {
                        let mut charge = JsonMemoryEstimate::default();
                        let size = usize::try_from(file.metadata()?.len())
                            .map_err(|_| AppError::new("resource_limit", "工作区意图过大"))?;
                        if size.saturating_mul(4) > remaining {
                            return Err(AppError::new(
                                "resource_limit",
                                "工作区意图超过恢复内存预算",
                            ));
                        }
                        // 解析与估算必须使用同一份字节；并发篡改不能在扫描后放大 JSON 树。
                        let mut bytes = Vec::with_capacity(size);
                        let mut buffer = [0u8; 8192];
                        loop {
                            let n = file.read(&mut buffer)?;
                            if n == 0 {
                                break;
                            }
                            if bytes.len().saturating_add(n) > size {
                                return Err(AppError::new("file_busy", "意图文件读取时发生变化"));
                            }
                            charge.feed(&buffer[..n]);
                            if size.saturating_add(charge.heap_bound()) > remaining {
                                return Err(AppError::new(
                                    "resource_limit",
                                    "工作区意图超过恢复内存预算",
                                ));
                            }
                            bytes.extend_from_slice(&buffer[..n]);
                        }
                        let value: Json = serde_json::from_slice(&bytes)?;
                        if intent_marker(&value, kind)?.is_some() {
                            return Err(AppError::invalid("意图文件不能引用另一个意图"));
                        }
                        Ok((value, size.saturating_add(charge.heap_bound())))
                    })?;
                    charged = charged.saturating_add(cost).saturating_add(8192);
                    *section = value;
                }
            }
        }
        Ok(())
    }

    async fn ready_draft(&self, id: Uuid) -> Result<Option<EditorFileRecord>> {
        let record = match self.edit_file(id).await {
            Ok(record) => record,
            Err(error) if error.code == "not_found" => return Ok(None),
            Err(error) => return Err(error),
        };
        if record.purpose != EditFilePurpose::Draft
            || record.state != EditFileState::Ready
            || !matches!(record.format.as_str(), "json" | "intent")
        {
            return Err(AppError::conflict("字段引用不是已发布的用户草稿"));
        }
        Ok(Some(record))
    }
    /// 流式计算编码和解码树的总上界；不信任客户端提供的大小。
    pub async fn draft_value_memory_bound(
        &self,
        id: Uuid,
        expected_revision: u64,
    ) -> Result<usize> {
        let record = self
            .ready_draft(id)
            .await?
            .ok_or_else(|| AppError::new("not_found", "用户草稿不存在"))?;
        if record.revision != expected_revision {
            return Err(AppError::conflict("草稿文件版本已变化"));
        }
        let settings = self.settings().await?;
        let limit = settings
            .editing
            .max_value_bytes
            .saturating_mul(6)
            .saturating_add(1024);
        read_stable_with(
            &file_path(&self.retired_cache_root, &record)?,
            limit,
            |file| {
                let mut estimate = crate::model::editing::JsonMemoryEstimate::default();
                let mut buffer = [0u8; 8192];
                let mut size = 0usize;
                loop {
                    let n = file.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    size = size.saturating_add(n);
                    if size > limit {
                        return Err(AppError::new("resource_limit", "草稿文件过大"));
                    }
                    estimate.feed(&buffer[..n]);
                }
                Ok(size
                    .saturating_add(estimate.heap_bound())
                    .saturating_add(8192))
            },
        )
    }

    /// 用预留上界约束同一份扫描和解析字节，即使文件在预留后变化也不能超额分配。
    pub async fn read_draft_value_bounded(
        &self,
        id: Uuid,
        expected_revision: u64,
        budget: usize,
    ) -> Result<Value> {
        let record = self
            .ready_draft(id)
            .await?
            .ok_or_else(|| AppError::new("not_found", "用户草稿不存在"))?;
        if record.revision != expected_revision {
            return Err(AppError::conflict("草稿文件版本已变化"));
        }
        self.decode_draft_value(&record, budget).await
    }

    pub(in crate::storage) async fn read_draft_value(&self, id: Uuid) -> Result<Option<Value>> {
        let Some(record) = self.ready_draft(id).await? else {
            return Ok(None);
        };
        Ok(Some(self.decode_draft_value(&record, usize::MAX).await?))
    }

    async fn decode_draft_value(&self, record: &EditorFileRecord, budget: usize) -> Result<Value> {
        let settings = self.settings().await?;
        let budget = budget.min(settings.limits.result_memory_bytes_per_client);
        let limit = settings
            .editing
            .max_value_bytes
            .saturating_mul(6)
            .saturating_add(1024);
        let value: Value = read_stable_with(
            &file_path(&self.retired_cache_root, record)?,
            limit,
            |file| {
                let size = usize::try_from(file.metadata()?.len())
                    .map_err(|_| AppError::new("resource_limit", "草稿文件过大"))?;
                if size.saturating_add(8192) > budget {
                    return Err(AppError::new("resource_limit", "草稿值超过内存预算"));
                }
                let mut bytes = Vec::with_capacity(size);
                let mut estimate = crate::model::editing::JsonMemoryEstimate::default();
                let mut buffer = [0u8; 8192];
                loop {
                    let n = file.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    if bytes.len().saturating_add(n) > size {
                        return Err(AppError::new("file_busy", "草稿文件读取时发生变化"));
                    }
                    estimate.feed(&buffer[..n]);
                    if size
                        .saturating_add(estimate.heap_bound())
                        .saturating_add(8192)
                        > budget
                    {
                        return Err(AppError::new("resource_limit", "草稿值超过内存预算"));
                    }
                    bytes.extend_from_slice(&buffer[..n]);
                }
                Ok(serde_json::from_slice(&bytes)?)
            },
        )?;
        if record.format == "intent" && !matches!(value, Value::Text(_) | Value::Bytes(_)) {
            return Err(AppError::invalid("定位身份必须是文本或字节"));
        }
        if has_reference(&value) {
            return Err(AppError::invalid("用户新值文件包含未解析引用"));
        }
        Ok(value)
    }

    /// 与 0.2 内存字段使用相同的分块 wire contract，不全量读取文件。
    pub async fn draft_value_chunk(
        &self,
        id: Uuid,
        offset: u64,
        max_bytes: usize,
    ) -> Result<Option<Json>> {
        if max_bytes == 0 || max_bytes > 256 * 1024 {
            return Err(AppError::invalid("字段块大小无效"));
        }
        let Some(record) = self.ready_draft(id).await? else {
            return Ok(None);
        };
        let limit = self
            .settings()
            .await?
            .editing
            .max_value_bytes
            .saturating_mul(6)
            .saturating_add(1024);
        let path = file_path(&self.retired_cache_root, &record)?;
        let chunk = read_stable_with(&path, limit, |file| {
            let total = file.metadata()?.len();
            if offset > total {
                return Err(AppError::invalid("字段偏移无效"));
            }
            file.seek(SeekFrom::Start(offset))?;
            let size = (total - offset).min(max_bytes as u64) as usize;
            let mut bytes = vec![0; size];
            file.read_exact(&mut bytes)?;
            let end = offset + size as u64;
            Ok(
                json!({"id":id,"offset":offset,"data_b64":STANDARD.encode(bytes),"next":(end < total).then_some(end),"total_bytes":total,"content_type":"application/vnd.datapier.value+json"}),
            )
        })?;
        Ok(Some(chunk))
    }

    /// 工作区恢复只认可同一工作区已保存的文件引用。
    pub async fn edit_file_referenced_in_workspace(
        &self,
        id: Uuid,
        workspace_id: &str,
    ) -> Result<bool> {
        let record = self.edit_file(id).await?;
        if record.workspace_id != workspace_id {
            return Ok(false);
        }
        let budget = self.settings().await?.limits.result_memory_bytes_per_client;
        let mut connection = self.pool.acquire().await?;
        workspace_has_reference(&mut connection, workspace_id, id, budget).await
    }
}
