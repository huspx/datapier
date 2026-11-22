use super::*;
use base64::Engine as _;
use std::cell::{Cell, RefCell};

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
struct UpdatePolicy {
    auto_check: bool,
    auto_download: bool,
    auto_apply: bool,
}

#[derive(Default)]
pub(super) struct NativeUiState {
    pub request: Option<Value>,
    started: Option<Instant>,
    status: Value,
    error: Option<String>,
    pub policy_error: Option<String>,
    saved: Option<UpdatePolicy>,
    draft: Option<UpdatePolicy>,
    submitted: Option<UpdatePolicy>,
    draftbar: RefCell<Option<(bool, Node<Msg>)>>,
    pub cache_hits: Cell<u64>,
    pub cache_builds: Cell<u64>,
    pub reduce_motion: bool,
    pub save_after_engine: bool,
}

impl NativeUiState {
    fn automatic_install_supported(&self) -> bool {
        self.status["automatic_install_supported"].as_bool() == Some(true)
    }

    pub(super) fn settings_dirty(&self) -> bool {
        self.draft != self.saved
    }

    pub(super) fn discard_settings(&mut self) {
        self.draft = self.saved;
    }

    pub(super) fn settings_busy(&self) -> bool {
        self.save_after_engine
            || self.request.as_ref().is_some_and(|request| {
                matches!(request["action"].as_str(), Some("configure" | "reload"))
            })
    }

    pub(super) fn settings_reloading(&self) -> bool {
        self.request
            .as_ref()
            .is_some_and(|request| request["action"] == "reload")
    }
}

impl App {
    pub(super) fn reload_update_settings(&mut self) {
        if self.native_ui.saved.is_some() {
            self.native_update_request("reload");
        }
    }

    pub(super) fn native_update_request(&mut self, action: &str) {
        if self.native_ui.request.is_some() {
            return;
        }
        if action == "apply"
            && (!self.native_ui.automatic_install_supported()
                || self.native_ui.status["prepared"].as_bool() != Some(true))
        {
            return;
        }
        let mut request = json!({"id":Uuid::new_v4(),"kind":"updates","action":action});
        if matches!(action, "configure" | "reload") {
            self.native_ui.policy_error = None;
        }
        if action == "configure" {
            let Some(policy) = self.native_ui.draft else {
                return;
            };
            request["settings"] = json!(policy);
            self.native_ui.submitted = Some(policy);
        }
        self.native_ui.request = Some(request);
        if action == "download" {
            self.native_ui.status["progress"] = Value::Null;
        }
        self.native_ui.started = Some(Instant::now());
        self.native_ui.error = None;
        if let Some(state) = match action {
            "check" => Some("checking"),
            "download" => Some("downloading"),
            "apply" => Some("installing"),
            _ => None,
        } {
            self.native_ui.status["state"] = json!(state);
        }
    }

    pub(super) fn native_ui_tick(&mut self) -> bool {
        let timeout = if self
            .native_ui
            .request
            .as_ref()
            .is_some_and(|request| request["action"] == "download")
        {
            Duration::from_secs(960)
        } else {
            Duration::from_secs(330)
        };
        if self
            .native_ui
            .started
            .is_some_and(|start| start.elapsed() > timeout)
        {
            let policy = self.native_ui.settings_busy();
            self.native_ui.started = None;
            self.native_ui.request = None;
            self.native_ui.submitted = None;
            self.native_ui.error = Some(i18n::tr("更新请求超时，请重新检查状态").into_owned());
            if policy {
                self.native_ui.policy_error = self.native_ui.error.clone();
            }
            return true;
        }
        false
    }

    pub(super) fn native_ui_action(&mut self, name: &str, backend: &Backend) -> bool {
        if let Some(encoded) = name.strip_prefix("datapier-plugin-update-status:") {
            let Some(value) = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            else {
                return true;
            };
            let Some(revision) = value["progress_revision"].as_u64() else {
                return true;
            };
            let current = self.native_ui.status["progress_revision"]
                .as_u64()
                .unwrap_or(0);
            if revision > current {
                for key in [
                    "state",
                    "message",
                    "target_version",
                    "progress",
                    "progress_revision",
                    "download_source",
                ] {
                    self.native_ui.status[key] = value[key].clone();
                }
                self.native_ui.status["prepared"] =
                    json!(value["state"] == "ready" || value["state"] == "waiting");
            }
            return true;
        }
        if let Some(encoded) = name.strip_prefix("datapier-plugin-reply:") {
            let Some(mut reply) = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            else {
                return true;
            };
            if self
                .native_ui
                .request
                .as_ref()
                .map(|request| &request["id"])
                != Some(&reply["id"])
            {
                return true;
            }
            let action = self
                .native_ui
                .request
                .as_ref()
                .and_then(|request| request["action"].as_str());
            let configuring = action == Some("configure");
            let reloading = action == Some("reload");
            let policy_request = configuring || reloading;
            let submitted = self.native_ui.submitted.take();
            self.native_ui.request = None;
            self.native_ui.started = None;
            let value = reply["value"].take();
            let policy = serde_json::from_value::<UpdatePolicy>(value["settings"].clone()).ok();
            if reply["ok"].as_bool() == Some(true) && (!policy_request || policy.is_some()) {
                if let Some(policy) = policy {
                    let replace = reloading
                        || !self.native_ui.settings_dirty()
                        || self.native_ui.draft == submitted;
                    self.native_ui.saved = Some(policy);
                    if replace {
                        self.native_ui.draft = Some(policy);
                    }
                }
                self.native_ui.error = None;
            } else {
                self.native_ui.error =
                    Some(reply["error"].as_str().unwrap_or("更新请求失败").to_owned());
            }
            if value.get("state").is_some()
                && value["progress_revision"].as_u64().unwrap_or(0)
                    >= self.native_ui.status["progress_revision"]
                        .as_u64()
                        .unwrap_or(0)
            {
                self.native_ui.status = value;
            }
            if policy_request {
                self.native_ui.policy_error = self.native_ui.error.clone();
            }
            return true;
        }
        if name == "app-settings" && self.native_ui.saved.is_none() {
            self.native_update_request("status");
        }
        if let Some(action) = name.strip_prefix("native-update:") {
            if matches!(action, "status" | "check" | "download" | "apply")
                && self.settings_save_token.is_none()
            {
                self.native_update_request(action);
            }
            return true;
        }
        if let Some(key) = name.strip_prefix("native-update-toggle:") {
            let supported = self.native_ui.automatic_install_supported();
            if self.native_ui.request.is_none()
                && self.settings_save_token.is_none()
                && let Some(policy) = self.native_ui.draft.as_mut()
            {
                match key {
                    "auto_check" => policy.auto_check = !policy.auto_check,
                    "auto_download" => {
                        policy.auto_download = !policy.auto_download;
                        if !policy.auto_download {
                            policy.auto_apply = false;
                        }
                    }
                    "auto_apply" if policy.auto_download && supported => {
                        policy.auto_apply = !policy.auto_apply;
                    }
                    _ => {}
                }
            }
            return true;
        }
        if let Some(id) = name.strip_prefix("native-todo-approval:") {
            if !self.approvals.open {
                self.approval_action("approval-toggle", backend);
            }
            self.approval_action("approval-tab-approvals", backend);
            self.approval_action(&format!("approval-select:{id}"), backend);
            return true;
        }
        if let Some(id) = name.strip_prefix("native-todo-auth:") {
            if !self.approvals.open {
                self.approval_action("approval-toggle", backend);
            }
            self.approval_action("approval-tab-authentication", backend);
            self.workbench.focus_request = Some(format!("approval-auth-{id}"));
            return true;
        }
        if let Some(section) = name.strip_prefix("datapier-open-todos") {
            if !self.approvals.open {
                self.approval_action("approval-toggle", backend);
            }
            match section {
                ":approvals" => {
                    self.approval_action("approval-tab-approvals", backend);
                }
                ":authentication" => {
                    self.approval_action("approval-tab-authentication", backend);
                }
                _ => self.approval_refresh(backend),
            }
            return true;
        }
        if let Some(encoded) = name.strip_prefix("datapier-project-path:") {
            if let Some(value) = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            {
                self.project_navigate_path(
                    backend,
                    value["host"].as_str().unwrap_or(""),
                    value["path"].as_str().unwrap_or(""),
                );
            }
            return true;
        }
        if let Some(rest) = name.strip_prefix("local-project-associate:") {
            let mut values = rest.splitn(3, ':');
            if let Some(profile) = values.next().and_then(|id| Uuid::parse_str(id).ok()) {
                let database = values
                    .next()
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned);
                let redis_db = values.next().and_then(|value| value.parse::<u32>().ok());
                self.project_associate_resource(
                    backend,
                    &profile.to_string(),
                    database.as_deref(),
                    redis_db,
                );
            }
            return true;
        }
        for (prefix, success) in [
            ("datapier-glimpse-opened:", true),
            ("datapier-glimpse-failed:", false),
        ] {
            if let Some(id) = name.strip_prefix(prefix) {
                self.project_glimpse_result(id, success);
                return true;
            }
        }
        false
    }
}

pub(super) fn update_settings(app: &App) -> Node<Msg> {
    let busy = app.native_ui.request.is_some() || app.settings_save_token.is_some();
    let mut panel = html::div().class("settings-section settings-section-wide");
    if let Some(policy) = app.native_ui.draft {
        for (key, label, value, enabled) in [
            ("auto_check", "自动检查更新", policy.auto_check, true),
            ("auto_download", "自动下载更新", policy.auto_download, true),
            (
                "auto_apply",
                "退出后自动应用",
                policy.auto_apply,
                policy.auto_download && app.native_ui.automatic_install_supported(),
            ),
        ] {
            let action = format!("native-update-toggle:{key}");
            panel = panel.child(
                ui::switch(i18n::tr(label).into_owned())
                    .on(value)
                    .disabled(busy || !enabled)
                    .on_change(move |_| Msg::Action(action.clone())),
            );
        }
    }
    panel = panel.child(
        html::p(i18n::tr("自动应用须启用自动下载，且仅在所有客户端安全退出后执行。").into_owned())
            .class("muted"),
    );
    if let Some(state) = app.native_ui.status.get("state").and_then(Value::as_str) {
        let label = match state {
            "unchecked" => "未检查",
            "checking" => "检查中",
            "current" => "已是最新",
            "available" => "发现更新",
            "downloading" => "下载中",
            "verifying" => "校验中",
            "ready" => "已准备，等待退出",
            "installing" => "安装中",
            "installed" => "安装成功",
            "waiting" => "等待客户端退出",
            "failed" => "更新失败",
            "development" => "开发安装",
            _ => state,
        };
        panel = panel.child(crate::advanced::fact(
            "更新状态",
            i18n::tr(label).into_owned(),
        ));
    }
    for (key, label) in [
        ("current_version", "当前版本"),
        ("target_version", "目标版本"),
    ] {
        if let Some(value) = app.native_ui.status[key].as_str() {
            panel = panel.child(crate::advanced::fact(label, value.to_owned()));
        }
    }
    if matches!(
        app.native_ui.status["state"].as_str(),
        Some("downloading" | "verifying")
    ) {
        let progress = &app.native_ui.status["progress"];
        let phase = match progress["phase"].as_str() {
            Some("plugin") => Some("插件包"),
            Some("program") => Some("程序归档"),
            _ => None,
        };
        if let (Some(phase), Some(received)) = (phase, progress["received_bytes"].as_u64()) {
            let source = match app.native_ui.status["download_source"].as_str() {
                Some("automatic") => i18n::tr("自动下载"),
                Some("manual") => i18n::tr("手动下载"),
                _ => i18n::tr("下载中"),
            };
            let label = format!("{} · {}", source, i18n::tr(phase));
            if let Some(total) = progress["total_bytes"]
                .as_u64()
                .filter(|total| *total > 0 && received <= *total)
            {
                let percent = received.saturating_mul(100) / total;
                panel = panel.child(crate::advanced::fact(
                    &label,
                    format!("{received} / {total} B · {percent}%"),
                ));
                panel = panel.child(
                    ui::progress()
                        .aria(label)
                        .value(received as f64 / total as f64),
                );
            } else {
                panel = panel.child(crate::advanced::fact(&label, format!("{received} B")));
            }
        }
    }
    if let Some(message) = app.native_ui.status["message"]
        .as_str()
        .filter(|message| !message.is_empty())
    {
        panel = panel.child(html::p(i18n::tr(message).into_owned()));
    }
    if let Some(next) = app.native_ui.status["next_check_at"]
        .as_i64()
        .and_then(|next| chrono::DateTime::from_timestamp(next, 0))
    {
        panel = panel.child(crate::advanced::fact(
            "下次检查时间",
            next.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S %:z")
                .to_string(),
        ));
    }
    if let Some(error) = app
        .native_ui
        .policy_error
        .as_ref()
        .or(app.native_ui.error.as_ref())
    {
        panel = panel.child(html::p(error.clone()).class("field-error"));
    }
    let mut actions = html::div().class("toolbar");
    let downloading = matches!(
        app.native_ui.status["state"].as_str(),
        Some("downloading" | "verifying")
    );
    for (action, label) in [
        ("status", "刷新更新状态"),
        ("check", "检查更新"),
        ("download", "下载更新"),
        ("apply", "退出后安装更新"),
    ] {
        actions = actions.child(
            ui::button(i18n::tr(label).into_owned())
                .disabled(
                    busy || (downloading && matches!(action, "check" | "download"))
                        || action == "apply"
                            && (app.native_ui.status["prepared"] != true
                                || !app.native_ui.automatic_install_supported()),
                )
                .on_click(Msg::Action(format!("native-update:{action}"))),
        );
    }
    panel.child(actions).into()
}

/// 此工具区只依赖界面语言；缓存复用 handlers，节点属性遵循 SDK 的 Clone 语义。
pub(super) fn draftbar(app: &App) -> Node<Msg> {
    let english = i18n::tr("项目") != "项目";
    let mut cached = app.native_ui.draftbar.borrow_mut();
    if let Some((language, node)) = &*cached
        && *language == english
    {
        app.native_ui
            .cache_hits
            .set(app.native_ui.cache_hits.get() + 1);
        return node.clone();
    }
    let mut node = html::div().class("draftbar").key("workspace-actions");
    for (label, action) in [
        ("导入数据", "transfer-new:import"),
        ("导出数据", "transfer-new:export"),
        ("迁移到…", "transfer-new:copy"),
        ("导出 SQL", "transfer-new:sql_dump"),
        ("恢复 SQL", "transfer-new:sql_restore"),
        ("项目", "project-new"),
    ] {
        node = node
            .child(ui::button(i18n::tr(label).into_owned()).on_click(Msg::Action(action.into())));
    }
    let node = Node::from(node);
    app.native_ui
        .cache_builds
        .set(app.native_ui.cache_builds.get() + 1);
    *cached = Some((english, node.clone()));
    node
}

pub(super) fn project_summary(app: &App) -> String {
    let project = &app.workbench.project;
    let Some(value) = &project.local_project else {
        return i18n::tr("当前没有项目关联").into_owned();
    };
    let environment = project
        .local_environment
        .and_then(|index| value["environments"].get(index));
    json!({"name":value["name"],"description":value["description"],"environment":environment.map(|env| json!({"name":env["name"],"description":env["description"],"type":env["environment_type"]}))}).to_string()
}

pub(super) fn program_state(app: &App) -> &'static str {
    if app
        .approvals
        .items
        .iter()
        .any(|item| item["state"].as_str() == Some("pending"))
        || !app.approvals.authentication.is_empty()
    {
        return "blocked";
    }
    if app.task.is_some()
        || app.operation_pending
        || app.dirty
        || app.settings_form_dirty()
        || app.has_live_drafts()
        || app.workbench.project.unsaved
        || app.tabs.iter().any(|tab| {
            tab.runtime.task.is_some()
                || tab.runtime.operation_pending
                || tab.runtime.workbench.project.unsaved
        })
        || app.approvals.observed_tasks.iter().any(|task| {
            matches!(
                task["state"].as_str(),
                Some("queued" | "running" | "cancelling")
            )
        })
    {
        "working"
    } else {
        "idle"
    }
}

pub(super) fn pending_notice(app: &App) -> Option<Node<Msg>> {
    if app.approvals.open || app.workbench.overlay.is_some() {
        return None;
    }
    let (id, label, action) = if let Some(item) = app
        .approvals
        .items
        .iter()
        .find(|item| item["state"].as_str() == Some("pending"))
    {
        let id = item["approval_id"].as_str()?;
        (id, "有命令等待审批", format!("native-todo-approval:{id}"))
    } else {
        let item = app.approvals.authentication.first()?;
        let id = item["request_id"].as_str()?;
        (id, "有连接等待认证", format!("native-todo-auth:{id}"))
    };
    Some(
        // 宿主 toast 没有点击事件；非模态 overlay 用公开按钮精确定位待办。
        ui::overlay()
            .anchor(ui::Anchor::Bottom)
            .size(ui::OverlaySize::Sm)
            .key(format!("pending-{id}"))
            .child(
                html::div()
                    .class("toolbar")
                    .child(html::span(i18n::tr(label).into_owned()).class("spacer"))
                    .child(ui::button(i18n::tr("查看").into_owned()).on_click(Msg::Action(action))),
            )
            .into(),
    )
}

#[cfg(test)]
#[path = "../tests/unit/native_ui.rs"]
mod tests;
