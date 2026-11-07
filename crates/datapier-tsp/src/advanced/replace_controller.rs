//! App 与批量替换工作台之间的真实目标和响应路由。
use crate::advanced::replace::{ReplaceState, ReplaceTarget};
use crate::{App, Backend, Editor, Result, Value, i18n, json};
use datapier_core::model::{AccessMode, ProviderKind, Value as DataValue};
use std::collections::BTreeMap;

impl App {
    // live_page 只保证已返回的当前窗口，句柄与偏移不进入工作区草稿。
    fn replace_page_context(&self) -> std::result::Result<Option<Value>, i18n::UiMessage> {
        if self.operation_pending || self.task.is_some() || self.page_token.is_some() {
            return Err(i18n::UiMessage::key("请等待当前任务或分页完成"));
        }
        if self
            .profile
            .as_ref()
            .is_some_and(|p| p.provider == ProviderKind::Redis)
        {
            return Ok(None);
        }
        let grid = self
            .grid
            .as_ref()
            .ok_or_else(|| i18n::UiMessage::key("当前结果没有可用的编辑基线或可靠键"))?;
        if self.result_id.as_deref() != Some(grid.page.result.id.to_string().as_str())
            || !self.browse_result
        {
            return Err(i18n::UiMessage::key("当前结果没有可用的编辑基线或可靠键"));
        }
        Ok(Some(
            json!({"result_id":grid.page.result.id,"start":grid.page.start,"next":grid.page.next}),
        ))
    }

    fn replace_target(&self) -> std::result::Result<ReplaceTarget, i18n::UiMessage> {
        let profile = self
            .profile
            .as_ref()
            .ok_or_else(|| i18n::UiMessage::key("请先打开连接"))?;
        self.replace_page_context()?;
        if profile.access_mode != AccessMode::ReadWrite || profile.is_archery() {
            return Err(i18n::UiMessage::key("当前连接不可写"));
        }
        if profile.provider == ProviderKind::Redis {
            if !self.redis_changes.is_empty() {
                return Err(i18n::UiMessage::key("请先处理待提交变更"));
            }
            let targets = self
                .selected_redis_key_targets()
                .into_iter()
                .map(|target| {
                    serde_json::to_value(target).map_err(|e| i18n::UiMessage::raw(e.to_string()))
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            return Ok(ReplaceTarget::Redis { targets });
        }
        if !self.browse_result
            || self.edit_token.is_none()
            || !self.sql_operation_allowed("update")
            || !self.grid.as_ref().is_some_and(|grid| grid.editable)
        {
            return Err(i18n::UiMessage::key("当前结果没有可用的编辑基线或可靠键"));
        }
        let grid = self
            .grid
            .as_ref()
            .ok_or_else(|| i18n::UiMessage::key("当前结果没有可用的编辑基线或可靠键"))?;
        if !grid.edits().is_empty() || !self.pending_rows.is_empty() {
            return Err(i18n::UiMessage::key("请先处理待提交变更"));
        }
        let schema = self
            .schema
            .as_ref()
            .ok_or_else(|| i18n::UiMessage::key("当前结果没有可用的编辑基线或可靠键"))?;
        let keys: Vec<String> = schema
            .get("keys")
            .and_then(Value::as_array)
            .and_then(|v| v.first())
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .filter(|v: &Vec<String>| !v.is_empty())
            .ok_or_else(|| i18n::UiMessage::key("当前结果没有可用的编辑基线或可靠键"))?;
        let key_for = |row: u64| -> std::result::Result<Value, i18n::UiMessage> {
            let invalid = || i18n::UiMessage::key("当前结果没有可用的编辑基线或可靠键");
            let original = grid.original_row(row).ok_or_else(invalid)?;
            if original.len() != grid.page.result.columns.len() {
                return Err(invalid());
            }
            let mut key = BTreeMap::new();
            for name in &keys {
                let mut positions = grid
                    .page
                    .result
                    .columns
                    .iter()
                    .enumerate()
                    .filter(|(_, col)| col.name == *name);
                let (index, _) = positions.next().ok_or_else(invalid)?;
                if positions.next().is_some() {
                    return Err(invalid());
                }
                let value = original.get(index).ok_or_else(invalid)?;
                if matches!(
                    value,
                    DataValue::Null | DataValue::Missing | DataValue::Large { .. }
                ) {
                    return Err(invalid());
                }
                key.insert(name.clone(), value.clone());
            }
            serde_json::to_value(key).map_err(|e| i18n::UiMessage::raw(e.to_string()))
        };
        let selected_keys = grid
            .selected_rows()
            .into_iter()
            .map(&key_for)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let page_keys = (grid.page.start..grid.page.start + grid.page.rows.len() as u64)
            .map(key_for)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let columns = grid
            .page
            .result
            .columns
            .iter()
            .filter_map(|column| {
                let kind = column
                    .type_name
                    .split(|c: char| c == '(' || c.is_ascii_whitespace())
                    .next()?;
                if column.generated
                    || ![
                        "char",
                        "varchar",
                        "tinytext",
                        "text",
                        "mediumtext",
                        "longtext",
                    ]
                    .iter()
                    .any(|name| kind.eq_ignore_ascii_case(name))
                {
                    return None;
                }
                Some(column.name.clone())
            })
            .collect();
        let filters = serde_json::from_str::<Vec<Value>>(self.field("filter"))
            .map_err(|e| i18n::UiMessage::raw(e.to_string()))?;
        let sort = serde_json::from_str::<Vec<Value>>(self.field("sort"))
            .map_err(|e| i18n::UiMessage::raw(e.to_string()))?;
        let target = serde_json::to_value(
            self.target
                .as_ref()
                .ok_or_else(|| i18n::UiMessage::key("请先选择资源"))?,
        )
        .map_err(|e| i18n::UiMessage::raw(e.to_string()))?;
        Ok(ReplaceTarget::Mysql {
            target,
            selected_keys,
            page_keys,
            filters,
            sort,
            columns,
        })
    }

    /// 基于当前真实会话和原始可靠键打开工作台，已有计划不能被覆盖。
    pub(crate) fn replace_open(&mut self) {
        if self.advanced_replace.target.is_some() {
            self.replace_context_changed();
            self.workbench.overlay = Some("replace".into());
            return;
        }
        let Some(session) = self.session_id() else {
            self.status = i18n::UiMessage::key("请先打开连接");
            return;
        };
        match self.replace_target() {
            Ok(target) => {
                let page = self.replace_page_context().ok().flatten();
                self.advanced_replace
                    .open(session, self.workspace_id.clone(), target);
                self.advanced_replace.bind_page(page);
                self.workbench.overlay = Some("replace".into());
            }
            Err(error) => self.status = error,
        }
    }

    /// 上下文或选集变化时调用；旧计划仅可丢弃，不能提交。
    pub(crate) fn replace_context_changed(&mut self) {
        if self.advanced_replace.target.is_none() {
            return;
        }
        let same_session = self.session_id() == self.advanced_replace.session_id;
        let target = self.replace_target().ok();
        let page = self.replace_page_context().ok();
        let changed = !same_session
            || target
                .as_ref()
                .is_none_or(|target| !self.advanced_replace.context_matches(target))
            || page
                .as_ref()
                .is_none_or(|page| !self.advanced_replace.page_matches(page));
        if changed {
            if same_session
                && !self.advanced_replace.has_live_plan()
                && let (Some(target), Some(page)) = (target, page)
            {
                self.advanced_replace.retarget(target, page);
            } else {
                self.advanced_replace.invalidate();
            }
        }
    }

    /// 转发替换控件动作；在派发之前重核目标和真实选集。
    pub(crate) fn replace_action(&mut self, action: &str, backend: &Backend) -> bool {
        if !action.starts_with("replace:") {
            return false;
        }
        self.replace_context_changed();
        let Some(tab) = self.tabs.get(self.active) else {
            return true;
        };
        let route = format!("{}:{}", tab.id, self.generation);
        let handled = self.advanced_replace.action(action, backend, &route);
        self.dirty = true;
        handled
    }

    /// 常规响应路由确认 tab 与 generation 后调用，nonce 再由状态核对。
    pub(crate) fn replace_response(
        &mut self,
        tag: &str,
        result: &Result<Value>,
        backend: &Backend,
    ) -> bool {
        let Some(token) = tag.strip_prefix("replace/") else {
            return false;
        };
        self.replace_context_changed();
        if let Some(tab) = self.tabs.get(self.active) {
            let route = format!("{}:{}", tab.id, self.generation);
            self.advanced_replace
                .response(token, result, backend, &route);
            self.advanced_replace.flush_plan_event(backend, &route);
        }
        if self.advanced_replace.take_refresh_required() {
            // 即使任务失败或结果待确认，也可能已有部分写入，旧结果与编辑令牌均不能再用。
            if let Some(grid) = self.grid.take() {
                self.tabs[self.active].layout = grid.settings;
            }
            self.edit_token = None;
            self.results.clear();
            self.result_id = None;
            self.page_history.clear();
            self.page_token = None;
            self.large = None;
            self.schema = None;
            self.browse_result = false;
            // 阻止旧结果页响应重新装回已失效的 grid；替换计划仍可用新路由丢弃。
            self.generation = self.generation.wrapping_add(1);
            self.workbench.load_resource = true;
            self.action("describe", backend);
        }
        true
    }

    /// 周期调用以补齐丢失的任务事件。
    pub(crate) fn replace_poll(&mut self, backend: &Backend) {
        if let Some(tab) = self.tabs.get(self.active) {
            let route = format!("{}:{}", tab.id, self.generation);
            self.advanced_replace.poll(backend, &route);
        }
    }

    pub(crate) fn replace_editor(&mut self, name: &str) -> Option<&mut Editor> {
        self.advanced_replace.editor(name)
    }

    /// 仅包含条件与身份，不包含预览行、原值、计划或摘要。
    pub(crate) fn replace_draft(&self) -> Option<Value> {
        let profile_id = self.profile.as_ref()?.id;
        Some(json!({"profile_id":profile_id,"intent":self.advanced_replace.draft()?}))
    }

    /// 恢复前重新打开当前目标；恢复后的意图必须重新准备才能提交。
    pub(crate) fn replace_restore_draft(&mut self, draft: &Value) -> bool {
        if draft.get("profile_id").and_then(Value::as_str)
            != self
                .profile
                .as_ref()
                .map(|profile| profile.id.to_string())
                .as_deref()
        {
            return false;
        }
        if self.advanced_replace.has_live_plan() {
            return false;
        }
        let Some(session) = self.session_id() else {
            return false;
        };
        let Ok(target) = self.replace_target() else {
            return false;
        };
        let Ok(page) = self.replace_page_context() else {
            return false;
        };
        if let Some(profile) = self
            .profile
            .as_ref()
            .filter(|profile| profile.provider == ProviderKind::Redis)
        {
            let Some(targets) = draft
                .pointer("/intent/target/targets")
                .and_then(Value::as_array)
            else {
                return false;
            };
            let database = profile.redis_db().to_string();
            if targets.iter().any(|target| {
                target.get("database").and_then(Value::as_str) != Some(database.as_str())
            }) {
                return false;
            }
        }
        let mut state = ReplaceState::default();
        state.open(session, self.workspace_id.clone(), target);
        state.bind_page(page);
        if !draft
            .get("intent")
            .is_some_and(|intent| state.restore_draft(intent))
        {
            return false;
        }
        self.advanced_replace = state;
        true
    }
}
