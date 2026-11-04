//! 工作区快照、标签运行态交换与退出保存。
use super::*;

impl App {
    pub(super) fn request_quit(&mut self, backend: &Backend) -> bool {
        if !self.finish_inline(true) {
            return false;
        }
        if !self.workspace_loaded && self.dirty {
            self.status = "工作区尚未加载，无法保存".into();
            return false;
        }
        self.save_workspace(backend);
        true
    }

    // 持久化编辑意图和布局，而非可继续提交的数据库基线；恢复后必须刷新并重新确认。
    pub(super) fn workspace_snapshot(&mut self) -> Workspace {
        self.capture_browse();
        let key_intents: BTreeMap<_, _> = self
            .tabs
            .iter()
            .filter_map(|tab| {
                self.redis_key_delete_snapshot(tab.id)
                    .map(|intent| (tab.id, intent))
            })
            .collect();
        for (index, tab) in self.tabs.iter_mut().enumerate() {
            let (target, grid, rows, redis, schema) = if index == self.active {
                (
                    &self.target,
                    &self.grid,
                    &self.pending_rows,
                    &self.redis_changes,
                    &self.schema,
                )
            } else {
                (
                    &tab.runtime.target,
                    &tab.runtime.grid,
                    &tab.runtime.pending_rows,
                    &tab.runtime.redis_changes,
                    &tab.runtime.schema,
                )
            };
            if let Some(g) = grid {
                tab.layout = g.settings.clone();
            }
            if let Some(intent) = key_intents.get(&tab.id) {
                tab.pending = intent.clone();
                tab.pending_live = true;
            } else if grid.as_ref().is_some_and(|g| !g.edits().is_empty())
                || !rows.is_empty()
                || !redis.is_empty()
            {
                let mut changes = rows.clone();
                let mut fallback = Vec::new();
                if let Some(g) = grid
                    && !g.edits().is_empty()
                {
                    let keys: Vec<String> = schema
                        .as_ref()
                        .and_then(|s| s.get("keys"))
                        .and_then(Value::as_array)
                        .and_then(|a| a.first())
                        .and_then(|v| serde_json::from_value(v.clone()).ok())
                        .unwrap_or_default();
                    match g.changes(&keys) {Ok(updates)=>changes.extend(updates),Err(_)=>fallback=g.edits().iter().map(|(cell,after)|json!({"row":cell.row,"column":cell.column,"before_row":g.original_row(cell.row),"after":after})).collect()}
                }
                tab.pending = json!({"target":target,"rows":changes,"redis":redis,"rows_include_edits":fallback.is_empty(),"edits":fallback,"requires_refresh":true});
                tab.pending_live = true;
            } else if tab.pending_live {
                tab.pending = Value::Null;
                tab.pending_live = false;
            }
        }
        Workspace {
            tabs: self
                .tabs
                .iter()
                .map(|tab| {
                    let redis = self.profiles.iter().any(|profile| {
                        Some(profile.id) == tab.profile_id
                            && profile.provider == datapier_core::model::ProviderKind::Redis
                    });
                    // 认证命令不进入工作区；只过滤序列化副本，保留内存输入供用户修改。
                    // 同时覆盖旧版将 Redis CLI 文本保存在 query 中的工作区。
                    let saved_editor = |editor: &Editor| {
                        if redis
                            && datapier_core::providers::redis_contains_auth(&editor.text)
                                .unwrap_or(true)
                        {
                            Editor::default()
                        } else {
                            editor.clone()
                        }
                    };
                    Tab {
                        title: tab.title.clone(),
                        query: saved_editor(&tab.query),
                        cli_query: saved_editor(&tab.cli_query),
                        profile_id: tab.profile_id,
                        id: tab.id,
                        layout: tab.layout.clone(),
                        browse: tab.browse.clone(),
                        pending: tab.pending.clone(),
                        pending_live: tab.pending_live,
                        runtime: TabRuntime::default(),
                    }
                })
                .collect(),
            active: self.active,
            pending: Value::Null,
            navigation_expanded: self.navigation.expanded,
            empty_context: if self.tabs.is_empty() {
                self.empty_context.clone()
            } else {
                None
            },
        }
    }
    // 保存携带已知 revision；失败后阻止自动重试，避免覆盖另一个 UI 的修改。
    // 请求期间的新修改会再次置 dirty，待确认后按新 revision 保存。
    pub(super) fn save_workspace(&mut self, backend: &Backend) {
        if !self.workspace_loaded || self.saving || self.save_blocked || !self.dirty {
            return;
        }
        let state = self.workspace_snapshot();
        match backend.request("workspace.save", "workspace.save", json!({"workspace_id":self.workspace_id,"expected_revision":self.revision,"state":state})) {Ok(())=>{self.saving=true;self.dirty=false;},Err(e)=>{self.status=i18n::UiMessage::raw(e.to_string());self.save_blocked=true;}}
    }
    pub(super) fn save_workspace_copy(&mut self, backend: &Backend) {
        if self.saving || self.copy_pending.is_some() {
            self.status = "工作区保存仍在进行".into();
            return;
        }
        let state = self.workspace_snapshot();
        let id = format!("{}-copy-{}", self.workspace_id, Uuid::new_v4());
        match backend.request(
            "workspace.copy",
            "workspace.save",
            json!({"workspace_id":id,"expected_revision":0,"state":state}),
        ) {
            Ok(()) => {
                self.copy_pending = Some(id);
                self.status = "正在另存工作区副本…".into();
            }
            Err(e) => self.status = i18n::UiMessage::raw(e.to_string()),
        }
    }
    pub(super) fn switch_tab(&mut self, next: usize) {
        if next == self.active || next >= self.tabs.len() {
            return;
        }
        self.capture_browse();
        macro_rules! swap_state {
            ($field:ident) => {
                std::mem::swap(&mut self.$field, &mut self.tabs[self.active].runtime.$field);
                std::mem::swap(&mut self.$field, &mut self.tabs[next].runtime.$field);
            };
        }
        swap_state!(profile);
        swap_state!(session);
        swap_state!(resources);
        swap_state!(cursor);
        swap_state!(scan_token);
        swap_state!(database);
        swap_state!(target);
        swap_state!(task);
        swap_state!(operation_pending);
        swap_state!(results);
        swap_state!(result_id);
        swap_state!(page_history);
        swap_state!(page_token);
        swap_state!(grid);
        swap_state!(schema);
        swap_state!(edit_token);
        swap_state!(pending_rows);
        swap_state!(redis_changes);
        swap_state!(redis_changes_revision);
        swap_state!(large);
        swap_state!(preview);
        swap_state!(preview_pending);
        swap_state!(paste_preview);
        swap_state!(detail);
        swap_state!(detail_context);
        swap_state!(browse_result);
        swap_state!(generation);
        swap_state!(current_pending);
        swap_state!(file_open_pending);
        swap_state!(query_edit_epoch);
        swap_state!(workbench);
        swap_state!(view);
        self.active = next;
        self.restore_browse();
        if self.profile.is_none() {
            self.profile = self.tabs[next]
                .profile_id
                .and_then(|id| self.profiles.iter().find(|p| p.id == id).cloned());
        }
        if self.view.is_empty() {
            self.view = if self.is_redis() { "data" } else { "query" }.into();
        }
        if self.detail.is_empty() && !self.tabs[next].pending.is_null() && self.edit_token.is_none()
        {
            self.detail = format!(
                "恢复的编辑意图（基线已失效，请刷新并重新确认）：{}",
                self.tabs[next].pending
            );
            self.set_recovery_detail_context();
        }
    }
    pub(super) fn migrate_cli_queries(&mut self) {
        for tab in &mut self.tabs {
            if tab.cli_query.text.is_empty()
                && !tab.query.text.is_empty()
                && self.profiles.iter().any(|profile| {
                    Some(profile.id) == tab.profile_id
                        && profile.provider == datapier_core::model::ProviderKind::Redis
                })
            {
                tab.cli_query = std::mem::take(&mut tab.query);
                self.dirty = true;
                tab.browse.cli_visible = true;
            }
        }
    }

    pub(super) fn current_redis_db(&self) -> u32 {
        self.tabs
            .get(self.active)
            .and_then(|tab| tab.browse.redis_db)
            .or_else(|| {
                self.empty_context
                    .as_ref()
                    .filter(|context| {
                        self.profile
                            .as_ref()
                            .is_some_and(|p| p.id == context.profile_id)
                    })
                    .and_then(|context| context.redis_db)
            })
            .unwrap_or_else(|| self.profile.as_ref().map_or(0, Profile::redis_db))
    }

    pub(super) fn capture_empty_context(&mut self) {
        self.empty_context = self.profile.as_ref().map(|profile| EmptyConnectionContext {
            profile_id: profile.id,
            database: self.database.clone(),
            redis_db: self.is_redis().then(|| self.current_redis_db()),
        });
    }

    pub(super) fn restore_empty_context(&mut self, clear_missing: bool) {
        if !self.tabs.is_empty() {
            return;
        }
        let profile = self.empty_context.as_ref().and_then(|context| {
            self.profiles
                .iter()
                .find(|profile| profile.id == context.profile_id)
                .cloned()
        });
        self.database = profile.as_ref().and_then(|_| {
            self.empty_context
                .as_ref()
                .and_then(|context| context.database.clone())
        });
        self.profile = profile;
        // 配置列表确认删除后清除悬空引用；首次工作区先于配置到达时保留待恢复目标。
        if clear_missing && self.profile.is_none() {
            self.empty_context = None;
        }
    }

    pub(super) fn empty_context_action(&mut self, name: &str, backend: &Backend) -> bool {
        if !self.tabs.is_empty() || self.profile.is_none() {
            return false;
        }
        if let Some(open) = name.strip_prefix("redis-db-menu-open:") {
            self.navigation.redis_db_menu = self.is_redis() && open == "true";
            return true;
        }
        if name.starts_with("redis-db:") && self.is_redis() {
            if name
                .strip_prefix("redis-db:")
                .and_then(|db| db.parse::<u32>().ok())
                == Some(self.current_redis_db())
            {
                self.navigation.redis_db_menu = false;
                return true;
            }
            self.create_query_tab();
            self.navigation_action(name, backend);
            return true;
        }
        if name == "open" {
            self.create_query_tab();
            self.action(name, backend);
            return true;
        }
        if name == "edit-profile" {
            let id = self.profile.as_ref().unwrap().id;
            self.edit_profile_after_close(id, false, backend);
            return true;
        }
        false
    }

    pub(super) fn capture_browse(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        let browse = BrowseSettings {
            pattern: self.field("pattern").into(),
            kind: self.field("type").into(),
            filter: self.field("filter").into(),
            sort: self.field("sort").into(),
            object_filter: self.field("object_filter").into(),
            row_search: self.field("row_search").into(),
            cli_visible: self.workbench.cli,
            database: self.database.clone(),
            target: self.target.clone(),
            redis_db: self.tabs[self.active].browse.redis_db.or_else(|| {
                self.profile
                    .as_ref()
                    .filter(|p| p.provider == datapier_core::model::ProviderKind::Redis)
                    .map(|p| p.redis_db())
            }),
        };
        self.tabs[self.active].browse = browse;
    }
    pub(super) fn restore_browse(&mut self) {
        if self.tabs.is_empty() {
            return;
        }
        let browse = self.tabs[self.active].browse.clone();
        self.field_set("pattern", browse.pattern);
        self.field_set("type", browse.kind);
        self.field_set("filter", browse.filter);
        self.field_set("sort", browse.sort);
        self.field_set("object_filter", browse.object_filter);
        self.field_set("row_search", browse.row_search);
        self.workbench.cli = browse.cli_visible;
        self.database = browse.database;
        self.target = browse.target;
    }
    // 用户选择 tab 要回到其内容；后台响应的临时切换仍只交换 runtime。
    pub(super) fn select_tab(&mut self, next: usize) {
        if next >= self.tabs.len() || !self.finish_inline(true) {
            return;
        }
        self.close_result_info(false);
        if next != self.active {
            self.archery.open_continuation = None;
            self.change_preview = change_preview::PreviewState::default();
        }
        self.workbench.overlay.take_if(|kind| kind == "history");
        self.switch_tab(next);
        self.workbench.overlay.take_if(|kind| kind == "history");
        self.focus_current_tab();
        self.dirty = true;
    }
    /// 用户切换需同时恢复键盘路由与宿主字段；后台临时 switch_tab 不触碰焦点。
    pub(super) fn focus_current_tab(&mut self) {
        let (focus, key) = if self.is_redis() && self.workbench.cli && self.target.is_none() {
            ("cli_text", "input-cli_text")
        } else if !self.is_redis() && self.target.is_none() {
            ("query", "query-editor")
        } else {
            ("grid", "grid")
        };
        self.focus = focus.into();
        self.workbench.focus_request = Some(key.into());
    }
}
// 输入已关闭时无法再向用户询问：等待保存确认，原工作区失败则尝试另存副本。
// 如果保存或副本仍无法确认，返回错误而不是将退出视为成功。
pub(super) fn finish_after_input_closed(app: &mut App, backend: &mut Backend) -> Result<()> {
    let mut copy_attempted = false;
    loop {
        while let Some(reply) = backend.receive() {
            app.routed_response(reply, backend);
        }
        if app.save_blocked {
            if app.copy_pending.is_none() {
                if copy_attempted {
                    return Err(AppError::new(
                        "workspace_save",
                        app.status.render().into_owned(),
                    ));
                }
                copy_attempted = true;
                app.save_workspace_copy(backend);
                if app.copy_pending.is_none() {
                    return Err(AppError::new(
                        "workspace_save",
                        app.status.render().into_owned(),
                    ));
                }
            }
        } else if app.dirty {
            if app.workspace_loaded {
                app.save_workspace(backend);
            } else if backend.is_closed() {
                return Err(AppError::new(
                    "workspace_save",
                    "工作区未加载，无法保存草稿",
                ));
            }
        } else if !app.saving && app.copy_pending.is_none() {
            return Ok(());
        }
        let Some(reply) = backend.receive_blocking() else {
            return Err(AppError::new(
                "workspace_save",
                "Engine 连接已关闭，工作区保存未确认",
            ));
        };
        app.routed_response(reply, backend);
    }
}
