use super::*;

impl App {
    pub(crate) fn navigation_delete_confirmation(&self) -> Option<&ProfileDeleteConfirmation> {
        self.navigation
            .delete_confirmation
            .as_ref()
            .filter(|confirmation| {
                self.tabs
                    .get(self.active)
                    .is_some_and(|tab| tab.id == confirmation.tab)
                    && self.workbench.overlay.as_deref() == Some("profile-delete")
            })
    }

    pub(crate) fn navigation_profile_delete_pending(&self, id: Uuid) -> bool {
        self.navigation.delete_operation.as_ref().is_some_and(|operation| operation.id == id)
            || self.navigation.pending.values().any(|pending| matches!(pending, Pending::DeleteProfile { id: pending_id, .. } if *pending_id == id))
    }

    pub(super) fn navigation_profile_delete_unlock(&mut self, id: Uuid) {
        for index in 0..self.tabs.len() {
            if self.tabs[index].profile_id == Some(id) {
                if index == self.active {
                    self.operation_pending = false;
                } else {
                    self.tabs[index].runtime.operation_pending = false;
                }
            }
        }
    }

    pub(super) fn navigation_profile_delete_next(&mut self, backend: &Backend) {
        let Some(operation) = self.navigation.delete_operation.as_mut() else {
            return;
        };
        let id = operation.id;
        let token = operation.token;
        if let Some((tab, session)) = operation.sessions.pop() {
            self.navigation_request(
                backend,
                Pending::CloseProfileSession {
                    token,
                    tab,
                    session: session.clone(),
                },
                "connection.close",
                json!({"session_id":session}),
            );
        } else {
            self.navigation_request(
                backend,
                Pending::DeleteProfile { id, token },
                "profile.delete",
                json!({"profile_id":id}),
            );
        }
    }

    pub(super) fn navigation_profile_session_closed(&mut self, tab: Uuid, session: &Value) {
        let Some(index) = self.tabs.iter().position(|item| item.id == tab) else {
            return;
        };
        let old = self.active;
        self.switch_tab(index);
        if self.session_id().as_ref() == Some(session) {
            self.session = None;
            self.generation = self.generation.wrapping_add(1);
            self.resources.clear();
            self.workbench.redis_tree.reset(&[]);
            self.workbench.redis_tree_owner = None;
            self.cursor = None;
            self.scan_token = None;
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
                self.browse_result = false;
                self.clear_cli_session();
            }
            self.workbench.queued_target = None;
        }
        self.switch_tab(old);
    }
    pub(crate) fn navigation_profile_has_pending_work(&self, id: Uuid) -> bool {
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
            if index == self.active {
                self.has_live_drafts()
                    || self.workbench.inline.is_some()
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
                    || runtime.task.is_some()
                    || runtime.operation_pending
                    || runtime.workbench.cli_running
                    || runtime.workbench.connecting
            }
        })
    }

    pub(super) fn navigation_profile_action(&mut self, name: &str, backend: &Backend) -> bool {
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
            self.navigation.delete_confirmation = Some(ProfileDeleteConfirmation {
                token: Uuid::new_v4(),
                id,
                name: profile.name.clone(),
                busy: false,
                error: None,
                tab: self.tabs[self.active].id,
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
            let mut sessions = Vec::new();
            for index in 0..self.tabs.len() {
                if self.tabs[index].profile_id != Some(confirmation.id) {
                    continue;
                }
                let session = if index == self.active {
                    self.session.as_ref()
                } else {
                    self.tabs[index].runtime.session.as_ref()
                };
                if let Some(session) = session.and_then(|session| session.get("session_id")) {
                    sessions.push((self.tabs[index].id, session.clone()));
                }
                if index == self.active {
                    self.operation_pending = true;
                } else {
                    self.tabs[index].runtime.operation_pending = true;
                }
            }
            self.navigation.delete_operation = Some(ProfileDeleteOperation {
                id: confirmation.id,
                token: confirmation.token,
                sessions,
            });
            self.navigation_profile_delete_next(backend);
            return true;
        }
        false
    }
}
