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

#[path = "schema_primary.rs"]
mod schema_primary;
#[path = "schema_reference.rs"]
pub(crate) mod schema_reference;

const COLUMN_TYPE_CHOICES: &[(&str, &str)] = &[
    ("BIGINT", "BIGINT"),
    ("INT", "INT"),
    ("DECIMAL", "DECIMAL(12,2)"),
    ("NUMERIC", "NUMERIC(12,2)"),
    ("VARCHAR", "VARCHAR(255)"),
    ("CHAR", "CHAR(1)"),
    ("VARBINARY", "VARBINARY(255)"),
    ("BINARY", "BINARY(1)"),
    ("BIT", "BIT(1)"),
    ("TEXT", "TEXT"),
    ("DATETIME", "DATETIME"),
    ("TIMESTAMP", "TIMESTAMP"),
    ("TIME", "TIME"),
    ("DATE", "DATE"),
    ("BOOLEAN", "BOOLEAN"),
];

/// 保留原始类型名、参数及未修改的后缀。
struct TypeParameters<'a> {
    name: &'a str,
    suffix: &'a str,
    first: &'a str,
    second: &'a str,
    decimal: bool,
    fractional_seconds: bool,
    min: u32,
    max: u32,
}

fn type_parameters(text: &str) -> Option<TypeParameters<'_>> {
    let start = text.len() - text.trim_start().len();
    let end = text[start..]
        .find(|c: char| !c.is_ascii_alphabetic())
        .map_or(text.len(), |n| n + start);
    let kind = text[start..end].to_ascii_uppercase();
    let fractional_seconds = matches!(kind.as_str(), "DATETIME" | "TIMESTAMP" | "TIME");
    let (decimal, min, max, default) = match kind.as_str() {
        "DECIMAL" | "NUMERIC" => (true, 1, 65, "10"),
        "VARCHAR" | "VARBINARY" => (false, 0, 65535, ""),
        "CHAR" | "BINARY" => (false, 0, 255, "1"),
        "BIT" => (false, 1, 64, "1"),
        "DATETIME" | "TIMESTAMP" | "TIME" => (false, 0, 6, "0"),
        _ => return None,
    };
    let rest = &text[end..];
    let trimmed = rest.trim_start();
    let (first, second, suffix) = if let Some(params) = trimmed.strip_prefix('(') {
        let close = params.find(')')?;
        let values = &params[..close];
        let (first, second) = values.split_once(',').unwrap_or((values, "0"));
        if first.trim().is_empty()
            || !first.trim().bytes().all(|c| c.is_ascii_digit())
            || (decimal
                && (second.trim().is_empty() || !second.trim().bytes().all(|c| c.is_ascii_digit())))
            || (!decimal && values.contains(','))
        {
            return None;
        }
        (first.trim(), second.trim(), &params[close + 1..])
    } else {
        (default, "0", rest)
    };
    Some(TypeParameters {
        name: &text[..end],
        suffix,
        first,
        second,
        decimal,
        fractional_seconds,
        min,
        max,
    })
}

fn type_parameter_error(
    params: &TypeParameters<'_>,
    first: &str,
    second: &str,
) -> Option<&'static str> {
    let first = first
        .parse::<u32>()
        .ok()
        .filter(|n| *n >= params.min && *n <= params.max);
    if first.is_none() {
        return Some(if params.decimal {
            "精度必须为 1 到 65 的整数"
        } else if params.fractional_seconds {
            "小数秒精度必须为 0 到 6 的整数"
        } else {
            "长度超出该类型允许的范围"
        });
    }
    if params.decimal
        && !second
            .parse::<u32>()
            .ok()
            .is_some_and(|n| n <= 30 && n <= first.unwrap_or(0))
    {
        return Some("小数位必须为 0 到 30 且不大于精度");
    }
    None
}

fn type_parameter_text(params: &TypeParameters<'_>) -> String {
    if params.decimal {
        format!("{},{}", params.first, params.second)
    } else {
        params.first.to_owned()
    }
}

fn parse_parameter_text<'a>(
    params: &TypeParameters<'_>,
    text: &'a str,
) -> Result<(&'a str, &'a str), &'static str> {
    let (first, second) = if params.decimal {
        text.split_once(',').unwrap_or((text, "0"))
    } else {
        (text, "0")
    };
    let (first, second) = (first.trim(), second.trim());
    if first.is_empty()
        || !first.bytes().all(|c| c.is_ascii_digit())
        || second.is_empty()
        || !second.bytes().all(|c| c.is_ascii_digit())
    {
        return Err("类型长度格式无效");
    }
    if let Some(error) = type_parameter_error(params, first, second) {
        return Err(error);
    }
    Ok((first, second))
}

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
    pub embedded: bool,
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
    pub embedded: bool,
    pub section: String,
    pub(crate) references: schema_reference::ReferenceState,
    selected_column: Option<String>,
    selected_items: BTreeMap<String, String>,
    open_select: Option<String>,
    destructive_reasons: Vec<String>,
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
                embedded: false,
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
            embedded: false,
            section: "columns".into(),
            selected_column: None,
            selected_items: BTreeMap::new(),
            open_select: None,
            destructive_reasons: Vec::new(),
            connect_nonce: None,
            invalid_numbers: BTreeSet::new(),
            generation: 0,
            pending: None,
            references: schema_reference::ReferenceState::default(),
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
        state.embedded = draft.embedded;
        state.dirty = draft.creating
            || !draft.operations.is_empty()
            || draft.algorithm.is_some()
            || draft.lock.is_some();
        state.restore_confirmation_required = state.dirty;
        state.draft = draft;
        state
    }

    /// 已有表未产生编辑意图时不保存独立结构草稿。
    pub(crate) fn workspace_draft(&self) -> Option<SchemaDraft> {
        if self.embedded && !self.dirty && !self.restore_confirmation_required {
            return None;
        }
        let mut draft = self.draft.clone();
        draft.embedded = self.embedded;
        Some(draft)
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
                        self.destructive_reasons.clear();
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
                            self.destructive_reasons.clear();
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
                            if let Some(reasons) =
                                step.get("destructive_reasons").and_then(Value::as_array)
                            {
                                self.destructive_reasons.extend(
                                    reasons.iter().filter_map(Value::as_str).map(str::to_owned),
                                );
                            }
                        }
                        self.preview_complete = next.is_none();
                        let mut accumulated = value.clone();
                        if offset > 0 {
                            let mut steps = self
                                .preview
                                .as_ref()
                                .and_then(|preview| preview.get("steps"))
                                .and_then(Value::as_array)
                                .cloned()
                                .unwrap_or_default();
                            steps.extend(value["steps"].as_array().into_iter().flatten().cloned());
                            accumulated["steps"] = Value::Array(steps);
                        }
                        self.preview = Some(accumulated);
                    }
                    self.summary = Some(
                        json!({"id":summary["id"],"revision":summary["revision"],"state":summary["state"],"complete":summary["complete"],"total_items":summary["total_items"],"plan_digest":summary["plan_digest"],"submittable":summary["submittable"]}),
                    );
                    if kind == RequestKind::Plan
                        && matches!(self.definition.as_ref(), Some(SchemaDefinition::Table(_)))
                    {
                        self.section = "ddl_preview".to_owned();
                    }
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

    pub(crate) fn is_plan_response(&self, tag: &str) -> bool {
        self.pending.as_ref().is_some_and(|pending| {
            pending.kind == RequestKind::Plan
                && pending.generation == self.generation
                && tag == format!("schema/{}", pending.token)
        })
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
            self.restore_confirmation_required = false;
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

    /// 宿主未执行 native editing 时，仍通过同一份 UTF-16 编辑器维护结构草稿。
    pub(crate) fn key(&mut self, name: &str, key: crate::editor::Key<'_>, shift: bool) -> bool {
        let target = name == "schema-target";
        let path = if target {
            ""
        } else if let Some(path) = name.strip_prefix("schema:") {
            path
        } else {
            return false;
        };
        if self.pending.is_some()
            || self.execution_pending
            || self.summary.is_some()
            || self.query_transferred
            || self.restore_confirmation_required
            || (target && !self.draft.creating)
        {
            return true;
        }
        let editor = if target {
            &mut self.target_name
        } else if let Some(editor) = self.editors.get_mut().get_mut(path) {
            editor
        } else {
            return true;
        };
        let old_len = editor.text.len();
        let text_edit = matches!(key, crate::editor::Key::Text(text) if !text.is_empty() || editor.cursor != editor.anchor)
            || matches!(key, crate::editor::Key::Undo);
        if editor.key(key, shift) && (text_edit || editor.text.len() != old_len) {
            if target {
                self.changed_target_name();
            } else {
                let value = editor.text.clone();
                self.changed_text(path, value);
            }
        }
        true
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
        if let Some(type_path) = path.strip_suffix("/@length") {
            self.changed_type_parameter(type_path, &value);
            return;
        }
        if let Some(index) = path
            .strip_prefix("/columns/")
            .and_then(|s| s.strip_suffix("/name"))
            .and_then(|s| s.parse::<usize>().ok())
        {
            if let Some(root) = self
                .definition
                .as_ref()
                .and_then(|d| serde_json::to_value(d).ok())
            {
                match schema_primary::rename_column(
                    &root,
                    index,
                    &value,
                    self.draft.target.database.as_deref(),
                    &self.draft.target.name,
                ) {
                    Ok(operations) => {
                        self.invalid_numbers.remove(path);
                        self.error = None;
                        for operation in operations {
                            if let SchemaOperation::Set { path: changed, .. } = &operation
                                && changed != path
                            {
                                self.editors.get_mut().remove(changed);
                            }
                            if let Err(error) = self.apply_operation(operation) {
                                self.error = Some(error);
                                break;
                            }
                        }
                    }
                    Err(error) => {
                        self.invalid_numbers.insert(path.to_owned());
                        self.error = Some(tr(&error).into_owned());
                        self.dirty = true;
                        self.generation = self.generation.wrapping_add(1);
                    }
                }
            }
            return;
        }
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

    fn column_type(&self, path: &str) -> Option<String> {
        self.definition
            .as_ref()
            .and_then(|definition| serde_json::to_value(definition).ok())
            .and_then(|value| {
                value
                    .pointer(path)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
    }

    fn changed_type_name(&mut self, path: &str, value: &str) {
        let Some(old) = self.column_type(path) else {
            return;
        };
        let old_params = type_parameters(&old);
        let new_params = type_parameters(value);
        let compatible = old_params
            .as_ref()
            .zip(new_params.as_ref())
            .is_some_and(|(old, new)| {
                (old.decimal && new.decimal)
                    || old.name.trim().eq_ignore_ascii_case(new.name.trim())
            });
        let updated = if compatible && !value.contains('(') {
            let old_params = old_params.as_ref().unwrap();
            let segment = &old[old_params.name.len()..old.len() - old_params.suffix.len()];
            let new_params = new_params.as_ref().unwrap();
            format!("{}{}{}", new_params.name, segment, new_params.suffix)
        } else {
            value.to_owned()
        };
        let length_path = format!("{path}/@length");
        let invalid_path = format!("{path}/@parameters");
        if !compatible {
            self.editors.get_mut().remove(&length_path);
            self.editors.get_mut().remove(&invalid_path);
            self.invalid_numbers.remove(&invalid_path);
        }
        if let Err(error) = self.apply_operation(SchemaOperation::Set {
            path: path.into(),
            value: json!(updated),
        }) {
            self.error = Some(error);
            return;
        }
        if compatible {
            let candidate = self
                .editors
                .get_mut()
                .get(&length_path)
                .map(|editor| editor.text.clone());
            if let (Some(params), Some(candidate)) = (type_parameters(&updated), candidate) {
                if parse_parameter_text(&params, &candidate).is_err() {
                    self.invalid_numbers.insert(invalid_path.clone());
                    self.editors.get_mut().insert(invalid_path, Editor::new(""));
                } else if self.invalid_numbers.contains(&invalid_path) {
                    self.changed_type_parameter(path, &candidate);
                }
            }
        }
    }

    fn changed_type_parameter(&mut self, path: &str, value: &str) {
        let Some(text) = self.column_type(path) else {
            return;
        };
        let Some(params) = type_parameters(&text) else {
            return;
        };
        let invalid_path = format!("{path}/@parameters");
        let Ok((first, second)) = parse_parameter_text(&params, value) else {
            self.invalid_numbers.insert(invalid_path.clone());
            self.editors.get_mut().insert(invalid_path, Editor::new(""));
            self.dirty = true;
            self.generation = self.generation.wrapping_add(1);
            self.preview = None;
            return;
        };
        let updated = if params.decimal {
            format!("{}({first},{second}){}", params.name, params.suffix)
        } else if params.fractional_seconds && first.bytes().all(|byte| byte == b'0') {
            // MySQL 将零精度时间类型读回为不带括号的类型名。
            format!("{}{}", params.name, params.suffix)
        } else {
            format!("{}({first}){}", params.name, params.suffix)
        };
        if self.invalid_numbers.remove(&invalid_path) {
            self.error = None;
        }
        self.editors.get_mut().remove(&invalid_path);
        if let Err(error) = self.apply_operation(SchemaOperation::Set {
            path: path.into(),
            value: json!(updated),
        }) {
            self.error = Some(error);
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
        if command == "section" {
            if matches!(
                path,
                "columns" | "indexes" | "foreign_keys" | "checks" | "options" | "ddl_preview"
            ) {
                self.section = path.to_owned();
            }
            return true;
        }
        if command == "column" {
            self.selected_column = Some(path.to_owned());
            return true;
        }
        if command == "item" {
            self.selected_items.insert(path.to_owned(), arg.to_owned());
            return true;
        }
        if command == "open" {
            self.open_select = if arg == "true" {
                Some(path.to_owned())
            } else {
                None
            };
            return true;
        }
        if !self.invalid_numbers.is_empty() {
            self.error = Some(tr("请先修正无效的字段输入").into_owned());
            return true;
        }
        if command == "primary" {
            if let Some(root) = self
                .definition
                .as_ref()
                .and_then(|d| serde_json::to_value(d).ok())
            {
                match schema_primary::toggle_primary(&root, path) {
                    Ok(operations) => {
                        for operation in operations {
                            if let Err(error) = self.apply_operation(operation) {
                                self.error = Some(error);
                                break;
                            }
                        }
                    }
                    Err(error) => self.error = Some(tr(&error).into_owned()),
                }
                self.editors
                    .get_mut()
                    .retain(|key, _| !key.starts_with("/indexes/"));
            }
            return true;
        }
        if command == "index-column" {
            self.open_select = None;
            if let Some(part_path) = path.strip_suffix("/column")
                && let Some(root) = self
                    .definition
                    .as_ref()
                    .and_then(|d| serde_json::to_value(d).ok())
                && let Some(mut part) = root.pointer(part_path).cloned()
            {
                let index_path = part_path
                    .split_once("/parts/")
                    .map(|(index, _)| index)
                    .unwrap_or("");
                let primary = root
                    .pointer(index_path)
                    .and_then(|index| index.get("kind"))
                    .and_then(Value::as_str)
                    == Some("primary");
                if primary && !arg.is_empty() {
                    let duplicate = root
                        .pointer(&format!("{index_path}/parts"))
                        .and_then(Value::as_array)
                        .is_some_and(|parts| {
                            parts.iter().enumerate().any(|(i, item)| {
                                format!("{index_path}/parts/{i}") != part_path
                                    && item.get("column").and_then(Value::as_str) == Some(arg)
                            })
                        });
                    if duplicate {
                        self.error = Some(tr("主键不能重复包含同一列").into_owned());
                        return true;
                    }
                }
                part["column"] = json!(arg);
                part["expression"] = Value::Null;
                if let Err(error) = self.apply_operation(SchemaOperation::Set {
                    path: part_path.into(),
                    value: part,
                }) {
                    self.error = Some(error);
                    return true;
                }
                self.editors
                    .get_mut()
                    .retain(|key, _| !key.starts_with(&(part_path.to_owned() + "/")));
                if primary
                    && !arg.is_empty()
                    && let Some(index) = root["columns"].as_array().and_then(|columns| {
                        columns
                            .iter()
                            .position(|column| column["name"].as_str() == Some(arg))
                    })
                {
                    match schema_primary::primary_column_changes(&root, index) {
                        Ok(changes) => {
                            for operation in changes.into_iter().flatten() {
                                if let Err(error) = self.apply_operation(operation) {
                                    self.error = Some(error);
                                    break;
                                }
                            }
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
            }
            return true;
        }
        if command == "remove" && path == "/columns" {
            if let (Ok(index), Some(root)) = (
                arg.parse::<usize>(),
                self.definition
                    .as_ref()
                    .and_then(|d| serde_json::to_value(d).ok()),
            ) {
                match schema_primary::remove_column(
                    &root,
                    index,
                    self.draft.target.database.as_deref(),
                    &self.draft.target.name,
                ) {
                    Ok(operations) => {
                        for operation in operations {
                            if let Err(error) = self.apply_operation(operation) {
                                self.error = Some(error);
                                break;
                            }
                        }
                    }
                    Err(error) => self.error = Some(tr(&error).into_owned()),
                }
                self.editors.get_mut().clear();
            }
            return true;
        }
        if command == "column-type" {
            self.open_select = None;
            if let Some((_, preset)) = COLUMN_TYPE_CHOICES.iter().find(|(kind, _)| *kind == arg)
                && let Some(old) = self.column_type(path)
            {
                let family = old
                    .trim_start()
                    .split(|c: char| !c.is_ascii_alphabetic())
                    .next()
                    .unwrap_or("");
                let numeric = |name: &str| {
                    [
                        "TINYINT",
                        "SMALLINT",
                        "MEDIUMINT",
                        "INT",
                        "INTEGER",
                        "BIGINT",
                        "DECIMAL",
                        "NUMERIC",
                        "FLOAT",
                        "DOUBLE",
                        "REAL",
                    ]
                    .iter()
                    .any(|kind| name.eq_ignore_ascii_case(kind))
                };
                let params = type_parameters(&old);
                let suffix =
                    if family.eq_ignore_ascii_case(arg) || (numeric(family) && numeric(arg)) {
                        params.as_ref().map_or_else(
                            || &old[old.len() - old.trim_start().len() + family.len()..],
                            |params| params.suffix,
                        )
                    } else {
                        ""
                    };
                let value = if family.eq_ignore_ascii_case(arg)
                    || (params.as_ref().is_some_and(|p| p.decimal)
                        && matches!(arg, "DECIMAL" | "NUMERIC"))
                {
                    arg
                } else {
                    preset
                };
                self.changed_type_name(path, &format!("{value}{suffix}"));
            }
            return true;
        }
        if command == "choice" {
            self.open_select = None;
            if path.starts_with("/indexes/") && path.ends_with("/kind") && arg == "primary" {
                self.error = Some(tr("请通过字段行设置主键").into_owned());
                return true;
            }
            if arg == "true"
                && path.starts_with("/columns/")
                && path.ends_with("/nullable")
                && let Some(root) = self
                    .definition
                    .as_ref()
                    .and_then(|d| serde_json::to_value(d).ok())
            {
                let name_path = path.trim_end_matches("/nullable").to_owned() + "/name";
                let name = root
                    .pointer(&name_path)
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if !name.is_empty()
                    && root["indexes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|index| {
                            index["kind"] == "primary"
                                && index["parts"]
                                    .as_array()
                                    .into_iter()
                                    .flatten()
                                    .any(|part| part["column"].as_str() == Some(name))
                        })
                {
                    self.error = Some(tr("主键列不能允许 NULL").into_owned());
                    return true;
                }
            }
            if path.starts_with("/columns/") && path.ends_with("/nullable") {
                if let Err(error) = self.apply_operation(SchemaOperation::Set {
                    path: path.into(),
                    value: json!(arg == "true"),
                }) {
                    self.error = Some(error);
                    return true;
                }
                let default_path = path.trim_end_matches("/nullable").to_owned() + "/default";
                if arg == "false"
                    && self
                        .definition
                        .as_ref()
                        .and_then(|definition| serde_json::to_value(definition).ok())
                        .and_then(|value| value.pointer(&format!("{default_path}/kind")).cloned())
                        == Some(json!("null"))
                    && let Err(error) = self.apply_operation(SchemaOperation::Set {
                        path: default_path,
                        value: json!({"kind":"unspecified"}),
                    })
                {
                    self.error = Some(error);
                }
                return true;
            }
            if path.ends_with("/default/kind")
                && self
                    .definition
                    .as_ref()
                    .and_then(|definition| serde_json::to_value(definition).ok())
                    .and_then(|value| {
                        value
                            .pointer(path)
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .as_deref()
                    == Some(arg)
            {
                return true;
            }
            let (target, value) = if path.ends_with("/default/kind") {
                (
                    path.trim_end_matches("/kind"),
                    if matches!(arg, "literal" | "expression") {
                        json!({"kind": arg, "value": ""})
                    } else {
                        json!({"kind": arg})
                    },
                )
            } else if self
                .definition
                .as_ref()
                .and_then(|definition| serde_json::to_value(definition).ok())
                .and_then(|value| value.pointer(path).and_then(Value::as_bool))
                .is_some()
            {
                (path, json!(arg == "true"))
            } else {
                (path, json!(arg))
            };
            if let Err(error) = self.apply_operation(SchemaOperation::Set {
                path: target.into(),
                value,
            }) {
                self.error = Some(error);
            }
            self.editors.get_mut().clear();
            return true;
        }
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
            "add" => template(path, arg).map(|mut value| {
                if path == "/constraints" && arg == "foreign_key" {
                    value["referenced_database"] =
                        json!(self.draft.target.database.as_deref().unwrap_or(""));
                }
                SchemaOperation::Insert {
                    path: path.into(),
                    value,
                }
            }),
            "remove" => arg.parse().ok().map(|index| SchemaOperation::Remove {
                path: path.into(),
                index,
            }),
            "move" => arg.split_once(',').and_then(|(from, to)| {
                Some(SchemaOperation::Move {
                    path: path.into(),
                    from: from.parse().ok()?,
                    to: to.parse().ok()?,
                })
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
        let mut panel = html::div().class("schema-panel advanced-panel");
        let mut context = html::div().class("advanced-facts");
        if let Some(database) = &self.draft.target.database {
            context = context.child(super::fact("数据库", database.clone()));
        }
        if !self.draft.target.name.is_empty() {
            context = context.child(super::fact("对象名称", self.draft.target.name.clone()));
        }
        panel = panel.child(context);
        if self.busy() {
            panel = panel.child(html::p(tr("处理中").into_owned()).class("notice"));
        }
        if self.summary.is_some() {
            panel = panel.child(
                html::p(tr("计划已冻结，修改条件前请先丢弃计划。").into_owned()).class("notice"),
            );
        }
        if self.session.is_null() && self.connect_nonce.is_none() {
            panel = panel.child(
                ui::button(tr("重试连接").into_owned())
                    .on_click(Msg::Action("schema-connect".into())),
            );
        }
        if self.outcome_unknown {
            panel = panel.child(html::p(tr("执行结果未知。请在数据库中重新读取目标结构，核对已执行的 DDL 和实际对象状态；确认后再解除等待，不会自动重试。").into_owned()).class("notice"));
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
            let completeness = match &snapshot.completeness.state {
                datapier_core::model::schema::CompletenessState::Complete => "完整",
                datapier_core::model::schema::CompletenessState::Incomplete => "不完整",
                datapier_core::model::schema::CompletenessState::Unknown => "未知",
                datapier_core::model::schema::CompletenessState::Unavailable => "不可用",
            };
            let mut facts = html::div()
                .class("advanced-facts")
                .child(super::fact("完整性", tr(completeness).into_owned()));
            for reason in &snapshot.completeness.reasons {
                facts = facts.child(super::fact("原因", reason.clone()));
            }
            for (name, capability) in &snapshot.capabilities {
                let label = match name.as_str() {
                    "check" => "CHECK 约束",
                    "expression_default" => "表达式默认值",
                    "functional_index" => "函数索引",
                    "descending_index" => "降序索引",
                    "index_visibility" => "索引可见性",
                    "invisible_column" => "不可见列",
                    _ => name,
                };
                let state = match capability.state.as_str() {
                    "unsupported" => tr("不支持").into_owned(),
                    "unknown" => tr("未知").into_owned(),
                    other => other.to_owned(),
                };
                let value = match &capability.reason {
                    Some(reason) => format!("{state} · {reason}"),
                    None => state,
                };
                facts = facts.child(super::fact(label, value));
            }
            panel = panel.child(facts);
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
        let busy = self.busy();
        let has_plan = self.summary.is_some();
        let table = matches!(self.definition.as_ref(), Some(SchemaDefinition::Table(_)));
        let show_plan_details = !table || self.section == "ddl_preview";
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
        let mut actions = html::div().class("advanced-actions schema-sticky-actions");
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
            actions = actions.child(
                ui::button(tr(label).into_owned())
                    .variant(
                        if (action == "schema-plan" && !has_plan)
                            || (action == "schema-submit" && has_plan)
                        {
                            ui::ButtonVariant::Primary
                        } else {
                            ui::ButtonVariant::Default
                        },
                    )
                    .disabled(busy || !enabled)
                    .on_click(Msg::Action(action.into())),
            );
        }
        panel = panel.child(actions);
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
        if let Some(definition) = &self.definition
            && let Ok(value) = serde_json::to_value(definition)
        {
            let table = value.get("kind").and_then(Value::as_str) == Some("table");
            let disabled = self.pending.is_some()
                || self.execution_pending
                || self.summary.is_some()
                || self.query_transferred
                || self.restore_confirmation_required;
            panel = panel.child(
                html::p(tr("以下修改仅保存在结构草稿中，提交后才会作用于数据库。").into_owned())
                    .class("muted"),
            );
            if table {
                panel = panel.child(schema_tabs(&self.section, false));
                if self.section == "columns" {
                    if let Some(content) = value.get("columns") {
                        panel = panel.child(render_value(
                            content,
                            "/columns",
                            &mut self.editors.borrow_mut(),
                            disabled,
                            true,
                            self.selected_column.as_deref(),
                            SchemaChoices {
                                open_select: self.open_select.as_deref(),
                                root: Some(&value),
                            },
                        ));
                    }
                } else if matches!(self.section.as_str(), "indexes" | "foreign_keys" | "checks") {
                    panel = panel.child(render_object_grid(
                        &value,
                        &self.section,
                        &mut self.editors.borrow_mut(),
                        disabled,
                        self.selected_items.get(&self.section).map(String::as_str),
                        self.open_select.as_deref(),
                        SchemaGridContext {
                            references: &self.references,
                            database: self.draft.target.database.as_deref().unwrap_or(""),
                            table_name: &self.draft.target.name,
                        },
                    ));
                } else if self.section == "options" {
                    let mut options = serde_json::Map::new();
                    for key in ["engine", "charset", "collation", "comment", "options"] {
                        if let Some(item) = value.get(key) {
                            options.insert(key.to_owned(), item.clone());
                        }
                    }
                    panel = panel.child(html::div().class("schema-design-table-options").child(
                        render_value(
                            &Value::Object(options),
                            "",
                            &mut self.editors.borrow_mut(),
                            disabled,
                            true,
                            self.selected_column.as_deref(),
                            SchemaChoices {
                                open_select: self.open_select.as_deref(),
                                root: Some(&value),
                            },
                        ),
                    ));
                }
            } else {
                panel = panel.child(render_value(
                    &value,
                    "",
                    &mut self.editors.borrow_mut(),
                    disabled,
                    false,
                    self.selected_column.as_deref(),
                    SchemaChoices {
                        open_select: self.open_select.as_deref(),
                        root: Some(&value),
                    },
                ));
            }
            if !table || self.section == "options" {
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
                            self.selected_column.as_deref(),
                            SchemaChoices {
                                open_select: self.open_select.as_deref(),
                                root: Some(&value),
                            },
                        ));
                }
            }
        }
        if self.confirm_submit && show_plan_details {
            let mut confirmation = html::div().class("advanced-section")
                .child(html::strong(tr("确认执行预览中的结构变更？").into_owned()))
                .child(super::fact("数据库", self.draft.target.database.clone().unwrap_or_default()))
                .child(super::fact("对象名称", self.draft.target.name.clone()))
                .child(html::p(tr("以下冻结的 DDL 将按顺序执行；已执行的变更不能自动回滚，请先确认备份和恢复方式。").into_owned()).class("notice"));
            for sql in &self.preview_sql {
                confirmation = confirmation.child(html::pre(sql.clone()).class("advanced-code"));
            }
            for reason in &self.destructive_reasons {
                confirmation = confirmation.child(html::p(reason.clone()).class("notice"));
            }
            confirmation = confirmation
                .child(
                    ui::button(tr("取消").into_owned())
                        .on_click(Msg::Action("schema-submit-cancel".into())),
                )
                .child(
                    ui::button(tr("确认执行").into_owned())
                        .variant(if self.destructive_reasons.is_empty() {
                            ui::ButtonVariant::Primary
                        } else {
                            ui::ButtonVariant::Danger
                        })
                        .disabled(busy || !can_submit)
                        .on_click(Msg::Action("schema-submit-confirm".into())),
                );
            panel = panel.child(confirmation);
        }
        if show_plan_details && self.query_sql().is_some() {
            panel = panel.child(
                ui::button(tr("转到查询编辑器").into_owned())
                    .on_click(Msg::Action("schema-query".into())),
            );
        }
        if show_plan_details {
            if let Some(summary) = &self.summary {
                let mut facts = html::div().class("advanced-facts schema-plan-summary");
                if let Some(state) = summary.get("state").and_then(Value::as_str) {
                    facts = facts.child(super::fact("状态", super::status(state)));
                }
                if let Some(total) = summary.get("total_items").and_then(Value::as_u64) {
                    facts = facts.child(super::fact("步骤", total.to_string()));
                }
                facts = facts
                    .child(super::fact(
                        "计划完整",
                        tr(if summary["complete"] == true {
                            "是"
                        } else {
                            "否"
                        })
                        .into_owned(),
                    ))
                    .child(super::fact(
                        "可提交",
                        tr(if summary["submittable"] == true {
                            "是"
                        } else {
                            "否"
                        })
                        .into_owned(),
                    ));
                panel = panel.child(facts);
                if self.preview.is_none() {
                    panel = panel.child(
                        html::p(tr("计划已生成，请使用上方预览按钮读取 DDL。").into_owned())
                            .class("notice"),
                    );
                } else if !self.preview_complete {
                    panel = panel.child(
                        html::p(tr("尚未读取全部预览页，请继续读取后再提交。").into_owned())
                            .class("notice"),
                    );
                }
            } else if table {
                let message = if self.execution.is_some() {
                    "计划已提交，请查看执行结果。"
                } else {
                    "尚未生成计划，请使用上方生成计划按钮查看 DDL。"
                };
                panel = panel.child(html::p(tr(message).into_owned()).class("notice"));
            }
        }
        if show_plan_details && let Some(preview) = &self.preview {
            let mut section = html::div()
                .class("advanced-section")
                .child(html::strong(tr("变更预览").into_owned()));
            if let Some(steps) = preview.get("steps").and_then(Value::as_array) {
                for step in steps {
                    if let Some(sql) = step.get("sql").and_then(Value::as_str) {
                        section = section.child(html::pre(sql.to_owned()).class("advanced-code"));
                    }
                    for key in ["destructive_reasons", "warnings"] {
                        if let Some(reasons) = step.get(key).and_then(Value::as_array) {
                            for reason in reasons.iter().filter_map(Value::as_str) {
                                section = section.child(html::p(reason.to_owned()).class("notice"));
                            }
                        }
                    }
                }
            }
            if preview.get("next").is_some_and(|v| !v.is_null()) {
                section = section.child(
                    ui::button(tr("下一页").into_owned())
                        .on_click(Msg::Action("schema-preview-next".into())),
                );
            }
            panel = panel
                .child(section)
                .child(super::diagnostic("schema-preview-diagnostic", preview));
        }
        if let Some(execution) = &self.execution {
            let mut section = html::div()
                .class("advanced-section")
                .child(html::strong(tr("执行结果").into_owned()));
            if let Some(result) = execution.get("result") {
                if let Some(state) = result
                    .pointer("/summary/state")
                    .or_else(|| result.get("state"))
                    .and_then(Value::as_str)
                {
                    section = section.child(super::fact("状态", super::status(state)));
                }
                if let Some(steps) = result.pointer("/summary/steps").and_then(Value::as_array) {
                    for (index, step) in steps.iter().enumerate() {
                        if let Some(state) = step.get("state").and_then(Value::as_str) {
                            section = section.child(super::fact(
                                &format!("{} {}", tr("步骤"), index + 1),
                                super::status(state),
                            ));
                        }
                        for key in ["reason", "error"] {
                            if let Some(reason) = step.get(key).and_then(Value::as_str) {
                                section = section.child(super::fact("原因", reason));
                            }
                        }
                    }
                }
            } else {
                section = section.child(html::p(tr("处理中").into_owned()).class("muted"));
            }
            panel = panel
                .child(section)
                .child(super::diagnostic("schema-execution-diagnostic", execution));
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
        "@length" => "长度",
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

pub(crate) fn schema_tabs(selected: &str, readonly: bool) -> Node<Msg> {
    let mut tabs = html::div()
        .class("advanced-tabs schema-design-tabs")
        .attr("role", "tablist");
    for (id, label) in [
        ("columns", "字段"),
        ("indexes", "索引"),
        ("foreign_keys", "外键"),
        ("checks", "CHECK"),
        ("options", "表选项"),
    ]
    .into_iter()
    .chain((!readonly).then_some(("ddl_preview", "预览 DDL")))
    {
        let focus = format!("schema-section-{id}");
        let action = format!(
            "{}:{id}",
            if readonly {
                "schema-read-section"
            } else {
                "schema-section"
            }
        );
        tabs = tabs.child(
            html::button(tr(label).into_owned())
                .key(focus.clone())
                .class(if selected == id {
                    "advanced-tab active"
                } else {
                    "advanced-tab"
                })
                .attr("role", "tab")
                .attr("aria-selected", selected == id)
                .on_click(Msg::Action(action)),
        );
    }
    tabs.into()
}

fn row_action(label: &str, icon: &str, action: String, disabled: bool) -> Node<Msg> {
    let label = tr(label).into_owned();
    ui::button("")
        .icon(icon.to_owned())
        .aria(label.clone())
        .title(label)
        .disabled(disabled)
        .on_click(Msg::Action(action))
        .into()
}

fn candidate_select(
    path: &str,
    label: &str,
    current: &str,
    candidates: &[String],
    disabled: bool,
    open: Option<&str>,
    command: &str,
) -> Node<Msg> {
    let toggle_path = path.to_owned();
    let change_path = path.to_owned();
    let command = command.to_owned();
    let mut select = ui::select()
        .aria(tr(label).into_owned())
        .value(current.to_owned())
        .disabled(disabled)
        .open(open == Some(path))
        .on_toggle(move |collapsed| {
            Msg::Action(format!("schema-op:open:{toggle_path}:{}", !collapsed))
        })
        .option(ui::SelectOption::new("", tr("请选择").into_owned()));
    if !current.is_empty() && !candidates.iter().any(|value| value == current) {
        select = select.option(ui::SelectOption::new(
            current.to_owned(),
            format!("{} · {}", current, tr("当前值未加载或不可见")),
        ));
    }
    for candidate in candidates {
        select = select.option(ui::SelectOption::new(candidate.clone(), candidate.clone()));
    }
    select
        .on_change(move |change| {
            Msg::Action(format!(
                "schema-op:{command}:{change_path}:{}",
                change.value.as_str().unwrap_or_default()
            ))
        })
        .into()
}

fn ordered_columns(
    items: &Value,
    path: &str,
    label: &str,
    candidates: &[String],
    index_parts: bool,
    disabled: bool,
    open: Option<&str>,
) -> Node<Msg> {
    let mut cell = html::div().class("schema-design-ordered-columns");
    let values = items.as_array();
    let count = values.map_or(0, Vec::len);
    for (index, item) in values.into_iter().flatten().enumerate() {
        let value = if index_parts {
            item.get("column").and_then(Value::as_str).unwrap_or("")
        } else {
            item.as_str().unwrap_or("")
        };
        let item_path = format!("{path}/{index}");
        let select_path = if index_parts {
            format!("{item_path}/column")
        } else {
            item_path
        };
        let mut line = html::div()
            .class("schema-design-ordered-column")
            .child(candidate_select(
                &select_path,
                label,
                value,
                candidates,
                disabled,
                open,
                if index_parts {
                    "index-column"
                } else {
                    "choice"
                },
            ));
        if let Some(expression) = item
            .get("expression")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            line = line.child(html::span(expression.to_owned()));
        }
        for (title, icon, action, unavailable) in [
            ("前移", "chev-up", "up", index == 0),
            ("后移", "chev", "down", index + 1 == count),
            ("删除", "trash", "remove", false),
        ] {
            line = line.child(row_action(
                title,
                icon,
                format!("schema-op:{action}:{path}:{index}"),
                disabled || unavailable,
            ));
        }
        cell = cell.child(line);
    }
    cell = cell.child(row_action(
        "添加列",
        "plus",
        format!("schema-op:add:{path}:item"),
        disabled,
    ));
    cell.into()
}

struct SchemaGridContext<'a> {
    references: &'a schema_reference::ReferenceState,
    database: &'a str,
    table_name: &'a str,
}

fn render_object_grid(
    root: &Value,
    section: &str,
    editors: &mut BTreeMap<String, Editor>,
    disabled: bool,
    selected_id: Option<&str>,
    open_select: Option<&str>,
    context: SchemaGridContext<'_>,
) -> Node<Msg> {
    let SchemaGridContext {
        references,
        database,
        table_name,
    } = context;
    let (path, kind, fields): (&str, &str, &[(&str, &str)]) = match section {
        "indexes" => (
            "/indexes",
            "",
            &[
                ("name", "名称"),
                ("kind", "种类"),
                ("@table", "表"),
                ("parts", "列"),
                ("visible", "可见"),
            ],
        ),
        "foreign_keys" => (
            "/constraints",
            "foreign_key",
            &[
                ("name", "名称"),
                ("columns", "本地列"),
                ("referenced_database", "引用数据库"),
                ("referenced_table", "引用表"),
                ("referenced_columns", "引用列"),
                ("on_update", "更新时"),
                ("on_delete", "删除时"),
            ],
        ),
        _ => (
            "/constraints",
            "check",
            &[
                ("name", "名称"),
                ("expression", "表达式"),
                ("enforced", "启用约束"),
            ],
        ),
    };
    let items = root.pointer(path).and_then(Value::as_array);
    let matching: Vec<_> = items
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, item)| {
            kind.is_empty() || item.get("kind").and_then(Value::as_str) == Some(kind)
        })
        .collect();
    let selected = selected_id
        .and_then(|id| {
            matching
                .iter()
                .position(|(_, item)| item.get("id").and_then(Value::as_str) == Some(id))
        })
        .or_else(|| (!matching.is_empty()).then_some(0));
    let mut grid = html::div().class("schema-design-object-grid");
    let mut header = html::div().class(format!(
        "schema-design-object-row schema-design-object-{section} schema-design-column-header"
    ));
    for (_, label) in fields {
        header = header.child(html::strong(tr(label).into_owned()));
    }
    header = header.child(html::strong(tr("操作").into_owned()));
    grid = grid.child(header);
    let local_columns: Vec<String> = root["columns"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|column| {
            column["name"]
                .as_str()
                .filter(|name| !name.is_empty())
                .map(str::to_owned)
        })
        .collect();
    for (position, (index, item)) in matching.iter().enumerate() {
        let id = item.get("id").and_then(Value::as_str).unwrap_or("");
        let referenced_database = item
            .get("referenced_database")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(database);
        let referenced_table = item
            .get("referenced_table")
            .and_then(Value::as_str)
            .unwrap_or("");
        let entry = references.entries.get(id).filter(|entry| {
            entry.database == referenced_database && entry.table == referenced_table
        });
        let self_reference = referenced_database == database
            && referenced_table == table_name
            && !table_name.is_empty();
        let mut row = html::div()
            .class(format!(
                "schema-design-object-row schema-design-object-{section}{}",
                if selected == Some(position) {
                    " selected"
                } else {
                    ""
                }
            ))
            .key(id.to_owned())
            .on_click(Msg::Action(format!("schema-op:item:{section}:{id}")));
        for (key, label) in fields {
            let field_path = format!("{path}/{index}/{key}");
            let mut cell = html::div().class("schema-design-object-cell");
            if *key == "@table" {
                cell = cell.child(html::span(table_name.to_owned()));
                row = row.child(cell);
                continue;
            }
            if matches!(*key, "parts" | "columns" | "referenced_columns") {
                let candidates = if *key == "referenced_columns" {
                    if self_reference {
                        local_columns.as_slice()
                    } else {
                        entry.map_or(&[][..], |entry| entry.columns.as_slice())
                    }
                } else {
                    local_columns.as_slice()
                };
                let empty = json!([]);
                cell = cell.child(ordered_columns(
                    item.get(*key).unwrap_or(&empty),
                    &field_path,
                    label,
                    candidates,
                    *key == "parts",
                    disabled,
                    open_select,
                ));
                if *key == "referenced_columns" && !self_reference {
                    let loading = entry.is_some_and(|entry| entry.columns_loading);
                    let complete = entry.is_some_and(|entry| entry.columns_complete);
                    if let Some(error) = entry.and_then(|entry| entry.columns_error.as_deref()) {
                        cell = cell.child(html::p(tr(error).into_owned()).class("field-error"));
                    }
                    if loading {
                        cell = cell.child(html::span(tr("正在加载引用列").into_owned()));
                    }
                    if !complete && !loading {
                        cell = cell.child(
                            ui::button(tr("加载引用列").into_owned())
                                .disabled(disabled || referenced_table.is_empty())
                                .on_click(Msg::Action(format!("schema-ref:load:{id}:columns"))),
                        );
                    }
                }
                row = row.child(cell);
                continue;
            }
            if *key == "referenced_table" {
                let mut candidates = entry.map_or_else(Vec::new, |entry| entry.tables.clone());
                if referenced_database == database
                    && !table_name.is_empty()
                    && !candidates.iter().any(|value| value == table_name)
                {
                    candidates.push(table_name.to_owned());
                }
                cell = cell.child(candidate_select(
                    &field_path,
                    label,
                    referenced_table,
                    &candidates,
                    disabled,
                    open_select,
                    "choice",
                ));
                let loading = entry.is_some_and(|entry| entry.tables_loading);
                let complete = entry.is_some_and(|entry| entry.tables_complete);
                if let Some(error) = entry.and_then(|entry| entry.tables_error.as_deref()) {
                    cell = cell.child(html::p(tr(error).into_owned()).class("field-error"));
                }
                if loading {
                    cell = cell.child(html::span(tr("正在加载引用表").into_owned()));
                }
                if !complete && !loading {
                    cell = cell.child(
                        ui::button(tr("加载引用表").into_owned())
                            .disabled(disabled || referenced_database.is_empty())
                            .on_click(Msg::Action(format!("schema-ref:load:{id}:tables"))),
                    );
                }
                row = row.child(cell);
                continue;
            }
            if let Some(value) = item.get(*key) {
                if let Some(on) = value.as_bool() {
                    let toggle_path = field_path.clone();
                    let change_path = field_path.clone();
                    cell = cell.child(
                        ui::select()
                            .aria(tr(label).into_owned())
                            .value(if on { "true" } else { "false" })
                            .open(open_select == Some(field_path.as_str()))
                            .disabled(disabled)
                            .option(ui::SelectOption::new("true", tr("是").into_owned()))
                            .option(ui::SelectOption::new("false", tr("否").into_owned()))
                            .on_toggle(move |collapsed| {
                                Msg::Action(format!("schema-op:open:{toggle_path}:{}", !collapsed))
                            })
                            .on_change(move |change| {
                                Msg::Action(format!(
                                    "schema-op:choice:{change_path}:{}",
                                    change.value.as_str().unwrap_or_default()
                                ))
                            }),
                    );
                } else {
                    cell = cell.child(render_value(
                        value,
                        &field_path,
                        editors,
                        disabled,
                        true,
                        None,
                        SchemaChoices {
                            open_select,
                            root: Some(root),
                        },
                    ));
                }
            }
            if *key == "name" {
                cell = cell.child(
                    ui::button(tr("扩展属性").into_owned())
                        .on_click(Msg::Action(format!("schema-op:item:{section}:{id}"))),
                );
            }
            row = row.child(cell);
        }
        let mut actions = html::div().class("schema-design-row-actions");
        for (label, icon, target) in [
            ("前移", "chev-up", position.checked_sub(1)),
            (
                "后移",
                "chev",
                (position + 1 < matching.len()).then_some(position + 1),
            ),
        ] {
            let destination = target.map(|n| matching[n].0).unwrap_or(*index);
            actions = actions.child(row_action(
                label,
                icon,
                format!("schema-op:move:{path}:{index},{destination}"),
                disabled || target.is_none(),
            ));
        }
        actions = actions.child(row_action(
            "删除",
            "trash",
            format!("schema-op:remove:{path}:{index}"),
            disabled,
        ));
        row = row.child(actions);
        grid = grid.child(row);
    }
    let add_kind = if kind.is_empty() { "item" } else { kind };
    let mut content = html::div().class("advanced-fields").child(grid).child(
        html::div().class("advanced-actions").child(row_action(
            "新增",
            "plus",
            format!("schema-op:add:{path}:{add_kind}"),
            disabled,
        )),
    );
    if let Some(position) = selected {
        let (index, item) = matching[position];
        let mut properties = item.as_object().cloned().unwrap_or_default();
        for (key, _) in fields {
            if *key != "parts" {
                properties.remove(*key);
            }
        }
        properties.remove("kind");
        let mut detail = html::div()
            .class("schema-design-column-details")
            .child(html::strong(tr("选中项扩展属性").into_owned()))
            .child(html::span(
                item.get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .unwrap_or("—")
                    .to_owned(),
            ));
        if properties.keys().any(|key| key != "id") {
            detail = detail.child(render_value(
                &Value::Object(properties),
                &format!("{path}/{index}"),
                editors,
                disabled,
                true,
                None,
                SchemaChoices {
                    open_select,
                    root: Some(root),
                },
            ));
        }
        content = content.child(detail);
    }
    content.into()
}

fn schema_choices(path: &str) -> &'static [(&'static str, &'static str)] {
    if path.ends_with("/default/kind") {
        &[
            ("unspecified", "未指定"),
            ("null", "NULL"),
            ("literal", "字面量"),
            ("expression", "表达式"),
        ]
    } else if path.contains("/indexes/") && path.ends_with("/kind") {
        &[
            ("unique", "唯一索引"),
            ("normal", "普通索引"),
            ("fulltext", "全文索引"),
            ("spatial", "空间索引"),
        ]
    } else if path == "/timing" {
        &[("BEFORE", "之前"), ("AFTER", "之后")]
    } else if path == "/event" {
        &[("INSERT", "插入"), ("UPDATE", "更新"), ("DELETE", "删除")]
    } else if path == "/security" {
        &[("DEFINER", "定义者"), ("INVOKER", "调用者")]
    } else if path == "/algorithm" {
        &[
            ("UNDEFINED", "未指定"),
            ("MERGE", "合并"),
            ("TEMPTABLE", "临时表"),
        ]
    } else if path == "/check_option" {
        &[("LOCAL", "本地"), ("CASCADED", "级联")]
    } else if path.contains("/constraints/")
        && (path.ends_with("/on_update") || path.ends_with("/on_delete"))
    {
        &[
            ("RESTRICT", "限制"),
            ("CASCADE", "级联"),
            ("SET NULL", "设为 NULL"),
            ("NO ACTION", "不操作"),
        ]
    } else {
        &[]
    }
}

#[derive(Clone, Copy)]
struct SchemaChoices<'a> {
    open_select: Option<&'a str>,
    root: Option<&'a Value>,
}

fn render_value(
    value: &Value,
    path: &str,
    editors: &mut BTreeMap<String, Editor>,
    disabled: bool,
    table: bool,
    selected_column: Option<&str>,
    choices: SchemaChoices<'_>,
) -> Node<Msg> {
    let SchemaChoices { open_select, root } = choices;
    let mut container = html::div().class("advanced-fields");
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
                if let Value::Bool(on) = value {
                    let action = format!("schema-op:toggle:{child}");
                    container = container.child(
                        html::div().class("schema-design-boolean").child(
                            ui::switch(tr(field_label(key)).into_owned())
                                .on(*on)
                                .disabled(disabled || locked_identity)
                                .on_change(move |_| Msg::Action(action.clone())),
                        ),
                    );
                    continue;
                }
                let mut field = html::div()
                    .class(if path.is_empty() && table {
                        "advanced-field schema-design-section"
                    } else {
                        "advanced-field"
                    })
                    .child(html::strong(tr(field_label(key)).into_owned()))
                    .child(render_value(
                        value,
                        &child,
                        editors,
                        disabled || locked_identity,
                        table,
                        selected_column,
                        SchemaChoices { open_select, root },
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
                    field = field.child(
                        ui::button(tr("清除").into_owned())
                            .disabled(disabled)
                            .on_click(Msg::Action(format!("schema-op:set:{child}:null"))),
                    );
                }
                container = container.child(field);
            }
        }
        Value::Array(items) => {
            if path == "/columns" && table {
                let selected = selected_column
                    .and_then(|id| {
                        items
                            .iter()
                            .position(|item| item.get("id").and_then(Value::as_str) == Some(id))
                    })
                    .or_else(|| (!items.is_empty()).then_some(0));
                let mut list = html::div().class("schema-design-columns");
                let mut header =
                    html::div().class("schema-design-column-row schema-design-column-header");
                for label in [
                    "名称",
                    "类型",
                    "长度",
                    "允许 NULL",
                    "默认值",
                    "约束",
                    "注释",
                    "操作",
                ] {
                    header = header.child(html::strong(tr(label).into_owned()));
                }
                list = list.child(header);
                for (index, item) in items.iter().enumerate() {
                    let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                    let mut primary = false;
                    let mut foreign = false;
                    for index in root
                        .and_then(|v| v.get("indexes"))
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if !name.is_empty()
                            && index
                                .get("parts")
                                .and_then(Value::as_array)
                                .is_some_and(|parts| {
                                    parts.iter().any(|part| {
                                        part.get("column").and_then(Value::as_str) == Some(name)
                                    })
                                })
                            && index.get("kind").and_then(Value::as_str) == Some("primary")
                        {
                            primary = true;
                        }
                    }
                    for constraint in root
                        .and_then(|v| v.get("constraints"))
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if !name.is_empty()
                            && constraint.get("kind").and_then(Value::as_str) == Some("foreign_key")
                            && constraint
                                .get("columns")
                                .and_then(Value::as_array)
                                .is_some_and(|columns| {
                                    columns.iter().any(|column| column.as_str() == Some(name))
                                })
                        {
                            foreign = true;
                        }
                    }
                    let mut row = html::div()
                        .class(if selected == Some(index) {
                            "schema-design-column-row selected"
                        } else {
                            "schema-design-column-row"
                        })
                        .key(id.to_owned())
                        .on_click(Msg::Action(format!("schema-op:column:{id}:")));
                    if let Some(value) = item.get("name") {
                        row = row.child(render_value(
                            value,
                            &format!("/columns/{index}/name"),
                            editors,
                            disabled,
                            table,
                            selected_column,
                            SchemaChoices { open_select, root },
                        ));
                    }
                    let type_path = format!("/columns/{index}/native_type");
                    let native_type = item
                        .get("native_type")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let params = type_parameters(native_type);
                    let family = native_type
                        .trim_start()
                        .split(|c: char| !c.is_ascii_alphabetic())
                        .next()
                        .unwrap_or("");
                    let current = COLUMN_TYPE_CHOICES
                        .iter()
                        .find(|(choice, _)| family.eq_ignore_ascii_case(choice))
                        .map_or(native_type, |(choice, _)| *choice);
                    let toggle_path = type_path.clone();
                    let change_path = type_path.clone();
                    let mut type_select = ui::select()
                        .aria(tr("类型").into_owned())
                        .value(current)
                        .open(open_select == Some(type_path.as_str()))
                        .disabled(disabled);
                    if !COLUMN_TYPE_CHOICES
                        .iter()
                        .any(|(choice, _)| *choice == current)
                    {
                        type_select = type_select.option(ui::SelectOption::new(current, current));
                    }
                    for (choice, _) in COLUMN_TYPE_CHOICES {
                        let label = if *choice == current {
                            params.as_ref().map_or_else(
                                || native_type.to_owned(),
                                |params| format!("{}{}", params.name, params.suffix),
                            )
                        } else {
                            (*choice).to_owned()
                        };
                        type_select = type_select.option(ui::SelectOption::new(*choice, label));
                    }
                    row = row.child(
                        type_select
                            .on_toggle(move |collapsed| {
                                Msg::Action(format!("schema-op:open:{toggle_path}:{}", !collapsed))
                            })
                            .on_change(move |change| {
                                Msg::Action(format!(
                                    "schema-op:column-type:{change_path}:{}",
                                    change.value.as_str().unwrap_or_default()
                                ))
                            }),
                    );
                    let mut length_cell = html::div();
                    if let Some(params) = params {
                        if params.fractional_seconds {
                            length_cell =
                                length_cell.attr("title", tr("小数秒精度（0–6）").into_owned());
                        }
                        let length_path = format!("{type_path}/@length");
                        length_cell = length_cell.child(render_value(
                            &Value::String(type_parameter_text(&params)),
                            &length_path,
                            editors,
                            disabled,
                            table,
                            selected_column,
                            SchemaChoices { open_select, root },
                        ));
                        if editors.contains_key(&format!("{type_path}/@parameters"))
                            && let Some(editor) = editors.get(&length_path)
                            && let Err(error) = parse_parameter_text(&params, &editor.text)
                        {
                            length_cell =
                                length_cell.child(html::p(tr(error).into_owned()).class("notice"));
                        }
                    }
                    row = row.child(length_cell);
                    let nullable_path = format!("/columns/{index}/nullable");
                    let change_path = nullable_path.clone();
                    let nullable = html::checkbox(format!("schema-nullable-{id}"), "true")
                        .class("schema-design-nullable")
                        .aria(tr("允许 NULL").into_owned())
                        .checked(item.get("nullable") == Some(&Value::Bool(true)))
                        .disabled(disabled || primary)
                        .on_change(move |change| {
                            Msg::Action(format!(
                                "schema-op:choice:{change_path}:{}",
                                change.checked == Some(true)
                            ))
                        });
                    row = row.child(nullable);
                    let mut default_cell = html::div().class("schema-design-default");
                    if let Some(default) = item.get("default") {
                        if let Some(kind) = default.get("kind") {
                            default_cell = default_cell.child(render_value(
                                kind,
                                &format!("/columns/{index}/default/kind"),
                                editors,
                                disabled,
                                table,
                                selected_column,
                                SchemaChoices { open_select, root },
                            ));
                        }
                        if let Some(value) = default.get("value") {
                            default_cell = default_cell.child(render_value(
                                value,
                                &format!("/columns/{index}/default/value"),
                                editors,
                                disabled,
                                table,
                                selected_column,
                                SchemaChoices { open_select, root },
                            ));
                        }
                    }
                    let pk_label = tr(if primary {
                        "取消主键"
                    } else {
                        "设为主键"
                    })
                    .into_owned();
                    let mut badges = html::div().class("schema-design-column-constraints");
                    if primary {
                        badges = badges.child(
                            html::span("")
                                .attr("role", "img")
                                .aria(tr("主键").into_owned())
                                .title(tr("主键").into_owned())
                                .child(ui::icon("key")),
                        );
                    }
                    if foreign {
                        badges = badges.child(
                            html::span("")
                                .attr("role", "img")
                                .aria(tr("外键").into_owned())
                                .title(tr("外键").into_owned())
                                .child(ui::icon("link")),
                        );
                    }
                    if !primary && !foreign {
                        badges = badges.child(html::span("—"));
                    }
                    row = row.child(default_cell).child(badges);
                    if let Some(value) = item.get("comment") {
                        row = row.child(render_value(
                            value,
                            &format!("/columns/{index}/comment"),
                            editors,
                            disabled,
                            table,
                            selected_column,
                            SchemaChoices { open_select, root },
                        ));
                    }
                    let mut actions = html::div().class("schema-design-row-actions").child(
                        ui::button("")
                            .icon("key")
                            .aria(pk_label.clone())
                            .title(pk_label)
                            .variant(if primary {
                                ui::ButtonVariant::Primary
                            } else {
                                ui::ButtonVariant::Default
                            })
                            .disabled(disabled || name.trim().is_empty())
                            .on_click(Msg::Action(format!("schema-op:primary:{id}:"))),
                    );
                    for (label, icon, command, unavailable) in [
                        ("上移字段", "chev-up", "up", index == 0),
                        ("下移字段", "chev", "down", index + 1 == items.len()),
                        ("删除字段", "trash", "remove", false),
                    ] {
                        actions = actions.child(row_action(
                            label,
                            icon,
                            format!("schema-op:{command}:/columns:{index}"),
                            disabled || unavailable,
                        ));
                    }
                    row = row.child(actions);
                    list = list.child(row);
                }
                container =
                    container
                        .child(list)
                        .child(html::div().class("advanced-actions").child(row_action(
                            "新增字段",
                            "plus",
                            "schema-op:add:/columns:column".into(),
                            disabled,
                        )));
                if let Some(index) = selected {
                    let mut properties = items[index].as_object().cloned().unwrap_or_default();
                    for key in ["name", "native_type", "nullable", "default", "comment"] {
                        properties.remove(key);
                    }
                    let details = html::div()
                        .class("schema-design-column-details")
                        .child(html::strong(tr("选中字段扩展属性").into_owned()))
                        .child(html::span(
                            items[index]
                                .get("name")
                                .and_then(Value::as_str)
                                .filter(|name| !name.is_empty())
                                .unwrap_or("—")
                                .to_owned(),
                        ))
                        .child(render_value(
                            &Value::Object(properties),
                            &format!("/columns/{index}"),
                            editors,
                            disabled,
                            table,
                            selected_column,
                            SchemaChoices { open_select, root },
                        ));
                    container = container.child(details);
                }
                return container.into();
            }
            for (index, item) in items.iter().enumerate() {
                let child = format!("{path}/{index}");
                let row_key = item
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| child.clone());
                let mut row =
                    html::div()
                        .class("advanced-section")
                        .key(row_key)
                        .child(render_value(
                            item,
                            &child,
                            editors,
                            disabled,
                            table,
                            selected_column,
                            SchemaChoices { open_select, root },
                        ));
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
                    container = container.child(row_action(
                        label,
                        "plus",
                        format!("schema-op:add:{path}:{kind}"),
                        disabled,
                    ));
                }
            }
        }
        Value::Bool(on) => {
            container = container.child(
                ui::switch(tr(field_label(path.rsplit('/').next().unwrap_or(""))).into_owned())
                    .on(*on)
                    .disabled(disabled)
                    .on_change({
                        let path = path.to_owned();
                        move |_| Msg::Action(format!("schema-op:toggle:{path}"))
                    }),
            );
        }
        Value::String(text) if !schema_choices(path).is_empty() => {
            let label = tr(if path.ends_with("/default/kind") {
                "默认值种类"
            } else {
                field_label(path.rsplit('/').next().unwrap_or(""))
            })
            .into_owned();
            let toggle_path = path.to_owned();
            let change_path = path.to_owned();
            let mut select = ui::select()
                .aria(label)
                .value(text.clone())
                .disabled(disabled)
                .open(open_select == Some(path))
                .on_toggle(move |collapsed| {
                    Msg::Action(format!("schema-op:open:{toggle_path}:{}", !collapsed))
                });
            let choices = schema_choices(path);
            if !choices.iter().any(|(value, _)| *value == text.as_str()) {
                select = select.option(ui::SelectOption::new(text.clone(), text.clone()));
            }
            for (value, label) in choices {
                select = select.option(ui::SelectOption::new(*value, tr(label).into_owned()));
            }
            container = container.child(select.on_change(move |change| {
                Msg::Action(format!(
                    "schema-op:choice:{change_path}:{}",
                    change.value.as_str().unwrap_or_default()
                ))
            }));
        }
        Value::String(text) if path.ends_with("/kind") => {
            container = container.child(html::span(text.clone()));
        }
        Value::String(text) => {
            let editor = editors
                .entry(path.to_owned())
                .or_insert_with(|| Editor::new(text));
            let name = format!("schema:{path}");
            let key = path.rsplit('/').next().unwrap_or("");
            let label = tr(if key == "value" && path.contains("/default/") {
                "默认值"
            } else {
                field_label(key)
            })
            .into_owned();
            let long = matches!(key, "select" | "body" | "expression");
            if long {
                let edit_name = name.clone();
                let undo_name = name.clone();
                container = container.child(
                    ui::editor()
                        .key(name.clone())
                        .aria(label)
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
                        }),
                );
            } else {
                let edit_name = name.clone();
                let undo_name = name.clone();
                container = container.child(
                    ui::input()
                        .key(name.clone())
                        .aria(label)
                        .variant(ui::InputVariant::Field)
                        .readonly(disabled)
                        .text(editor.text.clone())
                        .cursor(editor.cursor)
                        .anchor(editor.anchor)
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
                        }),
                );
            }
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
                selected_column,
                SchemaChoices { open_select, root },
            );
        }
    }
    container.into()
}

#[cfg(test)]
#[path = "../../tests/unit/advanced_schema.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/unit/schema_embedded_regression.rs"]
mod embedded_regression;
