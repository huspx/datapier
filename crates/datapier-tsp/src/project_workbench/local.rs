use super::*;

impl App {
    pub(super) fn project_discover_databases(&mut self, backend: &Backend) {
        let id = self.project_value("local-profile").to_owned();
        let token = Uuid::new_v4();
        self.workbench.project.local_database_discovery = ProjectDatabaseDiscovery {
            profile_id: id.clone(),
            token: Some(token),
            ..Default::default()
        };
        if id.is_empty() {
            self.workbench.project.local_database_discovery.token = None;
            return;
        }
        let session = self
            .profile
            .as_ref()
            .filter(|profile| profile.id.to_string() == id)
            .and_then(|_| self.session_id());
        let tag = format!(
            "{}:{}:project-db/{token}",
            self.tabs[self.active].id, self.generation
        );
        if let Err(error) = backend.request(
            tag,
            "database.list",
            json!({"profile_id": id, "session_id": session, "refresh": true}),
        ) {
            let discovery = &mut self.workbench.project.local_database_discovery;
            discovery.token = None;
            discovery.error = Some(error.to_string());
        }
    }
    pub(crate) fn project_navigate_path(&mut self, backend: &Backend, host: &str, path: &str) {
        let existing = self.tabs.iter().enumerate().find_map(|(index, tab)| {
            let (view, project) = if index == self.active {
                (&self.view, &self.workbench.project)
            } else {
                (&tab.runtime.view, &tab.runtime.workbench.project)
            };
            let navigation_pending = project.pending.as_ref().is_none_or(|(_, _, method)| {
                matches!(
                    method.as_str(),
                    "local_project.list" | "local_project.resolve"
                )
            });
            (view == "project-workbench" && project.local_form.is_empty() && navigation_pending)
                .then_some(index)
        });
        if let Some(index) = existing {
            self.switch_tab(index);
            // 新目录取代旧定位，但不丢弃同一标签中的项目草稿和预览。
            self.workbench.project.epoch = self.workbench.project.epoch.wrapping_add(1);
            self.workbench.project.pending = None;
            self.workbench.project.local_project = None;
            self.workbench.project.local_environment = None;
            self.workbench.project.local_form.clear();
            self.workbench.project.section = "local".to_owned();
            self.workbench.project.local_error = None;
            self.workbench.project.local_navigation = Some((host.to_owned(), path.to_owned()));
            self.project_send(backend, "local_project.list", json!({}));
        } else {
            self.project_action("project-new", backend);
            self.workbench.project.local_navigation = Some((host.to_owned(), path.to_owned()));
        }
    }

    pub(crate) fn project_associate_resource(
        &mut self,
        backend: &Backend,
        profile_id: &str,
        database: Option<&str>,
        redis_db: Option<u32>,
    ) {
        self.project_action("project-new", backend);
        self.workbench.project.section = "local".to_owned();
        let defaults = self
            .profiles
            .iter()
            .find(|profile| profile.id.to_string() == profile_id)
            .map(|profile| {
                let db = profile.default_database().unwrap_or("").to_owned();
                let redis = match &profile.connection {
                    datapier_core::model::ConnectionConfig::Direct(connection) => {
                        Some(connection.redis_db)
                    }
                    datapier_core::model::ConnectionConfig::Archery(connection) => {
                        connection.default_database.parse().ok()
                    }
                };
                (db, redis)
            });
        self.project_set("local-profile", profile_id);
        self.project_set(
            "local-database",
            database
                .map(str::to_owned)
                .or_else(|| defaults.as_ref().map(|v| v.0.clone()))
                .unwrap_or_default(),
        );
        self.project_set(
            "local-redis-db",
            redis_db
                .or_else(|| defaults.and_then(|v| v.1))
                .map(|value| value.to_string())
                .unwrap_or_default(),
        );
        self.workbench.project.local_form = "resource".into();
        self.workbench.project.local_resource_edit = None;
        self.workbench.overlay = Some("project-local".into());
        self.project_discover_databases(backend);
    }
    pub(super) fn project_document_scan(&mut self, root: &str) {
        // 仅枚举已选择目录的直属文件；嵌套说明文件可由用户填写相对路径。
        let result = (|| -> std::io::Result<Vec<String>> {
            let mut files = Vec::new();
            for entry in std::fs::read_dir(root)? {
                let entry = entry?;
                if !entry.file_type()?.is_file() {
                    continue;
                }
                let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                let lower = name.to_ascii_lowercase();
                if lower.starts_with("readme")
                    || [".md", ".markdown", ".txt", ".rst", ".adoc"]
                        .iter()
                        .any(|suffix| lower.ends_with(suffix))
                {
                    files.push(name);
                }
            }
            files.sort();
            Ok(files)
        })();
        match result {
            Ok(files) => {
                self.workbench.project.document_root = Some(root.to_owned());
                self.workbench.project.document_files = files;
                self.project_set("document-path", "");
                self.status.clear();
            }
            Err(error) => {
                self.workbench.project.document_root = None;
                self.workbench.project.document_files.clear();
                self.status = i18n::UiMessage::raw(error.to_string());
            }
        }
    }

    pub(super) fn project_document_preview(&mut self, relative: &str, how: Option<&str>) {
        let Some(root) = self.workbench.project.document_root.as_deref() else {
            return;
        };
        let relative = std::path::Path::new(relative);
        if relative.as_os_str().is_empty()
            || !relative
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            self.status = i18n::UiMessage::key("请输入项目内文件的相对路径");
            return;
        }
        if how.is_some() && !is_project_markdown(relative) {
            self.status = i18n::UiMessage::key("请选择 Markdown 文件");
            return;
        }
        let result = (|| -> std::io::Result<std::path::PathBuf> {
            let root = std::fs::canonicalize(root)?;
            let path = std::fs::canonicalize(root.join(relative))?;
            if !path.starts_with(&root) || !path.is_file() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "文件不在项目目录内或不是普通文件",
                ));
            }
            Ok(path)
        })();
        match result {
            Ok(path) => {
                self.status.clear();
                self.workbench.project.open_file =
                    Some(json!({"id":Uuid::new_v4(),"path":path,"host":"local","how":how}));
            }
            Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
        }
    }

    fn local_project_select(&mut self, project: Value) {
        if self
            .workbench
            .project
            .local_project
            .as_ref()
            .map(|old| &old["id"])
            != Some(&project["id"])
            || self
                .workbench
                .project
                .document_root
                .as_ref()
                .is_some_and(|root| {
                    !project["roots"].as_array().is_some_and(|roots| {
                        roots
                            .iter()
                            .any(|item| item["path"].as_str() == Some(root.as_str()))
                    })
                })
        {
            self.workbench.project.document_root = None;
            self.workbench.project.document_files.clear();
        }
        self.project_set("local-name", project["name"].as_str().unwrap_or(""));
        self.project_set(
            "local-description",
            project["description"].as_str().unwrap_or(""),
        );
        self.workbench.project.local_environment =
            project["environments"].as_array().and_then(|items| {
                items
                    .iter()
                    .position(|item| item["id"] == project["default_environment_id"])
                    .or_else(|| (!items.is_empty()).then_some(0))
            });
        self.workbench.project.local_project = Some(project);
    }

    pub(super) fn local_environment_select(&mut self, index: usize) {
        let Some(environment) = self
            .workbench
            .project
            .local_project
            .as_ref()
            .and_then(|project| project["environments"].get(index))
            .cloned()
        else {
            return;
        };
        self.project_set(
            "local-environment-name",
            environment["name"].as_str().unwrap_or(""),
        );
        self.project_set(
            "local-environment-type",
            environment["environment_type"].as_str().unwrap_or(""),
        );
        let kind = environment["environment_type"].as_str().unwrap_or("");
        self.project_set(
            "local-environment-kind",
            if matches!(kind, "dev" | "test" | "staging" | "production") {
                kind
            } else {
                "custom"
            },
        );
        self.project_set(
            "local-environment-description",
            environment["description"].as_str().unwrap_or(""),
        );
        self.workbench.project.local_environment = Some(index);
    }

    fn local_project_save(&mut self, backend: &Backend, project: Value, creating: bool) {
        // 所有嵌套关系共用项目 revision，冲突时保留当前表单，不能自动覆盖。
        let revision = if creating {
            Value::Null
        } else {
            project["revision"].clone()
        };
        self.project_send(
            backend,
            "local_project.save",
            json!({"project":project,"expected_revision":revision}),
        );
    }

    pub(super) fn local_project_action(&mut self, name: &str, backend: &Backend) -> bool {
        if !name.starts_with("project-local-") {
            return false;
        }
        if name == "project-local-documents-git" {
            let root = self
                .workbench
                .project
                .project
                .as_ref()
                .and_then(|p| p["root"].as_str())
                .map(str::to_owned);
            if let Some(root) = root {
                self.project_document_scan(&root);
            }
            return true;
        }
        if let Some(index) = name
            .strip_prefix("project-local-documents:")
            .and_then(|s| s.parse::<usize>().ok())
        {
            let root = self
                .workbench
                .project
                .local_roots
                .get(index)
                .and_then(|root| root["path"].as_str())
                .map(str::to_owned);
            if let Some(root) = root {
                self.project_document_scan(&root);
            }
            return true;
        }
        for (action, prefix, how) in [
            ("open", "project-local-document:", None),
            ("beside", "project-local-document-beside:", Some("beside")),
            ("over", "project-local-document-over:", Some("over")),
        ] {
            if name.strip_prefix("project-local-document-") == Some(action) {
                let path = self.project_value("document-path").to_owned();
                self.project_document_preview(&path, how);
                return true;
            }
            if let Some(index) = name
                .strip_prefix(prefix)
                .and_then(|s| s.parse::<usize>().ok())
            {
                if let Some(path) = self.workbench.project.document_files.get(index).cloned() {
                    self.project_document_preview(&path, how);
                }
                return true;
            }
        }
        if let Some(relative) = name.strip_prefix("project-local-glimpse:") {
            let safe = std::path::Path::new(relative)
                .components()
                .all(|part| matches!(part, std::path::Component::Normal(_)));
            if let Some(project) = &self.workbench.project.project {
                if safe
                    && !self.workbench.project.unsaved
                    && project["digests"].get(relative).is_some()
                {
                    if let Some(root) = project["root"].as_str() {
                        let path = std::path::Path::new(root).join(relative);
                        self.workbench.project.open_file =
                            Some(json!({"id":Uuid::new_v4(),"path":path,"host":"local"}));
                    }
                } else {
                    self.status = i18n::UiMessage::key("请先保存项目文件");
                }
            }
            return true;
        }
        if self.workbench.project.pending.is_some() {
            self.status = i18n::UiMessage::key("请求处理中，请稍候");
            return true;
        }
        if name == "project-local-form-cancel" {
            self.workbench.project.local_form.clear();
            self.workbench.project.local_resource_edit = None;
            self.workbench.project.local_database_discovery = ProjectDatabaseDiscovery::default();
            self.status.clear();
            return true;
        }
        if !self.workbench.project.local_form.is_empty()
            && ((self.workbench.project.local_form != "resource"
                || self.workbench.project.local_resource_edit.is_some())
                && (name.starts_with("project-local-select:")
                    || name.starts_with("project-local-environment:"))
                || matches!(
                    name,
                    "project-local-new"
                        | "project-local-settings"
                        | "project-local-environment-new"
                        | "project-local-environment-edit"
                        | "project-local-resource-new"
                ))
        {
            self.status = i18n::UiMessage::key("请先保存或取消当前表单");
            return true;
        }
        match name {
            "project-local-settings" => {
                if let Some(project) = self.workbench.project.local_project.clone() {
                    self.project_set("local-name", project["name"].as_str().unwrap_or(""));
                    self.project_set(
                        "local-description",
                        project["description"].as_str().unwrap_or(""),
                    );
                    self.workbench.project.local_roots =
                        project["roots"].as_array().cloned().unwrap_or_default();
                }
                self.project_set("local-root", "");
                self.workbench.project.local_form = "settings".into();
                self.workbench.overlay = Some("project-local".into());
                return true;
            }
            "project-local-environment-new" => {
                for (field, value) in [
                    ("local-environment-name", ""),
                    ("local-environment-type", ""),
                    ("local-environment-kind", "dev"),
                    ("local-environment-description", ""),
                ] {
                    self.project_set(field, value);
                }
                self.workbench.project.local_form = "environment-new".into();
                self.workbench.overlay = Some("project-local".into());
                return true;
            }
            "project-local-environment-edit" => {
                if let Some(index) = self.workbench.project.local_environment {
                    self.local_environment_select(index);
                }
                self.workbench.project.local_form = "environment-edit".into();
                self.workbench.overlay = Some("project-local".into());
                return true;
            }
            "project-local-resource-new" => {
                for field in [
                    "local-profile",
                    "local-database",
                    "local-redis-db",
                    "local-resource-description",
                ] {
                    self.project_set(field, "");
                }
                self.workbench.project.local_resource_edit = None;
                self.workbench.project.local_database_discovery =
                    ProjectDatabaseDiscovery::default();
                self.workbench.project.local_form = "resource".into();
                self.workbench.overlay = Some("project-local".into());
                return true;
            }
            "project-local-database-retry" => {
                self.project_discover_databases(backend);
                return true;
            }
            "project-local-root-open" => {
                self.project_set("local-root", "");
                self.workbench.project.local_form = "root-add".into();
                return true;
            }
            "project-local-root-cancel" => {
                self.workbench.project.local_form =
                    if self.project_value("local-root").is_empty() {
                        "settings"
                    } else {
                        "root-cancel-confirm"
                    }
                    .into();
                return true;
            }
            "project-local-root-continue" => {
                self.workbench.project.local_form = "root-add".into();
                return true;
            }
            "project-local-root-discard" => {
                self.project_set("local-root", "");
                self.workbench.project.local_form = "settings".into();
                return true;
            }
            "project-local-root-add" => {
                let path = self.project_value("local-root").trim().to_owned();
                if !path.is_empty() {
                    self.workbench
                        .project
                        .local_roots
                        .push(json!({"path":path,"identity":""}));
                    self.project_set("local-root", "");
                    self.workbench.project.local_form = "settings".into();
                }
                return true;
            }
            _ => {}
        }
        if let Some(index) = name
            .strip_prefix("project-local-root-delete:")
            .and_then(|v| v.parse::<usize>().ok())
        {
            if index < self.workbench.project.local_roots.len() {
                self.workbench.project.local_roots.remove(index);
            }
            return true;
        }
        if let Some(id) = name.strip_prefix("project-local-target:") {
            self.workbench.generic_select = None;
            if self
                .workbench
                .project
                .local_projects
                .iter()
                .any(|project| project["id"].as_str() == Some(id))
            {
                self.project_send(backend, "local_project.get", json!({"project_id":id}));
            }
            return true;
        }
        if let Some(index) = name
            .strip_prefix("project-local-select:")
            .and_then(|v| v.parse::<usize>().ok())
        {
            self.workbench.project.local_navigation = None;
            self.workbench.project.local_error = None;
            if let Some(id) = self
                .workbench
                .project
                .local_projects
                .get(index)
                .map(|project| project["id"].clone())
            {
                self.project_send(backend, "local_project.get", json!({"project_id":id}));
            }
            return true;
        }
        if let Some(index) = name
            .strip_prefix("project-local-environment:")
            .and_then(|v| v.parse::<usize>().ok())
        {
            self.local_environment_select(index);
            return true;
        }
        if name == "project-local-new" {
            self.workbench.project.local_form = "project-new".into();
            for (field, value) in [
                ("local-name", ""),
                ("local-description", ""),
                ("local-root", ""),
            ] {
                self.project_set(field, value);
            }
            self.workbench.overlay = Some("project-local".into());
            return true;
        }
        if name == "project-local-refresh" {
            self.workbench.project.local_error = None;
            self.project_send(backend, "local_project.list", json!({}));
            return true;
        }
        if name == "project-local-create" {
            let id = Uuid::new_v4();
            let project = json!({"id":id,"name":self.project_value("local-name"),"description":self.project_value("local-description"),"revision":0,"default_environment_id":null,"roots":[],"environments":[]});
            self.local_project_save(backend, project, true);
            return true;
        }
        let Some(mut project) = self.workbench.project.local_project.clone() else {
            return true;
        };
        if name == "project-local-delete" {
            self.workbench.project.local_delete_confirm = true;
            return true;
        }
        if name == "project-local-delete-cancel" {
            self.workbench.project.local_delete_confirm = false;
            return true;
        }
        if name == "project-local-delete-confirm" {
            self.workbench.project.local_delete_confirm = false;
            self.project_send(
                backend,
                "local_project.delete",
                json!({"project_id":project["id"],"expected_revision":project["revision"]}),
            );
            return true;
        }
        let mut changed = true;
        match name {
            "project-local-clear-default" => project["default_environment_id"] = Value::Null,
            "project-local-save" => {
                project["name"] = json!(self.project_value("local-name"));
                project["description"] = json!(self.project_value("local-description"));
                if self.workbench.project.local_form == "settings" {
                    project["roots"] = json!(self.workbench.project.local_roots);
                }
            }
            "project-local-environment-add" => {
                let kind = if self.project_value("local-environment-kind") == "custom" {
                    self.project_value("local-environment-type")
                } else {
                    self.project_value("local-environment-kind")
                };
                project["environments"].as_array_mut().unwrap().push(json!({"id":Uuid::new_v4(),"name":self.project_value("local-environment-name"),"environment_type":kind,"description":self.project_value("local-environment-description"),"resources":[]}));
            }
            "project-local-environment-save"
            | "project-local-default"
            | "project-local-environment-delete"
            | "project-local-resource-save" => {
                let Some(index) = self.workbench.project.local_environment else {
                    return true;
                };
                let Some(environment) = project["environments"].get_mut(index) else {
                    return true;
                };
                match name {
                    "project-local-environment-save" => {
                        let kind = if self.project_value("local-environment-kind") == "custom" {
                            self.project_value("local-environment-type")
                        } else {
                            self.project_value("local-environment-kind")
                        };
                        environment["name"] = json!(self.project_value("local-environment-name"));
                        environment["environment_type"] = json!(kind);
                        environment["description"] =
                            json!(self.project_value("local-environment-description"));
                    }
                    "project-local-default" => {
                        let id = environment["id"].clone();
                        project["default_environment_id"] = id;
                    }
                    "project-local-environment-delete" => {
                        let id = environment["id"].clone();
                        project["environments"]
                            .as_array_mut()
                            .unwrap()
                            .remove(index);
                        if project["default_environment_id"] == id {
                            project["default_environment_id"] = Value::Null;
                        }
                    }
                    _ => {
                        let Some(profile) = self
                            .profiles
                            .iter()
                            .find(|p| p.id.to_string() == self.project_value("local-profile"))
                        else {
                            self.status = i18n::UiMessage::key("请选择连接");
                            return true;
                        };
                        let is_redis =
                            profile.provider == datapier_core::model::ProviderKind::Redis;
                        let database = if is_redis {
                            ""
                        } else {
                            self.project_value("local-database").trim()
                        };
                        let redis = if is_redis {
                            self.project_value("local-redis-db").trim()
                        } else {
                            ""
                        };
                        let redis_db = if redis.is_empty() {
                            None
                        } else if let Ok(value) = redis.parse::<u32>() {
                            Some(value)
                        } else {
                            self.status = i18n::UiMessage::key("Redis DB 必须为非负整数");
                            return true;
                        };
                        if (!is_redis && database.is_empty()) || (is_redis && redis_db.is_none()) {
                            self.status = i18n::UiMessage::key("请填写数据库");
                            return true;
                        }
                        let discovery = &self.workbench.project.local_database_discovery;
                        let selected = if is_redis { redis } else { database };
                        if !discovery.loaded
                            || discovery.profile_id != self.project_value("local-profile")
                            || !discovery.databases.iter().any(|db| db == selected)
                        {
                            self.status = i18n::UiMessage::key("请选择已发现的数据库");
                            return true;
                        }
                        let resource = json!({"profile_id":self.project_value("local-profile"),"database":if database.is_empty(){None}else{Some(database)},"redis_db":redis_db,"description":self.project_value("local-resource-description")});
                        let resources = environment["resources"].as_array_mut().unwrap();
                        if resources.iter().enumerate().any(|(index, item)| {
                            Some(index) != self.workbench.project.local_resource_edit
                                && item["profile_id"] == resource["profile_id"]
                                && item["database"] == resource["database"]
                                && item["redis_db"] == resource["redis_db"]
                        }) {
                            self.status = i18n::UiMessage::key("该数据库已关联");
                            return true;
                        }
                        if let Some(index) = self.workbench.project.local_resource_edit {
                            let Some(existing) = resources.get_mut(index) else {
                                return true;
                            };
                            *existing = resource;
                        } else {
                            resources.push(resource);
                        }
                    }
                }
            }
            _ => {
                if let Some(index) = name
                    .strip_prefix("project-local-resource-delete:")
                    .and_then(|v| v.parse::<usize>().ok())
                {
                    if let Some(environment) = self
                        .workbench
                        .project
                        .local_environment
                        .and_then(|i| project["environments"].get_mut(i))
                    {
                        if index < environment["resources"].as_array().map_or(0, Vec::len) {
                            environment["resources"]
                                .as_array_mut()
                                .unwrap()
                                .remove(index);
                        } else {
                            changed = false;
                        }
                    } else {
                        changed = false;
                    }
                } else if let Some(index) = name
                    .strip_prefix("project-local-resource:")
                    .and_then(|v| v.parse::<usize>().ok())
                {
                    if let Some(resource) = self
                        .workbench
                        .project
                        .local_environment
                        .and_then(|i| project["environments"].get(i))
                        .and_then(|e| e["resources"].get(index))
                        .cloned()
                    {
                        for (field, key) in [
                            ("local-profile", "profile_id"),
                            ("local-database", "database"),
                            ("local-resource-description", "description"),
                        ] {
                            self.project_set(field, resource[key].as_str().unwrap_or(""));
                        }
                        self.project_set(
                            "local-redis-db",
                            resource["redis_db"]
                                .as_u64()
                                .map(|v| v.to_string())
                                .unwrap_or_default(),
                        );
                        self.workbench.project.local_resource_edit = Some(index);
                        self.workbench.project.local_form = "resource".into();
                        self.workbench.overlay = Some("project-local".into());
                        self.project_discover_databases(backend);
                    }
                    changed = false;
                } else {
                    changed = false;
                }
            }
        }
        if changed {
            self.local_project_save(backend, project, false);
        }
        true
    }

    pub(super) fn local_project_error(
        &mut self,
        method: &str,
        error: &impl std::fmt::Display,
    ) -> bool {
        if matches!(method, "local_project.list" | "local_project.resolve") {
            self.workbench.project.local_error = Some(i18n::UiMessage::raw(error.to_string()));
            true
        } else {
            false
        }
    }

    pub(super) fn project_database_discovery_response(&mut self, result: &Result<Value>) {
        let discovery = &self.workbench.project.local_database_discovery;
        let profile_id = discovery.profile_id.clone();
        let profile = self
            .profiles
            .iter()
            .find(|profile| profile.id.to_string() == profile_id);
        let visible = profile.and_then(|p| match &p.connection {
            datapier_core::model::ConnectionConfig::Direct(direct) => {
                direct.visible_databases.as_ref()
            }
            _ => None,
        });
        let redis =
            profile.is_some_and(|p| p.provider == datapier_core::model::ProviderKind::Redis);
        let parsed = result
            .as_ref()
            .map_err(|e| e.to_string())
            .and_then(|value| {
                if value["state"] != "known" {
                    return Err(value["reason"]
                        .as_str()
                        .unwrap_or("数据库发现失败 / Database discovery failed")
                        .to_owned());
                }
                let items = value["databases"].as_array().ok_or_else(|| {
                    "数据库列表响应无效 / Invalid database list response".to_owned()
                })?;
                Ok(items
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|db| {
                        (!redis || db.parse::<u32>().is_ok())
                            && visible.is_none_or(|v| v.iter().any(|item| item.as_str() == *db))
                    })
                    .map(str::to_owned)
                    .collect::<Vec<_>>())
            });
        let discovery = &mut self.workbench.project.local_database_discovery;
        discovery.token = None;
        match parsed {
            Ok(databases) => {
                discovery.databases = databases;
                discovery.loaded = true;
                discovery.error = None;
            }
            Err(error) => {
                discovery.databases.clear();
                discovery.loaded = false;
                discovery.error = Some(error);
            }
        }
    }

    pub(super) fn local_project_response(
        &mut self,
        method: &str,
        value: &Value,
        backend: &Backend,
    ) {
        match method {
            "local_project.list" => {
                self.workbench.project.local_projects =
                    value["projects"].as_array().cloned().unwrap_or_default();
                self.workbench.project.local_loaded = true;
                self.workbench.project.local_error = None;
                if let Some((host, path)) = self.workbench.project.local_navigation.clone() {
                    self.project_send(
                        backend,
                        "local_project.resolve",
                        json!({"host":host,"path":path}),
                    );
                } else if self.workbench.project.local_project.is_none()
                    && matches!(self.workbench.project.local_form.as_str(), "" | "resource")
                    && let Some(project) = self.workbench.project.local_projects.first().cloned()
                {
                    self.local_project_select(project);
                }
            }
            "local_project.resolve" => {
                let state = &mut self.workbench.project;
                let message = match value["status"].as_str() {
                    Some("matched")
                        if value["project"]["id"].as_str().is_some()
                            && state
                                .local_projects
                                .iter()
                                .any(|project| project["id"] == value["project"]["id"]) =>
                    {
                        None
                    }
                    Some("matched") => Some("项目列表已变化，请刷新后重新定位"),
                    Some("unassociated") => Some("目录未关联项目"),
                    Some("identity_changed") => Some("项目目录身份已改变"),
                    Some("unavailable") => Some("目录不可用"),
                    Some("remote") => Some("目录不在本机"),
                    _ => Some("项目定位结果无效"),
                };
                if let Some(message) = message {
                    state.local_error = Some(i18n::UiMessage::key(message));
                } else {
                    state.local_navigation = None;
                    state.local_error = None;
                    self.local_project_select(value["project"].clone());
                }
            }
            "local_project.get" | "local_project.save" => {
                if method == "local_project.save" {
                    self.workbench.project.local_form.clear();
                    self.workbench.project.local_resource_edit = None;
                    if self.workbench.overlay.as_deref() == Some("project-local") {
                        self.workbench_action("overlay-close", backend);
                    }
                }
                let previous = self.workbench.project.local_environment.and_then(|index| {
                    self.workbench
                        .project
                        .local_project
                        .as_ref()
                        .and_then(|p| p["environments"].get(index))
                        .map(|e| e["id"].clone())
                });
                self.local_project_select(value["project"].clone());
                if let Some(index) = previous.and_then(|id| {
                    value["project"]["environments"]
                        .as_array()
                        .and_then(|items| items.iter().position(|e| e["id"] == id))
                }) {
                    self.local_environment_select(index);
                }
                self.project_send(backend, "local_project.list", json!({}));
            }
            "local_project.delete" => {
                self.workbench.project.local_project = None;
                self.workbench.project.local_environment = None;
                self.workbench.project.local_form.clear();
                self.workbench.project.local_delete_confirm = false;
                if self.workbench.overlay.as_deref() == Some("project-local") {
                    self.workbench_action("overlay-close", backend);
                }
                self.project_send(backend, "local_project.list", json!({}));
            }
            _ => unreachable!(),
        }
    }
}
