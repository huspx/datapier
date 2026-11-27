//! 行草稿恢复状态、读回及临时结果释放。
use super::*;
use datapier_core::model::TableSchema;

#[derive(Clone, Copy)]
enum RestoreOp {
    Browse,
    Poll,
    Read,
    Value,
    Release,
}

#[derive(Clone)]
struct RestoreChunk {
    id: Uuid,
    kind: String,
    total: u64,
    data: Vec<u8>,
    column: String,
    estimate: datapier_core::model::editing::JsonMemoryEstimate,
}

/// 一次恢复冻结的目标、草稿及临时结果；完整核对前不修改现有草稿。
#[derive(Clone)]
pub(crate) struct RowRestore {
    pending: Value,
    session: Value,
    token: Value,
    target: Value,
    profile: Uuid,
    profile_revision: u64,
    columns: Vec<String>,
    draft_revision: u64,
    rows: Vec<RowChange>,
    files: Vec<grid::EditFileRef>,
    staged: Vec<RowChange>,
    index: usize,
    task: Option<String>,
    result: Option<Uuid>,
    before: Option<BTreeMap<String, DataValue>>,
    chunk: Option<RestoreChunk>,
    in_flight: Option<(Uuid, RestoreOp)>,
    bytes: usize,
}

impl App {
    /// 按可靠键重新读取每一行，整批成功后才恢复可提交草稿。
    pub(crate) fn workspace_restore_start(
        &mut self,
        backend: &Backend,
        pending: Value,
        rows: Vec<RowChange>,
    ) {
        let prepare = || -> Result<RowRestore> {
            if self.restore_rows.is_some() {
                return Err(AppError::new("busy", "正在恢复行草稿"));
            }
            let session = self
                .session_id()
                .ok_or_else(|| AppError::invalid("请先打开连接"))?;
            let token = self
                .edit_token
                .clone()
                .ok_or_else(|| AppError::invalid("请先打开编辑基线"))?;
            let profile = self
                .profile
                .as_ref()
                .ok_or_else(|| AppError::invalid("连接已失效"))?;
            let schema: TableSchema = serde_json::from_value(
                self.schema
                    .clone()
                    .ok_or_else(|| AppError::invalid("请重新打开表编辑基线"))?,
            )?;
            if rows.is_empty() {
                return Err(AppError::invalid("没有待恢复的行变更"));
            }
            let keys = schema
                .keys
                .first()
                .filter(|keys| !keys.is_empty())
                .ok_or_else(|| AppError::invalid("表没有可靠行标识"))?;
            let mut files: Vec<grid::EditFileRef> = serde_json::from_value(
                pending
                    .get("edit_files")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            )?;
            let mut versions = BTreeMap::new();
            for file in &files {
                if file.edit_file_revision == 0
                    || versions
                        .insert(file.edit_file_id, file.edit_file_revision)
                        .is_some_and(|old| old != file.edit_file_revision)
                {
                    return Err(AppError::invalid("草稿文件版本不一致"));
                }
            }
            let mut identities = Vec::new();
            let current = self
                .grid
                .as_ref()
                .map(|grid| grid.changes(keys))
                .transpose()?
                .unwrap_or_default();
            for row in &rows {
                if !matches!(row.operation.as_str(), "insert" | "update" | "delete") {
                    return Err(AppError::invalid("恢复行操作无效"));
                }
                if (row.operation == "insert" && (!row.key.is_empty() || !row.before.is_empty()))
                    || (row.operation == "delete" && !row.values.is_empty())
                    || (row.operation == "update" && row.values.is_empty())
                {
                    return Err(AppError::invalid("恢复行操作内容无效"));
                }
                if row.operation != "insert" {
                    if row.key.len() != keys.len()
                        || keys.iter().any(|key| !row.key.contains_key(key))
                        || row.key.values().any(|value| {
                            matches!(
                                value,
                                DataValue::Null | DataValue::Missing | DataValue::Large { .. }
                            )
                        })
                    {
                        return Err(AppError::invalid("恢复数据的行标识不匹配"));
                    }
                    if identities.contains(&row.key)
                        || self
                            .pending_rows
                            .iter()
                            .chain(current.iter())
                            .any(|old| old.operation != "insert" && old.key == row.key)
                    {
                        return Err(AppError::conflict("目标行已有待提交变更"));
                    }
                    identities.push(row.key.clone());
                }
                for value in row.values.values() {
                    if let DataValue::Large { id, .. } = value
                        && !files
                            .iter()
                            .any(|file| file.edit_file_id == *id && file.edit_file_revision > 0)
                    {
                        return Err(AppError::invalid("大值草稿缺少受控文件登记"));
                    }
                }
            }
            files.retain(|file| rows.iter().any(|row| row.values.values().any(|value| matches!(value, DataValue::Large { id, .. } if *id == file.edit_file_id))));
            Ok(RowRestore {
                pending,
                session,
                token,
                target: json!(self.target),
                profile: profile.id,
                profile_revision: profile.revision,
                columns: schema
                    .columns
                    .iter()
                    .map(|column| column.name.clone())
                    .collect(),
                draft_revision: self.draft_revision,
                rows,
                files,
                staged: Vec::new(),
                index: 0,
                task: None,
                result: None,
                before: None,
                chunk: None,
                in_flight: None,
                bytes: 0,
            })
        };
        match prepare() {
            Ok(state) => {
                self.restore_rows = Some(state);
                self.workspace_restore_advance(backend);
            }
            Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
        }
    }

    fn workspace_restore_send(
        &mut self,
        backend: &Backend,
        op: RestoreOp,
        method: &str,
        params: Value,
    ) {
        let nonce = Uuid::new_v4();
        let tag = format!(
            "{}:{}:restore-row/{nonce}",
            self.tabs[self.active].id, self.generation
        );
        match backend.request(tag, method, params) {
            Ok(()) => {
                if let Some(state) = &mut self.restore_rows {
                    state.in_flight = Some((nonce, op));
                }
            }
            Err(error) => self.workspace_restore_fail(backend, error.to_string()),
        }
    }

    fn workspace_restore_fail(&mut self, backend: &Backend, error: String) {
        if let Some(state) = self.restore_rows.take()
            && let Some(result) = state.result
        {
            let tag = format!(
                "{}:{}:restore-release/{}",
                self.tabs[self.active].id,
                self.generation,
                Uuid::new_v4()
            );
            if let Err(release) =
                backend.request(tag, "result.release", json!({"result_id":result}))
            {
                self.status = i18n::UiMessage::raw(format!("{error}; {release}"));
                return;
            }
        }
        self.status = i18n::UiMessage::raw(error);
    }

    fn workspace_restore_current(&self, state: &RowRestore) -> bool {
        self.session_id().as_ref() == Some(&state.session)
            && self.edit_token.as_ref() == Some(&state.token)
            && json!(self.target) == state.target
            && self.profile.as_ref().is_some_and(|profile| {
                profile.id == state.profile && profile.revision == state.profile_revision
            })
            && self.draft_revision == state.draft_revision
            && self
                .tabs
                .get(self.active)
                .is_some_and(|tab| tab.pending == state.pending)
    }

    fn workspace_restore_advance(&mut self, backend: &Backend) {
        let Some(state) = self.restore_rows.as_mut() else {
            return;
        };
        if state.in_flight.is_some() || state.task.is_some() || state.result.is_some() {
            return;
        }
        while state.index < state.rows.len() && state.rows[state.index].operation == "insert" {
            state.staged.push(state.rows[state.index].clone());
            state.index += 1;
        }
        if state.index == state.rows.len() {
            let state = self.restore_rows.take().unwrap();
            if !self.workspace_restore_current(&state) {
                self.status = "恢复期间目标或草稿已变化".into();
                return;
            }
            self.pending_rows.extend(state.staged);
            self.pending_edit_files.extend(state.files);
            self.pending_edit_files.sort();
            self.pending_edit_files.dedup();
            self.tabs[self.active].pending = Value::Null;
            self.tabs[self.active].pending_live = false;
            self.draft_revision = self.draft_revision.wrapping_add(1);
            self.dirty = true;
            self.detail = serde_json::to_string_pretty(
                &json!({"rows":self.pending_rows,"redis":self.redis_changes}),
            )
            .unwrap_or_default();
            self.set_detail_context("草稿详情");
            self.status = "编辑意图已恢复；提交时将检查原值，请先确认预览".into();
            return;
        }
        let filters: Vec<Value> = state.rows[state.index]
            .key
            .iter()
            .map(|(column, value)| json!({"column":column,"op":"eq","value":value}))
            .collect();
        let params = json!({"session_id":state.session,"workspace_id":self.workspace_id,"target":state.target,"filters":filters,"sort":[],"page_size":2});
        self.workspace_restore_send(backend, RestoreOp::Browse, "resource.browse", params);
    }

    /// 每次常规轮询调用；不会覆盖普通 tab 任务和结果。
    pub(crate) fn workspace_restore_poll(&mut self, backend: &Backend) {
        // 后台标签也必须推进只读任务；不切换活动标签或复用其普通任务字段。
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            if index == self.active {
                continue;
            }
            let Some(state) = tab.runtime.restore_rows.as_mut() else {
                continue;
            };
            if state.in_flight.is_some() {
                continue;
            }
            let Some(task) = state.task.as_ref() else {
                continue;
            };
            let nonce = Uuid::new_v4();
            let tag = format!("{}:{}:restore-row/{nonce}", tab.id, tab.runtime.generation);
            match backend.request(tag, "execution.get", json!({"task_id":task})) {
                Ok(()) => state.in_flight = Some((nonce, RestoreOp::Poll)),
                Err(error) => {
                    tab.runtime.restore_rows = None;
                    self.status = i18n::UiMessage::raw(error.to_string());
                }
            }
        }
        let Some(state) = &self.restore_rows else {
            return;
        };
        if !self.workspace_restore_current(state) {
            self.workspace_restore_fail(backend, "恢复期间目标或草稿已变化".into());
            return;
        }
        if state.in_flight.is_some() {
            return;
        }
        if let Some(task) = state.task.clone() {
            self.workspace_restore_send(
                backend,
                RestoreOp::Poll,
                "execution.get",
                json!({"task_id":task}),
            );
        } else {
            self.workspace_restore_advance(backend);
        }
    }

    fn workspace_restore_value_or_release(&mut self, backend: &Backend) {
        let Some(state) = self.restore_rows.as_mut() else {
            return;
        };
        if state.chunk.is_none()
            && let Some((
                column,
                DataValue::Large {
                    id,
                    original_type,
                    bytes,
                    ..
                },
            )) = state.before.as_ref().and_then(|before| {
                before
                    .iter()
                    .find(|(_, value)| matches!(value, DataValue::Large { .. }))
            })
        {
            let available = self
                .settings
                .limits
                .result_memory_bytes_per_client
                .saturating_sub(state.bytes);
            if *bytes > available as u64 {
                self.workspace_restore_fail(backend, "恢复基线超过内存预算".into());
                return;
            }
            let mut data = Vec::new();
            if data.try_reserve_exact(*bytes as usize).is_err() {
                self.workspace_restore_fail(backend, "恢复基线内存分配失败".into());
                return;
            }
            // 原始编码预留一次；每批在扩充解析树前另计节点和临时缓冲。
            state.bytes += *bytes as usize;
            state.chunk = Some(RestoreChunk {
                id: *id,
                kind: original_type.clone(),
                total: *bytes,
                data,
                estimate: Default::default(),
                column: column.clone(),
            });
        }
        if let Some(chunk) = &state.chunk {
            let params = json!({"value_id":chunk.id,"offset":chunk.data.len(),"max_bytes":262144});
            self.workspace_restore_send(backend, RestoreOp::Value, "result.value", params);
        } else if let Some(result) = state.result {
            self.workspace_restore_send(
                backend,
                RestoreOp::Release,
                "result.release",
                json!({"result_id":result}),
            );
        }
    }

    /// 接收已经按 tab/generation 路由的恢复响应；每一步再次核对 nonce。
    pub(crate) fn workspace_restore_response(
        &mut self,
        tag: &str,
        result: &Result<Value>,
        backend: &Backend,
    ) -> bool {
        if tag.starts_with("restore-release/") {
            if let Err(error) = result {
                self.status = i18n::UiMessage::raw(error.to_string());
            }
            return true;
        }
        let Some(token) = tag
            .strip_prefix("restore-row/")
            .and_then(|token| Uuid::parse_str(token).ok())
        else {
            return false;
        };
        let Some(state) = self.restore_rows.as_ref() else {
            return true;
        };
        let Some((expected, op)) = state.in_flight else {
            return true;
        };
        if token != expected {
            return true;
        }
        if !self.workspace_restore_current(state) {
            if matches!(op, RestoreOp::Release) && result.is_ok() {
                self.restore_rows.as_mut().unwrap().result = None;
            }
            self.workspace_restore_fail(backend, "恢复期间目标或草稿已变化".into());
            return true;
        }
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if matches!(op, RestoreOp::Release) {
                    self.restore_rows.as_mut().unwrap().result = None;
                }
                self.workspace_restore_fail(backend, error.to_string());
                return true;
            }
        };
        let state = self.restore_rows.as_mut().unwrap();
        state.in_flight = None;
        if matches!(op, RestoreOp::Release) {
            state.result = None;
        }
        let outcome = self.workspace_restore_accept(op, value);
        if let Err(error) = outcome {
            self.workspace_restore_fail(backend, error.to_string());
            return true;
        }
        match op {
            RestoreOp::Browse | RestoreOp::Poll => {
                if self
                    .restore_rows
                    .as_ref()
                    .is_some_and(|state| state.result.is_some())
                {
                    let id = self.restore_rows.as_ref().unwrap().result.unwrap();
                    self.workspace_restore_send(backend, RestoreOp::Read, "result.read", json!({"result_id":id,"start":0,"max_rows":2,"max_bytes":self.settings.limits.response_bytes}));
                }
            }
            RestoreOp::Read | RestoreOp::Value => self.workspace_restore_value_or_release(backend),
            RestoreOp::Release => self.workspace_restore_advance(backend),
        }
        true
    }

    fn workspace_restore_accept(&mut self, op: RestoreOp, value: &Value) -> Result<()> {
        let state = self
            .restore_rows
            .as_mut()
            .ok_or_else(|| AppError::invalid("恢复状态已失效"))?;
        match op {
            RestoreOp::Browse => {
                let task = value
                    .get("task_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| AppError::invalid("恢复读取缺少任务"))?;
                Uuid::parse_str(task).map_err(|_| AppError::invalid("恢复任务无效"))?;
                state.task = Some(task.to_owned());
            }
            RestoreOp::Poll => {
                if value.get("id").and_then(Value::as_str) != state.task.as_deref() {
                    return Err(AppError::conflict("恢复任务已变化"));
                }
                match value.get("state").and_then(Value::as_str) {
                    Some("queued" | "running" | "cancelling") => (),
                    Some("succeeded") => {
                        let ids = value
                            .get("results")
                            .and_then(Value::as_array)
                            .filter(|ids| ids.len() == 1)
                            .ok_or_else(|| AppError::invalid("恢复读取缺少唯一结果"))?;
                        state.result = Some(
                            Uuid::parse_str(ids[0].as_str().unwrap_or(""))
                                .map_err(|_| AppError::invalid("恢复结果无效"))?,
                        );
                        state.task = None;
                    }
                    _ => {
                        state.result = value
                            .get("results")
                            .and_then(Value::as_array)
                            .and_then(|ids| ids.first())
                            .and_then(Value::as_str)
                            .and_then(|id| Uuid::parse_str(id).ok());
                        return Err(AppError::conflict(format!("恢复读取失败: {}", value)));
                    }
                }
            }
            RestoreOp::Read => {
                let page: ResultPage = serde_json::from_value(value.clone())?;
                if Some(page.result.id) != state.result
                    || page.start != 0
                    || page.rows.len() != 1
                    || page.next.is_some()
                    || page.result.summary.get("has_more").and_then(Value::as_bool) != Some(false)
                {
                    return Err(AppError::conflict("恢复目标不存在或不唯一"));
                }
                let row = &page.rows[0];
                if row.len() != page.result.columns.len() {
                    return Err(AppError::conflict("恢复基线列数不匹配"));
                }
                let before: BTreeMap<_, _> = page
                    .result
                    .columns
                    .iter()
                    .zip(row)
                    .map(|(column, value)| (column.name.clone(), value.clone()))
                    .collect();
                if page.result.columns.len() != state.columns.len()
                    || page
                        .result
                        .columns
                        .iter()
                        .zip(&state.columns)
                        .any(|(column, expected)| &column.name != expected)
                {
                    return Err(AppError::conflict("恢复时表结构已变化"));
                }
                let encoded = serde_json::to_vec(&before)?;
                let mut estimate = datapier_core::model::editing::JsonMemoryEstimate::default();
                estimate.feed(&encoded);
                let charged = encoded.len().saturating_add(estimate.heap_bound());
                if state.bytes.saturating_add(charged)
                    > self.settings.limits.result_memory_bytes_per_client
                {
                    return Err(AppError::new("resource_limit", "恢复基线超过内存预算"));
                }
                state.bytes += charged;
                let expected = &state.rows[state.index];
                if before.len() != row.len()
                    || !expected
                        .key
                        .iter()
                        .all(|(key, value)| before.get(key) == Some(value))
                    || expected.values.keys().any(|key| !before.contains_key(key))
                {
                    return Err(AppError::conflict("恢复数据的行标识或列不匹配"));
                }
                state.before = Some(before);
            }
            RestoreOp::Value => {
                let chunk = state
                    .chunk
                    .as_mut()
                    .ok_or_else(|| AppError::invalid("恢复分块状态已失效"))?;
                let offset = chunk.data.len() as u64;
                if value.get("offset").and_then(Value::as_u64) != Some(offset)
                    || value.get("total_bytes").and_then(Value::as_u64) != Some(chunk.total)
                {
                    return Err(AppError::conflict("恢复值分块不一致"));
                }
                let bytes = STANDARD
                    .decode(
                        value
                            .get("data_b64")
                            .and_then(Value::as_str)
                            .ok_or_else(|| AppError::invalid("恢复值分块缺失"))?,
                    )
                    .map_err(|error| AppError::invalid(error.to_string()))?;
                let end = offset
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| AppError::invalid("恢复值长度溢出"))?;
                let mut estimate = chunk.estimate;
                estimate.feed(&bytes);
                let charged = estimate
                    .heap_bound()
                    .saturating_sub(chunk.estimate.heap_bound());
                if end > chunk.total
                    || state.bytes.saturating_add(charged)
                        > self.settings.limits.result_memory_bytes_per_client
                {
                    return Err(AppError::new("resource_limit", "恢复基线超过内存预算"));
                }
                chunk.data.extend_from_slice(&bytes);
                chunk.estimate = estimate;
                state.bytes += charged;
                if end == chunk.total && value.get("next").is_none_or(Value::is_null) {
                    let decoded: DataValue = serde_json::from_slice(&chunk.data)?;
                    if serde_json::to_value(&decoded)?
                        .get("type")
                        .and_then(Value::as_str)
                        != Some(chunk.kind.as_str())
                    {
                        return Err(AppError::conflict("恢复值类型不匹配"));
                    }
                    state
                        .before
                        .as_mut()
                        .unwrap()
                        .insert(chunk.column.clone(), decoded);
                    state.chunk = None;
                } else if value.get("next").and_then(Value::as_u64) != Some(end)
                    || end <= offset
                    || end >= chunk.total
                {
                    return Err(AppError::conflict("恢复值分块不连续"));
                }
            }
            RestoreOp::Release => {
                if value.get("released").and_then(Value::as_bool) != Some(true) {
                    return Err(AppError::invalid("临时结果释放未确认"));
                }
                let before = state
                    .before
                    .take()
                    .ok_or_else(|| AppError::invalid("恢复基线缺失"))?;
                let mut row = state.rows[state.index].clone();
                if !row.before.is_empty() && row.before != before {
                    return Err(AppError::conflict("恢复数据与当前行基线不一致"));
                }
                row.before = before;
                state.staged.push(row);
                state.index += 1;
                state.result = None;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/workspace.rs"]
mod tests;
