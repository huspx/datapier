use super::*;

#[derive(Clone)]
struct ProjectUpload {
    id: Option<Value>,
    revision: u64,
    project_id: Value,
    manifest: Value,
    replace: Value,
    files: Vec<(String, String)>,
    index: usize,
    offset: usize,
}

pub(super) fn external_deletions(
    baseline: &Value,
    external: &Value,
    old_digests: &Value,
) -> Vec<(String, usize, String)> {
    let mut result = Vec::new();
    for section in ["connections", "queries", "transfers"] {
        let key = if section == "connections" {
            "alias"
        } else {
            "id"
        };
        if let Some(items) = baseline[section].as_array() {
            for (index, item) in items.iter().enumerate() {
                let missing_entry = !external["manifest"][section]
                    .as_array()
                    .is_some_and(|current| current.iter().any(|other| other[key] == item[key]));
                let missing_file = item["path"].as_str().is_some_and(|path| {
                    old_digests.get(path).is_some()
                        && external["digests"].get(path).is_none()
                        && external["manifest"][section]
                            .as_array()
                            .is_some_and(|current| {
                                current.iter().any(|other| {
                                    other[key] == item[key] && other["path"] == item["path"]
                                })
                            })
                });
                if missing_entry || missing_file {
                    result.push((
                        section.into(),
                        index,
                        item["name"]
                            .as_str()
                            .or_else(|| item["alias"].as_str())
                            .unwrap_or("")
                            .to_owned(),
                    ));
                }
            }
        }
    }
    result
}
pub(super) fn external_changes(
    baseline: &Value,
    external: &Value,
    old_digests: &Value,
) -> Vec<(String, usize, String)> {
    let mut result = Vec::new();
    for section in ["connections", "queries", "transfers"] {
        let key = if section == "connections" {
            "alias"
        } else {
            "id"
        };
        if let Some(items) = external["manifest"][section].as_array() {
            for (index, item) in items.iter().enumerate() {
                let old = baseline[section]
                    .as_array()
                    .and_then(|items| items.iter().find(|old| old[key] == item[key]));
                let body_changed = item["path"].as_str().is_some_and(|path| {
                    old_digests.get(path) != external["digests"].get(path)
                        && external["digests"].get(path).is_some()
                });
                let deleted_body = old.and_then(|v| v["path"].as_str()).is_some_and(|path| {
                    item["path"].as_str() == Some(path)
                        && old_digests.get(path).is_some()
                        && external["digests"].get(path).is_none()
                });
                if !deleted_body && (old != Some(item) || body_changed) {
                    result.push((
                        section.into(),
                        index,
                        item["name"]
                            .as_str()
                            .or_else(|| item["alias"].as_str())
                            .unwrap_or("")
                            .to_owned(),
                    ));
                }
            }
        }
    }
    result
}

#[derive(Clone, Default)]
pub(super) struct ProjectDatabaseDiscovery {
    pub profile_id: String,
    pub token: Option<Uuid>,
    pub databases: Vec<String>,
    pub loaded: bool,
    pub error: Option<String>,
}

pub(super) fn is_project_markdown(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|extension| {
            ["md", "markdown"]
                .iter()
                .any(|kind| extension.eq_ignore_ascii_case(kind))
        })
}

#[derive(Clone, Default)]
pub(super) struct ProjectWorkbench {
    epoch: u64,
    pending: Option<(Uuid, u64, String)>,
    upload: Option<ProjectUpload>,
    pub project: Option<Value>,
    // 恢复只保存本机草稿与磁盘基线；项目句柄必须由用户重新打开。
    pub restored: Option<Value>,
    pub needs_baseline_check: bool,
    pub baseline_manifest: Option<Value>,
    pub deletion_decisions: std::collections::BTreeSet<String>,
    pub change_decisions: std::collections::BTreeSet<String>,
    pub bindings: Option<Value>,
    pub binding_group: String,
    pub binding_offset: u64,
    pub binding_history: Vec<u64>,
    binding_page_request: Option<(u64, u8)>,
    pub preview: Option<Value>,
    pub obsolete_preview: Option<Value>,
    pub unsaved: bool,
    pub close_confirm: bool,
    pub result: Option<Value>,
    pub artifacts: Vec<Value>,
    pub artifact_offset: u64,
    pub artifact_history: Vec<u64>,
    artifact_page_request: Option<(u64, u8)>,
    pub artifact_next: Option<u64>,
    pub artifact: Option<Value>,
    pub recovery: Option<Value>,
    pub recovery_preview: Option<Value>,
    pub recovery_action: String,
    pub file_next: Option<u64>,
    pub file_loading: bool,
    pub reload_confirm: bool,
    pub file_buffer: String,
    pub external: Option<Value>,
    pub external_text: String,
    pub external_next: Option<u64>,
    external_read_path: Option<String>,
    external_complete_path: Option<String>,
    pub transfer_files: BTreeMap<String, String>,
    pub section: String,
    pub transfer_loading: Option<(String, String)>,
    pub query_loading: Option<Value>,
    pub cleanup_confirm: bool,
    pub local_projects: Vec<Value>,
    pub local_project: Option<Value>,
    pub local_environment: Option<usize>,
    pub local_form: String,
    pub local_roots: Vec<Value>,
    pub local_resource_edit: Option<usize>,
    pub local_database_discovery: ProjectDatabaseDiscovery,
    pub git_binding_edit: bool,
    pub git_connection_new: bool,
    pub git_query_edit: bool,
    pub local_loaded: bool,
    pub open_file: Option<Value>,
    pub local_delete_confirm: bool,
    pub document_root: Option<String>,
    pub document_files: Vec<String>,
}
impl ProjectWorkbench {
    pub fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        if let Some(preview) = self.preview.take() {
            self.obsolete_preview = Some(preview);
        }
        self.pending = None;
        // 已失效的异步回复不得覆盖新输入；上传句柄由 Engine 过期清理。
        self.upload = None;
        self.file_loading = false;
        self.file_next = None;
        self.file_buffer.clear();
        self.query_loading = None;
        self.transfer_loading = None;
        self.binding_page_request = None;
        self.artifact_page_request = None;
    }
    pub fn is_pending(&self) -> bool {
        self.pending.is_some() || self.file_loading || self.upload.is_some()
    }
}
impl App {
    fn project_discover_databases(&mut self, backend: &Backend) {
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

    pub(super) fn project_key(&self, name: &str) -> String {
        format!("project-{}-{name}", self.tabs[self.active].id)
    }
    fn project_value(&self, name: &str) -> &str {
        self.field(&self.project_key(name))
    }
    fn project_set(&mut self, name: &str, value: impl Into<String>) {
        let key = self.project_key(name);
        self.field_set(&key, value);
    }
    pub(crate) fn project_form_snapshot(&self) -> Value {
        let fields: &[&str] = match self.workbench.project.local_form.as_str() {
            "project-new" => &["local-name", "local-description"],
            "settings" | "root-add" | "root-cancel-confirm" => {
                &["local-name", "local-description", "local-root"]
            }
            "environment-new" | "environment-edit" => &[
                "local-environment-name",
                "local-environment-kind",
                "local-environment-type",
                "local-environment-description",
            ],
            "resource" => &[
                "local-profile",
                "local-database",
                "local-redis-db",
                "local-resource-description",
            ],
            _ => &[],
        };
        let values: Vec<_> = fields
            .iter()
            .map(|field| self.project_value(field))
            .collect();
        if matches!(
            self.workbench.project.local_form.as_str(),
            "settings" | "root-add" | "root-cancel-confirm"
        ) {
            json!([values, self.workbench.project.local_roots])
        } else {
            json!(values)
        }
    }
    fn project_send(&mut self, backend: &Backend, method: &str, params: Value) {
        if !matches!(
            method,
            "project.bindings.get"
                | "project.file.read"
                | "project.recovery.read"
                | "transfer.artifact.list"
                | "transfer.artifact.get"
        ) {
            self.status.clear();
        }
        let token = Uuid::new_v4();
        self.workbench.project.pending = Some((token, self.workbench.project.epoch, method.into()));
        let tag = format!(
            "{}:{}:project-ui/{token}",
            self.tabs[self.active].id, self.generation
        );
        if let Err(error) = backend.request(tag, method, params) {
            self.workbench.project.binding_page_request = None;
            self.workbench.project.artifact_page_request = None;
            if method == "project.file.read" {
                self.workbench.project.file_loading = false;
                self.workbench.project.file_next = None;
                self.workbench.project.file_buffer.clear();
                self.workbench.project.transfer_loading = None;
                self.workbench.project.query_loading = None;
            }
            if let Some(upload) = self.workbench.project.upload.take()
                && let Some(id) = upload.id
            {
                let _ = backend.request(
                    "project.upload.cleanup",
                    "project.save.upload.cancel",
                    json!({"upload_id":id,"revision":upload.revision}),
                );
            }
            self.workbench.project.pending = None;
            self.status = i18n::UiMessage::raw(error.to_string());
        }
    }
    fn project_upload_next(&mut self, backend: &Backend) {
        let Some(upload) = &mut self.workbench.project.upload else {
            return;
        };
        let Some(id) = upload.id.clone() else {
            return;
        };
        while upload.index < upload.files.len()
            && upload.offset == upload.files[upload.index].1.len()
            && upload.offset != 0
        {
            upload.index += 1;
            upload.offset = 0;
        }
        let (method, params) = if let Some((path, text)) = upload.files.get(upload.index) {
            let mut end = (upload.offset + 256 * 1024).min(text.len());
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let params = json!({"upload_id":id,"revision":upload.revision,"path":path,
                "offset":upload.offset,"text":&text[upload.offset..end]});
            upload.offset = end;
            if text.is_empty() {
                upload.index += 1;
            }
            ("project.save.upload.chunk", params)
        } else {
            let params = json!({"project_id":upload.project_id,"manifest":upload.manifest,
                "upload_id":id,"revision":upload.revision,"replace_plan_id":upload.replace});
            ("project.save.prepare", params)
        };
        self.project_send(backend, method, params);
    }
    pub(super) fn project_response(
        &mut self,
        tag: &str,
        result: &Result<Value>,
        backend: &Backend,
    ) -> bool {
        if let Some(token) = tag
            .strip_prefix("project-db/")
            .and_then(|s| Uuid::parse_str(s).ok())
        {
            let discovery = &self.workbench.project.local_database_discovery;
            if discovery.token != Some(token)
                || self.workbench.overlay.as_deref() != Some("project-local")
                || self.workbench.project.local_form != "resource"
                || discovery.profile_id != self.project_value("local-profile")
            {
                return true;
            }
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
            return true;
        }
        let Some(token) = tag
            .strip_prefix("project-ui/")
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            return false;
        };
        let Some((expected, epoch, method)) = self.workbench.project.pending.take() else {
            return true;
        };
        if token != expected || epoch != self.workbench.project.epoch {
            self.workbench.project.pending = Some((expected, epoch, method));
            return true;
        }
        match result {
            Err(error) => {
                self.workbench.project.binding_page_request = None;
                self.workbench.project.artifact_page_request = None;
                if let Some(upload) = self.workbench.project.upload.take()
                    && let Some(id) = upload.id
                {
                    let _ = backend.request(
                        "project.upload.cleanup",
                        "project.save.upload.cancel",
                        json!({"upload_id":id,"revision":upload.revision}),
                    );
                }
                if method == "project.file.read" {
                    self.workbench.project.file_loading = false;
                    self.workbench.project.file_next = None;
                    self.workbench.project.file_buffer.clear();
                    self.workbench.project.transfer_loading = None;
                    self.workbench.project.query_loading = None;
                }
                // Engine 可能在新预览准备失败前已回收被替换的计划。
                if method == "project.save.prepare" || method == "project.save.commit" {
                    self.workbench.project.preview = None;
                    self.workbench.project.obsolete_preview = None;
                }
                self.status = i18n::UiMessage::raw(error.to_string());
            }
            Ok(value) => match method.as_str() {
                "local_project.list" => {
                    self.workbench.project.local_projects =
                        value["projects"].as_array().cloned().unwrap_or_default();
                    if self.workbench.project.local_project.is_none()
                        && matches!(self.workbench.project.local_form.as_str(), "" | "resource")
                        && let Some(project) =
                            self.workbench.project.local_projects.first().cloned()
                    {
                        self.local_project_select(project);
                    }
                    self.workbench.project.local_loaded = true;
                }
                "local_project.resolve" => {
                    if value["status"] == "matched" {
                        self.local_project_select(value["project"].clone());
                    } else {
                        self.status = i18n::UiMessage::raw(format!(
                            "仓库无法定位 / Repository could not be resolved: {}",
                            value["status"].as_str().unwrap_or("unknown")
                        ));
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
                "project.save.upload.begin" | "project.save.upload.chunk" => {
                    let valid = self
                        .workbench
                        .project
                        .upload
                        .as_ref()
                        .is_some_and(|upload| {
                            let revision = value["revision"].as_u64();
                            if method == "project.save.upload.begin" {
                                revision == Some(1) && value["upload_id"].as_str().is_some()
                            } else {
                                revision == upload.revision.checked_add(1)
                                    && value["offset"].as_u64() == Some(upload.offset as u64)
                            }
                        });
                    if !valid {
                        self.workbench.project.upload = None;
                        self.status = i18n::UiMessage::raw(
                            "项目上传确认偏移或版本无效，请重新准备".to_owned(),
                        );
                        return true;
                    }
                    if let Some(upload) = &mut self.workbench.project.upload {
                        if method == "project.save.upload.begin" {
                            upload.id = value.get("upload_id").cloned();
                        }
                        if let Some(revision) = value["revision"].as_u64() {
                            upload.revision = revision;
                        } else {
                            self.workbench.project.upload = None;
                            self.status = i18n::UiMessage::raw("项目上传响应缺少版本".to_owned());
                            return true;
                        }
                        if upload.id.is_none() {
                            self.workbench.project.upload = None;
                            self.status = i18n::UiMessage::raw("项目上传响应缺少句柄".to_owned());
                            return true;
                        }
                    }
                    self.project_upload_next(backend);
                }
                "project.open" | "project.create" | "project.import" => {
                    self.workbench.project.document_root = None;
                    self.workbench.project.document_files.clear();
                    self.workbench.project.baseline_manifest = Some(value["manifest"].clone());
                    let mut restored = false;
                    let mut opened = value.clone();
                    if let Some(saved) = self.workbench.project.restored.take() {
                        if saved["root"] != opened["root"] {
                            let _ = backend.request(
                                "project.restore.cleanup",
                                "project.close",
                                json!({"project_id":opened["project_id"],"discard_previews":true}),
                            );
                            self.workbench.project.restored = Some(saved);
                            self.status = i18n::UiMessage::raw(
                                "恢复的草稿属于另一项目目录，请打开原目录".to_owned(),
                            );
                            return true;
                        }
                        restored = true;
                        // 保留原基线，磁盘变化仍由保存冲突检查处理。
                        opened["manifest"] = saved["manifest"].clone();
                        opened["digests"] = saved["digests"].clone();
                        self.workbench.project.baseline_manifest = Some(
                            saved
                                .get("baseline_manifest")
                                .filter(|value| !value.is_null())
                                .cloned()
                                .unwrap_or_else(|| saved["manifest"].clone()),
                        );
                    } else {
                        self.workbench.project.transfer_files.clear();
                        self.workbench.project.unsaved = false;
                    }
                    self.workbench.project.project = Some(opened);
                    self.workbench.project.binding_offset = 0;
                    self.workbench.project.binding_history.clear();
                    self.workbench.project.artifact_offset = 0;
                    self.workbench.project.artifact_history.clear();
                    self.workbench.project.artifacts.clear();
                    self.workbench.project.artifact_next = None;
                    self.workbench.project.bindings = None;
                    self.workbench.project.preview = None;
                    self.workbench.project.obsolete_preview = None;
                    self.workbench.project.external = None;
                    self.project_set("path", value["root"].as_str().unwrap_or(""));
                    if restored {
                        self.workbench.project.needs_baseline_check = true;
                        self.project_send(
                            backend,
                            "project.external.read",
                            json!({"project_id":value["project_id"]}),
                        );
                    }
                }
                "project.bindings.confirm" => {
                    if let Some(bindings) = &mut self.workbench.project.bindings {
                        bindings["confirmed"] = json!(true);
                    }
                }
                "project.bindings.get" => {
                    if let Some((offset, direction)) =
                        self.workbench.project.binding_page_request.take()
                    {
                        match direction {
                            0 => self.workbench.project.binding_history.clear(),
                            1 => self
                                .workbench
                                .project
                                .binding_history
                                .push(self.workbench.project.binding_offset),
                            _ => {
                                self.workbench.project.binding_history.pop();
                            }
                        }
                        self.workbench.project.binding_offset = offset;
                    }
                    self.workbench.project.bindings = Some(value.clone());
                }
                "project.bindings.save" => {
                    // 修改绑定后旧摘要不再可确认，用户需重新读取当前绑定组。
                    self.workbench.project.bindings = None;
                }
                "project.save.prepare" => {
                    self.workbench.project.upload = None;
                    self.workbench.project.obsolete_preview = None;
                    self.workbench.project.preview = Some(value.clone());
                    self.workbench.project.section = "save".into();
                }
                "project.stages.discover" => {
                    self.project_send(
                        backend,
                        "transfer.artifact.list",
                        json!({"offset":self.workbench.project.artifact_page_request.map(|p| p.0).unwrap_or(0),"limit":100}),
                    );
                }
                "transfer.artifact.list" => {
                    if let Some((offset, direction)) =
                        self.workbench.project.artifact_page_request.take()
                    {
                        match direction {
                            0 => self.workbench.project.artifact_history.clear(),
                            1 => self
                                .workbench
                                .project
                                .artifact_history
                                .push(self.workbench.project.artifact_offset),
                            _ => {
                                self.workbench.project.artifact_history.pop();
                            }
                        }
                        self.workbench.project.artifact_offset = offset;
                    }
                    self.workbench.project.artifacts =
                        value["artifacts"].as_array().cloned().unwrap_or_default();
                    self.workbench.project.artifact_next = value["next"].as_u64();
                }
                "transfer.artifact.get" => {
                    let mut artifact = value["artifact"].clone();
                    if let Some(identity) = value.get("observed_identity") {
                        artifact["observed_identity"] = identity.clone();
                    }
                    self.workbench.project.artifact = Some(artifact);
                }
                "transfer.artifact.claim" => {
                    self.workbench.project.artifact = Some(value["artifact"].clone())
                }
                "project.recovery.read" => self.workbench.project.recovery = Some(value.clone()),
                "project.external.read" => {
                    if value.get("manifest").is_some() {
                        self.workbench.project.needs_baseline_check = false;
                        self.workbench.project.deletion_decisions.clear();
                        self.workbench.project.change_decisions.clear();
                        self.workbench.project.external_read_path = None;
                        self.workbench.project.external_complete_path = None;
                        self.workbench.project.external = Some(value.clone());
                        self.workbench.project.external_text.clear();
                        self.workbench.project.external_next = None;
                    } else if self
                        .workbench
                        .project
                        .external
                        .as_ref()
                        .is_some_and(|v| v["digests"] == value["digests"])
                    {
                        if value["deleted"] == true {
                            self.workbench.project.external_text =
                                "外部文件已删除，请选择合并删除或保留本地正文。".to_owned();
                        }
                        if value["deleted"] != true {
                            self.workbench
                                .project
                                .external_text
                                .push_str(value["text"].as_str().unwrap_or(""));
                        }
                        self.workbench.project.external_next = value["next"].as_u64();
                        if value["deleted"] != true
                            && self.workbench.project.external_next.is_none()
                        {
                            self.workbench.project.external_complete_path =
                                self.workbench.project.external_read_path.clone();
                        }
                    } else {
                        self.workbench.project.external = None;
                        self.status =
                            i18n::UiMessage::raw("外部项目再次改变，请重新查看".to_owned());
                    }
                }
                "project.external.accept" => {
                    self.workbench.project.baseline_manifest = self
                        .workbench
                        .project
                        .external
                        .as_ref()
                        .map(|e| e["manifest"].clone());
                    if let Some(project) = &mut self.workbench.project.project {
                        project["digests"] = value["digests"].clone();
                    }
                    self.workbench.project.external = None;
                    self.workbench.project.invalidate();
                }
                "project.file.read" => {
                    let text = value["text"].as_str().unwrap_or("");
                    self.workbench.project.file_buffer.push_str(text);
                    self.workbench.project.file_next = value["next"].as_u64();
                    self.workbench.project.file_loading =
                        self.workbench.project.file_next.is_some();
                    if !self.workbench.project.file_loading {
                        let text = std::mem::take(&mut self.workbench.project.file_buffer);
                        if let Some((name, path)) = self.workbench.project.transfer_loading.take() {
                            match serde_json::from_str::<Value>(&text) {
                                Ok(spec) => {
                                    self.transfer_load_preset(json!({"name":name,"spec":spec}));
                                    self.field_set(&self.transfer_key("project-path"), &path);
                                    self.view = "transfer-workbench".into();
                                }
                                Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
                            }
                        } else {
                            if let Some(query) = self.workbench.project.query_loading.take() {
                                self.project_set(
                                    "query-name",
                                    query["name"].as_str().unwrap_or(""),
                                );
                                self.project_set(
                                    "query-path",
                                    query["path"].as_str().unwrap_or(""),
                                );
                                self.project_set(
                                    "query-alias",
                                    query["connection_alias"].as_str().unwrap_or(""),
                                );
                            }
                            self.project_set("query-text", text);
                        }
                    }
                }
                "project.recovery.prepare" => {
                    self.workbench.project.recovery_preview = Some(value.clone())
                }
                "project.recovery.commit" | "transfer.artifact.cleanup" => {
                    self.workbench.project.recovery_preview = None;
                    self.workbench.project.result = Some(value.clone());
                }
                "project.save.commit" => {
                    self.workbench.project.baseline_manifest =
                        value.get("manifest").cloned().or_else(|| {
                            self.workbench
                                .project
                                .project
                                .as_ref()
                                .map(|p| p["manifest"].clone())
                        });
                    if let Some(project) = &mut self.workbench.project.project {
                        if let Some(manifest) = value.get("manifest") {
                            project["manifest"] = manifest.clone();
                        }
                        if let Some(digests) = value.get("digests") {
                            project["digests"] = digests.clone();
                        }
                    }
                    self.workbench.project.preview = None;
                    self.workbench.project.transfer_files.clear();
                    self.workbench.project.unsaved = false;
                    self.workbench.project.result = Some(value.clone());
                }
                "project.save.discard" => {
                    self.workbench.project.preview = None;
                    self.workbench.project.obsolete_preview = None;
                }
                "project.close" => {
                    self.workbench.project.project = None;
                    self.workbench.project.bindings = None;
                    self.workbench.project.binding_history.clear();
                    self.workbench.project.binding_offset = 0;
                    self.workbench.project.artifact_history.clear();
                    self.workbench.project.artifact_offset = 0;
                    self.workbench.project.artifacts.clear();
                    self.workbench.project.artifact_next = None;
                    self.workbench.project.preview = None;
                    self.workbench.project.obsolete_preview = None;
                    self.workbench.project.transfer_files.clear();
                    self.workbench.project.external = None;
                    self.workbench.project.unsaved = false;
                    self.workbench.project.close_confirm = false;
                    for name in ["query-name", "query-path", "query-alias", "query-text"] {
                        self.project_set(name, "");
                    }
                }
                _ => self.workbench.project.result = Some(value.clone()),
            },
        }
        true
    }

    pub(super) fn project_navigate_path(&mut self, backend: &Backend, host: &str, path: &str) {
        self.project_action("project-new", backend);
        self.workbench.project.section = "local".to_owned();
        self.project_send(
            backend,
            "local_project.resolve",
            json!({"host":host,"path":path}),
        );
    }

    pub(super) fn project_associate_resource(
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

    pub(super) fn project_glimpse_result(&mut self, id: &str, success: bool) {
        if self
            .workbench
            .project
            .open_file
            .as_ref()
            .and_then(|v| v["id"].as_str())
            == Some(id)
        {
            self.workbench.project.open_file = None;
            if !success {
                self.status =
                    i18n::UiMessage::raw("文件预览失败 / Could not preview file".to_owned());
            }
        }
    }

    fn project_document_scan(&mut self, root: &str) {
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

    fn project_document_preview(&mut self, relative: &str, how: Option<&str>) {
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

    fn local_environment_select(&mut self, index: usize) {
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

    fn local_project_action(&mut self, name: &str, backend: &Backend) -> bool {
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
                    self.status = i18n::UiMessage::raw(
                        "请先保存项目文件 / Save the project file first".to_owned(),
                    );
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
                            self.status = i18n::UiMessage::raw(
                                "Redis DB 必须是非负整数 / Redis DB must be a non-negative integer"
                                    .to_owned(),
                            );
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
                            self.status = i18n::UiMessage::raw(
                                "请选择已发现的数据库 / Select a discovered database".to_owned(),
                            );
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

    pub(super) fn project_action(&mut self, name: &str, backend: &Backend) -> bool {
        if self.local_project_action(name, backend) {
            return true;
        }
        if let Some(index) = name
            .strip_prefix("project-ui-alias:")
            .and_then(|s| s.parse::<usize>().ok())
        {
            if let Some(connection) = self
                .workbench
                .project
                .project
                .as_ref()
                .and_then(|p| p["manifest"]["connections"].get(index))
                .cloned()
            {
                let alias = connection["alias"].as_str().unwrap_or("").to_owned();
                let binding = self
                    .workbench
                    .project
                    .bindings
                    .as_ref()
                    .and_then(|v| v["bindings"].as_array())
                    .and_then(|items| {
                        items
                            .iter()
                            .find(|v| v["connection_alias"].as_str() == Some(alias.as_str()))
                    })
                    .cloned()
                    .unwrap_or(Value::Null);
                self.project_set("alias", alias);
                self.workbench.project.git_binding_edit = true;
                self.project_set("profile", binding["profile_id"].as_str().unwrap_or(""));
                self.project_set(
                    "database",
                    binding["overrides"]["database"].as_str().unwrap_or(""),
                );
                self.project_set(
                    "redis-db",
                    binding["overrides"]["redis_db"]
                        .as_u64()
                        .map(|v| v.to_string())
                        .unwrap_or_default(),
                );
            }
            return true;
        }
        if name == "project-new" {
            self.workbench.project.document_root = None;
            self.workbench.project.document_files.clear();
            self.workbench.project.local_project = None;
            self.workbench.project.local_environment = None;
            self.workbench.project.local_form.clear();
            self.workbench.project.local_resource_edit = None;
            self.workbench.project.section = "local".into();
            self.workbench.project.local_delete_confirm = false;
            self.workbench.project.binding_history.clear();
            self.workbench.project.binding_offset = 0;
            self.workbench.project.artifact_history.clear();
            self.workbench.project.artifact_offset = 0;
            self.workbench.project.artifacts.clear();
            self.workbench.project.artifact_next = None;
            self.create_query_tab();
            self.tabs[self.active].title = i18n::tr("项目").into_owned();
            self.view = "project-workbench".into();
            for (key, value) in [
                ("path", ""),
                ("name", ""),
                ("archive", ""),
                ("destination", ""),
                ("group", "default"),
                ("alias", ""),
                ("profile", ""),
                ("database", ""),
                ("redis-db", ""),
                ("logical-alias", ""),
                ("logical-provider", "mysql"),
                ("logical-source", "direct"),
                ("path-parameter", ""),
                ("local-path", ""),
                ("query-name", ""),
                ("external-path", ""),
                ("query-path", ""),
                ("query-alias", ""),
                ("query-text", ""),
                ("artifact-path", ""),
                ("artifact-index", "1"),
                ("artifact-side", "old"),
                ("local-name", ""),
                ("local-description", ""),
                ("local-root", ""),
                ("local-environment-name", ""),
                ("local-environment-type", "dev"),
                ("local-environment-kind", "dev"),
                ("local-environment-description", ""),
                ("local-profile", ""),
                ("local-database", ""),
                ("local-redis-db", ""),
                ("local-resource-description", ""),
            ] {
                self.project_set(key, value);
            }
            self.project_send(backend, "local_project.list", json!({}));
            return true;
        }
        if let Some(rest) = name.strip_prefix("field:project-") {
            if let Some((key, value)) = rest.split_once(':') {
                let key = format!("project-{key}");
                let profile_changed =
                    key == self.project_key("local-profile") && self.field(&key) != value;
                if profile_changed {
                    self.project_set("local-database", "");
                    self.project_set("local-redis-db", "");
                }
                self.field_set(&key, value);
                if profile_changed {
                    self.project_discover_databases(backend);
                }
                self.changed_field(&key);
                self.workbench.generic_select = None;
            }
            return true;
        }
        if !name.starts_with("project-ui-") {
            return false;
        }
        if self.workbench.project.pending.is_some() {
            self.status = i18n::UiMessage::key("请求处理中，请稍候");
            return true;
        }
        if self.saving && matches!(name, "project-ui-save-prepare" | "project-ui-save-commit") {
            self.status = i18n::UiMessage::key("工作区保存仍在进行");
            return true;
        }
        if self.workbench.project.file_loading && name != "project-ui-file-next" {
            self.status = i18n::UiMessage::raw("请先完整读取项目文件".to_owned());
            return true;
        }
        if name == "project-ui-connection-new" {
            self.workbench.project.git_connection_new = true;
            return true;
        }
        if name == "project-ui-query-new" {
            if self.workbench.project.unsaved {
                self.status = i18n::UiMessage::key("请先保存项目，再切换查询");
                return true;
            }
            for field in ["query-name", "query-path", "query-alias", "query-text"] {
                self.project_set(field, "");
            }
            self.workbench.project.git_query_edit = true;
            return true;
        }
        if name.starts_with("project-ui-query:") && self.workbench.project.unsaved {
            self.status = i18n::UiMessage::key("请先保存项目，再切换查询");
            return true;
        }
        if name.starts_with("project-ui-query:") {
            self.workbench.project.git_query_edit = true;
        }
        if matches!(
            name,
            "project-ui-open" | "project-ui-create" | "project-ui-import"
        ) && self.workbench.project.project.is_some()
        {
            self.status = i18n::UiMessage::key("请先保存并关闭当前项目");
            return true;
        }
        self.dirty = true;
        if let Some(section) = name.strip_prefix("project-ui-section:") {
            if section != "local" && !self.workbench.project.local_form.is_empty() {
                self.status = i18n::UiMessage::key("请先保存或取消当前表单");
                return true;
            }
            if matches!(
                section,
                "overview" | "bindings" | "local" | "queries" | "transfers" | "save" | "recovery"
            ) {
                if section == "local" {
                    for (field, default) in [
                        ("local-name", ""),
                        ("local-description", ""),
                        ("local-root", ""),
                        ("local-environment-name", ""),
                        ("local-environment-type", ""),
                        ("local-environment-kind", "dev"),
                        ("local-environment-description", ""),
                        ("local-profile", ""),
                        ("local-database", ""),
                        ("local-redis-db", ""),
                        ("local-resource-description", ""),
                    ] {
                        let key = self.project_key(field);
                        if !self.fields.contains_key(&key) {
                            self.field_set(&key, default);
                        }
                    }
                    if !self.workbench.project.local_loaded {
                        self.project_send(backend, "local_project.list", json!({}));
                    }
                }
                self.workbench.project.section = section.to_owned();
                self.focus.clear();
            }
            return true;
        }
        if let Some(rest) = name.strip_prefix("project-ui-delete-decision:") {
            if let Some((section, tail)) = rest.split_once(':')
                && let Some((index, decision)) = tail.split_once(':')
                && let Ok(index) = index.parse::<usize>()
                && matches!(decision, "delete" | "keep")
                && matches!(section, "connections" | "queries" | "transfers")
                && let Some(item) = self
                    .workbench
                    .project
                    .baseline_manifest
                    .as_ref()
                    .and_then(|m| m[section].get(index))
                    .cloned()
            {
                let key = if section == "connections" {
                    "alias"
                } else {
                    "id"
                };
                if decision == "keep"
                    && let Some(path) = item["path"].as_str()
                    && self
                        .workbench
                        .project
                        .project
                        .as_ref()
                        .is_some_and(|project| project["digests"].get(path).is_some())
                    && self
                        .workbench
                        .project
                        .external
                        .as_ref()
                        .is_some_and(|external| external["digests"].get(path).is_none())
                    && !(section == "queries" && self.project_value("query-path") == path)
                    && !self.workbench.project.transfer_files.contains_key(path)
                {
                    self.status = i18n::UiMessage::raw(
                        "缺少可保留的本地正文，请先从备份找回并填入草稿".to_owned(),
                    );
                    return true;
                }
                if decision == "keep"
                    && section == "queries"
                    && let Some(path) = item["path"].as_str()
                    && self.project_value("query-path") == path
                {
                    let text = self.project_value("query-text").to_owned();
                    self.workbench
                        .project
                        .transfer_files
                        .insert(path.to_owned(), text);
                    self.workbench.project.unsaved = true;
                }
                if decision == "delete" {
                    if let Some(project) = &mut self.workbench.project.project
                        && let Some(items) = project["manifest"][section].as_array_mut()
                    {
                        items.retain(|current| current[key] != item[key]);
                    }
                    if section == "queries"
                        && let Some(project) = &mut self.workbench.project.project
                    {
                        let id = &item["id"];
                        let layout = &mut project["manifest"]["layout"];
                        if let Some(tabs) = layout["tabs"].as_array_mut() {
                            tabs.retain(|value| value != id);
                        }
                        if let Some(splits) = layout["splits"].as_array_mut() {
                            for split in splits.iter_mut() {
                                if let Some(tabs) = split.as_array_mut() {
                                    tabs.retain(|value| value != id);
                                }
                            }
                            splits.retain(|split| {
                                split.as_array().is_some_and(|tabs| !tabs.is_empty())
                            });
                        }
                        if let Some(id) = id.as_str()
                            && let Some(columns) = layout["columns"].as_object_mut()
                        {
                            columns.remove(id);
                        }
                    }
                    if matches!(section, "queries" | "transfers")
                        && let Some(path) = item["path"].as_str()
                    {
                        self.workbench.project.transfer_files.remove(path);
                    }
                    if section == "queries"
                        && item["path"].as_str() == Some(self.project_value("query-path"))
                    {
                        self.project_set("query-path", "");
                    }
                    self.workbench.project.unsaved = true;
                    self.workbench.project.invalidate();
                }
                self.workbench
                    .project
                    .deletion_decisions
                    .insert(format!("{section}:{index}"));
            }
            return true;
        }
        let project = self.workbench.project.project.clone();
        let id = project.as_ref().and_then(|p| p.get("project_id")).cloned();
        if let Some(rest) = name.strip_prefix("project-ui-change-decision:") {
            if let Some((section, tail)) = rest.split_once(':')
                && let Some((index, decision)) = tail.split_once(':')
                && let Ok(index) = index.parse::<usize>()
                && matches!(section, "connections" | "queries" | "transfers")
                && matches!(decision, "merge" | "keep")
                && let (Some(external), Some(project), Some(baseline)) = (
                    self.workbench.project.external.clone(),
                    project.clone(),
                    self.workbench.project.baseline_manifest.clone(),
                )
                && external_changes(&baseline, &external, &project["digests"])
                    .iter()
                    .any(|(s, i, _)| s == section && *i == index)
            {
                let item = external["manifest"][section][index].clone();
                let key = if section == "connections" {
                    "alias"
                } else {
                    "id"
                };
                let old = baseline[section]
                    .as_array()
                    .and_then(|items| items.iter().find(|old| old[key] == item[key]));
                let path = item["path"].as_str().map(str::to_owned);
                let body_changed = path
                    .as_deref()
                    .is_some_and(|p| project["digests"].get(p) != external["digests"].get(p));
                let active_query = section == "queries"
                    && old
                        .and_then(|v| v["path"].as_str())
                        .is_some_and(|p| self.project_value("query-path") == p);
                if decision == "merge" && body_changed {
                    let Some(path) = path.as_deref() else {
                        return true;
                    };
                    if self.workbench.project.external_complete_path.as_deref() != Some(path) {
                        self.status =
                            i18n::UiMessage::raw("请先完整读取该外部文件，再合并正文".to_owned());
                        return true;
                    }
                    let text = self.workbench.project.external_text.clone();
                    self.workbench
                        .project
                        .transfer_files
                        .insert(path.to_owned(), text.clone());
                    if section == "queries"
                        && (active_query || self.project_value("query-path") == path)
                    {
                        self.project_set("query-text", text);
                    }
                }
                if decision == "keep"
                    && let Some(old_path) = old.and_then(|v| v["path"].as_str())
                    && project["digests"].get(old_path) != external["digests"].get(old_path)
                {
                    if section == "queries" && self.project_value("query-path") == old_path {
                        let text = self.project_value("query-text").to_owned();
                        self.workbench
                            .project
                            .transfer_files
                            .insert(old_path.to_owned(), text);
                    } else if !self.workbench.project.transfer_files.contains_key(old_path) {
                        self.status = i18n::UiMessage::raw(
                            "缺少可保留的本地正文，请先从备份找回并填入草稿".to_owned(),
                        );
                        return true;
                    }
                }
                if decision == "merge" {
                    if let Some(old_path) = old.and_then(|v| v["path"].as_str())
                        && Some(old_path) != path.as_deref()
                    {
                        self.workbench.project.transfer_files.remove(old_path);
                    }
                    if let Some(items) = self
                        .workbench
                        .project
                        .project
                        .as_mut()
                        .and_then(|p| p["manifest"][section].as_array_mut())
                    {
                        items.retain(|old| old[key] != item[key]);
                        items.push(item.clone());
                    }
                    if section == "queries"
                        && (active_query
                            || path.as_deref() == Some(self.project_value("query-path")))
                    {
                        if let Some(path) = path.as_deref() {
                            self.project_set("query-path", path);
                        }
                        self.project_set("query-name", item["name"].as_str().unwrap_or(""));
                        self.project_set(
                            "query-alias",
                            item["connection_alias"].as_str().unwrap_or(""),
                        );
                    }
                }
                self.workbench.project.unsaved = true;
                self.workbench.project.invalidate();
                self.workbench
                    .project
                    .change_decisions
                    .insert(format!("{section}:{index}"));
            }
            return true;
        }
        if (name == "project-ui-transfer-new" || name.starts_with("project-ui-transfer:"))
            && self.workbench.transfer.busy()
        {
            self.status = i18n::UiMessage::raw("请先完成或取消当前迁移任务".to_owned());
            return true;
        }
        if name == "project-ui-transfer-new" {
            self.transfer_action("transfer-project-new:copy", backend);
            return true;
        }
        if let Some(index) = name
            .strip_prefix("project-ui-transfer:")
            .and_then(|s| s.parse::<usize>().ok())
        {
            if let (Some(id), Some(transfer)) = (
                id.clone(),
                project
                    .as_ref()
                    .and_then(|p| p["manifest"]["transfers"].get(index))
                    .cloned(),
            ) {
                let title = transfer["name"].as_str().unwrap_or("").to_owned();
                let path = transfer["path"].as_str().unwrap_or("").to_owned();
                self.transfer_action("transfer-project-new:copy", backend);
                if let Some(text) = self.workbench.project.transfer_files.get(&path).cloned() {
                    match serde_json::from_str::<Value>(&text) {
                        Ok(spec) => {
                            self.transfer_load_preset(json!({"name":title,"spec":spec}));
                            self.field_set(&self.transfer_key("project-path"), &path);
                        }
                        Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
                    }
                } else {
                    self.view = "project-workbench".into();
                    self.workbench.project.transfer_loading = Some((title, path.clone()));
                    self.workbench.project.file_buffer.clear();
                    self.workbench.project.file_loading = true;
                    self.workbench.project.file_next = None;
                    self.project_send(
                        backend,
                        "project.file.read",
                        json!({"project_id":id,"path":path,"offset":0,"limit":65536}),
                    );
                }
            }
            return true;
        }
        match name {
            "project-ui-open" => {
                let mut params = json!({"path":self.project_value("path")});
                if let Some(saved) = &self.workbench.project.restored {
                    params["baseline_manifest"] = saved.get("baseline_manifest").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| saved["manifest"].clone());
                    params["baseline_digests"] = saved["digests"].clone();
                }
                self.project_send(backend,"project.open",params);
            },
            "project-ui-create" => self.project_send(backend,"project.create",json!({"path":self.project_value("path"),"name":self.project_value("name")})),
            "project-ui-import" => self.project_send(backend,"project.import",json!({"path":self.project_value("archive"),"destination":self.project_value("destination")})),
            "project-ui-export" => if let Some(id) = id { self.project_send(backend,"project.export",json!({"project_id":id,"path":self.project_value("archive")})); },
            "project-ui-bindings" => if let Some(id) = id { self.workbench.project.binding_group = self.project_value("group").to_owned(); self.workbench.project.bindings = None; self.workbench.project.binding_offset = 0; self.workbench.project.binding_history.clear(); self.workbench.project.binding_page_request = Some((0,0)); self.project_send(backend,"project.bindings.get",json!({"project_id":id,"binding_group":self.project_value("group"),"offset":0,"limit":100})); },
            "project-ui-bindings-next" => if let (Some(id),Some(offset)) = (id,self.workbench.project.bindings.as_ref().and_then(|v|v["next"].as_u64())) { if self.workbench.project.binding_group != self.project_value("group") { self.status = i18n::UiMessage::key("请先读取当前绑定组"); return true; } self.workbench.project.binding_page_request = Some((offset,1)); self.project_send(backend,"project.bindings.get",json!({"project_id":id,"binding_group":self.project_value("group"),"offset":offset,"limit":100})); },
            "project-ui-bindings-previous" => if let (Some(id),Some(offset)) = (id,self.workbench.project.binding_history.last().copied()) { if self.workbench.project.binding_group != self.project_value("group") { self.status = i18n::UiMessage::key("请先读取当前绑定组"); return true; } self.workbench.project.binding_page_request = Some((offset,2)); self.project_send(backend,"project.bindings.get",json!({"project_id":id,"binding_group":self.project_value("group"),"offset":offset,"limit":100})); },
            "project-ui-connection-add" => {
                let alias = self.project_value("logical-alias").trim().to_owned();
                if alias.is_empty() { self.field_error(&self.project_key("logical-alias"), i18n::UiMessage::key("请输入逻辑连接别名")); return true; }
                let connection = json!({"alias":alias,"provider":self.project_value("logical-provider"),"source_kind":self.project_value("logical-source"),"description":""});
                if let Some(project) = &mut self.workbench.project.project
                    && let Some(connections) = project["manifest"]["connections"].as_array_mut() {
                        if connections.iter().any(|c|c["alias"] == connection["alias"]) { self.status = i18n::UiMessage::key("逻辑连接别名已存在"); return true; }
                        connections.push(connection); self.workbench.project.unsaved = true; self.workbench.project.invalidate(); self.workbench.project.git_connection_new = false;
                    }
            },
            "project-ui-bindings-save" => if let Some(id) = id {
                if self.workbench.project.binding_group != self.project_value("group") || self.workbench.project.bindings.is_none() { self.status = i18n::UiMessage::key("请先读取当前绑定组"); return true; }
                let existing = self.workbench.project.bindings.as_ref().and_then(|v|v["bindings"].as_array()).and_then(|items|items.iter().find(|v|v["connection_alias"].as_str()==Some(self.project_value("alias"))));
                let revision = existing.and_then(|v|v["revision"].as_u64()).unwrap_or(0);
                let mut paths = existing.and_then(|v|v["overrides"].get("paths")).cloned().unwrap_or_else(||json!({}));
                let path_parameter = self.project_value("path-parameter").trim();
                if !path_parameter.is_empty() {
                    if !path_parameter.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || self.project_value("local-path").is_empty() {
                        self.field_error(&self.project_key("path-parameter"), i18n::UiMessage::raw("请输入有效路径参数名和本机路径".to_owned())); return true;
                    }
                    paths[path_parameter] = json!(self.project_value("local-path"));
                }
                let redis = self.project_value("redis-db");
                let redis_db = if redis.is_empty() { None } else { match redis.parse::<u32>() { Ok(db) => Some(db), Err(_) => { self.field_error(&self.project_key("redis-db"), i18n::UiMessage::key("Redis DB 必须为非负整数")); return true; } } };
                let binding = json!({"connection_alias":self.project_value("alias"),"profile_id":self.project_value("profile"),"database":if self.project_value("database").is_empty(){None}else{Some(self.project_value("database"))},"redis_db":redis_db,"paths":paths,"revision":revision});
                self.project_send(backend,"project.bindings.save",json!({"project_id":id,"binding_group":self.project_value("group"),"bindings":[binding]}));
            },
            "project-ui-bindings-confirm" => if let (Some(id), Some(bindings)) = (id, self.workbench.project.bindings.clone()) {
                if self.workbench.project.binding_group != self.project_value("group") { self.status = i18n::UiMessage::key("请先读取当前绑定组"); return true; }
                self.project_send(backend,"project.bindings.confirm",json!({"project_id":id,"binding_group":self.workbench.project.binding_group,"binding_digest":bindings["binding_digest"]}));
            },
            "project-ui-save-prepare" => if let (Some(id),Some(project)) = (id,project) {
                if self.workbench.project.needs_baseline_check || self.workbench.project.external.is_some() { self.status = i18n::UiMessage::raw("请先处理外部版本冲突".to_owned()); return true; }
                if self.workbench.project.file_loading { self.status = i18n::UiMessage::key("请先完整读取项目文件，再保存"); return true; }
                let mut manifest = project["manifest"].clone(); let mut files = self.workbench.project.transfer_files.clone();
                let path = self.project_value("query-path");
                if !path.is_empty() {
                    let provider = manifest["connections"].as_array().and_then(|items| items.iter().find(|c| c["alias"].as_str() == Some(self.project_value("query-alias")))).and_then(|c| c["provider"].as_str());
                    let language = match provider { Some("mysql") => "sql", Some("redis") => "redis", _ => { self.workbench.project.section = "queries".into(); self.field_error(&self.project_key("query-alias"), i18n::UiMessage::raw("请选择有效的查询连接别名".to_owned())); return true; } };
                    let mut query = manifest["queries"].as_array().and_then(|q|q.iter().find(|q|q["path"].as_str()==Some(path))).cloned().unwrap_or_else(||json!({"id":Uuid::new_v4()}));
                    query["language"] = json!(language);
                    query["name"] = json!(self.project_value("query-name")); query["path"] = json!(path); query["connection_alias"] = json!(self.project_value("query-alias"));
                    if let Some(queries) = manifest["queries"].as_array_mut() { queries.retain(|q|q["path"].as_str()!=Some(path)); queries.push(query); }
                    files.insert(path.to_owned(),self.project_value("query-text").to_owned());
                }
                let replace = self.workbench.project.preview.as_ref().or(self.workbench.project.obsolete_preview.as_ref()).and_then(|p|p.get("plan_id")).cloned().unwrap_or(Value::Null);
                self.workbench.project.upload = Some(ProjectUpload { id:None, revision:0, project_id:id.clone(), manifest, replace, files:files.into_iter().collect(), index:0, offset:0 });
                self.project_send(backend,"project.save.upload.begin",json!({"project_id":id}));
            },
            "project-ui-save-commit" => if let Some(preview) = self.workbench.project.preview.clone() { self.project_send(backend,"project.save.commit",json!({"plan_id":preview["plan_id"],"revision":preview["revision"],"digest":preview["digest"]})); },
            "project-ui-save-discard" => if let Some(preview) = self.workbench.project.preview.clone().or_else(||self.workbench.project.obsolete_preview.clone()) { self.project_send(backend,"project.save.discard",json!({"plan_id":preview["plan_id"]})); },
            "project-ui-external-read" => if let Some(id) = id { self.project_send(backend,"project.external.read",json!({"project_id":id})); },
            "project-ui-external-file" => if let Some(id) = id { let path = self.project_value("external-path").to_owned(); self.workbench.project.external_text.clear(); self.workbench.project.external_next = None; self.workbench.project.external_read_path = Some(path.clone()); self.workbench.project.external_complete_path = None; self.project_send(backend,"project.external.read",json!({"project_id":id,"path":path,"offset":0,"limit":65536})); },
            "project-ui-external-next" => if let (Some(id),Some(offset),Some(path)) = (id,self.workbench.project.external_next,self.workbench.project.external_read_path.clone()) { self.project_send(backend,"project.external.read",json!({"project_id":id,"path":path,"offset":offset,"limit":65536})); },
            "project-ui-external-accept" => if let (Some(id),Some(external)) = (id,self.workbench.project.external.clone()) {
                if let Some(baseline) = &self.workbench.project.baseline_manifest
                    && external_deletions(baseline, &external, &project.as_ref().map(|p| p["digests"].clone()).unwrap_or(Value::Null)).iter().any(|(section,index,_)| !self.workbench.project.deletion_decisions.contains(&format!("{section}:{index}")))
                { self.status = i18n::UiMessage::raw("请逐项决定外部删除是保留还是合并删除".to_owned()); return true; }
                if let Some(baseline) = &self.workbench.project.baseline_manifest
                    && external_changes(baseline, &external, &project.as_ref().map(|p| p["digests"].clone()).unwrap_or(Value::Null)).iter().any(|(section,index,_)| !self.workbench.project.change_decisions.contains(&format!("{section}:{index}")))
                { self.status = i18n::UiMessage::raw("请逐项决定外部新增或修改是保留本地还是合并外部版本".to_owned()); return true; }
                self.project_send(backend,"project.external.accept",json!({"project_id":id,"digests":external["digests"]}));
            },
            "project-ui-close" => { self.workbench.project.close_confirm = true; },
            "project-ui-close-confirm" => if let Some(id) = id { self.project_send(backend,"project.close",json!({"project_id":id,"discard_previews":true})); },
            "project-ui-close-cancel" => self.workbench.project.close_confirm = false,
            "project-ui-file-reload" => { self.workbench.project.reload_confirm = true; },
            "project-ui-file-reload-cancel" => { self.workbench.project.reload_confirm = false; },
            "project-ui-file-reload-confirm" => if let Some(id) = id { self.workbench.project.reload_confirm = false; self.workbench.project.file_buffer.clear(); self.workbench.project.file_loading = true; self.workbench.project.file_next = None; self.project_send(backend,"project.file.read",json!({"project_id":id,"path":self.project_value("query-path"),"offset":0,"limit":65536})); },
            "project-ui-file-next" => if let (Some(id),Some(offset)) = (id,self.workbench.project.file_next) { let path = self.workbench.project.transfer_loading.as_ref().map(|(_,path)| path.clone()).or_else(|| self.workbench.project.query_loading.as_ref().and_then(|q|q["path"].as_str()).map(str::to_owned)).unwrap_or_else(|| self.project_value("query-path").to_owned()); self.project_send(backend,"project.file.read",json!({"project_id":id,"path":path,"offset":offset,"limit":65536})); },
            "project-ui-artifact-list" => { self.workbench.project.artifact_page_request = Some((0,0)); self.project_send(backend,"project.stages.discover",json!({})); },
            "project-ui-artifact-next" => if let Some(offset)=self.workbench.project.artifact_next { self.workbench.project.artifact_page_request=Some((offset,1)); self.project_send(backend,"project.stages.discover",json!({})); },
            "project-ui-artifact-previous" => if let Some(offset)=self.workbench.project.artifact_history.last().copied() { self.workbench.project.artifact_page_request=Some((offset,2)); self.project_send(backend,"project.stages.discover",json!({})); },
            "project-ui-artifact-read" => if let Some(artifact)=self.workbench.project.artifact.clone() { self.project_send(backend,"transfer.artifact.get",json!({"id":artifact["id"],"revision":artifact["revision"]})); },
            "project-ui-artifact-claim" => if let Some(artifact)=self.workbench.project.artifact.clone() {
                if artifact["state"] == "preparing"
                    && matches!(artifact["kind"].as_str(), Some("export_partial" | "report" | "input_index"))
                    && artifact["observed_identity"].is_null() {
                    self.project_send(backend,"transfer.artifact.get",json!({"id":artifact["id"],"revision":artifact["revision"]}));
                } else {
                    self.project_send(backend,"transfer.artifact.claim",json!({"id":artifact["id"],"revision":artifact["revision"],"confirm_partial_sha256":artifact["observed_identity"]["sha256"]}));
                }
            },
            "project-ui-artifact-export" => if let Some(a) = self.workbench.project.artifact.clone() {
                let mut params = json!({"id":a["id"],"revision":a["revision"],"path":self.project_value("artifact-path")});
                if a["kind"] == "project_stage" && a["identity"]["kind"] == "save" {
                    let index = match self.project_value("artifact-index").parse::<usize>().ok().and_then(|n|n.checked_sub(1)) { Some(index) => index, None => { self.status = i18n::UiMessage::key("恢复文件序号必须从 1 开始"); return true; } };
                    params["index"] = json!(index); params["side"] = json!(self.project_value("artifact-side"));
                }
                self.project_send(backend,"transfer.artifact.export",params);
            },
            "project-ui-artifact-cleanup" => { self.workbench.project.cleanup_confirm = true; },
            "project-ui-artifact-cleanup-cancel" => { self.workbench.project.cleanup_confirm = false; },
            "project-ui-artifact-cleanup-confirm" => if let Some(a) = self.workbench.project.artifact.clone() {
                self.workbench.project.cleanup_confirm = false;
                let mut params = json!({"id":a["id"],"revision":a["revision"]});
                if let Some(recovery) = &self.workbench.project.recovery {
                    if !recovery["observed_directory_identity"].is_null() { params["confirm_stage_dir_identity"] = recovery["observed_directory_identity"].clone(); }
                    if !recovery["journal_sha256"].is_null() { params["confirm_journal_sha256"] = recovery["journal_sha256"].clone(); }
                }
                self.project_send(backend,"transfer.artifact.cleanup",params);
            },
            "project-ui-recovery-read" => if let Some(a) = self.workbench.project.artifact.clone() { self.project_send(backend,"project.recovery.read",json!({"id":a["id"],"revision":a["revision"]})); },
            "project-ui-recovery-continue" | "project-ui-recovery-rollback" => if let Some(a) = self.workbench.project.recovery.clone() { let action = if name.ends_with("continue") {"continue"} else {"rollback"}; self.workbench.project.recovery_action = action.into(); self.workbench.project.recovery_preview = None; self.project_send(backend,"project.recovery.prepare",json!({"id":a["id"],"revision":a["revision"],"action":action})); },
            "project-ui-recovery-commit" => if let Some(p) = self.workbench.project.recovery_preview.clone() { self.project_send(backend,"project.recovery.commit",json!({"plan_id":p["plan_id"],"revision":p["revision"],"digest":p["digest"]})); },
            _ => if let Some(index) = name.strip_prefix("project-ui-artifact-select:").and_then(|s|s.parse::<usize>().ok()) { self.workbench.project.artifact = self.workbench.project.artifacts.get(index).cloned(); self.workbench.project.recovery = None; self.workbench.project.recovery_preview = None; } else if let Some(index) = name.strip_prefix("project-ui-query:").and_then(|s|s.parse::<usize>().ok()) && let Some(query) = project.as_ref().and_then(|p|p["manifest"]["queries"].get(index)).cloned() { self.workbench.project.invalidate(); self.workbench.project.query_loading = Some(query.clone()); self.workbench.project.file_buffer.clear(); self.workbench.project.file_loading = true; self.workbench.project.file_next = None; if let Some(id) = id { self.project_send(backend,"project.file.read",json!({"project_id":id,"path":query["path"],"offset":0,"limit":65536})); } }
        }
        true
    }
}

#[cfg(test)]
#[path = "../tests/unit/project_lifecycle.rs"]
mod lifecycle_tests;
