use super::*;

mod candidates;
mod requests;
mod spec;

#[derive(Clone, Default)]
pub(super) struct TransferCatalog {
    pub token: Option<Uuid>,
    pub value: Option<Value>,
    pub signature: Value,
    pub error: Option<String>,
    pub session: Option<Value>,
}

/// 每个标签独立持有准备轮次；输入变化后绝不提交旧计划。
#[derive(Clone, Default)]
pub(super) struct TransferWorkbench {
    pub epoch: u64,
    pub pending: Option<(Uuid, u64, String)>,
    pub plan: Option<Value>,
    pub run: Option<Value>,
    pub items: Option<Value>,
    pub run_offset: u64,
    pub run_pages: Vec<u64>,
    pub run_spec: Option<Value>,
    pending_spec: Option<Value>,
    pub plan_page: Option<Value>,
    pub plan_section: String,
    pub plan_offset: u64,
    pub plan_pages: Vec<u64>,
    pub chunk_offset: u64,
    pub chunk_text: String,
    pub obsolete_plan: Option<Value>,
    pub cleanup_wait: bool,
    pub queued_prepare: Option<(String, Value)>,
    pub submitted_plan: bool,
    pub presets: Vec<Value>,
    pub preset_offset: u64,
    pub preset_next: bool,
    pub stage: String,
    pub selected_preset: Option<Value>,
    loaded_spec: Option<Value>,
    pub files: usize,
    pub objects: usize,
    pub object_filters: Vec<usize>,
    pub confirmations: std::collections::BTreeSet<String>,
    pub project_mode: bool,
    pub catalog: BTreeMap<String, TransferCatalog>,
    pub new_target: Option<String>,
    pub redis_keys: Option<std::collections::BTreeSet<String>>,
}
impl TransferWorkbench {
    pub fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        // 已提交的计划仍由 Engine 保留；再次准备前也要显式释放其预算。
        if let Some(plan) = self.plan.take() {
            self.obsolete_plan = Some(plan);
        }
        self.submitted_plan = false;
        self.queued_prepare = None;
        self.confirmations.clear();
    }
    pub fn busy(&self) -> bool {
        self.pending.is_some()
            || self.cleanup_wait
            || self.queued_prepare.is_some()
            || self.obsolete_plan.is_some()
            || (self.plan.is_some() && !self.submitted_plan)
            || self.run.as_ref().is_some_and(|run| {
                matches!(
                    run["state"].as_str(),
                    None | Some("queued") | Some("running") | Some("cancelling")
                )
            })
    }
}
impl App {
    pub(super) fn transfer_key(&self, name: &str) -> String {
        format!("transfer-{}-{name}", self.tabs[self.active].id)
    }
    fn transfer_value(&self, name: &str) -> &str {
        self.field(&self.transfer_key(name))
    }
    fn transfer_set(&mut self, name: &str, value: impl Into<String>) {
        let key = self.transfer_key(name);
        self.field_set(&key, value);
    }
    pub(super) fn transfer_output_directory(&self) -> bool {
        self.transfer_value("mode") == "export"
            && self.transfer_value("provider") == "mysql"
            && (0..self.workbench.transfer.objects)
                .filter(|index| {
                    !self
                        .transfer_value(&format!("object-source-{index}"))
                        .trim()
                        .is_empty()
                })
                .nth(1)
                .is_some()
    }

    fn transfer_file_defaults(&mut self, index: usize) {
        for (field, value) in [
            ("file", ""),
            ("file-target", ""),
            ("file-mapping", ""),
            ("file-range", ""),
            ("file-types", ""),
            ("file-format", "csv"),
            ("file-encoding", "utf-8"),
            ("file-delimiter", ","),
            ("file-header", "true"),
            ("file-backslash", "true"),
            ("file-null-marker", "\\N"),
            ("file-columns", "[]"),
            ("file-binary-columns", "[]"),
            ("file-sync", "false"),
        ] {
            self.transfer_set(&format!("{field}-{index}"), value);
        }
    }

    pub(super) fn append_transfer_paths(
        &mut self,
        paths: Vec<PathBuf>,
    ) -> std::result::Result<(), i18n::UiMessage> {
        use datapier_core::model::transfer::MAX_PLAN_ITEMS;
        if self.workbench.transfer.pending.is_some()
            || self.workbench.transfer.queued_prepare.is_some()
        {
            return Err(i18n::UiMessage::key("请求处理中，请稍候"));
        }
        let mut files = self.workbench.transfer.files;
        // 初始空行尚无输入文件，沿用它的资源入口预填和选项；有路径的行原样保留。
        let use_initial_row = files == 1 && self.transfer_value("file-0").trim().is_empty();
        let additional = paths.len().saturating_sub(usize::from(use_initial_row));
        if files.saturating_add(additional) > MAX_PLAN_ITEMS {
            return Err(i18n::UiMessage::template(
                "最多允许 {count} 个输入文件",
                vec![("count", i18n::UiArg::Raw(MAX_PLAN_ITEMS.to_string()))],
            ));
        }
        let paths = paths
            .into_iter()
            .map(|path| path.into_os_string().into_string())
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|_| i18n::UiMessage::key("路径无法用 UTF-8 无损表示"))?;
        let restore = self.transfer_value("mode") == "sql_restore";
        for (offset, path) in paths.into_iter().enumerate() {
            let index = if use_initial_row && offset == 0 {
                0
            } else {
                let index = files;
                self.transfer_file_defaults(index);
                files += 1;
                index
            };
            if !restore
                && std::path::Path::new(&path)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
            {
                self.transfer_set(&format!("file-format-{index}"), "json");
            }
            self.transfer_set(&format!("file-{index}"), path);
        }
        self.workbench.transfer.files = files;
        self.workbench.transfer.invalidate();
        self.dirty = true;
        Ok(())
    }

    pub(super) fn transfer_input_changed(&mut self, name: &str) {
        let Some(key) = name.strip_prefix(&format!("transfer-{}-file-", self.tabs[self.active].id))
        else {
            return;
        };
        let index = if let Ok(index) = key.parse::<usize>() {
            Some(index)
        } else {
            key.rsplit_once('-').and_then(|(part, index)| {
                matches!(
                    part,
                    "format"
                        | "encoding"
                        | "delimiter"
                        | "header"
                        | "columns"
                        | "null-marker"
                        | "backslash"
                        | "binary-columns"
                )
                .then(|| index.parse::<usize>().ok())
                .flatten()
            })
        };
        if let Some(index) = index {
            self.workbench
                .transfer
                .catalog
                .remove(&format!("input:{index}"));
            self.transfer_set(&format!("file-mapping-{index}"), "");
            if self.transfer_value("mode") == "sql_restore" {
                for object in 0..self.workbench.transfer.objects {
                    self.transfer_set(&format!("object-source-{object}"), "");
                    self.transfer_set(&format!("object-source-db-{object}"), "");
                    self.transfer_set(&format!("object-mapping-{object}"), "");
                }
            }
        }
    }
    pub(super) fn transfer_action(&mut self, name: &str, backend: &Backend) -> bool {
        if let Some(run_id) = name.strip_prefix("transfer-report-open:") {
            if Uuid::parse_str(run_id).is_ok() {
                self.transfer_action("transfer-new:copy", backend);
                self.workbench.transfer.run = Some(json!({"run_id":run_id}));
                self.workbench.transfer.stage = "report".into();
                self.transfer_send(backend, "transfer.run.get", json!({"run_id":run_id}));
            }
            return true;
        }
        if let Some(context) = name.strip_prefix("transfer-redis:") {
            let parts: Vec<_> = context.split(':').collect();
            if parts.len() == 3 {
                let mode = parts[0];
                let key = parts[1];
                let database = parts[2];
                self.transfer_action(&format!("transfer-new:{mode}"), backend);
                self.transfer_set("redis-selection", "keys");
                self.transfer_set("redis-keys", key);
                self.workbench.transfer.redis_keys =
                    Some(std::iter::once(key.to_owned()).collect());
                self.transfer_set("pattern", "");
                self.transfer_set("source-redis-db", database);
                self.transfer_set("destination-redis-db", database);
            }
            return true;
        }
        if let Some(context) = name.strip_prefix("transfer-context:") {
            use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
            let parts: Vec<_> = context.split(':').collect();
            if parts.len() == 4 {
                let mode = parts[0];
                let db = URL_SAFE_NO_PAD
                    .decode(parts[2])
                    .ok()
                    .and_then(|v| String::from_utf8(v).ok())
                    .unwrap_or_default();
                let table = URL_SAFE_NO_PAD
                    .decode(parts[3])
                    .ok()
                    .and_then(|v| String::from_utf8(v).ok())
                    .unwrap_or_default();
                let profile = Uuid::parse_str(parts[1])
                    .ok()
                    .and_then(|id| self.profiles.iter().find(|p| p.id == id))
                    .cloned();
                self.transfer_action(&format!("transfer-new:{mode}"), backend);
                if let Some(profile) = profile {
                    self.transfer_set("source-profile", profile.id.to_string());
                    self.transfer_set("destination-profile", profile.id.to_string());
                    self.transfer_set(
                        "provider",
                        if profile.provider == datapier_core::model::ProviderKind::Redis {
                            "redis"
                        } else {
                            "mysql"
                        },
                    );
                }
                if !db.is_empty() {
                    self.transfer_set("source-database", &db);
                    self.transfer_set("destination-database", &db);
                }
                if !table.is_empty() {
                    self.transfer_set("file-target-0", &table);
                    self.transfer_set("object-source-0", &table);
                    self.transfer_set("object-target-0", &table);
                }
            }
            return true;
        }
        if let Some(mode) = name
            .strip_prefix("transfer-new:")
            .or_else(|| name.strip_prefix("transfer-project-new:"))
        {
            if name.starts_with("transfer-project-new:")
                && (self.workbench.transfer.busy() || self.workbench.transfer.plan.is_some())
            {
                self.status = i18n::UiMessage::key("请求处理中，请稍候");
                return true;
            }
            self.dirty = true;
            if name.starts_with("transfer-project-new:") {
                for session in self
                    .workbench
                    .transfer
                    .catalog
                    .values_mut()
                    .filter_map(|entry| entry.session.take())
                {
                    let _ = backend.request(
                        "transfer-catalog-close",
                        "connection.close",
                        json!({"session_id":session}),
                    );
                }
                self.workbench.transfer = TransferWorkbench::default();
            }
            let profile = self.profile.clone();
            let redis_db = profile
                .as_ref()
                .map(|_| self.current_redis_db().to_string())
                .unwrap_or_default();
            let database = self.database.clone().unwrap_or_default();
            let target = self.target.clone();
            let project_mode = name.starts_with("transfer-project-new:");
            if !project_mode {
                self.create_query_tab();
            }
            self.workbench.transfer.project_mode = project_mode;
            self.tabs[self.active].title = i18n::tr("数据交换与迁移").into_owned();
            self.view = "transfer-workbench".into();
            let provider = profile
                .as_ref()
                .map(|p| {
                    if p.provider == datapier_core::model::ProviderKind::Redis {
                        "redis"
                    } else {
                        "mysql"
                    }
                })
                .unwrap_or("mysql");
            let id = profile.map(|p| p.id.to_string()).unwrap_or_default();
            for (key, value) in [
                ("mode", mode),
                ("provider", provider),
                ("source-profile", id.as_str()),
                ("destination-profile", id.as_str()),
                ("source-database", database.as_str()),
                ("destination-database", database.as_str()),
                ("source-redis-db", redis_db.as_str()),
                ("destination-redis-db", redis_db.as_str()),
                ("format", "csv"),
                ("encoding", "utf-8"),
                ("delimiter", ","),
                ("conflict", "insert"),
                ("conversion", "stop"),
                ("include-schema", "true"),
                ("include-data", "true"),
                ("create-missing", "false"),
                ("sync-schema", "false"),
                ("pattern", "*"),
                ("redis-selection", "pattern"),
            ] {
                self.transfer_set(key, value);
            }
            if project_mode {
                for key in [
                    "source-database",
                    "destination-database",
                    "source-redis-db",
                    "destination-redis-db",
                    "source-alias",
                    "destination-alias",
                    "project-path",
                ] {
                    self.transfer_set(key, "");
                }
            }
            self.transfer_set("report-format", "json");
            self.transfer_set("report-input", "false");
            for key in ["retry-failed", "retry-rolled-back", "retry-not-sent"] {
                self.transfer_set(key, "true");
            }
            self.workbench.transfer.files = 1;
            self.workbench.transfer.objects = 1;
            for key in [
                "object-source-0",
                "object-target-0",
                "object-mapping-0",
                "object-types-0",
                "output",
                "preset-name",
                "redis-keys",
                "source-prefix",
                "target-prefix",
                "report-path",
            ] {
                self.transfer_set(key, "");
            }
            self.transfer_file_defaults(0);
            self.transfer_set("definer-mode", "current_user");
            self.transfer_set("definer-map", "");
            self.transfer_set("triggers-confirmed", "false");
            self.transfer_object_defaults(0);
            if let Some(target) = target.filter(|t| t.kind == "table") {
                self.transfer_set("file-target-0", &target.name);
                self.transfer_set("object-source-0", &target.name);
                self.transfer_set("object-target-0", &target.name);
            }
            self.transfer_catalog_load(backend, "source", "databases", None);
            self.transfer_catalog_load(backend, "destination", "databases", None);
            return true;
        }
        if let Some(rest) = name.strip_prefix("field:transfer-") {
            if let Some((field, value)) = rest.split_once(':') {
                let field = format!("transfer-{field}");
                if matches!(value, "literal" | "expression")
                    && let Some(prefix) = field.strip_suffix("-kind")
                    && prefix.contains("-file-default-")
                {
                    let value_field = format!("{prefix}-value");
                    if !self.fields.contains_key(&value_field) {
                        self.field_set(&value_field, "");
                    }
                }
                let key = field
                    .strip_prefix(&format!("transfer-{}-", self.tabs[self.active].id))
                    .unwrap_or("")
                    .to_owned();
                if value == "__transfer_new__" {
                    self.transfer_set("new-target-name", "");
                    let index = key.rsplit('-').next().unwrap_or("0");
                    let group = if key.starts_with("file-") {
                        "file"
                    } else {
                        "object"
                    };
                    let types = self
                        .transfer_value(&format!("{group}-types-{index}"))
                        .to_owned();
                    self.transfer_set("new-target-types", types);
                    self.workbench.transfer.new_target = Some(key);
                    self.workbench.generic_select = None;
                    let focus = self.transfer_key("new-target-name");
                    self.focus = focus.clone();
                    self.workbench.focus_request = Some(format!("input-{focus}"));
                    return true;
                }
                let changed = self.field(&field) != value;
                self.field_set(&field, value);
                self.changed_field(&field);
                self.workbench.generic_select = None;
                if changed {
                    if key == "redis-selection" && value == "keys" {
                        self.workbench.transfer.redis_keys = Some(Default::default());
                    }
                    for side in ["source", "destination"] {
                        let connection_changed = key == format!("{side}-profile")
                            || key == format!("{side}-alias")
                            || key == "provider";
                        let database_changed =
                            key == format!("{side}-database") || key == format!("{side}-redis-db");
                        if connection_changed || database_changed {
                            if side == "source" {
                                self.workbench.transfer.redis_keys = Some(Default::default());
                            }
                            for session in self
                                .workbench
                                .transfer
                                .catalog
                                .iter()
                                .filter(|(name, _)| name.starts_with(&format!("{side}:")))
                                .filter_map(|(_, entry)| entry.session.as_ref())
                            {
                                let _ = backend.request(
                                    "transfer-catalog-close",
                                    "connection.close",
                                    json!({"session_id":session}),
                                );
                            }
                            self.workbench.transfer.catalog.retain(|name, _| {
                                !name.starts_with(&format!("{side}:"))
                                    || (!connection_changed && name == &format!("{side}:databases"))
                            });
                            if connection_changed {
                                if key == "provider" {
                                    self.transfer_set(&format!("{side}-profile"), "");
                                    self.transfer_set(&format!("{side}-alias"), "");
                                }
                                self.transfer_set(&format!("{side}-database"), "");
                                self.transfer_set(&format!("{side}-redis-db"), "");
                                self.transfer_catalog_load(backend, side, "databases", None);
                            } else if self.transfer_value("provider") == "mysql" {
                                self.transfer_catalog_load(
                                    backend,
                                    side,
                                    "objects",
                                    Some(json!({"kind":"table"})),
                                );
                            }
                            for i in 0..self.workbench.transfer.objects {
                                self.transfer_set(
                                    &format!(
                                        "object-{}-{i}",
                                        if side == "source" { "source" } else { "target" }
                                    ),
                                    "",
                                );
                                self.transfer_set(&format!("object-mapping-{i}"), "");
                            }
                            if side == "destination" {
                                for i in 0..self.workbench.transfer.files {
                                    self.transfer_set(&format!("file-target-{i}"), "");
                                    self.transfer_set(&format!("file-mapping-{i}"), "");
                                }
                            }
                        }
                    }
                    if let Some(index) = key
                        .strip_prefix("object-kind-")
                        .and_then(|value| value.parse::<usize>().ok())
                    {
                        for part in ["source", "target", "mapping"] {
                            self.transfer_set(&format!("object-{part}-{index}"), "");
                        }
                        for side in ["source", "destination"] {
                            if !self.workbench.transfer.catalog.contains_key(&format!(
                                "{side}:objects:{value}:{}",
                                self.transfer_value(&format!("{side}-database"))
                            )) {
                                self.transfer_catalog_load(
                                    backend,
                                    side,
                                    "objects",
                                    Some(json!({"kind":value})),
                                );
                            }
                        }
                    }
                    for (prefix, side, part) in [
                        ("object-source-db-", "source", "source"),
                        ("object-target-db-", "destination", "target"),
                    ] {
                        if let Some(index) = key
                            .strip_prefix(prefix)
                            .and_then(|value| value.parse::<usize>().ok())
                        {
                            self.transfer_set(&format!("object-{part}-{index}"), "");
                            self.transfer_set(&format!("object-mapping-{index}"), "");
                            let kind = self
                                .transfer_value(&format!("object-kind-{index}"))
                                .to_owned();
                            self.transfer_catalog_load(
                                backend,
                                side,
                                "objects",
                                Some(json!({"kind":kind,"database":value})),
                            );
                        }
                    }
                    for (prefix, side) in [
                        ("object-source-", "source"),
                        ("object-target-", "destination"),
                        ("file-target-", "destination"),
                    ] {
                        if let Some(index) = key
                            .strip_prefix(prefix)
                            .and_then(|value| value.parse::<usize>().ok())
                        {
                            if !value.is_empty() {
                                let kind = if prefix == "file-target-" {
                                    "table"
                                } else {
                                    self.transfer_value(&format!("object-kind-{index}"))
                                };
                                let database = if prefix == "file-target-" {
                                    ""
                                } else {
                                    self.transfer_value(&format!(
                                        "object-{}-db-{index}",
                                        if side == "source" { "source" } else { "target" }
                                    ))
                                };
                                let target = json!({"name":value,"kind":kind,"database":if database.is_empty() { None } else { Some(database) },"key_b64":null});
                                if matches!(kind, "table" | "view")
                                    && !(side == "source"
                                        && self.transfer_value("mode") == "sql_restore")
                                {
                                    self.transfer_catalog_load(
                                        backend,
                                        side,
                                        "columns",
                                        Some(target),
                                    );
                                }
                            }
                            if prefix == "file-target-" {
                                self.transfer_set(&format!("file-mapping-{index}"), "");
                            } else {
                                self.transfer_set(&format!("object-mapping-{index}"), "");
                            }
                            if prefix == "object-source-" {
                                let count = self
                                    .workbench
                                    .transfer
                                    .object_filters
                                    .get(index)
                                    .copied()
                                    .unwrap_or(0);
                                for filter in 0..count {
                                    self.transfer_set(
                                        &format!("object-filter-column-{index}-{filter}"),
                                        "",
                                    );
                                }
                            }
                        }
                    }
                }
            }
            return true;
        }
        if let Some(stage) = name.strip_prefix("transfer-stage:") {
            if matches!(
                stage,
                "source" | "objects" | "policy" | "preview" | "report"
            ) {
                self.workbench.transfer.stage = stage.into();
                if (stage == "objects" || stage == "source")
                    && self.transfer_value("provider") == "mysql"
                {
                    for side in ["source", "destination"] {
                        if !self.workbench.transfer.catalog.contains_key(&format!(
                            "{side}:objects:table:{}",
                            self.transfer_value(&format!("{side}-database"))
                        )) {
                            self.transfer_catalog_load(
                                backend,
                                side,
                                "objects",
                                Some(json!({"kind":"table"})),
                            );
                        }
                    }
                }
                self.focus.clear();
            }
            return true;
        }
        if let Some((side, scope)) = name
            .strip_prefix("transfer-catalog-retry:")
            .and_then(|value| value.split_once(':'))
        {
            if matches!(side, "source" | "destination") && matches!(scope, "databases" | "objects")
            {
                self.transfer_catalog_load(backend, side, scope, None);
            }
            return true;
        }
        if let Some(side) = name.strip_prefix("transfer-catalog-more:") {
            if matches!(side, "source" | "destination") {
                self.transfer_catalog_page(backend, side, "objects", None, true);
            }
            return true;
        }
        if let Some(rest) = name.strip_prefix("transfer-objects-load:") {
            let mut parts = rest.splitn(3, ':');
            if let (Some(side @ ("source" | "destination")), Some(kind), Some(database)) =
                (parts.next(), parts.next(), parts.next())
            {
                self.transfer_catalog_load(
                    backend,
                    side,
                    "objects",
                    Some(json!({"kind":kind,"database":database})),
                );
            }
            return true;
        }
        if let Some(rest) = name.strip_prefix("transfer-objects-more:") {
            let mut parts = rest.splitn(3, ':');
            if let (Some(side @ ("source" | "destination")), Some(kind), Some(database)) =
                (parts.next(), parts.next(), parts.next())
            {
                self.transfer_catalog_page(
                    backend,
                    side,
                    "objects",
                    Some(json!({"kind":kind,"database":database})),
                    true,
                );
            }
            return true;
        }
        if let Some(key) = name.strip_prefix("transfer-redis-key:") {
            let keys = self
                .workbench
                .transfer
                .redis_keys
                .get_or_insert_with(Default::default);
            if !keys.remove(key) {
                keys.insert(key.to_owned());
            }
            self.workbench.transfer.invalidate();
            return true;
        }
        if let Some(rest) = name.strip_prefix("transfer-columns-retry:") {
            let mut parts = rest.split(':');
            if let (
                Some(side @ ("source" | "destination")),
                Some(group @ ("file" | "object")),
                Some(index),
            ) = (parts.next(), parts.next(), parts.next())
            {
                let part = if side == "source" { "source" } else { "target" };
                let object = self
                    .transfer_value(&format!("{group}-{part}-{index}"))
                    .to_owned();
                let database = if group == "file" {
                    String::new()
                } else {
                    self.transfer_value(&format!("object-{part}-db-{index}"))
                        .to_owned()
                };
                let kind = if group == "file" {
                    "table".to_owned()
                } else {
                    self.transfer_value(&format!("object-kind-{index}"))
                        .to_owned()
                };
                if !object.is_empty() {
                    self.transfer_catalog_load(backend, side, "columns", Some(json!({"name":object,"kind":kind,"database":if database.is_empty(){None}else{Some(database)},"key_b64":null})));
                }
            }
            return true;
        }
        if let Some(i) = name
            .strip_prefix("transfer-input-inspect:")
            .and_then(|i| i.parse::<usize>().ok())
        {
            if i < self.workbench.transfer.files {
                self.transfer_input_load(backend, i);
            }
            return true;
        }
        if let Some(rest) = name.strip_prefix("transfer-map:") {
            use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
            let mut parts = rest.splitn(4, ':');
            if let (Some(group @ ("file" | "object")), Some(index), Some(source), Some(target)) =
                (parts.next(), parts.next(), parts.next(), parts.next())
                && let (Ok(source), Ok(target)) = (
                    URL_SAFE_NO_PAD.decode(source),
                    URL_SAFE_NO_PAD.decode(target),
                )
                && let (Ok(source), Ok(target)) =
                    (String::from_utf8(source), String::from_utf8(target))
            {
                let key = format!("{group}-mapping-{index}");
                let mut mapping = parse_mapping(self.transfer_value(&key)).unwrap_or_default();
                if group == "object" {
                    if mapping.is_empty() {
                        let source_object = self.transfer_value(&format!("object-source-{index}"));
                        let override_db = self.transfer_value(&format!("object-source-db-{index}"));
                        let restore = self.transfer_value("mode") == "sql_restore";
                        let database = if !override_db.is_empty() {
                            override_db
                        } else if restore {
                            ""
                        } else {
                            self.transfer_value("source-database")
                        };
                        let columns = if restore {
                            (0..self.workbench.transfer.files)
                                .filter_map(|i| {
                                    self.workbench
                                        .transfer
                                        .catalog
                                        .get(&format!("input:{i}"))
                                        .filter(|entry| {
                                            self.transfer_input_params(i)
                                                .ok()
                                                .is_some_and(|params| params == entry.signature)
                                        })
                                })
                                .filter_map(|entry| entry.value.as_ref())
                                .flat_map(|value| value["objects"].as_array().into_iter().flatten())
                                .find(|object| {
                                    object["name"] == source_object
                                        && (database.is_empty() || object["database"] == database)
                                })
                                .and_then(|object| object["columns"].as_array())
                        } else {
                            self.workbench
                                .transfer
                                .catalog
                                .get(&format!("source:columns:{database}:{source_object}"))
                                .and_then(|entry| entry.value.as_ref())
                                .and_then(|value| value["columns"].as_array())
                        };
                        if let Some(columns) = columns {
                            for column in
                                columns.iter().filter(|column| column["generated"] != true)
                            {
                                if let Some(name) = column["name"].as_str() {
                                    mapping.insert(name.to_owned(), name.to_owned());
                                }
                            }
                        }
                    }
                    let mapped = if target.is_empty() {
                        source.clone()
                    } else {
                        target
                    };
                    mapping.insert(source, mapped);
                } else if target.is_empty() {
                    mapping.remove(&source);
                } else {
                    mapping.insert(source, target);
                }
                self.transfer_set(&key, json!(mapping).to_string());
                self.workbench.transfer.invalidate();
                self.workbench.generic_select = None;
            }
            return true;
        }
        if name == "transfer-new-target-cancel" {
            if let Some(key) = self.workbench.transfer.new_target.take() {
                let focus = self.transfer_key(&key);
                self.focus = focus.clone();
                self.workbench.focus_request = Some(format!("input-{focus}"));
            }
            return true;
        }
        if name == "transfer-new-target-save" {
            let value = self.transfer_value("new-target-name").trim().to_owned();
            if value.is_empty() {
                self.field_error(
                    &self.transfer_key("new-target-name"),
                    i18n::UiMessage::local("目标对象名称不能为空"),
                );
            } else if let Some(key) = self.workbench.transfer.new_target.take() {
                let index = key.rsplit('-').next().unwrap_or("0");
                let group = if key.starts_with("file-") {
                    "file"
                } else {
                    "object"
                };
                let types = self.transfer_value("new-target-types").to_owned();
                self.transfer_set(&format!("{group}-types-{index}"), types);
                self.transfer_set(&key, value);
                if let Some(index) = key.strip_prefix("object-target-") {
                    self.transfer_set(&format!("object-create-{index}"), "true");
                }
                self.workbench.transfer.invalidate();
                let focus = self.transfer_key(&key);
                self.focus = focus.clone();
                self.workbench.focus_request = Some(format!("input-{focus}"));
            }
            return true;
        }
        if !name.starts_with("transfer-ui-") {
            return false;
        }
        self.dirty = true;
        if (self.workbench.transfer.pending.is_some()
            && name != "transfer-ui-prepare"
            && name != "transfer-ui-retry")
            || (self.workbench.transfer.queued_prepare.is_some()
                && !matches!(
                    name,
                    "transfer-ui-cleanup-poll"
                        | "transfer-ui-prepare"
                        | "transfer-ui-retry"
                        | "transfer-ui-refresh-run"
                        | "transfer-ui-run-items"
                        | "transfer-ui-cancel"
                ))
        {
            self.status = i18n::UiMessage::key("请求处理中，请稍候");
            return true;
        }
        if let Some(i) = name
            .strip_prefix("transfer-ui-filter-add:")
            .and_then(|s| s.parse::<usize>().ok())
        {
            if i < self.workbench.transfer.objects {
                let j = self
                    .workbench
                    .transfer
                    .object_filters
                    .get(i)
                    .copied()
                    .unwrap_or(0);
                if self.workbench.transfer.object_filters.len() <= i {
                    self.workbench.transfer.object_filters.resize(i + 1, 0);
                }
                self.workbench.transfer.object_filters[i] = j + 1;
                self.transfer_filter_defaults(i, j);
                self.workbench.transfer.invalidate();
            }
            return true;
        }
        if let Some((i, j)) = name
            .strip_prefix("transfer-ui-filter-remove:")
            .and_then(|s| s.split_once(':'))
            .and_then(|(i, j)| Some((i.parse::<usize>().ok()?, j.parse::<usize>().ok()?)))
        {
            let count = self
                .workbench
                .transfer
                .object_filters
                .get(i)
                .copied()
                .unwrap_or(0);
            if i < self.workbench.transfer.objects && j < count {
                for index in j..count - 1 {
                    for key in ["column", "op", "type", "value"] {
                        let value = self
                            .transfer_value(&format!("object-filter-{key}-{i}-{}", index + 1))
                            .to_owned();
                        self.transfer_set(&format!("object-filter-{key}-{i}-{index}"), value);
                    }
                }
                self.workbench.transfer.object_filters[i] = count - 1;
                self.workbench.transfer.invalidate();
            }
            return true;
        }
        if let Some(section) = name.strip_prefix("transfer-ui-section:") {
            if matches!(section, "items" | "samples" | "diagnostics" | "spec") {
                self.workbench.transfer.plan_section = section.into();
                self.workbench.transfer.plan_offset = 0;
                self.workbench.transfer.plan_pages.clear();
                self.workbench.transfer.chunk_offset = 0;
                self.workbench.transfer.chunk_text.clear();
                self.workbench.transfer.plan_page = None;
                self.transfer_action("transfer-ui-page-read", backend);
            }
            return true;
        }
        if let Some(index) = name
            .strip_prefix("transfer-ui-page-item:")
            .and_then(|v| v.parse::<usize>().ok())
        {
            if let (Some(plan), Some(item)) = (
                self.workbench.transfer.plan.clone(),
                self.workbench
                    .transfer
                    .plan_page
                    .as_ref()
                    .and_then(|p| p["items"].get(index))
                    .cloned(),
            ) {
                if !transfer_selectable_item(&item) {
                    return true;
                }
                let selected = !item["selected"].as_bool().unwrap_or(false);
                self.workbench.transfer.invalidate();
                self.transfer_send(backend,"transfer.plan.update",json!({"plan_id":plan.get("id").or_else(||plan.get("plan_id")),"expected_revision":plan["revision"],"item_selection":[{"id":item["id"],"selected":selected}]}));
            }
            return true;
        }
        match name {
            "transfer-ui-file-add" => {
                let i = self.workbench.transfer.files;
                if i >= datapier_core::model::transfer::MAX_PLAN_ITEMS {
                    return true;
                }
                self.transfer_file_defaults(i);
                self.workbench.transfer.files += 1;
                self.workbench.transfer.invalidate();
            }
            "transfer-ui-object-add" => {
                let i = self.workbench.transfer.objects;
                self.workbench.transfer.objects += 1;
                self.transfer_object_defaults(i);
                self.workbench.transfer.invalidate();
            }
            "transfer-ui-file-remove" => {
                if self.workbench.transfer.files > 1 {
                    self.workbench.transfer.files -= 1;
                    self.workbench.transfer.invalidate();
                }
            }
            "transfer-ui-object-remove" => {
                if self.workbench.transfer.objects > 1 {
                    self.workbench.transfer.objects -= 1;
                    self.workbench.transfer.invalidate();
                }
            }
            "transfer-ui-project-return" => {
                if self.workbench.transfer.busy() || self.workbench.transfer.plan.is_some() {
                    self.workbench.transfer.stage = "preview".into();
                    self.status = i18n::UiMessage::key("请先丢弃迁移计划并等待任务结束");
                } else {
                    self.view = "project-workbench".into();
                }
            }
            "transfer-ui-project-save" => {
                if self.workbench.transfer.busy() || self.workbench.transfer.plan.is_some() {
                    self.workbench.transfer.stage = "preview".into();
                    self.status = i18n::UiMessage::key("请先丢弃迁移计划并等待任务结束");
                    return true;
                }
                let name = self.transfer_value("preset-name").trim().to_owned();
                let path = self.transfer_value("project-path").trim().to_owned();
                if name.is_empty() || path.is_empty() {
                    self.transfer_field_error((
                        if name.is_empty() {
                            "preset-name"
                        } else {
                            "project-path"
                        }
                        .into(),
                        "请输入共享迁移名称和项目内路径".into(),
                    ));
                    return true;
                }
                for i in 0..self.workbench.transfer.objects {
                    for side in ["source", "target"] {
                        let field = format!("object-{side}-db-{i}");
                        if !self.transfer_value(&field).trim().is_empty() {
                            self.transfer_field_error((
                                field,
                                "共享配置的实际数据库请通过本机绑定填写".into(),
                            ));
                            return true;
                        }
                    }
                }
                match self.transfer_spec() {
                    Ok(spec) => {
                        let text = match serde_json::to_string_pretty(&spec) {
                            Ok(text) => text,
                            Err(error) => {
                                self.status = i18n::UiMessage::raw(error.to_string());
                                return true;
                            }
                        };
                        if let Some(project) = &mut self.workbench.project.project
                            && let Some(transfers) = project["manifest"]["transfers"].as_array_mut()
                        {
                            let id = transfers
                                .iter()
                                .find(|t| t["path"].as_str() == Some(path.as_str()))
                                .map(|t| t["id"].clone())
                                .unwrap_or_else(|| json!(Uuid::new_v4()));
                            transfers.retain(|t| t["path"].as_str() != Some(path.as_str()));
                            transfers.push(json!({"id":id,"name":name,"path":path}));
                            self.workbench.project.transfer_files.insert(path, text);
                            self.workbench.project.unsaved = true;
                            self.workbench.project.invalidate();
                            self.view = "project-workbench".into();
                        }
                    }
                    Err(error) => self.transfer_field_error(error),
                }
            }
            "transfer-ui-prepare" => match self.transfer_spec() {
                Ok(spec) => {
                    self.workbench.field_error = None;
                    self.workbench.transfer.stage = "preview".into();
                    self.workbench.transfer.invalidate();
                    self.workbench.transfer.queued_prepare = Some((
                        "transfer.prepare".into(),
                        if self.workbench.transfer.project_mode {
                            json!({"workspace_id":self.workspace_id,"spec":spec,"project_id":self.workbench.project.project.as_ref().map(|p| &p["project_id"]),"binding_group":self.field(&self.project_key("group"))})
                        } else {
                            json!({"workspace_id":self.workspace_id,"spec":spec})
                        },
                    ));
                    if self.workbench.transfer.pending.is_none() {
                        self.transfer_release_or_prepare(backend);
                    }
                }
                Err(error) => self.transfer_field_error(error),
            },
            "transfer-ui-cleanup-poll" => {
                if self.workbench.transfer.cleanup_wait {
                    self.transfer_release_or_prepare(backend);
                }
            }
            "transfer-ui-discard" => {
                self.workbench.transfer.invalidate();
                self.transfer_release_or_prepare(backend);
            }
            "transfer-ui-refresh-plan" => {
                if let Some(plan) = self.workbench.transfer.plan.clone() {
                    self.transfer_send(backend,"transfer.plan.read",json!({"plan_id":plan.get("id").or_else(||plan.get("plan_id")),"revision":plan.get("revision"),"section":"summary","offset":0,"limit":100}));
                }
            }
            "transfer-ui-page-read" => {
                if let Some(plan) = self.workbench.transfer.plan.clone() {
                    self.transfer_send(backend,"transfer.plan.read",json!({"plan_id":plan.get("id").or_else(||plan.get("plan_id")),"revision":plan.get("revision"),"section":self.workbench.transfer.plan_section,"offset":self.workbench.transfer.plan_offset,"limit":100,"chunk_offset":self.workbench.transfer.chunk_offset}));
                }
            }
            "transfer-ui-page-next" => {
                if let Some(next) = self
                    .workbench
                    .transfer
                    .plan_page
                    .as_ref()
                    .and_then(|p| p.get("next_offset"))
                    .and_then(Value::as_u64)
                {
                    self.workbench
                        .transfer
                        .plan_pages
                        .push(self.workbench.transfer.plan_offset);
                    self.workbench.transfer.plan_offset = next;
                    self.workbench.transfer.chunk_offset = 0;
                    self.workbench.transfer.chunk_text.clear();
                    self.transfer_action("transfer-ui-page-read", backend);
                }
            }
            "transfer-ui-page-prev" => {
                if let Some(previous) = self.workbench.transfer.plan_pages.pop() {
                    self.workbench.transfer.plan_offset = previous;
                    self.workbench.transfer.chunk_offset = 0;
                    self.workbench.transfer.chunk_text.clear();
                    self.transfer_action("transfer-ui-page-read", backend);
                }
            }
            "transfer-ui-submit" => {
                if let Some(plan) = self.workbench.transfer.plan.clone() {
                    self.workbench.transfer.pending_spec =
                        plan.get("spec").filter(|v| !v.is_null()).cloned();
                    self.transfer_send(backend,"transfer.submit",json!({"plan_id":plan.get("id").or_else(||plan.get("plan_id")),"revision":plan.get("revision"),"digest":plan.get("digest"),"destructive_confirmations":self.workbench.transfer.confirmations}));
                }
            }
            "transfer-ui-refresh-run" => {
                if let Some(run) = self.workbench.transfer.run.clone() {
                    self.transfer_send(
                        backend,
                        "transfer.run.get",
                        json!({"run_id":run.get("run_id").or_else(||run.get("id"))}),
                    );
                }
            }
            "transfer-ui-run-items" => {
                if let Some(run) = self.workbench.transfer.run.clone() {
                    self.transfer_send(backend,"transfer.run.items",json!({"run_id":run.get("run_id").or_else(||run.get("id")),"offset":self.workbench.transfer.run_offset,"limit":100}));
                }
            }
            "transfer-ui-items-next" => {
                if let Some(next) = self
                    .workbench
                    .transfer
                    .items
                    .as_ref()
                    .and_then(|p| p.get("next"))
                    .and_then(Value::as_u64)
                {
                    self.workbench
                        .transfer
                        .run_pages
                        .push(self.workbench.transfer.run_offset);
                    self.workbench.transfer.run_offset = next.saturating_add(1);
                    self.transfer_action("transfer-ui-run-items", backend);
                }
            }
            "transfer-ui-items-prev" => {
                if let Some(previous) = self.workbench.transfer.run_pages.pop() {
                    self.workbench.transfer.run_offset = previous;
                    self.transfer_action("transfer-ui-run-items", backend);
                }
            }
            "transfer-ui-cancel" => {
                if let Some(task) = self
                    .workbench
                    .transfer
                    .run
                    .as_ref()
                    .and_then(|v| v.get("task_id"))
                    .cloned()
                {
                    self.transfer_send(backend, "execution.cancel", json!({"task_id":task}));
                }
            }
            "transfer-ui-preset-list" | "transfer-ui-preset-next" | "transfer-ui-preset-prev" => {
                match name {
                    "transfer-ui-preset-list" => self.workbench.transfer.preset_offset = 0,
                    "transfer-ui-preset-next" if self.workbench.transfer.preset_next => {
                        self.workbench.transfer.preset_offset += 100
                    }
                    "transfer-ui-preset-prev" => {
                        self.workbench.transfer.preset_offset =
                            self.workbench.transfer.preset_offset.saturating_sub(100)
                    }
                    _ => return true,
                }
                self.transfer_send(backend, "transfer.preset.list", json!({"workspace_id":self.workspace_id,"offset":self.workbench.transfer.preset_offset,"limit":100}));
            }
            "transfer-ui-preset-save" => match self.transfer_spec() {
                Ok(spec) => {
                    if self.transfer_value("preset-name").trim().is_empty() {
                        self.transfer_field_error(("preset-name".into(), "请输入预设名称".into()));
                        return true;
                    }
                    let selected = self.workbench.transfer.selected_preset.clone();
                    self.transfer_send(backend,"transfer.preset.save",json!({"id":selected.as_ref().and_then(|v|v.get("id")),"expected_revision":selected.as_ref().and_then(|v|v.get("revision")),"workspace_id":self.workspace_id,"name":self.transfer_value("preset-name"),"spec":spec}));
                }
                Err(error) => self.transfer_field_error(error),
            },
            "transfer-ui-preset-delete" => {
                if let Some(preset) = self.workbench.transfer.selected_preset.clone() {
                    self.transfer_send(backend,"transfer.preset.delete",json!({"id":preset["id"],"expected_revision":preset["revision"],"workspace_id":self.workspace_id}));
                }
            }
            "transfer-ui-report" => {
                if let Some(run) = self.workbench.transfer.run.clone() {
                    let include = self.transfer_value("report-input") == "true";
                    let spec = if include {
                        match self.workbench.transfer.run_spec.clone().or_else(|| {
                            self.workbench
                                .transfer
                                .selected_preset
                                .as_ref()
                                .and_then(|p| p.get("spec"))
                                .cloned()
                        }) {
                            Some(spec) => Some(spec),
                            None => {
                                self.status =
                                    i18n::UiMessage::key("请先选择原预设，或仅导出报告元数据");
                                return true;
                            }
                        }
                    } else {
                        None
                    };
                    self.transfer_send(backend,"transfer.report.export",json!({"run_id":run.get("run_id").or_else(||run.get("id")),"path":self.transfer_value("report-path"),"format":self.transfer_value("report-format"),"include_failed_input":include,"spec":spec}));
                }
            }
            "transfer-ui-retry" => {
                if let Some(run) = self.workbench.transfer.run.clone() {
                    let Some(spec) = self.workbench.transfer.run_spec.clone().or_else(|| {
                        self.workbench
                            .transfer
                            .selected_preset
                            .as_ref()
                            .and_then(|p| p.get("spec"))
                            .cloned()
                    }) else {
                        self.status = i18n::UiMessage::key("请先选择原预设，再准备重试");
                        return true;
                    };
                    let states: Vec<_> = [
                        ("retry-failed", "failed"),
                        ("retry-rolled-back", "rolled_back"),
                        ("retry-not-sent", "not_sent"),
                    ]
                    .into_iter()
                    .filter_map(|(key, state)| {
                        (self.transfer_value(key) == "true").then_some(state)
                    })
                    .collect();
                    if states.is_empty() {
                        self.status = i18n::UiMessage::key("请至少选择一种续作状态");
                        return true;
                    }
                    self.workbench.transfer.invalidate();
                    self.workbench.transfer.queued_prepare = Some((
                        "transfer.retry.prepare".into(),
                        json!({"run_id":run.get("run_id").or_else(||run.get("id")),"spec":spec,"states":states}),
                    ));
                    if self.workbench.transfer.pending.is_none() {
                        self.transfer_release_or_prepare(backend);
                    }
                }
            }
            _ => {
                if let Some(id) = name.strip_prefix("transfer-ui-confirm:") {
                    if !self.workbench.transfer.confirmations.remove(id) {
                        self.workbench.transfer.confirmations.insert(id.to_owned());
                    }
                } else if let Some(index) = name
                    .strip_prefix("transfer-ui-preset-load:")
                    .and_then(|v| v.parse::<usize>().ok())
                {
                    if let Some(preset) = self.workbench.transfer.presets.get(index).cloned() {
                        self.transfer_load_preset(preset);
                    }
                } else if let Some(index) = name
                    .strip_prefix("transfer-ui-item:")
                    .and_then(|v| v.parse::<usize>().ok())
                    && let Some(plan) = self.workbench.transfer.plan.clone()
                    && let Some(item) = plan["items"].get(index)
                    && transfer_selectable_item(item)
                {
                    let selected = !item["selected"].as_bool().unwrap_or(false);
                    let item_id = item["id"].clone();
                    self.workbench.transfer.invalidate();
                    self.transfer_send(backend,"transfer.plan.update",json!({"plan_id":plan.get("id").or_else(||plan.get("plan_id")),"expected_revision":plan["revision"],"item_selection":[{"id":item_id,"selected":selected}]}));
                }
            }
        }
        true
    }
}

// 预设未暴露的类型属性必须保留；表单可编辑字段始终以当前输入为准。
fn retain_unedited_spec(current: &mut Value, base: &Value) {
    if let (Some(current), Some(base)) = (current.as_object_mut(), base.as_object()) {
        for (key, old) in base {
            if !current.contains_key(key) {
                current.insert(key.clone(), old.clone());
            }
        }
        if let (Some(value), Some(old)) = (current.get_mut("options"), base.get("options")) {
            retain_unedited_spec(value, old);
        }
        for group in ["inputs", "objects"] {
            if let (Some(values), Some(old_values)) = (
                current.get_mut(group).and_then(Value::as_array_mut),
                base.get(group).and_then(Value::as_array),
            ) {
                for (value, old) in values.iter_mut().zip(old_values) {
                    let key = if group == "inputs" {
                        "columns"
                    } else {
                        "target_columns"
                    };
                    if let (Some(columns), Some(old_columns)) = (
                        value.get_mut(key).and_then(Value::as_array_mut),
                        old.get(key).and_then(Value::as_array),
                    ) {
                        for column in columns {
                            if let Some(original) = old_columns.iter().find(|old| {
                                old["name"] == column["name"]
                                    && old["type_name"] == column["type_name"]
                            }) {
                                retain_unedited_spec(column, original);
                            }
                        }
                    }
                    retain_unedited_spec(value, old);
                }
            }
        }
    }
}

fn transfer_selectable_item(item: &Value) -> bool {
    item["phase"] == "schema"
        || (matches!(item["phase"].as_str(), Some("restore" | "after_data"))
            && item["source"].is_object()
            && item["target"].is_object())
}
pub(super) fn parse_mapping(text: &str) -> std::result::Result<BTreeMap<String, String>, String> {
    if text.trim_start().starts_with('{') {
        let mappings: BTreeMap<String, String> =
            serde_json::from_str(text).map_err(|_| "映射必须是 JSON 字符串对象".to_owned())?;
        if mappings
            .iter()
            .any(|(source, target)| source.trim().is_empty() || target.trim().is_empty())
        {
            return Err("映射字段为空或重复".into());
        }
        return Ok(mappings);
    }
    let mut mappings = BTreeMap::new();
    for entry in mapping_entries(text)?
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (source, target) = entry
            .split_once('=')
            .ok_or("映射格式：源字段=目标字段，使用逗号分隔")?;
        if source.trim().is_empty()
            || target.trim().is_empty()
            || mappings
                .insert(source.trim().into(), target.trim().into())
                .is_some()
        {
            return Err("映射字段为空或重复".into());
        }
    }
    Ok(mappings)
}
// SQL 类型参数及引号中的逗号不是映射分隔符；保持原始类型文本不变。
fn mapping_entries(text: &str) -> std::result::Result<Vec<&str>, String> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == delimiter {
                quote = None;
            }
        } else {
            match ch {
                '\'' | '"' | '`' => quote = Some(ch),
                '(' => depth += 1,
                ')' => {
                    if depth == 0 {
                        return Err("映射中的括号或引号未配对".into());
                    }
                    depth -= 1;
                }
                ',' if depth == 0 => {
                    entries.push(&text[start..index]);
                    start = index + 1;
                }
                _ => {}
            }
        }
    }
    if quote.is_some() || depth != 0 {
        return Err("映射中的括号或引号未配对".into());
    }
    entries.push(&text[start..]);
    Ok(entries)
}
pub(super) fn parse_types(text: &str) -> std::result::Result<Vec<Value>, String> {
    if text.trim_start().starts_with('[') {
        let values: Vec<Value> =
            serde_json::from_str(text).map_err(|_| "目标类型必须是 JSON 数组".to_owned())?;
        serde_json::from_value::<Vec<datapier_core::model::Column>>(json!(values))
            .map_err(|error| format!("目标类型无效：{error}"))?;
        return Ok(values);
    }
    Ok(parse_mapping(text)?
        .into_iter()
        .map(|(name, type_name)| json!({"name":name,"type_name":type_name}))
        .collect())
}
pub(super) fn column_default_key(file: usize, column: &str, part: &str) -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    format!(
        "file-default-{file}-{}-{part}",
        URL_SAFE_NO_PAD.encode(column.as_bytes())
    )
}

fn format_map(value: &Value) -> String {
    value
        .as_object()
        .map(|_| value.to_string())
        .unwrap_or_default()
}
fn format_types(value: &Value) -> String {
    value
        .as_array()
        .map(|_| value.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "../tests/unit/transfer_lifecycle.rs"]
mod lifecycle_tests;
