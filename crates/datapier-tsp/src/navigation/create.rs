use super::*;

impl App {
    pub(crate) fn navigation_database_create(&self) -> Option<&state::DatabaseCreate> {
        self.navigation.create_database.as_ref().filter(|form| {
            self.tabs
                .get(self.active)
                .is_some_and(|tab| tab.id == form.tab)
                && self.workbench.overlay.as_deref() == Some("database-create")
        })
    }

    pub(crate) fn navigation_database_create_for_tab(&self, tab: Uuid) -> bool {
        self.navigation
            .create_database
            .as_ref()
            .is_some_and(|form| form.tab == tab)
    }

    pub(crate) fn create_database_close_then(
        &mut self,
        action: Option<String>,
        backend: &Backend,
    ) -> bool {
        if let Some(form) = self.navigation.create_database.as_mut() {
            form.after_close = action;
        }
        self.create_database_close(backend)
    }
    pub(crate) fn database_create_input(&self) -> Option<(String, String, String)> {
        let form = self.navigation_database_create()?;
        Some((
            self.field("database-create-name").trim().to_owned(),
            form.charset.clone(),
            form.collation.clone(),
        ))
    }

    fn discard_database_plan(backend: &Backend, plan: &Value) {
        let _ = backend.request(
            "navigation.discard",
            "schema.discard",
            json!({"plan_id":plan["id"],"revision":plan["revision"]}),
        );
    }

    pub(crate) fn navigation_poll_create_database(&mut self, backend: &Backend) -> bool {
        let current = self.database_create_input();
        if let Some(current) = current {
            let mut discard = None;
            let mut should_plan = false;
            if let Some(form) = self.navigation.create_database.as_mut() {
                if !form.unknown && form.task.is_none() && current != form.observed {
                    form.observed = current.clone();
                    form.changed_at = Instant::now();
                    form.sql = None;
                    form.plan_input = None;
                    form.error = None;
                    if form.preview_input.is_none() {
                        discard = form.plan.take();
                    }
                }
                if !form.unknown
                    && form.task.is_none()
                    && !form.busy
                    && form.options.is_some()
                    && !current.0.is_empty()
                    && form.attempted.as_ref() != Some(&current)
                    && form.changed_at.elapsed() >= Duration::from_millis(250)
                {
                    form.attempted = Some(current);
                    should_plan = true;
                }
            }
            if let Some(plan) = discard {
                Self::discard_database_plan(backend, &plan);
            }
            if should_plan {
                self.create_database_action("database-create-plan", backend);
                return true;
            }
        }
        let Some(form) = self.navigation.create_database.as_mut() else {
            return false;
        };
        let Some(task) = form.task.clone() else {
            return false;
        };
        if form.polling || form.last_poll.elapsed() < Duration::from_millis(700) {
            return false;
        }
        form.polling = true;
        form.last_poll = Instant::now();
        let token = form.token;
        self.navigation_request(
            backend,
            Pending::CreateDatabase {
                token,
                stage: "poll",
            },
            "execution.get",
            json!({"task_id":task}),
        );
        true
    }

    fn create_database_request(
        &mut self,
        backend: &Backend,
        stage: &'static str,
        method: &str,
        params: Value,
    ) {
        let Some(form) = self.navigation.create_database.as_mut() else {
            return;
        };
        form.busy = true;
        form.error = None;
        let token = form.token;
        self.navigation_request(
            backend,
            Pending::CreateDatabase { token, stage },
            method,
            params,
        );
    }

    pub(crate) fn create_database_close(&mut self, backend: &Backend) -> bool {
        let Some(form) = self.navigation.create_database.as_ref() else {
            return false;
        };
        if form.busy || form.task.is_some() {
            self.status = UiMessage::key("操作进行中，请等待完成后再关闭");
            return true;
        }
        if let Some(session) = form.session.clone() {
            self.create_database_request(
                backend,
                "close",
                "connection.close",
                json!({"session_id":session}),
            );
            return true;
        }
        let form = self.navigation.create_database.take().unwrap();
        if !form.unknown
            && let Some(plan) = form.plan
        {
            let _ = backend.request(
                "navigation.discard",
                "schema.discard",
                json!({"plan_id":plan["id"],"revision":plan["revision"]}),
            );
        }
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == form.tab) {
            let workbench = if index == self.active {
                &mut self.workbench
            } else {
                &mut self.tabs[index].runtime.workbench
            };
            if workbench.overlay.as_deref() == Some("database-create") {
                workbench.overlay = None;
            }
        }
        if let Some(action) = form.after_close {
            if self
                .tabs
                .get(self.active)
                .is_some_and(|tab| tab.id == form.tab)
            {
                self.action(&action, backend);
            } else {
                self.status = UiMessage::key("标签已切换，请重新操作");
            }
        }
        true
    }

    pub(super) fn create_database_action(&mut self, name: &str, backend: &Backend) -> bool {
        if let Some(value) = name.strip_prefix("profile-create-database:") {
            let Ok(id) = Uuid::parse_str(value) else {
                return true;
            };
            let Some(profile) = self.profiles.iter().find(|p| p.id == id) else {
                return true;
            };
            if profile.provider != datapier_core::model::ProviderKind::Mysql
                || profile.is_archery()
                || profile.ensure_write().is_err()
            {
                self.status = UiMessage::key("仅可在可写 MySQL 连接中新建数据库");
                return true;
            }
            if self.navigation_connection_state(id).0 != "已连接" {
                self.status = UiMessage::key("请先连接数据库");
                return true;
            }
            if self.navigation.create_database.is_some() {
                self.status = UiMessage::key("该连接存在草稿或任务，请先处理");
                return true;
            }
            let profile_value = serde_json::to_value(profile).unwrap_or(Value::Null);
            if self.tabs.is_empty() {
                self.create_query_tab();
            }
            let token = Uuid::new_v4();
            self.navigation.create_database = Some(state::DatabaseCreate {
                token,
                tab: self.tabs[self.active].id,
                profile_id: id,
                profile: profile_value,
                session: None,
                options: None,
                charset: String::new(),
                collation: String::new(),
                observed: (String::new(), String::new(), String::new()),
                attempted: None,
                preview_input: None,
                plan_input: None,
                changed_at: Instant::now(),
                open_select: None,
                plan: None,
                sql: None,
                error: None,
                busy: false,
                task: None,
                polling: false,
                last_poll: Instant::now(),
                unknown: false,
                after_close: None,
            });
            self.field_set("database-create-name", "");
            self.workbench.overlay = Some("database-create".into());
            let secret = if self.form_id == Some(id) && !self.field("password").is_empty() {
                Some(self.field("password").to_owned())
            } else {
                None
            };
            self.create_database_request(
                backend,
                "open",
                "connection.open",
                json!({"profile_id":id,"secret":secret}),
            );
            return true;
        }
        if name == "database-create-cancel" {
            return self.create_database_close(backend);
        }
        if let Some(value) = name.strip_prefix("database-create-select-open:") {
            if let Some(form) = self.navigation.create_database.as_mut() {
                form.open_select = (!value.is_empty()).then(|| value.to_owned());
            }
            return true;
        }
        if let Some(value) = name.strip_prefix("database-create-charset:") {
            if let Some(form) = self.navigation.create_database.as_mut().filter(|f| {
                !f.unknown && f.task.is_none() && (!f.busy || f.preview_input.is_some())
            }) {
                if !value.is_empty()
                    && !form
                        .options
                        .as_ref()
                        .and_then(|options| options["collations"].as_array())
                        .is_some_and(|rows| {
                            rows.iter()
                                .any(|row| row["charset"].as_str() == Some(value))
                        })
                {
                    return true;
                }
                form.charset = value.to_owned();
                form.collation.clear();
                form.open_select = None;
            }
            return true;
        }
        if let Some(value) = name.strip_prefix("database-create-collation:") {
            if let Some(form) = self.navigation.create_database.as_mut().filter(|f| {
                !f.unknown && f.task.is_none() && (!f.busy || f.preview_input.is_some())
            }) {
                let Some(options) = form.options.as_ref() else {
                    return true;
                };
                let selected_charset = if form.charset.is_empty() {
                    options["charset"].as_str()
                } else {
                    Some(form.charset.as_str())
                };
                if !value.is_empty()
                    && !options["collations"].as_array().is_some_and(|rows| {
                        rows.iter().any(|row| {
                            row["charset"].as_str() == selected_charset
                                && row["name"].as_str() == Some(value)
                        })
                    })
                {
                    return true;
                }
                if !value.is_empty() && form.charset.is_empty() {
                    let Some(charset) = selected_charset else {
                        return true;
                    };
                    form.charset = charset.to_owned();
                }
                form.collation = value.to_owned();
                form.open_select = None;
            }
            return true;
        }
        if name == "database-create-plan" {
            let Some(form) = self
                .navigation_database_create()
                .filter(|f| !f.busy && f.plan.is_none() && f.options.is_some() && !f.unknown)
                .cloned()
            else {
                return true;
            };
            let db = self.field("database-create-name").trim().to_owned();
            if db.is_empty() {
                self.navigation.create_database.as_mut().unwrap().error =
                    Some("数据库名称不能为空".into());
                return true;
            }
            let current = self
                .profiles
                .iter()
                .find(|p| p.id == form.profile_id)
                .and_then(|p| serde_json::to_value(p).ok());
            if current.as_ref() != Some(&form.profile) {
                self.navigation.create_database.as_mut().unwrap().error =
                    Some("连接配置已更改，请重新打开".into());
                return true;
            }
            let input = (db.clone(), form.charset.clone(), form.collation.clone());
            self.navigation
                .create_database
                .as_mut()
                .unwrap()
                .preview_input = Some(input);
            let charset = (!form.charset.is_empty()).then_some(form.charset);
            let collation = (!form.collation.is_empty()).then_some(form.collation);
            let target = json!({"kind":"database","name":db,"database":db,"key_b64":null});
            self.create_database_request(backend, "plan", "schema.plan", json!({"session_id":form.session,"workspace_id":self.workspace_id,"intent":{"target":target,"baseline":null,"definition":{"kind":"database","charset":charset,"collation":collation},"profile_revision":"","algorithm":null,"lock":null,"server_version":"","sql_mode":""}}));
            return true;
        }
        if name == "database-create-submit" {
            let Some(form) = self
                .navigation_database_create()
                .filter(|f| {
                    !f.busy
                        && !f.unknown
                        && f.sql.is_some()
                        && f.plan_input == self.database_create_input()
                })
                .cloned()
            else {
                return true;
            };
            let Some(plan) = form.plan else { return true };
            self.create_database_request(backend, "submit", "schema.submit", json!({"plan_id":plan["id"],"revision":plan["revision"],"digest":plan["plan_digest"]}));
            return true;
        }
        false
    }

    pub(super) fn create_database_response(
        &mut self,
        token: Uuid,
        stage: &'static str,
        result: Result<Value>,
        backend: &Backend,
    ) {
        let name = self.field("database-create-name").trim().to_owned();
        let Some(form) = self
            .navigation
            .create_database
            .as_mut()
            .filter(|form| form.token == token)
        else {
            return;
        };
        if matches!(stage, "plan" | "preview") {
            let current = (name, form.charset.clone(), form.collation.clone());
            if form.preview_input.as_ref() != Some(&current) {
                let stale = if stage == "plan" {
                    result.as_ref().ok().cloned()
                } else {
                    form.plan.take()
                };
                form.busy = false;
                form.preview_input = None;
                form.plan_input = None;
                form.sql = None;
                form.attempted = None;
                if let Some(plan) = stale {
                    Self::discard_database_plan(backend, &plan);
                }
                return;
            }
        }
        form.busy = false;
        if stage == "poll" {
            form.polling = false;
        }
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if matches!(stage, "plan" | "preview") {
                    form.preview_input = None;
                    if let Some(plan) = form.plan.take() {
                        Self::discard_database_plan(backend, &plan);
                    }
                }
                if stage == "close" && error.code == "not_found" {
                    form.session = None;
                    self.create_database_close(backend);
                    return;
                }
                if stage == "refresh" {
                    form.error = Some(format!("数据库已创建，但刷新列表失败：{error}"));
                    return;
                }
                if stage == "submit"
                    && matches!(
                        error.code.as_str(),
                        "outcome_unknown" | "disconnected" | "timeout"
                    )
                {
                    form.unknown = true;
                }
                form.error = Some(error.to_string());
                return;
            }
        };
        match stage {
            "close" => {
                form.session = None;
                self.create_database_close(backend);
            }
            "open" => {
                let Some(session) = value.get("session_id").filter(|v| !v.is_null()).cloned()
                else {
                    form.error = Some("连接未返回会话".into());
                    return;
                };
                form.session = Some(session.clone());
                if value.get("profile") != Some(&form.profile) {
                    form.error = Some("连接配置已更改，请重新打开".into());
                    return;
                }
                self.create_database_request(backend, "options", "schema.describe", json!({"session_id":session,"target":{"kind":"database_options","name":"","database":null,"key_b64":null}}));
            }
            "options" => {
                let charset = value["charset"].as_str().filter(|name| !name.is_empty());
                let collation = value["collation"].as_str().filter(|name| !name.is_empty());
                let valid = value["collations"].as_array().is_some_and(|rows| {
                    !rows.is_empty()
                        && rows.iter().all(|row| {
                            row["charset"].as_str().is_some_and(|name| !name.is_empty())
                                && row["name"].as_str().is_some_and(|name| !name.is_empty())
                                && row["default"].is_boolean()
                        })
                        && rows.iter().any(|row| {
                            row["charset"].as_str() == charset && row["name"].as_str() == collation
                        })
                        && rows.iter().all(|row| {
                            rows.iter().any(|candidate| {
                                candidate["charset"] == row["charset"]
                                    && candidate["default"] == true
                            })
                        })
                }) && charset.is_some()
                    && collation.is_some();
                if valid {
                    form.options = Some(value);
                } else {
                    form.error = Some("服务端字符集或排序规则元数据不完整".into());
                }
            }
            "plan" => {
                if value["submittable"] != true
                    || value["complete"] != true
                    || value["total_items"] != 1
                {
                    form.preview_input = None;
                    Self::discard_database_plan(backend, &value);
                    form.error = Some("数据库计划不可执行".into());
                    return;
                }
                form.plan = Some(value.clone());
                self.create_database_request(
                    backend,
                    "preview",
                    "schema.preview",
                    json!({"plan_id":value["id"],"revision":value["revision"],"offset":0}),
                );
            }
            "preview" => {
                let valid = form.plan.as_ref().is_some_and(|plan| {
                    plan["id"] == value["id"]
                        && plan["revision"] == value["revision"]
                        && plan["plan_digest"] == value["plan_digest"]
                }) && value["next"].is_null()
                    && value["offset"] == 0
                    && value["complete"] == true
                    && value["total_items"] == 1
                    && value["steps"]
                        .as_array()
                        .is_some_and(|steps| steps.len() == 1);
                let sql = value.pointer("/steps/0/sql").and_then(Value::as_str);
                if !valid || sql.is_none() {
                    form.preview_input = None;
                    if let Some(plan) = form.plan.take() {
                        Self::discard_database_plan(backend, &plan);
                    }
                    form.error = Some("数据库计划预览不完整".into());
                    return;
                }
                form.sql = sql.map(str::to_owned);
                form.plan_input = form.preview_input.take();
            }
            "submit" => {
                if let Some(task) = value.get("task_id").and_then(Value::as_str) {
                    form.task = Some(task.to_owned());
                    form.last_poll = Instant::now() - Duration::from_secs(1);
                } else {
                    form.unknown = true;
                    form.error = Some("执行结果待确认，请核对数据库".into());
                }
            }
            "poll" => {
                let phase = value["state"].as_str().unwrap_or("");
                if matches!(phase, "queued" | "running" | "cancelling") {
                    return;
                }
                if !matches!(
                    phase,
                    "succeeded" | "failed" | "cancelled" | "outcome_unknown" | "wait_stopped"
                ) {
                    return;
                }
                form.task = None;
                let applied = phase == "succeeded"
                    && value.pointer("/summary/state").and_then(Value::as_str) == Some("applied")
                    && value
                        .pointer("/summary/steps")
                        .and_then(Value::as_array)
                        .is_some_and(|steps| {
                            !steps.is_empty() && steps.iter().all(|step| step["state"] == "applied")
                        });
                if applied {
                    let profile = form.profile_id;
                    let session = form.session.clone();
                    // DDL 已确认生效；刷新失败也绝不能允许重复提交。
                    form.unknown = true;
                    self.navigation_invalidate_profile(profile);
                    self.create_database_request(
                        backend,
                        "refresh",
                        "resource.list",
                        json!({"session_id":session,"database":null,"limit":200}),
                    );
                } else {
                    form.unknown = matches!(phase, "outcome_unknown" | "wait_stopped")
                        || value.pointer("/summary/state").and_then(Value::as_str)
                            == Some("outcome_unknown");
                    form.error = Some(if form.unknown {
                        "执行结果待确认，请核对数据库".into()
                    } else {
                        value.to_string()
                    });
                    form.plan = None;
                    form.sql = None;
                }
            }
            "refresh" => {
                let profile = form.profile_id;
                let created_name = form
                    .plan
                    .as_ref()
                    .and_then(|plan| plan.pointer("/target/name"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let hidden = self
                    .profiles
                    .iter()
                    .find(|p| p.id == profile)
                    .and_then(navigation_visible_databases)
                    .is_some_and(|names| !names.contains(&created_name));
                match serde_json::from_value::<ResourcePage>(value) {
                    Ok(page) => {
                        let tree = self.navigation.databases.entry(profile).or_default();
                        tree.databases = page.resources;
                        tree.loaded = true;
                        tree.loading = false;
                        tree.error = None;
                        self.status = UiMessage::key(if hidden {
                            "数据库已创建，但不在连接显示范围内；可在连接设置中选择"
                        } else {
                            "数据库已创建"
                        });
                        self.create_database_close(backend);
                    }
                    Err(error) => {
                        form.error = Some(format!("数据库已创建，但刷新列表失败：{error}"))
                    }
                }
            }
            _ => {}
        }
    }
}
