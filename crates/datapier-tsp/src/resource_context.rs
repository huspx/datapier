//! 连接选择、资源标签与加载目标。
use super::*;

impl App {
    /// 连接与资源各自保留 tab 运行态，切换对象不覆盖另一对象的草稿和结果。
    pub(super) fn select_profile(&mut self, index: usize, backend: &Backend) {
        let Some(profile) = self.profiles.get(index).cloned() else {
            return;
        };
        self.close_result_info(false);
        if self.tabs.is_empty() {
            self.action("new-tab", backend);
        }
        if let Some(index) = self.tabs.iter().enumerate().find_map(|(index, tab)| {
            let session = if index == self.active {
                &self.session
            } else {
                &tab.runtime.session
            };
            let has_cli_history = if index == self.active {
                !self.workbench.cli_history.is_empty()
            } else {
                !tab.runtime.workbench.cli_history.is_empty()
            };
            (tab.profile_id == Some(profile.id) && (session.is_some() || has_cli_history))
                .then_some(index)
        }) {
            self.select_tab(index);
            if self.active == index && self.session_id().is_none() {
                self.action("open", backend);
            }
            return;
        }
        if self.session.is_some()
            || self.has_live_drafts()
            || self.operation_pending
            || self.task.is_some()
            || (self.tabs[self.active].profile_id != Some(profile.id)
                && (self.target.is_some()
                    || !self.tabs[self.active].query.text.is_empty()
                    || !self.tabs[self.active].cli_query.text.is_empty()
                    || !self.workbench.cli_history.is_empty()))
        {
            self.action("new-tab", backend);
        }
        if self.tabs[self.active].profile_id != Some(profile.id) {
            self.database = profile.default_database().map(str::to_owned);
            self.tabs[self.active].browse.database = self.database.clone();
            self.tabs[self.active].browse.redis_db = (profile.provider
                == datapier_core::model::ProviderKind::Redis)
                .then(|| profile.redis_db());
        }
        self.profile = Some(profile.clone());
        self.tabs[self.active].profile_id = Some(profile.id);
        self.field_set("password", "");
        self.workbench.overlay = None;
        self.dirty = true;
        self.action("open", backend);
        self.focus_current_tab();
    }

    pub(super) fn open_query_tab(&mut self, backend: &Backend) {
        if !self.finish_inline(true) {
            return;
        }
        // Archery SQL 没有持久会话，先保留当前目标，不能让新 tab 退回默认库。
        let database = self
            .profile
            .as_ref()
            .filter(|profile| {
                profile.is_archery()
                    && profile.provider == datapier_core::model::ProviderKind::Mysql
            })
            .and_then(|_| self.database.clone());
        self.action("new-tab", backend);
        if let Some(database) = database {
            self.database = Some(database);
            self.tabs[self.active].browse.database = self.database.clone();
        }
        self.view = if self.is_redis() { "data" } else { "query" }.into();
        self.workbench.overlay = None;
        if self.session.is_none() && self.profile.is_some() {
            self.action("open", backend);
        }
        if self.is_redis() {
            self.workbench.cli = true;
            self.focus = "cli_text".into();
            self.workbench.focus_request = Some("input-cli_text".into());
        } else {
            self.focus = "query".into();
            self.workbench.focus_request = Some("query-editor".into());
        }
    }

    pub(super) fn select_resource(&mut self, index: usize, backend: &Backend) {
        if !self.finish_inline(true) {
            return;
        }
        let Some(resource) = self.resources.get(index).cloned() else {
            return;
        };
        let target = ResourceTarget {
            database: resource.database.or_else(|| self.database.clone()),
            name: resource.name,
            key_b64: resource.key_b64,
            kind: resource.kind,
        };
        if target.kind == "database" {
            if self.target.is_some()
                || self.has_live_drafts()
                || self.task.is_some()
                || self.operation_pending
            {
                self.action("new-tab", backend);
                self.workbench.queued_target = Some(target);
                self.action("open", backend);
            } else {
                self.generation = self.generation.wrapping_add(1);
                self.database = Some(target.name);
                self.field_set("object_filter", "");
                self.dirty = true;
                self.action("list", backend);
            }
            return;
        }
        let profile_id = self.profile.as_ref().map(|p| p.id);
        if let Some(index) = self.tabs.iter().enumerate().find_map(|(index, tab)| {
            let current = if index == self.active {
                &self.target
            } else {
                // 未访问的恢复 tab 尚无 runtime，资源身份已保存在 browse 中。
                &tab.browse.target
            };
            (tab.profile_id == profile_id
                && current.as_ref().is_some_and(|current| {
                    current.database == target.database
                        && current.name == target.name
                        && current.kind == target.kind
                        && current.key_b64 == target.key_b64
                }))
            .then_some(index)
        }) {
            self.select_tab(index);
            self.view = "data".into();
            self.workbench.overlay = None;
            if self.session_id().is_none() {
                self.action("open", backend);
            }
            return;
        }
        if self.target.is_some()
            || self.task.is_some()
            || self.operation_pending
            || self.has_live_drafts()
            || !self.tabs[self.active].query.text.is_empty()
        {
            // key 有独立 tab；资源导航仍沿用原来的扫描条件与目录展开状态。
            let browser = self.is_redis().then(|| {
                (
                    self.field("pattern").to_owned(),
                    self.field("type").to_owned(),
                    self.workbench.tree,
                    self.workbench.redis_expansion.clone(),
                )
            });
            self.action("new-tab", backend);
            if let Some((pattern, kind, tree, expansion)) = browser {
                self.field_set("pattern", pattern);
                self.field_set("type", kind);
                self.workbench.tree = tree;
                self.workbench.redis_expansion = expansion;
                self.capture_browse();
            }
            self.workbench.queued_target = Some(target);
            self.action("open", backend);
        } else {
            self.load_target(target, backend);
        }
    }

    pub(super) fn load_target(&mut self, target: ResourceTarget, backend: &Backend) {
        self.generation = self.generation.wrapping_add(1);
        self.scan_token = None;
        self.database = target.database.clone().or_else(|| self.database.clone());
        self.tabs[self.active].title = target.name.clone();
        self.target = Some(target);
        self.edit_token = None;
        self.schema = None;
        self.grid = None;
        self.results.clear();
        self.result_id = None;
        self.page_history.clear();
        self.page_token = None;
        self.large = None;
        self.browse_result = false;
        self.view = "data".into();
        self.workbench.section = "data".into();
        self.workbench.overlay = None;
        self.workbench.load_resource = true;
        self.workbench.focus_request = Some("grid".into());
        self.dirty = true;
        self.action("describe", backend);
    }

    pub(super) fn session_id(&self) -> Option<Value> {
        self.session
            .as_ref()
            .and_then(|s| s.get("session_id"))
            .cloned()
    }
}
