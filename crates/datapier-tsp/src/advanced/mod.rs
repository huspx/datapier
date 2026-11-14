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

fn fact(label: &str, value: impl Into<String>) -> Node<Msg> {
    html::div()
        .class("advanced-fact")
        .child(html::span(tr(label).into_owned()).class("advanced-fact-label"))
        .child(html::span(value.into()).class("advanced-fact-value"))
        .into()
}

fn diagnostic(key: &str, value: &Value) -> Node<Msg> {
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

fn status(value: &str) -> String {
    tr(match value {
        "ready" => "已就绪",
        "complete" => "完整",
        "completed" | "succeeded" => "已完成",
        "applied" => "已生效",
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
