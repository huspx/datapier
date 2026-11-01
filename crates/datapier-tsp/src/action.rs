use super::*;

impl App {
    pub(super) fn action(&mut self, name: &str, backend: &Backend) {
        if self.tab_close.is_some() {
            return;
        }
        if self.redis_selection_action(backend, name) {
            return;
        }
        match name {
            "redis-row-delete" => {
                self.request_redis_row_delete();
                return;
            }
            "redis-row-delete-confirm" => {
                self.confirm_redis_row_delete(backend);
                return;
            }
            "redis-row-delete-cancel" => {
                self.cancel_redis_row_delete();
                return;
            }
            "redis-open-current-key" => {
                if self.is_redis() && self.target.is_some() {
                    self.view = "data".into();
                    self.focus = "grid".into();
                    self.workbench.focus_request = Some("grid".into());
                }
                return;
            }
            _ => {}
        }
        if name == "change-preview-next" {
            self.change_preview_page(true, backend);
            return;
        }
        if name == "change-preview-prev" {
            self.change_preview_page(false, backend);
            return;
        }
        if self.application_action(name, backend)
            || ((!self.tabs.is_empty()
                || (self.workbench.overlay.as_deref() == Some("profile")
                    && (name.starts_with("profile-databases-")
                        || name.starts_with("form-redis-db"))))
                && self.navigation_action(name, backend))
            || (!self.tabs.is_empty() && self.query_action(name, backend))
        {
            return;
        }
        if self.tabs.is_empty()
            && !matches!(
                name,
                "new-tab" | "new-profile" | "save-profile" | "test-profile" | "workspace-copy"
            )
            && self.workbench.overlay.is_none()
        {
            return;
        }
        if name == "discard" {
            self.finish_inline(false);
        } else if matches!(
            name,
            "history"
                | "new-tab"
                | "new-profile"
                | "run"
                | "selection"
                | "current"
                | "submit"
                | "redis-submit"
                | "open-cell"
                | "open"
                | "browse"
                | "next"
                | "previous"
        ) && !self.finish_inline(true)
        {
            return;
        }
        if self.workbench_action(name, backend) {
            return;
        }
        let session = self.session_id();
        if matches!(
            name,
            "set-cell" | "set-null" | "set-json" | "insert-row" | "redis-stage" | "paste-confirm"
        ) && !self.tabs[self.active].pending.is_null()
            && !self.tabs[self.active].pending_live
        {
            self.status = "请先恢复或丢弃已保存的编辑意图".into();
            return;
        }
        if name == "open-cell" {
            self.open_cell(backend);
            return;
        }
        match name {
            "workspace-copy" => self.save_workspace_copy(backend),
            "databases" => {
                if self.target.is_some() || self.has_live_drafts() || self.task.is_some() {
                    self.action("new-tab", backend);
                    self.workbench.queued_target = Some(ResourceTarget {
                        database: None,
                        name: String::new(),
                        key_b64: None,
                        kind: "database".into(),
                    });
                    self.action("open", backend);
                } else {
                    self.database = None;
                    self.field_set("object_filter", "");
                    self.action("list", backend);
                }
            }
            "new-profile" => self.profile_form(None),
            "edit-profile" => {
                if let Some(id) = self.profile.as_ref().map(|p| p.id) {
                    self.edit_profile_after_close(id, false, backend);
                }
            }
            "save-profile" | "test-profile" => {
                self.submit_profile_form(name == "save-profile", backend);
            }
            "open" => {
                if let Some(p) = &self.profile {
                    let id = p.id;
                    let redis_profile_db = (p.provider
                        == datapier_core::model::ProviderKind::Redis)
                        .then_some(p.redis_db);
                    if self.task.is_some() || self.operation_pending {
                        self.status = "请等待或取消当前任务".into();
                    } else if self.has_live_drafts() {
                        self.status = "请先提交或丢弃待提交变更".into();
                    } else {
                        if let Some(old) = self.session_id() {
                            self.request(
                                backend,
                                "connection.close",
                                "connection.close",
                                json!({"session_id":old}),
                            );
                            self.session = None;
                        }
                        if self.workbench.queued_target.is_none() {
                            self.workbench.queued_target = self.target.take();
                        }
                        let redis_db = redis_profile_db.map(|default_db| {
                            self.workbench
                                .queued_target
                                .as_ref()
                                .and_then(|target| target.database.as_deref())
                                .and_then(|database| database.parse::<u32>().ok())
                                .or(self.tabs[self.active].browse.redis_db)
                                .unwrap_or(default_db)
                        });
                        self.edit_token = None;
                        self.schema = None;
                        self.grid = None;
                        self.results.clear();
                        self.result_id = None;
                        self.page_token = None;
                        self.page_history.clear();
                        self.large = None;
                        self.browse_result = false;
                        self.generation = self.generation.wrapping_add(1);
                        self.request(
                            backend,
                            "connection.open",
                            "connection.open",
                            json!({"profile_id":id,"redis_db":redis_db,"secret":null}),
                        );
                    }
                }
            }
            "list" | "more" => {
                if let Some(s) = session {
                    if name == "more" && self.scan_token.is_some() {
                        self.status = "正在读取资源，请等待".into();
                    } else if name == "more" && self.cursor.is_none() {
                        self.status = "扫描已完成".into();
                    } else {
                        let token = Uuid::new_v4();
                        self.scan_token = Some(token);
                        if name == "list" {
                            self.cursor = None;
                            self.resources.clear();
                            self.redis_prune_key_selection();
                            self.workbench.scan_dirty = false;
                            self.workbench.key_page = 0;
                        }
                        self.request(backend,&format!("resource.list/{token}"),"resource.list",json!({"session_id":s,"database":self.database,"cursor":if name == "more" {self.cursor.clone()} else {None},"pattern":self.field("pattern"),"type":if self.field("type").is_empty(){None}else{Some(self.field("type"))},"limit":200}));
                    }
                }
            }
            "browse" | "describe" | "edit-open" => {
                if let (Some(s), Some(target)) = (session, self.target.clone()) {
                    let method = match name {
                        "browse" => "resource.browse",
                        "describe" => "resource.describe",
                        _ => "change.open",
                    };
                    let filters: Value = match serde_json::from_str(self.field("filter")) {
                        Ok(v) => v,
                        Err(e) => {
                            self.status = i18n::UiMessage::template(
                                "过滤格式错误: {error}",
                                vec![("error", i18n::UiArg::Raw(e.to_string()))],
                            );
                            return;
                        }
                    };
                    let sort: Value = match serde_json::from_str(self.field("sort")) {
                        Ok(v) => v,
                        Err(e) => {
                            self.status = i18n::UiMessage::template(
                                "排序格式错误: {error}",
                                vec![("error", i18n::UiArg::Raw(e.to_string()))],
                            );
                            return;
                        }
                    };
                    self.request(backend, method, method, json!({"session_id":s,"target":target,"filters":filters,"sort":sort,"workspace_id":self.workspace_id}));
                }
            }
            "run" | "selection" => {
                let query = &self.tabs[self.active].query;
                let text = if name == "selection" {
                    query.selected_text().unwrap_or("").to_owned()
                } else {
                    query.text.clone()
                };
                self.execute_query_text(text, backend);
            }
            "current" => {
                if let Some(s) = session {
                    if self
                        .profile
                        .as_ref()
                        .is_some_and(|p| p.provider == datapier_core::model::ProviderKind::Mysql)
                    {
                        let token = Uuid::new_v4();
                        self.current_pending = Some((token, self.tabs[self.active].query.clone()));
                        self.request(
                            backend,
                            &format!("current.info/{token}"),
                            "connection.info",
                            json!({"session_id":s}),
                        );
                    } else {
                        self.status = "当前语句定位仅支持 MySQL".into();
                    }
                }
            }
            "cancel" => {
                if let Some(task) = self.task.clone() {
                    self.request(
                        backend,
                        "execution.cancel",
                        "execution.cancel",
                        json!({"task_id":task}),
                    );
                }
            }
            "next" => {
                if self.paste_preview.is_some() {
                    self.status = "请先确认或丢弃粘贴预览".into();
                } else if let Some(g) = &self.grid
                    && let Some(next) = g.page.next
                {
                    let start = g.page.start;
                    self.page_history.push(start);
                    self.page(backend, next);
                }
            }
            "previous" => {
                if self.paste_preview.is_some() {
                    self.status = "请先确认或丢弃粘贴预览".into();
                } else if let Some(start) = self.page_history.pop() {
                    self.page(backend, start);
                }
            }
            "new-tab" => {
                self.tabs.push(Tab {
                    title: format!("查询 {}", self.tabs.len() + 1),
                    query: Editor::new(""),
                    cli_query: Editor::default(),
                    profile_id: self.profile.as_ref().map(|p| p.id),
                    id: Uuid::new_v4(),
                    layout: Vec::new(),
                    browse: BrowseSettings {
                        redis_db: self
                            .profile
                            .as_ref()
                            .filter(|p| p.provider == datapier_core::model::ProviderKind::Redis)
                            .map(|p| p.redis_db),
                        ..BrowseSettings::default()
                    },
                    pending: Value::Null,
                    pending_live: false,
                    runtime: TabRuntime::default(),
                });
                self.tabs.last_mut().unwrap().runtime.profile = self.profile.clone();
                self.switch_tab(self.tabs.len() - 1);
                self.focus_current_tab();
                self.dirty = true;
            }
            "close-tab" => {
                if let Some(tab) = self.tabs.get(self.active) {
                    self.close_tab(tab.id, backend);
                }
            }
            "history" => {
                self.workbench.overlay = Some("history".into());
                self.workbench.focus_request = Some("input-history_filter".into());
                self.request(
                    backend,
                    "history.list",
                    "history.list",
                    json!({"workspace_id":self.workspace_id}),
                );
            }
            "file-open" | "file-save" => {
                if self.field("path").trim().is_empty() {
                    self.field_error("path", i18n::UiMessage::key("请填写路径"));
                    return;
                }
                let method = if name == "file-open" {
                    "file.open"
                } else {
                    "file.save"
                };
                let tag = if name == "file-open" {
                    let token = Uuid::new_v4();
                    self.file_open_pending = Some((
                        token,
                        self.tabs[self.active].query.text.clone(),
                        self.query_edit_epoch,
                    ));
                    format!("file.open/{token}")
                } else {
                    method.to_owned()
                };
                self.request(
                    backend,
                    &tag,
                    method,
                    json!({"path":self.field("path"),"text":self.tabs[self.active].query.text}),
                );
            }
            "preview-import" | "import" | "export" => {
                if let Some(s) = session {
                    if self.field("path").trim().is_empty() {
                        self.field_error("path", i18n::UiMessage::key("请填写路径"));
                        return;
                    }
                    if !matches!(self.field("format"), "csv" | "json") {
                        self.field_error("format", i18n::UiMessage::key("请选择有效格式"));
                        return;
                    }
                    let mapping: Value = match serde_json::from_str(self.field("mapping")) {
                        Ok(v) => v,
                        Err(e) => {
                            self.field_error(
                                "mapping",
                                i18n::UiMessage::template(
                                    "列映射格式错误: {error}",
                                    vec![("error", i18n::UiArg::Raw(e.to_string()))],
                                ),
                            );
                            return;
                        }
                    };
                    if !mapping.is_object() {
                        self.field_error("mapping", i18n::UiMessage::key("列映射必须是 JSON 对象"));
                        return;
                    }
                    let binary: Value = match serde_json::from_str(self.field("binary_columns")) {
                        Ok(v) => v,
                        Err(e) => {
                            self.field_error(
                                "binary_columns",
                                i18n::UiMessage::template(
                                    "二进制列格式错误: {error}",
                                    vec![("error", i18n::UiArg::Raw(e.to_string()))],
                                ),
                            );
                            return;
                        }
                    };
                    if !binary
                        .as_array()
                        .is_some_and(|items| items.iter().all(Value::is_string))
                    {
                        self.field_error(
                            "binary_columns",
                            i18n::UiMessage::key("二进制列必须是字符串数组"),
                        );
                        return;
                    }
                    let escaped = match self.field("backslash_escape").parse::<bool>() {
                        Ok(v) => v,
                        Err(_) => {
                            self.field_error(
                                "backslash_escape",
                                i18n::UiMessage::key("转义选项必须是布尔值"),
                            );
                            return;
                        }
                    };
                    let mut params = json!({"session_id":s,"workspace_id":self.workspace_id,"path":self.field("path"),"format":self.field("format"),"column_mapping":mapping,"null_marker":self.field("null_marker"),"backslash_escape":escaped,"binary_columns":binary});
                    if let Some(target) = self.target.clone() {
                        params["target"] = json!(target);
                    } else if name != "export"
                        && self.profile.as_ref().is_some_and(|p| {
                            p.provider == datapier_core::model::ProviderKind::Redis
                        })
                    {
                        params["target"] = json!({"name":"","database":self.profile.as_ref().map(|p|p.redis_db.to_string()),"kind":"database"});
                    }
                    if params.get("target").is_none()
                        && (name != "export" || self.result_id.is_none())
                    {
                        self.status = i18n::UiMessage::key("请先选择导入或导出目标");
                        return;
                    }
                    if name == "export" {
                        if let Some(id) = self.result_id.clone() {
                            params["result_id"] = json!(id);
                        }
                        self.request(backend, "transfer.export", "transfer.export", params);
                    } else if name == "preview-import" {
                        let id = Uuid::new_v4();
                        self.preview = None;
                        self.preview_pending = Some((id, params.clone()));
                        self.request(
                            backend,
                            &format!("transfer.preview/{id}"),
                            "transfer.preview",
                            params,
                        );
                    } else if self.preview.as_ref() == Some(&params) {
                        self.preview = None;
                        self.request(backend, "transfer.import", "transfer.import", params);
                    } else {
                        self.status = "请先预览并确认当前导入设置".into();
                    }
                }
            }
            "undo-grid" => {
                if let Some(g) = &mut self.grid {
                    g.undo();
                    self.dirty = true;
                }
            }
            "insert-row" => {
                if self.edit_token.is_none() {
                    self.status = "请先打开编辑基线".into();
                } else {
                    match serde_json::from_str::<BTreeMap<String, datapier_core::model::Value>>(
                        self.field("row_values"),
                    ) {
                        Ok(values) => {
                            if !self.validate_insert_values(&values, true) {
                                return;
                            }
                            self.pending_rows.push(datapier_core::model::RowChange {
                                operation: "insert".into(),
                                key: BTreeMap::new(),
                                before: BTreeMap::new(),
                                values,
                            });
                            self.detail = serde_json::to_string_pretty(&self.pending_rows)
                                .unwrap_or_default();
                            self.set_detail_context("草稿详情");
                            self.dirty = true;
                        }
                        Err(e) => {
                            self.field_error(
                                "row_values",
                                i18n::UiMessage::template(
                                    "新增行值格式错误: {error}",
                                    vec![("error", i18n::UiArg::Raw(e.to_string()))],
                                ),
                            );
                        }
                    }
                }
            }
            "restore-pending" => {
                let pending = self.tabs[self.active].pending.clone();
                if pending.is_null() {
                    self.status = "没有已恢复的编辑意图".into();
                    return;
                }
                if self.edit_token.is_none() || pending.get("target") != Some(&json!(self.target)) {
                    self.status = "请先选择原目标并重新打开编辑基线".into();
                    return;
                }
                let redis = pending
                    .get("redis")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if redis.iter().any(|change| {
                    matches!(
                        change.get("operation").and_then(Value::as_str),
                        Some("list_set" | "list_insert" | "list_delete")
                    )
                }) {
                    self.detail = serde_json::to_string_pretty(&pending).unwrap_or_default();
                    self.set_recovery_detail_context();
                    self.status =
                        "List 索引基线已失效；请根据刷新后的列表重新录入 List 操作".into();
                    return;
                }
                let mut rows: Vec<datapier_core::model::RowChange> = match serde_json::from_value(
                    pending.get("rows").cloned().unwrap_or_else(|| json!([])),
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        self.status = i18n::UiMessage::template(
                            "恢复行草稿失败: {error}",
                            vec![("error", i18n::UiArg::Raw(e.to_string()))],
                        );
                        return;
                    }
                };
                if !pending
                    .get("rows_include_edits")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    let edits = pending
                        .get("edits")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    if !edits.is_empty() {
                        let schema: datapier_core::model::TableSchema = match self
                            .schema
                            .clone()
                            .and_then(|v| serde_json::from_value(v).ok())
                        {
                            Some(v) => v,
                            None => {
                                self.status = "请重新打开表编辑基线".into();
                                return;
                            }
                        };
                        let Some(keys) = schema.keys.first().filter(|keys| !keys.is_empty()) else {
                            self.status = "表没有可靠行标识".into();
                            return;
                        };
                        let mut updates: BTreeMap<u64, datapier_core::model::RowChange> =
                            BTreeMap::new();
                        for item in edits {
                            let Some(row) = item["row"].as_u64() else {
                                self.status = "恢复数据缺少行号".into();
                                return;
                            };
                            let Some(column) = item["column"]
                                .as_u64()
                                .and_then(|x| usize::try_from(x).ok())
                                .and_then(|i| schema.columns.get(i))
                            else {
                                self.status = "恢复数据缺少列".into();
                                return;
                            };
                            let before: Vec<datapier_core::model::Value> =
                                match serde_json::from_value(item["before_row"].clone()) {
                                    Ok(v) => v,
                                    Err(e) => {
                                        self.status = i18n::UiMessage::template(
                                            "恢复基线失败: {error}",
                                            vec![("error", i18n::UiArg::Raw(e.to_string()))],
                                        );
                                        return;
                                    }
                                };
                            if before.len() != schema.columns.len() {
                                self.status = "恢复基线列数不匹配".into();
                                return;
                            }
                            let before: BTreeMap<_, _> = schema
                                .columns
                                .iter()
                                .zip(before)
                                .map(|(c, v)| (c.name.clone(), v))
                                .collect();
                            let key: BTreeMap<_, _> = keys
                                .iter()
                                .filter_map(|k| before.get(k).cloned().map(|v| (k.clone(), v)))
                                .collect();
                            if key.len() != keys.len()
                                || key.values().any(|v| {
                                    matches!(
                                        v,
                                        datapier_core::model::Value::Null
                                            | datapier_core::model::Value::Missing
                                    )
                                })
                            {
                                self.status = "恢复基线没有可靠行标识".into();
                                return;
                            }
                            let after = match serde_json::from_value(item["after"].clone()) {
                                Ok(v) => v,
                                Err(e) => {
                                    self.status = i18n::UiMessage::template(
                                        "恢复值无效: {error}",
                                        vec![("error", i18n::UiArg::Raw(e.to_string()))],
                                    );
                                    return;
                                }
                            };
                            let entry = updates.entry(row).or_insert_with(|| {
                                datapier_core::model::RowChange {
                                    operation: "update".into(),
                                    key,
                                    before: before.clone(),
                                    values: BTreeMap::new(),
                                }
                            });
                            if entry.before != before {
                                self.status = "恢复数据包含不一致的行基线".into();
                                return;
                            }
                            entry.values.insert(column.name.clone(), after);
                        }
                        rows.extend(updates.into_values());
                    }
                }
                self.pending_rows = rows;
                self.redis_changes = redis;
                self.redis_changes_revision = self.redis_changes_revision.wrapping_add(1);
                self.clear_redis_edit_state();
                self.tabs[self.active].pending = Value::Null;
                self.dirty = true;
                self.detail = serde_json::to_string_pretty(
                    &json!({"rows":self.pending_rows,"redis":self.redis_changes}),
                )
                .unwrap_or_default();
                self.set_detail_context("草稿详情");
                self.status = "编辑意图已恢复；提交时将检查原值，请先确认预览".into();
            }
            "undo-row" => {
                self.pending_rows.pop();
                self.detail = serde_json::to_string_pretty(&self.pending_rows).unwrap_or_default();
                self.set_detail_context("草稿详情");
                self.dirty = true;
            }
            "paste-confirm" => {
                if let Some(preview) = self.paste_preview.take() {
                    if let Err(error) =
                        self.check_rows_not_deleted(preview.edits.iter().map(|edit| edit.cell.row))
                    {
                        self.status = error;
                        return;
                    }
                    if let Some(g) = &mut self.grid {
                        match g.apply_paste(preview) {
                            Ok(()) => {
                                self.dirty = true;
                                self.status = "粘贴已加入待提交变更".into();
                            }
                            Err(e) => self.status = workbench::grid_error(e),
                        }
                    }
                }
            }
            "column-width" | "column-hide" | "column-freeze" => {
                let column = match self.field("column").parse::<usize>() {
                    Ok(column)
                        if self
                            .grid
                            .as_ref()
                            .is_some_and(|grid| column < grid.settings.len()) =>
                    {
                        column
                    }
                    _ => {
                        self.field_error("column", i18n::UiMessage::key("请输入有效列序号"));
                        return;
                    }
                };
                let width = if name == "column-width" {
                    match self.field("width").parse::<u16>() {
                        Ok(width) if width > 0 => Some(width),
                        _ => {
                            self.field_error("width", i18n::UiMessage::key("请输入大于零的列宽"));
                            return;
                        }
                    }
                } else {
                    None
                };
                if let Some(g) = &mut self.grid {
                    let mut setting = g.settings[column].clone();
                    match name {
                        "column-width" => setting.width = width.unwrap(),
                        "column-hide" => setting.hidden = !setting.hidden,
                        "column-freeze" => setting.frozen = !setting.frozen,
                        _ => unreachable!(),
                    }
                    if let Err(error) = g.set_column(column, setting) {
                        self.status = workbench::grid_error(error);
                    } else {
                        self.dirty = true;
                    }
                }
            }
            "discard" => {
                if let Some(g) = &mut self.grid {
                    g.discard();
                }
                self.pending_rows.clear();
                self.redis_changes.clear();
                self.redis_changes_revision = self.redis_changes_revision.wrapping_add(1);
                self.paste_preview = None;
                self.tabs[self.active].pending = Value::Null;
                self.tabs[self.active].pending_live = false;
                if let (Some(s), Some(token)) = (session, self.edit_token.take()) {
                    self.request(
                        backend,
                        "change.discard",
                        "change.discard",
                        json!({"session_id":s,"token":token}),
                    );
                }
                self.dirty = true;
            }
            "submit" | "redis-submit" => {
                if self.is_redis() && self.redis_row_delete_resolution_pending() {
                    self.status = i18n::UiMessage::key("请等待 Redis 条目读取完成");
                    return;
                }
                if self.workbench.overlay.as_deref() == Some("preview")
                    && self.change_preview_value().is_none()
                {
                    self.status = "预览已过期，请重新预览".into();
                    return;
                }
                match self.change_input() {
                    Ok(params) => self.request(backend, "change.submit", "change.submit", params),
                    Err(error) => self.status = error,
                }
            }
            "redis-undo" => {
                self.redis_changes.pop();
                self.redis_changes_revision = self.redis_changes_revision.wrapping_add(1);
                self.detail = serde_json::to_string_pretty(&self.redis_changes).unwrap_or_default();
                self.set_detail_context("草稿详情");
                self.dirty = true;
            }
            "set-cell" | "set-null" | "set-json" => {
                let target = self.detail_cell();
                if let Some(cell) = target
                    && let Err(error) = self.check_rows_not_deleted([cell.row])
                {
                    self.status = error;
                    return;
                }
                if let Some(g) = &mut self.grid
                    && let Some(cell) = target
                {
                    let text = self.workbench.cell_editor.text.clone();
                    let val = match name {
                        "set-null" => datapier_core::model::Value::Null,
                        "set-json" => match serde_json::from_str(&text) {
                            Ok(v) => datapier_core::model::Value::Json(v),
                            Err(e) => {
                                self.field_error("cell", i18n::UiMessage::raw(e.to_string()));
                                return;
                            }
                        },
                        _ => match g.parse_cell_text(cell, &text) {
                            Ok(v) => v,
                            Err(e) => {
                                let error = workbench::grid_cell_error(e, g, cell);
                                self.field_error("cell", error);
                                return;
                            }
                        },
                    };
                    if let Err(e) = g.edit(cell, val) {
                        let error = workbench::grid_cell_error(e, g, cell);
                        self.field_error("cell", error);
                    } else {
                        self.dirty = true;
                    }
                }
            }
            _ => {}
        }
    }
}
