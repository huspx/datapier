//! 结构设计的本地意图、表单和异步请求；持久化只包含用户操作。
use crate::{Editor, Msg, i18n::tr};
use datapier_core::model::{
    ResourceTarget,
    schema::{SchemaDefinition, SchemaSnapshot},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use tern_sdk::ui::{self, Node, html};
use uuid::Uuid;

/// 可恢复的操作序列；数组操作按当时的顺序重放，ID 保留在新增对象中。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SchemaOperation {
    Set {
        path: String,
        value: Value,
    },
    Insert {
        path: String,
        value: Value,
    },
    Remove {
        path: String,
        index: usize,
    },
    Move {
        path: String,
        from: usize,
        to: usize,
    },
}

/// 工作区可保存的数据，不包含基线、未编辑定义或预览。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SchemaDraft {
    pub target: ResourceTarget,
    pub creating: bool,
    #[serde(default)]
    pub baseline_fingerprint: Option<String>,
    pub operations: Vec<SchemaOperation>,
    pub algorithm: Option<String>,
    pub lock: Option<String>,
}

/// App 负责按 tag 路由响应并将请求交给现有 Backend。
pub struct SchemaRequest {
    pub tag: String,
    pub method: &'static str,
    pub params: Value,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestKind {
    Describe,
    Plan,
    Preview,
    Submit,
    Discard,
}
#[derive(Clone)]
struct Pending {
    token: Uuid,
    generation: u64,
    kind: RequestKind,
}

/// 结构编辑运行态。切换 tab 时须与 tab 一起保留，关闭前须显式处理草稿和计划。
#[derive(Clone)]
pub struct SchemaEditor {
    pub draft: SchemaDraft,
    pub session: Value,
    pub profile_revision: String,
    pub snapshot: Option<SchemaSnapshot>,
    initial_definition: Option<SchemaDefinition>,
    pub definition: Option<SchemaDefinition>,
    pub summary: Option<Value>,
    pub preview: Option<Value>,
    pub execution: Option<Value>,
    pub preview_complete: bool,
    preview_expected: u64,
    preview_sql: Vec<String>,
    refresh_plan_pending: bool,
    pub query_transferred: bool,
    pub error: Option<String>,
    pub dirty: bool,
    pub restore_confirmation_required: bool,
    pub execution_pending: bool,
    pub outcome_unknown: bool,
    pub confirm_submit: bool,
    pub confirm_discard: bool,
    pub connect_nonce: Option<Uuid>,
    target_name: Editor,
    invalid_numbers: BTreeSet<String>,
    generation: u64,
    pending: Option<Pending>,
    editors: RefCell<BTreeMap<String, Editor>>,
}

impl SchemaEditor {
    /// 新对象的初始定义由调用方按对象种类提供；已有对象先请求 describe。
    pub fn new(
        session: Value,
        target: ResourceTarget,
        profile_revision: String,
        initial: Option<SchemaDefinition>,
    ) -> Self {
        let creating = initial.is_some();
        Self {
            target_name: Editor::new(target.name.clone()),
            draft: SchemaDraft {
                target,
                creating,
                baseline_fingerprint: None,
                operations: Vec::new(),
                algorithm: None,
                lock: None,
            },
            session,
            profile_revision,
            snapshot: None,
            initial_definition: initial.clone(),
            definition: initial,
            summary: None,
            preview: None,
            execution: None,
            preview_complete: false,
            preview_expected: 0,
            preview_sql: Vec::new(),
            query_transferred: false,
            refresh_plan_pending: false,
            error: None,
            dirty: creating,
            restore_confirmation_required: false,
            execution_pending: false,
            outcome_unknown: false,
            confirm_submit: false,
            confirm_discard: false,
            connect_nonce: None,
            invalid_numbers: BTreeSet::new(),
            generation: 0,
            pending: None,
            editors: RefCell::new(BTreeMap::new()),
        }
    }

    /// 恢复草稿后必须重新 describe 并重放操作，不能恢复旧预览或执行凭据。
    pub fn restore(
        session: Value,
        profile_revision: String,
        draft: SchemaDraft,
        initial: Option<SchemaDefinition>,
    ) -> Self {
        let mut state = Self::new(session, draft.target.clone(), profile_revision, initial);
        state.dirty = draft.creating
            || !draft.operations.is_empty()
            || draft.algorithm.is_some()
            || draft.lock.is_some();
        state.restore_confirmation_required = state.dirty;
        state.draft = draft;
        state
    }

    /// 仅在基线指纹未变化时重放草稿；数组索引不能安全地重放到另一份结构。
    pub fn confirm_restore(&mut self) -> Result<(), String> {
        if !self.restore_confirmation_required || self.pending.is_some() {
            return Err(tr("当前不能恢复草稿").into_owned());
        }
        let initial = if self.draft.creating {
            self.initial_definition
                .as_ref()
                .ok_or_else(|| tr("缺少新对象的初始定义").into_owned())?
        } else {
            let snapshot = self
                .snapshot
                .as_ref()
                .ok_or_else(|| tr("请先读取当前结构").into_owned())?;
            if self.draft.baseline_fingerprint.as_deref() != Some(snapshot.fingerprint.as_str()) {
                return Err(tr("结构基线已变化，不能直接恢复草稿").into_owned());
            }
            &snapshot.definition
        };
        let mut definition = serde_json::to_value(initial).map_err(|e| e.to_string())?;
        for operation in &self.draft.operations {
            apply(&mut definition, operation)?;
        }
        self.definition = Some(serde_json::from_value(definition).map_err(|e| e.to_string())?);
        self.restore_confirmation_required = false;
        self.editors.get_mut().clear();
        Ok(())
    }

    fn request(
        &mut self,
        kind: RequestKind,
        method: &'static str,
        params: Value,
    ) -> Option<SchemaRequest> {
        if self.pending.is_some() {
            return None;
        }
        let token = Uuid::new_v4();
        self.pending = Some(Pending {
            token,
            generation: self.generation,
            kind,
        });
        self.error = None;
        Some(SchemaRequest {
            tag: format!("schema/{token}"),
            method,
            params,
        })
    }

    pub fn describe(&mut self, refresh: bool) -> Option<SchemaRequest> {
        if self.session.is_null()
            || self.summary.is_some()
            || self.execution_pending
            || self.draft.creating
            || (self.dirty && !self.restore_confirmation_required)
        {
            return None;
        }
        self.request(
            RequestKind::Describe,
            "schema.describe",
            json!({"session_id":self.session,"target":self.draft.target,"refresh":refresh}),
        )
    }

    pub fn plan(&mut self) -> Option<SchemaRequest> {
        if self.session.is_null()
            || self.summary.is_some()
            || self.execution_pending
            || self.query_transferred
            || !self.invalid_numbers.is_empty()
            || self.restore_confirmation_required
            || self.definition.is_none()
            || (!self.draft.creating && self.snapshot.is_none())
        {
            return None;
        }
        self.request(RequestKind::Plan, "schema.plan", json!({
            "session_id":self.session,
            "intent":{"target":self.draft.target,"baseline":if self.draft.creating { None } else { self.snapshot.as_ref() },"definition":self.definition,"profile_revision":self.profile_revision,"algorithm":self.draft.algorithm,"lock":self.draft.lock,"server_version":self.snapshot.as_ref().map_or("", |s| s.server_version.as_str()),"sql_mode":self.snapshot.as_ref().map_or("", |s| s.sql_mode.as_str())}
        }))
    }

    pub fn preview_page(&mut self, offset: u64) -> Option<SchemaRequest> {
        let summary = self.summary.as_ref()?;
        let id = summary.get("id")?.clone();
        let revision = summary.get("revision")?.clone();
        self.request(
            RequestKind::Preview,
            "schema.preview",
            json!({"plan_id":id,"revision":revision,"offset":offset}),
        )
    }

    pub fn submit(&mut self) -> Option<SchemaRequest> {
        if self.query_transferred {
            return None;
        }
        let summary = self.summary.as_ref()?;
        if summary.get("submittable")?.as_bool() != Some(true)
            || summary.get("complete")?.as_bool() != Some(true)
        {
            return None;
        }
        let id = summary.get("id")?.clone();
        let revision = summary.get("revision")?.clone();
        let digest = summary.get("plan_digest")?.as_str()?.to_owned();
        if digest.is_empty() || self.preview.is_none() || !self.preview_complete {
            return None;
        }
        self.request(
            RequestKind::Submit,
            "schema.submit",
            json!({"plan_id":id,"revision":revision,"digest":digest}),
        )
    }

    pub fn discard(&mut self) -> Option<SchemaRequest> {
        let summary = self.summary.as_ref()?;
        let id = summary.get("id")?.clone();
        let revision = summary.get("revision")?.clone();
        self.request(
            RequestKind::Discard,
            "schema.discard",
            json!({"plan_id":id,"revision":revision}),
        )
    }

    /// 只消费当前请求 token 和代次匹配的响应；错误保留草稿与执行结果。
    pub fn response(&mut self, tag: &str, result: &datapier_core::Result<Value>) -> bool {
        let Some(pending) = self.pending.as_ref() else {
            return false;
        };
        if tag != format!("schema/{}", pending.token) || pending.generation != self.generation {
            return false;
        }
        let kind = pending.kind;
        self.pending = None;
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                if kind == RequestKind::Preview
                    && matches!(
                        error.code.as_str(),
                        "stale_revision" | "handle_expired" | "not_found"
                    )
                {
                    self.summary = None;
                    self.preview = None;
                    self.preview_sql.clear();
                    self.preview_complete = false;
                    self.preview_expected = 0;
                }
                if kind == RequestKind::Submit
                    && matches!(error.code.as_str(), "outcome_unknown" | "disconnected")
                {
                    self.execution_pending = true;
                    self.outcome_unknown = true;
                    self.summary = None;
                    self.preview = None;
                    if error.code == "disconnected" {
                        self.session = Value::Null;
                    }
                }
                self.error = Some(error.to_string());
                return true;
            }
        };
        let outcome: Result<(), String> = (|| {
            match kind {
                RequestKind::Describe => {
                    let snapshot: SchemaSnapshot =
                        serde_json::from_value(value.clone()).map_err(|e| e.to_string())?;
                    if !self.restore_confirmation_required {
                        self.definition = Some(snapshot.definition.clone());
                        self.draft.baseline_fingerprint = Some(snapshot.fingerprint.clone());
                    }
                    self.snapshot = Some(snapshot);
                    self.editors.get_mut().clear();
                }
                RequestKind::Plan | RequestKind::Preview => {
                    let summary = value;
                    if kind == RequestKind::Plan {
                        self.preview_complete = false;
                        self.preview_expected = 0;
                        self.preview_sql.clear();
                        self.preview = None;
                    }
                    if summary.get("id").and_then(Value::as_str).is_none()
                        || summary.get("revision").and_then(Value::as_u64).is_none()
                    {
                        return Err("invalid plan summary".into());
                    }
                    if kind == RequestKind::Preview {
                        let old = self.summary.as_ref().ok_or("missing plan")?;
                        if old.get("id") != summary.get("id")
                            || old.get("revision") != summary.get("revision")
                            || old.get("plan_digest") != summary.get("plan_digest")
                        {
                            return Err("preview plan mismatch".into());
                        }
                        let offset = value
                            .get("offset")
                            .and_then(Value::as_u64)
                            .ok_or("missing preview offset")?;
                        let count = value
                            .get("steps")
                            .and_then(Value::as_array)
                            .ok_or("missing preview steps")?
                            .len() as u64;
                        let total = summary
                            .get("total_items")
                            .and_then(Value::as_u64)
                            .ok_or("missing preview total")?;
                        if offset == 0 {
                            self.preview_expected = 0;
                            self.preview_complete = false;
                            self.preview_sql.clear();
                        }
                        if offset != self.preview_expected
                            || offset.checked_add(count).is_none_or(|end| end > total)
                        {
                            return Err("preview page sequence mismatch".into());
                        }
                        self.preview_expected = offset + count;
                        let next = value.get("next").and_then(Value::as_u64);
                        if next.is_some_and(|next| next != self.preview_expected || next >= total)
                            || (next.is_none() && self.preview_expected != total)
                        {
                            return Err("invalid preview continuation".into());
                        }
                        for step in value["steps"].as_array().into_iter().flatten() {
                            let sql = step
                                .get("sql")
                                .and_then(Value::as_str)
                                .ok_or("missing preview SQL")?;
                            self.preview_sql.push(sql.to_owned());
                        }
                        self.preview_complete = next.is_none();
                        self.preview = Some(value.clone());
                    }
                    self.summary = Some(
                        json!({"id":summary["id"],"revision":summary["revision"],"state":summary["state"],"complete":summary["complete"],"total_items":summary["total_items"],"plan_digest":summary["plan_digest"],"submittable":summary["submittable"]}),
                    );
                }
                RequestKind::Submit => {
                    self.execution = Some(value.clone());
                    self.execution_pending = true;
                    self.summary = None;
                    self.preview = None;
                    self.preview_complete = false;
                    self.preview_expected = 0;
                }
                RequestKind::Discard => {
                    self.summary = None;
                    self.preview = None;
                    self.preview_complete = false;
                    self.preview_expected = 0;
                }
            }
            Ok(())
        })();
        if let Err(error) = outcome {
            self.error = Some(error);
        }
        true
    }

    /// 只有顺序读取完全部预览页才能将冻结计划转到自由 SQL。
    pub fn query_sql(&self) -> Option<String> {
        if !self.preview_complete || self.preview_sql.is_empty() {
            return None;
        }
        let mut result = String::new();
        for sql in &self.preview_sql {
            if !result.is_empty() {
                result.push_str("\n\n");
            }
            result.push_str(sql);
            if !sql.trim_end().ends_with(';') {
                result.push(';');
            }
        }
        Some(result)
    }
    pub(crate) fn has_pending_request(&self) -> bool {
        self.pending.is_some() || self.execution_pending
    }

    /// 结构变化后保留用户意图，但不能继续使用旧快照和计划。
    pub fn invalidate_schema(&mut self) {
        if !self
            .pending
            .as_ref()
            .is_some_and(|pending| pending.kind == RequestKind::Submit)
        {
            self.generation = self.generation.wrapping_add(1);
            self.pending = None;
        }
        self.refresh_plan_pending = false;
        self.snapshot = None;
        self.summary = None;
        self.preview = None;
        self.preview_sql.clear();
        self.preview_complete = false;
        self.preview_expected = 0;
        self.confirm_submit = false;
        if self.dirty {
            self.restore_confirmation_required = true;
        } else if !self.draft.creating {
            self.definition = None;
        }
        self.error = Some(tr("结构已变化，请重新读取并确认草稿").into_owned());
    }

    pub fn refresh_plan(&mut self, id: &str) -> Option<SchemaRequest> {
        if self
            .summary
            .as_ref()
            .and_then(|summary| summary["id"].as_str())
            != Some(id)
        {
            return None;
        }
        if self.pending.is_some() {
            self.refresh_plan_pending = true;
            return None;
        }
        self.preview_page(0)
    }

    pub fn take_plan_refresh(&mut self) -> Option<SchemaRequest> {
        if !std::mem::take(&mut self.refresh_plan_pending) {
            return None;
        }
        self.preview_page(0)
    }

    /// App 仅可用匹配的 execution 终态解锁；成功后须调用 describe 读回。
    pub fn execution_finished(&mut self, execution_id: &str, success: bool) -> bool {
        if !self.execution_pending
            || self
                .execution
                .as_ref()
                .and_then(|v| v.get("execution_id"))
                .and_then(Value::as_str)
                != Some(execution_id)
        {
            return false;
        }
        self.execution_pending = false;
        self.snapshot = None;
        if success {
            self.draft.operations.clear();
            self.draft.creating = false;
            self.draft.baseline_fingerprint = None;
            self.draft.algorithm = None;
            self.draft.lock = None;
            self.initial_definition = None;
            self.dirty = false;
            self.definition = None;
        } else {
            self.restore_confirmation_required = true;
        }
        self.invalid_numbers.clear();
        self.editors.get_mut().clear();
        true
    }

    /// 用户人工核对结果后解除等待；草稿仍须重新读取并确认，绝不自动重放。
    pub fn acknowledge_unknown(&mut self) -> bool {
        if !self.execution_pending || !self.outcome_unknown {
            return false;
        }
        self.execution_pending = false;
        self.outcome_unknown = false;
        self.snapshot = None;
        self.restore_confirmation_required = true;
        self.error = None;
        true
    }

    /// 请求或执行进行中时，App 不应关闭标签或迁移会话。
    pub fn busy(&self) -> bool {
        self.pending.is_some() || self.connect_nonce.is_some() || self.execution_pending
    }

    /// 应用一个用户操作，并在修改前要求先释放已有计划。
    pub fn apply_operation(&mut self, operation: SchemaOperation) -> Result<(), String> {
        if self.pending.is_some()
            || self.query_transferred
            || self.execution_pending
            || self.summary.is_some()
            || self.restore_confirmation_required
        {
            return Err(tr("请先丢弃计划或等待请求完成").into_owned());
        }
        let mut definition = serde_json::to_value(
            self.definition
                .as_ref()
                .ok_or_else(|| tr("尚未读取结构").into_owned())?,
        )
        .map_err(|e| e.to_string())?;
        apply(&mut definition, &operation)?;
        self.definition = Some(serde_json::from_value(definition).map_err(|e| e.to_string())?);
        if let (
            Some(SchemaOperation::Set {
                path: old_path,
                value: old_value,
            }),
            SchemaOperation::Set { path, value },
        ) = (self.draft.operations.last_mut(), &operation)
        {
            if old_path == path {
                *old_value = value.clone();
            } else {
                self.draft.operations.push(operation);
            }
        } else {
            self.draft.operations.push(operation);
        }
        self.dirty = true;
        self.generation = self.generation.wrapping_add(1);
        self.preview = None;
        Ok(())
    }

    /// App 将 `Msg::Edit` 中以 schema: 开头的字段交给此方法。
    pub fn edit(
        &mut self,
        name: &str,
        from: usize,
        to: usize,
        text: &str,
        cursor: usize,
        len: usize,
    ) -> bool {
        if name == "schema-target" {
            if self.pending.is_some()
                || self.query_transferred
                || self.execution_pending
                || self.summary.is_some()
                || self.restore_confirmation_required
                || !self.draft.creating
            {
                return true;
            }
            if self.target_name.text.encode_utf16().count() != len
                || !self.target_name.edit(from..to, text)
            {
                return true;
            }
            self.target_name.set_selection(cursor, cursor);
            self.changed_target_name();
            return true;
        }
        let Some(path) = name.strip_prefix("schema:") else {
            return false;
        };
        if self.pending.is_some()
            || self.query_transferred
            || self.execution_pending
            || self.summary.is_some()
            || self.restore_confirmation_required
        {
            return true;
        }
        let Some(editor) = self.editors.get_mut().get_mut(path) else {
            return true;
        };
        if editor.text.encode_utf16().count() != len || !editor.edit(from..to, text) {
            return true;
        }
        let _ = editor.set_selection(cursor, cursor);
        let value = editor.text.clone();
        self.changed_text(path, value);
        true
    }

    fn changed_target_name(&mut self) {
        self.draft.target.name = self.target_name.text.clone();
        if let Some(SchemaDefinition::Trigger(trigger)) = self.definition.as_mut() {
            trigger.name = self.draft.target.name.clone();
        }
        if let Some(SchemaDefinition::Trigger(trigger)) = self.initial_definition.as_mut() {
            trigger.name = self.draft.target.name.clone();
        }
        self.dirty = true;
        self.generation = self.generation.wrapping_add(1);
    }

    fn changed_text(&mut self, path: &str, value: String) {
        if path == "/@algorithm" || path == "/@lock" {
            let setting = if value.is_empty() { None } else { Some(value) };
            if path == "/@algorithm" {
                self.draft.algorithm = setting;
            } else {
                self.draft.lock = setting;
            }
            self.dirty = true;
            self.generation = self.generation.wrapping_add(1);
        } else {
            let value = if path.ends_with("/prefix_length") {
                match value.parse::<u32>() {
                    Ok(number) if number > 0 => {
                        self.invalid_numbers.remove(path);
                        json!(number)
                    }
                    _ => {
                        self.invalid_numbers.insert(path.to_owned());
                        self.error = Some(tr("前缀长度必须为正整数").into_owned());
                        return;
                    }
                }
            } else {
                Value::String(value)
            };
            if let Err(error) = self.apply_operation(SchemaOperation::Set {
                path: path.into(),
                value,
            }) {
                self.error = Some(error);
            }
        }
    }

    /// 将原生撤销应用到相同的类型化意图和工作区草稿。
    pub fn undo(&mut self, name: &str) -> bool {
        if name != "schema-target" && !name.starts_with("schema:") {
            return false;
        }
        if self.query_transferred {
            return true;
        }
        if self.busy() || self.summary.is_some() || self.restore_confirmation_required {
            return true;
        }
        if name == "schema-target" {
            if self.draft.creating && self.target_name.undo() {
                self.changed_target_name();
            }
        } else if let Some(path) = name.strip_prefix("schema:")
            && let Some(editor) = self.editors.get_mut().get_mut(path)
            && editor.undo()
        {
            let value = editor.text.clone();
            self.changed_text(path, value);
        }
        true
    }

    /// 处理表单动作；请求动作由 App 调用对应方法后派发。
    pub fn action(&mut self, action: &str) -> bool {
        let Some(rest) = action.strip_prefix("schema-op:") else {
            return false;
        };
        let mut parts = rest.splitn(3, ':');
        let command = parts.next().unwrap_or("");
        let path = parts.next().unwrap_or("");
        let arg = parts.next().unwrap_or("");
        let operation = match command {
            "toggle" => self
                .definition
                .as_ref()
                .and_then(|d| serde_json::to_value(d).ok())
                .and_then(|v| v.pointer(path).and_then(Value::as_bool))
                .map(|v| SchemaOperation::Set {
                    path: path.into(),
                    value: Value::Bool(!v),
                }),
            "set" => serde_json::from_str(arg)
                .ok()
                .map(|value| SchemaOperation::Set {
                    path: path.into(),
                    value,
                }),
            "add" => template(path, arg).map(|value| SchemaOperation::Insert {
                path: path.into(),
                value,
            }),
            "remove" => arg.parse().ok().map(|index| SchemaOperation::Remove {
                path: path.into(),
                index,
            }),
            "up" | "down" => arg.parse::<usize>().ok().and_then(|from| {
                if command == "up" {
                    from.checked_sub(1)
                } else {
                    from.checked_add(1)
                }
                .map(|to| SchemaOperation::Move {
                    path: path.into(),
                    from,
                    to,
                })
            }),
            _ => None,
        };
        if let Some(operation) = operation {
            match self.apply_operation(operation) {
                Ok(()) => {
                    self.editors.get_mut().clear();
                    self.invalid_numbers.clear();
                }
                Err(error) => self.error = Some(error),
            }
        }
        true
    }

    /// 渲染公开 SDK 控件；App 将 schema: 编辑事件和 schema-op: 动作路由回本状态。
    pub fn view(&self) -> Node<Msg> {
        let mut panel = html::div().class("schema-panel");
        if self.session.is_null() && self.connect_nonce.is_none() {
            panel = panel.child(
                ui::button(tr("重试连接").into_owned())
                    .on_click(Msg::Action("schema-connect".into())),
            );
        }
        if self.outcome_unknown {
            panel = panel.child(
                ui::button(tr("已核对结果，解除等待").into_owned())
                    .on_click(Msg::Action("schema-unknown-acknowledge".into())),
            );
        }
        if self.draft.creating {
            let disabled = self.busy()
                || self.summary.is_some()
                || self.restore_confirmation_required
                || self.query_transferred;
            panel = panel.child(
                ui::field(tr("对象名称").into_owned()).child(
                    ui::input()
                        .key("schema-target")
                        .aria(tr("对象名称").into_owned())
                        .variant(ui::InputVariant::Field)
                        .readonly(disabled)
                        .text(self.target_name.text.clone())
                        .cursor(self.target_name.cursor)
                        .anchor(self.target_name.anchor)
                        .on_focus(Msg::Focus("schema-target".into()))
                        .on_undo(Msg::Undo("schema-target".into()))
                        .on_edit(|e| {
                            Msg::Edit(
                                "schema-target".into(),
                                e.from,
                                e.to,
                                e.text.clone(),
                                e.cursor,
                                e.len,
                            )
                        }),
                ),
            );
        }
        if let Some(snapshot) = &self.snapshot {
            panel = panel.child(html::span(format!(
                "{}: {:?}",
                tr("完整性"),
                snapshot.completeness.state
            )));
            for reason in &snapshot.completeness.reasons {
                panel = panel.child(html::span(reason.clone()));
            }
            for (name, capability) in &snapshot.capabilities {
                panel = panel.child(html::span(format!(
                    "{}: {} {}",
                    name,
                    capability.state,
                    capability.reason.as_deref().unwrap_or("")
                )));
            }
        }
        if self.restore_confirmation_required {
            panel = panel.child(html::span(tr("请读取当前结构并确认恢复草稿").into_owned()));
            panel = panel.child(
                ui::button(tr("确认恢复").into_owned())
                    .on_click(Msg::Action("schema-restore-confirm".into())),
            );
        }
        if let Some(error) = &self.error {
            panel = panel.child(html::span(error.clone()).class("field-error"));
        }
        if let Some(definition) = &self.definition
            && let Ok(value) = serde_json::to_value(definition)
        {
            let table = value.get("kind").and_then(Value::as_str) == Some("table");
            let disabled = self.pending.is_some()
                || self.execution_pending
                || self.summary.is_some()
                || self.query_transferred
                || self.restore_confirmation_required;
            panel = panel.child(render_value(
                &value,
                "",
                &mut self.editors.borrow_mut(),
                disabled,
                table,
            ));
            for (label, path, text) in [
                (
                    "ALGORITHM",
                    "/@algorithm",
                    self.draft.algorithm.as_deref().unwrap_or(""),
                ),
                ("LOCK", "/@lock", self.draft.lock.as_deref().unwrap_or("")),
            ] {
                panel = panel
                    .child(html::strong(tr(label).into_owned()))
                    .child(render_value(
                        &Value::String(text.to_owned()),
                        path,
                        &mut self.editors.borrow_mut(),
                        disabled,
                        table,
                    ));
            }
        }
        let busy = self.busy();
        let has_plan = self.summary.is_some();
        if self.query_transferred {
            panel = panel
                .child(html::span(
                    tr("结构草稿已冻结，可返回继续编辑").into_owned(),
                ))
                .child(
                    ui::button(tr("恢复结构草稿编辑").into_owned())
                        .on_click(Msg::Action("schema-query-resume".into())),
                );
        }
        let can_submit = self.preview_complete
            && !self.query_transferred
            && self.summary.as_ref().is_some_and(|s| {
                s["complete"] == true
                    && s["submittable"] == true
                    && s["plan_digest"].as_str().is_some()
            });
        for (label, action, enabled) in [
            (
                "读取结构",
                "schema-describe",
                !self.draft.creating
                    && !has_plan
                    && (!self.dirty || self.restore_confirmation_required),
            ),
            (
                "生成计划",
                "schema-plan",
                !self.session.is_null()
                    && !has_plan
                    && self.definition.is_some()
                    && !self.restore_confirmation_required
                    && !self.query_transferred
                    && self.invalid_numbers.is_empty(),
            ),
            ("预览", "schema-preview", has_plan),
            ("提交", "schema-submit", can_submit),
            ("丢弃计划", "schema-discard", has_plan),
        ] {
            panel = panel.child(
                ui::button(tr(label).into_owned())
                    .disabled(busy || !enabled)
                    .on_click(Msg::Action(action.into())),
            );
        }
        if self.dirty || self.restore_confirmation_required {
            panel = panel.child(
                ui::button(tr("放弃结构草稿").into_owned())
                    .disabled(busy || has_plan)
                    .on_click(Msg::Action("schema-draft-discard".into())),
            );
        }
        if self.confirm_discard {
            panel = panel
                .child(html::p(tr("确认放弃结构草稿？").into_owned()))
                .child(
                    ui::button(tr("确认放弃").into_owned())
                        .disabled(busy || has_plan)
                        .on_click(Msg::Action("schema-draft-discard-confirm".into())),
                )
                .child(
                    ui::button(tr("取消").into_owned())
                        .on_click(Msg::Action("schema-draft-discard-cancel".into())),
                );
        }
        if self.confirm_submit {
            panel = panel
                .child(html::p(tr("确认执行预览中的结构变更？").into_owned()))
                .child(
                    ui::button(tr("确认执行").into_owned())
                        .disabled(busy || !can_submit)
                        .on_click(Msg::Action("schema-submit-confirm".into())),
                )
                .child(
                    ui::button(tr("取消").into_owned())
                        .on_click(Msg::Action("schema-submit-cancel".into())),
                );
        }
        if self.query_sql().is_some() {
            panel = panel.child(
                ui::button(tr("转到查询编辑器").into_owned())
                    .on_click(Msg::Action("schema-query".into())),
            );
        }
        if self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.ddl.is_empty())
        {
            panel = panel.child(
                ui::button(tr("将原定义转到查询编辑器").into_owned())
                    .on_click(Msg::Action("schema-query-original".into())),
            );
        }
        if let Some(preview) = &self.preview {
            panel = panel.child(html::strong(tr("变更预览").into_owned()));
            panel = panel.child(html::pre(
                serde_json::to_string_pretty(preview).unwrap_or_default(),
            ));
            if preview.get("next").is_some_and(|v| !v.is_null()) {
                panel = panel.child(
                    ui::button(tr("下一页").into_owned())
                        .on_click(Msg::Action("schema-preview-next".into())),
                );
            }
        }
        if let Some(execution) = &self.execution {
            panel = panel.child(html::pre(
                serde_json::to_string_pretty(execution).unwrap_or_default(),
            ));
        }
        panel.into()
    }
}

fn apply(root: &mut Value, op: &SchemaOperation) -> Result<(), String> {
    match op {
        SchemaOperation::Set { path, value } => {
            *root
                .pointer_mut(path)
                .ok_or("schema path no longer exists")? = value.clone();
        }
        SchemaOperation::Insert { path, value } => root
            .pointer_mut(path)
            .and_then(Value::as_array_mut)
            .ok_or("schema list no longer exists")?
            .push(value.clone()),
        SchemaOperation::Remove { path, index } => {
            let list = root
                .pointer_mut(path)
                .and_then(Value::as_array_mut)
                .ok_or("schema list no longer exists")?;
            if *index >= list.len() {
                return Err("schema item no longer exists".into());
            }
            list.remove(*index);
        }
        SchemaOperation::Move { path, from, to } => {
            let list = root
                .pointer_mut(path)
                .and_then(Value::as_array_mut)
                .ok_or("schema list no longer exists")?;
            if *from >= list.len() || *to >= list.len() {
                return Err("schema item no longer exists".into());
            }
            let item = list.remove(*from);
            list.insert(*to, item);
        }
    }
    Ok(())
}

fn template(path: &str, kind: &str) -> Option<Value> {
    let id = Uuid::new_v4().to_string();
    match path.rsplit('/').next()? {
        "columns" if path == "/columns" && kind == "column" => Some(
            json!({"id":id,"name":"","native_type":"","nullable":true,"default":{"kind":"unspecified"},"on_update":null,"auto_increment":false,"generated":null,"charset":null,"collation":null,"visible":true,"comment":"","extra":[]}),
        ),
        "indexes" => Some(
            json!({"id":id,"name":"","kind":"normal","parts":[],"visible":true,"method":null,"options":[]}),
        ),
        "constraints" if kind == "foreign_key" => Some(
            json!({"kind":"foreign_key","id":id,"name":"","columns":[],"referenced_database":"","referenced_table":"","referenced_columns":[],"on_update":"RESTRICT","on_delete":"RESTRICT"}),
        ),
        "constraints" if kind == "check" => {
            Some(json!({"kind":"check","id":id,"name":"","expression":"","enforced":true}))
        }
        "parts" => {
            Some(json!({"column":"","expression":null,"prefix_length":null,"descending":false}))
        }
        "columns" | "referenced_columns" | "options" | "extra" => Some(json!("")),
        _ => None,
    }
}

fn field_label(key: &str) -> &str {
    match key {
        "columns" => "字段",
        "indexes" => "索引",
        "constraints" => "约束",
        "name" => "名称",
        "native_type" => "类型",
        "nullable" => "允许 NULL",
        "default" => "默认值",
        "value" => "值",
        "on_update" => "更新时",
        "auto_increment" => "自增",
        "generated" => "生成列",
        "expression" => "表达式",
        "stored" => "存储生成列",
        "charset" => "字符集",
        "collation" => "排序规则",
        "visible" => "可见",
        "comment" => "注释",
        "extra" => "其他属性",
        "kind" => "种类",
        "parts" => "索引列",
        "column" => "字段",
        "prefix_length" => "前缀长度",
        "descending" => "降序",
        "method" => "索引方法",
        "options" => "选项",
        "referenced_database" => "引用数据库",
        "referenced_table" => "引用表",
        "referenced_columns" => "引用字段",
        "on_delete" => "删除时",
        "enforced" => "启用约束",
        "engine" => "存储引擎",
        "select" => "SELECT 正文",
        "algorithm" => "ALGORITHM",
        "definer" => "DEFINER",
        "security" => "SQL SECURITY",
        "check_option" => "CHECK OPTION",
        "character_set_client" => "创建字符集",
        "collation_connection" => "创建排序规则",
        "database" => "数据库",
        "table" => "表",
        "timing" => "时机",
        "event" => "事件",
        "body" => "SQL 正文",
        "sql_mode" => "SQL MODE",
        "order" => "触发顺序",
        "follows" => "跟随",
        "trigger" => "参照触发器",
        _ => key,
    }
}

fn render_value(
    value: &Value,
    path: &str,
    editors: &mut BTreeMap<String, Editor>,
    disabled: bool,
    table: bool,
) -> Node<Msg> {
    let mut container = html::div();
    match value {
        Value::Object(map) => {
            for (key, value) in map {
                if key == "id" {
                    continue;
                }
                let child = format!("{path}/{}", key.replace('~', "~0").replace('/', "~1"));
                let locked_identity = path.is_empty()
                    && map.get("kind").and_then(Value::as_str) == Some("trigger")
                    && matches!(key.as_str(), "name" | "database");
                container = container
                    .child(html::strong(tr(field_label(key)).into_owned()))
                    .child(render_value(
                        value,
                        &child,
                        editors,
                        disabled || locked_identity,
                        table,
                    ));
                if !value.is_null()
                    && (matches!(
                        key.as_str(),
                        "generated"
                            | "charset"
                            | "collation"
                            | "method"
                            | "prefix_length"
                            | "definer"
                            | "check_option"
                            | "character_set_client"
                            | "collation_connection"
                            | "order"
                    ) || (key == "on_update" && path.starts_with("/columns/"))
                        || (matches!(key.as_str(), "column" | "expression")
                            && path.contains("/parts/")))
                {
                    container = container.child(
                        ui::button(tr("清除").into_owned())
                            .disabled(disabled)
                            .on_click(Msg::Action(format!("schema-op:set:{child}:null"))),
                    );
                }
            }
        }
        Value::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                let child = format!("{path}/{index}");
                let row_key = item
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| child.clone());
                let mut row = html::div()
                    .key(row_key)
                    .child(render_value(item, &child, editors, disabled, table));
                for (label, action, enabled) in [
                    ("删除", "remove", true),
                    ("前移", "up", index > 0),
                    ("后移", "down", index + 1 < items.len()),
                ] {
                    row = row.child(
                        ui::button(tr(label).into_owned())
                            .disabled(disabled || !enabled)
                            .on_click(Msg::Action(format!("schema-op:{action}:{path}:{index}"))),
                    );
                }
                container = container.child(row);
            }
            let kinds: &[(&str, &str)] = if path.ends_with("/constraints") {
                &[("外键", "foreign_key"), ("CHECK", "check")]
            } else {
                &[(
                    "新增",
                    if path == "/columns" && table {
                        "column"
                    } else {
                        "item"
                    },
                )]
            };
            for (label, kind) in kinds {
                if template(path, kind).is_some() {
                    container = container.child(
                        ui::button(tr(label).into_owned())
                            .disabled(disabled)
                            .on_click(Msg::Action(format!("schema-op:add:{path}:{kind}"))),
                    );
                }
            }
        }
        Value::Bool(on) => {
            container = container.child(
                ui::switch(tr("启用").into_owned())
                    .on(*on)
                    .disabled(disabled)
                    .on_change({
                        let path = path.to_owned();
                        move |_| Msg::Action(format!("schema-op:toggle:{path}"))
                    }),
            );
        }
        Value::String(text) if path.ends_with("/kind") => {
            container = container.child(html::span(text.clone()));
            let choices: &[&str] = if path.ends_with("/default/kind") {
                &["unspecified", "null", "literal", "expression"]
            } else if path.contains("/indexes/") {
                &["primary", "unique", "normal", "fulltext", "spatial"]
            } else {
                &[]
            };
            for choice in choices {
                let (target, replacement) = if path.ends_with("/default/kind") {
                    (
                        path.trim_end_matches("/kind"),
                        if *choice == "literal" || *choice == "expression" {
                            json!({"kind":choice,"value":""})
                        } else {
                            json!({"kind":choice})
                        },
                    )
                } else {
                    (path, json!(choice))
                };
                container = container.child(
                    ui::button(tr(choice).into_owned())
                        .disabled(disabled)
                        .on_click(Msg::Action(format!("schema-op:set:{target}:{replacement}"))),
                );
            }
        }
        Value::String(text) => {
            let editor = editors
                .entry(path.to_owned())
                .or_insert_with(|| Editor::new(text));
            let name = format!("schema:{path}");
            let edit_name = name.clone();
            let undo_name = name.clone();
            let input = ui::editor()
                .key(name.clone())
                .aria(tr("编辑").into_owned())
                .readonly(disabled)
                .text(editor.text.clone())
                .cursor(editor.cursor)
                .anchor(editor.anchor)
                .max_lines(16u32)
                .on_focus(Msg::Focus(name))
                .on_undo(Msg::Undo(undo_name))
                .on_edit(move |e| {
                    Msg::Edit(
                        edit_name.clone(),
                        e.from,
                        e.to,
                        e.text.clone(),
                        e.cursor,
                        e.len,
                    )
                });
            container = container.child(input);
        }
        Value::Null => {
            let replacement = if path.ends_with("/generated") {
                json!({"expression":"","stored":false})
            } else if path.ends_with("/order") {
                json!({"follows":true,"trigger":""})
            } else if path.ends_with("/prefix_length") {
                json!(1)
            } else {
                json!("")
            };
            container = container.child(
                ui::button(tr("设置").into_owned())
                    .disabled(disabled)
                    .on_click(Msg::Action(format!("schema-op:set:{path}:{replacement}"))),
            );
        }
        Value::Number(number) => {
            return render_value(
                &Value::String(number.to_string()),
                path,
                editors,
                disabled,
                table,
            );
        }
    }
    container.into()
}

#[cfg(test)]
#[path = "../../tests/unit/advanced_schema.rs"]
mod tests;
