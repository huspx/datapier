use super::*;

impl App {
    pub(crate) fn navigation_action(&mut self, name: &str, backend: &Backend) -> bool {
        self.navigation_group_action(name)
            || self.navigation_drop_action(name, backend)
            || self.navigation_profile_action(name, backend)
            || self.navigation_connection_action(name, backend)
            || self.navigation_tree_action(name, backend)
    }

    pub(super) fn navigation_connection_action(&mut self, name: &str, backend: &Backend) -> bool {
        if name == "navigation-refresh" {
            self.navigation_load_tree(backend, true);
            self.action("list", backend);
            return true;
        }
        if let Some(open) = name
            .strip_prefix("redis-db-menu-open:")
            .and_then(|value| value.parse::<bool>().ok())
        {
            self.navigation.redis_db_menu = open;
            return true;
        }
        if let Some(open) = name
            .strip_prefix("form-redis-db-menu-open:")
            .and_then(|value| value.parse::<bool>().ok())
        {
            self.navigation.form_redis_db_menu = open;
            return true;
        }
        if let Some(db) = name
            .strip_prefix("form-redis-db:")
            .and_then(|v| v.parse::<u32>().ok())
        {
            self.navigation.form_redis_db_menu = false;
            self.field_set("redis_db", db.to_string());
            return true;
        }
        if name == "profile-databases-load" {
            if !self.validate_profile_form() {
                return true;
            }
            match self.form_profile() {
                Ok(profile) => {
                    let profile = serde_json::to_value(profile).unwrap_or(Value::Null);
                    let secret = if self.field("password").is_empty() {
                        Value::Null
                    } else {
                        json!(self.field("password"))
                    };
                    self.navigation
                        .pending
                        .retain(|_, p| !matches!(p, Pending::Form { .. }));
                    self.navigation.form.loading = true;
                    self.navigation.form.error = None;
                    self.navigation_request(
                        backend,
                        Pending::Form {
                            profile: profile.clone(),
                            secret: secret.clone(),
                        },
                        "profile.databases",
                        json!({"profile":profile,"secret":secret}),
                    );
                }
                Err(error) => self.navigation.form.error = Some(error),
            }
            return true;
        }
        if let Some(index) = name
            .strip_prefix("profile-databases-toggle:")
            .and_then(|v| v.parse::<usize>().ok())
        {
            if let Some(item) = self.navigation.form.candidates.get(index).cloned() {
                let form = &mut self.navigation.form;
                form.all = false;
                if !form.selected.remove(&item) {
                    form.selected.insert(item);
                }
            }
            return true;
        }
        if matches!(
            name,
            "profile-databases-select-matching" | "profile-databases-clear-matching"
        ) {
            let filter = self.field("database_search").to_lowercase();
            let form = &mut self.navigation.form;
            form.all = false;
            for item in &form.candidates {
                if item.to_lowercase().contains(&filter) {
                    if name == "profile-databases-select-matching" {
                        form.selected.insert(item.clone());
                    } else {
                        form.selected.remove(item);
                    }
                }
            }
            return true;
        }
        if let Some(db) = name
            .strip_prefix("redis-db:")
            .and_then(|v| v.parse::<u32>().ok())
        {
            self.navigation.redis_db_menu = false;
            if !self.is_redis() || self.tabs[self.active].browse.redis_db == Some(db) {
                return true;
            }
            if self.has_live_drafts()
                || !self.tabs[self.active].pending.is_null()
                || self.workbench.inline.is_some()
                || self.workbench.overlay.is_some()
            {
                self.status = UiMessage::key("请先处理当前编辑和待提交变更");
                return true;
            }
            if self.task.is_some() || self.operation_pending || self.workbench.cli_running {
                self.status = UiMessage::key("请等待或取消当前任务");
                return true;
            }
            let Some(profile) = self.profile.as_ref() else {
                return true;
            };
            if profile.archery().is_some_and(|archery| {
                !archery
                    .databases
                    .iter()
                    .any(|database| database.parse::<u32>() == Ok(db))
            }) {
                return true;
            }
            let id = profile.id;
            self.close_result_info(false);
            self.operation_pending = true;
            self.navigation_request(
                backend,
                Pending::Redis {
                    tab: self.tabs[self.active].id,
                    generation: self.generation,
                    db,
                },
                "connection.open",
                json!({"profile_id":id,"redis_db":db}),
            );
            return true;
        }
        false
    }
}
