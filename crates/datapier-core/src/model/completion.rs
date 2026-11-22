use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// 编辑器目录读取请求；只包含目标和前缀，不传入 SQL 文本。
#[derive(Clone, Debug, Deserialize)]
pub struct CompletionRequest {
    pub session_id: Uuid,
    pub editor_id: Uuid,
    pub request_id: Value,
    pub context_generation: u64,
    pub scope: CompletionScope,
    #[serde(default)]
    pub database: Option<String>,
    #[serde(default)]
    pub relation: Option<String>,
    #[serde(default)]
    pub prefix: String,
    #[serde(default)]
    pub cursor: Option<String>,
    pub limit: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum CompletionScope {
    Databases,
    Relations,
    Columns,
}

/// 名称始终保留上游原文；不存在的类型和注释不凭猜测补齐。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompletionItem {
    pub kind: String,
    pub database: Option<String>,
    pub relation: Option<String>,
    pub name: String,
    pub data_type: Option<String>,
    pub comment: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CompletionPage {
    pub items: Vec<CompletionItem>,
    pub completeness: String,
    pub next_cursor: Option<String>,
    pub reason: Option<String>,
}
