use super::*;

impl App {
    pub(crate) fn navigation_delete_confirmation(&self) -> Option<&ProfileDeleteConfirmation> {
        self.navigation
            .delete_confirmation
            .as_ref()
            .filter(|confirmation| {
                self.tabs.get(self.active).map(|tab| tab.id) == confirmation.tab
                    && self.workbench.overlay.as_deref() == Some("profile-delete")
            })
    }

    pub(crate) fn navigation_profile_delete_pending(&self, id: Uuid) -> bool {
        self.navigation.delete_operation.as_ref().is_some_and(|operation| operation.id == id)
            || self.navigation.pending.values().any(|pending| matches!(pending, Pending::DeleteProfile { id: pending_id, .. } if *pending_id == id))
    }

    pub(crate) fn navigation_profile_delete_abort(&mut self, token: Uuid, error: String) {
        if self
            .navigation
            .delete_operation
            .as_ref()
            .is_none_or(|operation| operation.token != token)
        {
            return;
        }
        self.navigation.delete_operation = None;
        if let Some(confirmation) = self
            .navigation
            .delete_confirmation
            .as_mut()
            .filter(|confirmation| confirmation.token == token)
        {
            confirmation.busy = false;
            confirmation.error = Some(error.clone());
            confirmation.tab = self.tabs.get(self.active).map(|tab| tab.id);
        }
        self.status = UiMessage::raw(error);
    }

    pub(crate) fn navigation_profile_delete_after_tabs(&mut self, token: Uuid, backend: &Backend) {
        let Some(operation) = self
            .navigation
            .delete_operation
            .as_ref()
            .filter(|operation| operation.token == token)
        else {
            return;
        };
        let id = operation.id;
        let valid = self
            .navigation
            .delete_confirmation
            .as_ref()
            .filter(|confirmation| confirmation.token == token)
            .and_then(|confirmation| {
                self.profiles
                    .iter()
                    .find(|profile| profile.id == id)
                    .and_then(|profile| serde_json::to_value(profile).ok())
                    .map(|profile| profile == confirmation.profile)
            })
            .unwrap_or(false);
        if !valid {
            self.navigation_profile_delete_abort(token, "连接配置不存在或已更改".into());
            return;
        }
        if let Some(confirmation) = self.navigation.delete_confirmation.as_mut() {
            confirmation.tab = self.tabs.get(self.active).map(|tab| tab.id);
        }
        self.navigation_invalidate_profile(id);
        self.navigation_request(
            backend,
            Pending::DeleteProfile { id, token },
            "profile.delete",
            json!({"profile_id":id}),
        );
    }

    pub(crate) fn navigation_profile_has_pending_work(&self, id: Uuid) -> bool {
        if self
            .navigation
            .create_database
            .as_ref()
            .is_some_and(|form| form.profile_id == id)
        {
            return true;
        }
        if self
            .navigation
            .drop_operation
            .as_ref()
            .is_some_and(|operation| operation.confirmation.profile_id == id)
        {
            return true;
        }
        if self.profile_saves.contains(&id) {
            return true;
        }
        self.tabs.iter().enumerate().any(|(index, tab)| {
            if tab.profile_id != Some(id) {
                return false;
            }
            if !tab.pending.is_null() {
                return true;
            }
            if self.tab_has_advanced_work(index) {
                return true;
            }
            if index == self.active {
                self.has_live_drafts()
                    || self.workbench.inline.is_some()
                    || self.workbench.draft_scope_pending.is_some()
                    || !self.workbench.draft_scope_recovery.is_empty()
                    || self.task.is_some()
                    || self.operation_pending
                    || self.workbench.cli_running
                    || self.workbench.connecting
            } else {
                let runtime = &tab.runtime;
                runtime
                    .grid
                    .as_ref()
                    .is_some_and(|grid| !grid.edits().is_empty())
                    || !runtime.pending_rows.is_empty()
                    || !runtime.redis_changes.is_empty()
                    || runtime.paste_preview.is_some()
                    || self.redis_key_delete_tab_pending(tab.id)
                    || runtime.workbench.inline.is_some()
                    || runtime.workbench.draft_scope_pending.is_some()
                    || !runtime.workbench.draft_scope_recovery.is_empty()
                    || runtime.task.is_some()
                    || runtime.operation_pending
                    || runtime.workbench.cli_running
                    || runtime.workbench.connecting
            }
        })
    }

    pub(super) fn navigation_profile_action(&mut self, name: &str, backend: &Backend) -> bool {
        if let Some(value) = name.strip_prefix("profile-close:") {
            let Ok(id) = Uuid::parse_str(value) else {
                return true;
            };
            if self.navigation.delete_operation.is_some()
                || self.navigation_profile_has_pending_work(id)
            {
                self.status = UiMessage::key("该连接存在草稿或任务，请先处理");
                return true;
            }
            self.close_profile_tabs_then(&[id], &format!("profile-close-finished:{id}"), backend);
            return true;
        }
        if let Some(value) = name.strip_prefix("profile-delete-request:") {
            let Some(id) = Uuid::parse_str(value).ok() else {
                return true;
            };
            let Some(profile) = self.profiles.iter().find(|profile| profile.id == id) else {
                self.status = UiMessage::key("连接配置不存在或已更改");
                return true;
            };
            if self.navigation.delete_operation.is_some()
                || self.navigation_profile_delete_pending(id)
            {
                self.status = UiMessage::key("正在删除连接，请等待");
                return true;
            }
            if self.navigation_profile_has_pending_work(id) {
                self.status = UiMessage::key("该连接存在草稿或任务，请先处理");
                return true;
            }
            self.navigation.delete_confirmation = Some(ProfileDeleteConfirmation {
                token: Uuid::new_v4(),
                id,
                name: profile.name.clone(),
                busy: false,
                error: None,
                tab: self.tabs.get(self.active).map(|tab| tab.id),
                profile: serde_json::to_value(profile).unwrap_or(Value::Null),
            });
            self.workbench.overlay = Some("profile-delete".into());
            return true;
        }
        if let Some(value) = name.strip_prefix("profile-delete-cancel:") {
            if self
                .navigation_delete_confirmation()
                .is_some_and(|confirmation| {
                    confirmation.token.to_string() == value && !confirmation.busy
                })
            {
                self.navigation.delete_confirmation = None;
                self.workbench.overlay = None;
            }
            return true;
        }
        if let Some(value) = name.strip_prefix(concat!("profile-delete-", "confirm:")) {
            let Some(confirmation) = self
                .navigation_delete_confirmation()
                .filter(|confirmation| {
                    confirmation.token.to_string() == value && !confirmation.busy
                })
                .cloned()
            else {
                return true;
            };
            let current = self
                .profiles
                .iter()
                .find(|profile| profile.id == confirmation.id)
                .and_then(|profile| serde_json::to_value(profile).ok());
            let error = if current.as_ref() != Some(&confirmation.profile) {
                Some("连接配置不存在或已更改")
            } else if self.navigation_profile_has_pending_work(confirmation.id) {
                Some("该连接存在草稿或任务，请先处理")
            } else {
                None
            };
            if let Some(error) = error {
                self.navigation.delete_confirmation.as_mut().unwrap().error = Some(error.into());
                self.status = UiMessage::key(error);
                return true;
            }
            let confirmation_state = self.navigation.delete_confirmation.as_mut().unwrap();
            confirmation_state.busy = true;
            confirmation_state.error = None;
            self.navigation.delete_operation = Some(ProfileDeleteOperation {
                id: confirmation.id,
                token: confirmation.token,
            });
            if !self.close_profile_tabs_then(
                &[confirmation.id],
                &format!("profile-delete-tabs-closed:{}", confirmation.token),
                backend,
            ) {
                self.navigation_profile_delete_abort(
                    confirmation.token,
                    "该连接存在草稿或任务，请先处理".into(),
                );
            }
            return true;
        }
        false
    }
}
