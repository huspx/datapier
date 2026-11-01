//! 应用级设置和查询／标签操作；不重建数据库会话或工作区编辑状态。
use super::*;

pub(super) fn mysql_mode(value: &Value) -> Option<editor::MysqlMode> {
    let mode = value.get("sql_mode")?.as_str()?;
    Some(editor::MysqlMode {
        no_backslash_escapes: mode
            .split(',')
            .any(|s| s.eq_ignore_ascii_case("NO_BACKSLASH_ESCAPES")),
        ansi_quotes: mode
            .split(',')
            .any(|s| s.eq_ignore_ascii_case("ANSI_QUOTES")),
    })
}

impl App {
    pub(super) fn refresh_settings(&mut self, backend: &Backend) {
        if self.settings_save_token.is_some() || self.settings_unsaved {
            return;
        }
        if let Err(error) = backend.request(
            format!("settings.get/{}", self.settings_epoch),
            "settings.get",
            json!({}),
        ) {
            self.status = i18n::UiMessage::raw(error.to_string());
        }
    }

    fn save_settings(&mut self, backend: &Backend) {
        let token = Uuid::new_v4();
        self.settings_save_token = Some(token);
        self.settings_unsaved = false;
        if let Err(error) = backend.request(
            format!("settings.set/{token}"),
            "settings.set",
            json!({"settings":self.settings}),
        ) {
            self.settings_save_token = None;
            self.settings_unsaved = true;
            self.status = i18n::UiMessage::template(
                "语言选择仅应用于本次会话，未保存: {error}",
                vec![("error", i18n::UiArg::Raw(error.to_string()))],
            );
        }
    }

    pub(super) fn settings_response(&mut self, response: Response, backend: &Backend) {
        let saving = if let Some(token) = response.tag.strip_prefix("settings.set/") {
            if Uuid::parse_str(token)
                .ok()
                .is_none_or(|token| Some(token) != self.settings_save_token)
            {
                return;
            }
            self.settings_save_token = None;
            true
        } else if let Some(epoch) = response.tag.strip_prefix("settings.get/") {
            if epoch.parse::<u64>().ok() != Some(self.settings_epoch)
                || self.settings_save_token.is_some()
                || self.settings_unsaved
            {
                return;
            }
            false
        } else {
            return;
        };
        let response_pending = saving && self.settings_unsaved;
        match response
            .result
            .and_then(|v| serde_json::from_value::<AppSettings>(v).map_err(Into::into))
        {
            Ok(settings) => {
                if !saving || !self.settings_unsaved {
                    self.settings = settings;
                    self.settings_unsaved = false;
                    i18n::set_preference(self.settings.language);
                }
            }
            Err(error) => {
                self.settings_unsaved |= saving;
                self.status = if saving {
                    i18n::UiMessage::template(
                        "语言选择仅应用于本次会话，未保存: {error}",
                        vec![("error", i18n::UiArg::Raw(error.to_string()))],
                    )
                } else {
                    i18n::UiMessage::raw(error.to_string())
                };
            }
        }
        if response_pending {
            // 只有处理完上一笔写入后才能发送期间选中的最新设置。
            self.save_settings(backend);
        }
    }

    pub(super) fn application_action(&mut self, name: &str, backend: &Backend) -> bool {
        if let Some(id) = name
            .strip_prefix("profile-settings:")
            .and_then(|id| Uuid::parse_str(id).ok())
        {
            self.edit_profile_after_close(id, false, backend);
            return true;
        }
        if let Some(id) = name
            .strip_prefix("close-tab:")
            .and_then(|id| Uuid::parse_str(id).ok())
        {
            self.close_tab(id, backend);
            return true;
        }
        for (prefix, direction) in [
            ("tab-close-left:", -1),
            ("tab-close-right:", 1),
            ("tab-close-all:", 0),
        ] {
            if let Some(id) = name
                .strip_prefix(prefix)
                .and_then(|s| Uuid::parse_str(s).ok())
            {
                if let Some(index) = self.tabs.iter().position(|tab| tab.id == id) {
                    let ids = self
                        .tabs
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| {
                            direction == 0
                                || (direction < 0 && *i < index)
                                || (direction > 0 && *i > index)
                        })
                        .map(|(_, tab)| tab.id)
                        .collect();
                    self.close_tabs(ids, None, backend);
                }
                return true;
            }
        }
        if name == "app-settings" {
            if self.finish_inline(true) {
                self.workbench.overlay = Some("settings".into());
            }
            return true;
        }
        if name == "query-result-close" {
            // 隐藏结果不等于取消任务；任务和独立结果引用继续由所属 tab 管理。
            self.workbench.query_ui.results_hidden = true;
            return true;
        }
        if let Some(language) = name.strip_prefix("language:") {
            let preference = match language {
                "system" => LanguagePreference::System,
                "zh-CN" => LanguagePreference::ZhCn,
                "en" => LanguagePreference::En,
                _ => return true,
            };
            self.workbench.generic_select = None;
            self.settings.language = preference;
            self.settings_epoch = self.settings_epoch.wrapping_add(1);
            i18n::set_preference(preference);
            self.settings_unsaved = true;
            if self.settings_save_token.is_none() {
                self.save_settings(backend);
            }
            return true;
        }
        false
    }

    pub(super) fn close_tab(&mut self, id: Uuid, backend: &Backend) {
        self.close_tabs(vec![id], None, backend);
    }

    pub(super) fn refresh_query_mode(&mut self, backend: &Backend) {
        if !self.is_redis()
            && let Some(session) = self.session_id()
        {
            self.request(
                backend,
                "query.info",
                "connection.info",
                json!({"session_id":session}),
            );
        }
    }

    pub(super) fn execute_query_text(&mut self, text: String, backend: &Backend) {
        if text.trim().is_empty() {
            self.status = i18n::UiMessage::key("没有可执行的文本");
            return;
        }
        if !self.finish_inline(true) {
            return;
        }
        if self.operation_pending || self.task.is_some() {
            self.status = i18n::UiMessage::key("该 tab 已有正在执行的任务");
            return;
        }
        if self.has_live_drafts() {
            self.status = i18n::UiMessage::key("请先提交或丢弃待提交变更与粘贴预览");
            return;
        }
        let Some(session) = self.session_id() else {
            self.status = i18n::UiMessage::key("请先连接数据库");
            return;
        };
        // SQL 可修改会话 sql_mode；旧模式与旧结果响应均不能用于新执行。
        self.generation = self.generation.wrapping_add(1);
        self.workbench.sql_mode = None;
        self.workbench.query_ui.executed = true;
        self.workbench.query_ui.results_hidden = false;
        self.workbench.query_ui.menu_open = false;
        self.browse_result = false;
        self.grid = None;
        self.schema = None;
        self.edit_token = None;
        self.target = None;
        self.results.clear();
        self.result_id = None;
        self.page_token = None;
        self.page_history.clear();
        self.large = None;
        self.view = "query".into();
        self.request(
            backend,
            "execution.start",
            "execution.start",
            json!({"session_id":session,"workspace_id":self.workspace_id,"text":text}),
        );
    }
}

#[cfg(test)]
#[path = "../tests/unit/application.rs"]
mod tests;
