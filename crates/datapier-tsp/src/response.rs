use super::*;

impl App {
    /// 事件是失效提示；只按作用域处理本窗口的标签，不从事件载荷恢复数据。
    fn editing_event(&mut self, kind: &str, payload: &Value, backend: &Backend) {
        if kind == "value_file.changed" {
            self.editing_files_changed(backend);
            return;
        }
        let profile_id = payload["scope"]["profile_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok());
        if kind == "schema.changed" && profile_id.is_none() {
            return;
        }
        let database = payload["scope"]["database"].as_str();
        let id = payload["id"].as_str().unwrap_or("");
        let old = self.active;
        for index in 0..self.tabs.len() {
            if profile_id.is_some_and(|id| self.tabs[index].profile_id != Some(id)) {
                continue;
            }
            self.switch_tab(index);
            if database.is_some_and(|db| self.database.as_deref() != Some(db)) {
                continue;
            }
            if kind == "schema.changed" {
                self.workbench.schema_invalidated_task |=
                    self.operation_pending || self.task.is_some();
                let previous = self.generation;
                self.generation = self.generation.wrapping_add(1);
                let original = self
                    .workbench
                    .value_schema_generations
                    .filter(|(_, current)| *current == previous)
                    .map_or(previous, |(original, _)| original);
                self.workbench.value_schema_generations = Some((original, self.generation));
                self.scan_token = None;
                self.page_token = None;
                self.result_id = None;
                self.page_history.clear();
                self.resources.clear();
                self.schema = None;
                self.edit_token = None;
                self.preview = None;
                self.preview_pending = None;
                self.draft_revision = self.draft_revision.wrapping_add(1);
                self.advanced_replace.invalidate();
                if let Some(schema) = &mut self.advanced_schema {
                    schema.invalidate_schema();
                }
                if let Some(grid) = &mut self.grid {
                    grid.editable = false;
                }
                self.advanced_value_schema_invalidated();
                self.status = i18n::UiMessage::key("结构已变化，请重新读取并确认草稿");
                if let Some(request) = self
                    .advanced_schema
                    .as_mut()
                    .and_then(|schema| schema.describe(true))
                {
                    self.advanced_schema_send(backend, request);
                } else if self.view == "data"
                    && !self.has_live_drafts()
                    && self.advanced_value.session.is_none()
                    && self.task.is_none()
                    && !self.operation_pending
                    && !self
                        .advanced_schema
                        .as_ref()
                        .is_some_and(|schema| schema.has_pending_request())
                    && let Some(target) = self.target.clone()
                {
                    self.load_target(target, backend);
                }
            } else if kind == "edit_plan.changed" {
                let request = self
                    .advanced_schema
                    .as_mut()
                    .and_then(|schema| schema.refresh_plan(id));
                if let Some(request) = request {
                    self.advanced_schema_send(backend, request);
                }
                let route = format!("{}:{}", self.tabs[self.active].id, self.generation);
                self.advanced_replace
                    .refresh_plan_event(id, backend, &route);
            }
        }
        self.switch_tab(old);
        if kind == "schema.changed" {
            if self
                .profile
                .as_ref()
                .is_some_and(|profile| Some(profile.id) == profile_id)
            {
                self.navigation_load_tree(backend, true);
            } else {
                if let Some(profile_id) = profile_id {
                    self.navigation.databases.remove(&profile_id);
                }
            }
        }
    }

    pub(super) fn archery_auth_required(
        &mut self,
        code: Option<&str>,
        details: &Value,
        backend: &Backend,
    ) {
        if code != Some("auth_required") {
            return;
        }
        let server_id = details
            .get("server_id")
            .and_then(Value::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
            .or_else(|| {
                self.profile
                    .as_ref()
                    .and_then(|profile| profile.archery())
                    .map(|source| source.server_id)
            });
        if let Some(server_id) = server_id {
            self.archery_reauthenticate(server_id, backend);
        }
    }

    pub(super) fn page(&mut self, backend: &Backend, start: u64) {
        if let Some(id) = self.result_id.clone() {
            let size = self.result_page_size(&id);
            let token = Uuid::new_v4();
            let tag = format!(
                "{}:{}:result.page/{token}",
                self.tabs[self.active].id, self.generation
            );
            match backend.request(
                tag,
                "result.read",
                json!({"result_id":id,"start":start,"max_rows":size,"max_bytes":1048576}),
            ) {
                Ok(()) => self.page_token = Some(token),
                Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
            }
        }
    }
    pub(super) fn execute_current_statement(
        &mut self,
        query: &editor::Editor,
        context: editor::SqlContext,
        backend: &Backend,
    ) {
        match editor::current_statement_in(&query.text, query.cursor, context) {
            Ok(range) => {
                if let (Some(start), Some(end)) = (
                    editor::utf16_to_byte(&query.text, range.start),
                    editor::utf16_to_byte(&query.text, range.end),
                ) {
                    self.execute_query_text(query.text[start..end].to_owned(), backend);
                }
            }
            Err(error) => self.status = i18n::UiMessage::local(error),
        }
    }
    pub(super) fn response(&mut self, response: Response, backend: &Backend) {
        if response.tag.starts_with("workspace.load/")
            || response.tag.starts_with("workspace.load.read/")
            || response.tag == "workspace.load.cleanup"
        {
            self.workspace_load_response(response, backend);
            return;
        }
        // 认证句柄释放属于内部清理，其 ACK 不能覆盖登录状态或结果详情。
        if response.tag == "archery-release" {
            return;
        }
        // 窗口租约续期的成功回执不能覆盖用户正在查看的执行状态。
        if matches!(response.tag.as_str(), "window.bind" | "window.active")
            && response.result.is_ok()
        {
            return;
        }
        if response.result.is_err()
            && response.tag.starts_with("large.value/")
            && !self.large.as_ref().is_some_and(|load| {
                response.tag == format!("large.value/{}/{}", load.id, load.offset)
            })
        {
            return;
        }
        let open = if response.tag == "connection.open"
            && self
                .archery
                .open_capture
                .as_ref()
                .is_some_and(|open| self.archery_open_context_matches(open))
        {
            self.archery.open_capture.take()
        } else {
            None
        };
        let value = match response.result {
            Ok(v) => v,
            Err(e) => {
                self.archery_auth_required(Some(&e.code), &e.details, backend);
                if response.tag == "connection.open"
                    && e.code == "auth_required"
                    && let Some(profile) = self.profile.as_ref().filter(|p| {
                        p.archery()
                            .is_some_and(|a| Some(a.server_id) == self.archery.reauth_server)
                    })
                {
                    self.archery.connection_auth_error =
                        Some((self.tabs[self.active].id, self.generation, profile.id));
                }
                if e.code == "auth_required"
                    && let Some(open) = open.filter(|open| self.archery_open_context_matches(open))
                    && self.archery.reauth_server
                        == self
                            .profile
                            .as_ref()
                            .and_then(|p| p.archery())
                            .map(|source| source.server_id)
                    && self.archery.reauth_server.is_some()
                {
                    self.archery.open_continuation = Some(open);
                }
                if response.tag == "change.submit" {
                    self.redis_key_delete_submit_failed();
                }
                if self.workbench_error(&response.tag, &e) {
                    return;
                }
                if response.tag == "current.info" {
                    self.current_pending = None;
                } else if response.tag == "file.open" {
                    self.file_open_pending = None;
                }
                self.status = i18n::UiMessage::raw(e.to_string());
                if response.tag == "connection.open" {
                    self.workbench.connecting = false;
                    self.workbench.connection_error = Some(self.status.render().into_owned());
                    self.workbench.connection_tls_required = e.code == "tls_required";
                } else if matches!(response.tag.as_str(), "profile.save" | "profile.test") {
                    self.workbench.profile_error = Some(self.status.render().into_owned());
                }
                if response.tag == "workspace.save" {
                    self.saving = false;
                    self.dirty = true;
                    self.save_blocked = true;
                }
                if response.tag == "workspace.copy" {
                    self.copy_pending = None;
                }
                if response.tag == "transfer.preview" {
                    self.preview_pending = None;
                    self.preview = None;
                }
                if response.tag.starts_with("large.value/") {
                    self.large = None;
                    self.workbench.inline = None;
                    self.workbench.redis_loading = None;
                }
                if response.tag == "resource.describe" {
                    self.workbench.load_resource = false;
                }
                if matches!(
                    response.tag.as_str(),
                    "connection.open"
                        | "execution.start"
                        | "archery.sql_check"
                        | "resource.browse"
                        | "change.submit"
                        | "transfer.import"
                        | "transfer.export"
                ) {
                    self.operation_pending = false;
                    self.workbench.schema_invalidated_task = false;
                }
                return;
            }
        };
        if response.tag.starts_with("cli.")
            || (self.workbench.cli_running && response.tag == "execution.get")
        {
            self.workbench_response(&response.tag, value, backend);
            return;
        }
        if response.tag.starts_with("large.value/") {
            self.large_response(&response.tag, value, backend);
            return;
        }
        match response.tag.as_str() {
            // 事件仅提示查询任务状态；事件丢失或早于任务 ID 返回时由周期轮询补足。
            "execution.event" => {
                if let Some(
                    kind @ ("schema.changed" | "edit_plan.changed" | "value_file.changed"),
                ) = value.get("kind").and_then(Value::as_str)
                {
                    self.editing_event(kind, &value["payload"], backend);
                    return;
                }
                if value.get("kind").and_then(Value::as_str) == Some("approval.changed") {
                    self.approval_refresh(backend);
                    return;
                }
                if value.get("kind").and_then(Value::as_str) == Some("settings.changed") {
                    self.refresh_settings(backend);
                    return;
                }
                if value.get("kind").and_then(Value::as_str) == Some("connections.changed") {
                    self.request(backend, "profile.list", "profile.list", json!({}));
                    self.refresh_archery_servers(backend);
                    return;
                }
                if value.get("kind").and_then(Value::as_str) == Some("archery.auth.changed") {
                    if let Some(server_id) = value["payload"]["server_id"]
                        .as_str()
                        .and_then(|id| Uuid::parse_str(id).ok())
                    {
                        self.archery_auth_changed(server_id, backend);
                    }
                    return;
                }
                if value.get("kind").and_then(Value::as_str) == Some("execution.state")
                    && let Some(task) = value.get("task_id").and_then(Value::as_str)
                    && self.task.as_deref() == Some(task)
                {
                    self.request(
                        backend,
                        &format!("execution.get/{task}"),
                        "execution.get",
                        json!({"task_id": task}),
                    );
                }
            }
            "connect" => {
                self.status = "已连接 Engine".into();
                self.refresh_settings(backend);
                self.refresh_archery_servers(backend);
                self.request(backend, "profile.list", "profile.list", json!({}));
                self.start_workspace_load(backend);
            }
            "profile.list" => {
                self.profiles =
                    serde_json::from_value(value.get("profiles").cloned().unwrap_or_default())
                        .unwrap_or_default();
                self.restore_empty_context(true);
                self.migrate_cli_queries();
                if self.profile.is_none() {
                    self.profile = self
                        .tabs
                        .get(self.active)
                        .and_then(|tab| tab.profile_id)
                        .and_then(|id| self.profiles.iter().find(|p| p.id == id).cloned());
                }
                if self.restore_focus_pending {
                    self.restore_focus_pending = false;
                    self.focus_current_tab();
                }
                self.navigation_load_tree(backend, false);
            }
            "profile.save" => self.profile_saved(value, backend),
            "profile.test" => {
                self.workbench.profile_error = None;
                self.status = i18n::UiMessage::raw(value.to_string());
            }
            "workspace.load" => {
                let result = value["revision"]
                    .as_u64()
                    .ok_or_else(|| AppError::invalid("工作区版本无效"))
                    .and_then(|revision| {
                        self.restore_workspace_state(revision, value["state"].clone())
                    });
                if let Err(error) = result {
                    self.status = i18n::UiMessage::raw(error.to_string());
                }
            }
            "workspace.save" => {
                self.revision = value["revision"].as_u64().unwrap_or(self.revision);
                self.saving = false;
            }
            "workspace.copy" => {
                if let Some(copy) = self.copy_pending.take() {
                    let original = std::mem::replace(&mut self.workspace_id, copy.clone());
                    self.revision = value["revision"].as_u64().unwrap_or(1);
                    self.save_blocked = false;
                    self.dirty = true;
                    self.status = i18n::UiMessage::template(
                        "已另存工作区副本 {copy}；原工作区 {original} 未覆盖",
                        vec![
                            ("copy", i18n::UiArg::Raw(copy)),
                            ("original", i18n::UiArg::Raw(original)),
                        ],
                    );
                }
            }
            "connection.open" => {
                self.operation_pending = false;
                self.workbench.connecting = false;
                match serde_json::from_value::<Profile>(
                    value.get("profile").cloned().unwrap_or(Value::Null),
                ) {
                    Ok(profile) => {
                        self.workbench.connection_error = None;
                        self.tabs[self.active].profile_id = Some(profile.id);
                        let redis = profile.provider == datapier_core::model::ProviderKind::Redis;
                        let archery_redis = redis && profile.is_archery();
                        if redis {
                            let db = value
                                .get("redis_db")
                                .and_then(Value::as_u64)
                                .and_then(|db| u32::try_from(db).ok())
                                .unwrap_or_else(|| profile.redis_db());
                            self.tabs[self.active].browse.redis_db = Some(db);
                            self.database = Some(db.to_string());
                        } else {
                            self.database = value
                                .get("database")
                                .and_then(Value::as_str)
                                .map(str::to_owned)
                                .or_else(|| profile.default_database().map(str::to_owned));
                        }
                        self.profile = Some(profile);
                        self.session = Some(value);
                        self.workbench.cli_database = None;
                        self.workbench.sql_mode = None;
                        self.status = "数据库连接已打开".into();
                        if archery_redis {
                            self.workbench.queued_target = None;
                            self.target = None;
                            self.workbench.cli = true;
                            self.view = "data".into();
                            self.focus = "cli_text".into();
                            self.workbench.focus_request = Some("input-cli_text".into());
                        } else {
                            if let Some(target) = self.workbench.queued_target.take() {
                                if target.kind == "database" {
                                    if self
                                        .session
                                        .as_ref()
                                        .and_then(|session| session.get("database"))
                                        .and_then(Value::as_str)
                                        .is_none()
                                    {
                                        self.database =
                                            (!target.name.is_empty()).then_some(target.name);
                                    }
                                } else {
                                    self.load_target(target, backend);
                                }
                            }
                            self.navigation_apply_queued_section();
                            if self.workbench.section == "structure" {
                                self.action("section:structure", backend);
                            }
                            self.action("list", backend);
                            self.refresh_query_mode(backend);
                            self.navigation_load_tree(backend, false);
                        }
                    }
                    Err(e) => {
                        let error = format!("数据库会话缺少有效连接信息: {e}");
                        self.workbench.connection_error = Some(error.clone());
                        self.status = i18n::UiMessage::raw(error);
                    }
                }
            }
            "resource.list" => match serde_json::from_value::<ResourcePage>(value) {
                Ok(page) => {
                    if self.is_redis() {
                        let owner = self.redis_selection_scope();
                        if self.workbench.redis_tree_owner != owner {
                            self.resources.clear();
                            self.workbench.redis_tree.reset(&[]);
                            self.workbench.redis_tree_owner = owner;
                        } else if self.workbench.redis_tree.len() != self.resources.len() {
                            self.workbench.redis_tree.reset(&self.resources);
                        }
                        self.workbench.redis_tree.append(&page.resources);
                        // Core 的扫描缓存按原始 key 去重，各页只发出尚未交付的项。
                        self.resources.extend(page.resources);
                        if self.workbench.tree {
                            self.normalize_key_page();
                        } else {
                            self.workbench.key_page =
                                self.resources.len().saturating_sub(1) / workbench::KEY_PAGE_SIZE;
                        }
                    } else {
                        self.workbench.redis_tree.reset(&[]);
                        self.workbench.redis_tree_owner = None;
                        self.resources = page.resources;
                    }
                    self.cursor = page.cursor;
                }
                Err(e) => self.status = i18n::UiMessage::raw(e.to_string()),
            },
            "transfer.preview" => {
                let valid = value
                    .get("errors")
                    .and_then(Value::as_array)
                    .is_some_and(Vec::is_empty);
                self.detail = serde_json::to_string_pretty(&value).unwrap_or_default();
                self.set_detail_context("导入预览");
                self.preview = if valid {
                    self.preview_pending.take().map(|(_, p)| p)
                } else {
                    self.preview_pending = None;
                    None
                };
                self.status = if valid {
                    "预览完成；检查后可确认导入".into()
                } else {
                    "预览发现错误，请修正后重新预览".into()
                };
            }
            "resource.describe" => {
                if let (Some(target), Some(remote)) = (self.target.as_mut(), value.get("target")) {
                    let same = remote.get("name").and_then(Value::as_str)
                        == Some(target.name.as_str())
                        && remote.get("database").and_then(Value::as_str)
                            == target.database.as_deref()
                        && remote.get("key_b64").and_then(Value::as_str)
                            == target.key_b64.as_deref();
                    if same {
                        let kind = remote
                            .get("kind")
                            .and_then(Value::as_str)
                            .filter(|kind| matches!(*kind, "table" | "view"))
                            .or_else(|| {
                                value
                                    .get("details")
                                    .and_then(|d| d.get("relation_kind"))
                                    .and_then(Value::as_str)
                                    .filter(|kind| matches!(*kind, "table" | "view"))
                            });
                        if let Some(kind) = kind {
                            target.kind = kind.to_owned();
                        }
                    }
                }
                self.schema = Some(value.clone());
                self.detail = value.to_string();
                self.set_detail_context("资源详情");
                if std::mem::take(&mut self.workbench.load_resource) {
                    let writable = self.profile.as_ref().is_some_and(|p| {
                        p.access_mode == datapier_core::model::AccessMode::ReadWrite
                    });
                    let editable = writable
                        && (self.is_redis()
                            || (["insert", "update", "delete"]
                                .iter()
                                .any(|op| self.sql_operation_allowed(op))
                                && self
                                    .schema
                                    .as_ref()
                                    .and_then(|s| s["keys"].as_array())
                                    .is_some_and(|keys| !keys.is_empty())));
                    self.action(if editable { "edit-open" } else { "browse" }, backend);
                }
            }
            "query.info" => {
                self.workbench.sql_mode = application::mysql_mode(&value);
                if self.workbench.sql_mode.is_none() {
                    self.status = i18n::UiMessage::key("数据库未返回 sql_mode，无法安全处理选区");
                }
            }
            "current.info" => {
                if let Some(settings) = application::mysql_mode(&value) {
                    self.workbench.sql_mode = Some(settings);
                    let Some((_, query)) = self.current_pending.take() else {
                        return;
                    };
                    self.execute_current_statement(
                        &query,
                        editor::SqlContext::Session(settings),
                        backend,
                    );
                } else {
                    self.current_pending = None;
                    self.status = "数据库未返回 sql_mode，无法安全定位当前语句".into();
                }
            }
            "change.open" => {
                self.edit_token = value.get("token").cloned();
                if let Some(schema) = value.get("schema") {
                    self.schema = Some(schema.clone());
                } else if self.is_redis() {
                    // change.open 返回编辑基线；保留前一步 describe 已取得的长度。
                    let length = self
                        .schema
                        .as_ref()
                        .and_then(|schema| schema.get("length"))
                        .cloned();
                    let mut metadata = value.clone();
                    if let Some(length) = length {
                        metadata["length"] = length;
                    }
                    self.schema = Some(metadata);
                }
                self.detail = value.to_string();
                self.set_detail_context("资源详情");
                if let Some(id) = value.get("result_id").and_then(Value::as_str) {
                    self.browse_result = true;
                    self.results = vec![id.to_owned()];
                    self.page_history.clear();
                    self.result_id = Some(id.into());
                    self.page(backend, 0);
                } else {
                    // MySQL change.open 只返回 token/schema；行数据必须再通过 browse 获取。
                    self.action("browse", backend);
                }
            }
            "change.discard" => {
                self.workbench.load_resource = true;
                self.action("describe", backend);
            }
            "execution.start" | "archery.sql_check" | "resource.browse" | "change.submit"
            | "transfer.import" | "transfer.export" => {
                self.operation_pending = false;
                self.large = None;
                self.task = value
                    .get("task_id")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                self.status = i18n::UiMessage::template(
                    "任务已提交: {task}",
                    vec![("task", i18n::UiArg::Raw(value.to_string()))],
                );
                self.last_poll = Instant::now() - Duration::from_secs(2);
            }
            "execution.get" => {
                let status_label = match value["state"].as_str() {
                    Some("queued") => i18n::UiArg::Key("排队中"),
                    Some("running") => i18n::UiArg::Key("执行中"),
                    Some("cancelling") => i18n::UiArg::Key("取消中"),
                    Some("succeeded") => i18n::UiArg::Key("已完成"),
                    Some("failed") => i18n::UiArg::Key("失败"),
                    Some("cancelled") => i18n::UiArg::Key("已取消"),
                    Some("wait_stopped") => i18n::UiArg::Key("已结束等待，远端完成状态未知"),
                    Some("outcome_unknown") => i18n::UiArg::Key("结果待确认"),
                    _ => i18n::UiArg::Raw(value["state"].to_string()),
                };
                self.status =
                    i18n::UiMessage::template("任务 {state}", vec![("state", status_label)]);
                let state = value["state"].as_str().unwrap_or("");
                let mut details = value.clone();
                // 普通轮询不含执行文本，保留同一任务按需加载的文本。
                if details.get("text").is_none_or(Value::is_null)
                    && let Ok(previous) = serde_json::from_str::<Value>(&self.detail)
                    && previous.get("id") == details.get("id")
                    && let Some(text) = previous.get("text")
                {
                    details["text"] = text.clone();
                }
                self.detail = details.to_string();
                self.set_detail_context("任务详情");
                // 查询仍在运行时首批结果也可用；同一结果不能因轮询而跳回第一页。
                if !self.workbench.schema_invalidated_task
                    && let Some(items) = value.get("results").and_then(Value::as_array)
                {
                    let results: Vec<String> = items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    if self.result_id.is_none()
                        && let Some(id) = results.first().cloned()
                    {
                        self.browse_result = value["method"].as_str() == Some("resource.browse");
                        self.result_id = Some(id);
                        self.page(backend, 0);
                    }
                    self.results = results;
                }
                if !matches!(state, "queued" | "running" | "cancelling") {
                    self.workbench.schema_invalidated_task = false;
                    self.task = None;
                    if !self.is_redis() && !self.browse_result {
                        self.refresh_query_mode(backend);
                    }
                    if value["method"].as_str() == Some("change.submit") {
                        if self.redis_key_delete_finish_task(&value, backend) {
                            return;
                        }
                        self.edit_token = None;
                        if let Some(g) = &mut self.grid {
                            g.editable = false;
                        }
                        let applied = state == "succeeded"
                            && value["summary"]["items"].as_array().is_some_and(|items| {
                                items
                                    .iter()
                                    .all(|item| item["status"].as_str() == Some("applied"))
                            });
                        if applied {
                            let target_invalidated = self.redis_changes.iter().any(|change| {
                                matches!(
                                    change["operation"].as_str(),
                                    Some("rename" | "delete_key")
                                )
                            });
                            // 提交成功后旧行基线不再可靠；保留布局并重新浏览取得新结果。
                            if let Some(g) = self.grid.take() {
                                self.tabs[self.active].layout = g.settings;
                            }
                            self.pending_rows.clear();
                            self.pending_edit_files.clear();
                            self.redis_changes.clear();
                            self.redis_changes_revision =
                                self.redis_changes_revision.wrapping_add(1);
                            self.draft_revision = self.draft_revision.wrapping_add(1);
                            self.clear_redis_edit_state();
                            self.workbench.overlay = None;
                            self.tabs[self.active].pending = Value::Null;
                            self.dirty = true;
                            self.tabs[self.active].pending_live = false;
                            self.results.clear();
                            self.result_id = None;
                            self.page_history.clear();
                            self.page_token = None;
                            self.large = None;
                            self.schema = None;
                            self.generation = self.generation.wrapping_add(1);
                            if target_invalidated {
                                self.target = None;
                                self.resources.clear();
                                self.workbench.redis_tree.reset(&[]);
                                self.workbench.redis_tree_owner = None;
                                self.action("list", backend);
                            } else {
                                self.workbench.load_resource = true;
                                self.action("describe", backend);
                            }
                            return;
                        } else {
                            self.status = i18n::UiMessage::Concat(vec![
                                std::mem::take(&mut self.status),
                                i18n::UiMessage::key("；基线已失效，检查结果后重新打开"),
                            ]);
                        }
                    }
                    self.archery_auth_required(
                        value["error"]["code"].as_str(),
                        &value["error"]["details"],
                        backend,
                    );
                }
            }
            "result.page" => match serde_json::from_value::<ResultPage>(value) {
                Ok(mut page) => {
                    self.remember_result_start(&page);
                    if let Some(g) = &mut self.grid
                        && g.page.result.id == page.result.id
                    {
                        // 同一结果的约束已在创建 grid 时确定，分页沿用该基线。
                        if g.editable
                            && let Err(error) =
                                grid::merge_table_columns(&mut page, &g.page.result.columns)
                        {
                            self.status = workbench::grid_error(error);
                        } else if let Err(error) = g.set_page(page) {
                            self.status = workbench::grid_error(error);
                        }
                        return;
                    }
                    let mut editable = self.browse_result
                        && self.edit_token.is_some()
                        && self.profile.as_ref().is_some_and(|p| {
                            p.provider == datapier_core::model::ProviderKind::Mysql
                        })
                        && self
                            .schema
                            .as_ref()
                            .and_then(|s| s.get("keys"))
                            .and_then(Value::as_array)
                            .is_some_and(|keys| !keys.is_empty());
                    if editable {
                        let columns = self
                            .schema
                            .as_ref()
                            .and_then(|s| s.get("columns"))
                            .and_then(|columns| {
                                serde_json::from_value::<Vec<datapier_core::model::Column>>(
                                    columns.clone(),
                                )
                                .ok()
                            });
                        let columns = columns.map(|mut columns| {
                            if self
                                .target
                                .as_ref()
                                .is_some_and(|target| target.kind == "view")
                            {
                                let writable = self
                                    .schema
                                    .as_ref()
                                    .and_then(|schema| {
                                        schema.pointer("/details/view_mapping/writable")
                                    })
                                    .and_then(Value::as_array);
                                for column in &mut columns {
                                    if !self.sql_operation_allowed("update")
                                        || !writable.is_some_and(|items| {
                                            items.iter().any(|item| {
                                                item.as_str() == Some(column.name.as_str())
                                            })
                                        })
                                    {
                                        column.generated = true;
                                    }
                                }
                            }
                            columns
                        });
                        let merged = columns.as_deref().is_some_and(|columns| {
                            grid::merge_table_columns(&mut page, columns).is_ok()
                        });
                        if !merged {
                            self.status = "结果列与表结构不匹配，无法编辑".into();
                            editable = false;
                        }
                    }
                    if self.has_live_drafts() {
                        self.status = "结果已就绪；请先提交或丢弃当前草稿".into();
                        return;
                    }
                    let mut g = GridState::new(page, editable);
                    if self.tabs[self.active].layout.len() == g.settings.len() {
                        g.settings = self.tabs[self.active].layout.clone();
                    }
                    self.grid = Some(g);
                }
                Err(e) => self.status = i18n::UiMessage::raw(e.to_string()),
            },
            "file.open" => {
                let Some((_, original, epoch)) = self.file_open_pending.take() else {
                    return;
                };
                if epoch != self.query_edit_epoch || self.tabs[self.active].query.text != original {
                    self.status = "打开文件期间查询已更改，保留当前内容".into();
                } else if let Some(text) = value.get("text").and_then(Value::as_str) {
                    self.tabs[self.active].query = Editor::new(text);
                    self.changed_field("query");
                }
            }
            "archery.history.cancel" => {}
            _ => {
                self.detail = value.to_string();
                self.detail_context = DetailContext::default();
                self.status = i18n::UiMessage::raw(response.tag);
            }
        }
    }
}
