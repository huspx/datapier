//! Engine 请求派发、诊断读取与迟到响应隔离。
use super::*;

impl App {
    pub(super) fn request_diagnostic_task(&mut self, backend: &Backend) {
        self.workbench.diagnostic_task_request = None;
        if self.detail_context.kind != "任务详情" {
            return;
        }
        let Some(task) = serde_json::from_str::<Value>(&self.detail)
            .ok()
            .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_owned))
            .filter(|id| Uuid::parse_str(id).is_ok())
        else {
            return;
        };
        let token = Uuid::new_v4();
        self.workbench.diagnostic_task_request = Some((token, task.clone()));
        self.request(
            backend,
            &format!("diagnostics.task/{token}"),
            "execution.get",
            json!({"task_id":task,"include_text":true}),
        );
    }

    pub(super) fn diagnostic_task_response(&mut self, token: &str, result: &Result<Value>) {
        let Some((expected, task)) = self.workbench.diagnostic_task_request.as_ref() else {
            return;
        };
        if Uuid::parse_str(token).ok() != Some(*expected) {
            return;
        }
        let task = task.clone();
        self.workbench.diagnostic_task_request = None;
        if self.workbench.overlay.as_deref() != Some("diagnostics")
            || self.detail_context.kind != "任务详情"
        {
            return;
        }
        let current = serde_json::from_str::<Value>(&self.detail).ok();
        if current
            .as_ref()
            .and_then(|value| value.get("id"))
            .and_then(Value::as_str)
            != Some(task.as_str())
        {
            return;
        }
        match result {
            Ok(value) if value.get("id").and_then(Value::as_str) == Some(task.as_str()) => {
                self.detail = value.to_string()
            }
            Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
            _ => {}
        }
    }

    // 异步响应先按 tab 和 generation 路由，任务、分页等再核对各自的 ID，
    // 避免迟到响应覆盖切换后的上下文；处理后台 tab 后恢复原活动 tab。
    pub(super) fn routed_response(&mut self, mut response: Response, backend: &Backend) {
        if let Some((tab, token)) = response
            .tag
            .strip_prefix("draft-scope-release/")
            .and_then(|tag| tag.split_once('/'))
            && let (Ok(tab), Ok(token)) = (Uuid::parse_str(tab), Uuid::parse_str(token))
        {
            if let Some(index) = self.tabs.iter().position(|item| item.id == tab) {
                let active = self.active;
                self.switch_tab(index);
                self.draft_scope_release_response(token, &response.result);
                self.switch_tab(active);
            }
            return;
        }
        if response.tag.starts_with("workspace.upload.") {
            self.workspace_upload_response(response, backend);
            return;
        }
        if response.tag.starts_with("advanced-value-retired/") {
            if let Err(error) = response.result {
                self.status = i18n::UiMessage::raw(error.to_string());
            }
            return;
        }
        if response.tag.starts_with("editing-files/") {
            self.editing_files_response(response, backend);
            return;
        }
        if let Some(token) = response.tag.strip_prefix("archery-servers-global/") {
            if Uuid::parse_str(token).ok() != self.server_list_token {
                return;
            }
            self.server_list_token = None;
            response.tag = "archery-servers-global".into();
            self.archery_global_servers_response(response);
            return;
        }
        if let Some(token) = response.tag.strip_prefix("history.page/") {
            self.history_response(token, &response.result, backend);
            return;
        }
        if let Some(token) = response.tag.strip_prefix("history.text/") {
            self.history_text_response(token, &response.result);
            return;
        }
        if response.tag.starts_with("archery-ui/") {
            self.archery_response(response, backend);
            return;
        }
        if self.redis_selection_response(backend, &response.tag, &response.result) {
            return;
        }
        if self.tabs_close_response(&response, backend) {
            return;
        }
        if let Some((kind, id)) = response.tag.split_once('/')
            && matches!(kind, "profile.save" | "profile.test")
            && Uuid::parse_str(id).ok() == self.form_id
        {
            if kind == "profile.save"
                && let Ok(id) = Uuid::parse_str(id)
            {
                self.profile_saves.remove(&id);
            }
            response.tag = kind.to_owned();
            self.response(response, backend);
            return;
        }
        if let Some(token) = response.tag.strip_prefix("profile.list/") {
            if Uuid::parse_str(token).ok() != self.profile_list_token {
                return;
            }
            self.profile_list_token = None;
            response.tag = "profile.list".into();
        }
        if let Some(id) = response
            .tag
            .rsplit(':')
            .next()
            .and_then(|tag| tag.strip_prefix("profile.save/"))
            .and_then(|id| Uuid::parse_str(id).ok())
        {
            self.profile_saves.remove(&id);
        }
        if response.tag.starts_with("mcp-rules-get/")
            || response.tag.starts_with("mcp-rules-set/")
            || response.tag.starts_with("mcp-policy-set/")
        {
            self.mcp_rules_response(&response.tag, &response.result, backend);
            return;
        }
        if self.approval_response(&response.tag, &response.result, backend) {
            return;
        }
        if response.tag.starts_with("settings.") {
            self.settings_response(response, backend);
            return;
        }
        if self.navigation_response(&response, backend) {
            return;
        }
        let mut pieces = response.tag.splitn(3, ':');
        let route = pieces
            .next()
            .and_then(|s| Uuid::parse_str(s).ok())
            .and_then(|id| {
                Some((
                    id,
                    pieces.next()?.parse::<u64>().ok()?,
                    pieces.next()?.to_owned(),
                ))
            });
        if let Some((id, generation, tag)) = route {
            let Some(index) = self.tabs.iter().position(|tab| tab.id == id) else {
                if response.tag.contains(":advanced-value/") {
                    self.advanced_value_release_stale_preview(backend, &response.result);
                }
                return;
            };
            if self.tab_is_closing(id) {
                if response.tag.contains(":advanced-value/") {
                    self.advanced_value_release_stale_preview(backend, &response.result);
                }
                return;
            }
            let old = self.active;
            let old_status = (index != old).then(|| self.status.clone());
            self.switch_tab(index);
            if generation == self.generation {
                if tag.strip_prefix("draft-scope/").is_some_and(|token| {
                    self.draft_scope_response(token, &response.result, backend)
                }) || self.workspace_restore_response(&tag, &response.result, backend)
                    || self.advanced_schema_response(&tag, &response.result, backend)
                    || self.advanced_value_response(&tag, &response.result, backend)
                    || self.replace_response(&tag, &response.result, backend)
                    || self.redis_row_delete_response(&tag, &response.result)
                {
                } else if let Some(token) = tag.strip_prefix("change.preview/") {
                    self.change_preview_response(token, &response.result);
                } else if let Some(token) = tag.strip_prefix("diagnostics.task/") {
                    self.diagnostic_task_response(token, &response.result);
                } else if let Some(task) = tag.strip_prefix("execution.get/") {
                    if !self.advanced_schema_task_response(task, &response.result, backend)
                        && self.task.as_deref() == Some(task)
                    {
                        response.tag = "execution.get".into();
                        self.response(response, backend);
                    }
                } else if let Some(token) = tag.strip_prefix("trigger-list/") {
                    self.trigger_list_response(token, &response.result);
                } else if let Some(token) = tag.strip_prefix("resource.list/") {
                    if Uuid::parse_str(token).ok() == self.scan_token {
                        self.scan_token = None;
                        response.tag = "resource.list".into();
                        self.response(response, backend);
                    }
                } else if let Some(token) = tag.strip_prefix("result.page/") {
                    if Uuid::parse_str(token).ok() == self.page_token {
                        self.page_token = None;
                        response.tag = "result.page".into();
                        self.response(response, backend);
                    }
                } else if let Some((result_id, token)) = tag
                    .strip_prefix("result.count/")
                    .and_then(|value| value.split_once('/'))
                {
                    if let Some(state) = self.workbench.query_ui.pagination.get_mut(result_id)
                        && state.count_token == Uuid::parse_str(token).ok()
                        && state.count_token.is_some()
                    {
                        state.count_token = None;
                        match response.result {
                            Ok(value) if self.result_id.as_deref() == Some(result_id) => {
                                if let Some(grid) = self
                                    .grid
                                    .as_mut()
                                    .filter(|grid| grid.page.result.id.to_string() == result_id)
                                {
                                    grid.page.result.summary["total_rows"] =
                                        value["total_rows"].clone();
                                    grid.page.result.summary["counted_at"] =
                                        value["counted_at"].clone();
                                }
                            }
                            Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
                            _ => {}
                        }
                    }
                } else if let Some(token) = tag.strip_prefix("transfer.preview/") {
                    if Uuid::parse_str(token).ok()
                        == self.preview_pending.as_ref().map(|(id, _)| *id)
                    {
                        response.tag = "transfer.preview".into();
                        self.response(response, backend);
                    }
                } else if let Some(token) = tag.strip_prefix("current.info/") {
                    if Uuid::parse_str(token).ok()
                        == self.current_pending.as_ref().map(|(id, _)| *id)
                    {
                        response.tag = "current.info".into();
                        self.response(response, backend);
                    }
                } else if let Some(token) = tag.strip_prefix("file.open/") {
                    if Uuid::parse_str(token).ok()
                        == self.file_open_pending.as_ref().map(|(id, _, _)| *id)
                    {
                        response.tag = "file.open".into();
                        self.response(response, backend);
                    }
                } else if let Some((kind, id)) = tag.split_once('/')
                    && matches!(kind, "profile.save" | "profile.test")
                {
                    if Uuid::parse_str(id).ok() == self.form_id {
                        response.tag = kind.into();
                        self.response(response, backend);
                    } else if kind == "profile.save" {
                        self.request(backend, "profile.list", "profile.list", json!({}));
                    }
                } else {
                    response.tag = tag;
                    self.response(response, backend);
                }
            } else if tag.starts_with("advanced-value/") {
                if self.workbench.value_schema_generations == Some((generation, self.generation)) {
                    self.advanced_value_response(&tag, &response.result, backend);
                } else {
                    self.advanced_value_release_stale_preview(backend, &response.result);
                }
            } else if self.workbench.value_schema_generations == Some((generation, self.generation))
            {
                if tag.starts_with("replace/") {
                    self.replace_response(&tag, &response.result, backend);
                } else if tag.starts_with("schema/") {
                    self.advanced_schema_response(&tag, &response.result, backend);
                } else if matches!(
                    tag.as_str(),
                    "execution.start"
                        | "archery.sql_check"
                        | "resource.browse"
                        | "change.submit"
                        | "transfer.import"
                        | "transfer.export"
                ) {
                    response.tag = tag;
                    self.response(response, backend);
                } else if let Some(task) = tag.strip_prefix("execution.get/")
                    && self.task.as_deref() == Some(task)
                {
                    response.tag = "execution.get".into();
                    self.response(response, backend);
                }
            }
            self.switch_tab(old);
            if let Some(status) = old_status {
                self.status = status;
            }
        } else {
            self.response(response, backend);
        }
    }
    pub(super) fn request(&mut self, backend: &Backend, tag: &str, method: &str, params: Value) {
        if method == "connection.open"
            && params["profile_id"]
                .as_str()
                .and_then(|id| Uuid::parse_str(id).ok())
                .is_some_and(|id| self.navigation_profile_delete_pending(id))
        {
            self.status = i18n::UiMessage::key("正在删除连接，请等待");
            return;
        }
        let global = matches!(tag, "profile.list" | "workspace.load" | "workspace.save")
            || method == "profile.save"
            || method == "profile.test";
        let long = matches!(
            method,
            "resource.browse"
                | "connection.open"
                | "execution.start"
                | "archery.sql_check"
                | "change.submit"
                | "transfer.import"
                | "transfer.export"
        );
        if long && (self.operation_pending || self.task.is_some()) {
            self.status = "该 tab 已有正在执行的任务".into();
            return;
        }
        if matches!(
            method,
            "execution.start" | "resource.browse" | "change.open"
        ) && self.has_live_drafts()
        {
            self.status = "请先提交或丢弃待提交变更与粘贴预览".into();
            return;
        }
        let save_id = (method == "profile.save")
            .then(|| {
                params["profile"]["id"]
                    .as_str()
                    .and_then(|id| Uuid::parse_str(id).ok())
            })
            .flatten();
        if let Some(id) = save_id
            && (self.profile_saves.contains(&id) || self.navigation_profile_delete_pending(id))
        {
            self.status = i18n::UiMessage::key("该连接存在草稿或任务，请先处理");
            return;
        }
        let tag = if tag == "profile.list" {
            let token = Uuid::new_v4();
            self.profile_list_token = Some(token);
            format!("profile.list/{token}")
        } else if global {
            tag.to_owned()
        } else {
            format!("{}:{}:{tag}", self.tabs[self.active].id, self.generation)
        };
        if method == "connection.open" {
            self.archery_capture_connection_open(&params);
        }
        match backend.request(tag, method, params) {
            Ok(()) => {
                if let Some(id) = save_id {
                    self.profile_saves.insert(id);
                }
                if long {
                    self.operation_pending = true
                }
                if method == "connection.open" {
                    self.workbench.connecting = true;
                    self.workbench.connection_error = None;
                    self.workbench.connection_tls_required = false;
                }
            }
            Err(e) => {
                let error = e.to_string();
                if method == "connection.open" {
                    self.workbench.connecting = false;
                    self.workbench.connection_error = Some(error.clone());
                    self.workbench.connection_tls_required = e.code == "tls_required";
                } else if matches!(method, "profile.save" | "profile.test") {
                    self.workbench.profile_error = Some(error.clone());
                }
                self.status = i18n::UiMessage::raw(error);
            }
        }
    }
    pub(super) fn refresh_archery_servers(&mut self, backend: &Backend) {
        let token = Uuid::new_v4();
        self.server_list_token = Some(token);
        if let Err(error) = backend.request(
            format!("archery-servers-global/{token}"),
            "archery.server.list",
            json!({}),
        ) {
            self.server_list_token = None;
            self.status = i18n::UiMessage::raw(error.to_string());
        }
    }
}
