use super::*;

impl App {
    pub(super) fn page(&mut self, backend: &Backend, start: u64) {
        if let Some(id) = self.result_id.clone() {
            let token = Uuid::new_v4();
            self.page_token = Some(token);
            self.request(
                backend,
                &format!("result.page/{token}"),
                "result.page",
                json!({"result_id":id,"start":start,"max_rows":200,"max_bytes":1048576}),
            );
        }
    }
    pub(super) fn response(&mut self, response: Response, backend: &Backend) {
        let value = match response.result {
            Ok(v) => v,
            Err(e) => {
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
                        | "resource.browse"
                        | "change.submit"
                        | "transfer.import"
                        | "transfer.export"
                ) {
                    self.operation_pending = false;
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
                if value.get("kind").and_then(Value::as_str) == Some("settings.changed") {
                    self.refresh_settings(backend);
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
                self.request(backend, "profile.list", "profile.list", json!({}));
                self.request(
                    backend,
                    "workspace.load",
                    "workspace.load",
                    json!({"workspace_id":self.workspace_id}),
                );
            }
            "profile.list" => {
                self.profiles =
                    serde_json::from_value(value.get("profiles").cloned().unwrap_or_default())
                        .unwrap_or_default();
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
                self.revision = value["revision"].as_u64().unwrap_or(0);
                if self.dirty {
                    if let Ok(state) = serde_json::from_value::<Workspace>(value["state"].clone()) {
                        self.tabs.extend(state.tabs);
                        self.migrate_cli_queries();
                    }
                } else if let Ok(state) =
                    serde_json::from_value::<Workspace>(value["state"].clone())
                {
                    self.navigation.expanded = state.navigation_expanded;
                    self.tabs = state.tabs;
                    self.active = state.active.min(self.tabs.len().saturating_sub(1));
                    self.migrate_cli_queries();
                    if !self.tabs.is_empty() {
                        self.restore_browse();
                        self.profile = self.tabs[self.active]
                            .profile_id
                            .and_then(|id| self.profiles.iter().find(|p| p.id == id).cloned());
                        if !self.workspace_interacted {
                            self.focus_current_tab();
                            self.restore_focus_pending = self.profile.is_none()
                                && self.tabs[self.active].profile_id.is_some();
                        }
                        if !self.tabs[self.active].pending.is_null() {
                            self.detail = format!(
                                "恢复的编辑意图（基线已失效，请刷新并重新确认）：{}",
                                self.tabs[self.active].pending
                            );
                            self.set_recovery_detail_context();
                        }
                    }
                }
                self.workspace_loaded = true;
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
                        if profile.provider == datapier_core::model::ProviderKind::Redis {
                            self.tabs[self.active].browse.redis_db = Some(profile.redis_db);
                        }
                        self.database = profile.database.clone();
                        self.profile = Some(profile);
                        self.session = Some(value);
                        self.workbench.sql_mode = None;
                        self.status = "数据库连接已打开".into();
                        if let Some(target) = self.workbench.queued_target.take() {
                            if target.kind == "database" {
                                self.database = (!target.name.is_empty()).then_some(target.name);
                            } else {
                                self.load_target(target, backend);
                            }
                        }
                        self.navigation_apply_queued_section();
                        self.action("list", backend);
                        self.refresh_query_mode(backend);
                        self.navigation_load_tree(backend, false);
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
                        // Core 的扫描缓存按原始 key 去重，各页只发出尚未交付的项。
                        self.resources.extend(page.resources);
                        if self.workbench.tree {
                            self.normalize_key_page();
                        } else {
                            self.workbench.key_page =
                                self.resources.len().saturating_sub(1) / workbench::KEY_PAGE_SIZE;
                        }
                    } else {
                        self.resources = page.resources;
                    }
                    self.cursor = page.cursor;
                }
                Err(e) => self.status = i18n::UiMessage::raw(e.to_string()),
            },
            "history.list" => {
                self.workbench.history = value
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                self.workbench.history_selected = if self.field("history_filter").is_empty()
                    && !self.workbench.history.is_empty()
                {
                    Some(0)
                } else {
                    None
                };
            }
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
                self.schema = Some(value.clone());
                self.detail = value.to_string();
                self.set_detail_context("资源详情");
                if std::mem::take(&mut self.workbench.load_resource) {
                    let writable = self.profile.as_ref().is_some_and(|p| {
                        p.access_mode == datapier_core::model::AccessMode::ReadWrite
                    });
                    let editable = writable
                        && (self.is_redis()
                            || (self.target.as_ref().is_some_and(|t| t.kind == "table")
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
                    match editor::current_statement(&query.text, query.cursor, settings) {
                        Ok(range) => {
                            if let (Some(start), Some(end)) = (
                                editor::utf16_to_byte(&query.text, range.start),
                                editor::utf16_to_byte(&query.text, range.end),
                            ) {
                                let text = query.text[start..end].to_string();
                                self.execute_query_text(text, backend);
                            }
                        }
                        Err(e) => self.status = i18n::UiMessage::local(e),
                    }
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
            "execution.start" | "resource.browse" | "change.submit" | "transfer.import"
            | "transfer.export" => {
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
                if !matches!(state, "queued" | "running" | "cancelling") {
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
                            self.redis_changes.clear();
                            self.redis_changes_revision =
                                self.redis_changes_revision.wrapping_add(1);
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
                    self.results = value
                        .get("results")
                        .and_then(Value::as_array)
                        .map(|v| {
                            v.iter()
                                .filter_map(Value::as_str)
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default();
                    if let Some(id) = self.results.first().cloned() {
                        self.browse_result = value["method"].as_str() == Some("resource.browse");
                        self.result_id = Some(id);
                        self.page(backend, 0);
                    }
                }
            }
            "result.page" => match serde_json::from_value::<ResultPage>(value) {
                Ok(mut page) => {
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
            _ => {
                self.detail = value.to_string();
                self.detail_context = DetailContext::default();
                self.status = i18n::UiMessage::raw(response.tag);
            }
        }
    }
}
