use super::*;
use crate::i18n::UiMessage;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Default)]
pub(super) struct DatabaseNode {
    pub expanded: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub resources: Vec<Resource>,
    pub collapsed_kinds: BTreeSet<String>,
    loaded: bool,
}

#[derive(Clone, Default)]
pub(super) struct DatabaseTree {
    pub databases: Vec<Resource>,
    pub loading: bool,
    pub error: Option<String>,
    pub nodes: BTreeMap<String, DatabaseNode>,
    loaded: bool,
}

#[derive(Clone, Default)]
pub(super) struct DatabaseForm {
    pub loaded: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub candidates: Vec<String>,
    pub selected: BTreeSet<String>,
    pub all: bool,
}

#[derive(Clone)]
pub(super) struct ProfileDeleteConfirmation {
    pub token: Uuid,
    pub id: Uuid,
    pub name: String,
    pub busy: bool,
    pub error: Option<String>,
    tab: Uuid,
    profile: Value,
}

#[derive(Clone)]
struct ProfileDeleteOperation {
    id: Uuid,
    token: Uuid,
    sessions: Vec<(Uuid, Value)>,
}

#[derive(Clone)]
pub(super) struct ResourceDropConfirmation {
    pub token: Uuid,
    pub name: String,
    pub database: String,
    pub kind: &'static str,
    pub busy: bool,
    pub task_started: bool,
    pub cancel_requested: bool,
    pub error: Option<String>,
    tab: Uuid,
    generation: u64,
    profile_id: Uuid,
    profile: Value,
    session_profile: Value,
    source_profile: Value,
    session: Value,
}

#[derive(Clone)]
struct DropOperation {
    confirmation: ResourceDropConfirmation,
    task: Option<String>,
    polling: bool,
    last_poll: Instant,
    ddl_session: Option<Value>,
    sql: String,
    terminal: Option<(String, String)>,
    closing: bool,
    cancel_requested: bool,
    cancel_pending: bool,
}

#[derive(Clone)]
struct QueuedSection {
    target: ResourceTarget,
    section: String,
}

#[derive(Clone)]
enum Pending {
    Tree {
        profile: Uuid,
        database: Option<String>,
        session: Value,
    },
    Form {
        profile: Value,
        secret: Value,
    },
    Redis {
        tab: Uuid,
        generation: u64,
        db: u32,
    },
    DeleteProfile {
        id: Uuid,
        token: Uuid,
    },
    CloseProfileSession {
        token: Uuid,
        tab: Uuid,
        session: Value,
    },
    DropOpen {
        token: Uuid,
    },
    DropClose {
        token: Uuid,
    },
    DropCancel {
        token: Uuid,
    },
    DropStart {
        token: Uuid,
    },
    DropPoll {
        token: Uuid,
    },
}

/// 跨 tab 的导航状态；仅收展状态写入工作区，异步请求按 token 隔离。
#[derive(Clone)]
pub(super) struct NavigationState {
    pub expanded: bool,
    pub narrow_expanded: bool,
    pub databases: BTreeMap<Uuid, DatabaseTree>,
    pub form: DatabaseForm,
    pub redis_db_menu: bool,
    pub form_redis_db_menu: bool,
    delete_confirmation: Option<ProfileDeleteConfirmation>,
    delete_operation: Option<ProfileDeleteOperation>,
    drop_confirmation: Option<ResourceDropConfirmation>,
    drop_operation: Option<DropOperation>,
    queued_sections: BTreeMap<Uuid, QueuedSection>,
    pending: BTreeMap<Uuid, Pending>,
}
impl Default for NavigationState {
    fn default() -> Self {
        Self {
            expanded: true,
            narrow_expanded: false,
            databases: BTreeMap::new(),
            form: DatabaseForm::default(),
            redis_db_menu: false,
            form_redis_db_menu: false,
            delete_confirmation: None,
            delete_operation: None,
            drop_confirmation: None,
            drop_operation: None,
            queued_sections: BTreeMap::new(),
            pending: BTreeMap::new(),
        }
    }
}

impl App {
    pub(super) fn navigation_drop_confirmation(&self) -> Option<&ResourceDropConfirmation> {
        self.navigation.drop_confirmation.as_ref().filter(|drop| {
            self.tabs
                .get(self.active)
                .is_some_and(|tab| tab.id == drop.tab)
                && self.workbench.overlay.as_deref() == Some("resource-drop")
        })
    }

    pub(super) fn navigation_drop_running(&self) -> bool {
        self.navigation
            .drop_operation
            .as_ref()
            .is_some_and(|operation| {
                self.tabs
                    .get(self.active)
                    .is_some_and(|tab| tab.id == operation.confirmation.tab)
            })
    }

    pub(super) fn navigation_poll_drop(&mut self, backend: &Backend) -> bool {
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

    fn navigation_drop_target_exists(&self, drop: &ResourceDropConfirmation) -> bool {
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

    fn navigation_same_connection(left: &Value, right: &Value) -> bool {
        let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
            return false;
        };
        left.iter()
            .filter(|(key, _)| !matches!(key.as_str(), "name" | "visible_databases"))
            .all(|(key, value)| right.get(key) == Some(value))
            && right
                .iter()
                .filter(|(key, _)| !matches!(key.as_str(), "name" | "visible_databases"))
                .all(|(key, value)| left.get(key) == Some(value))
    }

    fn navigation_drop_source_valid(&self, drop: &ResourceDropConfirmation) -> bool {
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

    fn navigation_drop_context_valid(&self, drop: &ResourceDropConfirmation) -> bool {
        self.tabs[self.active].id == drop.tab
            && self.navigation_drop_source_valid(drop)
            && self.navigation_drop_target_exists(drop)
    }

    fn navigation_drop_origin_pending(&mut self, tab: Uuid, pending: bool) {
        if let Some(index) = self.tabs.iter().position(|item| item.id == tab) {
            if index == self.active {
                self.operation_pending = pending;
            } else {
                self.tabs[index].runtime.operation_pending = pending;
            }
        }
    }
    fn navigation_drop_invalidate(&mut self, drop: &ResourceDropConfirmation, backend: &Backend) {
        let old = self.active;
        for index in 0..self.tabs.len() {
            if self.tabs[index].profile_id != Some(drop.profile_id) {
                continue;
            }
            self.switch_tab(index);
            if self.database.as_deref() == Some(drop.database.as_str()) {
                if drop.kind == "database" {
                    self.resources.clear();
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

    fn navigation_drop_end(&mut self, token: Uuid, state: &str, detail: String, backend: &Backend) {
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

    fn navigation_drop_cancel_task(&mut self, backend: &Backend) {
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

    fn navigation_drop_detail(
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

    fn navigation_drop_complete(
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
        if matches!(state, "succeeded" | "outcome_unknown") {
            self.navigation_drop_invalidate(&drop, backend);
        }
        self.status = UiMessage::key(match state {
            "succeeded" => "资源已删除",
            "cancelled" => "资源删除已取消",
            "outcome_unknown" => "删除结果未知，请检查数据库后再操作",
            _ => "资源删除失败",
        });
        if detail == "连接配置已更改，请先重连" {
            self.status = UiMessage::key("连接配置已更改，请先重连");
        }
        self.navigation_drop_detail(&drop, "任务详情", detail);
    }

    pub(super) fn navigation_delete_confirmation(&self) -> Option<&ProfileDeleteConfirmation> {
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

    pub(super) fn navigation_profile_delete_pending(&self, id: Uuid) -> bool {
        self.navigation.delete_operation.as_ref().is_some_and(|operation| operation.id == id)
            || self.navigation.pending.values().any(|pending| matches!(pending, Pending::DeleteProfile { id: pending_id, .. } if *pending_id == id))
    }

    fn navigation_profile_delete_unlock(&mut self, id: Uuid) {
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

    fn navigation_profile_delete_next(&mut self, backend: &Backend) {
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

    fn navigation_profile_session_closed(&mut self, tab: Uuid, session: &Value) {
        let Some(index) = self.tabs.iter().position(|item| item.id == tab) else {
            return;
        };
        let old = self.active;
        self.switch_tab(index);
        if self.session_id().as_ref() == Some(session) {
            self.session = None;
            self.generation = self.generation.wrapping_add(1);
            self.resources.clear();
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
    pub(super) fn navigation_profile_has_pending_work(&self, id: Uuid) -> bool {
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

    pub(super) fn navigation_apply_queued_section(&mut self) {
        let tab = self.tabs[self.active].id;
        if let Some(queued) = self.navigation.queued_sections.remove(&tab)
            && self.target.as_ref().is_some_and(|target| {
                target.database == queued.target.database
                    && target.name == queued.target.name
                    && target.kind == queued.target.kind
                    && target.key_b64 == queued.target.key_b64
            })
        {
            self.workbench.section = queued.section;
        }
    }

    pub(super) fn navigation_form_open(&mut self, profile: Option<&Profile>) {
        let selected = profile.and_then(|p| p.visible_databases.as_ref());
        self.navigation.form = DatabaseForm {
            selected: selected.into_iter().flatten().cloned().collect(),
            all: selected.is_none(),
            ..DatabaseForm::default()
        };
        self.field_set("database_search", "");
        self.navigation
            .pending
            .retain(|_, p| !matches!(p, Pending::Form { .. }));
    }
    pub(super) fn navigation_form_changed(&mut self) {
        self.navigation
            .pending
            .retain(|_, p| !matches!(p, Pending::Form { .. }));
        self.navigation.form.candidates.clear();
        self.navigation.form.loaded = false;
        self.navigation.form.loading = false;
        self.navigation.form.error = None;
    }
    pub(super) fn navigation_invalidate_profile(&mut self, id: Uuid) {
        self.navigation.databases.remove(&id);
        self.navigation
            .pending
            .retain(|_, p| !matches!(p, Pending::Tree { profile, .. } if *profile == id));
    }

    pub(super) fn navigation_form_selection(&self) -> Option<Vec<String>> {
        (!self.navigation.form.all).then(|| self.navigation.form.selected.iter().cloned().collect())
    }

    pub(super) fn navigation_connection_state(&self, id: Uuid) -> (&'static str, Option<String>) {
        let mut connecting = false;
        let mut error = None;
        for (index, tab) in self.tabs.iter().enumerate() {
            if tab.profile_id != Some(id) {
                continue;
            }
            let (session, workbench) = if index == self.active {
                (&self.session, &self.workbench)
            } else {
                (&tab.runtime.session, &tab.runtime.workbench)
            };
            if session.is_some() {
                return ("已连接", None);
            }
            connecting |= workbench.connecting;
            if error.is_none() {
                error = workbench.connection_error.clone();
            }
        }
        if connecting {
            ("连接中", None)
        } else if error.is_some() {
            ("连接失败", error)
        } else {
            ("未连接", None)
        }
    }

    fn navigation_request(
        &mut self,
        backend: &Backend,
        pending: Pending,
        method: &str,
        params: Value,
    ) {
        let token = Uuid::new_v4();
        match backend.request(format!("navigation/{token}"), method, params) {
            Ok(()) => {
                self.navigation.pending.insert(token, pending);
            }
            Err(error) => self.navigation_finish(pending, Err(error), backend),
        }
    }

    pub(super) fn navigation_load_tree(&mut self, backend: &Backend, refresh: bool) {
        let (Some(profile), Some(session)) = (self.profile.as_ref(), self.session_id()) else {
            return;
        };
        if profile.provider != datapier_core::model::ProviderKind::Mysql {
            return;
        }
        let id = profile.id;
        if refresh {
            // 刷新保留展开意图，但旧目录请求不能再覆盖这一轮数据。
            self.navigation.pending.retain(
                |_, pending| !matches!(pending, Pending::Tree { profile, .. } if *profile == id),
            );
        }
        let visible = self
            .profiles
            .iter()
            .find(|p| p.id == id)
            .unwrap_or(profile)
            .visible_databases
            .as_ref();
        let mut expanded = Vec::new();
        let tree = self.navigation.databases.entry(id).or_default();
        if !refresh && (tree.loading || tree.loaded) {
            return;
        }
        if refresh {
            for (database, node) in &mut tree.nodes {
                node.loaded = false;
                node.error = None;
                node.resources.clear();
                node.loading =
                    node.expanded && visible.is_none_or(|databases| databases.contains(database));
                if node.loading {
                    expanded.push(database.clone());
                }
            }
        }
        tree.loading = true;
        tree.error = None;
        self.navigation_request(
            backend,
            Pending::Tree {
                profile: id,
                database: None,
                session: json!(session),
            },
            "resource.list",
            json!({"session_id":session,"database":null,"limit":200}),
        );
        for database in expanded {
            self.navigation_request(
                backend,
                Pending::Tree {
                    profile: id,
                    database: Some(database.clone()),
                    session: session.clone(),
                },
                "resource.list",
                json!({"session_id":session,"database":database,"limit":200}),
            );
        }
    }

    pub(super) fn navigation_action(&mut self, name: &str, backend: &Backend) -> bool {
        if let Some((kind, database)) = name
            .strip_prefix("navigation-group:")
            .and_then(|value| value.split_once(':'))
        {
            if !matches!(kind, "table" | "view") {
                return true;
            }
            let Some(node) = self
                .profile
                .as_ref()
                .and_then(|profile| self.navigation.databases.get_mut(&profile.id))
                .and_then(|tree| tree.nodes.get_mut(database))
            else {
                return true;
            };
            if !node.collapsed_kinds.remove(kind) {
                node.collapsed_kinds.insert(kind.into());
            }
            return true;
        }
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
            if !self.is_redis() || self.profile.as_ref().is_none_or(|p| p.redis_db == db) {
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
            let id = profile.id;
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
        if let Some(database) = name.strip_prefix("navigation-db-refresh:") {
            let (Some(profile), Some(session)) = (self.profile.as_ref(), self.session_id()) else {
                return true;
            };
            let id = profile.id;
            if self
                .profiles
                .iter()
                .find(|p| p.id == id)
                .unwrap_or(profile)
                .visible_databases
                .as_ref()
                .is_some_and(|visible| !visible.iter().any(|item| item == database))
            {
                return true;
            }
            if !self
                .navigation
                .databases
                .get(&id)
                .is_some_and(|tree| tree.nodes.contains_key(database))
            {
                return true;
            }
            self.navigation.pending.retain(|_, pending| !matches!(pending,
                Pending::Tree { profile, database: Some(pending_database), .. } if *profile == id && pending_database == database));
            let node = self
                .navigation
                .databases
                .get_mut(&id)
                .unwrap()
                .nodes
                .get_mut(database)
                .unwrap();
            node.loading = true;
            node.loaded = false;
            node.error = None;
            node.expanded = true;
            self.navigation_request(
                backend,
                Pending::Tree {
                    profile: id,
                    database: Some(database.into()),
                    session: session.clone(),
                },
                "resource.list",
                json!({"session_id":session,"database":database,"limit":200}),
            );
            return true;
        }
        if let Some(database) = name.strip_prefix("navigation-db:") {
            let (Some(profile), Some(session)) = (self.profile.as_ref(), self.session_id()) else {
                return true;
            };
            let id = profile.id;
            let visible = self.profiles.iter().find(|p| p.id == id).unwrap_or(profile);
            if visible
                .visible_databases
                .as_ref()
                .is_some_and(|v| !v.iter().any(|v| v == database))
            {
                return true;
            }
            let node = self
                .navigation
                .databases
                .entry(id)
                .or_default()
                .nodes
                .entry(database.to_string())
                .or_default();
            node.expanded = !node.expanded;
            if node.expanded && !node.loaded && !node.loading {
                node.loading = true;
                node.error = None;
                self.navigation_request(
                    backend,
                    Pending::Tree {
                        profile: id,
                        database: Some(database.into()),
                        session: json!(session),
                    },
                    "resource.list",
                    json!({"session_id":session,"database":database,"limit":200}),
                );
            }
            return true;
        }
        let resource_action = if let Some(rest) = name.strip_prefix("navigation-resource-section:")
        {
            rest.split_once(':').and_then(|(section, rest)| {
                matches!(section, "data" | "structure" | "ddl").then_some((section, rest))
            })
        } else {
            name.strip_prefix("navigation-resource:")
                .map(|rest| ("data", rest))
        };
        if let Some((section, rest)) = resource_action {
            if let Some((database, index)) = rest.rsplit_once(':') {
                let resource = self
                    .profile
                    .as_ref()
                    .and_then(|p| self.navigation.databases.get(&p.id))
                    .and_then(|tree| tree.nodes.get(database))
                    .and_then(|node| {
                        index
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| node.resources.get(i))
                    })
                    .cloned();
                if let Some(resource) = resource {
                    if !self.finish_inline(true) {
                        return true;
                    }
                    let target = ResourceTarget {
                        database: Some(database.into()),
                        name: resource.name,
                        key_b64: resource.key_b64,
                        kind: resource.kind,
                    };
                    if let Some(index) = self.tabs.iter().enumerate().find_map(|(i, tab)| {
                        let current = if i == self.active {
                            &self.target
                        } else {
                            &tab.browse.target
                        };
                        (tab.profile_id == self.profile.as_ref().map(|p| p.id)
                            && current.as_ref().is_some_and(|t| {
                                t.database == target.database
                                    && t.name == target.name
                                    && t.kind == target.kind
                            }))
                        .then_some(i)
                    }) {
                        self.select_tab(index);
                        self.view = "data".into();
                        // 工作区只恢复资源上下文；该 tab 的数据库会话必须重新建立。
                        if self.session_id().is_none() {
                            self.navigation.queued_sections.insert(
                                self.tabs[self.active].id,
                                QueuedSection {
                                    target: target.clone(),
                                    section: section.into(),
                                },
                            );
                            self.action("open", backend);
                        } else {
                            self.workbench.section = section.into();
                        }
                    } else if self.target.is_some()
                        || self.task.is_some()
                        || self.operation_pending
                        || self.has_live_drafts()
                        || !self.tabs[self.active].query.text.is_empty()
                    {
                        self.action("new-tab", backend);
                        self.workbench.queued_target = Some(target.clone());
                        self.navigation.queued_sections.insert(
                            self.tabs[self.active].id,
                            QueuedSection {
                                target,
                                section: section.into(),
                            },
                        );
                        self.action("open", backend);
                    } else {
                        self.load_target(target, backend);
                        self.workbench.section = section.into();
                    }
                }
            }
            return true;
        }
        false
    }

    /// 返回 true 表示响应已由导航消费，必须在 tab 路由前调用。
    pub(super) fn navigation_response(&mut self, response: &Response, backend: &Backend) -> bool {
        if response.tag == "navigation.close" {
            if let Err(error) = &response.result {
                self.status = UiMessage::raw(error.to_string());
            }
            return true;
        }
        let Some(token) = response
            .tag
            .strip_prefix("navigation/")
            .and_then(|v| Uuid::parse_str(v).ok())
        else {
            return false;
        };
        if let Some(pending) = self.navigation.pending.remove(&token) {
            self.navigation_finish(pending, response.result.clone(), backend);
        } else if let Ok(value) = &response.result
            && let Some(session) = value.get("session_id")
        {
            let _ = backend.request(
                "navigation.close",
                "connection.close",
                json!({"session_id":session}),
            );
        }
        true
    }

    fn navigation_finish(&mut self, pending: Pending, result: Result<Value>, backend: &Backend) {
        match pending {
            Pending::DropOpen { token } => {
                let Some(operation) = self
                    .navigation
                    .drop_operation
                    .as_ref()
                    .filter(|operation| operation.confirmation.token == token)
                else {
                    return;
                };
                let drop = operation.confirmation.clone();
                match result {
                    Err(error) => {
                        self.navigation_drop_complete(token, "failed", error.to_string(), backend)
                    }
                    Ok(value) => {
                        let Some(session) = value
                            .get("session_id")
                            .filter(|session| !session.is_null())
                            .cloned()
                        else {
                            self.navigation_drop_complete(
                                token,
                                "failed",
                                value.to_string(),
                                backend,
                            );
                            return;
                        };
                        if session == drop.session {
                            self.navigation_drop_complete(
                                token,
                                "failed",
                                "独立会话无效".into(),
                                backend,
                            );
                            return;
                        }
                        let source_valid = self.navigation_drop_source_valid(&drop)
                            && self.navigation_drop_target_exists(&drop);
                        let operation = self.navigation.drop_operation.as_mut().unwrap();
                        operation.ddl_session = Some(session.clone());
                        if value.get("profile") != Some(&drop.profile) || !source_valid {
                            self.navigation_drop_end(
                                token,
                                "failed",
                                "连接配置已更改，请先重连".into(),
                                backend,
                            );
                        } else if operation.cancel_requested {
                            self.navigation_drop_end(token, "cancelled", String::new(), backend);
                        } else {
                            let sql = operation.sql.clone();
                            self.navigation_request(backend, Pending::DropStart { token }, "execution.start",
                                json!({"session_id":session,"workspace_id":self.workspace_id,"text":sql,"mysql_write":true}));
                        }
                    }
                }
            }
            Pending::DropClose { token } => {
                let Some(operation) = self
                    .navigation
                    .drop_operation
                    .as_mut()
                    .filter(|operation| operation.confirmation.token == token)
                else {
                    return;
                };
                operation.closing = false;
                match result {
                    Ok(_) => {
                        let (state, detail) = operation.terminal.take().unwrap();
                        self.navigation_drop_complete(token, &state, detail, backend);
                    }
                    Err(error) if error.code == "not_found" => {
                        let (state, detail) = operation.terminal.take().unwrap();
                        self.navigation_drop_complete(token, &state, detail, backend);
                    }
                    Err(error) => {
                        self.status = UiMessage::key("关闭删除专用连接失败，正在重试");
                        let drop = operation.confirmation.clone();
                        self.navigation_drop_detail(&drop, "错误详情", error.to_string());
                    }
                }
            }
            Pending::DropCancel { token } => {
                let Some(operation) = self
                    .navigation
                    .drop_operation
                    .as_mut()
                    .filter(|operation| operation.confirmation.token == token)
                else {
                    return;
                };
                operation.cancel_pending = false;
                if let Err(error) = result {
                    self.status = UiMessage::raw(error.to_string());
                }
            }
            Pending::DropStart { token } => {
                if self
                    .navigation
                    .drop_operation
                    .as_ref()
                    .is_none_or(|operation| operation.confirmation.token != token)
                {
                    return;
                }
                match result {
                    Ok(value) => {
                        if let Some(task) = value.get("task_id").and_then(Value::as_str) {
                            self.navigation.drop_operation.as_mut().unwrap().task =
                                Some(task.to_owned());
                            if let Some(confirmation) = self
                                .navigation
                                .drop_confirmation
                                .as_mut()
                                .filter(|confirmation| confirmation.token == token)
                            {
                                confirmation.task_started = true;
                            }
                            if self
                                .navigation
                                .drop_operation
                                .as_ref()
                                .unwrap()
                                .cancel_requested
                            {
                                self.navigation_drop_cancel_task(backend);
                            }
                            self.navigation_poll_drop(backend);
                        } else {
                            self.navigation_drop_end(
                                token,
                                "outcome_unknown",
                                value.to_string(),
                                backend,
                            );
                        }
                    }
                    Err(error) => {
                        let state = if matches!(
                            error.code.as_str(),
                            "outcome_unknown" | "disconnected" | "timeout"
                        ) {
                            "outcome_unknown"
                        } else {
                            "failed"
                        };
                        self.navigation_drop_end(token, state, error.to_string(), backend);
                    }
                }
            }
            Pending::DropPoll { token } => {
                let Some(operation) = self
                    .navigation
                    .drop_operation
                    .as_mut()
                    .filter(|operation| operation.confirmation.token == token)
                else {
                    return;
                };
                operation.polling = false;
                match result {
                    Ok(value) => {
                        let state = value.get("state").and_then(Value::as_str).unwrap_or("");
                        if !matches!(state, "queued" | "running" | "cancelling") {
                            let state = if matches!(
                                state,
                                "succeeded" | "failed" | "cancelled" | "outcome_unknown"
                            ) {
                                state
                            } else {
                                "outcome_unknown"
                            };
                            self.navigation_drop_end(token, state, value.to_string(), backend);
                        }
                    }
                    Err(error) => {
                        self.status = UiMessage::key("无法查询删除任务，正在重试");
                        let drop = operation.confirmation.clone();
                        self.navigation_drop_detail(&drop, "错误详情", error.to_string());
                    }
                }
            }
            Pending::CloseProfileSession {
                token,
                tab,
                session,
            } => {
                let Some(operation) = self
                    .navigation
                    .delete_operation
                    .as_ref()
                    .filter(|operation| operation.token == token)
                else {
                    return;
                };
                let id = operation.id;
                let result = match result {
                    Err(error) if error.code == "not_found" => Ok(Value::Null),
                    other => other,
                };
                match result {
                    Ok(_) => {
                        self.navigation_profile_session_closed(tab, &session);
                        self.navigation_profile_delete_unlock(id);
                        if self.navigation_profile_has_pending_work(id) {
                            self.navigation.delete_operation = None;
                            if let Some(confirmation) = self
                                .navigation
                                .delete_confirmation
                                .as_mut()
                                .filter(|confirmation| confirmation.token == token)
                            {
                                confirmation.busy = false;
                                confirmation.error = Some("该连接存在草稿或任务，请先处理".into());
                            }
                            self.status = UiMessage::key("该连接存在草稿或任务，请先处理");
                        } else {
                            for index in 0..self.tabs.len() {
                                if self.tabs[index].profile_id == Some(id) {
                                    if index == self.active {
                                        self.operation_pending = true;
                                    } else {
                                        self.tabs[index].runtime.operation_pending = true;
                                    }
                                }
                            }
                            self.navigation_profile_delete_next(backend);
                        }
                    }
                    Err(error) => {
                        self.navigation.delete_operation = None;
                        self.navigation_profile_delete_unlock(id);
                        if let Some(confirmation) = self
                            .navigation
                            .delete_confirmation
                            .as_mut()
                            .filter(|confirmation| confirmation.token == token)
                        {
                            confirmation.busy = false;
                            confirmation.error = Some(error.to_string());
                        }
                        self.status = UiMessage::raw(error.to_string());
                    }
                }
            }
            Pending::DeleteProfile { id, token } => {
                if self
                    .navigation
                    .delete_operation
                    .as_ref()
                    .is_none_or(|operation| operation.token != token || operation.id != id)
                {
                    return;
                }
                self.navigation.delete_operation = None;
                self.navigation_profile_delete_unlock(id);
                match result {
                    Ok(_) => {
                        self.profiles.retain(|profile| profile.id != id);
                        self.navigation_invalidate_profile(id);
                        // 删除后拒收此前发出的列表，并重新读取当前配置。
                        self.profile_list_token = None;
                        self.request(backend, "profile.list", "profile.list", json!({}));
                        if self
                            .navigation
                            .delete_confirmation
                            .as_ref()
                            .is_some_and(|confirmation| confirmation.token == token)
                        {
                            let confirmation = self.navigation.delete_confirmation.take().unwrap();
                            if let Some(index) =
                                self.tabs.iter().position(|tab| tab.id == confirmation.tab)
                            {
                                let workbench = if index == self.active {
                                    &mut self.workbench
                                } else {
                                    &mut self.tabs[index].runtime.workbench
                                };
                                if workbench.overlay.as_deref() == Some("profile-delete") {
                                    workbench.overlay = None;
                                }
                            }
                        }
                        self.status = UiMessage::key("连接配置已删除");
                    }
                    Err(error) => {
                        if let Some(confirmation) = self
                            .navigation
                            .delete_confirmation
                            .as_mut()
                            .filter(|confirmation| confirmation.token == token)
                        {
                            confirmation.busy = false;
                            confirmation.error = Some(error.to_string());
                        }
                        self.status = UiMessage::raw(error.to_string());
                    }
                }
            }
            Pending::Form { profile, secret } => {
                let mut current = self
                    .form_profile()
                    .ok()
                    .and_then(|p| serde_json::to_value(p).ok());
                let mut profile = profile;
                if let Some(value) = current.as_mut().and_then(Value::as_object_mut) {
                    value.remove("visible_databases");
                }
                if let Some(value) = profile.as_object_mut() {
                    value.remove("visible_databases");
                }
                let current_secret = if self.field("password").is_empty() {
                    Value::Null
                } else {
                    json!(self.field("password"))
                };
                self.navigation.form.loading = false;
                if current.as_ref() != Some(&profile) || current_secret != secret {
                    return;
                }
                match result {
                    Ok(value) => match serde_json::from_value::<ResourcePage>(value) {
                        Ok(page) => {
                            let form = &mut self.navigation.form;
                            form.candidates = page.resources.into_iter().map(|r| r.name).collect();
                            if form.all {
                                form.selected.clear();
                                form.selected.extend(form.candidates.iter().cloned());
                            }
                            form.loaded = true;
                            form.error = None;
                        }
                        Err(error) => self.navigation.form.error = Some(error.to_string()),
                    },
                    Err(error) => self.navigation.form.error = Some(error.to_string()),
                }
            }
            Pending::Tree {
                profile,
                database,
                session,
            } => {
                // 会话被关闭或配置范围改变后，迟到结果不得重建节点。
                let alive = self.tabs.iter().enumerate().any(|(i, tab)| {
                    tab.profile_id == Some(profile)
                        && if i == self.active {
                            self.session_id().is_some_and(|s| json!(s) == session)
                        } else {
                            tab.runtime
                                .session
                                .as_ref()
                                .and_then(|v| v.get("session_id"))
                                == Some(&session)
                        }
                });
                let allowed = self
                    .profiles
                    .iter()
                    .find(|p| p.id == profile)
                    .is_some_and(|p| {
                        database.as_ref().is_none_or(|d| {
                            p.visible_databases.as_ref().is_none_or(|v| v.contains(d))
                        })
                    });
                let Some(tree) = self.navigation.databases.get_mut(&profile) else {
                    return;
                };
                if let Some(database) = database {
                    let Some(node) = tree.nodes.get_mut(&database) else {
                        return;
                    };
                    node.loading = false;
                    if !alive || !allowed {
                        return;
                    }
                    match result.and_then(|v| {
                        serde_json::from_value::<ResourcePage>(v)
                            .map_err(|e| AppError::new("invalid_response", e.to_string()))
                    }) {
                        Ok(page) => {
                            node.resources = page.resources;
                            node.loaded = true;
                            node.error = None;
                        }
                        Err(e) => node.error = Some(e.to_string()),
                    }
                } else {
                    tree.loading = false;
                    if !alive || !allowed {
                        return;
                    }
                    match result.and_then(|v| {
                        serde_json::from_value::<ResourcePage>(v)
                            .map_err(|e| AppError::new("invalid_response", e.to_string()))
                    }) {
                        Ok(page) => {
                            tree.databases = page.resources;
                            tree.loaded = true;
                            tree.error = None;
                        }
                        Err(e) => tree.error = Some(e.to_string()),
                    }
                }
            }
            Pending::Redis {
                tab,
                generation,
                db,
            } => {
                let Some(index) = self.tabs.iter().position(|t| t.id == tab) else {
                    if let Ok(value) = result
                        && let Some(session) = value.get("session_id")
                    {
                        let _ = backend.request(
                            "navigation.close",
                            "connection.close",
                            json!({"session_id":session}),
                        );
                    }
                    return;
                };
                let old = self.active;
                self.switch_tab(index);
                if self.generation != generation {
                    if let Ok(value) = result
                        && let Some(session) = value.get("session_id")
                    {
                        let _ = backend.request(
                            "navigation.close",
                            "connection.close",
                            json!({"session_id":session}),
                        );
                    }
                } else {
                    self.operation_pending = false;
                    match result {
                        Ok(value) => {
                            let profile =
                                serde_json::from_value::<Profile>(value["profile"].clone()).ok();
                            let safe = !self.has_live_drafts()
                                && self.tabs[self.active].pending.is_null()
                                && self.workbench.inline.is_none()
                                && self.workbench.overlay.is_none()
                                && self.task.is_none()
                                && !self.workbench.cli_running;
                            if safe
                                && profile.as_ref().is_some_and(|p| p.redis_db == db)
                                && value.get("session_id").is_some()
                            {
                                if let Some(session) = self.session_id() {
                                    let _ = backend.request(
                                        "navigation.close",
                                        "connection.close",
                                        json!({"session_id":session}),
                                    );
                                }
                                self.generation = self.generation.wrapping_add(1);
                                self.session = Some(value);
                                self.profile = profile;
                                self.tabs[self.active].browse.redis_db = Some(db);
                                self.dirty = true;
                                self.workbench.sql_mode = None;
                                self.target = None;
                                self.resources.clear();
                                self.cursor = None;
                                self.scan_token = None;
                                self.grid = None;
                                self.schema = None;
                                self.edit_token = None;
                                self.results.clear();
                                self.result_id = None;
                                self.page_token = None;
                                self.page_history.clear();
                                self.large = None;
                                self.browse_result = false;
                                self.clear_cli_session();
                                self.action("list", backend);
                            } else {
                                if let Some(session) = value.get("session_id") {
                                    let _ = backend.request(
                                        "navigation.close",
                                        "connection.close",
                                        json!({"session_id":session}),
                                    );
                                }
                                self.status = UiMessage::key(if safe {
                                    "切换数据库返回了无效会话"
                                } else {
                                    "切换期间出现编辑或任务，已保留原会话"
                                });
                            }
                        }
                        Err(error) => self.status = UiMessage::raw(error.to_string()),
                    }
                }
                if old != index {
                    self.switch_tab(old);
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/navigation.rs"]
mod tests;
