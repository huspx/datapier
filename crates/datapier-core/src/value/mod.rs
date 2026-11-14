//! 类型化单值的字节缓冲、结构化文本和离线媒体预览。
//!
//! 此模块不持有数据库会话，也不执行文件或宿主缓存操作。调用者负责会话归属、
//! 总预算、撤销历史以及后台任务的生命周期。

mod media;
mod structured;

pub use media::{Preview, preview_image};
pub use structured::{format_json, format_xml, validate_json, validate_xml};

use crate::model::{AppError, Result, Value};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::ops::Range;

/// 可编辑的原始类型；NULL 与不存在值不携带字节。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueKind {
    Null,
    Missing,
    Bool,
    Integer,
    Unsigned,
    Decimal,
    Float,
    Text,
    Bytes,
    Date,
    Time,
    DateTime,
    Json,
}

/// 将完整类型化值转换为原始内容，不使用展示文本。
pub fn to_bytes(value: &Value) -> Result<(ValueKind, Option<Vec<u8>>)> {
    use Value::*;
    let (kind, data) = match value {
        Null => (ValueKind::Null, None),
        Missing => (ValueKind::Missing, None),
        Bool(v) => (ValueKind::Bool, Some(v.to_string().into_bytes())),
        Integer(v) => (ValueKind::Integer, Some(v.as_bytes().to_vec())),
        Unsigned(v) => (ValueKind::Unsigned, Some(v.as_bytes().to_vec())),
        Decimal(v) => (ValueKind::Decimal, Some(v.as_bytes().to_vec())),
        Float(v) => (ValueKind::Float, Some(v.as_bytes().to_vec())),
        Text(v) => (ValueKind::Text, Some(v.as_bytes().to_vec())),
        Bytes(v) => (
            ValueKind::Bytes,
            Some(
                STANDARD
                    .decode(v)
                    .map_err(|e| AppError::invalid(format!("无效 Base64: {e}")))?,
            ),
        ),
        Date(v) => (ValueKind::Date, Some(v.as_bytes().to_vec())),
        Time(v) => (ValueKind::Time, Some(v.as_bytes().to_vec())),
        DateTime(v) => (ValueKind::DateTime, Some(v.as_bytes().to_vec())),
        Json(v) => (ValueKind::Json, Some(serde_json::to_vec(v)?)),
        Array(_) | Map(_) | Large { .. } => {
            return Err(AppError::new(
                "unsupported_type",
                "该值不是可直接编辑的完整单值",
            ));
        }
    };
    Ok((kind, data))
}

/// 按原始类型从字节恢复值；没有字节只允许用于 NULL 或 Missing。
pub fn from_bytes(kind: ValueKind, bytes: Option<&[u8]>) -> Result<Value> {
    use ValueKind::*;
    if matches!(kind, Null | Missing) {
        if bytes.is_some() {
            return Err(AppError::invalid("NULL 或不存在值不能携带字节"));
        }
        return Ok(if kind == Null {
            Value::Null
        } else {
            Value::Missing
        });
    }
    let bytes = bytes.ok_or_else(|| AppError::invalid("非 NULL 值需要字节内容"))?;
    if kind == Bytes {
        return Ok(Value::Bytes(STANDARD.encode(bytes)));
    }
    let text = std::str::from_utf8(bytes)
        .map_err(|e| AppError::invalid(format!("内容不是 UTF-8: {e}")))?;
    let valid_integer = |s: &str, signed: bool| {
        let digits = if signed {
            s.strip_prefix('-').unwrap_or(s)
        } else {
            s
        };
        !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
    };
    // MySQL TIME 是可为负且超过 24 小时的持续时间，不是时钟时间。
    let valid_time = |s: &str| {
        let s = s.strip_prefix('-').unwrap_or(s);
        let mut parts = s.split(':');
        let (Some(h), Some(m), Some(sec), None) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            return false;
        };
        let (whole, fraction) = sec
            .split_once('.')
            .map_or((sec, None), |(a, b)| (a, Some(b)));
        !h.is_empty()
            && h.bytes().all(|b| b.is_ascii_digit())
            && h.parse::<u16>().is_ok_and(|v| v <= 838)
            && m.len() == 2
            && m.parse::<u8>().is_ok_and(|v| v < 60)
            && whole.len() == 2
            && whole.parse::<u8>().is_ok_and(|v| v < 60)
            && fraction.is_none_or(|v| {
                !v.is_empty() && v.len() <= 6 && v.bytes().all(|b| b.is_ascii_digit())
            })
    };
    Ok(match kind {
        Bool => Value::Bool(match text {
            "true" => true,
            "false" => false,
            _ => return Err(AppError::invalid("布尔值必须为 true 或 false")),
        }),
        Integer if valid_integer(text, true) => Value::Integer(text.into()),
        Unsigned if valid_integer(text, false) => Value::Unsigned(text.into()),
        Decimal if text.parse::<bigdecimal::BigDecimal>().is_ok() => Value::Decimal(text.into()),
        Float if text.parse::<f64>().is_ok_and(|v| v.is_finite()) => Value::Float(text.into()),
        Text => Value::Text(text.into()),
        Date if chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").is_ok() => {
            Value::Date(text.into())
        }
        Time if valid_time(text) => Value::Time(text.into()),
        DateTime if chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f").is_ok() => {
            Value::DateTime(text.into())
        }
        Json => {
            structured::validate_unique_json(text)?;
            Value::Json(serde_json::from_str(text)?)
        }
        _ => {
            return Err(AppError::new(
                "type_error",
                format!("内容不符合 {kind:?} 类型"),
            ));
        }
    })
}

/// 带版本和容量上限的原始字节缓冲。NULL 不能被误当成空字节。
#[derive(Debug, Clone)]
pub struct ValueBuffer {
    kind: ValueKind,
    data: Option<Vec<u8>>,
    revision: u64,
    max_bytes: usize,
}

impl ValueBuffer {
    /// 初始化时必须持有完整值，容量计算由调用者纳入客户端总预算。
    pub fn new(value: &Value, max_bytes: usize) -> Result<Self> {
        let (kind, data) = to_bytes(value)?;
        if data.as_ref().is_some_and(|v| v.len() > max_bytes) {
            return Err(limit());
        }
        Ok(Self {
            kind,
            data,
            revision: 0,
            max_bytes,
        })
    }
    pub fn kind(&self) -> ValueKind {
        self.kind
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn len(&self) -> Option<usize> {
        self.data.as_ref().map(Vec::len)
    }
    pub fn is_empty(&self) -> bool {
        self.data.as_ref().is_some_and(Vec::is_empty)
    }
    pub fn bytes(&self) -> Option<&[u8]> {
        self.data.as_deref()
    }
    /// 返回指定窗口的原字节，offset 超出结尾时返回错误。
    pub fn read(&self, offset: usize, length: usize) -> Result<&[u8]> {
        let data = self
            .data
            .as_deref()
            .ok_or_else(|| AppError::invalid("该值没有字节内容"))?;
        if offset > data.len() {
            return Err(AppError::invalid("读取偏移超出值长度"));
        }
        Ok(&data[offset..offset.saturating_add(length).min(data.len())])
    }
    /// 按字节替换范围；文本模式要求旧值的两个边界和新片段均为 UTF-8。
    pub fn patch(
        &mut self,
        expected_revision: u64,
        range: Range<usize>,
        bytes: &[u8],
        text: bool,
    ) -> Result<u64> {
        if self.revision != expected_revision {
            return Err(AppError::conflict("值 revision 已变化"));
        }
        let data = self
            .data
            .as_mut()
            .ok_or_else(|| AppError::invalid("NULL 或不存在值不能 patch"))?;
        if range.start > range.end || range.end > data.len() {
            return Err(AppError::invalid("patch 范围无效"));
        }
        if text {
            let s = std::str::from_utf8(data).map_err(|e| AppError::invalid(e.to_string()))?;
            if !s.is_char_boundary(range.start)
                || !s.is_char_boundary(range.end)
                || std::str::from_utf8(bytes).is_err()
            {
                return Err(AppError::invalid("文本 patch 必须位于 UTF-8 边界"));
            }
        }
        let size = data
            .len()
            .checked_sub(range.end - range.start)
            .and_then(|n| n.checked_add(bytes.len()))
            .ok_or_else(limit)?;
        if size > self.max_bytes {
            return Err(limit());
        }
        let next = self.revision.checked_add(1).ok_or_else(limit)?;
        data.splice(range, bytes.iter().copied());
        self.revision = next;
        Ok(next)
    }
    /// 显式替换整个值，可用于导入、格式化、撤销及切换 NULL。
    pub fn replace(
        &mut self,
        expected_revision: u64,
        kind: ValueKind,
        data: Option<Vec<u8>>,
    ) -> Result<u64> {
        if self.revision != expected_revision {
            return Err(AppError::conflict("值 revision 已变化"));
        }
        if data.as_ref().is_some_and(|v| v.len() > self.max_bytes) {
            return Err(limit());
        }
        from_bytes(kind, data.as_deref())?;
        let next = self.revision.checked_add(1).ok_or_else(limit)?;
        self.kind = kind;
        self.data = data;
        self.revision = next;
        Ok(next)
    }
    /// 校验并生成可 stage 的类型化值。
    pub fn value(&self) -> Result<Value> {
        from_bytes(self.kind, self.data.as_deref())
    }
}

/// 解析可带空白分隔符的完整十六进制字节对，错误位置为 UTF-8 字节偏移。
pub fn parse_hex(input: &str, max_bytes: usize) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut high = None;
    for (pos, ch) in input.char_indices() {
        if ch.is_ascii_whitespace() {
            if high.is_some() {
                return Err(AppError::invalid(format!("十六进制字节不完整，位置 {pos}")));
            }
            continue;
        }
        let digit = ch
            .to_digit(16)
            .filter(|_| ch.is_ascii())
            .ok_or_else(|| AppError::invalid(format!("无效十六进制字符，位置 {pos}")))?
            as u8;
        if let Some(h) = high.take() {
            if out.len() == max_bytes {
                return Err(limit());
            }
            out.push((h << 4) | digit);
        } else {
            high = Some(digit);
        }
    }
    if high.is_some() {
        return Err(AppError::invalid(format!(
            "十六进制字节不完整，位置 {}",
            input.len()
        )));
    }
    Ok(out)
}

pub(crate) fn limit() -> AppError {
    AppError::new("resource_limit", "单值操作超过内存或尺寸限制")
}
