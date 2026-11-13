//! 高级单值编辑请求路由；tab/generation 由 App 的路由器核对。
use super::{
    files::FileUi,
    value::{Mode, ValueUi},
};
use crate::{App, Backend, Cell, Msg, Result, Uuid, Value, i18n, json};

impl App {
    /// 保留用户输入；在途分块完成后再释放对应派生图片。
    pub(crate) fn advanced_value_schema_invalidated(&mut self) {
        if self.advanced_value.session.is_some() {
            if !self.advanced_value.pending {
                self.advanced_value.invalidate_preview();
            }
            self.advanced_value.error =
                Some(i18n::tr("结构已变化，值编辑基线已失效；新值仍保留").into_owned());
        }
    }

    /// 仅释放 Engine 返回的派生预览；不要释放其父值会话。
    pub(crate) fn advanced_value_release_stale_preview(
        &self,
        backend: &Backend,
        result: &Result<Value>,
    ) {
        if let Ok(value) = result
            && let (Some(id), Some(revision)) = (
                value.get("preview_id").and_then(Value::as_str),
                value.get("revision").and_then(Value::as_u64),
            )
        {
            let _ = backend.request(
                format!("advanced-value-retired/{}", Uuid::new_v4()),
                "value.release",
                json!({"value_session":id,"revision":revision}),
            );
        }
    }

    pub(crate) fn advanced_value_release_previews(&mut self, backend: &Backend) -> bool {
        while let Some((id, revision)) = self.advanced_value.retired_previews.last() {
            if let Err(error) = backend.request(
                format!("advanced-value-retired/{}", Uuid::new_v4()),
                "value.release",
                json!({"value_session":id,"revision":revision}),
            ) {
                self.advanced_value.error = Some(error.to_string());
                return false;
            }
            self.advanced_value.retired_previews.pop();
        }
        true
    }

    /// 汇总当前和后台标签的草稿、撤销历史及仍打开的值会话。
    pub(crate) fn retained_value_sources(
        &self,
    ) -> std::collections::BTreeSet<crate::grid::ValueSource> {
        fn collect(
            sources: &mut std::collections::BTreeSet<crate::grid::ValueSource>,
            grid: Option<&crate::GridState>,
            value: &ValueUi,
        ) {
            if let Some(grid) = grid {
                sources.extend(grid.retained_value_sources());
            }
            if let Some(id) = value
                .session
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
            {
                sources.insert(crate::grid::ValueSource {
                    value_session: id,
                    revision: value.revision,
                });
            }
        }
        let mut sources = std::collections::BTreeSet::new();
        collect(&mut sources, self.grid.as_ref(), &self.advanced_value);
        for (index, tab) in self.tabs.iter().enumerate() {
            if index != self.active {
                collect(
                    &mut sources,
                    tab.runtime.grid.as_ref(),
                    &tab.runtime.advanced_value,
                );
            }
        }
        sources
    }

    /// 对引用差集发出释放请求；入队失败时保留引用供下轮重试。
    pub(crate) fn release_retired_value_sources(
        &mut self,
        backend: &Backend,
        known: &mut std::collections::BTreeSet<crate::grid::ValueSource>,
    ) {
        let current = self.retained_value_sources();
        let current_ids: std::collections::BTreeSet<_> =
            current.iter().map(|source| source.value_session).collect();
        let retired: Vec<_> = known
            .iter()
            .copied()
            .filter(|source| !current_ids.contains(&source.value_session))
            .collect();
        // 同一活动会话的 revision 前进时只更新记录，不释放旧 revision。
        known.retain(|source| !current_ids.contains(&source.value_session));
        known.extend(current);
        for source in retired {
            let tag = format!(
                "advanced-value-retired/{}/{}",
                source.value_session, source.revision
            );
            match backend.request(
                tag,
                "value.release",
                json!({"value_session":source.value_session,"revision":source.revision}),
            ) {
                Ok(()) => {
                    known.remove(&source);
                }
                Err(error) => {
                    self.status = i18n::UiMessage::raw(error.to_string());
                }
            }
        }
    }

    fn advanced_value_baseline_current(&self) -> bool {
        let Some(source) = self.advanced_value.source.as_ref() else {
            return false;
        };
        if source["draft_revision"].as_u64() != Some(self.draft_revision)
            || source["session"] != json!(self.session_id())
            || source["target"] != json!(self.target)
            || source["edit_token"] != json!(self.edit_token)
            || self.edit_token.is_none()
        {
            return false;
        }
        let Some(grid) = self.grid.as_ref() else {
            return false;
        };
        let Some(row) = source["row"].as_u64() else {
            return false;
        };
        let Some(column) = source["column"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
        else {
            return false;
        };
        json!(grid.page.result.id) == source["result_id"]
            && grid
                .page
                .result
                .columns
                .get(column)
                .is_some_and(|item| Some(item.name.as_str()) == source["column_name"].as_str())
            && grid
                .value(Cell { row, column })
                .is_some_and(|value| json!(value) == source["original"])
    }

    fn advanced_value_send(
        &mut self,
        backend: &Backend,
        operation: &str,
        method: &str,
        params: Value,
    ) {
        let nonce = Uuid::new_v4();
        let tag = format!(
            "{}:{}:advanced-value/{nonce}",
            self.tabs[self.active].id, self.generation
        );
        match backend.request(tag, method, params) {
            Ok(()) => {
                self.advanced_value.pending = true;
                self.advanced_files.pending = true;
                self.advanced_value.request = Some((nonce, operation.to_owned()));
            }
            Err(error) => {
                self.advanced_value.format_undo = None;
                if operation == "value.external.close"
                    && self.advanced_files.confirm_clean.is_some()
                {
                    self.advanced_files.error = Some(error.to_string());
                } else {
                    self.advanced_value.error = Some(error.to_string());
                }
            }
        }
    }

    /// params 是 Engine 的 value.open 参数；入口调用方负责提供真实目标与角色。
    pub(crate) fn advanced_value_open(&mut self, backend: &Backend, mut params: Value) {
        if self.advanced_value.pending || self.advanced_value.session.is_some() {
            return;
        }
        let Some(cell) = self
            .detail_cell()
            .or_else(|| self.grid.as_ref().and_then(|grid| grid.focus))
        else {
            self.status = i18n::UiMessage::key("没有选中单元格");
            return;
        };
        let Some(grid) = self.grid.as_ref() else {
            return;
        };
        // Engine 按结果快照打开值；不能把已有本地草稿当成同一基线。
        if grid.edits().contains_key(&cell) {
            self.status = i18n::UiMessage::key("目标行已有待提交变更");
            return;
        }
        let Some(original) = grid.value(cell) else {
            return;
        };
        let Some(column) = grid.page.result.columns.get(cell.column) else {
            return;
        };
        let column_name = column.name.clone();
        if params.get("session_id") != self.session_id().as_ref()
            || params.get("target") != Some(&json!(self.target))
            || params.get("result_id") != Some(&json!(grid.page.result.id))
            || params.get("row_no") != Some(&json!(cell.row))
            || params.get("column").and_then(Value::as_str) != Some(column_name.as_str())
        {
            self.status = i18n::UiMessage::key("目标单元格已不可用");
            return;
        }
        if self.is_redis()
            && params.get("readonly").and_then(Value::as_bool) != Some(true)
            && params
                .get("edit_token")
                .is_some_and(|token| !token.is_null())
        {
            use datapier_core::model::Value as DataValue;
            let kind = self
                .schema
                .as_ref()
                .and_then(|schema| schema.get("type"))
                .and_then(Value::as_str);
            let (role, identity) = match (kind, column_name.as_str()) {
                (Some("string"), "value") => ("string", None),
                (Some("hash"), "value") => ("hash_value", Some("field")),
                (Some("list"), "value") => ("list_value", Some("index")),
                (Some("set"), "member") => ("set_member", Some("member")),
                (Some("zset"), "member") => ("zset_member", Some("member")),
                _ => {
                    self.status = i18n::UiMessage::key("当前 Redis 类型不能编辑行");
                    return;
                }
            };
            params["role"] = json!(role);
            if let Some(name) = identity {
                let Some(index) = grid
                    .page
                    .result
                    .columns
                    .iter()
                    .position(|column| column.name == name)
                else {
                    self.status = i18n::UiMessage::key("Redis 行缺少字段");
                    return;
                };
                let identity_cell = Cell {
                    row: cell.row,
                    column: index,
                };
                let Some(raw_identity) = grid.original(identity_cell) else {
                    self.status = i18n::UiMessage::key("Redis 行缺少字段");
                    return;
                };
                // 使用结果原始 Large 句柄，Engine 按结果归属解引用；不传 UI 展示值。
                let identity_value = match (name, raw_identity) {
                    ("index", DataValue::Integer(value)) => match value.parse::<i64>() {
                        Ok(index) if index >= 0 => json!(index),
                        _ => {
                            self.status = i18n::UiMessage::key("Redis 行缺少字段");
                            return;
                        }
                    },
                    ("index", _) => {
                        self.status = i18n::UiMessage::key("Redis 行缺少字段");
                        return;
                    }
                    (_, value) => json!(value),
                };
                if name == "index" {
                    let original = identity_value.as_i64().unwrap();
                    if let Err(error) = self.redis_list_index(original, false) {
                        self.status = error;
                        return;
                    }
                }
                params[name] = identity_value;
            }
        }
        params["draft_revision"] = json!(self.draft_revision);
        let grid_edits: Vec<_> = grid
            .edits()
            .iter()
            .map(|(cell, value)| json!([cell.row, cell.column, value]))
            .collect();
        let source = json!({"session":self.session_id(),"connection":self.profile.as_ref().map(|p| &p.name),"target":self.target,"edit_token":self.edit_token,
            "result_id":grid.page.result.id,"row":cell.row,"column":cell.column,"column_name":column_name,"original":original,
            "draft_revision":params["draft_revision"],"grid_edits":grid_edits,"pending_rows":self.pending_rows,"redis_revision":self.redis_changes_revision});
        self.advanced_value = ValueUi {
            max_text_bytes: self.settings.editing.max_text_editor_bytes,
            max_value_bytes: self.settings.editing.max_value_bytes,
            allow_null: !self.is_redis(),
            source: Some(source),
            ..ValueUi::default()
        };
        self.advanced_files = FileUi::default();
        self.workbench.overlay = Some("advanced-value".into());
        self.advanced_value_send(backend, "open", "value.open", params);
    }

    /// 响应 action 之前由 App 调用；返回 true 表示该动作属于本编辑器。
    pub(crate) fn advanced_value_action(&mut self, action: &str, backend: &Backend) -> bool {
        if action == "advanced-value-open" {
            return false;
        }
        if !action.starts_with("advanced-value-")
            && !action.starts_with("advanced-file-")
            && !action.starts_with("advanced-external-")
        {
            return false;
        }
        if action == "advanced-value-close" && self.advanced_files.confirm_clean.is_some() {
            self.advanced_files.confirm_clean = None;
            return true;
        }
        if action == "advanced-value-continue"
            || (action == "advanced-value-close" && self.advanced_value.confirm_close)
        {
            self.advanced_value.confirm_close = false;
            return true;
        }
        if let Some(open) = action.strip_prefix("advanced-value-kind-open:") {
            self.advanced_value.kind_select_open = open == "true";
            return true;
        }
        if self.advanced_value.confirm_close && action != "advanced-value-discard" {
            return true;
        }
        if matches!(action, "advanced-value-close" | "advanced-value-discard") {
            if self.advanced_value.pending {
                return true;
            }
            if self.advanced_files.file_session.is_some() {
                self.advanced_value.error =
                    Some(i18n::tr("请先选择保留或清理编辑文件").into_owned());
            } else if (self.advanced_value.dirty()
                || (self.advanced_value.applied && !self.advanced_value.staged))
                && action != "advanced-value-discard"
            {
                self.advanced_value.confirm_close = true;
            } else {
                self.advanced_value.invalidate_preview();
                if !self.advanced_value_release_previews(backend) {
                    return true;
                }
                // 差集清理器在下一轮释放未被草稿引用的会话。
                self.workbench.overlay = None;
                self.advanced_value = ValueUi::default();
                self.advanced_files = FileUi::default();
            }
            return true;
        }
        if action.starts_with("advanced-file-") || action.starts_with("advanced-external-") {
            if self.advanced_value.pending || self.advanced_value.dirty() {
                self.advanced_files.error = Some(i18n::tr("请先应用当前修改").into_owned());
                return true;
            }
            if matches!(action, "advanced-file-import" | "advanced-external-read")
                && !self.advanced_value_baseline_current()
            {
                self.advanced_files.error =
                    Some(i18n::tr("草稿已变化，请重新打开值编辑器").into_owned());
                return true;
            }
            let Some(session) = self.advanced_value.session.clone() else {
                return true;
            };
            let format = match self.advanced_value.mode {
                Mode::Text => "txt",
                Mode::Json => "json",
                Mode::Xml => "xml",
                _ => "bin",
            };
            if let Some((method, params)) = self.advanced_files.action(
                action,
                &session,
                self.advanced_value.revision,
                self.advanced_value.writable,
                format,
            ) {
                self.advanced_value_send(backend, method, method, params);
            }
        } else {
            if let Some(mode) = action.strip_prefix("advanced-value-mode:")
                && matches!(mode, "text" | "hex" | "json" | "xml" | "image")
                && !self.advanced_value.pending
            {
                self.focus = format!("advanced-value-tab-{mode}");
                self.workbench.focus_request = Some(String::new());
            }
            if matches!(
                action,
                "advanced-value-stage" | "advanced-value-set-null" | "advanced-value-replace-null"
            ) && !self.advanced_value_baseline_current()
            {
                self.advanced_value.error =
                    Some(i18n::tr("草稿已变化，请重新打开值编辑器").into_owned());
                return true;
            }
            match self.advanced_value.action(action) {
                Ok(Some((method, params))) => {
                    self.advanced_value_send(backend, action, method, params)
                }
                Ok(None) => {}
                Err(error) => self.advanced_value.error = Some(error.to_string()),
            }
        }
        true
    }

    /// 在普通编辑器分发前处理本编辑器的原生输入。
    pub(crate) fn advanced_value_edit(&mut self, message: &Msg) -> bool {
        match message {
            Msg::Edit(name, from, to, text, cursor, len) if name == "advanced-value" => {
                self.advanced_value.edit(*from, *to, text, *cursor, *len);
                true
            }
            Msg::Undo(name) if name == "advanced-value" => {
                self.advanced_value.undo();
                true
            }
            Msg::Edit(name, from, to, text, cursor, len) if name == "advanced-file-path" => {
                self.advanced_files.edit(*from, *to, text, *cursor, *len);
                true
            }
            Msg::Undo(name) if name == "advanced-file-path" => {
                if !self.advanced_files.pending {
                    self.advanced_files.path.undo();
                }
                true
            }
            _ => false,
        }
    }

    /// tag 为去掉 tab/generation 前缀后的值；先核对请求 nonce，再消费结果。
    pub(crate) fn advanced_value_response(
        &mut self,
        tag: &str,
        result: &Result<Value>,
        backend: &Backend,
    ) -> bool {
        let Some(token) = tag.strip_prefix("advanced-value/") else {
            return false;
        };
        let Some((expected, operation)) = self.advanced_value.request.take() else {
            self.advanced_value_release_stale_preview(backend, result);
            return true;
        };
        if Uuid::parse_str(token).ok() != Some(expected) {
            self.advanced_value.request = Some((expected, operation));
            self.advanced_value_release_stale_preview(backend, result);
            return true;
        }
        self.advanced_value.pending = false;
        self.advanced_files.pending = false;
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                self.advanced_value.format_undo = None;
                if operation == "value.external.close"
                    && self.advanced_files.confirm_clean.is_some()
                {
                    self.advanced_files.error = Some(error.to_string());
                } else {
                    self.advanced_value.error = Some(error.to_string());
                }
                return true;
            }
        };
        if matches!(
            operation.as_str(),
            "value.file.import"
                | "value.external.read"
                | "advanced-value-set-null"
                | "advanced-value-replace-null"
        ) && !self.advanced_value_baseline_current()
        {
            // Engine 已接受内容时保留其新 revision，以便随后释放；不能回填旧目标。
            if value.get("value_session").and_then(Value::as_str)
                == self.advanced_value.session.as_deref()
                && let Some(revision) = value.get("revision").and_then(Value::as_u64)
            {
                self.advanced_value.revision = revision;
            }
            if operation == "value.external.read"
                && let Err(error) = self.advanced_files.update_revision(value)
            {
                self.advanced_files.error = Some(error.to_string());
            }
            self.advanced_value.writable = false;
            self.advanced_value.invalidate_preview();
            self.advanced_value.error =
                Some(i18n::tr("草稿已变化，请重新打开值编辑器").into_owned());
            return true;
        }
        let next = match operation.as_str() {
            "open"
            | "advanced-value-patch"
            | "advanced-value-format"
            | "value.file.import"
            | "advanced-value-set-null"
            | "advanced-value-replace-null" => self.advanced_value.summary_response(value),
            "value.external.read" => self
                .advanced_files
                .update_revision(value)
                .and_then(|_| self.advanced_value.summary_response(value)),
            "read" => self.advanced_value.read_response(value),
            "advanced-value-validate" => {
                self.advanced_value.validation_response(value).map(|_| None)
            }
            "advanced-value-preview-image" => self.advanced_value.image_response(value).map(Some),
            "value.external.open" => self.advanced_files.open_response(value).map(|_| None),
            "value.external.launch" => self.advanced_files.launch_response(value).map(|_| None),
            "value.external.close" => {
                self.advanced_files.closed();
                Ok(None)
            }
            "value.file.export" => Ok(None),
            "advanced-value-stage" => self.advanced_value_stage(value).map(|_| None),
            _ if operation.starts_with("advanced-value-mode:")
                || operation == "advanced-value-next"
                || operation == "advanced-value-previous" =>
            {
                self.advanced_value.read_response(value)
            }
            _ => Err(crate::AppError::invalid("未知值编辑响应")),
        };
        if next.is_ok()
            && matches!(
                operation.as_str(),
                "advanced-value-patch"
                    | "advanced-value-format"
                    | "value.file.import"
                    | "value.external.read"
                    | "advanced-value-set-null"
                    | "advanced-value-replace-null"
            )
        {
            self.advanced_value.applied = true;
        }
        if next.is_err() {
            self.advanced_value_release_stale_preview(backend, result);
            self.advanced_value.invalidate_preview();
        }
        match next {
            Ok(Some((method, params))) => self.advanced_value_send(backend, "read", method, params),
            Ok(None) => {}
            Err(error) => {
                self.advanced_value.format_undo = None;
                self.advanced_value.error = Some(error.to_string());
            }
        }
        true
    }

    /// 只在来源单元格及编辑基线仍与打开时一致时合并类型化草稿。
    fn advanced_value_stage(&mut self, response: &Value) -> Result<()> {
        use datapier_core::model::Value as DataValue;
        let source = self
            .advanced_value
            .source
            .clone()
            .ok_or_else(|| crate::AppError::invalid("值来源已失效"))?;
        if source["draft_revision"].as_u64() != Some(self.draft_revision) {
            return Err(crate::AppError::conflict("草稿已变化，请重新打开值编辑器"));
        }
        if source["session"] != json!(self.session_id())
            || source["target"] != json!(self.target)
            || source["edit_token"] != json!(self.edit_token)
            || self.edit_token.is_none()
        {
            return Err(crate::AppError::conflict("值编辑基线已变化"));
        }
        if response.get("value_session").and_then(Value::as_str)
            != self.advanced_value.session.as_deref()
            || response.get("revision").and_then(Value::as_u64)
                != Some(self.advanced_value.revision)
        {
            return Err(crate::AppError::conflict("值草稿响应已过期"));
        }
        if response.get("result_id") != Some(&source["result_id"])
            || response.get("row_no") != Some(&source["row"])
            || response.get("column") != Some(&source["column_name"])
        {
            return Err(crate::AppError::conflict("值草稿目标不匹配"));
        }
        let row = source["row"]
            .as_u64()
            .ok_or_else(|| crate::AppError::invalid("缺少行"))?;
        let column = source["column"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| crate::AppError::invalid("缺少列"))?;
        let cell = Cell { row, column };
        let grid = self
            .grid
            .as_ref()
            .ok_or_else(|| crate::AppError::invalid("结果已关闭"))?;
        let edits: Vec<_> = grid
            .edits()
            .iter()
            .map(|(cell, value)| json!([cell.row, cell.column, value]))
            .collect();
        if json!(grid.page.result.id) != source["result_id"]
            || grid
                .page
                .result
                .columns
                .get(column)
                .is_none_or(|current| Some(current.name.as_str()) != source["column_name"].as_str())
            || grid
                .value(cell)
                .is_none_or(|current| json!(current) != source["original"])
            || json!(edits) != source["grid_edits"]
            || json!(self.pending_rows) != source["pending_rows"]
            || source["redis_revision"].as_u64() != Some(self.redis_changes_revision)
        {
            return Err(crate::AppError::conflict("值目标或草稿已变化"));
        }
        if self.is_redis() {
            let mut changes = response
                .get("redis_changes")
                .and_then(Value::as_array)
                .filter(|items| items.iter().all(Value::is_object))
                .cloned()
                .ok_or_else(|| crate::AppError::invalid("缺少 Redis 草稿操作"))?;
            if changes.is_empty() {
                self.status = i18n::UiMessage::key("没有变化");
                return Ok(());
            }
            if self
                .schema
                .as_ref()
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str)
                == Some("list")
                && source["column_name"].as_str() == Some("value")
            {
                let index_column = grid
                    .page
                    .result
                    .columns
                    .iter()
                    .position(|c| c.name == "index")
                    .ok_or_else(|| crate::AppError::invalid("Redis 行缺少索引"))?;
                let original = match grid.original(Cell {
                    row,
                    column: index_column,
                }) {
                    Some(DataValue::Integer(index)) => index
                        .parse::<i64>()
                        .map_err(|_| crate::AppError::invalid("List 原始索引无效"))?,
                    _ => return Err(crate::AppError::invalid("Redis 行缺少索引")),
                };
                if changes.len() != 1
                    || changes[0].get("operation").and_then(Value::as_str) != Some("list_set")
                    || changes[0].get("index").and_then(Value::as_i64) != Some(original)
                {
                    return Err(crate::AppError::invalid("List 值草稿无效"));
                }
                let index = self
                    .redis_list_index(original, false)
                    .map_err(|error| crate::AppError::invalid(error.render().into_owned()))?;
                changes[0]["index"] = json!(index);
            }
            if self
                .schema
                .as_ref()
                .and_then(|schema| schema.get("type"))
                .and_then(Value::as_str)
                == Some("zset")
                && source["column_name"].as_str() == Some("member")
            {
                // 单操作在 Engine 内重新读取 score 并执行身份碰撞检查，不保存旧 score。
                if changes.len() != 1
                    || changes[0].get("operation").and_then(Value::as_str)
                        != Some("zset_replace_member")
                    || changes[0].get("preserve_score").and_then(Value::as_bool) != Some(true)
                    || changes[0].get("member").is_none_or(Value::is_null)
                    || changes[0].get("new_member").is_none_or(Value::is_null)
                    || changes[0].get("score").is_some()
                {
                    return Err(crate::AppError::invalid("ZSet 成员草稿无效"));
                }
            }
            if self
                .schema
                .as_ref()
                .and_then(|schema| schema.get("type"))
                .and_then(Value::as_str)
                == Some("set")
                && source["column_name"].as_str() == Some("member")
                && (changes.len() != 1
                    || changes[0].get("operation").and_then(Value::as_str)
                        != Some("set_replace_member")
                    || changes[0].get("member").is_none_or(Value::is_null)
                    || changes[0].get("new_member").is_none_or(Value::is_null))
            {
                return Err(crate::AppError::invalid("Set 成员草稿无效"));
            }
            for change in &changes {
                let operation = change
                    .get("operation")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                for name in ["field", "member", "new_member", "value"] {
                    let Some(value) = change
                        .get(name)
                        .filter(|value| value.get("type").and_then(Value::as_str) == Some("large"))
                    else {
                        continue;
                    };
                    let id = value
                        .pointer("/value/id")
                        .and_then(Value::as_str)
                        .and_then(|id| Uuid::parse_str(id).ok())
                        .ok_or_else(|| crate::AppError::invalid("Redis 大值标识无效"))?;
                    let identity = name == "field"
                        || (name == "member"
                            && matches!(
                                operation,
                                "set_remove"
                                    | "set_replace_member"
                                    | "zset_remove"
                                    | "zset_replace_member"
                            ));
                    let descriptor = if identity {
                        change.get("identity_edit_file")
                    } else {
                        Some(change)
                    };
                    if !descriptor.is_some_and(|file| {
                        file.get("edit_file_id")
                            .and_then(Value::as_str)
                            .and_then(|id| Uuid::parse_str(id).ok())
                            == Some(id)
                            && file
                                .get("edit_file_revision")
                                .and_then(Value::as_u64)
                                .is_some_and(|revision| revision > 0)
                    }) {
                        return Err(crate::AppError::invalid("Redis 大值或身份缺少受控文件登记"));
                    }
                }
            }
            let start = self.redis_changes.len();
            self.workbench
                .redis_undo_groups
                .retain(|&(_, end)| end <= start);
            self.redis_changes.extend(changes);
            self.redis_changes_revision = self.redis_changes_revision.wrapping_add(1);
            self.workbench
                .redis_undo_groups
                .push((start, self.redis_changes.len()));
        } else {
            let value_session = self
                .advanced_value
                .session
                .as_deref()
                .and_then(|id| Uuid::parse_str(id).ok())
                .ok_or_else(|| crate::AppError::invalid("值会话无效"))?;
            let value: DataValue = serde_json::from_value(
                response
                    .get("value")
                    .cloned()
                    .ok_or_else(|| crate::AppError::invalid("值草稿缺少类型化内容"))?,
            )?;
            self.check_rows_not_deleted([row])
                .map_err(|e| crate::AppError::invalid(e.render().into_owned()))?;
            let grid = self
                .grid
                .as_mut()
                .ok_or_else(|| crate::AppError::invalid("结果已关闭"))?;
            if !grid.editable {
                return Err(crate::AppError::invalid("目标不可编辑"));
            }
            if matches!(value, DataValue::Large { .. }) {
                let id = response
                    .get("edit_file_id")
                    .and_then(Value::as_str)
                    .and_then(|id| Uuid::parse_str(id).ok())
                    .ok_or_else(|| crate::AppError::invalid("大值草稿缺少文件登记"))?;
                let revision = response
                    .get("edit_file_revision")
                    .and_then(Value::as_u64)
                    .filter(|revision| *revision > 0)
                    .ok_or_else(|| crate::AppError::invalid("大值草稿缺少文件版本"))?;
                grid.edit_file(cell, value, id, revision)?;
            } else {
                grid.edit_typed(cell, value)?;
            }
            if !grid.bind_value_source(cell, value_session, self.advanced_value.revision) {
                self.status = i18n::UiMessage::key("没有变化");
                return Ok(());
            }
        }
        // 提交引用固定该 revision；继续修改应通过新会话处理。
        self.advanced_value.writable = false;
        self.advanced_value.staged = true;
        self.draft_revision = self.draft_revision.wrapping_add(1);
        self.dirty = true;
        self.status = i18n::UiMessage::key("已加入待提交变更");
        Ok(())
    }

    /// runtime 在 UI 线程调用并将完整 PNG 注册为宿主 blob。
    pub(crate) fn advanced_value_take_png(&mut self) -> Option<Vec<u8>> {
        self.advanced_value.take_preview_png()
    }
}
