//! 编辑限制、外部编辑器配置与持久化文件登记模型。
use super::{AppError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;
use uuid::Uuid;

/// 增量估算 JSON 树、解析缓冲及编码临时空间；调用方另计输入缓冲。
#[derive(Debug, Clone, Copy, Default)]
pub struct JsonMemoryEstimate {
    quoted: bool,
    escaped: bool,
    nodes: usize,
    bytes: usize,
}

impl JsonMemoryEstimate {
    /// 可在任意字节边界分段输入，字符串转义状态跨分段保留。
    pub fn feed(&mut self, bytes: &[u8]) {
        self.bytes = self.bytes.saturating_add(bytes.len());
        for &byte in bytes {
            if self.quoted {
                if self.escaped {
                    self.escaped = false;
                } else if byte == b'\\' {
                    self.escaped = true;
                } else if byte == b'"' {
                    self.quoted = false;
                }
            } else if byte == b'"' {
                self.quoted = true;
                self.nodes = self.nodes.saturating_add(1);
            } else if matches!(byte, b'{' | b'}' | b'[' | b']' | b',' | b':') {
                self.nodes = self.nodes.saturating_add(1);
            }
        }
    }

    /// 不含输入字节本身的保守堆空间上界。
    pub fn heap_bound(&self) -> usize {
        // 解析、规范化和参数绑定可能并存多份树，预留容器扩容及 Value 槽位。
        let node_bytes = std::mem::size_of::<Json>().saturating_mul(8);
        self.bytes
            .saturating_mul(3)
            .saturating_add(self.nodes.saturating_mul(node_bytes))
    }
}

/// 用户可调整的编辑资源上限，仍受客户端和协议预算约束。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct EditingLimits {
    pub max_plan_items: usize,
    pub max_plan_bytes: usize,
    pub max_value_bytes: usize,
    pub max_text_editor_bytes: usize,
    pub preview_rows: usize,
}
impl Default for EditingLimits {
    fn default() -> Self {
        Self {
            max_plan_items: 10_000,
            max_plan_bytes: 32 * 1024 * 1024,
            max_value_bytes: 16 * 1024 * 1024,
            max_text_editor_bytes: 1024 * 1024,
            preview_rows: 100,
        }
    }
}
impl EditingLimits {
    /// 检查正值以及文本编辑器不能大于单值上限。
    pub fn validate(&self) -> Result<()> {
        if self.max_plan_items == 0
            || self.max_plan_bytes == 0
            || self.max_value_bytes == 0
            || self.max_text_editor_bytes == 0
            || self.preview_rows == 0
            || self.max_text_editor_bytes > self.max_value_bytes
        {
            return Err(AppError::invalid(
                "编辑限制必须为正，文本编辑上限不能超过单值上限",
            ));
        }
        Ok(())
    }
}

/// 外部程序运行方式。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExternalEditorMode {
    Gui,
    Terminal,
}

/// 编辑器使用原生 argv，不经过 shell 解析。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExternalEditorConfig {
    pub mode: ExternalEditorMode,
    pub program: String,
    pub args: Vec<String>,
}
impl ExternalEditorConfig {
    /// 文件占位符在所有参数中必须恰好出现一次。
    pub fn validate(&self) -> Result<()> {
        if self.program.trim().is_empty()
            || self.program.contains('\0')
            || self.args.iter().any(|s| s.contains('\0'))
            || self
                .args
                .iter()
                .map(|s| s.matches("{file}").count())
                .sum::<usize>()
                != 1
        {
            return Err(AppError::invalid(
                "编辑器程序不能为空，参数中必须恰有一个 {file}",
            ));
        }
        Ok(())
    }
    /// 将受控文件路径替换到 argv 中。
    pub fn arguments(&self, file: &std::path::Path) -> Result<Vec<String>> {
        self.validate()?;
        if !file.is_absolute() {
            return Err(AppError::invalid("编辑文件路径必须是绝对路径"));
        }
        let file = file
            .to_str()
            .ok_or_else(|| AppError::invalid("编辑文件路径不是有效 UTF-8"))?;
        Ok(self
            .args
            .iter()
            .map(|arg| arg.replace("{file}", file))
            .collect())
    }
}

/// 文件用途决定可否原位修改及工作区引用规则。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EditFilePurpose {
    External,
    Draft,
}
/// 文件登记状态；删除后不保留记录。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EditFileState {
    Preparing,
    Ready,
    Opened,
    Returned,
    Orphaned,
    CleanupPending,
}

/// 登记时由调用方提供的归属和内容描述；文件身份与路径仍由 Store 生成。
pub struct EditFileRegistration<'a> {
    pub workspace_id: &'a str,
    pub profile_id: &'a str,
    pub purpose: EditFilePurpose,
    pub target: Json,
    pub format: &'a str,
    pub baseline_digest: &'a str,
    pub draft_revision: u64,
}
/// 不包含用户文件内容的持久登记。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EditorFileRecord {
    pub id: Uuid,
    pub workspace_id: String,
    pub profile_id: String,
    pub relative_dir: String,
    pub purpose: EditFilePurpose,
    pub target: Json,
    pub state: EditFileState,
    pub format: String,
    pub baseline_digest: String,
    /// 关联编辑代次的 1 起始表示；Engine 在运行期 0 起始修订的登记边界转换。
    pub draft_revision: u64,
    pub revision: u64,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}
