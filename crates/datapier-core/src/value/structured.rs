//! JSON 和 XML 的安全校验及显式格式化。

use super::limit;
use crate::model::{AppError, Result};
use serde::Deserialize;
use serde::de::{DeserializeSeed, Deserializer, Error as _, MapAccess, SeqAccess, Visitor};
use std::collections::HashSet;
use std::fmt;

// serde_json::Value 会覆盖重复键，因此在构造 Value 前遍历原始流。
struct Unique;
impl<'de> DeserializeSeed<'de> for Unique {
    type Value = ();
    fn deserialize<D: Deserializer<'de>>(self, d: D) -> std::result::Result<(), D::Error> {
        d.deserialize_any(UniqueVisitor)
    }
}
struct UniqueVisitor;
impl<'de> Visitor<'de> for UniqueVisitor {
    type Value = ();
    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("无重复键的 JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, _: bool) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_i64<E: serde::de::Error>(self, _: i64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_u64<E: serde::de::Error>(self, _: u64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_f64<E: serde::de::Error>(self, _: f64) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_str<E: serde::de::Error>(self, _: &str) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_string<E: serde::de::Error>(self, _: String) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_none<E: serde::de::Error>(self) -> std::result::Result<(), E> {
        Ok(())
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> std::result::Result<(), A::Error> {
        while seq.next_element_seed(Unique)?.is_some() {}
        Ok(())
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<(), A::Error> {
        let mut keys = HashSet::new();
        while let Some(key) = map.next_key::<String>()? {
            if !keys.insert(key) {
                return Err(A::Error::custom("JSON 对象包含重复键"));
            }
            map.next_value_seed(Unique)?;
        }
        Ok(())
    }
}

/// 校验 JSON 语法；重复键在查看时保留。
pub fn validate_json(input: &str) -> Result<()> {
    let mut de = serde_json::Deserializer::from_str(input);
    serde::de::IgnoredAny::deserialize(&mut de).map_err(|e| AppError::invalid(e.to_string()))?;
    de.end().map_err(|e| AppError::invalid(e.to_string()))
}

pub(super) fn validate_unique_json(input: &str) -> Result<()> {
    let mut de = serde_json::Deserializer::from_str(input);
    Unique
        .deserialize(&mut de)
        .map_err(|e| AppError::invalid(e.to_string()))?;
    de.end().map_err(|e| AppError::invalid(e.to_string()))
}

/// 返回格式化后的新文本；绝不修改传入值或丢弃重复键。
pub fn format_json(input: &str, max_bytes: usize) -> Result<String> {
    validate_unique_json(input)?;
    let value: serde_json::Value = serde_json::from_str(input)?;
    let mut writer = super::media::BoundedWriter::new(max_bytes);
    serde_json::to_writer_pretty(&mut writer, &value).map_err(|e| {
        if writer.exceeded() {
            limit()
        } else {
            AppError::invalid(e.to_string())
        }
    })?;
    String::from_utf8(writer.into_inner()).map_err(|e| AppError::invalid(e.to_string()))
}

/// 校验完整 UTF-8 XML、根、命名空间和属性；拒绝 DTD 和外部实体。
pub fn validate_xml(input: &str) -> Result<()> {
    parse_xml(input).map(|_| ())
}

pub(super) fn parse_xml(input: &str) -> Result<roxmltree::Document<'_>> {
    // roxmltree 对空 DTD 也允许通过，需要显式拦截整个声明。
    let mut reader = quick_xml::Reader::from_str(input);
    loop {
        match reader
            .read_event()
            .map_err(|e| AppError::invalid(format!("XML: {e}")))?
        {
            quick_xml::events::Event::DocType(_) => {
                return Err(AppError::invalid("不允许 XML DTD"));
            }
            quick_xml::events::Event::Eof => break,
            _ => {}
        }
    }
    let opts = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: 100_000,
        ..Default::default()
    };
    roxmltree::Document::parse_with_options(input, opts)
        .map_err(|e| AppError::invalid(format!("XML: {e}")))
}

/// 对可安全缩进的元素内容生成新文本；混合内容及保留空白的文档拒绝格式化。
pub fn format_xml(input: &str, max_bytes: usize) -> Result<String> {
    let doc = parse_xml(input)?;
    if input.contains("<![CDATA[") {
        return Err(AppError::new(
            "unsupported_format",
            "含 CDATA 的 XML 不安全格式化",
        ));
    }
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    for node in doc.descendants().filter(|n| n.is_element()) {
        if node.attribute(("http://www.w3.org/XML/1998/namespace", "space")) == Some("preserve") {
            return Err(AppError::new(
                "unsupported_format",
                "xml:space=preserve 不安全格式化",
            ));
        }
        let children: Vec<_> = node.children().collect();
        if !children.iter().any(|n| n.is_element()) {
            continue;
        }
        if children
            .iter()
            .any(|n| n.is_text() && !n.text().unwrap_or("").trim().is_empty())
        {
            return Err(AppError::new("unsupported_format", "混合内容不安全格式化"));
        }
        let depth = node.ancestors().filter(|n| n.is_element()).count();
        let range = node.range();
        let raw = &input[range.clone()];
        let close = raw
            .rfind("</")
            .ok_or_else(|| AppError::invalid("XML 闭合标签无效"))?
            + range.start;
        let items: Vec<_> = children.iter().filter(|n| !n.is_text()).collect();
        if items.is_empty() {
            continue;
        }
        for (index, child) in items.iter().enumerate() {
            let start = if index == 0 {
                // `>` 可出现在引号内，使用首个子节点前最后一个 `>`。
                input[range.start..child.range().start]
                    .rfind('>')
                    .ok_or_else(|| AppError::invalid("XML 开始标签无效"))?
                    + range.start
                    + 1
            } else {
                items[index - 1].range().end
            };
            let end = child.range().start;
            if !input[start..end].chars().all(char::is_whitespace) {
                return Err(AppError::new("unsupported_format", "XML 内容不能安全缩进"));
            }
            edits.push((start..end, format!("\n{}", "  ".repeat(depth))));
        }
        let start = items.last().unwrap().range().end;
        if !input[start..close].chars().all(char::is_whitespace) {
            return Err(AppError::new("unsupported_format", "XML 内容不能安全缩进"));
        }
        edits.push((start..close, format!("\n{}", "  ".repeat(depth - 1))));
    }
    let mut size = input.len();
    for (range, replacement) in &edits {
        size = size
            .checked_sub(range.len())
            .and_then(|n| n.checked_add(replacement.len()))
            .ok_or_else(limit)?;
    }
    if size > max_bytes {
        return Err(limit());
    }
    edits.sort_unstable_by_key(|(r, _)| std::cmp::Reverse(r.start));
    let mut out = input.to_owned();
    for (range, replacement) in edits {
        out.replace_range(range, &replacement);
    }
    Ok(out)
}
