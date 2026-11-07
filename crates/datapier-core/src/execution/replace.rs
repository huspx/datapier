//! 有界 Unicode 查找替换，正则条件和捕获引用只在准备阶段编译。
use crate::model::{AppError, Result, replace::ReplaceSpec};
use regex::{Regex, RegexBuilder};

#[derive(Debug)]
enum Part {
    Text(String),
    Capture(usize),
}

/// 可跨多个值复用的已校验替换条件。
pub struct CompiledReplacement {
    regex: Regex,
    parts: Vec<Part>,
}

/// 一次替换的完整结果及匹配数量。
#[derive(Debug, PartialEq, Eq)]
pub struct Replacement {
    pub value: String,
    pub matches: usize,
    pub changed: bool,
}

impl CompiledReplacement {
    /// 编译条件并拒绝不存在的捕获引用。
    pub fn new(spec: &ReplaceSpec) -> Result<Self> {
        if !spec.regex && spec.pattern.is_empty() {
            return Err(AppError::invalid("literal 查找内容不能为空"));
        }
        let pattern = if spec.regex {
            spec.pattern.clone()
        } else {
            regex::escape(&spec.pattern)
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!spec.case_sensitive)
            .multi_line(spec.multi_line)
            .dot_matches_new_line(spec.dot_matches_new_line)
            .build()
            .map_err(|e| AppError::invalid(format!("无效正则表达式: {e}")))?;
        let parts = if spec.regex {
            parse_replacement(&regex, &spec.replacement)?
        } else {
            vec![Part::Text(spec.replacement.clone())]
        };
        Ok(Self { regex, parts })
    }

    /// 输出不得超过字节预算；零宽匹配由 regex 按 UTF-8 边界推进。
    pub fn apply(&self, input: &str, max_output_bytes: usize) -> Result<Replacement> {
        let mut output = String::new();
        let mut end = 0;
        let mut matches = 0;
        for captures in self.regex.captures_iter(input) {
            let found = captures.get(0).expect("完整匹配总存在");
            append(&mut output, &input[end..found.start()], max_output_bytes)?;
            for part in &self.parts {
                match part {
                    Part::Text(text) => append(&mut output, text, max_output_bytes)?,
                    Part::Capture(index) => {
                        if let Some(value) = captures.get(*index) {
                            append(&mut output, value.as_str(), max_output_bytes)?;
                        }
                    }
                }
            }
            matches += 1;
            end = found.end();
        }
        append(&mut output, &input[end..], max_output_bytes)?;
        let changed = output != input;
        Ok(Replacement {
            value: output,
            matches,
            changed,
        })
    }
}

fn append(output: &mut String, text: &str, budget: usize) -> Result<()> {
    if text.len() > budget.saturating_sub(output.len()) {
        return Err(AppError::invalid("替换输出超过字节预算"));
    }
    output.push_str(text);
    Ok(())
}

fn parse_replacement(regex: &Regex, replacement: &str) -> Result<Vec<Part>> {
    let bytes = replacement.as_bytes();
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut pos = 0;
    while pos < bytes.len() {
        if bytes[pos] != b'$' {
            let ch = replacement[pos..].chars().next().expect("有效 UTF-8");
            text.push(ch);
            pos += ch.len_utf8();
            continue;
        }
        if bytes.get(pos + 1) == Some(&b'$') {
            text.push('$');
            pos += 2;
            continue;
        }
        let start = pos + 1;
        let (name, next) = if bytes.get(start) == Some(&b'{') {
            let rest = &replacement[start + 1..];
            let close = rest
                .find('}')
                .ok_or_else(|| AppError::invalid("未闭合的捕获引用"))?;
            (&rest[..close], start + close + 2)
        } else {
            let mut end = start;
            while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
                end += 1;
            }
            if end == start {
                text.push('$');
                pos += 1;
                continue;
            }
            (&replacement[start..end], end)
        };
        let index = if name.bytes().all(|b| b.is_ascii_digit()) && !name.is_empty() {
            name.parse::<usize>()
                .ok()
                .filter(|i| *i < regex.captures_len())
        } else {
            regex.capture_names().position(|n| n == Some(name))
        }
        .ok_or_else(|| AppError::invalid(format!("未知捕获引用: {name}")))?;
        if !text.is_empty() {
            parts.push(Part::Text(std::mem::take(&mut text)));
        }
        parts.push(Part::Capture(index));
        pos = next;
    }
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    Ok(parts)
}

#[cfg(test)]
#[path = "../../tests/unit/execution/replace.rs"]
mod tests;
