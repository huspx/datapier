//! 表单关闭比较编辑意图；确认步骤保留原表单及焦点。
use super::*;

#[derive(Clone)]
pub(crate) struct ModalDraft {
    kind: String,
    fields: Vec<(&'static str, String)>,
    sql: Option<Value>,
    ttl: RedisTtlPolicy,
    focus: String,
}

fn draft_fields(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        "row" => &["row_values"],
        "filters" => &["filter", "sort"],
        "cell" => &["cell"],
        "redis" => &[
            "operation",
            "redis_value",
            "redis_field",
            "redis_index",
            "redis_member",
            "redis_score",
            "redis_key",
            "redis_ttl",
        ],
        "import" | "export" | "transfer" => &[
            "path",
            "format",
            "mapping",
            "null_marker",
            "backslash_escape",
            "binary_columns",
        ],
        "file" => &["path"],
        _ => return None,
    })
}

impl App {
    pub(crate) fn track_modal_draft(&mut self) {
        let kind = self.workbench.overlay.as_deref().unwrap_or("");
        if self
            .workbench
            .modal_draft
            .as_ref()
            .is_some_and(|draft| draft.kind == kind)
        {
            return;
        }
        self.workbench.modal_draft = (!kind.is_empty()).then(|| ModalDraft {
            kind: kind.to_owned(),
            fields: draft_fields(kind)
                .unwrap_or(&[])
                .iter()
                .map(|&name| (name, self.field(name).to_owned()))
                .collect(),
            sql: matches!(kind, "row" | "filters")
                .then(|| self.workbench.sql_forms.draft_snapshot(kind)),
            ttl: self.workbench.ttl_policy,
            focus: self.focus.clone(),
        });
        self.workbench.overlay_close_confirm = false;
        self.workbench.modal_after_close = None;
    }

    pub(crate) fn modal_has_unsaved(&self) -> bool {
        match self.workbench.overlay.as_deref() {
            Some("settings") => self.settings_form_dirty(),
            Some("profile") if !self.archery.open => self.connection_draft_dirty(),
            _ => self.workbench.modal_draft.as_ref().is_some_and(|draft| {
                draft
                    .fields
                    .iter()
                    .any(|(name, value)| self.field(name) != value)
                    || draft.sql.as_ref().is_some_and(|sql| {
                        *sql != self.workbench.sql_forms.draft_snapshot(&draft.kind)
                    })
                    || (draft.kind == "redis" && draft.ttl != self.workbench.ttl_policy)
            }),
        }
    }

    pub(crate) fn confirm_modal_action(&mut self, action: String) {
        self.workbench.overlay_close_confirm = true;
        self.workbench.modal_after_close = Some(action);
        if let Some(draft) = self.workbench.modal_draft.as_mut() {
            draft.focus = self.focus.clone();
        }
        self.workbench.generic_select = None;
        self.workbench.sql_forms.open_select = None;
        self.workbench.cell_fields_open = false;
        self.workbench.focus_request = Some("overlay-close-safe".into());
    }

    fn modal_close_busy(&self) -> bool {
        match self.workbench.overlay.as_deref() {
            Some("profile") => self.connection_form_busy(),
            Some("settings") => {
                self.settings_save_token.is_some() || !self.field("mcp-rule-pending").is_empty()
            }
            Some("import" | "export" | "transfer") => self.operation_pending,
            Some("file") => self.file_open_pending.is_some(),
            _ => false,
        }
    }

    pub(crate) fn modal_close_action(&mut self, action: &str, backend: &Backend) -> bool {
        if self.workbench.overlay.as_deref() == Some("settings")
            && self.field("settings-reload-confirm") == "true"
        {
            if action == "overlay-close" {
                self.application_action("settings-reload-cancel", backend);
                return true;
            }
            if !matches!(action, "settings-reload-confirm" | "settings-reload-cancel") {
                return true;
            }
        }
        if action == "overlay-close-continue"
            || (action == "overlay-close" && self.workbench.overlay_close_confirm)
        {
            self.workbench.overlay_close_confirm = false;
            self.workbench.modal_after_close = None;
            if let Some(draft) = &self.workbench.modal_draft {
                self.focus = draft.focus.clone();
                self.workbench.focus_request = Some(format!("input-{}", draft.focus));
            }
            return true;
        }
        if action == "overlay-close-discard" {
            if !self.workbench.overlay_close_confirm || self.modal_close_busy() {
                return true;
            }
            let after = self.workbench.modal_after_close.take();
            match self.workbench.overlay.as_deref() {
                Some("settings") => self.settings_discard_form(),
                Some("profile") => self.connection_draft_capture(),
                _ => {
                    if let Some(draft) = self.workbench.modal_draft.take() {
                        for (name, text) in draft.fields {
                            self.field_set(name, text);
                        }
                        self.workbench.ttl_policy = draft.ttl;
                        if matches!(draft.kind.as_str(), "row" | "filters") {
                            self.init_sql_form(&draft.kind);
                        }
                    }
                }
            }
            self.workbench.overlay_close_confirm = false;
            self.workbench_action("overlay-close", backend);
            if let Some(after) = after.filter(|after| after != "overlay-close") {
                self.action(&after, backend);
            }
            return true;
        }
        if self.workbench.overlay_close_confirm {
            return true;
        }
        if let Some(cell) = action
            .strip_prefix("modal-open-cell:")
            .and_then(|value| value.split_once(':'))
            .and_then(|(row, column)| {
                Some(Cell {
                    row: row.parse().ok()?,
                    column: column.parse().ok()?,
                })
            })
        {
            self.open_cell_at(cell, backend);
            return true;
        }
        let closing = action == "overlay-close"
            || action.starts_with("cell-field:")
            || (action == "advanced-value-open"
                && self.workbench.overlay.as_deref() == Some("cell"))
            || (action == "archery-open" && self.workbench.overlay.as_deref() == Some("profile"));
        let closing = closing
            || (self.workbench.overlay.is_some()
                && (matches!(action, "app-settings" | "history")
                    || action.starts_with("overlay:")));
        if !closing
            || self.archery.open
            || self.workbench.overlay.as_deref() == Some("advanced-value")
        {
            return false;
        }
        if self.modal_close_busy() {
            self.status = UiMessage::key("操作进行中，请等待完成后再关闭");
            return true;
        }
        if self.modal_has_unsaved() {
            self.confirm_modal_action(action.to_owned());
            return true;
        }
        false
    }
}

#[cfg(test)]
#[path = "../../tests/unit/modal_drafts.rs"]
mod tests;
