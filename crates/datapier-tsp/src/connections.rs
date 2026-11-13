use super::*;
use datapier_core::model::{
    AccessMode, ConnectionConfig, DirectConnection, MysqlConnectionOptions, ProviderKind, TlsConfig,
};
pub(super) mod archery;

/// 新建表单与应用初始值共用同一份默认值；重开时同时丢弃旧字段的撤销历史。
pub(super) const PROFILE_DEFAULTS: &[(&str, &str)] = &[
    ("name", ""),
    ("host", "127.0.0.1"),
    ("port", "3306"),
    ("user", ""),
    ("password", ""),
    ("database", ""),
    ("mysql_timezone", "+00:00"),
    ("mysql_connect_timeout_seconds", ""),
    ("mysql_query_timeout_seconds", ""),
    ("redis_db", "0"),
    ("redis_databases_override", ""),
    ("profile_revision", "1"),
    ("provider", "mysql"),
    ("access", "read_only"),
    ("tls", "false"),
    ("ca", ""),
    ("cert", ""),
    ("key", ""),
];

impl App {
    pub(crate) fn connection_draft_capture(&mut self) {
        self.archery.direct_draft = Some((
            PROFILE_DEFAULTS
                .iter()
                .map(|(name, _)| self.field(name).to_owned())
                .collect(),
            self.navigation.form.all,
            self.navigation.form.selected.clone(),
        ));
    }

    pub(crate) fn connection_draft_dirty(&self) -> bool {
        self.archery
            .direct_draft
            .as_ref()
            .is_some_and(|(fields, all, selected)| {
                PROFILE_DEFAULTS
                    .iter()
                    .zip(fields)
                    .any(|((name, _), value)| self.field(name) != value)
                    || self.navigation.form.all != *all
                    || (!*all && self.navigation.form.selected != *selected)
            })
    }

    pub(super) fn connection_form_busy(&self) -> bool {
        self.form_id
            .is_some_and(|id| self.profile_saves.contains(&id))
            || (self.archery.open
                && self.archery.pending.as_ref().is_some_and(|(_, method)| {
                    matches!(
                        method.as_str(),
                        "archery.server.save"
                            | "archery.profile.apply_batch"
                            | "archery.server.delete"
                            | "archery.auth.close"
                    )
                }))
    }

    pub(super) fn connection_form_is_editing(&self) -> bool {
        self.form_editing
    }
    pub(super) fn connection_form_has_tabs(&self) -> bool {
        self.form_id
            .is_some_and(|id| self.tabs.iter().any(|tab| tab.profile_id == Some(id)))
    }

    pub(super) fn profile_form(&mut self, profile: Option<Profile>) {
        if self.connection_form_busy() {
            return;
        }
        self.navigation_form_open(profile.as_ref());
        self.navigation.form_redis_db_menu = false;
        self.workbench.profile_error = None;
        self.workbench.field_error = None;
        self.form_editing = profile.is_some();
        self.archery.state.clear();
        self.archery.error = None;
        self.archery.epoch = Uuid::new_v4();
        self.field_set("archery_otp", "");
        self.form_id = Some(profile.as_ref().map_or_else(Uuid::new_v4, |p| p.id));
        // 连接表单不编辑 MCP 策略；已有连接保留基线，新连接取全局默认。
        self.navigation.form_approval_policy = profile
            .as_ref()
            .map_or(self.settings.approval.default_policy, |profile| {
                profile.approval_policy
            });
        for &(name, value) in PROFILE_DEFAULTS {
            self.field_set(name, value);
        }
        if let Some(profile) = profile {
            if profile.archery().is_some() {
                self.workbench.profile_error =
                    Some(i18n::tr("请通过 Archery 服务管理修改连接").into_owned());
                self.workbench.overlay = Some("profile".into());
                self.workbench.focus_request = Some("input-name".into());
                return;
            }
            self.field_set("profile_revision", profile.revision.to_string());

            self.field_set("name", profile.name.clone());
            let Ok(direct) = profile.direct() else {
                self.workbench.profile_error =
                    Some(i18n::tr("请通过 Archery 服务管理修改连接").into_owned());
                self.workbench.overlay = Some("profile".into());
                return;
            };
            self.field_set("host", direct.host.clone());
            self.field_set("port", direct.port.to_string());
            self.field_set("user", direct.username.clone());
            self.field_set("database", direct.database.clone().unwrap_or_default());
            self.field_set("redis_db", direct.redis_db.to_string());
            self.field_set(
                "mysql_timezone",
                direct.mysql.timezone.clone().unwrap_or_default(),
            );
            self.field_set(
                "mysql_connect_timeout_seconds",
                direct
                    .mysql
                    .connect_timeout_seconds
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            );
            self.field_set(
                "mysql_query_timeout_seconds",
                direct
                    .mysql
                    .query_timeout_seconds
                    .map(|value| value.to_string())
                    .unwrap_or_default(),
            );
            self.field_set(
                "redis_databases_override",
                direct
                    .redis_databases_override
                    .map(|count| count.to_string())
                    .unwrap_or_default(),
            );
            self.field_set(
                "provider",
                match profile.provider {
                    ProviderKind::Mysql => "mysql",
                    ProviderKind::Redis => "redis",
                },
            );
            self.field_set(
                "access",
                match profile.access_mode {
                    AccessMode::ReadOnly => "read_only",
                    AccessMode::ReadWrite => "read_write",
                },
            );
            self.field_set("tls", direct.tls.enabled.to_string());
            self.field_set("ca", direct.tls.ca_path.clone().unwrap_or_default());
            self.field_set(
                "cert",
                direct.tls.client_cert_path.clone().unwrap_or_default(),
            );
            self.field_set(
                "key",
                direct.tls.client_key_path.clone().unwrap_or_default(),
            );
        }
        self.connection_draft_capture();
        self.workbench.profile_tls_details = self.field("tls") == "true";
        self.workbench.profile_timeout_details = false;
        self.workbench.overlay = Some("profile".into());
        self.workbench.focus_request = Some("input-name".into());
    }

    pub(super) fn form_profile(&self) -> std::result::Result<Profile, String> {
        let optional = |name: &str| {
            let value = self.field(name);
            (!value.is_empty()).then(|| value.to_owned())
        };
        Ok(Profile {
            id: self.form_id.ok_or("请先新建或编辑连接")?,
            revision: self
                .field("profile_revision")
                .parse::<u64>()
                .map_err(|e| e.to_string())?,
            approval_policy: self.navigation.form_approval_policy,
            name: self.field("name").to_owned(),
            connection: ConnectionConfig::Direct(DirectConnection {
                host: self.field("host").to_owned(),
                port: self
                    .field("port")
                    .parse::<u16>()
                    .map_err(|e| e.to_string())?,
                username: self.field("user").to_owned(),
                database: optional("database"),
                visible_databases: self.navigation_form_selection(),
                redis_db: if self.field("provider") == "redis" {
                    self.field("redis_db")
                        .parse::<u32>()
                        .map_err(|e| e.to_string())?
                } else {
                    0
                },
                redis_databases_override: if self.field("provider") == "redis" {
                    optional("redis_databases_override")
                        .map(|value| value.parse::<u32>().map_err(|e| e.to_string()))
                        .transpose()?
                } else {
                    None
                },
                mysql: MysqlConnectionOptions {
                    timezone: optional("mysql_timezone"),
                    connect_timeout_seconds: optional("mysql_connect_timeout_seconds")
                        .map(|value| value.parse::<u64>())
                        .transpose()
                        .or_else(|error| {
                            if self.field("provider") == "redis" {
                                Ok(None)
                            } else {
                                Err(error.to_string())
                            }
                        })?,
                    query_timeout_seconds: optional("mysql_query_timeout_seconds")
                        .map(|value| value.parse::<u64>())
                        .transpose()
                        .or_else(|error| {
                            if self.field("provider") == "redis" {
                                Ok(None)
                            } else {
                                Err(error.to_string())
                            }
                        })?,
                },
                tls: TlsConfig {
                    enabled: self
                        .field("tls")
                        .parse::<bool>()
                        .map_err(|e| e.to_string())?,
                    ca_path: optional("ca"),
                    client_cert_path: optional("cert"),
                    client_key_path: optional("key"),
                },
            }),
            provider: match self.field("provider") {
                "mysql" => ProviderKind::Mysql,
                "redis" => ProviderKind::Redis,
                _ => return Err("未知数据库类型".into()),
            },
            access_mode: match self.field("access") {
                "read_only" => AccessMode::ReadOnly,
                "read_write" => AccessMode::ReadWrite,
                _ => return Err("无效访问模式".into()),
            },
        })
    }

    pub(super) fn validate_profile_form(&mut self) -> bool {
        let error = if self.field("name").trim().is_empty() {
            Some(("name", "请填写连接名称"))
        } else if self.field("host").trim().is_empty() {
            Some(("host", "请填写主机"))
        } else if self
            .field("port")
            .parse::<u16>()
            .ok()
            .filter(|port| *port != 0)
            .is_none()
        {
            Some(("port", "请输入有效端口"))
        } else if self.field("provider") == "redis"
            && self.field("redis_db").parse::<u32>().is_err()
        {
            Some(("redis_db", "请输入有效 Redis DB"))
        } else if self.field("provider") == "redis"
            && !self.field("redis_databases_override").is_empty()
            && self
                .field("redis_databases_override")
                .parse::<u32>()
                .ok()
                .filter(|count| *count > 0)
                .is_none()
        {
            Some(("redis_databases_override", "请输入有效 Redis DB 数量"))
        } else if self.field("provider") == "mysql"
            && (MysqlConnectionOptions {
                timezone: (!self.field("mysql_timezone").is_empty())
                    .then(|| self.field("mysql_timezone").to_owned()),
                ..Default::default()
            })
            .validate()
            .is_err()
        {
            Some(("mysql_timezone", "时区格式无效"))
        } else if let Some(field) = (self.field("provider") == "mysql")
            .then(|| {
                [
                    "mysql_connect_timeout_seconds",
                    "mysql_query_timeout_seconds",
                ]
                .into_iter()
                .find(|field| {
                    !self.field(field).is_empty()
                        && self
                            .field(field)
                            .parse::<u64>()
                            .ok()
                            .filter(|value| (1..=86400).contains(value))
                            .is_none()
                })
            })
            .flatten()
        {
            Some((field, "超时秒数须为 1–86400 的整数，留空继承全局设置"))
        } else if !matches!(self.field("provider"), "mysql" | "redis") {
            Some(("provider", "未知数据库类型"))
        } else if !matches!(self.field("access"), "read_only" | "read_write") {
            Some(("access", "无效访问模式"))
        } else if !matches!(self.field("tls"), "true" | "false") {
            Some(("tls", "无效 TLS 配置"))
        } else if self.field("tls") == "false"
            && ["ca", "cert", "key"]
                .iter()
                .any(|field| !self.field(field).is_empty())
        {
            Some(("tls", "使用证书路径前请启用 TLS"))
        } else if let Some(field) = ["ca", "cert", "key"]
            .into_iter()
            .find(|field| !self.field(field).is_empty() && self.field(field).trim().is_empty())
        {
            Some((field, "证书路径不能只包含空白"))
        } else if self.field("cert").is_empty() != self.field("key").is_empty() {
            Some((
                if self.field("cert").is_empty() {
                    "cert"
                } else {
                    "key"
                },
                "客户端证书和私钥必须成对填写",
            ))
        } else {
            None
        };
        if let Some((field, message)) = error {
            if matches!(field, "ca" | "cert" | "key" | "tls") {
                self.workbench.profile_tls_details = true;
            }
            if matches!(
                field,
                "mysql_connect_timeout_seconds" | "mysql_query_timeout_seconds"
            ) {
                self.workbench.profile_timeout_details = true;
            }
            self.field_error(field, i18n::UiMessage::key(message));
            return false;
        }
        self.workbench.field_error = None;
        true
    }

    pub(super) fn submit_profile_form(&mut self, save: bool, backend: &Backend) {
        if self.connection_form_busy() {
            return;
        }
        if !self.validate_profile_form() {
            return;
        }
        let profile = match self.form_profile() {
            Ok(profile) => profile,
            Err(error) => {
                self.workbench.profile_error = Some(error.clone());
                self.status = i18n::UiMessage::local(error);
                return;
            }
        };
        // 保存后会重连该配置的标签；先拒绝任何可能被重连丢弃的工作。
        if save && self.form_editing && self.navigation_profile_has_pending_work(profile.id) {
            let reason = "该连接存在草稿或任务，请先处理";
            self.workbench.profile_error = Some(i18n::tr(reason).into_owned());
            self.status = i18n::UiMessage::key(reason);
            return;
        }
        self.workbench.profile_error = None;
        let secret =
            (!self.field("password").is_empty()).then(|| self.field("password").to_owned());
        let method = if save { "profile.save" } else { "profile.test" };
        self.request(
            backend,
            &format!("{method}/{}", profile.id),
            method,
            json!({"profile":profile,"secret":secret,"save_secret":true}),
        );
    }

    pub(super) fn profile_saved(&mut self, value: Value, backend: &Backend) {
        let profile: Profile = match serde_json::from_value(value["profile"].clone()) {
            Ok(profile) => profile,
            Err(error) => {
                self.workbench.profile_error = Some(error.to_string());
                self.status = i18n::UiMessage::raw(error.to_string());
                return;
            }
        };
        self.workbench.profile_error = None;
        self.form_id = Some(profile.id);
        self.navigation_invalidate_profile(profile.id);
        if let Some(current) = self.profiles.iter_mut().find(|p| p.id == profile.id) {
            *current = profile.clone();
        } else {
            self.profiles.push(profile.clone());
        }
        self.field_set("password", "");
        self.workbench.overlay = None;
        if self.form_editing {
            self.reconnect_saved_profile(profile, backend);
        } else {
            // 新建并保存其他配置，不改变现有标签的连接或编辑归属。
            if self.session.is_none()
                && self
                    .tabs
                    .get(self.active)
                    .is_some_and(|tab| tab.profile_id.is_none_or(|id| id == profile.id))
            {
                self.profile = Some(profile.clone());
                self.tabs[self.active].profile_id = Some(profile.id);
                self.dirty = true;
            }
            self.status = i18n::UiMessage::key("连接已保存");
        }
        self.request(backend, "profile.list", "profile.list", json!({}));
    }

    fn reconnect_saved_profile(&mut self, profile: Profile, backend: &Backend) {
        // 异步保存完成前状态仍可能因外部输入变化；绝不因此丢弃任务或草稿。
        if self.navigation_profile_has_pending_work(profile.id) {
            self.status = i18n::UiMessage::key("连接已保存；请处理相关草稿或任务后再次保存以重连");
            return;
        }
        let active = self.active;
        let mut found = false;
        for index in 0..self.tabs.len() {
            if self.tabs[index].profile_id != Some(profile.id) {
                continue;
            }
            found = true;
            self.switch_tab(index);
            if self
                .profile
                .as_ref()
                .is_some_and(|current| current.provider != profile.provider)
            {
                // 资源 target 带 provider 语义，不能把表名当作 Redis key 或反向复用。
                self.target = None;
                self.database = None;
                self.workbench.queued_target = None;
                self.tabs[index].browse = BrowseSettings::default();
                self.field_set("pattern", "*");
                self.field_set("type", "");
                self.field_set("filter", "[]");
                self.field_set("sort", "[]");
                self.view = if profile.provider == ProviderKind::Redis {
                    "data"
                } else {
                    "query"
                }
                .into();
            }
            self.profile = Some(profile.clone());
            self.clear_cli_session();
            self.action("open", backend);
        }
        self.switch_tab(active);
        if !found {
            self.status = i18n::UiMessage::key("连接已保存");
            return;
        }
        self.dirty = true;
        self.status = i18n::UiMessage::key("连接已保存，正在重连");
    }
}

#[cfg(test)]
#[path = "../tests/unit/connections.rs"]
mod tests;
