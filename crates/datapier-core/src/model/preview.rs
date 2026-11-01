//! 变更预览的稳定响应及展示预算；展示值不能用于执行。
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::Serialize;
use serde_json::Value as Json;

use super::{AppError, Result};

/// 分页后的实际语句或命令预览。
#[derive(Serialize)]
pub struct ChangePreview {
    pub provider: &'static str,
    pub transactional: bool,
    pub total_changes: usize,
    pub offset: usize,
    pub next_offset: Option<usize>,
    pub items: Vec<PreviewItem>,
    pub notes: Vec<String>,
}

/// 一个草稿项的语句；附加命令可以没有草稿下标。
#[derive(Serialize)]
pub struct PreviewItem {
    pub index: Option<usize>,
    pub operation: String,
    pub statements: Vec<PreviewStatement>,
}

/// 执行计划中一条语句及其按实际绑定顺序排列的参数。
#[derive(Serialize)]
pub struct PreviewStatement {
    pub role: &'static str,
    pub text: String,
    pub parameters: Vec<PreviewParameter>,
}

/// 展示值可能截断，byte_len 始终表示实际值的原始字节数。
#[derive(Serialize)]
pub struct PreviewParameter {
    pub kind: &'static str,
    pub display: String,
    pub byte_len: Option<usize>,
    pub complete: bool,
    pub dynamic: bool,
}

/// 全页共享的参数展示预算，限制 JSON 转义后的展示文本长度。
pub struct PreviewBudget {
    remaining: usize,
}
impl Default for PreviewBudget {
    fn default() -> Self {
        Self::new()
    }
}
impl PreviewBudget {
    pub fn new() -> Self {
        Self {
            remaining: 512 * 1024,
        }
    }

    /// text 使用 JSON 字符串显示，bytes 使用 Base64；其他类型显示原始文本。
    pub fn parameter(
        &mut self,
        kind: &'static str,
        bytes: &[u8],
        dynamic: bool,
    ) -> PreviewParameter {
        let limit = self.remaining.min(1024);
        // 只编码可能展示的前缀，避免大字段仅为预览而再次完整分配。
        let (full, input_complete) = match kind {
            "text" => {
                let text = std::str::from_utf8(bytes).expect("text bind is UTF-8");
                let mut end = text.len().min(limit);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                (
                    serde_json::to_string(&text[..end]).expect("string serialization"),
                    end == text.len(),
                )
            }
            "bytes" => {
                let end = bytes.len().min(limit.saturating_add(2) / 4 * 3);
                (STANDARD.encode(&bytes[..end]), end == bytes.len())
            }
            _ => {
                let end = bytes.len().min(limit);
                (
                    String::from_utf8_lossy(&bytes[..end]).into_owned(),
                    end == bytes.len(),
                )
            }
        };
        let complete = input_complete && full.len() <= limit;
        let display = if complete {
            full
        } else {
            let marker = if limit >= 3 {
                "…"
            } else if limit == 2 {
                ".."
            } else if limit == 1 {
                "."
            } else {
                ""
            };
            let mut end = limit.saturating_sub(marker.len()).min(full.len());
            while !full.is_char_boundary(end) {
                end -= 1;
            }
            let mut out = full[..end].to_owned();
            out.push_str(marker);
            out
        };
        self.remaining = self.remaining.saturating_sub(display.len());
        PreviewParameter {
            kind,
            display,
            byte_len: if dynamic { None } else { Some(bytes.len()) },
            complete: complete && !dynamic,
            dynamic,
        }
    }
}

/// 解析分页参数并限制每页草稿项数量。
pub fn preview_page(params: &Json, total: usize) -> Result<(usize, usize, Option<usize>)> {
    let parse = |key: &str, default: usize| -> Result<usize> {
        match params.get(key) {
            None => Ok(default),
            Some(value) => serde_json::from_value(value.clone()).map_err(Into::into),
        }
    };
    let offset = parse("offset", 0)?;
    let limit = parse("limit", 20)?.clamp(1, 20);
    let end = offset.saturating_add(limit).min(total);
    Ok((offset, end, (end < total).then_some(end)))
}

/// 序列化并拒绝超过 IPC 单帧的响应，留出协议封装空间。
pub fn preview_json(preview: ChangePreview) -> Result<Json> {
    let value = serde_json::to_value(preview)?;
    if serde_json::to_vec(&value)?.len() > 8 * 1024 * 1024 - 1024 {
        return Err(AppError::new(
            "result_too_large",
            "变更预览超过 IPC 单帧大小",
        ));
    }
    Ok(value)
}

#[cfg(test)]
#[path = "../../tests/unit/model_preview.rs"]
mod tests;
