//! 结构设计、批量替换与高级值编辑。
pub(crate) mod files;
pub(crate) mod replace;
mod replace_controller;
pub(crate) mod schema;
pub(crate) mod schema_controller;
pub(crate) mod value;
mod value_controller;

use crate::{Msg, i18n::tr};
use serde_json::Value;
use tern_sdk::ui::{self, Node, html};

/// 只比较已取得的文本；执行语句和参数仍由各自的预览区展示。
pub(crate) fn text_diff(before: &str, after: &str, path: &str) -> Node<Msg> {
    // 大量重复行可能使对比退化；前置字节预算与算法时限共同限制界面线程耗时。
    let diff = similar::TextDiff::configure()
        .algorithm(similar::Algorithm::Patience)
        .timeout(std::time::Duration::from_millis(10))
        .diff_lines(before, after);
    let lines = diff
        .iter_all_changes()
        .map(|change| {
            let prefix = match change.tag() {
                similar::ChangeTag::Equal => ' ',
                similar::ChangeTag::Delete => '-',
                similar::ChangeTag::Insert => '+',
            };
            let value = change.value();
            let value = value.strip_suffix('\n').unwrap_or(value);
            format!("{prefix}{value}")
        })
        .collect();
    ui::diff()
        .path(path.to_owned())
        .hunks(vec![ui::Hunk {
            old_start: Some(1),
            new_start: Some(1),
            lines,
        }])
        .prop("wrap", false)
        .into()
}

pub(crate) fn fact(label: &str, value: impl Into<String>) -> Node<Msg> {
    html::div()
        .class("advanced-fact")
        .child(html::span(tr(label).into_owned()).class("advanced-fact-label"))
        .child(html::span(value.into()).class("advanced-fact-value"))
        .into()
}

pub(crate) fn diagnostic(key: &str, value: &Value) -> Node<Msg> {
    ui::tree()
        .item(
            ui::TreeItem::new(key, tr("完整诊断").into_owned())
                .open(false)
                .child(ui::TreeItem::new(
                    format!("{key}-value"),
                    serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string()),
                )),
        )
        .into()
}

pub(crate) fn status(value: &str) -> String {
    tr(match value {
        "ready" => "已就绪",
        "preparing" => "准备中",
        "invalid" => "需调整计划",
        "invalidated" => "计划已失效",
        "read_failed" => "读取失败",
        "not_sent" => "未发送",
        "sent" => "已发送",
        "rolled_back" => "已回滚",
        "skipped" => "已跳过",
        "complete" => "完整",
        "completed" | "succeeded" => "已完成",
        "applied" => "已生效",
        "published" => "已保存",
        "cleaned" => "已清理",
        "cleanup_pending" => "待清理",
        "orphaned" => "已遗留",
        "owned" => "已领取",
        "incomplete" => "不完整",
        "pending" | "queued" | "running" | "cancelling" => "处理中",
        "failed" => "失败",
        "cancelled" => "已取消",
        "conflict" => "冲突",
        "partial" => "部分生效",
        "not_executed" => "未执行",
        "excluded" => "已排除",
        "unchanged" => "未变更",
        "outcome_unknown" => "结果待确认",
        "unknown" => "未知",
        "verification_mismatch" => "读回结果不一致",
        _ => "未知",
    })
    .into_owned()
}
