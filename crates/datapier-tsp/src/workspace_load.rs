//! 有界读取工作区快照；只有完整且仍属于当前工作区的响应才能恢复。
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::editing::JsonMemoryEstimate;

const CHUNK_BYTES: usize = 256 * 1024;

pub(super) struct WorkspaceLoad {
    token: Uuid,
    workspace: String,
    initial_revision: u64,
    handle: Option<Uuid>,
    revision: u64,
    total: usize,
    bytes: Vec<u8>,
    estimate: JsonMemoryEstimate,
}

impl App {
    pub(super) fn start_workspace_load(&mut self, backend: &Backend) {
        if let Some(old) = self.workspace_load.take()
            && let Some(handle) = old.handle
        {
            Self::close_workspace_load(backend, handle);
        }
        let token = Uuid::new_v4();
        let load = WorkspaceLoad {
            token,
            workspace: self.workspace_id.clone(),
            initial_revision: self.revision,
            handle: None,
            revision: 0,
            total: 0,
            bytes: Vec::new(),
            estimate: JsonMemoryEstimate::default(),
        };
        match backend.request(
            format!("workspace.load/{token}"),
            "workspace.load",
            json!({"workspace_id":&load.workspace}),
        ) {
            Ok(()) => self.workspace_load = Some(load),
            Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
        }
    }

    fn close_workspace_load(backend: &Backend, handle: Uuid) {
        let _ = backend.request(
            "workspace.load.cleanup",
            "workspace.save.close",
            json!({"upload_id":handle}),
        );
    }

    fn workspace_load_current(&self, load: &WorkspaceLoad) -> bool {
        self.workspace_id == load.workspace
            && self.revision == load.initial_revision
            && !self.workspace_loaded
            && !self.saving
            && self.workspace_upload.is_none()
    }

    pub(super) fn workspace_load_response(&mut self, response: Response, backend: &Backend) {
        if response.tag == "workspace.load.cleanup" {
            return;
        }
        let initial = response.tag.starts_with("workspace.load/");
        let token = if initial {
            response.tag.strip_prefix("workspace.load/")
        } else {
            response
                .tag
                .strip_prefix("workspace.load.read/")
                .and_then(|tag| tag.split('/').next())
        }
        .and_then(|s| Uuid::parse_str(s).ok());
        let matches = self
            .workspace_load
            .as_ref()
            .is_some_and(|load| Some(load.token) == token);
        if !matches {
            // 初始响应可能在取消后才创建句柄；读响应则在 tag 中保留句柄。
            let handle = if initial {
                response
                    .result
                    .as_ref()
                    .ok()
                    .and_then(|v| v["load_handle"].as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
            } else {
                response
                    .tag
                    .split('/')
                    .nth(2)
                    .and_then(|s| Uuid::parse_str(s).ok())
            };
            if let Some(handle) = handle {
                Self::close_workspace_load(backend, handle);
            }
            return;
        }
        let mut load = self.workspace_load.take().unwrap();
        if !self.workspace_load_current(&load) {
            let handle = load.handle.or_else(|| {
                response
                    .result
                    .as_ref()
                    .ok()
                    .and_then(|v| v["load_handle"].as_str())
                    .and_then(|s| Uuid::parse_str(s).ok())
            });
            if let Some(handle) = handle {
                Self::close_workspace_load(backend, handle);
            }
            return;
        }
        let result = (|| -> Result<Option<Value>> {
            let value = response.result?;
            if initial {
                if load.handle.is_some() {
                    return Err(AppError::invalid("工作区加载响应重复"));
                }
                if let Some(handle) = value.get("load_handle") {
                    load.handle = Some(serde_json::from_value(handle.clone())?);
                }
                load.revision = value["revision"]
                    .as_u64()
                    .ok_or_else(|| AppError::invalid("工作区版本无效"))?;
                if load.handle.is_some() {
                    let budget = self.settings.limits.result_memory_bytes_per_client;
                    // 输入缓冲及解析树至少需要四倍线长；节点额外开销在读取时计入。
                    load.total = value["bytes"]
                        .as_u64()
                        .and_then(|n| usize::try_from(n).ok())
                        .filter(|n| *n > 0 && n.saturating_mul(4) <= budget)
                        .ok_or_else(|| {
                            AppError::new("resource_limit", "工作区读取长度超过内存预算")
                        })?;
                    load.bytes
                        .try_reserve_exact(load.total)
                        .map_err(|_| AppError::new("resource_limit", "无法预留工作区读取缓冲"))?;
                } else {
                    return Ok(Some(
                        value
                            .get("state")
                            .cloned()
                            .ok_or_else(|| AppError::invalid("工作区状态缺失"))?,
                    ));
                }
            } else {
                let handle = load
                    .handle
                    .ok_or_else(|| AppError::invalid("工作区读取句柄缺失"))?;
                let expected_tag = format!(
                    "workspace.load.read/{}/{}/{}",
                    load.token,
                    handle,
                    load.bytes.len()
                );
                if response.tag != expected_tag {
                    return Err(AppError::invalid("工作区读取响应错位"));
                }
                let offset = value["offset"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok());
                let total = value["total_bytes"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok());
                let encoded = value["data_b64"]
                    .as_str()
                    .ok_or_else(|| AppError::invalid("工作区读取数据无效"))?;
                if offset != Some(load.bytes.len())
                    || total != Some(load.total)
                    || encoded.len() > CHUNK_BYTES.div_ceil(3) * 4
                {
                    return Err(AppError::invalid("工作区读取偏移或长度不匹配"));
                }
                let chunk = STANDARD
                    .decode(encoded)
                    .map_err(|_| AppError::invalid("工作区读取数据编码无效"))?;
                if chunk.is_empty()
                    || chunk.len() > CHUNK_BYTES
                    || chunk.len() > load.total - load.bytes.len()
                {
                    return Err(AppError::invalid("工作区读取块长度无效"));
                }
                let mut estimate = load.estimate;
                estimate.feed(&chunk);
                if load.total.saturating_add(estimate.heap_bound())
                    > self.settings.limits.result_memory_bytes_per_client
                {
                    return Err(AppError::new("resource_limit", "工作区解析超过内存预算"));
                }
                load.estimate = estimate;
                load.bytes.extend_from_slice(&chunk);
                let next = value
                    .get("next")
                    .ok_or_else(|| AppError::invalid("工作区读取下一偏移缺失"))?;
                if next.is_null() {
                    if load.bytes.len() != load.total {
                        return Err(AppError::invalid("工作区读取未完成"));
                    }
                    return Ok(Some(serde_json::from_slice(&load.bytes)?));
                }
                if next.as_u64().and_then(|n| usize::try_from(n).ok()) != Some(load.bytes.len())
                    || load.bytes.len() >= load.total
                {
                    return Err(AppError::invalid("工作区读取下一偏移无效"));
                }
            }
            let handle = load
                .handle
                .ok_or_else(|| AppError::invalid("工作区读取句柄缺失"))?;
            backend.request(
                format!(
                    "workspace.load.read/{}/{}/{}",
                    load.token,
                    handle,
                    load.bytes.len()
                ),
                "workspace.save.read",
                json!({"upload_id":handle,"offset":load.bytes.len(),"max_bytes":CHUNK_BYTES}),
            )?;
            Ok(None)
        })();
        match result {
            Ok(Some(state)) => {
                if let Some(handle) = load.handle {
                    Self::close_workspace_load(backend, handle);
                }
                if let Err(error) = self.restore_workspace_state(load.revision, state) {
                    self.status = i18n::UiMessage::raw(error.to_string());
                }
            }
            Ok(None) => self.workspace_load = Some(load),
            Err(error) => {
                if let Some(handle) = load.handle {
                    Self::close_workspace_load(backend, handle);
                }
                self.status = i18n::UiMessage::raw(error.to_string());
            }
        }
    }

    pub(super) fn restore_workspace_state(&mut self, revision: u64, value: Value) -> Result<()> {
        // 新工作区以空对象表示，不能把它当作格式错误或覆盖用户输入。
        let state = if value.as_object().is_some_and(|v| v.is_empty()) {
            None
        } else {
            Some(serde_json::from_value::<Workspace>(value)?)
        };
        self.revision = revision;
        if let Some(state) = state {
            if self.dirty {
                self.tabs.extend(state.tabs);
                self.migrate_cli_queries();
            } else {
                self.navigation.expanded = state.navigation_expanded;
                self.empty_context = state.empty_context;
                self.tabs = state.tabs;
                self.active = state.active.min(self.tabs.len().saturating_sub(1));
                self.migrate_cli_queries();
                if !self.tabs.is_empty() {
                    self.restore_browse();
                    self.profile = self.tabs[self.active]
                        .profile_id
                        .and_then(|id| self.profiles.iter().find(|p| p.id == id).cloned());
                    if !self.workspace_interacted {
                        self.focus_current_tab();
                        self.restore_focus_pending =
                            self.profile.is_none() && self.tabs[self.active].profile_id.is_some();
                    }
                    if !self.tabs[self.active].pending.is_null() {
                        self.detail = format!(
                            "恢复的编辑意图（基线已失效，请刷新并重新确认）：{}",
                            self.tabs[self.active].pending
                        );
                        self.set_recovery_detail_context();
                    }
                }
                self.restore_empty_context(false);
            }
        }
        self.workspace_loaded = true;
        Ok(())
    }
}
