//! 数据交换的用户意图与有界计划；运行期数据和凭据不属于持久化配置。

use super::{AppError, Column, ProviderKind, ResourceTarget, Result, Value};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};
use uuid::Uuid;

pub const TRANSFER_VERSION: u32 = 1;
pub const MAX_BATCH_ROWS: usize = 500;
pub const MAX_BATCH_BYTES: usize = 1024 * 1024;
pub const MAX_VALUE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PLAN_ITEMS: usize = 10_000;
pub const MAX_PLAN_BYTES: usize = 32 * 1024 * 1024;

/// 最新准备状态，仅用于展示，不属于计划或持久化记录。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PreparationProgress {
    pub phase: PreparationPhase,
    /// 当前输入从 1 开始计数；无当前输入时为 None。
    pub input_index: Option<usize>,
    pub input_count: usize,
    pub indexed_bytes: u64,
    pub input_bytes: Option<u64>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PreparationPhase {
    #[default]
    Connecting,
    Indexing,
    Preflight,
    SqlPreflight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferMode {
    Import,
    Export,
    Copy,
    SqlDump,
    SqlRestore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    #[default]
    Insert,
    Skip,
    Overwrite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConversionPolicy {
    #[default]
    Stop,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReadConsistency {
    #[default]
    Transaction,
    BestEffort,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferFormat {
    #[default]
    Csv,
    Json,
    Sql,
}

/// 项目只保存 connection_alias；profile_id 仅用于本机解析后的意图。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferEndpoint {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile_id: Option<Uuid>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redis_db: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CsvOptions {
    #[serde(default = "utf8")]
    pub encoding: String,
    #[serde(default = "comma")]
    pub delimiter: String,
    #[serde(default = "yes")]
    pub header: bool,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default = "null_marker")]
    pub null_marker: String,
    #[serde(default = "yes")]
    pub backslash_escape: bool,
    #[serde(default)]
    pub binary_columns: Vec<String>,
}
fn utf8() -> String {
    "utf-8".into()
}
fn comma() -> String {
    ",".into()
}
fn null_marker() -> String {
    "\\N".into()
}
fn yes() -> bool {
    true
}
impl Default for CsvOptions {
    fn default() -> Self {
        Self {
            encoding: utf8(),
            delimiter: comma(),
            header: true,
            columns: Vec::new(),
            null_marker: null_marker(),
            backslash_escape: true,
            binary_columns: Vec::new(),
        }
    }
}
impl CsvOptions {
    pub fn validate(&self) -> Result<()> {
        if self.delimiter.len() != 1
            || !self.delimiter.is_ascii()
            || matches!(self.delimiter.as_bytes()[0], 0 | b'\r' | b'\n' | b'"')
        {
            return Err(AppError::invalid("CSV 分隔符必须是一个有效 ASCII 字符"));
        }
        if !matches!(
            self.encoding.to_ascii_lowercase().as_str(),
            "utf-8" | "utf-8-bom" | "utf-16le" | "utf-16be" | "gb18030" | "gbk"
        ) {
            return Err(AppError::invalid("不支持的 CSV 编码"));
        }
        if !self.header && self.columns.is_empty() {
            return Err(AppError::invalid("无表头 CSV 必须明确列顺序"));
        }
        unique_names(&self.columns, "CSV 列")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferInput {
    pub path: PathBuf,
    #[serde(default)]
    pub format: TransferFormat,
    #[serde(default)]
    pub csv: CsvOptions,
    #[serde(default)]
    pub target: Option<ResourceTarget>,
    #[serde(default)]
    pub column_mapping: BTreeMap<String, String>,
    #[serde(default)]
    pub columns: Vec<Column>,
    /// 按输入列名确认默认值语义；未提供默认值的列可省略。
    #[serde(default)]
    pub column_defaults: BTreeMap<String, super::schema::ColumnDefault>,
    #[serde(default)]
    pub sync_schema: bool,
    #[serde(default)]
    pub selected_schema_items: Vec<String>,
    /// 人工续作仅针对已核对输入版本的记录区间，端点包含在区间内。
    #[serde(default)]
    pub record_ranges: Vec<RecordRange>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordRange {
    pub first: u64,
    pub last: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferOutput {
    pub path: PathBuf,
    pub format: TransferFormat,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ColumnMapping {
    pub source: String,
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterOperator {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    Like,
    IsNull,
    IsNotNull,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferFilter {
    pub column: String,
    pub operator: FilterOperator,
    #[serde(default)]
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferObject {
    pub source: ResourceTarget,
    pub target: ResourceTarget,
    #[serde(default)]
    pub columns: Vec<ColumnMapping>,
    /// 显式目标类型意图；空时沿用源列类型，已有表仍须确认结构差异。
    #[serde(default)]
    pub target_columns: Vec<Column>,
    #[serde(default)]
    pub filters: Vec<TransferFilter>,
    #[serde(default)]
    pub locator_key: Vec<String>,
    #[serde(default)]
    pub create_if_missing: bool,
    #[serde(default)]
    pub sync_schema: bool,
    /// 空集合意味着不自动调整已有表；这些稳定 ID 来自结构差异预览。
    #[serde(default)]
    pub selected_schema_items: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedisSelection {
    #[serde(default)]
    pub keys_b64: Vec<String>,
    #[serde(default)]
    pub pattern: Option<String>,
    #[serde(default)]
    pub source_prefix_b64: Option<String>,
    #[serde(default)]
    pub target_prefix_b64: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "accounts")]
pub enum DefinerMapping {
    #[default]
    CurrentUser,
    Preserve,
    Map(BTreeMap<String, String>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferOptions {
    #[serde(default)]
    pub conflict: ConflictPolicy,
    #[serde(default)]
    pub conversion_errors: ConversionPolicy,
    #[serde(default)]
    pub consistency: ReadConsistency,
    #[serde(default = "yes")]
    pub include_schema: bool,
    #[serde(default = "yes")]
    pub include_data: bool,
    #[serde(default)]
    pub create_missing: bool,
    #[serde(default)]
    pub locator_key: Vec<String>,
    #[serde(default)]
    pub definer: DefinerMapping,
    #[serde(default)]
    pub existing_triggers_confirmed: bool,
    #[serde(default = "sample_rows")]
    pub preview_rows: usize,
}
fn sample_rows() -> usize {
    100
}
impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            conflict: ConflictPolicy::Insert,
            conversion_errors: ConversionPolicy::Stop,
            consistency: ReadConsistency::Transaction,
            include_schema: true,
            include_data: true,
            create_missing: false,
            locator_key: Vec::new(),
            definer: DefinerMapping::CurrentUser,
            existing_triggers_confirmed: false,
            preview_rows: sample_rows(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferSpec {
    #[serde(default = "spec_version")]
    pub version: u32,
    pub mode: TransferMode,
    pub provider: ProviderKind,
    #[serde(default)]
    pub source: Option<TransferEndpoint>,
    #[serde(default)]
    pub destination: Option<TransferEndpoint>,
    #[serde(default)]
    pub inputs: Vec<TransferInput>,
    #[serde(default)]
    pub output: Option<TransferOutput>,
    #[serde(default)]
    pub objects: Vec<TransferObject>,
    #[serde(default)]
    pub redis: RedisSelection,
    #[serde(default)]
    pub options: TransferOptions,
}
fn spec_version() -> u32 {
    TRANSFER_VERSION
}
impl TransferSpec {
    pub fn validate(&self) -> Result<()> {
        if self.version != TRANSFER_VERSION {
            return Err(AppError::new("format_version", "不支持的数据交换配置版本"));
        }
        let needs_source = matches!(
            self.mode,
            TransferMode::Copy | TransferMode::Export | TransferMode::SqlDump
        );
        let needs_target = matches!(
            self.mode,
            TransferMode::Copy | TransferMode::Import | TransferMode::SqlRestore
        );
        if needs_source && self.source.is_none() {
            return Err(AppError::invalid("缺少源连接"));
        }
        if needs_target && self.destination.is_none() {
            return Err(AppError::invalid("缺少目标连接"));
        }
        if matches!(self.mode, TransferMode::Import | TransferMode::SqlRestore)
            && self.inputs.is_empty()
        {
            return Err(AppError::invalid("请至少选择一个输入文件"));
        }
        if matches!(self.mode, TransferMode::Export | TransferMode::SqlDump)
            && self.output.is_none()
        {
            return Err(AppError::invalid("缺少输出文件"));
        }
        if matches!(self.mode, TransferMode::SqlDump | TransferMode::SqlRestore)
            && self.provider != ProviderKind::Mysql
        {
            return Err(AppError::invalid("SQL 备份恢复仅用于 MySQL"));
        }
        if !self.options.include_data && !self.options.include_schema {
            return Err(AppError::invalid("至少选择结构或数据"));
        }
        if self.options.preview_rows > 1000 {
            return Err(AppError::invalid("每对象预览最多 1000 条记录"));
        }
        if self.objects.len() > MAX_PLAN_ITEMS || self.inputs.len() > MAX_PLAN_ITEMS {
            return Err(AppError::new("resource_limit", "数据交换对象数超限"));
        }
        for input in &self.inputs {
            if input.path.as_os_str().is_empty() {
                return Err(AppError::invalid("输入文件路径不能为空"));
            }
            if input.format == TransferFormat::Csv {
                input.csv.validate()?;
            }
            if input.format == TransferFormat::Sql && self.mode != TransferMode::SqlRestore {
                return Err(AppError::invalid("SQL 文件必须使用恢复 SQL 流程"));
            }
            if input
                .record_ranges
                .iter()
                .any(|r| r.first == 0 || r.last < r.first)
            {
                return Err(AppError::invalid("输入记录范围无效"));
            }
            let targets: Vec<_> = input.column_mapping.values().cloned().collect();
            unique_names(&targets, "目标列")?;
        }
        for object in &self.objects {
            let source: Vec<_> = object.columns.iter().map(|c| c.source.clone()).collect();
            let target: Vec<_> = object.columns.iter().map(|c| c.target.clone()).collect();
            unique_names(&source, "源列")?;
            unique_names(&target, "目标列")?;
            unique_names(&object.locator_key, "定位键")?;
            for filter in &object.filters {
                let unary = matches!(
                    filter.operator,
                    FilterOperator::IsNull | FilterOperator::IsNotNull
                );
                if filter.column.is_empty() || unary == filter.value.is_some() {
                    return Err(AppError::invalid("过滤条件与参数不匹配"));
                }
                if matches!(
                    filter.value,
                    Some(Value::Large { .. } | Value::Missing | Value::Array(_) | Value::Map(_))
                ) {
                    return Err(AppError::invalid("过滤值必须是完整标量"));
                }
            }
        }
        if self.redis.source_prefix_b64.is_some() != self.redis.target_prefix_b64.is_some() {
            return Err(AppError::invalid("源前缀和目标前缀必须同时指定"));
        }
        if let Some(output) = &self.output
            && output.path.as_os_str().is_empty()
        {
            return Err(AppError::invalid("输出文件路径不能为空"));
        }
        Ok(())
    }
}
fn unique_names(names: &[String], label: &str) -> Result<()> {
    let mut seen = BTreeSet::new();
    for name in names {
        if name.is_empty() || !seen.insert(name) {
            return Err(AppError::invalid(format!("{label}为空或重复")));
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputFingerprint {
    pub path: PathBuf,
    pub bytes: u64,
    pub identity: Option<String>,
    pub modified_ns: Option<String>,
    pub sha256: String,
    pub block_bytes: usize,
    /// 索引只包含输入文件的哈希，不包含解析后的输入值。
    pub block_digests: Vec<String>,
}

/// 报告与人工续作用的输入身份，不包含运行期分块索引或输入记录内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferInputIdentity {
    pub path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub identity: Option<String>,
    pub modified_ns: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferPlanItem {
    pub id: String,
    pub phase: String,
    pub action: String,
    pub source: Option<ResourceTarget>,
    pub target: Option<ResourceTarget>,
    pub sql: Option<String>,
    pub destructive: bool,
    pub selected: bool,
    pub precondition: Option<String>,
    pub notes: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferSample {
    pub object: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    pub sampled: bool,
    pub errors: Vec<TransferDiagnostic>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferDiagnostic {
    pub code: String,
    pub message: String,
    pub object: Option<String>,
    pub input: Option<usize>,
    pub record: Option<u64>,
    pub byte_offset: Option<u64>,
    pub blocking: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreparedTransfer {
    pub id: Uuid,
    pub revision: u64,
    pub state: String,
    pub digest: String,
    #[serde(default)]
    pub retry_of: Option<Uuid>,
    /// 空列表表示重试 failed、rolled_back 和 not_sent 状态。
    #[serde(default)]
    pub retry_states: Vec<TransferStepState>,
    pub spec: TransferSpec,
    pub items: Vec<TransferPlanItem>,
    pub samples: Vec<TransferSample>,
    pub diagnostics: Vec<TransferDiagnostic>,
    pub inputs: Vec<InputFingerprint>,
    #[serde(default)]
    pub source_schema_fingerprints: BTreeMap<usize, String>,
    #[serde(default)]
    pub source_identity: Option<String>,
    #[serde(default)]
    pub destination_identity: Option<String>,
    pub profile_digests: BTreeMap<Uuid, String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TransferProgress {
    pub read_rows: u64,
    pub read_keys: u64,
    pub written_rows: u64,
    pub written_keys: u64,
    pub skipped: u64,
    pub failed: u64,
    pub rolled_back: u64,
    pub outcome_unknown: u64,
    pub processed_bytes: u64,
    pub total_bytes: Option<u64>,
    pub current_object: Option<String>,
    pub phase: String,
    /// 已计入进度的步骤数量，即下一个步骤序号。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_ordinal: Option<u64>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferStepState {
    NotSent,
    Sent,
    Applied,
    RolledBack,
    Failed,
    Skipped,
    OutcomeUnknown,
}
impl TransferStepState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotSent => "not_sent",
            Self::Sent => "sent",
            Self::Applied => "applied",
            Self::RolledBack => "rolled_back",
            Self::Failed => "failed",
            Self::Skipped => "skipped",
            Self::OutcomeUnknown => "outcome_unknown",
        }
    }
}
/// 日志定位白名单：保留执行范围和摘要，绝不携带数据库行值或完整 key。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransferLocator {
    pub object: Option<String>,
    pub input: Option<usize>,
    pub first_record: Option<u64>,
    pub last_record: Option<u64>,
    pub byte_start: Option<u64>,
    pub byte_end: Option<u64>,
    /// 每个元组在源文件中的起始字节位置，按递增顺序保存，不包含数据值。
    #[serde(default)]
    pub record_offsets: Vec<u64>,
    pub source_digest: Option<String>,
    pub target_digest: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferStepOutcome {
    pub ordinal: u64,
    pub locator: TransferLocator,
    pub phase: String,
    pub state: TransferStepState,
    pub digest: String,
    pub affected: u64,
    pub error_code: Option<String>,
    pub database_code: Option<String>,
    #[serde(default)]
    pub item_states: Vec<TransferStepState>,
    #[serde(default)]
    pub error_record: Option<u64>,
}
