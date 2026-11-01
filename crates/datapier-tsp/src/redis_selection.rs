use super::*;

#[derive(Clone)]
struct DeleteContext {
    tab: Uuid,
    generation: u64,
    profile: Uuid,
    database: u32,
    session: Value,
    targets: Vec<ResourceTarget>,
}

#[derive(Clone)]
enum DeleteDraft {
    Confirm(DeleteContext),
    Opening(DeleteContext, Uuid),
    Ready(DeleteContext, Value),
}

#[derive(Default)]
pub(super) struct RedisKeyDeleteState {
    drafts: BTreeMap<Uuid, DeleteDraft>,
    opening: BTreeMap<Uuid, DeleteContext>,
}

impl DeleteDraft {
    fn context(&self) -> &DeleteContext {
        match self {
            Self::Confirm(context) | Self::Opening(context, _) | Self::Ready(context, _) => context,
        }
    }
}

impl App {
    fn redis_delete_context(&self, targets: Vec<ResourceTarget>) -> Option<DeleteContext> {
        let tab = self.tabs.get(self.active)?;
        let profile = self.profile.as_ref()?;
        if !self.is_redis() || profile.ensure_write().is_err() || targets.is_empty() {
            return None;
        }
        Some(DeleteContext {
            tab: tab.id,
            generation: self.generation,
            profile: profile.id,
            database: profile.redis_db,
            session: self.session_id()?,
            targets,
        })
    }

    fn redis_delete_context_current(&self, context: &DeleteContext) -> bool {
        let Some(index) = self.tabs.iter().position(|tab| tab.id == context.tab) else {
            return false;
        };
        let (generation, profile, session) = if index == self.active {
            (
                self.generation,
                self.profile.as_ref(),
                self.session.as_ref(),
            )
        } else {
            let runtime = &self.tabs[index].runtime;
            (
                runtime.generation,
                runtime.profile.as_ref(),
                runtime.session.as_ref(),
            )
        };
        generation == context.generation
            && profile.is_some_and(|p| p.id == context.profile && p.redis_db == context.database)
            && session.and_then(|s| s.get("session_id")) == Some(&context.session)
    }

    pub(super) fn pending_key_delete_count(&self) -> usize {
        self.tabs
            .get(self.active)
            .and_then(|tab| self.redis_key_delete.drafts.get(&tab.id))
            .and_then(|draft| match draft {
                DeleteDraft::Ready(context, _) => Some(context.targets.len()),
                _ => None,
            })
            .unwrap_or(0)
    }

    pub(super) fn redis_key_delete_targets(&self) -> &[ResourceTarget] {
        self.tabs
            .get(self.active)
            .and_then(|tab| self.redis_key_delete.drafts.get(&tab.id))
            .map_or(&[], |draft| draft.context().targets.as_slice())
    }

    pub(super) fn redis_key_delete_opening(&self) -> bool {
        self.tabs.get(self.active).is_some_and(|tab| {
            matches!(
                self.redis_key_delete.drafts.get(&tab.id),
                Some(DeleteDraft::Opening(..))
            )
        })
    }

    pub(super) fn redis_key_delete_tab_pending(&self, tab: Uuid) -> bool {
        self.redis_key_delete.drafts.contains_key(&tab)
    }

    // 只保存意图，WATCH 连接和令牌不能跨进程或会话恢复。
    pub(super) fn redis_key_delete_snapshot(&self, tab: Uuid) -> Option<Value> {
        let draft = self.redis_key_delete.drafts.get(&tab)?;
        if !matches!(draft, DeleteDraft::Ready(..)) {
            return None;
        }
        let context = draft.context();
        Some(json!({"operation":"delete_keys", "targets":context.targets,
            "profile_id":context.profile, "redis_db":context.database, "requires_refresh":true}))
    }

    pub(super) fn redis_key_delete_input(
        &self,
    ) -> std::result::Result<Option<Value>, i18n::UiMessage> {
        let Some(tab) = self.tabs.get(self.active) else {
            return Ok(None);
        };
        let Some(draft) = self.redis_key_delete.drafts.get(&tab.id) else {
            return Ok(None);
        };
        let DeleteDraft::Ready(context, token) = draft else {
            return Err(i18n::UiMessage::key("请等待 Redis 删除基线建立"));
        };
        if !self.redis_delete_context_current(context) {
            return Err(i18n::UiMessage::key("Redis 删除上下文已变化，请重新确认"));
        }
        Ok(Some(
            json!({"session_id":context.session,"workspace_id":self.workspace_id,
            "token":token,"targets":context.targets,"operation":"delete_keys"}),
        ))
    }

    pub(super) fn redis_selection_action(&mut self, backend: &Backend, action: &str) -> bool {
        let Some(tab) = self.tabs.get(self.active) else {
            return false;
        };
        let tab_id = tab.id;
        match action {
            "redis-key-delete-request" | "restore-pending" => {
                let restored = action == "restore-pending";
                if restored
                    && tab.pending.get("operation").and_then(Value::as_str) != Some("delete_keys")
                {
                    return false;
                }
                if self.operation_pending || self.task.is_some() {
                    self.status = "请等待或取消当前任务".into();
                    return true;
                }
                if self.redis_key_delete.drafts.contains_key(&tab_id) {
                    self.status = "请先提交或丢弃待提交变更".into();
                    return true;
                }
                let targets = if restored {
                    let pending = &tab.pending;
                    if tab.pending_live {
                        self.status = "请先提交或丢弃待提交变更".into();
                        return true;
                    }
                    let profile_matches = self.profile.as_ref().is_some_and(|p| {
                        pending.get("profile_id") == Some(&json!(p.id))
                            && pending.get("redis_db").and_then(Value::as_u64)
                                == Some(u64::from(p.redis_db))
                    });
                    if !profile_matches {
                        self.status = "请连接原 Redis 数据库后重新确认".into();
                        return true;
                    }
                    match serde_json::from_value::<Vec<ResourceTarget>>(pending["targets"].clone())
                    {
                        Ok(targets) => targets,
                        Err(_) => {
                            self.status = "保存的 Redis 删除目标无效".into();
                            return true;
                        }
                    }
                } else {
                    if self.has_live_drafts() || !tab.pending.is_null() {
                        self.status = "请先提交或丢弃待提交变更".into();
                        return true;
                    }
                    self.selected_redis_key_targets()
                };
                let Some(context) = self.redis_delete_context(targets) else {
                    self.status = "请选择可写 Redis 连接中的 key".into();
                    return true;
                };
                self.redis_key_delete
                    .drafts
                    .insert(tab_id, DeleteDraft::Confirm(context));
                self.workbench.overlay = Some("redis-key-delete".into());
                true
            }
            "redis-key-delete-confirm" => {
                if self.operation_pending || self.task.is_some() {
                    self.status = "请等待或取消当前任务".into();
                    return true;
                }
                let Some(DeleteDraft::Confirm(context)) =
                    self.redis_key_delete.drafts.get(&tab_id).cloned()
                else {
                    return true;
                };
                if !self.redis_delete_context_current(&context) {
                    self.redis_key_delete.drafts.remove(&tab_id);
                    self.workbench.overlay = None;
                    self.status = "Redis 删除上下文已变化，请重新确认".into();
                    return true;
                }
                let request_id = Uuid::new_v4();
                let tag = format!("redis-key-delete.open/{request_id}");
                let params = json!({"session_id":context.session,"workspace_id":self.workspace_id,
                    "targets":context.targets});
                match backend.request(tag, "change.open", params) {
                    Ok(()) => {
                        self.redis_key_delete
                            .opening
                            .insert(request_id, context.clone());
                        self.redis_key_delete
                            .drafts
                            .insert(tab_id, DeleteDraft::Opening(context, request_id));
                    }
                    Err(error) => {
                        self.redis_key_delete.drafts.remove(&tab_id);
                        self.status = i18n::UiMessage::raw(error.to_string());
                    }
                }
                self.workbench.overlay = None;
                true
            }
            "redis-key-delete-cancel" => {
                if matches!(
                    self.redis_key_delete.drafts.get(&tab_id),
                    Some(DeleteDraft::Confirm(_) | DeleteDraft::Opening(..))
                ) {
                    self.redis_key_delete.drafts.remove(&tab_id);
                }
                self.workbench.overlay = None;
                true
            }
            "discard"
                if self.redis_key_delete.drafts.contains_key(&tab_id)
                    || tab.pending.get("operation").and_then(Value::as_str)
                        == Some("delete_keys") =>
            {
                if self.operation_pending || self.task.is_some() {
                    self.status = "请等待或取消当前任务".into();
                    return true;
                }
                if let Some(DeleteDraft::Ready(context, token)) =
                    self.redis_key_delete.drafts.remove(&tab_id)
                {
                    let _ = backend.request(
                        format!("redis-key-delete.discard/{}", Uuid::new_v4()),
                        "change.discard",
                        json!({"session_id":context.session,"token":token}),
                    );
                }
                self.tabs[self.active].pending = Value::Null;
                self.tabs[self.active].pending_live = false;
                self.workbench.overlay = None;
                self.dirty = true;
                true
            }
            _ => false,
        }
    }

    // 无 tab 路由的请求标识允许迟到响应释放令牌。
    pub(super) fn redis_selection_response(
        &mut self,
        backend: &Backend,
        tag: &str,
        result: &Result<Value>,
    ) -> bool {
        if tag.starts_with("redis-key-delete.discard/") {
            return true;
        }
        let Some(id) = tag
            .strip_prefix("redis-key-delete.open/")
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            return false;
        };
        let Some(context) = self.redis_key_delete.opening.remove(&id) else {
            return true;
        };
        let tab = context.tab;
        let still_opening = matches!(self.redis_key_delete.drafts.get(&tab),
            Some(DeleteDraft::Opening(_, request)) if *request == id);
        if still_opening {
            self.redis_key_delete.drafts.remove(&tab);
        }
        match result {
            Err(error) => self.status = i18n::UiMessage::raw(error.to_string()),
            Ok(value) => {
                let token = value.get("token").cloned().filter(|v| v.as_str().is_some());
                if let Some(token) = token {
                    if still_opening
                        && self.redis_delete_context_current(&context)
                        && value.get("operation").and_then(Value::as_str) == Some("delete_keys")
                        && value.get("targets") == Some(&json!(context.targets))
                    {
                        self.redis_key_delete
                            .drafts
                            .insert(tab, DeleteDraft::Ready(context, token));
                        if let Some(index) = self.tabs.iter().position(|t| t.id == tab) {
                            self.tabs[index].pending_live = true;
                        }
                        self.dirty = true;
                    } else {
                        let _ = backend.request(
                            format!("redis-key-delete.discard/{}", Uuid::new_v4()),
                            "change.discard",
                            json!({"session_id":context.session,"token":token}),
                        );
                        self.status = "Redis 删除上下文已变化，请重新确认".into();
                    }
                } else {
                    self.status = "Redis 删除基线缺少令牌".into();
                }
            }
        }
        true
    }

    // 提交后令牌可能已被消费；失败后必须重新 WATCH。
    pub(super) fn redis_key_delete_submit_failed(&mut self) {
        let Some(tab) = self.tabs.get(self.active).map(|tab| tab.id) else {
            return;
        };
        if let Some(intent) = self.redis_key_delete_snapshot(tab) {
            self.redis_key_delete.drafts.remove(&tab);
            self.tabs[self.active].pending = intent;
            self.tabs[self.active].pending_live = false;
            self.dirty = true;
        }
    }

    pub(super) fn redis_key_delete_finish_task(
        &mut self,
        value: &Value,
        backend: &Backend,
    ) -> bool {
        let Some(tab) = self.tabs.get(self.active).map(|tab| tab.id) else {
            return false;
        };
        let Some(intent) = self.redis_key_delete_snapshot(tab) else {
            return false;
        };
        let succeeded = value["state"].as_str() == Some("succeeded")
            && value["summary"]["items"].as_array().is_some_and(|items| {
                !items.is_empty()
                    && items
                        .iter()
                        .all(|item| item["status"].as_str() == Some("applied"))
            });
        self.redis_key_delete.drafts.remove(&tab);
        let active_deleted = succeeded
            && self.target.as_ref().is_some_and(|target| {
                target.key_b64.as_ref().is_some_and(|key| {
                    intent["targets"].as_array().is_some_and(|targets| {
                        targets
                            .iter()
                            .any(|item| item["key_b64"].as_str() == Some(key.as_str()))
                    })
                })
            });
        self.tabs[self.active].pending = if succeeded { Value::Null } else { intent };
        self.tabs[self.active].pending_live = false;
        self.workbench.overlay = None;
        self.dirty = true;
        if succeeded {
            if active_deleted {
                if let (Some(session), Some(token)) = (self.session_id(), self.edit_token.take()) {
                    let _ = backend.request(
                        format!("redis-key-delete.discard/{}", Uuid::new_v4()),
                        "change.discard",
                        json!({"session_id":session,"token":token}),
                    );
                }
                self.target = None;
                self.schema = None;
                self.grid = None;
                self.result_id = None;
                self.results.clear();
                self.page_history.clear();
                self.page_token = None;
                self.browse_result = false;
                self.clear_redis_edit_state();
            }
            self.action("list", backend);
        } else {
            self.status = "删除未完成，基线已失效，请检查结果后重新确认".into();
        }
        true
    }
}

#[cfg(test)]
#[path = "../tests/unit/redis_selection.rs"]
mod tests;
