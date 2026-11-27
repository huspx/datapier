use super::*;
use datapier_core::model::project::{ProjectLayout, ProjectQuery};
use std::collections::{BTreeMap, BTreeSet};

/// 共享查询身份同时包含目录，避免导入相同 UUID 的项目复用其他目录的草稿。
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct ProjectQueryIdentity {
    project: Uuid,
    root: String,
    query: Uuid,
    #[serde(default)]
    binding_group: String,
    #[serde(default)]
    query_snapshot: Value,
}

#[derive(Default)]
pub(super) struct ProjectLayoutState {
    pending: Option<LayoutLoad>,
    pub(super) groups: Vec<Vec<Uuid>>,
    execution: Option<ProjectExecution>,
    resume_text: bool,
    resume_current: bool,
}

enum ProjectExecuteIntent {
    Text(String),
    Current,
}

enum ProjectExecutePhase {
    Resolve,
    Connecting(Value),
    Recheck(Value, Value),
}

struct ProjectExecution {
    token: Uuid,
    tab: Uuid,
    identity: ProjectQueryIdentity,
    epoch: u64,
    text: String,
    cursor: usize,
    anchor: usize,
    intent: ProjectExecuteIntent,
    phase: ProjectExecutePhase,
}

struct LayoutLoad {
    token: Uuid,
    owner: Uuid,
    project: Uuid,
    root: String,
    manifest: Value,
    layout: ProjectLayout,
    queries: Vec<ProjectQuery>,
    texts: BTreeMap<Uuid, String>,
    draft: (bool, String, String),
    index: usize,
    offset: usize,
    buffer: String,
}
impl App {
    pub(super) fn project_layout_can_execute(&self) -> bool {
        self.tabs
            .get(self.active)
            .is_some_and(|tab| tab.project_query.is_some())
    }

    pub(super) fn project_layout_is_redis(&self) -> bool {
        self.tabs
            .get(self.active)
            .and_then(|tab| tab.project_query.as_ref())
            .is_some_and(|id| id.query_snapshot["language"] == "redis")
    }

    /// 每次显式执行都重新解析确认过的绑定；恢复调用只消费一次。
    pub(super) fn project_layout_execute_text(&mut self, text: &str, backend: &Backend) -> bool {
        if self.project_layout.resume_text {
            self.project_layout.resume_text = false;
            return false;
        }
        if !self.project_layout_can_execute() {
            return false;
        }
        self.start_project_execution(ProjectExecuteIntent::Text(text.to_owned()), backend);
        true
    }

    fn start_project_execution(&mut self, intent: ProjectExecuteIntent, backend: &Backend) {
        if self.project_layout.execution.is_some() || self.operation_pending || self.task.is_some()
        {
            self.status = i18n::UiMessage::key("项目查询正在处理，请稍候");
            return;
        }
        if !self.finish_inline(true) || self.has_live_drafts() {
            self.status = i18n::UiMessage::key("请先处理当前待提交变更");
            return;
        }
        let tab = &self.tabs[self.active];
        let Some(identity) = tab.project_query.clone() else {
            return;
        };
        if identity.binding_group.is_empty() || identity.query_snapshot.is_null() {
            self.status = i18n::UiMessage::key("请重新打开项目并应用布局，再执行查询");
            return;
        }
        if tab.query.text.trim().is_empty() {
            self.status = i18n::UiMessage::key("没有可执行的文本");
            return;
        }
        self.project_layout.execution = Some(ProjectExecution {
            token: Uuid::new_v4(),
            tab: tab.id,
            identity,
            epoch: self.query_edit_epoch,
            text: tab.query.text.clone(),
            cursor: tab.query.cursor,
            anchor: tab.query.anchor,
            intent,
            phase: ProjectExecutePhase::Resolve,
        });
        self.project_execution_resolve(backend);
    }

    fn project_execution_resolve(&mut self, backend: &Backend) {
        let Some(exec) = self.project_layout.execution.as_ref() else {
            return;
        };
        let id = &exec.identity;
        if let Err(error) = backend.request(format!("project-layout-execute/{}", exec.token), "project.query.resolve",
            json!({"project_id":id.project,"query_id":id.query,"binding_group":id.binding_group,"expected_root":id.root})) {
            self.project_layout.execution = None;
            self.status = i18n::UiMessage::raw(error.to_string());
        }
    }

    fn project_execution_context_matches(&self, exec: &ProjectExecution) -> bool {
        self.tabs.get(self.active).is_some_and(|tab| {
            tab.id == exec.tab
                && tab.project_query.as_ref() == Some(&exec.identity)
                && self.query_edit_epoch == exec.epoch
                && tab.query.text == exec.text
                && tab.query.cursor == exec.cursor
                && tab.query.anchor == exec.anchor
        })
    }

    fn project_execution_response(&mut self, response: &Response, backend: &Backend) {
        let Some(mut exec) = self.project_layout.execution.take() else {
            return;
        };
        if response.tag != format!("project-layout-execute/{}", exec.token) {
            self.project_layout.execution = Some(exec);
            return;
        }
        if !self.project_execution_context_matches(&exec) {
            self.status = i18n::UiMessage::key("查询或标签已更改，请重新执行");
            return;
        }
        let resolved = match &response.result {
            Ok(value)
                if value["root"] == exec.identity.root
                    && value["binding_group"] == exec.identity.binding_group
                    && value["query"] == exec.identity.query_snapshot =>
            {
                value.clone()
            }
            Ok(_) => {
                self.status = i18n::UiMessage::key("项目查询或目录已更改，请重新应用布局");
                return;
            }
            Err(error) => {
                self.status = i18n::UiMessage::raw(error.to_string());
                return;
            }
        };
        let Some(profile_id) = resolved["profile_id"]
            .as_str()
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            self.status = i18n::UiMessage::key("项目绑定缺少有效连接");
            return;
        };
        match &exec.phase {
            ProjectExecutePhase::Recheck(expected, session)
                if *expected == resolved && self.session_id().as_ref() == Some(session) =>
            {
                self.finish_project_execution(exec, backend);
            }
            ProjectExecutePhase::Recheck(..) | ProjectExecutePhase::Connecting(_) => {
                self.status = i18n::UiMessage::key("项目绑定在连接期间已更改，请重新执行");
            }
            ProjectExecutePhase::Resolve => {
                let database = resolved["database"].as_str().map(str::to_owned);
                let redis_db = resolved["redis_db"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok());
                let Some(profile) = self.profiles.iter().find(|p| p.id == profile_id).cloned()
                else {
                    self.status = i18n::UiMessage::key("绑定的连接不在当前连接列表，请刷新后重试");
                    return;
                };
                let is_redis = profile.provider == datapier_core::model::ProviderKind::Redis;
                let expected_database = database.as_deref().or_else(|| profile.default_database());
                let expected_redis_db = redis_db.unwrap_or_else(|| profile.redis_db());
                let already_connected = self.session_id().is_some()
                    && self.profile.as_ref().is_some_and(|p| p.id == profile_id)
                    && if is_redis {
                        self.tabs[self.active].browse.redis_db == Some(expected_redis_db)
                    } else {
                        self.database.as_deref() == expected_database
                    };
                if already_connected {
                    self.finish_project_execution(exec, backend);
                    return;
                }
                self.profile = Some(profile);
                self.database = database;
                self.tabs[self.active].browse.redis_db = redis_db;
                self.target = None;
                self.workbench.queued_target = None;
                exec.phase = ProjectExecutePhase::Connecting(resolved);
                self.project_layout.execution = Some(exec);
                self.action("open", backend);
                if !self.operation_pending {
                    self.project_layout.execution = None;
                }
            }
        }
    }

    /// 连接完成后再核对绑定，防止连接期间项目或绑定发生变化。
    pub(super) fn project_layout_connected(&mut self, backend: &Backend) {
        let Some(session) = self.session_id() else {
            self.project_layout_connection_failed();
            return;
        };
        let Some(exec) = self.project_layout.execution.as_mut() else {
            return;
        };
        if self
            .tabs
            .get(self.active)
            .is_none_or(|tab| tab.id != exec.tab)
        {
            return;
        }
        let ProjectExecutePhase::Connecting(expected) = &exec.phase else {
            return;
        };
        exec.phase = ProjectExecutePhase::Recheck(expected.clone(), session);
        self.view = "query".into();
        if self.is_redis() {
            self.workbench.cli = false;
        }
        self.project_execution_resolve(backend);
    }

    pub(super) fn project_layout_connection_failed(&mut self) {
        if self.project_layout.execution.as_ref().is_some_and(|exec| {
            self.tabs
                .get(self.active)
                .is_some_and(|tab| tab.id == exec.tab)
                && matches!(&exec.phase, ProjectExecutePhase::Connecting(_))
        }) {
            self.project_layout.execution = None;
        }
    }

    fn finish_project_execution(&mut self, exec: ProjectExecution, backend: &Backend) {
        if !self.project_execution_context_matches(&exec) {
            self.status = i18n::UiMessage::key("查询或标签已更改，请重新执行");
            return;
        }
        match exec.intent {
            ProjectExecuteIntent::Text(text) => {
                self.project_layout.resume_text = true;
                self.execute_query_text(text, backend);
                self.project_layout.resume_text = false;
            }
            ProjectExecuteIntent::Current => {
                self.project_layout.resume_current = true;
                self.action("current", backend);
                self.project_layout.resume_current = false;
            }
        }
    }
}

impl App {
    pub(super) fn project_layout_action(&mut self, name: &str, backend: &Backend) -> bool {
        if name == "current" && self.project_layout_can_execute() {
            if self.project_layout.resume_current {
                self.project_layout.resume_current = false;
                return false;
            }
            self.start_project_execution(ProjectExecuteIntent::Current, backend);
            return true;
        }
        if name != "project-layout-apply" {
            return false;
        }
        if self.project_layout.pending.is_some() || self.workbench.project.is_pending() {
            self.status = i18n::UiMessage::key("项目正在加载，请稍后应用布局");
            return true;
        }
        let result = self.prepare_project_layout();
        match result {
            Ok(load) => {
                self.project_layout.pending = Some(load);
                self.project_layout_next(backend);
            }
            Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
        }
        true
    }

    fn prepare_project_layout(&self) -> Result<LayoutLoad> {
        let project = self
            .workbench
            .project
            .project
            .as_ref()
            .ok_or_else(|| AppError::invalid("请先打开项目"))?;
        let manifest = project["manifest"].clone();
        let project_id = serde_json::from_value::<Uuid>(manifest["id"].clone())?;
        let handle = serde_json::from_value::<Uuid>(project["project_id"].clone())?;
        if project_id != handle {
            return Err(AppError::invalid("项目身份不一致"));
        }
        let root = project["root"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| AppError::invalid("项目目录不可用"))?
            .to_owned();
        let layout: ProjectLayout = serde_json::from_value(manifest["layout"].clone())?;
        let queries: Vec<ProjectQuery> = serde_json::from_value(manifest["queries"].clone())?;
        if !layout.resources.is_empty() {
            return Err(AppError::invalid("项目资源引用缺少资源描述，无法应用布局"));
        }
        let by_id: BTreeMap<_, _> = queries.into_iter().map(|query| (query.id, query)).collect();
        let mut seen = BTreeSet::new();
        for id in &layout.tabs {
            if !seen.insert(*id) || !by_id.contains_key(id) {
                return Err(AppError::invalid("布局标签重复或引用不存在的查询"));
            }
        }
        if layout.tabs.is_empty() {
            return Err(AppError::invalid("项目布局没有标签"));
        }
        let mut grouped = BTreeSet::new();
        for group in &layout.splits {
            if group.is_empty() {
                return Err(AppError::invalid("项目布局包含空分栏"));
            }
            for id in group {
                if !seen.contains(id) || !grouped.insert(*id) {
                    return Err(AppError::invalid("分栏引用不在标签中或重复"));
                }
            }
        }
        if layout.columns.keys().any(|id| !seen.contains(id)) {
            return Err(AppError::invalid("列设置引用不在布局中的查询"));
        }
        let queries: Vec<_> = layout.tabs.iter().map(|id| by_id[id].clone()).collect();
        let mut texts = BTreeMap::new();
        // 编辑中的项目文件优先于已保存快照，不能在应用布局时丢弃草稿。
        let draft = (
            self.workbench.project.unsaved,
            self.field(&self.project_key("query-path")).to_owned(),
            self.field(&self.project_key("query-text")).to_owned(),
        );
        if draft.0
            && let Some(query) = queries.iter().find(|q| q.path == draft.1)
        {
            texts.insert(query.id, draft.2.clone());
        }
        Ok(LayoutLoad {
            token: Uuid::new_v4(),
            owner: self.tabs[self.active].id,
            project: project_id,
            root,
            manifest,
            layout,
            queries,
            texts,
            draft,
            index: 0,
            offset: 0,
            buffer: String::new(),
        })
    }

    fn project_layout_next(&mut self, backend: &Backend) {
        let Some(load) = self.project_layout.pending.as_mut() else {
            return;
        };
        while load.index < load.queries.len()
            && load.texts.contains_key(&load.queries[load.index].id)
        {
            load.index += 1;
        }
        if load.index == load.queries.len() {
            let load = self.project_layout.pending.take().unwrap();
            self.finish_project_layout(load);
            return;
        }
        let query = &load.queries[load.index];
        let result = backend.request(
            format!("project-layout/{}", load.token),
            "project.file.read",
            json!({"project_id":load.project,"path":query.path,"offset":load.offset,"limit":65536}),
        );
        if let Err(error) = result {
            self.project_layout.pending = None;
            self.status = i18n::UiMessage::raw(error.to_string());
        }
    }

    pub(super) fn project_layout_response(
        &mut self,
        response: &Response,
        backend: &Backend,
    ) -> bool {
        if response.tag.starts_with("project-layout-execute/") {
            self.project_execution_response(response, backend);
            return true;
        }
        let Some(token) = response.tag.strip_prefix("project-layout/") else {
            return false;
        };
        let Some(load) = self.project_layout.pending.as_mut() else {
            return true;
        };
        if Uuid::parse_str(token).ok() != Some(load.token) {
            return true;
        }
        let value = match &response.result {
            Ok(value) => value,
            Err(error) => {
                self.project_layout.pending = None;
                self.status = i18n::UiMessage::raw(error.to_string());
                return true;
            }
        };
        let Some(text) = value["text"].as_str() else {
            self.project_layout.pending = None;
            self.status = i18n::UiMessage::key("项目文件响应缺少正文");
            return true;
        };
        load.buffer.push_str(text);
        if let Some(next) = value["next"].as_u64() {
            if next as usize != load.offset + text.len() || next as usize <= load.offset {
                self.project_layout.pending = None;
                self.status = i18n::UiMessage::key("项目文件分页无效");
                return true;
            }
            load.offset = next as usize;
        } else {
            load.texts.insert(
                load.queries[load.index].id,
                std::mem::take(&mut load.buffer),
            );
            load.index += 1;
            load.offset = 0;
        }
        self.project_layout_next(backend);
        true
    }

    fn finish_project_layout(&mut self, mut load: LayoutLoad) {
        let Some(owner) = self.tabs.iter().position(|t| t.id == load.owner) else {
            self.status = i18n::UiMessage::key("项目标签已关闭，布局未应用");
            return;
        };
        let current = if owner == self.active {
            &self.workbench.project
        } else {
            &self.tabs[owner].runtime.workbench.project
        };
        if !current
            .project
            .as_ref()
            .is_some_and(|p| p["root"] == load.root && p["manifest"] == load.manifest)
        {
            self.status = i18n::UiMessage::key("项目在加载期间已更改，布局未应用");
            return;
        }
        let prefix = format!("project-{}-", load.owner);
        if current.unsaved != load.draft.0
            || self.field(&format!("{prefix}query-path")) != load.draft.1
            || self.field(&format!("{prefix}query-text")) != load.draft.2
        {
            self.status = i18n::UiMessage::key("项目查询草稿在加载期间已更改，布局未应用");
            return;
        }
        let group = self.field(&format!("{prefix}group"));
        let group = if group.is_empty() { "default" } else { group }.to_owned();
        let active_id = self.tabs[self.active].id;
        let mut mapping = BTreeMap::new();
        for query in &load.queries {
            let identity = ProjectQueryIdentity {
                project: load.project,
                root: load.root.clone(),
                query: query.id,
                binding_group: group.clone(),
                query_snapshot: serde_json::to_value(query).expect("ProjectQuery 可序列化"),
            };
            let index = self.tabs.iter().position(|tab| {
                tab.project_query.as_ref().is_some_and(|old| {
                    old.project == identity.project
                        && old.root == identity.root
                        && old.query == identity.query
                })
            });
            let index = if let Some(index) = index {
                index
            } else {
                let runtime = TabRuntime {
                    view: "query".into(),
                    ..TabRuntime::default()
                };
                self.tabs.push(Tab {
                    title: query.name.clone(),
                    query: Editor::new(
                        load.texts
                            .remove(&query.id)
                            .expect("完整读取查询后才应用布局"),
                    ),
                    cli_query: Editor::default(),
                    profile_id: None,
                    id: Uuid::new_v4(),
                    layout: Vec::new(),
                    project_query: Some(identity.clone()),
                    project_columns: None,
                    browse: BrowseSettings::default(),
                    schema_draft: None,
                    replace_draft: None,
                    pending: Value::Null,
                    workbench_draft: Value::Null,
                    pending_live: false,
                    runtime,
                });
                self.tabs.len() - 1
            };
            let tab = &mut self.tabs[index];
            tab.project_query = Some(identity);
            tab.title = query.name.clone();
            tab.project_columns = load.layout.columns.get(&query.id).cloned();
            tab.layout.clear();
            mapping.insert(query.id, tab.id);
        }
        let ordered: Vec<_> = load.layout.tabs.iter().map(|id| mapping[id]).collect();
        let positions: BTreeMap<_, _> =
            ordered.iter().enumerate().map(|(i, id)| (*id, i)).collect();
        self.tabs
            .sort_by_key(|tab| positions.get(&tab.id).map_or((1, 0), |i| (0, *i)));
        self.active = self
            .tabs
            .iter()
            .position(|tab| tab.id == active_id)
            .unwrap();
        let mut column_error = false;
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            if !positions.contains_key(&tab.id) {
                continue;
            }
            let Some(names) = tab.project_columns.as_ref() else {
                continue;
            };
            let grid = if index == self.active {
                self.grid.as_mut()
            } else {
                tab.runtime.grid.as_mut()
            };
            if let Some(grid) = grid {
                column_error |= !set_visible_columns(grid, names);
            }
        }
        self.project_layout.groups = load
            .layout
            .splits
            .iter()
            .map(|group| group.iter().map(|id| mapping[id]).collect())
            .collect();
        let first = self
            .tabs
            .iter()
            .position(|tab| tab.id == ordered[0])
            .unwrap();
        self.switch_tab(first);
        self.view = "query".into();
        self.focus_current_tab();
        self.dirty = true;
        self.status = i18n::UiMessage::key(if column_error {
            "布局已应用；部分结果列名不匹配或重复，列设置未应用"
        } else {
            "布局已应用；未自动连接或执行查询"
        });
    }

    pub(super) fn apply_project_columns(&mut self, grid: &mut GridState) {
        let Some(names) = self.tabs[self.active].project_columns.as_ref() else {
            return;
        };
        if !set_visible_columns(grid, names) {
            self.status = i18n::UiMessage::key("项目列设置与结果列不匹配或列名重复，未应用");
        }
    }
}

fn set_visible_columns(grid: &mut GridState, names: &[String]) -> bool {
    let mut seen = BTreeSet::new();
    let ambiguous = grid
        .page
        .result
        .columns
        .iter()
        .any(|col| !seen.insert(col.name.as_str()));
    if ambiguous || names.iter().any(|name| !seen.contains(name.as_str())) {
        return false;
    }
    for (setting, column) in grid.settings.iter_mut().zip(&grid.page.result.columns) {
        setting.hidden = !names.contains(&column.name);
    }
    grid.project_visibility_changed();
    true
}
#[cfg(test)]
#[path = "../tests/unit/project_layout.rs"]
mod tests;

impl App {
    pub(super) fn project_split_editor_key(&self) -> Option<String> {
        self.tabs.get(self.active)?;
        if self.view == "query" && !self.browse_result && active_group(self).is_some() {
            Some(self.sql_editor_key(self.active, true))
        } else {
            None
        }
    }

    /// 仅接受当前可见分栏的事件，拒绝切换或关闭后迟到的编辑事件。
    pub(super) fn resolve_project_editor(&mut self, name: String) -> Option<String> {
        let Some(id) = name.strip_prefix("project-query:") else {
            return Some(name);
        };
        let id = Uuid::parse_str(id).ok()?;
        if self.view != "query"
            || self.browse_result
            || !self.modal_allows_field("query")
            || !active_group(self).is_some_and(|group| group.contains(&id))
        {
            return None;
        }
        let index = self.tabs.iter().position(|tab| tab.id == id)?;
        if index != self.active {
            self.select_tab(index);
        }
        (self.tabs.get(self.active).is_some_and(|tab| tab.id == id)).then(|| "query".into())
    }
}

pub(super) fn active_group(app: &App) -> Option<&[Uuid]> {
    let active = app.tabs.get(app.active)?.id;
    app.project_layout
        .groups
        .iter()
        .find(|group| group.len() > 1 && group.contains(&active))
        .map(Vec::as_slice)
}
