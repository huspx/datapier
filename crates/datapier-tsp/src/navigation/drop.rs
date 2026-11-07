use super::*;

impl App {
    pub(crate) fn navigation_drop_confirmation(&self) -> Option<&ResourceDropConfirmation> {
        self.navigation.drop_confirmation.as_ref().filter(|drop| {
            self.tabs
                .get(self.active)
                .is_some_and(|tab| tab.id == drop.tab)
                && self.workbench.overlay.as_deref() == Some("resource-drop")
        })
    }

    pub(crate) fn navigation_drop_running(&self) -> bool {
        self.navigation
            .drop_operation
            .as_ref()
            .is_some_and(|operation| {
                self.tabs
                    .get(self.active)
                    .is_some_and(|tab| tab.id == operation.confirmation.tab)
            })
    }

    pub(crate) fn navigation_poll_drop(&mut self, backend: &Backend) -> bool {
        let Some(operation) = self.navigation.drop_operation.as_mut() else {
            return false;
        };
        if operation.polling
            || operation.closing
            || operation.last_poll.elapsed() < Duration::from_millis(700)
        {
            return false;
        }
        operation.last_poll = Instant::now();
        let token = operation.confirmation.token;
        if operation.terminal.is_some() {
            let Some(session) = operation.ddl_session.clone() else {
                return false;
            };
            operation.closing = true;
            self.navigation_request(
                backend,
                Pending::DropClose { token },
                "connection.close",
                json!({"session_id":session}),
            );
        } else {
            let Some(task) = operation.task.clone() else {
                return false;
            };
            operation.polling = true;
            self.navigation_request(
                backend,
                Pending::DropPoll { token },
                "execution.get",
                json!({"task_id":task}),
            );
        }
        true
    }

    pub(super) fn navigation_drop_target_exists(&self, drop: &ResourceDropConfirmation) -> bool {
        self.navigation
            .databases
            .get(&drop.profile_id)
            .is_some_and(|tree| {
                if drop.kind == "database" {
                    tree.databases
                        .iter()
                        .any(|resource| resource.name == drop.database)
                } else {
                    tree.nodes.get(&drop.database).is_some_and(|node| {
                        node.resources
                            .iter()
                            .any(|resource| resource.kind == "table" && resource.name == drop.name)
                    })
                }
            })
    }

    pub(super) fn navigation_same_connection(left: &Value, right: &Value) -> bool {
        fn identity(value: &Value) -> Option<Value> {
            let mut value = value.clone();
            let profile = value.as_object_mut()?;
            profile.remove("name");
            if let Some(connection) = profile.get_mut("connection").and_then(Value::as_object_mut) {
                connection.remove("visible_databases");
            }
            Some(value)
        }
        identity(left)
            .zip(identity(right))
            .is_some_and(|(left, right)| left == right)
    }

    pub(super) fn navigation_drop_source_valid(&self, drop: &ResourceDropConfirmation) -> bool {
        let Some(index) = self
            .tabs
            .iter()
            .position(|tab| tab.id == drop.tab && tab.profile_id == Some(drop.profile_id))
        else {
            return false;
        };
        let (session, profile, generation) = if index == self.active {
            (
                self.session.as_ref(),
                self.profile.as_ref(),
                self.generation,
            )
        } else {
            let runtime = &self.tabs[index].runtime;
            (
                runtime.session.as_ref(),
                runtime.profile.as_ref(),
                runtime.generation,
            )
        };
        let saved = self
            .profiles
            .iter()
            .find(|profile| profile.id == drop.profile_id)
            .and_then(|profile| serde_json::to_value(profile).ok());
        generation == drop.generation
            && session.and_then(|session| session.get("session_id")) == Some(&drop.session)
            && session.and_then(|session| session.get("profile")) == Some(&drop.source_profile)
            && profile
                .and_then(|profile| serde_json::to_value(profile).ok())
                .as_ref()
                == Some(&drop.session_profile)
            && saved.as_ref() == Some(&drop.profile)
            && Self::navigation_same_connection(&drop.source_profile, &drop.profile)
            && Self::navigation_same_connection(&drop.session_profile, &drop.profile)
            && !self.navigation_profile_delete_pending(drop.profile_id)
    }

    pub(super) fn navigation_drop_context_valid(&self, drop: &ResourceDropConfirmation) -> bool {
        self.tabs[self.active].id == drop.tab
            && self.navigation_drop_source_valid(drop)
            && self.navigation_drop_target_exists(drop)
    }

    pub(super) fn navigation_drop_origin_pending(&mut self, tab: Uuid, pending: bool) {
        if let Some(index) = self.tabs.iter().position(|item| item.id == tab) {
            if index == self.active {
                self.operation_pending = pending;
            } else {
                self.tabs[index].runtime.operation_pending = pending;
            }
        }
    }
    pub(super) fn navigation_drop_invalidate(
        &mut self,
        drop: &ResourceDropConfirmation,
        backend: &Backend,
    ) {
        let old = self.active;
        for index in 0..self.tabs.len() {
            if self.tabs[index].profile_id != Some(drop.profile_id) {
                continue;
            }
            self.switch_tab(index);
            if self.database.as_deref() == Some(drop.database.as_str()) {
                if drop.kind == "database" {
                    self.resources.clear();
                    self.workbench.redis_tree.reset(&[]);
                    self.workbench.redis_tree_owner = None;
                } else {
                    self.resources.retain(|resource| {
                        !(resource.kind == "table" && resource.name == drop.name)
                    });
                }
            } else if drop.kind == "database" && self.database.is_none() {
                self.resources.retain(|resource| {
                    !(resource.kind == "database" && resource.name == drop.database)
                });
            }
            let affected = self.target.as_ref().is_some_and(|target| {
                target.database.as_deref().or(self.database.as_deref())
                    == Some(drop.database.as_str())
                    && (drop.kind == "database"
                        || (target.kind == "table" && target.name == drop.name))
            });
            if !affected {
                continue;
            }
            if !self.operation_pending || self.tabs[self.active].id == drop.tab {
                self.generation = self.generation.wrapping_add(1);
            }
            self.schema = None;
            self.edit_token = None;
            if self.has_live_drafts()
                || !self.tabs[self.active].pending.is_null()
                || self.workbench.inline.is_some()
            {
                if let Some(grid) = self.grid.as_mut() {
                    grid.editable = false;
                }
            } else {
                self.grid = None;
                self.results.clear();
                self.result_id = None;
                self.page_token = None;
                self.page_history.clear();
                self.large = None;
            }
            self.workbench.load_resource = false;
        }
        self.switch_tab(old);
        let database = (drop.kind == "table").then(|| drop.database.clone());
        self.navigation.pending.retain(|_, pending| !matches!(pending, Pending::Tree { profile, database: pending_db, .. }
            if *profile == drop.profile_id && (pending_db == &database || (database.is_none() && pending_db.as_deref() == Some(drop.database.as_str())))));
        let tree = self
            .navigation
            .databases
            .entry(drop.profile_id)
            .or_default();
        if let Some(database) = &database {
            let node = tree.nodes.entry(database.clone()).or_default();
            node.resources
                .retain(|resource| !(resource.kind == "table" && resource.name == drop.name));
            node.loaded = false;
            node.loading = true;
            node.error = None;
        } else {
            tree.databases
                .retain(|resource| resource.name != drop.database);
            tree.nodes.remove(&drop.database);
            tree.loaded = false;
            tree.loading = true;
            tree.error = None;
        }
        self.navigation_request(
            backend,
            Pending::Tree {
                profile: drop.profile_id,
                database: database.clone(),
                session: drop.session.clone(),
            },
            "resource.list",
            json!({"session_id":drop.session,"database":database,"limit":200}),
        );
    }

    pub(super) fn navigation_drop_end(
        &mut self,
        token: Uuid,
        state: &str,
        detail: String,
        backend: &Backend,
    ) {
        let Some(operation) = self
            .navigation
            .drop_operation
            .as_mut()
            .filter(|operation| operation.confirmation.token == token)
        else {
            return;
        };
        if operation.ddl_session.is_none() {
            self.navigation_drop_complete(token, state, detail, backend);
        } else {
            operation.terminal = Some((state.into(), detail));
            operation.last_poll = Instant::now() - Duration::from_secs(1);
            self.navigation_poll_drop(backend);
        }
    }

    pub(super) fn navigation_drop_cancel_task(&mut self, backend: &Backend) {
        let Some(operation) = self.navigation.drop_operation.as_mut() else {
            return;
        };
        if operation.cancel_pending || operation.terminal.is_some() {
            return;
        }
        operation.cancel_requested = true;
        if let Some(confirmation) = self
            .navigation
            .drop_confirmation
            .as_mut()
            .filter(|confirmation| confirmation.token == operation.confirmation.token)
        {
            confirmation.cancel_requested = true;
        }
        if let Some(task) = operation.task.clone() {
            let token = operation.confirmation.token;
            operation.cancel_pending = true;
            self.navigation_request(
                backend,
                Pending::DropCancel { token },
                "execution.cancel",
                json!({"task_id":task}),
            );
        }
        self.status = UiMessage::key("正在取消资源删除");
    }

    pub(super) fn navigation_drop_detail(
        &mut self,
        drop: &ResourceDropConfirmation,
        kind: &'static str,
        detail: String,
    ) {
        let context = DetailContext {
            kind,
            connection: drop
                .profile
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            resource: Some(if drop.kind == "database" {
                drop.database.clone()
            } else {
                format!("{}.{}", drop.database, drop.name)
            }),
        };
        if let Some(index) = self.tabs.iter().position(|tab| tab.id == drop.tab) {
            if index == self.active {
                self.detail = detail;
                self.detail_context = context;
            } else {
                self.tabs[index].runtime.detail = detail;
                self.tabs[index].runtime.detail_context = context;
            }
        }
    }

    pub(super) fn navigation_drop_complete(
        &mut self,
        token: Uuid,
        state: &str,
        detail: String,
        backend: &Backend,
    ) {
        let Some(operation) = self.navigation.drop_operation.take() else {
            return;
        };
        if operation.confirmation.token != token {
            self.navigation.drop_operation = Some(operation);
            return;
        }
        let drop = operation.confirmation;
        self.navigation_drop_origin_pending(drop.tab, false);
        if self
            .navigation
            .drop_confirmation
            .as_ref()
            .is_some_and(|current| current.token == token)
        {
            self.navigation.drop_confirmation = None;
            if let Some(index) = self.tabs.iter().position(|tab| tab.id == drop.tab) {
                let workbench = if index == self.active {
                    &mut self.workbench
                } else {
                    &mut self.tabs[index].runtime.workbench
                };
                if workbench.overlay.as_deref() == Some("resource-drop") {
                    workbench.overlay = None;
                }
            }
        }
        if matches!(state, "succeeded" | "outcome_unknown" | "wait_stopped") {
            self.navigation_drop_invalidate(&drop, backend);
        }
        self.status = UiMessage::key(match state {
            "succeeded" => "资源已删除",
            "cancelled" => "资源删除已取消",
            "outcome_unknown" | "wait_stopped" => "删除结果未知，请检查数据库后再操作",
            _ => "资源删除失败",
        });
        if detail == "连接配置已更改，请先重连" {
            self.status = UiMessage::key("连接配置已更改，请先重连");
        }
        self.navigation_drop_detail(&drop, "任务详情", detail);
    }

    pub(super) fn navigation_drop_action(&mut self, name: &str, backend: &Backend) -> bool {
        if name == "cancel"
            && self
                .navigation
                .drop_operation
                .as_ref()
                .is_some_and(|operation| operation.confirmation.tab == self.tabs[self.active].id)
        {
            self.navigation_drop_cancel_task(backend);
            return true;
        }
        if let Some(value) = name.strip_prefix("resource-drop-stop:") {
            if self
                .navigation
                .drop_operation
                .as_ref()
                .is_some_and(|operation| {
                    operation.confirmation.token.to_string() == value
                        && operation.confirmation.tab == self.tabs[self.active].id
                })
            {
                self.navigation_drop_cancel_task(backend);
            }
            return true;
        }
        if let Some(value) = name.strip_prefix("resource-drop-cancel:") {
            if self
                .navigation_drop_confirmation()
                .is_some_and(|drop| drop.token.to_string() == value && !drop.busy)
            {
                self.navigation.drop_confirmation = None;
                self.workbench.overlay = None;
            }
            return true;
        }
        if let Some(value) = name.strip_prefix("resource-drop-confirm:") {
            let Some(drop) = self
                .navigation_drop_confirmation()
                .filter(|drop| drop.token.to_string() == value && !drop.busy)
                .cloned()
            else {
                return true;
            };
            let error = if !self.navigation_drop_source_valid(&drop) {
                Some("连接配置已更改，请先重连")
            } else if !self.navigation_drop_context_valid(&drop) {
                Some("删除目标或连接已变化，请重新选择")
            } else if self.navigation_profile_has_pending_work(drop.profile_id) {
                Some("该连接存在草稿或任务，请先处理")
            } else {
                None
            };
            if let Some(error) = error {
                self.navigation.drop_confirmation.as_mut().unwrap().error = Some(error.into());
                self.status = UiMessage::key(error);
                return true;
            }
            let sql = if drop.kind == "database" {
                format!("DROP DATABASE {}", grid::mysql_identifier(&drop.database))
            } else {
                format!(
                    "DROP TABLE {}.{}",
                    grid::mysql_identifier(&drop.database),
                    grid::mysql_identifier(&drop.name)
                )
            };
            self.navigation.drop_confirmation.as_mut().unwrap().busy = true;
            self.status = UiMessage::key("正在删除资源…");
            self.navigation.drop_operation = Some(DropOperation {
                confirmation: drop.clone(),
                task: None,
                polling: false,
                last_poll: Instant::now() - Duration::from_secs(1),
                cancel_requested: false,
                cancel_pending: false,
                ddl_session: None,
                sql,
                terminal: None,
                closing: false,
            });
            self.navigation_drop_origin_pending(drop.tab, true);
            let secret =
                if self.form_id == Some(drop.profile_id) && !self.field("password").is_empty() {
                    Some(self.field("password").to_owned())
                } else {
                    None
                };
            self.navigation_request(
                backend,
                Pending::DropOpen { token: drop.token },
                "connection.open",
                json!({"profile_id":drop.profile_id,"secret":secret}),
            );
            return true;
        }
        let drop_target = if let Some(database) = name.strip_prefix("navigation-drop-db:") {
            Some(("database", database, database.to_owned()))
        } else if let Some(rest) = name.strip_prefix("navigation-drop-table:") {
            rest.rsplit_once(':').and_then(|(database, index)| {
                let resource = self
                    .profile
                    .as_ref()
                    .and_then(|profile| self.navigation.databases.get(&profile.id))
                    .and_then(|tree| tree.nodes.get(database))
                    .and_then(|node| {
                        index
                            .parse::<usize>()
                            .ok()
                            .and_then(|index| node.resources.get(index))
                    })?;
                (resource.kind == "table").then(|| ("table", database, resource.name.clone()))
            })
        } else {
            None
        };
        if let Some((kind, database, target_name)) = drop_target {
            let (Some(profile), Some(session)) = (self.profile.as_ref(), self.session_id()) else {
                self.status = UiMessage::key("请先连接数据库");
                return true;
            };
            let Some(saved) = self.profiles.iter().find(|saved| saved.id == profile.id) else {
                self.status = UiMessage::key("连接配置不存在或已更改");
                return true;
            };
            let saved_identity = serde_json::to_value(saved).unwrap_or(Value::Null);
            let current_identity = serde_json::to_value(profile).unwrap_or(Value::Null);
            let source_profile = self
                .session
                .as_ref()
                .and_then(|session| session.get("profile"))
                .cloned()
                .unwrap_or(Value::Null);
            if !Self::navigation_same_connection(&source_profile, &saved_identity)
                || !Self::navigation_same_connection(&current_identity, &saved_identity)
            {
                self.status = UiMessage::key("连接配置已更改，请先重连");
                return true;
            }
            if profile.provider != datapier_core::model::ProviderKind::Mysql
                || saved.provider != datapier_core::model::ProviderKind::Mysql
                || profile.is_archery()
                || saved.is_archery()
                || profile.ensure_write().is_err()
                || saved.ensure_write().is_err()
            {
                self.status = UiMessage::key("仅可在可写 MySQL 连接中删除资源");
                return true;
            }
            if self.navigation.drop_operation.is_some()
                || self.navigation_profile_has_pending_work(profile.id)
            {
                self.status = UiMessage::key("该连接存在草稿或任务，请先处理");
                return true;
            }
            let drop = ResourceDropConfirmation {
                token: Uuid::new_v4(),
                name: target_name,
                database: database.into(),
                kind,
                busy: false,
                task_started: false,
                cancel_requested: false,
                error: None,
                tab: self.tabs[self.active].id,
                generation: self.generation,
                profile_id: profile.id,
                profile: saved_identity,
                session_profile: current_identity,
                source_profile,
                session,
            };
            if !self.navigation_drop_context_valid(&drop) {
                self.status = UiMessage::key("删除目标或连接已变化，请重新选择");
                return true;
            }
            self.navigation.drop_confirmation = Some(drop);
            self.workbench.overlay = Some("resource-drop".into());
            return true;
        }
        if name.starts_with("navigation-drop-db:") || name.starts_with("navigation-drop-table:") {
            return true;
        }
        false
    }
}
