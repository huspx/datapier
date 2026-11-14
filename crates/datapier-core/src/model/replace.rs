//! 批量查找替换的条件与范围。
use serde::{Deserialize, Serialize};

/// 冻结目标范围；Redis 只支持已选 key。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReplaceScope {
    Selected,
    CurrentPage,
    Filtered,
}

/// 查找条件和用户明确选择的列或 Redis 角色。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaceSpec {
    pub pattern: String,
    pub replacement: String,
    pub regex: bool,
    pub case_sensitive: bool,
    pub multi_line: bool,
    pub dot_matches_new_line: bool,
    pub scope: ReplaceScope,
    #[serde(default)]
    pub columns: Vec<String>,
    #[serde(default)]
    pub roles: Vec<String>,
}
