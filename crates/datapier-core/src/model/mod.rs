//! Core 与调用方共享的连接、类型化值、结果和任务模型。

pub mod editing;
pub mod preview;
pub mod replace;
pub mod schema;

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct AppSettings {
    pub revision: u64,
    pub language: LanguagePreference,
    #[serde(default)]
    pub editing: editing::EditingLimits,
    #[serde(default)]
    pub external_editor: Option<editing::ExternalEditorConfig>,
    pub mcp: McpSettings,
    pub paging: PagingSettings,
    pub archery: ArcherySettings,
    pub limits: LimitSettings,
    pub approval: ApprovalSettings,
    pub notifications: NotificationSettings,
    pub history: HistorySettings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct McpSettings {
    pub fine_grained_access: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PagingSettings {
    pub mysql_table_rows: usize,
    pub sql_rows: usize,
    pub redis_value_rows: usize,
    pub history_rows: usize,
    pub archery_grid_rows: usize,
    pub redis_key_batch: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ArcherySettings {
    pub request_rows: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct LimitSettings {
    pub mcp_response_rows: usize,
    pub response_bytes: usize,
    pub value_preview_bytes: usize,
    pub connect_timeout_seconds: u64,
    pub query_timeout_seconds: u64,
    pub count_timeout_seconds: u64,
    pub result_idle_seconds: u64,
    pub result_memory_bytes_per_client: usize,
    pub running_tasks_per_mcp_client: usize,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    Off,
    #[default]
    Dangerous,
    AllWrites,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct ApprovalSettings {
    pub ttl_seconds: u64,
    pub default_policy: ApprovalPolicy,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NotificationSettings {
    pub approval: bool,
    pub authentication: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(default)]
pub struct HistorySettings {
    pub auto_cleanup_enabled: bool,
    pub retention_days: Option<u64>,
    pub max_entries: Option<u64>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            revision: 1,
            editing: editing::EditingLimits::default(),
            external_editor: None,
            language: LanguagePreference::default(),
            mcp: McpSettings::default(),
            paging: PagingSettings::default(),
            archery: ArcherySettings::default(),
            limits: LimitSettings::default(),
            approval: ApprovalSettings::default(),
            notifications: NotificationSettings::default(),
            history: HistorySettings::default(),
        }
    }
}
impl Default for PagingSettings {
    fn default() -> Self {
        Self {
            mysql_table_rows: 100,
            sql_rows: 100,
            redis_value_rows: 100,
            history_rows: 20,
            archery_grid_rows: 100,
            redis_key_batch: 1000,
        }
    }
}
impl Default for ArcherySettings {
    fn default() -> Self {
        Self { request_rows: 100 }
    }
}
impl Default for LimitSettings {
    fn default() -> Self {
        Self {
            mcp_response_rows: 1000,
            response_bytes: 1_048_576,
            value_preview_bytes: 4096,
            connect_timeout_seconds: 10,
            query_timeout_seconds: 30,
            count_timeout_seconds: 10,
            result_idle_seconds: 300,
            result_memory_bytes_per_client: 67_108_864,
            running_tasks_per_mcp_client: 4,
        }
    }
}
impl Default for ApprovalSettings {
    fn default() -> Self {
        Self {
            ttl_seconds: 600,
            default_policy: ApprovalPolicy::Dangerous,
        }
    }
}
impl Default for NotificationSettings {
    fn default() -> Self {
        Self {
            approval: true,
            authentication: true,
        }
    }
}
impl AppSettings {
    pub fn validate(&self) -> Result<()> {
        self.editing.validate()?;
        if let Some(editor) = &self.external_editor {
            editor.validate()?;
        }
        let p = &self.paging;
        let l = &self.limits;
        if self.revision == 0
            || [
                p.mysql_table_rows,
                p.sql_rows,
                p.redis_value_rows,
                p.history_rows,
                p.archery_grid_rows,
                p.redis_key_batch,
                self.archery.request_rows,
                l.mcp_response_rows,
                l.response_bytes,
                l.value_preview_bytes,
                l.result_memory_bytes_per_client,
                l.running_tasks_per_mcp_client,
            ]
            .contains(&0)
            || [
                l.connect_timeout_seconds,
                l.query_timeout_seconds,
                l.count_timeout_seconds,
                l.result_idle_seconds,
                self.approval.ttl_seconds,
            ]
            .contains(&0)
            || self.history.retention_days == Some(0)
            || self.history.max_entries == Some(0)
            || l.response_bytes < 1024
            || l.response_bytes > 1_048_576
            || l.value_preview_bytes > l.response_bytes
            || l.result_memory_bytes_per_client < l.response_bytes
            || l.mcp_response_rows > 1000
            || [
                p.mysql_table_rows,
                p.sql_rows,
                p.redis_value_rows,
                p.history_rows,
                p.archery_grid_rows,
            ]
            .iter()
            .any(|n| *n > 1000)
        {
            return Err(AppError::invalid("设置超出允许范围"));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Tui,
    Tsp,
    Mcp,
    WindowBridge,
    ValueEditor,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpAccessLevel {
    Disabled,
    ReadOnly,
    ReadWrite,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpAccessRule {
    pub profile_id: Uuid,
    pub scope_kind: String,
    pub scope_key: String,
    pub level: McpAccessLevel,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseDiscovery {
    pub state: String,
    pub source: String,
    pub databases: Vec<String>,
    pub configured_count: Option<u32>,
    pub reason: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub id: Uuid,
    pub execution_id: Uuid,
    pub owner: Uuid,
    pub session_id: Uuid,
    pub profile_id: Uuid,
    pub request: Json,
    pub digest: String,
    pub profile_revision: u64,
    pub policy_revision: u64,
    pub created_at: i64,
    pub expires_at: i64,
    pub state: String,
    pub decided_at: Option<i64>,
    pub decision_source: Option<String>,
}
fn initial_revision() -> u64 {
    1
}

/// 不含密码的连接配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Profile {
    pub id: Uuid,
    pub name: String,
    #[serde(default = "initial_revision")]
    pub revision: u64,
    #[serde(default)]
    pub approval_policy: ApprovalPolicy,
    pub provider: ProviderKind,
    #[serde(default)]
    pub access_mode: AccessMode,
    pub connection: ConnectionConfig,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConnectionConfig {
    Direct(DirectConnection),
    Archery(ArcheryConnection),
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectConnection {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub visible_databases: Option<Vec<String>>,
    #[serde(default)]
    pub redis_db: u32,
    #[serde(default)]
    pub redis_databases_override: Option<u32>,
    #[serde(default)]
    pub tls: TlsConfig,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArcheryConnection {
    pub server_id: Uuid,
    pub instance_id: i64,
    pub instance_name: String,
    pub databases: Vec<String>,
    pub default_database: String,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct HttpTls {
    #[serde(default)]
    pub ca_path: Option<String>,
}
/// 验证码由用户输入，或由本机保存的 TOTP 配置生成。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum TotpMode {
    #[default]
    Manual,
    Automatic,
}

/// 独立凭据的明确更新意图；保留和清除不接受额外字段。
#[derive(Debug, Clone, Serialize, Default)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CredentialUpdate {
    #[default]
    Keep,
    Replace {
        value: String,
        persist: bool,
    },
    Clear,
}

impl<'de> Deserialize<'de> for CredentialUpdate {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        // 空结构体变体也必须执行未知字段检查，避免 unit 变体忽略多余参数。
        #[derive(Deserialize)]
        #[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
        enum Wire {
            Keep {},
            Replace { value: String, persist: bool },
            Clear {},
        }
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Keep {} => Self::Keep,
            Wire::Replace { value, persist } => Self::Replace { value, persist },
            Wire::Clear {} => Self::Clear,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArcheryServer {
    pub id: Uuid,
    pub name: String,
    pub base_url: String,
    pub username: String,
    #[serde(default)]
    pub http_tls: HttpTls,
    #[serde(default)]
    pub totp_mode: TotpMode,
    pub revision: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArcheryInstance {
    pub id: i64,
    pub name: String,
    pub provider: ProviderKind,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CredentialMode {
    Unknown,
    Session,
    Keyring,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArcheryServerInfo {
    #[serde(flatten)]
    pub server: ArcheryServer,
    pub credential_mode: CredentialMode,
    pub totp_credential_mode: CredentialMode,
    pub totp_configured: bool,
    pub connection_count: u64,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Unsupported,
    Unknown,
    Available,
    Denied,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    pub state: CapabilityState,
    pub reason: Option<String>,
}
impl Capability {
    pub fn is_supported(&self) -> bool {
        self.state != CapabilityState::Unsupported
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub query: Capability,
    pub browse: Capability,
    pub structure: Capability,
    pub edit: Capability,
    pub import: Capability,
    pub result_export: Capability,
    pub database_export: Capability,
    pub transactions: Capability,
    pub platform_history: Capability,
    pub sql_check: Capability,
    pub stop_waiting: Capability,
    pub server_cancel: Capability,
}
impl Capabilities {
    pub fn ensure_method(&self, method: &str, params: &Json) -> Result<()> {
        let c = match method {
            "execution.start" | "execution.run" => &self.query,
            "resource.list" | "resource.browse" => &self.browse,
            "resource.describe" => &self.structure,
            "transfer.export" if params.get("result_id").is_some_and(|v| !v.is_null()) => {
                &self.result_export
            }
            "transfer.export" => &self.database_export,
            "transfer.import" | "transfer.preview" => &self.import,
            "archery.query_history" => &self.platform_history,
            "archery.sql_check" => &self.sql_check,
            m if m.starts_with("change.") => &self.edit,
            m if m.starts_with("transaction.") => &self.transactions,
            _ => return Ok(()),
        };
        if c.is_supported() {
            Ok(())
        } else {
            Err(AppError::new(
                "capability_unavailable",
                c.reason
                    .clone()
                    .unwrap_or_else(|| "此连接不支持该操作".into()),
            ))
        }
    }
    /// 仅记录已观察结果；拒绝后仍允许用户显式重试，不能把本地错误当平台拒绝。
    pub fn observe(&mut self, method: &str, result: &Result<Json>) {
        let capability = match method {
            "execution.start" | "execution.run" => &mut self.query,
            "resource.list" | "resource.browse" => &mut self.browse,
            "resource.describe" => &mut self.structure,
            "archery.sql_check" => &mut self.sql_check,
            "archery.query_history" => &mut self.platform_history,
            _ => return,
        };
        match result {
            Ok(_) => {
                capability.state = CapabilityState::Available;
                capability.reason = None;
            }
            Err(error)
                if error.code == "permission_denied"
                    && error
                        .details
                        .get("http_status")
                        .or_else(|| error.details.pointer("/cause/http_status"))
                        .and_then(Json::as_u64)
                        == Some(403) =>
            {
                capability.state = CapabilityState::Denied;
                capability.reason = Some(error.message.clone());
            }
            _ => (),
        }
    }
}
impl Profile {
    /// 只读连接返回权限错误，供写操作入口统一调用。
    pub fn ensure_write(&self) -> Result<()> {
        if self.is_archery() || self.access_mode == AccessMode::ReadOnly {
            Err(AppError::new("permission_denied", "连接为只读模式"))
        } else {
            Ok(())
        }
    }
    pub fn direct(&self) -> Result<&DirectConnection> {
        match &self.connection {
            ConnectionConfig::Direct(d) => Ok(d),
            _ => Err(AppError::invalid("连接不是直连")),
        }
    }
    pub fn direct_mut(&mut self) -> Result<&mut DirectConnection> {
        match &mut self.connection {
            ConnectionConfig::Direct(d) => Ok(d),
            _ => Err(AppError::invalid("连接不是直连")),
        }
    }
    pub fn archery(&self) -> Option<&ArcheryConnection> {
        match &self.connection {
            ConnectionConfig::Archery(a) => Some(a),
            _ => None,
        }
    }
    pub fn is_archery(&self) -> bool {
        self.archery().is_some()
    }
    pub fn default_database(&self) -> Option<&str> {
        match &self.connection {
            ConnectionConfig::Direct(d) => d.database.as_deref(),
            ConnectionConfig::Archery(a) => Some(&a.default_database),
        }
    }
    pub fn redis_db(&self) -> u32 {
        match &self.connection {
            ConnectionConfig::Direct(d) => d.redis_db,
            ConnectionConfig::Archery(a) => a.default_database.parse().unwrap_or(0),
        }
    }
    pub fn capabilities(&self) -> Capabilities {
        let cap = |state| Capability {
            state,
            reason: None,
        };
        let a = self.is_archery();
        let yes = cap(CapabilityState::Available);
        let remote = cap(CapabilityState::Unknown);
        let no = cap(CapabilityState::Unsupported);
        let read = if a { remote.clone() } else { yes.clone() };
        let write = if a || self.access_mode == AccessMode::ReadOnly {
            no.clone()
        } else {
            yes.clone()
        };
        Capabilities {
            query: read.clone(),
            browse: if a && self.provider == ProviderKind::Redis {
                no.clone()
            } else {
                read.clone()
            },
            structure: if a && self.provider == ProviderKind::Redis {
                no.clone()
            } else {
                read
            },
            edit: write.clone(),
            import: write,
            result_export: yes.clone(),
            database_export: if a { no.clone() } else { yes.clone() },
            transactions: if a { no.clone() } else { yes.clone() },
            platform_history: if a { remote.clone() } else { no.clone() },
            sql_check: if a && self.provider == ProviderKind::Mysql {
                remote
            } else {
                no.clone()
            },
            stop_waiting: yes.clone(),
            server_cancel: if !a && self.provider == ProviderKind::Mysql {
                yes
            } else {
                no
            },
        }
    }
    /// 校验来源对应的配置及选定目标。
    pub fn validate(&self) -> Result<()> {
        if self.id.is_nil() || self.revision == 0 || self.name.trim().is_empty() {
            return Err(AppError::invalid("连接名称不能为空"));
        }
        let d = match &self.connection {
            ConnectionConfig::Direct(d) => d,
            ConnectionConfig::Archery(a) => {
                let mut seen = std::collections::HashSet::new();
                if self.access_mode != AccessMode::ReadOnly
                    || a.instance_name.trim().is_empty()
                    || a.databases.is_empty()
                    || !a.databases.contains(&a.default_database)
                    || a.databases.iter().any(|db| {
                        db.trim().is_empty()
                            || !seen.insert(db)
                            || (self.provider == ProviderKind::Redis
                                && db.parse::<u32>().ok().is_none_or(|n| n.to_string() != *db))
                    })
                {
                    return Err(AppError::invalid("Archery 连接或数据库选择无效"));
                }
                return Ok(());
            }
        };
        if d.redis_databases_override == Some(0) {
            return Err(AppError::invalid("Redis 数据库数量必须为正数"));
        }
        if d.host.trim().is_empty() || d.port == 0 {
            return Err(AppError::invalid("连接地址和端口不可为空"));
        }
        let paths = [
            &d.tls.ca_path,
            &d.tls.client_cert_path,
            &d.tls.client_key_path,
        ];
        if paths
            .iter()
            .any(|p| p.as_ref().is_some_and(|s| s.trim().is_empty()))
        {
            return Err(AppError::invalid("TLS 证书路径不能为空白"));
        }
        if !d.tls.enabled && paths.iter().any(|p| p.is_some()) {
            return Err(AppError::invalid("关闭 TLS 时不能提供证书路径"));
        }
        if d.tls.client_cert_path.is_some() != d.tls.client_key_path.is_some() {
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
/// 当前缓存行按指定预算划分的页起点及完成状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResultPagination {
    pub starts: Vec<u64>,
    pub row_count: u64,
    pub complete: bool,
    pub page_size: u64,
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
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub server_id: Option<Uuid>,
    #[serde(default)]
    pub instance_id: Option<i64>,
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
