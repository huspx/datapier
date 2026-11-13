use super::*;

impl App {
    /// 返回 true 表示响应已由导航消费，必须在 tab 路由前调用。
    pub(crate) fn navigation_response(&mut self, response: &Response, backend: &Backend) -> bool {
        if response.tag == "navigation.close" {
            if let Err(error) = &response.result {
                self.status = UiMessage::raw(error.to_string());
            }
            return true;
        }
        if response.tag == "navigation.discard" {
            if let Err(error) = &response.result {
                self.status = UiMessage::raw(error.to_string());
            }
            return true;
        }
        let Some(token) = response
            .tag
            .strip_prefix("navigation/")
            .and_then(|v| Uuid::parse_str(v).ok())
        else {
            return false;
        };
        if let Some(pending) = self.navigation.pending.remove(&token) {
            self.navigation_finish(pending, response.result.clone(), backend);
        } else if let Ok(value) = &response.result
            && let Some(session) = value.get("session_id")
        {
            let _ = backend.request(
                "navigation.close",
                "connection.close",
                json!({"session_id":session}),
            );
        }
        true
    }

    pub(super) fn navigation_finish(
        &mut self,
        pending: Pending,
        result: Result<Value>,
        backend: &Backend,
    ) {
        match pending {
            Pending::CreateDatabase { token, stage } => {
                self.create_database_response(token, stage, result, backend)
            }
            Pending::DropOpen { token } => {
                let Some(operation) = self
                    .navigation
                    .drop_operation
                    .as_ref()
                    .filter(|operation| operation.confirmation.token == token)
                else {
                    return;
                };
                let drop = operation.confirmation.clone();
                match result {
                    Err(error) => {
                        self.navigation_drop_complete(token, "failed", error.to_string(), backend)
                    }
                    Ok(value) => {
                        let Some(session) = value
                            .get("session_id")
                            .filter(|session| !session.is_null())
                            .cloned()
                        else {
                            self.navigation_drop_complete(
                                token,
                                "failed",
                                value.to_string(),
                                backend,
                            );
                            return;
                        };
                        if session == drop.session {
                            self.navigation_drop_complete(
                                token,
                                "failed",
                                "独立会话无效".into(),
                                backend,
                            );
                            return;
                        }
                        let source_valid = self.navigation_drop_source_valid(&drop)
                            && self.navigation_drop_target_exists(&drop);
                        let operation = self.navigation.drop_operation.as_mut().unwrap();
                        operation.ddl_session = Some(session.clone());
                        if value.get("profile") != Some(&drop.profile) || !source_valid {
                            self.navigation_drop_end(
                                token,
                                "failed",
                                "连接配置已更改，请先重连".into(),
                                backend,
                            );
                        } else if operation.cancel_requested {
                            self.navigation_drop_end(token, "cancelled", String::new(), backend);
                        } else {
                            let sql = operation.sql.clone();
                            self.navigation_request(backend, Pending::DropStart { token }, "execution.start",
                                json!({"session_id":session,"workspace_id":self.workspace_id,"text":sql,"mysql_write":true}));
                        }
                    }
                }
            }
            Pending::DropClose { token } => {
                let Some(operation) = self
                    .navigation
                    .drop_operation
                    .as_mut()
                    .filter(|operation| operation.confirmation.token == token)
                else {
                    return;
                };
                operation.closing = false;
                match result {
                    Ok(_) => {
                        let (state, detail) = operation.terminal.take().unwrap();
                        self.navigation_drop_complete(token, &state, detail, backend);
                    }
                    Err(error) if error.code == "not_found" => {
                        let (state, detail) = operation.terminal.take().unwrap();
                        self.navigation_drop_complete(token, &state, detail, backend);
                    }
                    Err(error) => {
                        self.status = UiMessage::key("关闭删除专用连接失败，正在重试");
                        let drop = operation.confirmation.clone();
                        self.navigation_drop_detail(&drop, "错误详情", error.to_string());
                    }
                }
            }
            Pending::DropCancel { token } => {
                let Some(operation) = self
                    .navigation
                    .drop_operation
                    .as_mut()
                    .filter(|operation| operation.confirmation.token == token)
                else {
                    return;
                };
                operation.cancel_pending = false;
                if let Err(error) = result {
                    self.status = UiMessage::raw(error.to_string());
                }
            }
            Pending::DropStart { token } => {
                if self
                    .navigation
                    .drop_operation
                    .as_ref()
                    .is_none_or(|operation| operation.confirmation.token != token)
                {
                    return;
                }
                match result {
                    Ok(value) => {
                        if let Some(task) = value.get("task_id").and_then(Value::as_str) {
                            self.navigation.drop_operation.as_mut().unwrap().task =
                                Some(task.to_owned());
                            if let Some(confirmation) = self
                                .navigation
                                .drop_confirmation
                                .as_mut()
                                .filter(|confirmation| confirmation.token == token)
                            {
                                confirmation.task_started = true;
                            }
                            if self
                                .navigation
                                .drop_operation
                                .as_ref()
                                .unwrap()
                                .cancel_requested
                            {
                                self.navigation_drop_cancel_task(backend);
                            }
                            self.navigation_poll_drop(backend);
                        } else {
                            self.navigation_drop_end(
                                token,
                                "outcome_unknown",
                                value.to_string(),
                                backend,
                            );
                        }
                    }
                    Err(error) => {
                        let state = if matches!(
                            error.code.as_str(),
                            "outcome_unknown" | "disconnected" | "timeout"
                        ) {
                            "outcome_unknown"
                        } else {
                            "failed"
                        };
                        self.navigation_drop_end(token, state, error.to_string(), backend);
                    }
                }
            }
            Pending::DropPoll { token } => {
                let Some(operation) = self
                    .navigation
                    .drop_operation
                    .as_mut()
                    .filter(|operation| operation.confirmation.token == token)
                else {
                    return;
                };
                operation.polling = false;
                match result {
                    Ok(value) => {
                        let state = value.get("state").and_then(Value::as_str).unwrap_or("");
                        if !matches!(state, "queued" | "running" | "cancelling") {
                            let state = if matches!(
                                state,
                                "succeeded"
                                    | "failed"
                                    | "cancelled"
                                    | "outcome_unknown"
                                    | "wait_stopped"
                            ) {
                                state
                            } else {
                                "outcome_unknown"
                            };
                            self.navigation_drop_end(token, state, value.to_string(), backend);
                        }
                    }
                    Err(error) => {
                        self.status = UiMessage::key("无法查询删除任务，正在重试");
                        let drop = operation.confirmation.clone();
                        self.navigation_drop_detail(&drop, "错误详情", error.to_string());
                    }
                }
            }
            Pending::DeleteProfile { id, token } => {
                if self
                    .navigation
                    .delete_operation
                    .as_ref()
                    .is_none_or(|operation| operation.token != token || operation.id != id)
                {
                    return;
                }
                self.navigation.delete_operation = None;
                match result {
                    Ok(_) => {
                        self.profiles.retain(|profile| profile.id != id);
                        self.navigation_invalidate_profile(id);
                        // 删除后拒收此前发出的列表，并重新读取当前配置。
                        self.profile_list_token = None;
                        self.request(backend, "profile.list", "profile.list", json!({}));
                        if self
                            .navigation
                            .delete_confirmation
                            .as_ref()
                            .is_some_and(|confirmation| confirmation.token == token)
                        {
                            let confirmation = self.navigation.delete_confirmation.take().unwrap();
                            if let Some(index) = confirmation
                                .tab
                                .and_then(|id| self.tabs.iter().position(|tab| tab.id == id))
                            {
                                let workbench = if index == self.active {
                                    &mut self.workbench
                                } else {
                                    &mut self.tabs[index].runtime.workbench
                                };
                                if workbench.overlay.as_deref() == Some("profile-delete") {
                                    workbench.overlay = None;
                                }
                            } else if confirmation.tab.is_none()
                                && self.tabs.is_empty()
                                && self.workbench.overlay.as_deref() == Some("profile-delete")
                            {
                                self.workbench.overlay = None;
                            }
                        }
                        self.status = UiMessage::key("连接配置已删除");
                    }
                    Err(error) => {
                        if let Some(confirmation) = self
                            .navigation
                            .delete_confirmation
                            .as_mut()
                            .filter(|confirmation| confirmation.token == token)
                        {
                            confirmation.busy = false;
                            confirmation.error = Some(error.to_string());
                        }
                        self.status = UiMessage::raw(error.to_string());
                    }
                }
            }
            Pending::Form { profile, secret } => {
                let mut current = self
                    .form_profile()
                    .ok()
                    .and_then(|p| serde_json::to_value(p).ok());
                let mut profile = profile;
                if let Some(value) = current.as_mut().and_then(Value::as_object_mut) {
                    value.remove("visible_databases");
                }
                if let Some(value) = profile.as_object_mut() {
                    value.remove("visible_databases");
                }
                let current_secret = if self.field("password").is_empty() {
                    Value::Null
                } else {
                    json!(self.field("password"))
                };
                self.navigation.form.loading = false;
                if current.as_ref() != Some(&profile) || current_secret != secret {
                    return;
                }
                match result {
                    Ok(value) => match serde_json::from_value::<ResourcePage>(value) {
                        Ok(page) => {
                            let form = &mut self.navigation.form;
                            form.candidates = page.resources.into_iter().map(|r| r.name).collect();
                            if form.all {
                                form.selected.clear();
                                form.selected.extend(form.candidates.iter().cloned());
                            }
                            form.loaded = true;
                            form.error = None;
                        }
                        Err(error) => self.navigation.form.error = Some(error.to_string()),
                    },
                    Err(error) => self.navigation.form.error = Some(error.to_string()),
                }
            }
            Pending::Tree {
                profile,
                database,
                session,
            } => {
                // 会话被关闭或配置范围改变后，迟到结果不得重建节点。
                let alive = self.tabs.iter().enumerate().any(|(i, tab)| {
                    tab.profile_id == Some(profile)
                        && if i == self.active {
                            self.session_id().is_some_and(|s| json!(s) == session)
                        } else {
                            tab.runtime
                                .session
                                .as_ref()
                                .and_then(|v| v.get("session_id"))
                                == Some(&session)
                        }
                });
                let allowed = self
                    .profiles
                    .iter()
                    .find(|p| p.id == profile)
                    .is_some_and(|p| {
                        database.as_ref().is_none_or(|d| {
                            navigation_visible_databases(p).is_none_or(|v| v.contains(d))
                        })
                    });
                let Some(tree) = self.navigation.databases.get_mut(&profile) else {
                    return;
                };
                if let Some(database) = database {
                    let Some(node) = tree.nodes.get_mut(&database) else {
                        return;
                    };
                    node.loading = false;
                    if !alive || !allowed {
                        return;
                    }
                    match result.and_then(|v| {
                        serde_json::from_value::<ResourcePage>(v)
                            .map_err(|e| AppError::new("invalid_response", e.to_string()))
                    }) {
                        Ok(page) => {
                            node.resources = page.resources;
                            node.loaded = true;
                            node.error = None;
                        }
                        Err(e) => node.error = Some(e.to_string()),
                    }
                } else {
                    tree.loading = false;
                    if !alive || !allowed {
                        return;
                    }
                    match result.and_then(|v| {
                        serde_json::from_value::<ResourcePage>(v)
                            .map_err(|e| AppError::new("invalid_response", e.to_string()))
                    }) {
                        Ok(page) => {
                            tree.databases = page.resources;
                            tree.loaded = true;
                            tree.error = None;
                        }
                        Err(e) => tree.error = Some(e.to_string()),
                    }
                }
            }
            Pending::RedisDiscovery {
                profile_id,
                profile,
                secret,
                session,
            } => {
                if let Some(id) = profile_id {
                    let alive = self.tabs.iter().enumerate().any(|(index, tab)| {
                        tab.profile_id == Some(id)
                            && if index == self.active {
                                self.session_id()
                                    .is_some_and(|value| Some(json!(value)) == session)
                            } else {
                                tab.runtime
                                    .session
                                    .as_ref()
                                    .and_then(|value| value.get("session_id"))
                                    == session.as_ref()
                            }
                    });
                    if !alive {
                        self.navigation.redis_discovery.remove(&id);
                        return;
                    }
                } else {
                    let current = self
                        .form_profile()
                        .ok()
                        .and_then(|value| serde_json::to_value(value).ok());
                    let current_secret = if self.field("password").is_empty() {
                        Value::Null
                    } else {
                        json!(self.field("password"))
                    };
                    if current != profile || current_secret != secret {
                        self.navigation.form_redis_discovery.loading = false;
                        return;
                    }
                }
                let state = if let Some(id) = profile_id {
                    self.navigation.redis_discovery.entry(id).or_default()
                } else {
                    &mut self.navigation.form_redis_discovery
                };
                state.loading = false;
                match result {
                    Ok(value) => {
                        if let Some(databases) = value.get("databases").and_then(Value::as_array) {
                            state.databases = databases
                                .iter()
                                .filter_map(Value::as_str)
                                .filter(|value| value.parse::<u32>().is_ok())
                                .map(str::to_owned)
                                .collect();
                            state.loaded = true;
                            state.error = value
                                .get("reason")
                                .and_then(Value::as_str)
                                .map(str::to_owned);
                            if state.databases.is_empty() && state.error.is_none() {
                                state.error = Some(
                                    crate::i18n::tr("未能确定 Redis DB 数量，请配置数量覆盖")
                                        .into_owned(),
                                );
                            }
                        } else {
                            state.error =
                                Some(crate::i18n::tr("数据库发现返回了无效结果").into_owned());
                        }
                    }
                    Err(error) => state.error = Some(error.to_string()),
                }
            }
            Pending::Redis {
                tab,
                generation,
                db,
            } => {
                let Some(index) = self.tabs.iter().position(|t| t.id == tab) else {
                    if let Ok(value) = result
                        && let Some(session) = value.get("session_id")
                    {
                        let _ = backend.request(
                            "navigation.close",
                            "connection.close",
                            json!({"session_id":session}),
                        );
                    }
                    return;
                };
                let old = self.active;
                self.switch_tab(index);
                if self.generation != generation {
                    if let Ok(value) = result
                        && let Some(session) = value.get("session_id")
                    {
                        let _ = backend.request(
                            "navigation.close",
                            "connection.close",
                            json!({"session_id":session}),
                        );
                    }
                } else {
                    self.operation_pending = false;
                    match result {
                        Ok(value) => {
                            let profile =
                                serde_json::from_value::<Profile>(value["profile"].clone()).ok();
                            let safe = !self.has_live_drafts()
                                && self.tabs[self.active].pending.is_null()
                                && self.workbench.inline.is_none()
                                && self.workbench.overlay.is_none()
                                && self.task.is_none()
                                && !self.workbench.cli_running;
                            if safe
                                && profile.is_some()
                                && value.get("redis_db").and_then(Value::as_u64)
                                    == Some(u64::from(db))
                                && value.get("session_id").is_some()
                            {
                                if let Some(session) = self.session_id() {
                                    let _ = backend.request(
                                        "navigation.close",
                                        "connection.close",
                                        json!({"session_id":session}),
                                    );
                                }
                                self.close_result_info(false);
                                self.generation = self.generation.wrapping_add(1);
                                self.session = Some(value);
                                self.profile = profile;
                                self.tabs[self.active].browse.redis_db = Some(db);
                                self.workbench.cli_database = None;
                                self.database = Some(db.to_string());
                                self.dirty = true;
                                self.workbench.sql_mode = None;
                                self.target = None;
                                self.resources.clear();
                                self.workbench.redis_tree.reset(&[]);
                                self.workbench.redis_tree_owner = None;
                                self.cursor = None;
                                self.scan_token = None;
                                self.grid = None;
                                self.schema = None;
                                self.edit_token = None;
                                self.results.clear();
                                self.result_id = None;
                                self.page_token = None;
                                self.page_history.clear();
                                self.large = None;
                                self.browse_result = false;
                                self.clear_cli_session();
                                if !self.profile.as_ref().is_some_and(Profile::is_archery) {
                                    self.action("list", backend);
                                }
                            } else {
                                if let Some(session) = value.get("session_id") {
                                    let _ = backend.request(
                                        "navigation.close",
                                        "connection.close",
                                        json!({"session_id":session}),
                                    );
                                }
                                self.status = UiMessage::key(if safe {
                                    "切换数据库返回了无效会话"
                                } else {
                                    "切换期间出现编辑或任务，已保留原会话"
                                });
                            }
                        }
                        Err(error) => self.status = UiMessage::raw(error.to_string()),
                    }
                }
                if old != index {
                    self.switch_tab(old);
                }
            }
        }
    }
}
