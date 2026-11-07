use super::*;

impl App {
    pub(crate) fn resize_cli(&mut self, share: f64) {
        if share.is_finite()
            && !self.tabs.is_empty()
            && self.is_redis()
            && self.workbench.cli
            && self.workbench.overlay.is_none()
            && self
                .profile
                .as_ref()
                .is_some_and(|profile| !profile.is_archery())
        {
            // 宿主约束实际 pane 最小尺寸；偏好只保存比例，不从夹紧后的高度反推。
            self.cli_share = share.clamp(0.15, 0.85);
        }
    }

    pub(crate) fn workbench_key(&mut self, key: &tern_sdk::Key, backend: &Backend) -> bool {
        // 键盘与鼠标共用类型切换入口，保持默认端口和数据库候选失效规则一致。
        if self.workbench.overlay.as_deref() == Some("profile")
            && let Some(focused) = self.focus.strip_prefix("provider-")
            && !key.ctrl
            && !key.meta
            && !key.alt
            && matches!(
                key.name.as_str(),
                "left" | "right" | "up" | "down" | "home" | "end" | "space" | "enter"
            )
        {
            if !self.connection_form_busy()
                && !self.form_editing
                && !(self.archery.open && self.archery.preview)
                && self.archery.pending.is_none()
            {
                let providers: &[&str] = &["mysql", "redis", "archery"];
                let current = providers
                    .iter()
                    .position(|provider| *provider == focused)
                    .unwrap_or(0);
                let next = match key.name.as_str() {
                    "home" => 0,
                    "end" => providers.len() - 1,
                    "left" | "up" => (current + providers.len() - 1) % providers.len(),
                    "right" | "down" => (current + 1) % providers.len(),
                    _ => current,
                };
                let provider = providers[next];
                if provider != "archery" || !self.archery.open {
                    let action = if provider == "archery" {
                        "archery-open".to_owned()
                    } else {
                        format!("field:provider:{provider}")
                    };
                    self.action(&action, backend);
                }
                self.focus = format!("provider-{provider}");
                self.workbench.focus_request = Some(format!("input-provider-{provider}"));
            }
            return true;
        }
        // 原生下拉先消费关闭与确认键，避免穿透到 CLI 或所属弹窗。
        let picker_open = self.navigation.redis_db_menu
            || self.navigation.form_redis_db_menu
            || self.workbench.key_type_menu
            || self.workbench.cell_fields_open
            || self.workbench.sql_forms.open_select.is_some()
            || self.workbench.generic_select.is_some();
        if picker_open && key.is("escape") {
            self.navigation.redis_db_menu = false;
            self.navigation.form_redis_db_menu = false;
            self.workbench.key_type_menu = false;
            self.workbench.cell_fields_open = false;
            self.workbench.close_form_select();
            self.workbench.generic_select = None;
            return true;
        }
        if picker_open && key.is("enter") {
            return true;
        }
        if self.navigation.redis_db_menu
            && matches!(key.name.as_str(), "up" | "down" | "home" | "end")
        {
            return true;
        }
        if self.approvals.open {
            if key.is("escape") {
                self.action("approval-close", backend);
                return true;
            }
            return false;
        }
        if self.workbench.overlay.is_some() {
            if key.is("escape") {
                self.action("overlay-close", backend);
                return true;
            }
            return false;
        }
        if !picker_open && self.focus == "result_info" && !key.ctrl && !key.meta && !key.alt {
            if matches!(key.name.as_str(), "enter" | "space") {
                let grid = if self.is_redis() {
                    self.workbench.cli_grid.as_ref()
                } else {
                    self.grid.as_ref()
                };
                if let Some(grid) = grid {
                    let action = format!("result-info-toggle:{}", grid.page.result.id);
                    self.workbench_action(&action, backend);
                }
                return true;
            }
            if matches!(key.name.as_str(), "tab" | "escape") {
                self.close_result_info(false);
                self.focus_current_tab();
                return true;
            }
        }
        if !picker_open
            && self.is_redis()
            && self.workbench.cli
            && self
                .profile
                .as_ref()
                .is_some_and(|profile| !profile.is_archery())
            && !key.ctrl
            && !key.meta
            && !key.alt
        {
            if key.name == "tab" && matches!(self.focus.as_str(), "cli_text" | "cli_height") {
                let (focus, node) = if self.focus == "cli_text" {
                    ("cli_height", "")
                } else {
                    ("cli_text", "input-cli_text")
                };
                self.focus = focus.into();
                self.workbench.focus_request = Some(node.into());
                return true;
            }
            if self.focus == "cli_height" {
                match key.name.as_str() {
                    "up" | "left" => self.resize_cli(self.cli_share + 0.05),
                    "down" | "right" => self.resize_cli(self.cli_share - 0.05),
                    "home" => self.resize_cli(0.15),
                    "end" => self.resize_cli(0.85),
                    "escape" | "enter" => {
                        self.focus = "cli_text".into();
                        self.workbench.focus_request = Some("input-cli_text".into());
                    }
                    _ => {}
                }
                return true;
            }
        }
        if self.is_redis() && self.workbench.cli && self.focus == "cli_text" && key.is("enter") {
            self.workbench_action("cli-run", backend);
            return true;
        }
        if self.workbench.inline.is_some() {
            if key.is("escape") {
                self.finish_inline(false);
                return true;
            }
            if key.is("enter") {
                self.finish_inline(true);
                return true;
            }
            if key.name == "tab" && !key.ctrl && !key.alt && !key.meta {
                if !self.finish_inline(true) {
                    return true;
                }
                let mut found = false;
                if let Some(grid) = &mut self.grid {
                    let direction = if key.shift {
                        Navigation::BackTab
                    } else {
                        Navigation::Tab
                    };
                    let max = grid
                        .page
                        .rows
                        .len()
                        .saturating_mul(grid.visible_columns().len());
                    let original = grid.focus;
                    for _ in 0..max {
                        let previous = grid.focus;
                        grid.navigate(direction, false);
                        if grid.focus == previous {
                            break;
                        }
                        if let Some(cell) = grid.focus
                            && grid
                                .page
                                .result
                                .columns
                                .get(cell.column)
                                .is_some_and(|c| !c.generated)
                            && grid
                                .value(cell)
                                .is_some_and(|value| !matches!(value, DataValue::Missing))
                        {
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        grid.focus = original;
                        grid.anchor = original;
                    }
                }
                if found {
                    self.begin_inline(backend);
                }
                return true;
            }
            return false;
        }
        if self.focus == "grid" && (key.is("enter") || key.is("f2")) {
            if self.is_redis() {
                self.workbench_redis_edit(backend);
            } else {
                self.begin_inline(backend);
            }
            return true;
        }
        false
    }
}
