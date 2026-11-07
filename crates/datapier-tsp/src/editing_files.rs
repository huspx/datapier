//! 全局编辑文件登记管理；登记不授予数据库访问或恢复基线权限。
use super::*;

#[derive(Default)]
pub(super) struct FileManagement {
    pub items: Vec<Value>,
    pub offset: u64,
    pub next: Option<u64>,
    pub pending: Option<(Uuid, String)>,
    pub selected: Option<String>,
    pub confirm_cleanup: bool,
    pub refresh_pending: bool,
}

impl App {
    fn editing_file_request(&mut self, backend: &Backend, method: &str, params: Value) {
        if self.editing_files.pending.is_some() {
            return;
        }
        let token = Uuid::new_v4();
        self.editing_files.pending = Some((token, method.to_owned()));
        if let Err(error) = backend.request(format!("editing-files/{token}"), method, params) {
            self.editing_files.pending = None;
            self.status = i18n::UiMessage::raw(error.to_string());
        }
    }

    pub(super) fn editing_files_list(&mut self, backend: &Backend, offset: u64) {
        self.editing_file_request(
            backend,
            "value.files.list",
            json!({"offset":offset,"limit":100}),
        );
    }

    pub(super) fn editing_files_changed(&mut self, backend: &Backend) {
        if self.editing_files.pending.is_some() {
            self.editing_files.refresh_pending = true;
        } else {
            self.editing_files_list(backend, self.editing_files.offset);
        }
    }

    pub(super) fn editing_files_action(&mut self, name: &str, backend: &Backend) -> bool {
        if name == "editing-files-refresh" {
            self.editing_files_list(backend, 0);
            return true;
        }
        if name == "editing-files-next" {
            if let Some(next) = self.editing_files.next {
                self.editing_files_list(backend, next);
            }
            return true;
        }
        if name == "editing-files-prev" {
            self.editing_files_list(backend, self.editing_files.offset.saturating_sub(100));
            return true;
        }
        if let Some(id) = name.strip_prefix("editing-files-select:") {
            if self
                .editing_files
                .items
                .iter()
                .any(|item| item["id"].as_str() == Some(id))
            {
                self.editing_files.selected = Some(id.to_owned());
                self.editing_files.confirm_cleanup = false;
            }
            return true;
        }
        if !matches!(
            name,
            "editing-files-claim"
                | "editing-files-export"
                | "editing-files-cleanup"
                | "editing-files-cleanup-confirm"
                | "editing-files-cleanup-cancel"
        ) {
            return false;
        }
        if name == "editing-files-cleanup-cancel" {
            self.editing_files.confirm_cleanup = false;
            return true;
        }
        if name == "editing-files-cleanup" {
            self.editing_files.confirm_cleanup = true;
            return true;
        }
        let Some(id) = self.editing_files.selected.clone() else {
            return true;
        };
        let Some(revision) = self
            .editing_files
            .items
            .iter()
            .find(|item| item["id"].as_str() == Some(id.as_str()))
            .and_then(|item| item["revision"].as_u64())
        else {
            return true;
        };
        let (method, params) = match name {
            "editing-files-claim" => (
                "value.files.claim",
                json!({"file_id":id,"revision":revision}),
            ),
            "editing-files-export" => (
                "value.files.export",
                json!({"file_id":id,"revision":revision,"path":self.field("editing_file_path"),"overwrite":false}),
            ),
            _ => (
                "value.files.cleanup",
                json!({"file_id":id,"revision":revision}),
            ),
        };
        self.editing_files.confirm_cleanup = false;
        self.editing_file_request(backend, method, params);
        true
    }

    pub(super) fn editing_files_response(&mut self, response: Response, backend: &Backend) -> bool {
        let Some(token) = response
            .tag
            .strip_prefix("editing-files/")
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            return false;
        };
        let Some((expected, method)) = self.editing_files.pending.take() else {
            return true;
        };
        if expected != token {
            self.editing_files.pending = Some((expected, method));
            return true;
        }
        match response.result {
            Ok(value) if method == "value.files.list" => {
                self.editing_files.items = value["items"].as_array().cloned().unwrap_or_default();
                self.editing_files.offset = value["offset"].as_u64().unwrap_or(0);
                self.editing_files.next = value["next"].as_u64();
                self.editing_files.selected = None;
            }
            Ok(_) => {
                self.status = i18n::UiMessage::key("编辑文件操作完成");
                self.editing_files_list(backend, self.editing_files.offset);
            }
            Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
        }
        if std::mem::take(&mut self.editing_files.refresh_pending) {
            self.editing_files_list(backend, self.editing_files.offset);
        }
        true
    }
}
