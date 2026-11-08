use super::*;

impl App {
    pub(crate) fn archery_response(&mut self, response: Response, backend: &Backend) {
        let parts: Vec<_> = response.tag.split('/').collect();
        if parts.len() != 4
            || Uuid::parse_str(parts[1]).ok() != Some(self.archery.epoch)
            || self
                .archery
                .pending
                .as_ref()
                .map(|p| p.0.to_string())
                .as_deref()
                != Some(parts[2])
        {
            return;
        }
        let method = parts[3];
        self.archery.pending = None;
        let value = match response.result {
            Ok(value) => value,
            Err(error) => {
                let message = error.to_string();
                if (method == "archery.auth.begin" && error.code == "auth_failed")
                    || (matches!(method, "archery.auth.begin" | "archery.auth.verify")
                        && error.code == "auth_required")
                {
                    self.archery.state = error.code.clone();
                    self.archery.auth_dialog = true;
                    self.workbench.overlay = Some("archery-auth".into());
                    self.workbench.focus_request = Some("input-password".into());
                }
                if error.code == "auth_required"
                    && matches!(method, "archery.instances" | "archery.databases")
                {
                    let instance = self.archery.loading_instance;
                    self.archery_release(backend);
                    self.archery.resume_instance = instance;
                    self.archery.state.clear();
                    self.archery.error = Some(message);
                    self.archery.auth_dialog = true;
                    self.workbench.overlay = Some("archery-auth".into());
                    self.workbench.focus_request = Some("input-password".into());
                    return;
                }
                if method == "archery.databases"
                    && let Some(id) = self.archery.loading_instance.take()
                    && let Some(sel) = self.archery.selected.get_mut(&id)
                {
                    sel.error = Some(message.clone());
                }
                self.archery.error = Some(message);
                if method == "archery.databases" {
                    self.archery_discover_next(backend);
                }
                if matches!(
                    error.code.as_str(),
                    "transport" | "disconnected" | "outcome_unknown" | "ipc_closed"
                ) {
                    match method {
                        "archery.server.save" => {
                            self.archery.recovery = Some("server");
                            self.archery_send(backend, "archery.server.list", json!({}), false);
                        }
                        "archery.profile.apply_batch" => {
                            self.archery.recovery = Some("batch");
                            self.archery_send(backend, "profile.list", json!({}), false);
                        }
                        "archery.server.delete" => {
                            self.archery.recovery = Some("delete");
                            self.archery_send(backend, "archery.server.list", json!({}), false);
                        }
                        _ => {}
                    }
                }
                return;
            }
        };
        match method {
            "archery.server.list" => {
                let Some(servers) = value["servers"].as_array() else {
                    self.archery.error = Some(i18n::tr("服务列表响应无效").into_owned());
                    return;
                };
                self.archery.servers = servers.clone();
                if self.archery.recovery == Some("delete") {
                    if self.archery.server.as_ref().is_some_and(|server| {
                        !self
                            .archery
                            .servers
                            .iter()
                            .any(|actual| actual["id"] == server["id"])
                    }) {
                        self.archery.recovery = None;
                        self.archery.server = None;
                        self.archery.saved = false;
                        self.request(backend, "profile.list", "profile.list", json!({}));
                    } else {
                        self.archery.error =
                            Some(i18n::tr("服务删除结果不确定，请核对服务").into_owned());
                    }
                    return;
                }
                if self.archery.recovery == Some("server") {
                    if let Some(expected) = self.archery.staged_server.clone() {
                        let found = self
                            .archery
                            .servers
                            .iter()
                            .find(|s| s["id"] == expected["id"]);
                        match found {
                            None if expected["revision"] == 0 => {
                                self.archery.error = Some(
                                    i18n::tr("服务保存结果不确定，请核对服务后重试").into_owned(),
                                );
                            }
                            Some(actual)
                                if actual["revision"].as_u64()
                                    == expected["revision"]
                                        .as_u64()
                                        .and_then(|r| r.checked_add(1))
                                    && [
                                        "name",
                                        "base_url",
                                        "username",
                                        "http_tls",
                                        "totp_mode",
                                    ]
                                    .iter()
                                    .all(|key| actual[*key] == expected[*key]) =>
                            {
                                self.archery.server = Some(actual.clone());
                                self.archery.saved = true;
                                if self.field("password").is_empty()
                                    && self.field("archery_totp_uri").is_empty()
                                    && !self.archery.clear_password
                                    && !self.archery.clear_totp
                                {
                                    self.archery.recovery = None;
                                }
                                self.archery.error =
                                    Some(i18n::tr("服务已保存；连接尚未保存").into_owned());
                            }
                            _ => {
                                self.archery.error = Some(
                                    i18n::tr("服务保存结果不确定，请核对服务后重试").into_owned(),
                                )
                            }
                        }
                    }
                    return;
                }
                if let Some(target) = self
                    .archery
                    .open_server
                    .take()
                    .or(self.archery.reauth_server)
                {
                    if let Some(index) = self.archery.servers.iter().position(|s| {
                        s["id"].as_str().and_then(|id| Uuid::parse_str(id).ok()) == Some(target)
                    }) {
                        self.archery_action(&format!("archery-server:{index}"), backend);
                    } else {
                        self.archery.error = Some(i18n::tr("找不到 Archery 服务").into_owned());
                    }
                }
            }
            "profile.list" if self.archery.recovery == Some("batch") => {
                let Some(profiles) = value["profiles"].as_array() else {
                    self.archery.error = Some(i18n::tr("连接列表响应无效").into_owned());
                    return;
                };
                let Some(server) = self.archery.server.as_ref() else {
                    return;
                };
                let expected = self.archery_desired_profiles(server);
                let actual: Vec<_> = profiles
                    .iter()
                    .filter(|p| p["connection"]["server_id"] == server["id"])
                    .cloned()
                    .collect();
                if actual.len() == expected.len()
                    && expected.iter().all(|profile| actual.contains(profile))
                {
                    self.archery.recovery = None;
                    self.archery_release(backend);
                    self.archery.open = false;
                    self.workbench.overlay = None;
                    self.request(backend, "profile.list", "profile.list", json!({}));
                    self.status = i18n::UiMessage::key("连接已保存");
                } else {
                    self.archery.error =
                        Some(i18n::tr("批量保存结果不一致，请核对连接").into_owned());
                }
            }
            "archery.auth.begin" | "archery.auth.verify" | "archery.auth.status" => {
                if value["auth_generation"]
                    .as_u64()
                    .is_some_and(|generation| generation < self.archery.auth_generation)
                {
                    return;
                }
                if let Some(id) = value["auth_id"].as_str() {
                    self.archery.auth_id = Some(id.to_owned());
                }
                self.archery.state = value["state"].as_str().unwrap_or_default().to_owned();
                self.archery.controller_token =
                    value["controller_token"].as_str().map(str::to_owned);
                self.archery.auth_generation =
                    value["auth_generation"].as_u64().unwrap_or_default();
                self.field_set("archery_otp", "");
                if self.archery.state == "challenge_required"
                    && self.archery.controller_token.is_some()
                {
                    self.workbench.focus_request = Some("input-archery_otp".into());
                    self.archery.auth_dialog = true;
                    self.workbench.overlay = Some("archery-auth".into());
                }
                if self.archery.state == "ready" {
                    if self.archery.reauth_server.take().is_some() {
                        let continuation = self.archery.open_continuation.take();
                        self.archery_release(backend);
                        self.archery.open = false;
                        self.workbench.overlay = self.archery.return_overlay.take();
                        if self.archery.connection_auth_error.take().is_some_and(
                            |(tab, generation, profile)| {
                                self.tabs
                                    .get(self.active)
                                    .is_some_and(|active| active.id == tab)
                                    && self.generation == generation
                                    && self
                                        .profile
                                        .as_ref()
                                        .is_some_and(|active| active.id == profile)
                            },
                        ) {
                            self.workbench.connection_error = None;
                            self.workbench.connection_tls_required = false;
                        }
                        if let Some(open) =
                            continuation.filter(|open| self.archery_open_context_matches(open))
                        {
                            self.workbench.connection_error = None;
                            self.workbench.connection_tls_required = false;
                            self.status = i18n::UiMessage::key("登录成功，正在打开连接");
                            // 只续接首次打开，不重放已失败的查询或 Redis 命令。
                            self.request(
                                backend,
                                "connection.open",
                                "connection.open",
                                open.params,
                            );
                            self.archery.open_capture = None;
                        } else {
                            self.status = i18n::UiMessage::key("登录成功，请手动重试");
                        }
                    } else {
                        self.archery.auth_dialog = false;
                        self.workbench.overlay = Some("profile".into());
                        if std::mem::take(&mut self.archery.save_after_auth) {
                            self.archery_save_server(backend);
                        } else if let Some(instance) = self.archery.resume_instance.take() {
                            self.archery_action(&format!("archery-load:{instance}"), backend);
                        } else if !self.archery.server_only {
                            self.archery_action("archery-instances", backend);
                        }
                    }
                }
            }
            "archery.instances" => {
                let mut instances = value["instances"].as_array().cloned().unwrap_or_default();
                for profile in &self.archery.baseline {
                    let connection = profile.archery().unwrap();
                    if !instances
                        .iter()
                        .any(|instance| instance["id"].as_i64() == Some(connection.instance_id))
                    {
                        instances.push(json!({"id":connection.instance_id,"name":connection.instance_name,"provider":profile.provider,"missing":true}));
                    }
                }
                self.archery.instances = instances;
                self.archery.discovery_queue = self
                    .archery
                    .expanded
                    .iter()
                    .filter(|id| {
                        self.archery
                            .selected
                            .get(id)
                            .is_some_and(|selection| selection.databases.is_none())
                    })
                    .copied()
                    .collect();
                self.archery_discover_next(backend);
            }
            "archery.databases" => {
                if !value["databases"]
                    .as_array()
                    .is_some_and(|items| items.iter().all(Value::is_string))
                {
                    let message = i18n::tr("数据库列表响应无效").into_owned();
                    if let Some(id) = self.archery.loading_instance.take()
                        && let Some(sel) = self.archery.selected.get_mut(&id)
                    {
                        sel.error = Some(message.clone());
                    }
                    self.archery.error = Some(message);
                    self.archery_discover_next(backend);
                    return;
                }
                if let Some(id) = self.archery.loading_instance.take()
                    && let Some(sel) = self.archery.selected.get_mut(&id)
                {
                    let mut databases: Vec<String> = value["databases"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect();
                    self.archery
                        .available_by_instance
                        .insert(id, databases.iter().cloned().collect());
                    sel.available_databases = Some(databases.iter().cloned().collect());
                    for selected in &sel.selected {
                        if !databases.contains(selected) {
                            databases.push(selected.clone());
                        }
                    }
                    sel.databases = Some(databases);
                    sel.error = None;
                }
                self.archery_discover_next(backend);
            }
            "archery.auth.close" => {
                self.archery.auth_id = None;
                if std::mem::take(&mut self.archery.deferred_delete)
                    && let Some(server) = self.archery.server.clone()
                {
                    self.archery_send(
                        backend,
                        "archery.server.delete",
                        json!({"server_id":server["id"],"expected_revision":server["revision"],"expected_profile_ids":self.archery.baseline.iter().map(|p| p.id).collect::<Vec<_>>()}),
                        false,
                    );
                }
            }
            "archery.server.save" => {
                let Some(server) = Some(value.clone()).filter(|server| {
                    self.archery.staged_server.as_ref().is_some_and(|expected| {
                        server["id"] == expected["id"]
                            && server["revision"].as_u64()
                                == expected["revision"].as_u64().and_then(|r| r.checked_add(1))
                    })
                }) else {
                    self.archery.recovery = Some("server");
                    self.archery_send(backend, "archery.server.list", json!({}), false);
                    return;
                };
                self.archery.server = Some(server.clone());
                self.archery.saved = true;
                self.field_set("password", "");
                self.field_set("archery_totp_uri", "");
                self.archery.clear_password = false;
                self.archery.clear_totp = false;
                if self.archery.parent_server_form.is_some() {
                    self.archery_server_form_return(Some(server), false, backend);
                } else if self.archery.server_only {
                    self.archery_release(backend);
                    self.archery.open = false;
                    self.workbench.overlay = self.archery.return_overlay.take();
                } else if self.archery.preview {
                    self.archery_save_batch(backend);
                } else {
                    self.archery_send(backend, "archery.server.list", json!({}), false);
                }
            }
            "archery.profile.apply_batch" => {
                if value["profiles"].as_array().is_none_or(|profiles| {
                    profiles.len() != self.archery.selected.len()
                        || !self.archery.selected.values().all(|selection| {
                            profiles
                                .iter()
                                .any(|p| p["id"] == json!(selection.profile_id))
                        })
                }) {
                    self.archery.recovery = Some("batch");
                    self.archery_send(backend, "profile.list", json!({}), false);
                    return;
                }
                self.archery_release(backend);
                self.archery.open = false;
                self.workbench.overlay = None;
                self.request(backend, "profile.list", "profile.list", json!({}));
                self.status = i18n::UiMessage::key("连接已保存");
            }
            "archery.server.delete" => {
                if let Some(server) = &self.archery.server {
                    self.archery
                        .servers
                        .retain(|item| item["id"] != server["id"]);
                }
                self.request(backend, "profile.list", "profile.list", json!({}));
                if self.archery.parent_server_form.is_some() {
                    self.archery_server_form_return(None, true, backend);
                } else {
                    self.archery.server = None;
                    self.archery.saved = false;
                    self.archery.state.clear();
                    self.archery.open = false;
                    self.workbench.overlay = self.archery.return_overlay.take();
                }
            }
            _ => {}
        }
    }
}
