//! Core 与调用方共享的连接、类型化值、结果和任务模型。

pub mod preview;

use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use uuid::Uuid;

pub type Result<T, E = AppError> = std::result::Result<T, E>;

/// 可序列化的结构化错误；数据库错误可附带原始数据库错误码。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppError {
    pub code: String,
    pub message: String,
    #[serde(default)]
    pub details: Box<Json>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_code: Option<String>,
}
impl AppError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            details: Box::new(Json::Null),
            database_code: None,
        }
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("invalid_input", message)
    }
    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new("conflict", message)
    }
    pub fn cancelled() -> Self {
        Self::new("cancelled", "操作已取消")
    }
    pub fn unknown(message: impl Into<String>) -> Self {
        Self::new("outcome_unknown", message)
    }
    pub fn with_details(mut self, details: Json) -> Self {
        self.details = Box::new(details);
        self
    }
}
impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for AppError {}
impl From<std::io::Error> for AppError {
    fn from(e: std::io::Error) -> Self {
        Self::new("io", e.to_string())
    }
}
impl From<serde_json::Error> for AppError {
    fn from(e: serde_json::Error) -> Self {
        Self::invalid(e.to_string())
    }
}
impl From<sqlx::Error> for AppError {
    fn from(e: sqlx::Error) -> Self {
        let database_code = e
            .as_database_error()
            .and_then(|d| d.code())
            .map(|s| s.into_owned());
        Self {
            code: "database".into(),
            message: e.to_string(),
            details: Box::new(Json::Null),
            database_code,
        }
    }
}
impl From<redis::RedisError> for AppError {
    fn from(e: redis::RedisError) -> Self {
        Self::new("database", e.to_string())
    }
}

/// 随程序编译的数据库种类。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Mysql,
    Redis,
}
/// 连接级访问策略；写操作应在 Core 内执行检查。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    #[default]
    ReadOnly,
    ReadWrite,
}
/// 连接的 TLS 开关及可选 CA、客户端证书和私钥路径。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TlsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub ca_path: Option<String>,
    #[serde(default)]
    pub client_cert_path: Option<String>,
    #[serde(default)]
    pub client_key_path: Option<String>,
}
/// 应用全局界面语言偏好。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum LanguagePreference {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "en")]
    En,
}

/// 保存在数据目录中、由各工作区共享的应用设置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppSettings {
    #[serde(default)]
    pub language: LanguagePreference,
}

/// 不含密码的连接配置；凭据由存储层单独管理。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: Uuid,
    pub name: String,
    pub provider: ProviderKind,
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub database: Option<String>,
    /// None 显示全部可访问数据库，空列表表示全部隐藏。
    #[serde(default)]
    pub visible_databases: Option<Vec<String>>,
    #[serde(default)]
    pub redis_db: u32,
    #[serde(default)]
    pub access_mode: AccessMode,
    #[serde(default)]
    pub tls: TlsConfig,
}
impl Profile {
    /// 只读连接返回权限错误，供写操作入口统一调用。
    pub fn ensure_write(&self) -> Result<()> {
        if self.access_mode == AccessMode::ReadOnly {
            Err(AppError::new("permission_denied", "连接为只读模式"))
        } else {
            Ok(())
        }
    }
    /// 校验连接名称、主机及非零端口；不验证网络连通性。
    pub fn validate(&self) -> Result<()> {
        if self.host.trim().is_empty() || self.name.trim().is_empty() || self.port == 0 {
            return Err(AppError::invalid("连接名称、地址和端口不可为空"));
        }
        let paths = [
            &self.tls.ca_path,
            &self.tls.client_cert_path,
            &self.tls.client_key_path,
        ];
        if paths
            .iter()
            .any(|p| p.as_ref().is_some_and(|s| s.trim().is_empty()))
        {
            return Err(AppError::invalid("TLS 证书路径不能为空白"));
        }
        if !self.tls.enabled && paths.iter().any(|p| p.is_some()) {
            return Err(AppError::invalid("关闭 TLS 时不能提供证书路径"));
        }
        if self.tls.client_cert_path.is_some() != self.tls.client_key_path.is_some() {
            return Err(AppError::invalid("TLS 客户端证书与私钥必须成对提供"));
        }
        Ok(())
    }
}

/// 可往返的类型化数据值，不能用展示文本替代写回值。
///
/// 整数和小数使用字符串保存精度，二进制使用 Base64；`Null` 与
/// Redis 不存在值 `Missing` 不同。`Large` 引用结果缓存中的序列化值，
/// 结果释放后引用不再有效，内容可通过存储层按需读取。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Value {
    Null,
    Missing,
    Bool(bool),
    Integer(String),
    Unsigned(String),
    Decimal(String),
    Float(String),
    Text(String),
    Bytes(String),
    Date(String),
    Time(String),
    DateTime(String),
    Json(Json),
    Array(Vec<Value>),
    Map(Vec<(Value, Value)>),
    Large {
        id: Uuid,
        original_type: String,
        bytes: u64,
        preview: String,
    },
}
impl Value {
    /// 生成面向界面的展示文本，不保证可作为原值解析或写回。
    pub fn text(&self) -> String {
        match self {
            Self::Null => "NULL".into(),
            Self::Missing => "不存在".into(),
            Self::Bool(b) => b.to_string(),
            Self::Integer(s)
            | Self::Unsigned(s)
            | Self::Decimal(s)
            | Self::Float(s)
            | Self::Text(s)
            | Self::Date(s)
            | Self::Time(s)
            | Self::DateTime(s) => s.clone(),
            Self::Bytes(s) => format!("base64:{s}"),
            Self::Json(v) => v.to_string(),
            Self::Large { preview, .. } => preview.clone(),
            Self::Array(_) | Self::Map(_) => match serde_json::to_string(self) {
                Ok(text) => text,
                Err(error) => format!("<值无法显示：{error}>"),
            },
        }
    }
}
/// 结果或表的列元数据；缺省列可为空且不是生成列或主键。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    pub type_name: String,
    #[serde(default = "default_true")]
    pub nullable: bool,
    #[serde(default)]
    pub generated: bool,
    #[serde(default)]
    pub primary_key: bool,
    #[serde(default)]
    pub auto_increment: bool,
    #[serde(default)]
    pub default_value: Option<String>,
    /// 资源结构中的原始列注释；任意查询结果未提供时为 None。
    #[serde(default)]
    pub comment: Option<String>,
}
fn default_true() -> bool {
    true
}
impl Column {
    pub fn new(name: impl Into<String>, type_name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            type_name: type_name.into(),
            nullable: true,
            generated: false,
            primary_key: false,
            auto_increment: false,
            default_value: None,
            comment: None,
        }
    }
}
/// 资源定位信息；Redis 的原始 key 由 `key_b64` 表达。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceTarget {
    #[serde(default)]
    pub database: Option<String>,
    pub name: String,
    #[serde(default)]
    pub key_b64: Option<String>,
    #[serde(default)]
    pub kind: String,
}
/// 浏览返回的资源及 provider 特有的附加数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub key_b64: Option<String>,
    #[serde(default)]
    pub data: Json,
}
/// 资源浏览页；游标和完成状态由对应 provider 定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourcePage {
    pub resources: Vec<Resource>,
    pub cursor: Option<String>,
    pub complete: bool,
}
/// 表结构、键、DDL 和 provider 特有的结构详情。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableSchema {
    pub target: ResourceTarget,
    pub columns: Vec<Column>,
    pub keys: Vec<Vec<String>>,
    pub ddl: String,
    pub engine: String,
    #[serde(default)]
    pub details: Json,
}
/// 待提交的行变更；`key` 定位行，`before` 表示原值，`values` 表示新值。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RowChange {
    pub operation: String,
    #[serde(default)]
    pub key: BTreeMap<String, Value>,
    #[serde(default)]
    pub before: BTreeMap<String, Value>,
    #[serde(default)]
    pub values: BTreeMap<String, Value>,
}
/// 缓存结果的列、行数和完成状态；未完成结果不能视为全量数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultMeta {
    pub id: Uuid,
    pub execution_id: Uuid,
    pub columns: Vec<Column>,
    pub kind: String,
    pub row_count: u64,
    pub complete: bool,
    #[serde(default)]
    pub summary: Json,
}
/// 缓存结果的一页；`next` 是下一页起始行，缺省表示当前无后续行。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultPage {
    pub result: ResultMeta,
    pub rows: Vec<Vec<Value>>,
    pub start: u64,
    pub next: Option<u64>,
}
/// 创建任务时冻结的连接和资源上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskContext {
    pub connection: String,
    pub provider: ProviderKind,
    pub database: Option<String>,
    pub redis_db: Option<u32>,
    #[serde(default)]
    pub targets: Vec<ResourceTarget>,
}

/// 任务关联结果的轻量状态，不包含列定义或行数据。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResultSummary {
    pub id: Uuid,
    pub row_count: u64,
    pub complete: bool,
    pub kind: String,
    pub column_count: Option<u64>,
    #[serde(default)]
    pub redis_type: Option<String>,
}

/// 持久化任务状态；`sent` 与 `may_write` 共同用于中断后的保守恢复。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRecord {
    pub id: Uuid,
    pub workspace_id: String,
    pub owner: Uuid,
    pub method: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub execution_text_truncated: bool,
    #[serde(default)]
    pub started_at: Option<f64>,
    #[serde(default)]
    pub elapsed_ms: Option<u64>,
    #[serde(default)]
    pub context: Option<TaskContext>,
    #[serde(default)]
    pub created_at: f64,
    pub state: String,
    pub sent: bool,
    pub may_write: bool,
    pub results: Vec<Uuid>,
    #[serde(default)]
    pub result_summaries: Vec<TaskResultSummary>,
    pub summary: Json,
    pub error: Option<AppError>,
}

/// 从 JSON 参数对象读取必填字段，缺失或反序列化失败时返回输入错误。
pub fn param<T: serde::de::DeserializeOwned>(params: &Json, key: &str) -> Result<T> {
    serde_json::from_value(
        params
            .get(key)
            .cloned()
            .ok_or_else(|| AppError::invalid(format!("缺少参数 {key}")))?,
    )
    .map_err(Into::into)
}
/// 已取消时返回取消错误；不表示数据库端操作已撤销。
pub fn ensure_not_cancelled(cancel: &tokio_util::sync::CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(AppError::cancelled())
    } else {
        Ok(())
    }
}
/// 构造执行结果 ID 与受影响行数的摘要。
pub fn summary(results: Vec<Uuid>, affected_rows: u64) -> Json {
    json!({"results":results,"affected_rows":affected_rows})
}
