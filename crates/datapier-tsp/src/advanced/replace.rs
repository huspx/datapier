//! 批量替换的输入、冻结预览和计划操作。
use crate::{Backend, Editor, Msg, Result, Value, i18n, json};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::replace::{ReplaceScope, ReplaceSpec};
use tern_sdk::ui::{self, Node, html};
use uuid::Uuid;

const PAGE_SIZE: u64 = 20;

/// 由调用方从当前会话和真实选集构造；键必须是完整可靠键而非行号。
#[derive(Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub(crate) enum ReplaceTarget {
    Mysql {
        target: Value,
        selected_keys: Vec<Value>,
        page_keys: Vec<Value>,
        filters: Vec<Value>,
        sort: Vec<Value>,
        columns: Vec<String>,
    },
    Redis {
        targets: Vec<Value>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Prepare,
    Read,
    Update,
    Submit,
    Discard,
    Detail,
    Poll,
    Cancel,
}

#[derive(Clone, Copy)]
enum TaskKind {
    Prepare,
    Submit,
}

type ReplaceDetail = (Value, String, u64, Vec<u8>, Option<u64>);

/// 一次打开的替换工作台。调用方在目标、选集或连接变化时必须调用 `invalidate`。
#[derive(Clone, Default)]
pub(crate) struct ReplaceState {
    pub pattern: Editor,
    pub replacement: Editor,
    pub target: Option<ReplaceTarget>,
    context_target: Option<ReplaceTarget>,
    context_page: Option<Value>,
    restored: bool,
    pub session_id: Option<Value>,
    pub workspace_id: String,
    pub scope: Option<ReplaceScope>,
    pub columns: Vec<String>,
    pub roles: Vec<String>,
    pub regex: bool,
    pub case_sensitive: bool,
    pub multi_line: bool,
    pub dot_matches_new_line: bool,
    summary: Option<Value>,
    page: Option<Value>,
    pending: Option<(Uuid, Operation)>,
    task: Option<(Value, TaskKind)>,
    last_poll: Option<std::time::Instant>,
    refresh_required: bool,
    event_refresh_pending: bool,
    error: Option<String>,
    result: Option<Value>,
    detail: Option<ReplaceDetail>,
    invalid: bool,
}

impl ReplaceState {
    /// 初始化时只接收真实目标；修改条件前须先丢弃已有计划。
    pub(crate) fn open(&mut self, session_id: Value, workspace_id: String, target: ReplaceTarget) {
        *self = Self {
            session_id: Some(session_id),
            workspace_id,
            context_target: Some(target.clone()),
            target: Some(target),
            scope: Some(ReplaceScope::Selected),
            case_sensitive: true,
            ..Self::default()
        };
    }

    /// 连接、目标或原始选集改变后禁止提交旧计划；仍允许显式丢弃。
    pub(crate) fn invalidate(&mut self) {
        self.invalid = true;
    }

    /// 尚未创建计划时，可将原有条件用于用户重新选择的实际目标。
    pub(crate) fn retarget(&mut self, target: ReplaceTarget, page: Option<Value>) {
        self.context_target = Some(target.clone());
        self.target = Some(target);
        self.context_page = page;
        self.invalid = false;
        self.error = None;
    }

    pub(crate) fn context_matches(&self, target: &ReplaceTarget) -> bool {
        self.context_target.as_ref() == Some(target)
    }

    /// 保存当前 live result 的身份与页边界，仅用于运行期失效检查。
    pub(crate) fn bind_page(&mut self, page: Option<Value>) {
        self.context_page = page;
    }

    pub(crate) fn page_matches(&self, page: &Option<Value>) -> bool {
        &self.context_page == page
    }

    /// 提交终态后调用方必须作废旧浏览结果和编辑基线。
    pub(crate) fn take_refresh_required(&mut self) -> bool {
        std::mem::take(&mut self.refresh_required)
    }

    /// 返回可持久化的用户意图；不包含计划、摘要或数据库原值。
    pub(crate) fn draft(&self) -> Option<Value> {
        Some(json!({"version":1,"target":self.target.as_ref()?,"spec":self.spec()}))
    }

    /// 恢复用户选定的原始身份；调用方先核对 profile、DB 和目标。
    pub(crate) fn restore_draft(&mut self, draft: &Value) -> bool {
        if draft.get("version").and_then(Value::as_u64) != Some(1) {
            return false;
        }
        let Ok(mut target) = serde_json::from_value::<ReplaceTarget>(
            draft.get("target").cloned().unwrap_or(Value::Null),
        ) else {
            return false;
        };
        let Ok(spec) = serde_json::from_value::<ReplaceSpec>(
            draft.get("spec").cloned().unwrap_or(Value::Null),
        ) else {
            return false;
        };
        match (self.target.as_ref(), &mut target) {
            (
                Some(ReplaceTarget::Mysql {
                    target: current,
                    columns,
                    ..
                }),
                ReplaceTarget::Mysql {
                    target: saved,
                    columns: saved_columns,
                    ..
                },
            ) if current == &*saved
                && spec.columns.iter().all(|column| columns.contains(column)) =>
            {
                *saved_columns = columns.clone();
            }
            (Some(ReplaceTarget::Redis { .. }), ReplaceTarget::Redis { .. })
                if spec.scope == ReplaceScope::Selected => {}
            _ => return false,
        }
        self.target = Some(target);
        self.restored = true;
        self.pattern = Editor::new(spec.pattern);
        self.replacement = Editor::new(spec.replacement);
        self.scope = Some(spec.scope);
        self.columns = spec.columns;
        self.roles = spec.roles;
        self.regex = spec.regex;
        self.case_sensitive = spec.case_sensitive;
        self.multi_line = spec.multi_line;
        self.dot_matches_new_line = spec.dot_matches_new_line;
        true
    }

    /// 活动计划或任务必须显式丢弃、完成或取消后才能替换状态。
    pub(crate) fn has_live_plan(&self) -> bool {
        self.summary.is_some() || self.task.is_some() || self.pending.is_some()
    }

    /// App 的编辑器分派应将这两个字段转发到这里。
    pub(crate) fn editor(&mut self, name: &str) -> Option<&mut Editor> {
        if self.summary.is_some() || self.task.is_some() || self.pending.is_some() {
            return None;
        }
        match name {
            "replace-pattern" => Some(&mut self.pattern),
            "replace-replacement" => Some(&mut self.replacement),
            _ => None,
        }
    }

    fn plan(&self) -> Option<(&Value, u64)> {
        let summary = self.summary.as_ref()?;
        Some((
            summary.get("id").or_else(|| summary.get("plan_id"))?,
            summary.get("revision")?.as_u64()?,
        ))
    }

    fn spec(&self) -> ReplaceSpec {
        ReplaceSpec {
            pattern: self.pattern.text.clone(),
            replacement: self.replacement.text.clone(),
            regex: self.regex,
            case_sensitive: self.case_sensitive,
            multi_line: self.multi_line,
            dot_matches_new_line: self.dot_matches_new_line,
            scope: self.scope.unwrap_or(ReplaceScope::Selected),
            columns: self.columns.clone(),
            roles: self.roles.clone(),
        }
    }

    fn request(
        &mut self,
        backend: &Backend,
        route: &str,
        operation: Operation,
        method: &str,
        mut params: Value,
    ) {
        if self.pending.is_some() {
            return;
        }
        let Some(session) = self.session_id.clone() else {
            self.error = Some(i18n::tr("请先打开连接").into_owned());
            return;
        };
        params["session_id"] = session;
        params["workspace_id"] = json!(self.workspace_id);
        let token = Uuid::new_v4();
        self.error = None;
        match backend.request(format!("{route}:replace/{token}"), method, params) {
            Ok(()) => self.pending = Some((token, operation)),
            Err(error) => self.error = Some(error.to_string()),
        }
    }

    /// 在 App 的定时轮询中调用，或在收到任务状态事件后调用。
    pub(crate) fn poll(&mut self, backend: &Backend, route: &str) {
        if self.pending.is_some()
            || self
                .last_poll
                .is_some_and(|last| last.elapsed() < std::time::Duration::from_secs(1))
        {
            return;
        }
        if let Some((task_id, _)) = &self.task {
            let params = json!({"task_id":task_id});
            self.request(backend, route, Operation::Poll, "execution.get", params);
            if self.pending.is_some() {
                self.last_poll = Some(std::time::Instant::now());
            }
        }
    }
    /// 事件只提示状态可能变化，必须读取受限预览页重新核对。
    pub(crate) fn refresh_plan_event(&mut self, id: &str, backend: &Backend, route: &str) {
        if self.plan().and_then(|(plan, _)| plan.as_str()) == Some(id) {
            self.event_refresh_pending = true;
            self.flush_plan_event(backend, route);
        }
    }

    pub(crate) fn flush_plan_event(&mut self, backend: &Backend, route: &str) {
        if self.event_refresh_pending && self.pending.is_none() {
            self.event_refresh_pending = false;
            self.read(backend, route, 0);
        }
    }

    fn prepare(&mut self, backend: &Backend, route: &str) {
        if self.summary.is_some() || self.invalid {
            return;
        }
        let spec = self.spec();
        let Some(target) = self.target.as_ref() else {
            return;
        };
        let params = match target {
            ReplaceTarget::Mysql {
                target,
                selected_keys,
                page_keys,
                filters,
                sort,
                ..
            } => {
                let keys = if spec.scope == ReplaceScope::CurrentPage {
                    page_keys
                } else {
                    selected_keys
                };
                if spec.columns.is_empty()
                    || (spec.scope != ReplaceScope::Filtered && keys.is_empty())
                {
                    self.error = Some(i18n::tr("请选择替换列和目标行").into_owned());
                    return;
                }
                json!({"target": target, "keys": keys, "filters": filters, "sort": sort, "spec": spec})
            }
            ReplaceTarget::Redis { targets } => {
                if targets.is_empty() || spec.roles.is_empty() {
                    self.error = Some(i18n::tr("请选择 key 和替换角色").into_owned());
                    return;
                }
                json!({"targets": targets, "spec": spec})
            }
        };
        self.request(
            backend,
            route,
            Operation::Prepare,
            "replace.prepare",
            params,
        );
    }

    fn read(&mut self, backend: &Backend, route: &str, offset: u64) {
        let Some((id, revision)) = self.plan() else {
            return;
        };
        let params =
            json!({"plan_id": id, "revision": revision, "offset": offset, "limit": PAGE_SIZE});
        self.request(backend, route, Operation::Read, "replace.read", params);
    }

    fn discard(&mut self, backend: &Backend, route: &str) {
        let Some((id, revision)) = self.plan() else {
            return;
        };
        let params = json!({"plan_id": id, "revision": revision});
        self.request(
            backend,
            route,
            Operation::Discard,
            "replace.discard",
            params,
        );
    }

    fn submit(&mut self, backend: &Backend, route: &str) {
        if self.invalid {
            return;
        }
        let Some(summary) = self.summary.as_ref() else {
            return;
        };
        if summary.get("complete").and_then(Value::as_bool) != Some(true)
            || summary.get("submittable").and_then(Value::as_bool) != Some(true)
        {
            return;
        }
        let Some(digest) = summary
            .get("plan_digest")
            .or_else(|| summary.get("digest"))
            .filter(|v| v.as_str().is_some())
        else {
            return;
        };
        let Some((id, revision)) = self.plan() else {
            return;
        };
        let params = json!({"plan_id": id, "revision": revision, "digest": digest});
        self.request(backend, route, Operation::Submit, "replace.submit", params);
    }

    fn toggle_item(&mut self, backend: &Backend, route: &str, index: usize) {
        if self.invalid {
            return;
        }
        let Some(item) = self
            .page
            .as_ref()
            .and_then(|p| p.get("items"))
            .and_then(Value::as_array)
            .and_then(|items| items.get(index))
        else {
            return;
        };
        let Some(id) = item.get("id").cloned() else {
            return;
        };
        let Some(selected) = item.get("selected").and_then(Value::as_bool) else {
            self.error = Some(i18n::tr("替换条目状态无效").into_owned());
            return;
        };
        let Some((plan_id, revision)) = self.plan() else {
            return;
        };
        let params = json!({"plan_id":plan_id,"revision":revision,"selections":[{"id":id,"selected":!selected}]});
        self.request(backend, route, Operation::Update, "replace.update", params);
    }

    fn detail_read(
        &mut self,
        backend: &Backend,
        route: &str,
        id: Value,
        side: String,
        offset: u64,
    ) {
        let Some((plan_id, revision)) = self.plan() else {
            return;
        };
        let params =
            json!({"plan_id":plan_id,"revision":revision,"item_id":id,"side":side,"offset":offset});
        self.request(backend, route, Operation::Detail, "replace.read", params);
        if self.pending.is_some() {
            self.detail = Some((id, side, offset, Vec::new(), None));
        }
    }

    /// 处理本模块产生的 action；返回 false 代表交由其他模块处理。
    /// `route` 必须为当前 tab 与 generation 组成的 `tab:generation`。
    pub(crate) fn action(&mut self, action: &str, backend: &Backend, route: &str) -> bool {
        if !action.starts_with("replace:") {
            return false;
        }
        if self.pending.is_some() {
            return true;
        }
        if self.task.is_some() && !matches!(action, "replace:cancel" | "replace:refresh") {
            return true;
        }
        match action {
            "replace:reset" if self.invalid && self.summary.is_none() => {
                *self = Self::default();
            }
            "replace:prepare" => self.prepare(backend, route),
            "replace:discard" => self.discard(backend, route),
            "replace:submit" => self.submit(backend, route),
            "replace:prev" => {
                let offset = self
                    .page
                    .as_ref()
                    .and_then(|p| p.get("offset"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .saturating_sub(PAGE_SIZE);
                self.read(backend, route, offset);
            }
            "replace:next" => {
                if let Some(offset) = self
                    .page
                    .as_ref()
                    .and_then(|p| p.get("next_offset"))
                    .and_then(Value::as_u64)
                {
                    self.read(backend, route, offset);
                }
            }
            "replace:read" => self.read(backend, route, 0),
            "replace:cancel" => {
                if let Some((id, _)) = &self.task {
                    let params = json!({"task_id":id});
                    self.request(
                        backend,
                        route,
                        Operation::Cancel,
                        "execution.cancel",
                        params,
                    );
                }
            }
            "replace:refresh" => self.poll(backend, route),
            "replace:detail-next" => {
                if let Some((id, side, _, _, Some(offset))) = &self.detail {
                    self.detail_read(backend, route, id.clone(), side.clone(), *offset);
                }
            }
            _ if action.starts_with("replace:detail:") => {
                let mut parts = action[15..].split(':');
                if let (Some(side @ ("before" | "after")), Some(index)) =
                    (parts.next(), parts.next())
                    && let Some(id) = index
                        .parse::<usize>()
                        .ok()
                        .and_then(|i| {
                            self.page
                                .as_ref()?
                                .get("items")?
                                .as_array()?
                                .get(i)?
                                .get("id")
                        })
                        .cloned()
                {
                    self.detail_read(backend, route, id, side.to_owned(), 0);
                }
            }
            "replace:regex" if self.summary.is_none() => self.regex = !self.regex,
            "replace:case" if self.summary.is_none() => self.case_sensitive = !self.case_sensitive,
            "replace:multiline" if self.summary.is_none() => self.multi_line = !self.multi_line,
            "replace:dot" if self.summary.is_none() => {
                self.dot_matches_new_line = !self.dot_matches_new_line
            }
            "replace:selected" if self.summary.is_none() => {
                self.scope = Some(ReplaceScope::Selected)
            }
            "replace:page" if self.summary.is_none() => {
                self.scope = Some(ReplaceScope::CurrentPage)
            }
            "replace:filtered" if self.summary.is_none() => {
                self.scope = Some(ReplaceScope::Filtered)
            }
            _ if action.starts_with("replace:item:") => {
                if let Ok(i) = action[13..].parse() {
                    self.toggle_item(backend, route, i);
                }
            }
            _ if action.starts_with("replace:column:") && self.summary.is_none() => {
                if let Ok(i) = action[15..].parse::<usize>()
                    && let Some(ReplaceTarget::Mysql { columns, .. }) = &self.target
                    && let Some(column) = columns.get(i).cloned()
                {
                    toggle(&mut self.columns, column);
                }
            }
            _ if action.starts_with("replace:role:") && self.summary.is_none() => {
                let role = &action[13..];
                if REDIS_ROLES.iter().any(|(r, _)| *r == role) {
                    toggle(&mut self.roles, role.to_owned());
                }
            }
            _ => {}
        }
        true
    }

    /// App 的响应路由在确认 tab/generation 后传入 token；过期响应不会覆盖状态。
    pub(crate) fn response(
        &mut self,
        token: &str,
        result: &Result<Value>,
        backend: &Backend,
        route: &str,
    ) {
        let Some((expected, operation)) = self.pending else {
            return;
        };
        if Uuid::parse_str(token).ok() != Some(expected) {
            return;
        }
        self.pending = None;
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if operation == Operation::Read
                    && matches!(
                        error.code.as_str(),
                        "stale_revision" | "handle_expired" | "not_found"
                    )
                {
                    self.invalid = true;
                    self.page = None;
                }
                if matches!(operation, Operation::Submit | Operation::Update) {
                    self.invalid = true;
                }
                if operation == Operation::Submit {
                    self.refresh_required = true;
                }
                self.error = Some(error.to_string());
                return;
            }
        };
        match operation {
            Operation::Prepare | Operation::Submit if value.get("task_id").is_some() => {
                let kind = if operation == Operation::Prepare {
                    TaskKind::Prepare
                } else {
                    TaskKind::Submit
                };
                self.task = Some((value["task_id"].clone(), kind));
                self.last_poll = None;
                if operation == Operation::Submit {
                    self.page = None;
                }
                self.poll(backend, route);
            }
            Operation::Prepare | Operation::Update => {
                let incoming = value.get("summary").unwrap_or(value);
                if operation == Operation::Update {
                    let valid = self.plan().is_some_and(|(id, revision)| {
                        incoming.get("id").or_else(|| incoming.get("plan_id")) == Some(id)
                            && incoming
                                .get("revision")
                                .and_then(Value::as_u64)
                                .is_some_and(|next| next > revision)
                            && incoming
                                .get("plan_digest")
                                .or_else(|| incoming.get("digest"))
                                .and_then(Value::as_str)
                                .is_some()
                    });
                    if !valid {
                        self.invalid = true;
                        self.error = Some(i18n::tr("替换预览响应无效").into_owned());
                        return;
                    }
                }
                // update 只返回 revision/digest 的 provider 需要保留 plan ID。
                if operation == Operation::Update {
                    if let Some(summary) = self.summary.as_mut().and_then(Value::as_object_mut)
                        && let Some(fields) = incoming.as_object()
                    {
                        for (key, value) in fields {
                            summary.insert(key.clone(), value.clone());
                        }
                    }
                } else {
                    self.summary = Some(incoming.clone());
                }
                if self.plan().is_some() {
                    let offset = self
                        .page
                        .as_ref()
                        .and_then(|p| p.get("offset"))
                        .and_then(Value::as_u64)
                        .unwrap_or(0);
                    self.page = None;
                    self.detail = None;
                    self.read(backend, route, offset);
                } else {
                    self.error = Some(i18n::tr("替换计划尚未就绪").into_owned());
                }
            }
            Operation::Read => {
                let Some(summary) = value.get("summary") else {
                    self.error = Some(i18n::tr("替换预览响应无效").into_owned());
                    return;
                };
                let expected = self.plan();
                let actual_id = summary.get("id").or_else(|| summary.get("plan_id"));
                let old_digest = self
                    .summary
                    .as_ref()
                    .and_then(|s| s.get("plan_digest").or_else(|| s.get("digest")));
                let new_digest = summary.get("plan_digest").or_else(|| summary.get("digest"));
                if expected.is_none_or(|(id, revision)| {
                    actual_id != Some(id)
                        || summary.get("revision").and_then(Value::as_u64) != Some(revision)
                }) || old_digest != new_digest
                    || !value.get("items").is_some_and(Value::is_array)
                    || !value.get("offset").is_some_and(Value::is_u64)
                {
                    self.invalid = true;
                    self.error = Some(i18n::tr("替换预览响应无效").into_owned());
                    return;
                }
                self.summary = Some(summary.clone());
                self.page = Some(value.clone());
            }
            Operation::Discard => {
                self.summary = None;
                self.page = None;
                self.detail = None;
            }
            Operation::Submit => {
                self.result = Some(value.clone());
                self.page = None;
                self.invalid = true;
                self.refresh_required = true;
            }
            Operation::Detail => {
                let bytes = value
                    .get("base64")
                    .and_then(Value::as_str)
                    .and_then(|s| STANDARD.decode(s).ok());
                if let (Some((_, _, _, data, next)), Some(bytes)) = (self.detail.as_mut(), bytes) {
                    *data = bytes;
                    *next = value.get("next_offset").and_then(Value::as_u64);
                } else {
                    self.error = Some(i18n::tr("替换详情响应无效").into_owned());
                }
            }
            Operation::Poll => {
                let Some((task_id, kind)) = self.task.clone() else {
                    return;
                };
                if value.get("id") != Some(&task_id) {
                    self.error = Some(i18n::tr("任务状态无效").into_owned());
                    return;
                }
                match value.get("state").and_then(Value::as_str) {
                    Some("queued" | "running" | "cancelling") => {}
                    Some("succeeded") => {
                        self.task = None;
                        match kind {
                            TaskKind::Prepare => {
                                self.summary = value
                                    .get("summary")
                                    .map(|s| s.get("summary").unwrap_or(s).clone());
                                if self.plan().is_some() {
                                    self.read(backend, route, 0);
                                } else {
                                    self.error = Some(i18n::tr("替换计划尚未就绪").into_owned());
                                }
                            }
                            TaskKind::Submit => {
                                self.result = value
                                    .get("summary")
                                    .cloned()
                                    .or_else(|| Some(value.clone()));
                                self.invalid = true;
                                self.refresh_required = true;
                            }
                        }
                    }
                    Some("failed" | "cancelled" | "outcome_unknown" | "wait_stopped") => {
                        self.task = None;
                        if matches!(kind, TaskKind::Submit) {
                            self.invalid = true;
                            self.refresh_required = true;
                        }
                        self.result = Some(value.clone());
                        if let Some(error) = value.get("error").filter(|v| !v.is_null()) {
                            self.error = Some(display(error));
                        }
                    }
                    _ => self.error = Some(i18n::tr("任务状态无效").into_owned()),
                }
            }
            Operation::Cancel => {
                self.poll(backend, route);
            }
        }
    }
}

const REDIS_ROLES: &[(&str, &str)] = &[
    ("key", "Key"),
    ("string_value", "String 值"),
    ("hashfield", "Hash 字段"),
    ("hash_value", "Hash 值"),
    ("list_value", "List 值"),
    ("member", "Set／ZSet 成员"),
];

fn toggle(values: &mut Vec<String>, value: String) {
    if let Some(index) = values.iter().position(|v| *v == value) {
        values.remove(index);
    } else {
        values.push(value);
    }
}

fn button(label: &str, action: String, disabled: bool) -> Node<Msg> {
    ui::button(i18n::tr(label).into_owned())
        .variant(
            if action == "replace:submit" || action == "replace:prepare" {
                ui::ButtonVariant::Primary
            } else {
                ui::ButtonVariant::Default
            },
        )
        .key(action.clone())
        .disabled(disabled)
        .on_click(Msg::Action(action))
        .into()
}
fn choice(label: &str, action: String, selected: bool, disabled: bool) -> Node<Msg> {
    let mark = if selected { "☑ " } else { "☐ " };
    ui::button(format!("{mark}{}", i18n::tr(label)))
        .variant(ui::ButtonVariant::Quiet)
        .on(selected)
        .key(action.clone())
        .disabled(disabled)
        .on_click(Msg::Action(action))
        .into()
}
fn field(name: &str, label: &str, editor: &Editor, disabled: bool) -> Node<Msg> {
    let focus = name.to_owned();
    let undo = focus.clone();
    let edit = focus.clone();
    ui::field(i18n::tr(label).into_owned())
        .child(
            ui::input()
                .aria(i18n::tr(label).into_owned())
                .variant(ui::InputVariant::Field)
                .key(format!("input-{name}"))
                .readonly(disabled)
                .text(editor.text.clone())
                .cursor(editor.cursor)
                .anchor(editor.anchor)
                .on_focus(Msg::Focus(focus))
                .on_undo(Msg::Undo(undo))
                .on_edit(move |e| {
                    Msg::Edit(edit.clone(), e.from, e.to, e.text.clone(), e.cursor, e.len)
                }),
        )
        .into()
}
fn display(value: &Value) -> String {
    if let Some(encoded) = value.get("base64").and_then(Value::as_str)
        && let Ok(bytes) = STANDARD.decode(encoded)
    {
        let text = std::str::from_utf8(&bytes)
            .map(str::to_owned)
            .unwrap_or_else(|_| format!("base64:{encoded}"));
        return if value.get("complete").and_then(Value::as_bool) == Some(false) {
            format!("{text}… ({})", i18n::tr("摘要"))
        } else {
            text
        };
    }
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

/// 渲染可嵌入 overlay 的正文；按钮动作由 `action` 处理。
pub(crate) fn view(state: &ReplaceState) -> Node<Msg> {
    let frozen = state.summary.is_some() || state.task.is_some();
    let busy = state.pending.is_some() || state.task.is_some();
    let mut panel = html::div()
        .class("change-preview advanced-panel")
        .child(field(
            "replace-pattern",
            "查找",
            &state.pattern,
            frozen || busy,
        ))
        .child(field(
            "replace-replacement",
            "替换为",
            &state.replacement,
            frozen || busy,
        ));
    if let Some(ReplaceTarget::Mysql { target, .. }) = &state.target {
        let mut context = html::div().class("advanced-facts");
        for (key, label) in [("database", "数据库"), ("name", "对象名称")] {
            if let Some(value) = target.get(key).and_then(Value::as_str) {
                context = context.child(super::fact(label, value));
            }
        }
        panel = panel.child(context);
    }
    if let Some(ReplaceTarget::Redis { targets }) = &state.target {
        let mut context = html::div().class("advanced-facts");
        for target in targets {
            if let Some(database) = target.get("database").and_then(Value::as_str) {
                context = context.child(super::fact("数据库", database));
            }
            if let Some(name) = target.get("name").and_then(Value::as_str) {
                context = context.child(super::fact("目标", name));
            }
        }
        panel = panel.child(context);
    }
    if frozen {
        panel = panel.child(
            html::p(i18n::tr("计划已冻结，修改条件前请先丢弃计划。").into_owned()).class("notice"),
        );
    }
    if state.invalid {
        panel = panel.child(
            html::p(i18n::tr("计划已失效，请重新打开替换工作台。").into_owned()).class("notice"),
        );
    }
    if state.invalid && !frozen && !busy {
        panel = panel.child(button("丢弃替换草稿", "replace:reset".into(), false));
    }
    for (label, action, enabled) in [
        ("正则表达式", "replace:regex", state.regex),
        ("区分大小写", "replace:case", state.case_sensitive),
        ("多行", "replace:multiline", state.multi_line),
        ("点匹配换行", "replace:dot", state.dot_matches_new_line),
    ] {
        let action = action.to_owned();
        panel = panel.child(
            ui::field(i18n::tr(label).into_owned()).child(
                ui::switch(i18n::tr(label).into_owned())
                    .on(enabled)
                    .disabled(frozen || busy)
                    .on_change(move |_| Msg::Action(action.clone())),
            ),
        );
    }
    if let Some(target) = &state.target {
        match target {
            ReplaceTarget::Mysql { columns, .. } => {
                let mut scopes = html::div().class("advanced-actions");
                for (label, action, scope) in [
                    ("选中行", "replace:selected", ReplaceScope::Selected),
                    ("当前页", "replace:page", ReplaceScope::CurrentPage),
                    ("筛选结果", "replace:filtered", ReplaceScope::Filtered),
                ] {
                    scopes = scopes.child(choice(
                        label,
                        action.into(),
                        state.scope == Some(scope),
                        frozen || busy,
                    ));
                }
                panel = panel.child(scopes);
                panel = panel.child(html::strong(i18n::tr("替换列").into_owned()));
                let mut choices = html::div().class("advanced-actions");
                for (index, column) in columns.iter().enumerate() {
                    let action = format!("replace:column:{index}");
                    let mark = if state.columns.contains(column) {
                        "☑ "
                    } else {
                        "☐ "
                    };
                    choices = choices.child(
                        ui::button(format!("{mark}{column}"))
                            .variant(ui::ButtonVariant::Quiet)
                            .on(state.columns.contains(column))
                            .key(action.clone())
                            .disabled(frozen || busy)
                            .on_click(Msg::Action(action)),
                    );
                }
                panel = panel.child(choices);
            }
            ReplaceTarget::Redis { .. } => {
                panel = panel.child(html::strong(i18n::tr("替换角色").into_owned()));
                let mut choices = html::div().class("advanced-actions");
                for (role, label) in REDIS_ROLES {
                    choices = choices.child(choice(
                        label,
                        format!("replace:role:{role}"),
                        state.roles.iter().any(|v| v == role),
                        frozen || busy,
                    ));
                }
                panel = panel.child(choices);
            }
        }
    }
    if state.restored {
        panel = panel.child(html::p(i18n::tr("已恢复替换条件和目标；不会恢复 grid 选中状态，请准备预览以重新确认数据库基线。").into_owned()).class("notice"));
    }
    if let Some(error) = &state.error {
        panel = panel.child(html::p(error.clone()).class("field-error"));
    }
    if busy {
        panel = panel.child(html::p(i18n::tr("处理中").into_owned()).class("muted"));
    }
    if state.task.is_some() {
        panel = panel
            .child(button(
                "刷新状态",
                "replace:refresh".into(),
                state.pending.is_some(),
            ))
            .child(button(
                "取消",
                "replace:cancel".into(),
                state.pending.is_some(),
            ));
    }
    if let Some(summary) = &state.summary {
        let mut facts = html::div().class("advanced-facts");
        for (key, label) in [
            ("scanned", "已扫描"),
            ("matched", "命中数"),
            ("total_items", "预览项"),
            ("non_utf8", "非 UTF-8"),
            ("unchanged", "未变更"),
            ("inapplicable", "不适用"),
            ("collisions", "碰撞"),
        ] {
            if let Some(value) = summary.get(key).filter(|value| !value.is_null()) {
                facts = facts.child(super::fact(label, display(value)));
            }
        }
        if let Some(reason) = summary.get("reason").and_then(Value::as_str) {
            facts = facts.child(super::fact("原因", reason));
        }
        panel = panel
            .child(html::strong(i18n::tr("变更预览").into_owned()))
            .child(facts)
            .child(super::diagnostic("replace-summary-diagnostic", summary));
    }
    if state.summary.is_some() && state.page.is_none() && !busy {
        panel = panel.child(button("刷新预览", "replace:read".into(), false));
    }
    if let Some(items) = state
        .page
        .as_ref()
        .and_then(|p| p.get("items"))
        .and_then(Value::as_array)
    {
        for (index, item) in items.iter().enumerate() {
            let selected = item.get("selected").and_then(Value::as_bool);
            let mut row = html::div().class("draft-item");
            if let Some(selected) = selected {
                row = row.child(choice(
                    "包含",
                    format!("replace:item:{index}"),
                    selected,
                    busy || state.invalid,
                ));
            } else {
                row = row.child(super::fact("状态", i18n::tr("未知").into_owned()));
            }
            for (key, label) in [
                ("key", "目标"),
                ("identity", "身份"),
                ("role", "角色"),
                ("before", "原值"),
                ("after", "新值"),
                ("matches", "命中数"),
                ("error", "错误"),
                ("collision", "碰撞"),
            ] {
                if let Some(value) = item.get(key).filter(|v| !v.is_null()) {
                    let text = if key == "role" {
                        value
                            .as_str()
                            .and_then(|role| REDIS_ROLES.iter().find(|(name, _)| *name == role))
                            .map(|(_, label)| i18n::tr(label).into_owned())
                            .unwrap_or_else(|| display(value))
                    } else if key == "collision" {
                        value
                            .as_bool()
                            .map(|collision| {
                                i18n::tr(if collision { "是" } else { "否" }).into_owned()
                            })
                            .unwrap_or_else(|| display(value))
                    } else {
                        display(value)
                    };
                    row = row.child(super::fact(label, text));
                    if matches!(key, "before" | "after")
                        && value.get("complete").and_then(Value::as_bool) == Some(false)
                    {
                        row = row.child(button(
                            "查看完整值",
                            format!("replace:detail:{key}:{index}"),
                            busy,
                        ));
                    }
                }
            }
            panel = panel.child(row);
        }
        let offset = state
            .page
            .as_ref()
            .and_then(|p| p.get("offset"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let next = state
            .page
            .as_ref()
            .and_then(|p| p.get("next_offset"))
            .and_then(Value::as_u64)
            .is_some();
        panel = panel
            .child(button("上一页", "replace:prev".into(), busy || offset == 0))
            .child(button("下一页", "replace:next".into(), busy || !next));
    }
    if let Some((_, _, offset, bytes, next)) = &state.detail {
        let text = std::str::from_utf8(bytes)
            .map(str::to_owned)
            .unwrap_or_else(|_| format!("base64:{}", STANDARD.encode(bytes)));
        panel = panel
            .child(html::strong(format!(
                "{} · {offset}",
                i18n::tr("值详情（分块偏移）")
            )))
            .child(html::pre(text).class("preview"));
        panel = panel.child(button(
            "下一块",
            "replace:detail-next".into(),
            busy || next.is_none(),
        ));
    }
    if let Some(result) = &state.result {
        let mut section = html::div()
            .class("advanced-section")
            .child(html::strong(i18n::tr("执行结果").into_owned()));
        if let Some(status) = result
            .get("status")
            .or_else(|| result.get("state"))
            .and_then(Value::as_str)
        {
            section = section.child(super::fact("状态", super::status(status)));
        }
        for (key, label) in [
            ("applied_commands", "已执行命令"),
            ("total_commands", "命令总数"),
            ("affected_rows", "影响行数"),
            ("error", "错误"),
        ] {
            if let Some(value) = result.get(key).filter(|value| !value.is_null()) {
                section = section.child(super::fact(label, display(value)));
            }
        }
        if let Some(items) = result.get("items").and_then(Value::as_array) {
            for (index, item) in items.iter().enumerate() {
                if let Some(status) = item.get("status").and_then(Value::as_str) {
                    section = section.child(super::fact(
                        &format!("{} {}", i18n::tr("项目"), index + 1),
                        super::status(status),
                    ));
                }
                if let Some(error) = item.get("error").filter(|value| !value.is_null()) {
                    section = section.child(super::fact("错误", display(error)));
                }
            }
        }
        panel = panel
            .child(section)
            .child(super::diagnostic("replace-result-diagnostic", result));
    }
    panel = panel.child(button(
        "准备预览",
        "replace:prepare".into(),
        busy || frozen || state.invalid,
    ));
    if frozen {
        let can_submit = state.summary.as_ref().is_some_and(|s| {
            s.get("complete").and_then(Value::as_bool) == Some(true)
                && s.get("submittable").and_then(Value::as_bool) == Some(true)
                && s.get("plan_digest")
                    .or_else(|| s.get("digest"))
                    .and_then(Value::as_str)
                    .is_some()
        });
        panel = panel
            .child(button("丢弃", "replace:discard".into(), busy))
            .child(button(
                "提交",
                "replace:submit".into(),
                busy || state.invalid || !can_submit,
            ));
    }
    panel.into()
}
