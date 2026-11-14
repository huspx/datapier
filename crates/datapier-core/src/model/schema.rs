//! 结构设计的冻结快照与 DDL 计划；原始定义和完整性不可用时不能推断为空。
use super::ResourceTarget;
use serde::{Deserialize, Serialize};

/// 元数据来源能否证明完整。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CompletenessState {
    Complete,
    Incomplete,
    Unknown,
    Unavailable,
}
/// 完整性及属性级原因。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Completeness {
    pub state: CompletenessState,
    pub reasons: Vec<String>,
}
/// 一项能力的状态和限制原因。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaCapability {
    pub state: String,
    pub reason: Option<String>,
}
/// 数据库对象的完整基线和创建上下文。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSnapshot {
    pub target: ResourceTarget,
    pub server_version: String,
    pub sql_mode: String,
    pub definition: SchemaDefinition,
    pub ddl: String,
    pub dependencies: Vec<String>,
    pub completeness: Completeness,
    pub fingerprint: String,
    pub capabilities: std::collections::BTreeMap<String, SchemaCapability>,
}
/// 不同对象的类型化定义。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SchemaDefinition {
    Database(DatabaseDefinition),
    Table(TableDefinition),
    View(ViewDefinition),
    Trigger(TriggerDefinition),
}
/// 新建数据库的字符集与排序规则；空值使用服务端相应默认值。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DatabaseDefinition {
    pub charset: Option<String>,
    pub collation: Option<String>,
}
/// 默认值种类不能根据文本猜测。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum ColumnDefault {
    Unspecified,
    Null,
    Literal(String),
    Expression(String),
}
/// 生成列的表达式及存储方式。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GeneratedColumn {
    pub expression: String,
    pub stored: bool,
}
/// 字段 ID 在重命名前后保持不变。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ColumnDefinition {
    pub id: String,
    pub name: String,
    pub native_type: String,
    pub nullable: bool,
    pub default: ColumnDefault,
    pub on_update: Option<String>,
    pub auto_increment: bool,
    pub generated: Option<GeneratedColumn>,
    pub charset: Option<String>,
    pub collation: Option<String>,
    pub visible: bool,
    pub comment: String,
    pub extra: Vec<String>,
}
/// 索引类型。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IndexKind {
    Primary,
    Unique,
    Normal,
    Fulltext,
    Spatial,
}
/// 索引一部分，列名与表达式必须恰好指定一个。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexPart {
    pub column: Option<String>,
    pub expression: Option<String>,
    pub prefix_length: Option<u32>,
    pub descending: bool,
}
/// 索引定义，保留无法解释的选项。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct IndexDefinition {
    pub id: String,
    pub name: String,
    pub kind: IndexKind,
    pub parts: Vec<IndexPart>,
    pub visible: bool,
    pub method: Option<String>,
    pub options: Vec<String>,
}
/// 外键和 CHECK 的服务端名称及引用属性。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConstraintDefinition {
    ForeignKey {
        id: String,
        name: String,
        columns: Vec<String>,
        referenced_database: String,
        referenced_table: String,
        referenced_columns: Vec<String>,
        on_update: String,
        on_delete: String,
    },
    Check {
        id: String,
        name: String,
        expression: String,
        enforced: bool,
    },
}
/// 表定义；未知的表选项保留原 token。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableDefinition {
    pub columns: Vec<ColumnDefinition>,
    pub indexes: Vec<IndexDefinition>,
    pub constraints: Vec<ConstraintDefinition>,
    pub engine: String,
    pub charset: Option<String>,
    pub collation: Option<String>,
    pub comment: String,
    pub options: Vec<String>,
}
/// 视图属性和 SELECT 正文。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ViewDefinition {
    pub select: String,
    pub columns: Vec<String>,
    pub algorithm: String,
    pub definer: Option<String>,
    pub security: String,
    pub check_option: Option<String>,
    pub character_set_client: Option<String>,
    pub collation_connection: Option<String>,
}
/// 触发器属性；顺序引用同事件及时机的相邻触发器。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TriggerDefinition {
    pub database: String,
    pub name: String,
    pub table: String,
    pub timing: String,
    pub event: String,
    pub body: String,
    pub definer: Option<String>,
    pub sql_mode: String,
    pub character_set_client: Option<String>,
    pub collation_connection: Option<String>,
    pub order: Option<TriggerOrder>,
}
/// FOLLOWS/PRECEDES 顺序及参照触发器。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TriggerOrder {
    pub follows: bool,
    pub trigger: String,
}
/// 结构变更意图；基线为空表示创建，目标为空表示删除。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaEditIntent {
    pub target: ResourceTarget,
    pub baseline: Option<SchemaSnapshot>,
    pub definition: Option<SchemaDefinition>,
    pub profile_revision: String,
    pub algorithm: Option<String>,
    pub lock: Option<String>,
    #[serde(default)]
    pub server_version: String,
    #[serde(default)]
    pub sql_mode: String,
}
/// 发送每步前需要证明的对象状态。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "fingerprint", rename_all = "snake_case")]
pub enum SchemaPrecondition {
    Absent,
    Fingerprint(String),
}
/// 有序 DDL 及执行前提。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaStep {
    pub sql: String,
    pub precondition: SchemaPrecondition,
    pub destructive_reasons: Vec<String>,
    pub warnings: Vec<String>,
}
/// 预览与提交共用的不可变计划。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaPlan {
    pub intent: SchemaEditIntent,
    pub steps: Vec<SchemaStep>,
    pub plan_digest: String,
}
