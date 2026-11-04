//! 应用初始状态与字段路由。
use super::*;

impl App {
    pub(super) fn field(&self, name: &str) -> &str {
        if name == "cli_text" {
            self.tabs
                .get(self.active)
                .map_or("", |tab| tab.cli_query.text.as_str())
        } else if name == "cell" {
            &self.workbench.cell_editor.text
        } else if name == "row_values" {
            &self.workbench.sql_forms.row_json.text
        } else {
            self.fields.get(name).map(|x| x.text.as_str()).unwrap_or("")
        }
    }
    pub(super) fn editor(&mut self, name: &str) -> Option<&mut Editor> {
        if name == "query" {
            self.tabs.get_mut(self.active).map(|x| &mut x.query)
        } else if name == "cli_text" {
            self.tabs.get_mut(self.active).map(|tab| &mut tab.cli_query)
        } else if name == "cell" {
            Some(&mut self.workbench.cell_editor)
        } else if name == "row_values" || name.starts_with("sql-form-") {
            self.workbench.form_editor(name)
        } else {
            self.fields.get_mut(name)
        }
    }
    pub(super) fn field_set(&mut self, name: &str, text: impl Into<String>) {
        if self
            .workbench
            .field_error
            .as_ref()
            .is_some_and(|error| error.field == name)
        {
            self.workbench.field_error = None;
        }
        let text = text.into();
        let editor = Editor::new(text);
        if name == "cli_text" {
            if let Some(tab) = self.tabs.get_mut(self.active) {
                tab.cli_query = editor;
            }
        } else if name == "cell" {
            self.workbench.cell_editor = editor;
        } else if name == "row_values" {
            self.workbench.sql_forms.row_json = editor;
        } else {
            self.fields.insert(name.into(), editor);
        }
        if name == "row_search" {
            self.sync_grid_visible_rows();
        }
    }
    pub(super) fn has_live_drafts(&self) -> bool {
        self.grid.as_ref().is_some_and(|g| !g.edits().is_empty())
            || !self.pending_rows.is_empty()
            || !self.redis_changes.is_empty()
            || self
                .tabs
                .get(self.active)
                .is_some_and(|tab| self.redis_key_delete_tab_pending(tab.id))
            || self.paste_preview.is_some()
    }
    pub(super) fn changed_field(&mut self, name: &str) {
        if self
            .workbench
            .field_error
            .as_ref()
            .is_some_and(|error| error.field == name)
        {
            self.workbench.field_error = None;
        }
        if name == "row_search" {
            self.sync_grid_visible_rows();
        }
        if matches!(
            name,
            "name"
                | "host"
                | "port"
                | "user"
                | "password"
                | "provider"
                | "tls"
                | "ca"
                | "cert"
                | "key"
                | "database"
                | "redis_db"
        ) {
            self.navigation_form_changed();
        }
        if name == "query" {
            self.query_edit_epoch = self.query_edit_epoch.wrapping_add(1);
            self.workbench.query_ui.menu_open = false;
            self.workbench.query_ui.executed = false;
        }
        self.workbench.form_changed(name);
        self.dirty = true;
        // 查询文本失效时清除旧结果；基表数据继续由资源浏览持有，不能随查询草稿丢弃。
        if name == "query" && !self.browse_result && !self.is_redis() {
            self.grid = None;
            self.results.clear();
            self.result_id = None;
            self.page_history.clear();
            self.page_token = None;
            self.large = None;
        }
        if matches!(name, "pattern" | "type") {
            self.scan_token = None;
            self.cursor = None;
            self.workbench.scan_dirty = true;
        }
    }
    pub(super) fn new(workspace_id: String) -> Self {
        let mut fields = BTreeMap::new();
        for (name, value) in connections::PROFILE_DEFAULTS.iter().copied().chain([
            ("pattern", "*"),
            ("type", ""),
            ("path", ""),
            ("format", "csv"),
            ("mapping", "{}"),
            ("filter", "[]"),
            ("sort", "[]"),
            ("operation", "set_string"),
            ("redis_value", ""),
            ("redis_field", ""),
            ("redis_member", ""),
            ("redis_index", "0"),
            ("redis_score", "0"),
            ("redis_key", ""),
            ("redis_ttl", ""),
            ("column", "0"),
            ("width", "160"),
            ("object_filter", ""),
            ("history_filter", ""),
            ("row_search", ""),
        ]) {
            fields.insert(name.into(), Editor::new(value));
        }
        fields.insert("null_marker".into(), Editor::new("\\N"));
        fields.insert("backslash_escape".into(), Editor::new("true"));
        fields.insert("binary_columns".into(), Editor::new("[]"));
        Self {
            workspace_id,
            revision: 0,
            workspace_loaded: false,
            saving: false,
            save_blocked: false,
            copy_pending: None,
            dirty: false,
            restore_focus_pending: false,
            workspace_interacted: false,
            tabs: vec![Tab {
                title: "查询 1".into(),
                query: Editor::new(""),
                cli_query: Editor::default(),
                profile_id: None,
                id: Uuid::new_v4(),
                layout: Vec::new(),
                browse: BrowseSettings::default(),
                pending: Value::Null,
                pending_live: false,
                runtime: TabRuntime::default(),
            }],
            active: 0,
            fields,
            focus: "query".into(),
            profiles: vec![],
            profile: None,
            empty_context: None,
            profile_list_token: None,
            server_list_token: None,
            profile_saves: Default::default(),
            form_id: None,
            form_editing: false,
            archery: connections::archery::Wizard::default(),
            session: None,
            resources: vec![],
            cursor: None,
            scan_token: None,
            database: None,
            target: None,
            task: None,
            operation_pending: false,
            results: Vec::new(),
            result_id: None,
            page_history: Vec::new(),
            page_token: None,
            grid: None,
            schema: None,
            edit_token: None,
            pending_rows: Vec::new(),
            redis_changes: Vec::new(),
            redis_changes_revision: 0,
            large: None,
            preview: None,
            preview_pending: None,
            browse_result: false,
            generation: 0,
            current_pending: None,
            file_open_pending: None,
            query_edit_epoch: 0,
            paste_preview: None,
            status: "正在连接 Engine…".into(),
            detail: String::new(),
            detail_context: DetailContext::default(),
            view: "query".into(),
            workbench: Workbench::default(),
            last_poll: Instant::now(),
            viewport_width: 0,
            cli_share: 0.35,
            navigation: navigation::NavigationState::default(),
            icon_blobs: None,
            settings: AppSettings::default(),
            settings_epoch: 0,
            settings_save_token: None,
            settings_unsaved: false,
            tab_close: None,
            change_preview: change_preview::PreviewState::default(),
            redis_keys: Default::default(),
            redis_key_delete: Default::default(),
            last_settings_poll: Instant::now(),
        }
    }
}
