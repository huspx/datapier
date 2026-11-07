//! 应用初始状态与字段路由。
use super::*;

fn apply_draft_key(
    text: &mut String,
    cursor: &mut usize,
    anchor: &mut usize,
    key: &tern_sdk::Key,
) -> bool {
    let Some(mut start) = editor::utf16_to_byte(text, (*cursor).min(*anchor)) else {
        return false;
    };
    let Some(mut end) = editor::utf16_to_byte(text, (*cursor).max(*anchor)) else {
        return false;
    };
    let replacement = match key.name.as_str() {
        "backspace" => {
            if start == end && start > 0 {
                start = text[..start]
                    .char_indices()
                    .last()
                    .map_or(0, |(index, _)| index);
            }
            ""
        }
        "delete" => {
            if start == end && end < text.len() {
                end += text[end..].chars().next().unwrap().len_utf8();
            }
            ""
        }
        "enter" => "\n",
        "paste" => key.text.as_deref().unwrap_or(""),
        _ => match key.typed() {
            Some(text) => text,
            None => return false,
        },
    };
    let next = text[..start].encode_utf16().count() + replacement.encode_utf16().count();
    text.replace_range(start..end, replacement);
    *cursor = next;
    *anchor = next;
    true
}

impl App {
    pub(super) fn field(&self, name: &str) -> &str {
        if name == "cli_text" {
            self.tabs
                .get(self.active)
                .map_or("", |tab| tab.cli_query.text.as_str())
        } else if name == "cell" {
            &self.workbench.cell_editor.text
        } else if name == "row_values" {
            &self.workbench.sql_forms.row_json.text
        } else {
            self.fields.get(name).map(|x| x.text.as_str()).unwrap_or("")
        }
    }
    pub(super) fn editor(&mut self, name: &str) -> Option<&mut Editor> {
        if name == "replace-pattern" || name == "replace-replacement" {
            return self.replace_editor(name);
        }
        if name == "query" {
            self.tabs.get_mut(self.active).map(|x| &mut x.query)
        } else if name == "cli_text" {
            self.tabs.get_mut(self.active).map(|tab| &mut tab.cli_query)
        } else if name == "cell" {
            Some(&mut self.workbench.cell_editor)
        } else if name == "row_values" || name.starts_with("sql-form-") {
            self.workbench.form_editor(name)
        } else {
            self.fields.get_mut(name)
        }
    }
    pub(super) fn field_set(&mut self, name: &str, text: impl Into<String>) {
        if self
            .workbench
            .field_error
            .as_ref()
            .is_some_and(|error| error.field == name)
        {
            self.workbench.field_error = None;
        }
        let text = text.into();
        let editor = Editor::new(text);
        if name == "cli_text" {
            if let Some(tab) = self.tabs.get_mut(self.active) {
                tab.cli_query = editor;
            }
        } else if name == "cell" {
            self.workbench.cell_editor = editor;
        } else if name == "row_values" {
            self.workbench.sql_forms.row_json = editor;
        } else {
            self.fields.insert(name.into(), editor);
        }
        if name == "row_search" {
            self.sync_grid_visible_rows();
        }
    }
    pub(super) fn has_live_drafts(&self) -> bool {
        self.grid.as_ref().is_some_and(|g| !g.edits().is_empty())
            || !self.pending_rows.is_empty()
            || !self.redis_changes.is_empty()
            || self
                .tabs
                .get(self.active)
                .is_some_and(|tab| self.redis_key_delete_tab_pending(tab.id))
            || self.paste_preview.is_some()
    }

    fn draft_scope_context(&self) -> Option<(Uuid, Value, Value, u64, Option<Value>)> {
        self.profile.as_ref().zip(self.session_id()).zip(self.target.as_ref())
            .filter(|(_, target)| matches!(target.kind.as_str(), "table" | "view"))
            .map(|((profile, session), target)| (profile.id, session,
                json!({"database":target.database.as_deref().or(self.database.as_deref()).or(profile.default_database()).unwrap_or_default(),"name":target.name}), self.generation, self.edit_token.clone()))
    }

    pub(super) fn draft_input_field(&self, name: &str) -> bool {
        name == "cell"
            || name == "row_values"
            || name.starts_with("sql-form-row-")
            || (name == "advanced-value" && self.advanced_value.writable)
    }

    pub(super) fn draft_scope_ready(&self) -> bool {
        if !self.workbench.draft_scope_recovery.is_empty() {
            return false;
        }
        let Some(target) = self
            .target
            .as_ref()
            .filter(|target| matches!(target.kind.as_str(), "table" | "view"))
        else {
            return true;
        };
        let Some((profile, session)) = self.profile.as_ref().zip(
            self.session
                .as_ref()
                .and_then(|session| session.get("session_id")),
        ) else {
            return false;
        };
        let database = target
            .database
            .as_deref()
            .or(self.database.as_deref())
            .or(profile.default_database())
            .unwrap_or_default();
        self.workbench
            .draft_scope
            .as_ref()
            .is_some_and(|(p, s, t, generation, token)| {
                *p == profile.id
                    && s == session
                    && t.get("database").and_then(Value::as_str) == Some(database)
                    && t.get("name").and_then(Value::as_str) == Some(target.name.as_str())
                    && *generation == self.generation
                    && *token == self.edit_token
            })
    }

    fn ensure_draft_batch(&mut self, field: &str) {
        if self.workbench.draft_scope_pending.as_ref().and_then(|p| p.actions.last())
            .is_some_and(|last| matches!(last, workbench::DraftScopeAction::EditBatch { field: previous, .. } if previous == field)) { return; }
        let editor = if field == "advanced-value" {
            Some(&self.advanced_value.editor)
        } else {
            self.editor(field).map(|editor| &*editor)
        };
        if let Some(editor) = editor {
            let original = editor.text.clone();
            let cursor = editor.cursor;
            let anchor = editor.anchor;
            self.workbench
                .draft_scope_pending
                .as_mut()
                .unwrap()
                .actions
                .push(workbench::DraftScopeAction::EditBatch {
                    field: field.into(),
                    text: original.clone(),
                    original,
                    cursor,
                    anchor,
                });
        }
    }

    fn queue_draft_scope_action(&mut self, action: workbench::DraftScopeAction) {
        if let workbench::DraftScopeAction::Edit(Msg::Edit(
            field,
            from,
            to,
            replacement,
            cursor,
            len,
        )) = &action
        {
            self.ensure_draft_batch(field);
            if let Some(workbench::DraftScopeAction::EditBatch {
                field: previous,
                text,
                cursor: current,
                anchor,
                ..
            }) = self
                .workbench
                .draft_scope_pending
                .as_mut()
                .unwrap()
                .actions
                .last_mut()
                && previous == field
                && text.encode_utf16().count() == *len
                && let (Some(start), Some(end)) = (
                    editor::utf16_to_byte(text, *from),
                    editor::utf16_to_byte(text, *to),
                )
                && start <= end
            {
                text.replace_range(start..end, replacement);
                *current = *cursor;
                *anchor = *cursor;
                return;
            }
        }
        if let workbench::DraftScopeAction::Key(field, key) = &action
            && field != "advanced-value"
        {
            self.ensure_draft_batch(field);
            if let Some(workbench::DraftScopeAction::EditBatch {
                field: previous,
                text,
                cursor,
                anchor,
                ..
            }) = self
                .workbench
                .draft_scope_pending
                .as_mut()
                .unwrap()
                .actions
                .last_mut()
                && previous == field
                && apply_draft_key(text, cursor, anchor, key)
            {
                return;
            }
        }
        let actions = &mut self.workbench.draft_scope_pending.as_mut().unwrap().actions;
        if !matches!((&action, actions.last()), (workbench::DraftScopeAction::Action(name), Some(workbench::DraftScopeAction::Action(existing))) if existing == name)
        {
            actions.push(action);
        }
    }

    /// 只有服务器确认当前会话、目标和代次的登记后，才能执行数据编辑。
    pub(super) fn guard_draft_scope(
        &mut self,
        backend: &Backend,
        action: workbench::DraftScopeAction,
    ) -> bool {
        if !self.workbench.draft_scope_recovery.is_empty() {
            self.status = i18n::UiMessage::key("请先查看并处理未确认输入，再重新打开基线");
            return false;
        }
        let Some((profile, session, target, generation, edit_token)) = self.draft_scope_context()
        else {
            if self
                .target
                .as_ref()
                .is_some_and(|target| matches!(target.kind.as_str(), "table" | "view"))
            {
                self.workbench
                    .draft_scope_recovery
                    .push(workbench::DraftScopePending {
                        profile: self
                            .profile
                            .as_ref()
                            .map_or(Uuid::nil(), |profile| profile.id),
                        session: Value::Null,
                        target: json!(self.target),
                        generation: self.generation,
                        edit_token: self.edit_token.clone(),
                        result_id: self.result_id.clone(),
                        overlay: self.workbench.overlay.clone(),
                        grid_focus: self.grid.as_ref().and_then(|grid| grid.focus),
                        revision: 0,
                        request: None,
                        error: Some(AppError::new("ipc_closed", "连接不可用，请重新打开基线")),
                        actions: vec![action],
                    });
                self.status = i18n::UiMessage::key("连接不可用，未确认输入已保留，请重新打开基线");
                return false;
            }
            return true;
        };
        if self.workbench.draft_scope.as_ref()
            == Some(&(
                profile,
                session.clone(),
                target.clone(),
                generation,
                edit_token.clone(),
            ))
        {
            return true;
        }
        self.sync_active_draft_scope(backend);
        if !self.workbench.draft_scope_recovery.is_empty() {
            self.status = i18n::UiMessage::key("请先查看并处理未确认输入，再重新打开基线");
            return false;
        }
        let matches = self
            .workbench
            .draft_scope_pending
            .as_ref()
            .is_some_and(|pending| {
                pending.profile == profile
                    && pending.session == session
                    && pending.target == target
                    && pending.generation == generation
                    && pending.edit_token == edit_token
                    && pending.result_id == self.result_id
                    && pending.overlay == self.workbench.overlay
                    && pending.grid_focus == self.grid.as_ref().and_then(|grid| grid.focus)
            });
        if !matches {
            self.workbench.draft_scope_pending = Some(workbench::DraftScopePending {
                profile,
                session,
                target,
                generation,
                edit_token,
                result_id: self.result_id.clone(),
                overlay: self.workbench.overlay.clone(),
                grid_focus: self.grid.as_ref().and_then(|grid| grid.focus),
                revision: 0,
                request: None,
                error: None,
                actions: Vec::new(),
            });
        }
        self.queue_draft_scope_action(action);
        let pending = self.workbench.draft_scope_pending.as_mut().unwrap();
        if pending.request.is_none() {
            self.workbench.draft_scope_serial = self.workbench.draft_scope_serial.saturating_add(1);
            pending.revision = self.workbench.draft_scope_serial;
            let token = Uuid::new_v4();
            let tag = format!(
                "{}:{}:draft-scope/{token}",
                self.tabs[self.active].id, generation
            );
            match backend.request(tag, "workspace.draft_scope", json!({"profile_id":pending.profile,
                "session_id":pending.session,"target":pending.target,"edit_token":pending.edit_token,"dirty":true,"revision":pending.revision})) {
                Ok(()) => { pending.request = Some(token); pending.error = None; }
                Err(error) => {
                    self.status = i18n::UiMessage::raw(format!("草稿尚未取得保护，请重试：{error}"));
                    pending.error = Some(error);
                }
            }
        }
        if pending.request.is_some() {
            self.status = i18n::UiMessage::key("正在确认草稿保护，编辑尚未生效");
        }
        false
    }

    pub(crate) fn draft_scope_unconfirmed(&self) -> bool {
        !self.workbench.draft_scope_recovery.is_empty()
            || self.workbench.draft_scope_pending.is_some()
            || (!self.draft_scope_ready()
                && (self.has_live_drafts()
                    || (self.advanced_value.writable && self.advanced_value.dirty())))
    }

    pub(crate) fn draft_scope_save_check(&self) -> Result<()> {
        if !self.workbench.draft_scope_recovery.is_empty() {
            return Err(AppError::new("busy", "存在未确认输入，请先查看并处理"));
        }
        if let Some(pending) = &self.workbench.draft_scope_pending {
            return Err(pending
                .error
                .clone()
                .unwrap_or_else(|| AppError::new("busy", "正在确认草稿保护")));
        }
        if self.draft_scope_unconfirmed() {
            return Err(AppError::new("busy", "草稿未取得保护，请重新确认基线"));
        }
        Ok(())
    }

    pub(super) fn cancel_pending_draft_scope(&mut self, backend: &Backend) {
        if let Some(pending) = self.workbench.draft_scope_pending.take() {
            self.enqueue_draft_scope_release(pending.profile, pending.session, pending.target);
            self.send_draft_scope_releases(backend);
        }
    }

    fn enqueue_draft_scope_release(&mut self, profile: Uuid, session: Value, target: Value) {
        self.workbench.draft_scope_serial = self.workbench.draft_scope_serial.saturating_add(1);
        self.workbench
            .draft_scope_releases
            .push(workbench::DraftScopeRelease {
                profile,
                session,
                target,
                revision: self.workbench.draft_scope_serial,
                request: None,
                retry_at: Instant::now(),
            });
    }

    fn send_draft_scope_releases(&mut self, backend: &Backend) {
        for release in &mut self.workbench.draft_scope_releases {
            if release.request.is_some() || Instant::now() < release.retry_at {
                continue;
            }
            let token = Uuid::new_v4();
            let tag = format!("draft-scope-release/{}/{token}", self.tabs[self.active].id);
            match backend.request(tag, "workspace.draft_scope", json!({"profile_id":release.profile,
                "session_id":release.session,"target":release.target,"dirty":false,"revision":release.revision})) {
                Ok(()) => release.request = Some(token),
                Err(error) => {
                    release.retry_at = Instant::now() + Duration::from_secs(2);
                    self.status = i18n::UiMessage::raw(format!("草稿保护释放失败，将重试：{error}"));
                }
            }
        }
    }

    pub(super) fn draft_scope_release_response(&mut self, token: Uuid, result: &Result<Value>) {
        let Some(index) = self
            .workbench
            .draft_scope_releases
            .iter()
            .position(|release| release.request == Some(token))
        else {
            return;
        };
        let release = &mut self.workbench.draft_scope_releases[index];
        if result.as_ref().is_ok_and(|value| {
            value.get("revision").and_then(Value::as_u64) == Some(release.revision)
        }) || result
            .as_ref()
            .is_err_and(|error| error.code == "stale_revision")
        {
            self.workbench.draft_scope_releases.remove(index);
        } else {
            release.request = None;
            release.retry_at = Instant::now() + Duration::from_secs(2);
            self.status = i18n::UiMessage::key("草稿保护释放未确认，将重试");
        }
    }

    pub(super) fn draft_scope_response(
        &mut self,
        token: &str,
        result: &Result<Value>,
        backend: &Backend,
    ) -> bool {
        let Some(pending) = self.workbench.draft_scope_pending.as_ref() else {
            return true;
        };
        if Uuid::parse_str(token).ok() != pending.request {
            return true;
        }
        if backend.is_closed() {
            if let Some(pending) = self.workbench.draft_scope_pending.as_mut() {
                pending.request = None;
                pending.error = Some(AppError::new("ipc_closed", "Engine 已断开，草稿保护未确认"));
            }
            self.status =
                i18n::UiMessage::key("Engine 已断开，草稿保护未确认，请重新连接并确认基线");
            return true;
        }
        let current = self.draft_scope_context();
        let expected = (
            pending.profile,
            pending.session.clone(),
            pending.target.clone(),
            pending.generation,
            pending.edit_token.clone(),
        );
        if current.as_ref() != Some(&expected)
            || pending.result_id != self.result_id
            || pending.overlay != self.workbench.overlay
            || pending.grid_focus != self.grid.as_ref().and_then(|grid| grid.focus)
        {
            self.sync_active_draft_scope(backend);
            return true;
        }
        if !result.as_ref().is_ok_and(|value| {
            value.get("revision").and_then(Value::as_u64) == Some(pending.revision)
        }) {
            let pending = self.workbench.draft_scope_pending.as_mut().unwrap();
            pending.request = None;
            pending.error =
                Some(result.as_ref().err().cloned().unwrap_or_else(|| {
                    AppError::new("invalid_response", "草稿保护确认版本不匹配")
                }));
            self.status = i18n::UiMessage::raw(match result {
                Err(error) => format!("草稿未取得保护，请保留输入并重新打开基线或重试：{error}"),
                Ok(_) => "草稿保护确认无效，请重试".into(),
            });
            return true;
        }
        let mut pending = self.workbench.draft_scope_pending.take().unwrap();
        self.workbench.draft_scope = Some(expected);
        let actions = std::mem::take(&mut pending.actions);
        for action in actions {
            let mismatched = match &action {
                workbench::DraftScopeAction::EditBatch {
                    field, original, ..
                } => {
                    if field == "advanced-value" {
                        self.advanced_value.editor.text != *original
                    } else {
                        self.editor(field)
                            .is_none_or(|editor| editor.text != *original)
                    }
                }
                _ => false,
            };
            if mismatched || !pending.actions.is_empty() {
                pending.actions.push(action);
                continue;
            }
            match action {
                workbench::DraftScopeAction::Action(name) => self.action(&name, backend),
                workbench::DraftScopeAction::Edit(message) => self.dispatch_editor_message(message),
                workbench::DraftScopeAction::EditBatch {
                    field,
                    original,
                    text,
                    cursor,
                    anchor,
                } => {
                    let len = original.encode_utf16().count();
                    self.dispatch_editor_message(Msg::Edit(
                        field.clone(),
                        0,
                        len,
                        text,
                        cursor,
                        len,
                    ));
                    if field == "advanced-value" {
                        self.advanced_value.editor.set_selection(anchor, cursor);
                    } else if let Some(editor) = self.editor(&field) {
                        editor.set_selection(anchor, cursor);
                    }
                }
                workbench::DraftScopeAction::Key(name, key) => {
                    let focus = std::mem::replace(&mut self.focus, name);
                    self.input_key(&key);
                    self.focus = focus;
                }
                workbench::DraftScopeAction::Protect => {}
            }
        }
        if !pending.actions.is_empty() {
            self.workbench.draft_scope_recovery.push(pending);
            self.status = i18n::UiMessage::key("输入上下文已变化，未确认内容已保留");
        }
        true
    }

    pub(crate) fn sync_draft_scope(&mut self, backend: &Backend) {
        if backend.is_closed() {
            let active = self.active;
            for index in 0..self.tabs.len() {
                self.switch_tab(index);
                if self.workbench.draft_scope.take().is_some()
                    || self.workbench.draft_scope_pending.is_some()
                {
                    self.status =
                        i18n::UiMessage::key("Engine 已断开，草稿保护未确认，请重新连接并确认基线");
                }
                if let Some(pending) = self.workbench.draft_scope_pending.as_mut() {
                    pending.request = None;
                    pending.error =
                        Some(AppError::new("ipc_closed", "Engine 已断开，草稿保护未确认"));
                }
            }
            self.switch_tab(active);
            return;
        }
        let active = self.active;
        for index in 0..self.tabs.len() {
            self.switch_tab(index);
            self.sync_active_draft_scope(backend);
        }
        self.switch_tab(active);
    }

    fn sync_active_draft_scope(&mut self, backend: &Backend) {
        let current = self.draft_scope_context();
        let editing = self.has_live_drafts()
            || self.workbench.inline.is_some()
            || matches!(
                self.workbench.overlay.as_deref(),
                Some("cell" | "row" | "advanced-value")
            );
        let pending_current = self
            .workbench
            .draft_scope_pending
            .as_ref()
            .is_some_and(|pending| {
                current.as_ref()
                    == Some(&(
                        pending.profile,
                        pending.session.clone(),
                        pending.target.clone(),
                        pending.generation,
                        pending.edit_token.clone(),
                    ))
                    && pending.result_id == self.result_id
                    && pending.overlay == self.workbench.overlay
                    && pending.grid_focus == self.grid.as_ref().and_then(|grid| grid.focus)
            });
        let old = if !pending_current {
            self.workbench.draft_scope_pending.take().map(|pending| {
                let scope = (
                    pending.profile,
                    pending.session.clone(),
                    pending.target.clone(),
                    pending.generation,
                    pending.edit_token.clone(),
                );
                if pending
                    .actions
                    .iter()
                    .any(|action| !matches!(action, workbench::DraftScopeAction::Protect))
                {
                    self.workbench.draft_scope_recovery.push(pending);
                    self.status = i18n::UiMessage::key(
                        "编辑上下文已变化，未确认输入已保留，请查看后重新打开基线",
                    );
                }
                scope
            })
        } else {
            None
        };
        let confirmed_old = if self
            .workbench
            .draft_scope
            .as_ref()
            .is_some_and(|scope| current.as_ref() != Some(scope) || !editing)
        {
            self.workbench.draft_scope.take()
        } else {
            None
        };
        for (profile, session, target, _, _) in old.into_iter().chain(confirmed_old) {
            self.enqueue_draft_scope_release(profile, session, target);
        }
        self.send_draft_scope_releases(backend);
    }
    pub(super) fn changed_field(&mut self, name: &str) {
        if self
            .workbench
            .field_error
            .as_ref()
            .is_some_and(|error| error.field == name)
        {
            self.workbench.field_error = None;
        }
        if name == "row_search" {
            self.sync_grid_visible_rows();
        }
        if matches!(
            name,
            "name"
                | "host"
                | "port"
                | "user"
                | "password"
                | "provider"
                | "tls"
                | "ca"
                | "cert"
                | "key"
                | "database"
                | "redis_db"
        ) {
            self.navigation_form_changed();
        }
        if name == "query" {
            self.query_edit_epoch = self.query_edit_epoch.wrapping_add(1);
            self.workbench.query_ui.menu_open = false;
            self.workbench.query_ui.executed = false;
        }
        self.workbench.form_changed(name);
        self.dirty = true;
        // 查询文本失效时清除旧结果；基表数据继续由资源浏览持有，不能随查询草稿丢弃。
        if name == "query" && !self.browse_result && !self.is_redis() {
            self.grid = None;
            self.results.clear();
            self.result_id = None;
            self.page_history.clear();
            self.page_token = None;
            self.large = None;
        }
        if matches!(name, "pattern" | "type") {
            self.scan_token = None;
            self.cursor = None;
            self.workbench.scan_dirty = true;
        }
    }
    /// 视图的每项写操作必须由 Core 独立证明；缺少映射时保持只读。
    pub(super) fn sql_operation_allowed(&self, operation: &str) -> bool {
        match self.target.as_ref().map(|target| target.kind.as_str()) {
            Some("table") => true,
            Some("view") => {
                self.schema
                    .as_ref()
                    .and_then(|schema| schema.get("details"))
                    .and_then(|details| details.get("view_mapping"))
                    .and_then(|mapping| mapping.get(operation))
                    .and_then(Value::as_bool)
                    == Some(true)
            }
            _ => false,
        }
    }
    pub(super) fn new(workspace_id: String) -> Self {
        let mut fields = BTreeMap::new();
        for (name, value) in connections::PROFILE_DEFAULTS.iter().copied().chain([
            ("pattern", "*"),
            ("type", ""),
            ("path", ""),
            ("format", "csv"),
            ("mapping", "{}"),
            ("filter", "[]"),
            ("sort", "[]"),
            ("operation", "set_string"),
            ("redis_value", ""),
            ("redis_field", ""),
            ("redis_member", ""),
            ("redis_index", "0"),
            ("redis_score", "0"),
            ("redis_key", ""),
            ("redis_ttl", ""),
            ("column", "0"),
            ("width", "160"),
            ("object_filter", ""),
            ("history_filter", ""),
            ("row_search", ""),
        ]) {
            fields.insert(name.into(), Editor::new(value));
        }
        fields.insert("null_marker".into(), Editor::new("\\N"));
        fields.insert("backslash_escape".into(), Editor::new("true"));
        fields.insert("binary_columns".into(), Editor::new("[]"));
        Self {
            workspace_id,
            revision: 0,
            workspace_loaded: false,
            workspace_load: None,
            workspace_upload: None,
            saving: false,
            save_blocked: false,
            copy_pending: None,
            dirty: false,
            restore_focus_pending: false,
            workspace_interacted: false,
            tabs: vec![Tab {
                title: "查询 1".into(),
                query: Editor::new(""),
                cli_query: Editor::default(),
                profile_id: None,
                id: Uuid::new_v4(),
                layout: Vec::new(),
                browse: BrowseSettings::default(),
                pending: Value::Null,
                pending_live: false,
                schema_draft: None,
                replace_draft: None,
                runtime: TabRuntime::default(),
            }],
            active: 0,
            fields,
            focus: "query".into(),
            profiles: vec![],
            profile: None,
            empty_context: None,
            profile_list_token: None,
            server_list_token: None,
            profile_saves: Default::default(),
            form_id: None,
            form_editing: false,
            archery: connections::archery::Wizard::default(),
            session: None,
            resources: vec![],
            cursor: None,
            scan_token: None,
            database: None,
            target: None,
            task: None,
            operation_pending: false,
            results: Vec::new(),
            result_id: None,
            page_history: Vec::new(),
            page_token: None,
            grid: None,
            schema: None,
            edit_token: None,
            pending_rows: Vec::new(),
            pending_edit_files: Vec::new(),
            draft_revision: 0,
            restore_rows: None,
            advanced_schema: None,
            advanced_replace: Default::default(),
            advanced_value: Default::default(),
            advanced_files: Default::default(),
            redis_changes: Vec::new(),
            redis_changes_revision: 0,
            large: None,
            preview: None,
            preview_pending: None,
            browse_result: false,
            generation: 0,
            current_pending: None,
            file_open_pending: None,
            query_edit_epoch: 0,
            paste_preview: None,
            status: "正在连接 Engine…".into(),
            detail: String::new(),
            detail_context: DetailContext::default(),
            view: "query".into(),
            workbench: Workbench::default(),
            last_poll: Instant::now(),
            viewport_width: 0,
            cli_share: 0.35,
            navigation: navigation::NavigationState::default(),
            icon_blobs: None,
            settings: AppSettings::default(),
            settings_loaded: false,
            settings_epoch: 0,
            settings_save_token: None,
            settings_unsaved: false,
            editing_files: Default::default(),
            tab_close: None,
            change_preview: change_preview::PreviewState::default(),
            redis_keys: Default::default(),
            redis_key_delete: Default::default(),
            approvals: Default::default(),
            window_data_dir: None,
            last_settings_poll: Instant::now(),
        }
    }
}
