use super::*;

impl App {
    pub(crate) fn archery_action(&mut self, action: &str, backend: &Backend) -> bool {
        if action == "archery-open" {
            self.archery_release(backend);
            let servers = std::mem::take(&mut self.archery.servers);
            let direct_draft = self.archery.direct_draft.take();
            self.archery = Wizard {
                open: true,
                epoch: Uuid::new_v4(),
                save_secret: true,
                servers,
                direct_draft,
                ..Wizard::default()
            };
            for name in [
                "archery_name",
                "archery_url",
                "archery_user",
                "archery_ca",
                "archery_otp",
                "archery_totp_uri",
            ] {
                self.field_set(name, "");
            }
            self.archery_send(backend, "archery.server.list", json!({}), false);
            return true;
        }
        if !self.archery.open {
            return false;
        }
        if action == "archery-discard-cancel"
            || (action == "overlay-close" && self.archery.discard_action.is_some())
        {
            self.archery.discard_action = None;
            return true;
        }
        if action == "archery-discard-confirm" {
            if let Some(next) = self.archery.discard_action.take() {
                self.archery.discard_accepted = true;
                if next.starts_with("field:provider:") {
                    self.action(&next, backend);
                    return true;
                }
                let next = if next == "overlay-close" && self.archery.auth_dialog {
                    "archery-auth-back"
                } else {
                    &next
                };
                return self.archery_action(next, backend);
            }
            return true;
        }
        if self.archery.discard_action.is_some() {
            return true;
        }
        let accepted = std::mem::take(&mut self.archery.discard_accepted);
        if action.starts_with("field:provider:") {
            if self.connection_form_busy() {
                return true;
            }
            if !accepted && self.archery_draft_dirty() {
                self.archery.discard_action = Some(action.to_owned());
                return true;
            }
            self.archery_release(backend);
            self.archery.open = false;
            return false;
        }
        let switching = action
            .strip_prefix("archery-server:")
            .and_then(|index| index.parse::<usize>().ok())
            .and_then(|index| self.archery.servers.get(index))
            .is_some_and(|server| {
                self.archery
                    .server
                    .as_ref()
                    .is_some_and(|old| old["id"] != server["id"])
            });
        let leaving = (matches!(action, "overlay-close" | "archery-direct")
            && !self.archery.auth_dialog)
            || (matches!(action, "overlay-close" | "archery-auth-back")
                && self.archery.auth_dialog
                && self.archery.reauth_server.is_some());
        if !accepted
            && (leaving || switching)
            && !self.connection_form_busy()
            && self.archery_draft_dirty()
        {
            self.archery.discard_action = Some(action.to_owned());
            return true;
        }
        if action == "overlay-close" && self.archery.auth_dialog {
            return self.archery_action("archery-auth-back", backend);
        }
        if action == "overlay-close" || action == "archery-direct" {
            if self.archery.pending.as_ref().is_some_and(|(_, method)| {
                matches!(
                    method.as_str(),
                    "archery.server.save"
                        | "archery.profile.apply_batch"
                        | "archery.server.delete"
                        | "archery.auth.close"
                )
            }) {
                return true;
            }
            if action == "overlay-close" && self.archery.parent_server_form.is_some() {
                self.archery_server_form_return(None, false, backend);
                return true;
            }
            self.archery_release(backend);
            self.archery.open = false;
            if action == "overlay-close" {
                self.workbench.overlay = self.archery.return_overlay.take();
            }
            return true;
        }
        if action == "archery-cancel-auth" {
            self.archery.open_continuation = None;
            if self.archery.pending.as_ref().is_some_and(|(_, method)| {
                matches!(
                    method.as_str(),
                    "archery.auth.begin"
                        | "archery.auth.verify"
                        | "archery.instances"
                        | "archery.databases"
                )
            }) {
                self.archery_release(backend);
                self.archery.state.clear();
                self.archery.controller_token = None;
                self.archery.auth_generation = 0;
                self.archery.resume_instance = None;
                self.archery.save_after_auth = false;
                self.archery.error = Some(i18n::tr("已停止等待").into_owned());
            }
            return true;
        }
        if !action.starts_with("archery-") {
            return false;
        }
        if self.archery.pending.is_some() {
            if (action == "archery-new"
                || action == "archery-auth-back"
                || action.starts_with("archery-server:"))
                && self.archery.pending.as_ref().is_some_and(|(_, method)| {
                    matches!(
                        method.as_str(),
                        "archery.auth.begin"
                            | "archery.auth.verify"
                            | "archery.instances"
                            | "archery.databases"
                    )
                })
            {
                let password = self.field("password").to_owned();
                self.archery_release(backend);
                if action == "archery-auth-back" {
                    self.field_set("password", password);
                }
            } else {
                return true;
            }
        }
        if self.archery.recovery.is_some() && action != "archery-reconcile" {
            return true;
        }
        self.archery.error = None;
        match action {
            "archery-reconcile" => match self.archery.recovery {
                Some("server" | "delete") => {
                    self.archery_send(backend, "archery.server.list", json!({}), false)
                }
                Some("batch") => self.archery_send(backend, "profile.list", json!({}), false),
                _ => {}
            },
            _ if action.starts_with("archery-remove:") => {
                if let Ok(id) = action[15..].parse::<i64>() {
                    self.archery.selected.remove(&id);
                }
            }
            "archery-new" => self.archery_server_form_open(true),
            "archery-manage" => {
                if self.archery.saved {
                    self.archery_server_form_open(false);
                }
            }
            "archery-totp-details" => self.archery.totp_details = !self.archery.totp_details,
            "archery-clear-ca" => self.field_set("archery_ca", ""),
            "archery-server-details" => {
                if self
                    .field("archery_url")
                    .trim()
                    .get(..8)
                    .is_some_and(|prefix| prefix.eq_ignore_ascii_case("https://"))
                {
                    self.archery.server_details = !self.archery.server_details;
                }
            }
            "archery-relogin" => {
                self.archery_release(backend);
                self.archery.state.clear();
                self.archery.auth_dialog = true;
                self.workbench.overlay = Some("archery-auth".into());
                self.workbench.focus_request = Some("input-password".into());
            }
            "archery-confirm-delete" => {
                self.archery.delete_confirm = true;
            }
            "archery-toggle-secret" => {
                self.archery.save_secret = !self.archery.save_secret;
            }
            "archery-toggle-totp" => self.archery.save_totp = !self.archery.save_totp,
            "archery-toggle-totp-mode" => {
                self.archery.totp_automatic = !self.archery.totp_automatic
            }
            "archery-clear-password" => self.archery.clear_password = !self.archery.clear_password,
            "archery-clear-totp" => self.archery.clear_totp = !self.archery.clear_totp,
            "archery-auth-open" => {
                if self.archery_validate_server() {
                    self.archery.auth_dialog = true;
                    self.workbench.overlay = Some("archery-auth".into());
                    self.workbench.focus_request = Some("input-password".into());
                }
            }
            "archery-auth-back" => {
                self.archery.open_continuation = None;
                let password = self.field("password").to_owned();
                let otp = self.field("archery_otp").to_owned();
                self.archery.save_after_auth = false;
                self.archery_release(backend);
                self.field_set("password", password);
                self.field_set("archery_otp", otp);
                self.archery.auth_dialog = false;
                self.archery.state.clear();
                if self.archery.reauth_server.take().is_some() {
                    self.field_set("password", "");
                    self.field_set("archery_otp", "");
                    self.archery.open = false;
                    self.workbench.overlay = self.archery.return_overlay.take();
                } else {
                    self.workbench.overlay = Some("profile".into());
                }
            }
            "archery-login" => {
                if self.field("password").is_empty()
                    && (!self.archery.saved
                        || matches!(self.archery.state.as_str(), "auth_required" | "auth_failed")
                        || (self.archery.server_only && self.archery_password_policy_changed()))
                {
                    self.archery.auth_dialog = true;
                    self.workbench.overlay = Some("archery-auth".into());
                    self.field_error("password", i18n::UiMessage::key("请输入密码"));
                    return true;
                }
                let secret =
                    (!self.field("password").is_empty()).then(|| self.field("password").to_owned());
                let mut server = if self.archery.saved {
                    self.archery.server.clone().unwrap_or(Value::Null)
                } else {
                    json!({"id":self.archery.server.as_ref().and_then(|s| s["id"].as_str()).map(str::to_owned).unwrap_or_else(|| Uuid::new_v4().to_string()),"name":self.field("archery_name"),"base_url":self.field("archery_url"),"username":self.field("archery_user"),"http_tls":{"ca_path":(!self.field("archery_ca").is_empty()).then(||self.field("archery_ca"))},"revision":0,"totp_mode":"manual"})
                };
                server["name"] = json!(self.field("archery_name"));
                server["http_tls"]["ca_path"] =
                    json!((!self.field("archery_ca").is_empty()).then(|| self.field("archery_ca")));
                server["totp_mode"] = json!(if self.archery.totp_automatic {
                    "automatic"
                } else {
                    "manual"
                });
                if server["name"].as_str().unwrap_or("").trim().is_empty()
                    || server["base_url"].as_str().unwrap_or("").trim().is_empty()
                    || server["username"].as_str().unwrap_or("").trim().is_empty()
                {
                    self.archery.error =
                        Some(i18n::tr("请填写服务名称、地址和用户名").into_owned());
                    return true;
                }
                let candidate = self.archery.reauth_server.is_none()
                    && (!self.archery.saved
                        || ((self.archery.manage || self.archery.server_only)
                            && (secret.is_some()
                                || !self.field("archery_totp_uri").is_empty()
                                || self.archery.clear_password
                                || self.archery.clear_totp
                                || self
                                    .archery
                                    .servers
                                    .iter()
                                    .find(|old| old["id"] == server["id"])
                                    .is_none_or(|old| {
                                        old["http_tls"] != server["http_tls"]
                                            || old["totp_mode"] != server["totp_mode"]
                                    }))));
                self.archery.auth_candidate = candidate;
                self.archery.server = Some(server.clone());
                let params = json!({"expected_revision":server["revision"],"server":server,"password":secret,"totp_uri":(!self.field("archery_totp_uri").is_empty()).then(||self.field("archery_totp_uri")),"candidate":candidate});
                self.archery_send(backend, "archery.auth.begin", params, true);
            }
            "archery-verify" => {
                if self.field("archery_otp").trim().is_empty() {
                    self.field_error("archery_otp", i18n::UiMessage::key("请输入验证码"));
                    return true;
                }
                if let Some(id) = self.archery.auth_id.clone() {
                    let otp = self.field("archery_otp").to_owned();
                    self.field_set("archery_otp", "");
                    self.archery_send(
                        backend,
                        "archery.auth.verify",
                        json!({"auth_id":id,"otp":otp,"controller_token":self.archery.controller_token,"auth_generation":self.archery.auth_generation}),
                        true,
                    );
                }
            }
            "archery-instances" => {
                if self.archery.server.is_none() {
                    self.archery.error = Some(i18n::tr("请选择服务器").into_owned());
                    return true;
                }
                if self.archery.state == "ready" {
                    if let Some(id) = self.archery.auth_id.clone() {
                        self.archery_send(
                            backend,
                            "archery.instances",
                            json!({"auth_id":id}),
                            true,
                        );
                    }
                } else {
                    self.archery.resume_instance = None;
                    self.archery_action("archery-login", backend);
                }
            }
            "archery-preview" => {
                let ids: Vec<_> = self
                    .archery
                    .instances
                    .iter()
                    .filter_map(|instance| instance["id"].as_i64())
                    .chain(self.archery.selected.keys().copied().filter(|id| {
                        !self
                            .archery
                            .instances
                            .iter()
                            .any(|instance| instance["id"].as_i64() == Some(*id))
                    }))
                    .collect();
                for id in ids {
                    let Some(selection) = self.archery.selected.get(&id) else {
                        continue;
                    };
                    let field = format!("archery_profile_name_{}", selection.profile_id);
                    if self.field(&field).trim().is_empty() {
                        self.archery.expanded.insert(id);
                        self.field_error(&field, i18n::UiMessage::key("请填写连接名称"));
                        return true;
                    }
                    if selection.databases.is_none()
                        || selection.selected.is_empty()
                        || selection
                            .default
                            .as_ref()
                            .is_none_or(|d| !selection.selected.contains(d))
                    {
                        self.archery.expanded.insert(id);
                        self.archery.selected.get_mut(&id).unwrap().error = Some(
                            i18n::tr("每个所选实例必须成功加载并选择数据库及默认目标").into_owned(),
                        );
                        self.workbench.focus_request = Some(format!("archery-database-group-{id}"));
                        return true;
                    }
                }
                if let Some(server) = self.archery.server.as_ref() {
                    let desired = self.archery_desired_profiles(server);
                    if desired.len() == self.archery.baseline.len()
                        && self
                            .archery
                            .baseline
                            .iter()
                            .all(|old| desired.contains(&json!(old)))
                    {
                        self.archery.error = Some(i18n::tr("没有连接变更").into_owned());
                    } else {
                        self.archery.preview = true;
                    }
                } else {
                    self.archery.error = Some(i18n::tr("请先登录服务").into_owned());
                }
            }
            "archery-back" => self.archery.preview = false,
            "archery-save" => {
                if !self.archery.saved {
                    self.archery_save_server(backend);
                } else {
                    let Some(server) = self.archery.server.as_ref() else {
                        return true;
                    };
                    let desired = self.archery_desired_profiles(server);
                    let affected: Vec<_> = self
                        .archery
                        .baseline
                        .iter()
                        .filter(|old| {
                            !desired
                                .iter()
                                .any(|new| new["id"] == json!(old.id) && *new == json!(old))
                        })
                        .map(|old| old.id)
                        .collect();
                    let unchanged =
                        affected.is_empty() && desired.len() == self.archery.baseline.len();
                    if unchanged {
                        self.archery_release(backend);
                        self.archery.open = false;
                        self.workbench.overlay = None;
                    } else {
                        self.close_profile_tabs_then(&affected, "archery-apply-closed", backend);
                    }
                }
            }
            "archery-save-server" => {
                if !self.archery_validate_server() {
                    return true;
                }
                if self.archery_password_policy_changed() && self.field("password").is_empty() {
                    self.archery.save_after_auth = true;
                    self.archery.auth_dialog = true;
                    self.workbench.overlay = Some("archery-auth".into());
                    self.field_error("password", i18n::UiMessage::key("请输入密码"));
                    return true;
                }
                let original = self.archery.server.as_ref().and_then(|server| {
                    self.archery
                        .servers
                        .iter()
                        .find(|item| item["id"] == server["id"])
                });
                let auth_changed = !self.archery.saved
                    || self.archery.clear_password
                    || self.archery.clear_totp
                    || !self.field("password").is_empty()
                    || !self.field("archery_totp_uri").is_empty()
                    || original.is_some_and(|old| {
                        old["http_tls"]["ca_path"].as_str().unwrap_or("")
                            != self.field("archery_ca")
                            || (old["totp_mode"] == "automatic") != self.archery.totp_automatic
                    });
                if auth_changed && (!self.archery.auth_candidate || self.archery.state != "ready") {
                    self.archery.save_after_auth = true;
                    self.archery.auth_dialog = true;
                    self.workbench.overlay = Some("archery-auth".into());
                    self.workbench.focus_request = Some("input-password".into());
                } else {
                    self.archery_save_server(backend);
                }
            }
            "archery-apply-closed" => self.archery_save_batch(backend),
            "archery-delete-server" => {
                if self.archery.delete_confirm {
                    let ids: Vec<_> = self.archery.baseline.iter().map(|p| p.id).collect();
                    self.close_profile_tabs_then(&ids, "archery-delete-closed", backend);
                }
            }
            "archery-delete-closed" => {
                if let Some(server) = self.archery.server.clone() {
                    if let Some(auth_id) = self.archery.auth_id.clone() {
                        self.archery.deferred_delete = true;
                        self.archery_send(backend, "archery.auth.close", json!({"auth_id":auth_id,"controller_token":self.archery.controller_token,"auth_generation":self.archery.auth_generation}), false);
                    } else {
                        let ids: Vec<_> = self.archery.baseline.iter().map(|p| p.id).collect();
                        self.archery_send(backend, "archery.server.delete", json!({"server_id":server["id"],"expected_revision":server["revision"],"expected_profile_ids":ids}), false);
                    }
                }
            }
            _ if action.starts_with("archery-server:") => {
                if let Ok(index) = action[15..].parse::<usize>()
                    && let Some(server) = self.archery.servers.get(index).cloned()
                {
                    if self
                        .archery
                        .server
                        .as_ref()
                        .is_some_and(|current| current["id"] == server["id"])
                    {
                        return true;
                    }
                    if self.form_editing
                        && !self.archery.server_only
                        && self
                            .archery
                            .server
                            .as_ref()
                            .is_some_and(|current| current["id"] != server["id"])
                    {
                        return true;
                    }
                    self.archery_release(backend);
                    self.field_set("archery_totp_uri", "");
                    self.archery.expanded.clear();
                    self.archery.clear_password = false;
                    self.archery.clear_totp = false;
                    self.archery.auth_candidate = false;
                    self.archery.epoch = Uuid::new_v4();
                    self.archery.server = Some(server.clone());
                    self.archery.saved = true;
                    self.archery.totp_automatic = server["totp_mode"] == "automatic";
                    self.archery.save_secret = server["credential_mode"] == "keyring";
                    self.archery.save_totp = server["totp_credential_mode"] == "keyring";
                    self.archery.state.clear();
                    self.archery.instances.clear();
                    self.archery.available_by_instance.clear();
                    if let Some(id) = server["id"]
                        .as_str()
                        .and_then(|id| Uuid::parse_str(id).ok())
                    {
                        self.archery_baseline(id);
                        self.archery.instances = self.archery.baseline.iter().filter_map(|profile| profile.archery().map(|connection| json!({"id":connection.instance_id,"name":connection.instance_name,"provider":profile.provider}))).collect();
                    }
                    if let Some(instance) = self.archery.focus_instance.take() {
                        self.archery.expanded.insert(instance);
                        if let Some(selection) = self.archery.selected.get(&instance) {
                            self.workbench.focus_request = Some(format!(
                                "input-archery_profile_name_{}",
                                selection.profile_id
                            ));
                        }
                    }
                    self.archery.preview = false;
                    self.field_set("archery_name", server["name"].as_str().unwrap_or(""));
                    self.field_set("archery_url", server["base_url"].as_str().unwrap_or(""));
                    self.field_set("archery_user", server["username"].as_str().unwrap_or(""));
                    self.field_set(
                        "archery_ca",
                        server["http_tls"]["ca_path"].as_str().unwrap_or(""),
                    );
                }
            }
            _ if action.starts_with("archery-instance:") => {
                if let Ok(index) = action[17..].parse::<usize>()
                    && let Some(instance) = self.archery.instances.get(index).cloned()
                    && let Some(id) = instance["id"].as_i64()
                    && self.archery.selected.remove(&id).is_none()
                {
                    let saved = self
                        .archery
                        .baseline
                        .iter()
                        .find(|p| p.archery().is_some_and(|a| a.instance_id == id))
                        .cloned();
                    let selection = if let Some(profile) = saved {
                        let connection = profile.archery().unwrap();
                        self.field_set(
                            &format!("archery_profile_name_{}", profile.id),
                            profile.name.clone(),
                        );
                        Selection {
                            profile_id: profile.id,
                            instance: json!({"id":connection.instance_id,"name":connection.instance_name,"provider":profile.provider}),
                            available_databases: self
                                .archery
                                .available_by_instance
                                .get(&id)
                                .cloned(),
                            databases: Some(connection.databases.clone()),
                            selected: connection.databases.iter().cloned().collect(),
                            default: Some(connection.default_database.clone()),
                            error: None,
                        }
                    } else {
                        let profile_id = Uuid::new_v4();
                        self.field_set(
                            &format!("archery_profile_name_{profile_id}"),
                            instance["name"].as_str().unwrap_or(""),
                        );
                        Selection {
                            profile_id,
                            instance,
                            available_databases: None,
                            databases: None,
                            selected: BTreeSet::new(),
                            default: None,
                            error: None,
                        }
                    };
                    self.archery.selected.insert(id, selection);
                }
            }
            _ if action.starts_with("archery-expand:") => {
                if let Ok(id) = action[15..].parse::<i64>()
                    && !self.archery.expanded.remove(&id)
                {
                    self.archery.expanded.insert(id);
                    if self
                        .archery
                        .selected
                        .get(&id)
                        .is_some_and(|selection| selection.databases.is_none())
                    {
                        self.archery_action(&format!("archery-load:{id}"), backend);
                    }
                }
            }
            _ if action.starts_with("archery-load:") => {
                if let Ok(id) = action[13..].parse::<i64>() {
                    if self.archery.state != "ready" {
                        self.archery.resume_instance = Some(id);
                        self.archery_action("archery-login", backend);
                    } else if let Some(auth_id) = self.archery.auth_id.clone() {
                        if let Some(sel) = self.archery.selected.get_mut(&id) {
                            sel.error = None;
                        }
                        self.archery.loading_instance = Some(id);
                        self.archery_send(
                            backend,
                            "archery.databases",
                            json!({"auth_id":auth_id,"instance_id":id}),
                            true,
                        );
                    }
                }
            }
            _ if action.starts_with("archery-db:") || action.starts_with("archery-default:") => {
                let default = action.starts_with("archery-default:");
                let rest = if default {
                    &action[16..]
                } else {
                    &action[11..]
                };
                if let Some((id, index)) = rest.split_once(':')
                    && let (Ok(id), Ok(index)) = (id.parse::<i64>(), index.parse::<usize>())
                    && self.archery.instances.iter().any(|instance| {
                        instance["id"].as_i64() == Some(id)
                            && instance["missing"].as_bool() != Some(true)
                    })
                    && let Some(sel) = self.archery.selected.get_mut(&id)
                    && let Some(db) = sel.databases.as_ref().and_then(|d| d.get(index)).cloned()
                    && sel
                        .available_databases
                        .as_ref()
                        .is_none_or(|available| available.contains(&db))
                {
                    sel.error = None;
                    if default {
                        if sel.selected.contains(&db) {
                            sel.default = Some(db);
                            self.focus = format!("archery-default-{id}-{index}");
                            self.workbench.focus_request = Some(String::new());
                        }
                    } else if !sel.selected.remove(&db) {
                        sel.selected.insert(db);
                    } else if sel.default.as_ref() == Some(&db) {
                        sel.default = None;
                    }
                }
            }
            _ => {}
        }
        true
    }
}
