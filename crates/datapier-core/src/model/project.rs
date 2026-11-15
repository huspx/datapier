//! Git 项目文件及发布恢复的公开数据模型。

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use uuid::Uuid;

/// 项目唯一共享清单，不包含本机绑定和运行状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectManifest {
    pub format: String,
    pub version: u32,
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub connections: Vec<ProjectConnection>,
    #[serde(default)]
    pub queries: Vec<ProjectQuery>,
    #[serde(default)]
    pub transfers: Vec<ProjectTransfer>,
    #[serde(default)]
    pub layout: ProjectLayout,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<Value>,
}

/// 仅描述共享逻辑需求，实际 profile 和凭据属于本机。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectConnection {
    pub alias: String,
    pub provider: String,
    pub source_kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<Value>,
    #[serde(default)]
    pub description: String,
}

/// 查询正文由 path 指向的独立文件承载。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectQuery {
    pub id: Uuid,
    pub name: String,
    pub path: String,
    pub language: String,
    pub connection_alias: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<Value>,
}

/// 迁移配置正文是独立 JSON 文件。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectTransfer {
    pub id: Uuid,
    pub name: String,
    pub path: String,
}

/// 共享标签与列设置只允许引用稳定对象 ID。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectLayout {
    #[serde(default)]
    pub tabs: Vec<Uuid>,
    #[serde(default)]
    pub resources: Vec<Uuid>,
    #[serde(default)]
    pub splits: Vec<Vec<Uuid>>,
    #[serde(default)]
    pub columns: BTreeMap<Uuid, Vec<String>>,
}

/// 一次打开捕获的完整共享文件与摘要；仅含用户明确保存的文本。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    pub root: String,
    pub manifest: ProjectManifest,
    pub files: BTreeMap<String, String>,
    pub digests: BTreeMap<String, String>,
}

/// 预览包含完整新旧内容，调用方应分页展示并提示敏感文本。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectFileChange {
    pub path: String,
    pub old_content: Option<String>,
    pub new_content: Option<String>,
    pub old_digest: Option<String>,
    pub new_digest: Option<String>,
}

/// 提交时校验 digest 和每个原文件摘要。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSavePreview {
    pub project_id: Uuid,
    pub root: String,
    pub baseline: BTreeMap<String, String>,
    pub changes: Vec<ProjectFileChange>,
    pub digest: String,
}

/// 持久化登记使用的 stage 身份；目录位于调用方提供的私有 staging 根内。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectStageIdentity {
    pub id: Uuid,
    pub project_id: Uuid,
    pub root: String,
    pub stage_dir: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectStageState {
    Preparing,
    Ready,
    Published,
    RecoveryConflict,
}

/// 每项记录在发布前就固定旧新摘要，避免中断后猜测用户意图。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectStage {
    pub identity: ProjectStageIdentity,
    pub state: ProjectStageState,
    pub preview: ProjectSavePreview,
    pub applied: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ProjectRecoveryAction {
    Continue,
    Rollback,
}

/// 归档导入的可登记身份；发布前必须登记并保持 staging 目录。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectImportStage {
    pub id: Uuid,
    pub project_id: Uuid,
    pub stage_dir: String,
    pub destination: String,
    pub revision: u64,
    pub digests: BTreeMap<String, String>,
    #[serde(default)]
    pub candidates: Vec<String>,
}
