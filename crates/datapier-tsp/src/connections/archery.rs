use super::*;
use std::collections::{BTreeMap, BTreeSet};

mod action;
mod response;

#[derive(Default)]
pub(crate) struct Wizard {
    pub open: bool,
    pub epoch: Uuid,
    pub pending: Option<(Uuid, String)>,
    pub servers: Vec<Value>,
    pub loading_instance: Option<i64>,
    pub resume_instance: Option<i64>,
    pub discovery_queue: Vec<i64>,
    pub server: Option<Value>,
    pub saved: bool,
    pub auth_id: Option<String>,
    pub state: String,
    pub instances: Vec<Value>,
    pub selected: BTreeMap<i64, Selection>,
    pub expanded: BTreeSet<i64>,
    pub error: Option<String>,
    pub preview: bool,
    pub manage: bool,
    pub available_by_instance: BTreeMap<i64, BTreeSet<String>>,
    pub server_only: bool,
    parent_server_form: Option<ServerFormParent>,
    pub server_details: bool,
    pub totp_details: bool,
    pub discard_action: Option<String>,
    pub discard_accepted: bool,
    pub direct_draft: Option<(Vec<String>, bool, BTreeSet<String>)>,
    pub delete_confirm: bool,
    pub reauth_server: Option<Uuid>,
    pub save_secret: bool,
    pub save_totp: bool,
    pub clear_password: bool,
    pub clear_totp: bool,
    pub totp_automatic: bool,
    pub auth_dialog: bool,
    pub auth_candidate: bool,
    pub save_after_auth: bool,
    pub recovery: Option<&'static str>,
    pub staged_server: Option<Value>,
    pub baseline: Vec<Profile>,
    pub controller_token: Option<String>,
    pub auth_generation: u64,
    pub open_server: Option<Uuid>,
    pub focus_instance: Option<i64>,
    pub return_overlay: Option<String>,
    pub(crate) open_capture: Option<OpenContinuation>,
    pub(crate) open_continuation: Option<OpenContinuation>,
    pub(crate) connection_auth_error: Option<(Uuid, u64, Uuid)>,
    pub deferred_delete: bool,
}

pub(crate) struct OpenContinuation {
    tab: Uuid,
    generation: u64,
    profile: Value,
    database: Option<String>,
    target: Value,
    queued_target: Value,
    params: Value,
}

pub(crate) struct Selection {
    pub profile_id: Uuid,
    pub instance: Value,
    pub databases: Option<Vec<String>>,
    pub available_databases: Option<BTreeSet<String>>,
    pub selected: BTreeSet<String>,
    pub default: Option<String>,
    pub error: Option<String>,
}

struct ServerFormParent {
    server: Option<Value>,
    saved: bool,
    auth_id: Option<String>,
    state: String,
    controller_token: Option<String>,
    auth_generation: u64,
    fields: Vec<(&'static str, String)>,
}

impl App {
    /// 比较可提交内容；发现列表或展开状态变化不构成编辑。
    fn archery_draft_dirty(&self) -> bool {
        let state = &self.archery;
        if state.server_only {
            let original = state
                .server
                .as_ref()
                .and_then(|server| state.servers.iter().find(|item| item["id"] == server["id"]));
            let changed = |field: &str, value: &str| self.field(field) != value;
            return changed(
                "archery_name",
                original.and_then(|s| s["name"].as_str()).unwrap_or(""),
            ) || changed(
                "archery_url",
                original.and_then(|s| s["base_url"].as_str()).unwrap_or(""),
            ) || changed(
                "archery_user",
                original.and_then(|s| s["username"].as_str()).unwrap_or(""),
            ) || changed(
                "archery_ca",
                original
                    .and_then(|s| s["http_tls"]["ca_path"].as_str())
                    .unwrap_or(""),
            ) || !self.field("password").is_empty()
                || !self.field("archery_totp_uri").is_empty()
                || state.clear_password
                || state.clear_totp
                || original.is_some_and(|s| {
                    state.save_secret != (s["credential_mode"] == "keyring")
                        || state.save_totp != (s["totp_credential_mode"] == "keyring")
                        || state.totp_automatic != (s["totp_mode"] == "automatic")
                })
                || (original.is_none()
                    && (!state.save_secret || state.save_totp || state.totp_automatic));
        }
        state.server.as_ref().is_some_and(|server| {
            let desired = self.archery_desired_profiles(server);
            desired.len() != state.baseline.len()
                || state
                    .baseline
                    .iter()
                    .any(|old| !desired.contains(&json!(old)))
        }) || !self.field("password").is_empty()
            || !self.field("archery_otp").is_empty()
    }

    pub(crate) fn archery_capture_connection_open(&mut self, params: &Value) {
        self.archery.open_continuation = None;
        self.archery.open_capture = self
            .profile
            .as_ref()
            .filter(|profile| profile.is_archery() && params["profile_id"] == json!(profile.id))
            .map(|profile| OpenContinuation {
                tab: self.tabs[self.active].id,
                generation: self.generation,
                profile: json!(profile),
                database: self.database.clone(),
                target: json!(self.target),
                queued_target: json!(self.workbench.queued_target),
                params: params.clone(),
            });
    }

    pub(crate) fn archery_open_context_matches(&self, open: &OpenContinuation) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| tab.id == open.tab)
            && self.generation == open.generation
            && self
                .profile
                .as_ref()
                .is_some_and(|profile| json!(profile) == open.profile)
            && self.database == open.database
            && json!(self.target) == open.target
            && json!(self.workbench.queued_target) == open.queued_target
    }

    pub(crate) fn archery_password_policy_changed(&self) -> bool {
        self.archery.saved
            && !self.archery.clear_password
            && self.archery.server.as_ref().is_some_and(|server| {
                self.archery.save_secret != (server["credential_mode"] == "keyring")
            })
    }

    pub(crate) fn archery_totp_policy_changed(&self) -> bool {
        self.archery.saved
            && !self.archery.clear_totp
            && self.archery.server.as_ref().is_some_and(|server| {
                self.archery.save_totp != (server["totp_credential_mode"] == "keyring")
            })
    }

    fn archery_validate_server(&mut self) -> bool {
        for (field, message) in [
            ("archery_name", "请填写服务名称"),
            ("archery_url", "请填写服务地址"),
            ("archery_user", "请填写用户名"),
        ] {
            if self.field(field).trim().is_empty() {
                self.field_error(field, i18n::UiMessage::key(message));
                return false;
            }
        }
        if self.field("archery_totp_uri").is_empty()
            && ((self.archery.totp_automatic
                && (self.archery.clear_totp
                    || self
                        .archery
                        .server
                        .as_ref()
                        .is_none_or(|server| server["totp_configured"] != true)))
                || (self.archery.server_only && self.archery_totp_policy_changed()))
        {
            self.archery.totp_details = true;
            self.field_error("archery_totp_uri", i18n::UiMessage::key("请填写 TOTP 链接"));
            return false;
        }
        self.workbench.field_error = None;
        true
    }

    pub(crate) fn archery_global_servers_response(&mut self, response: Response) {
        match response.result {
            Ok(value) => {
                if let Some(servers) = value["servers"].as_array() {
                    self.archery.servers = servers.clone();
                }
            }
            Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
        }
    }
    pub(crate) fn archery_auth_changed(&mut self, server_id: Uuid, backend: &Backend) {
        if self.archery.pending.is_none()
            && self
                .archery
                .server
                .as_ref()
                .and_then(|s| s["id"].as_str())
                .and_then(|id| Uuid::parse_str(id).ok())
                == Some(server_id)
            && let Some(auth_id) = self.archery.auth_id.clone()
        {
            self.archery_send(
                backend,
                "archery.auth.status",
                json!({"auth_id":auth_id}),
                false,
            );
        }
    }

    pub(crate) fn archery_open_server(
        &mut self,
        server_id: Uuid,
        instance_id: i64,
        backend: &Backend,
    ) {
        self.profile_form(None);
        self.form_editing = true;
        self.archery_action("archery-open", backend);
        self.archery.open_server = Some(server_id);
        self.archery.focus_instance = Some(instance_id);
    }
    pub(crate) fn archery_manage_server(&mut self, server_id: Option<Uuid>, backend: &Backend) {
        let previous = self.workbench.overlay.clone();
        self.profile_form(None);
        self.archery_action("archery-open", backend);
        self.archery.server_only = true;
        self.archery.manage = true;
        self.archery.return_overlay = previous;
        self.archery.open_server = server_id;
    }
    fn archery_server_form_open(&mut self, create: bool) {
        let fields = [
            "archery_name",
            "archery_url",
            "archery_user",
            "archery_ca",
            "archery_totp_uri",
            "password",
        ]
        .into_iter()
        .map(|name| (name, self.field(name).to_owned()))
        .collect();
        self.archery.parent_server_form = Some(ServerFormParent {
            server: self.archery.server.clone(),
            saved: self.archery.saved,
            auth_id: self.archery.auth_id.take(),
            state: std::mem::take(&mut self.archery.state),
            controller_token: self.archery.controller_token.take(),
            auth_generation: self.archery.auth_generation,
            fields,
        });
        self.archery.server_only = true;
        self.archery.manage = true;
        self.archery.server_details = false;
        self.archery.totp_details = false;
        self.archery.clear_password = false;
        self.archery.clear_totp = false;
        self.archery.auth_candidate = false;
        self.field_set("password", "");
        self.field_set("archery_totp_uri", "");
        if create {
            self.archery.server = None;
            self.archery.saved = false;
            self.archery.totp_automatic = false;
            self.archery.save_secret = true;
            self.archery.save_totp = false;
            for name in ["archery_name", "archery_url", "archery_user", "archery_ca"] {
                self.field_set(name, "");
            }
        }
    }

    fn archery_server_form_return(
        &mut self,
        saved: Option<Value>,
        deleted: bool,
        backend: &Backend,
    ) {
        self.archery_release(backend);
        let Some(parent) = self.archery.parent_server_form.take() else {
            return;
        };
        if let Some(server) = &saved {
            if let Some(existing) = self
                .archery
                .servers
                .iter_mut()
                .find(|item| item["id"] == server["id"])
            {
                *existing = server.clone();
            } else {
                self.archery.servers.push(server.clone());
            }
        }
        let selected = match (parent.server, saved) {
            (Some(old), Some(new)) if old["id"] == new["id"] => Some(new),
            (Some(old), _) => Some(old),
            (None, Some(new)) => Some(new),
            (None, None) => None,
        };
        self.archery.server = if deleted { None } else { selected };
        self.archery.saved = if deleted {
            false
        } else {
            parent.saved || self.archery.server.is_some()
        };
        self.archery.auth_id = if deleted { None } else { parent.auth_id };
        self.archery.state = if deleted { String::new() } else { parent.state };
        self.archery.controller_token = if deleted {
            None
        } else {
            parent.controller_token
        };
        self.archery.auth_generation = parent.auth_generation;
        self.archery.server_only = false;
        self.archery.manage = false;
        self.archery.delete_confirm = false;
        self.archery.error = None;
        for (name, value) in parent.fields {
            self.field_set(name, value);
        }
        self.archery.auth_candidate = false;
        if let Some(server) = self.archery.server.clone() {
            self.archery.totp_automatic = server["totp_mode"] == "automatic";
            self.archery.save_secret = server["credential_mode"] == "keyring";
            self.archery.save_totp = server["totp_credential_mode"] == "keyring";
            self.field_set("archery_name", server["name"].as_str().unwrap_or(""));
            self.field_set("archery_url", server["base_url"].as_str().unwrap_or(""));
            self.field_set("archery_user", server["username"].as_str().unwrap_or(""));
            self.field_set(
                "archery_ca",
                server["http_tls"]["ca_path"].as_str().unwrap_or(""),
            );
        }
        if deleted {
            self.archery.baseline.clear();
            self.archery.selected.clear();
            self.archery.instances.clear();
        }
        self.workbench.overlay = Some("profile".into());
    }

    fn archery_baseline(&mut self, server_id: Uuid) {
        self.archery.baseline = self
            .profiles
            .iter()
            .filter(|profile| {
                profile
                    .archery()
                    .is_some_and(|connection| connection.server_id == server_id)
            })
            .cloned()
            .collect();
        self.archery.selected.clear();
        for profile in &self.archery.baseline {
            let connection = profile.archery().unwrap();
            self.archery.selected.insert(connection.instance_id, Selection {
                profile_id: profile.id,
                instance: json!({"id": connection.instance_id, "name": connection.instance_name, "provider": profile.provider}),
                available_databases: None,
                databases: Some(connection.databases.clone()),
                selected: connection.databases.iter().cloned().collect(),
                default: Some(connection.default_database.clone()),
                error: None,
            });
        }
        let names: Vec<_> = self
            .archery
            .baseline
            .iter()
            .map(|p| (p.id, p.name.clone()))
            .collect();
        for (id, name) in names {
            self.field_set(&format!("archery_profile_name_{id}"), name);
        }
    }
    pub(crate) fn archery_reauthenticate(&mut self, server_id: Uuid, backend: &Backend) {
        let previous = self.workbench.overlay.clone();
        self.profile_form(None);
        self.archery_action("archery-open", backend);
        self.archery.return_overlay = previous;
        self.archery.reauth_server = Some(server_id);
        self.archery.auth_dialog = true;
        self.workbench.overlay = Some("archery-auth".into());
        self.workbench.focus_request = Some("input-password".into());
    }

    fn archery_discover_next(&mut self, backend: &Backend) {
        if !self.archery.discovery_queue.is_empty() {
            let id = self.archery.discovery_queue.remove(0);
            self.archery.loading_instance = Some(id);
            if let Some(auth_id) = self.archery.auth_id.clone() {
                self.archery_send(
                    backend,
                    "archery.databases",
                    json!({"auth_id":auth_id,"instance_id":id}),
                    true,
                );
            }
        }
    }

    fn archery_send(
        &mut self,
        backend: &Backend,
        method: &str,
        mut params: Value,
        cancellable: bool,
    ) {
        let operation = Uuid::new_v4();
        if cancellable {
            params["operation_id"] = json!(operation);
        }
        let tag = format!("archery-ui/{}/{}/{}", self.archery.epoch, operation, method);
        match backend.request(tag, method, params) {
            Ok(()) => self.archery.pending = Some((operation, method.to_owned())),
            Err(error) => self.archery.error = Some(error.to_string()),
        }
    }

    pub(crate) fn archery_release(&mut self, backend: &Backend) {
        if let Some((operation, method)) = self.archery.pending.take()
            && matches!(
                method.as_str(),
                "archery.auth.begin"
                    | "archery.auth.verify"
                    | "archery.instances"
                    | "archery.databases"
            )
        {
            let _ = backend.request(
                "archery-release",
                "archery.operation.cancel",
                json!({"operation_id":operation}),
            );
        }
        if let Some(auth_id) = self.archery.auth_id.take() {
            let _ = backend.request(
                "archery-release",
                "archery.auth.close",
                json!({"auth_id":auth_id,"controller_token":self.archery.controller_token,"auth_generation":self.archery.auth_generation}),
            );
        }
        self.archery.discovery_queue.clear();
        self.archery.loading_instance = None;
        self.field_set("password", "");
        self.field_set("archery_otp", "");
    }

    fn archery_save_server(&mut self, backend: &Backend) {
        if let Some(mut server) = self.archery.server.clone() {
            server["name"] = json!(self.field("archery_name"));
            server["totp_mode"] = json!(if self.archery.totp_automatic {
                "automatic"
            } else {
                "manual"
            });
            server["http_tls"]["ca_path"] =
                json!((!self.field("archery_ca").is_empty()).then(|| self.field("archery_ca")));
            self.archery.staged_server = Some(server.clone());
            let password = self.field("password");
            let update = if self.archery.clear_password {
                json!({"kind":"clear"})
            } else if password.is_empty() {
                json!({"kind":"keep"})
            } else {
                json!({"kind":"replace","value":password,"persist":self.archery.save_secret})
            };
            let totp = self.field("archery_totp_uri");
            let totp_update = if self.archery.clear_totp {
                json!({"kind":"clear"})
            } else if totp.is_empty() {
                json!({"kind":"keep"})
            } else {
                json!({"kind":"replace","value":totp,"persist":self.archery.save_totp})
            };
            self.archery_send(backend, "archery.server.save", json!({"auth_id":self.archery.auth_id,"expected_revision":server["revision"],"server":server,"password_update":update,"totp_update":totp_update}), false);
        }
    }

    pub(crate) fn archery_desired_profiles(&self, server: &Value) -> Vec<Value> {
        self.archery.selected.values().map(|s| {
            let databases = self.archery.baseline.iter().find(|old| old.id == s.profile_id).and_then(Profile::archery)
                .filter(|old| old.databases.iter().cloned().collect::<BTreeSet<_>>() == s.selected)
                .map(|old| old.databases.clone()).unwrap_or_else(|| s.selected.iter().cloned().collect());
            json!({"id":s.profile_id,"name":self.field(&format!("archery_profile_name_{}", s.profile_id)),"provider":s.instance["provider"],"access_mode":"read_only","connection":{"kind":"archery","server_id":server["id"],"instance_id":s.instance["id"],"instance_name":s.instance["name"],"databases":databases,"default_database":s.default}})
        }).collect()
    }

    fn archery_save_batch(&mut self, backend: &Backend) {
        let Some(server) = self.archery.server.clone() else {
            return;
        };
        let desired = self.archery_desired_profiles(&server);
        self.archery_send(backend, "archery.profile.apply_batch", json!({"auth_id":self.archery.auth_id,"server_id":server["id"],"expected_server_revision":server["revision"],"expected_profiles":self.archery.baseline,"desired_profiles":desired}), false);
    }
}
