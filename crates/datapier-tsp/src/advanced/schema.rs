//! 结构设计的本地意图、表单和异步请求；持久化只包含用户操作。
use crate::{Editor, i18n::tr};
use datapier_core::model::{
    ResourceTarget,
    schema::{SchemaDefinition, SchemaSnapshot},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[path = "schema_primary.rs"]
mod schema_primary;
#[path = "schema_reference.rs"]
pub(crate) mod schema_reference;

mod form;
mod object_grid;
mod view;

pub(crate) use view::schema_tabs;

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

#[cfg(test)]
#[path = "../../tests/unit/advanced_schema.rs"]
mod tests;

#[cfg(test)]
#[path = "../../tests/unit/schema_embedded_regression.rs"]
mod embedded_regression;
