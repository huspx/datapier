//! 表设计面板与分区导航。
use super::{SchemaDefinition, SchemaEditor, Value};
use super::{
    form::{SchemaChoices, render_value},
    object_grid::{SchemaGridContext, render_object_grid},
};
use crate::{
    Msg,
    advanced::{diagnostic, fact, status, text_diff},
    i18n::tr,
};
use tern_sdk::ui::{self, Node, html};

impl SchemaEditor {
    /// 渲染公开 SDK 控件；App 将 schema: 编辑事件和 schema-op: 动作路由回本状态。
    pub fn view(&self) -> Node<Msg> {
        let mut panel = html::div().class("schema-panel advanced-panel");
        let mut context = html::div().class("advanced-facts");
        if let Some(database) = &self.draft.target.database {
            context = context.child(fact("数据库", database.clone()));
        }
        if !self.draft.target.name.is_empty() {
            context = context.child(fact("对象名称", self.draft.target.name.clone()));
        }
        panel = panel.child(context);
        if self.busy() {
            panel = panel.child(html::p(tr("处理中").into_owned()).class("notice"));
        }
        if self.summary.is_some() {
            panel = panel.child(
                html::p(tr("计划已冻结，修改条件前请先丢弃计划。").into_owned()).class("notice"),
            );
        }
        if self.session.is_null() && self.connect_nonce.is_none() {
            panel = panel.child(
                ui::button(tr("重试连接").into_owned())
                    .on_click(Msg::Action("schema-connect".into())),
            );
        }
        if self.outcome_unknown {
            panel = panel.child(html::p(tr("执行结果未知。请在数据库中重新读取目标结构，核对已执行的 DDL 和实际对象状态；确认后再解除等待，不会自动重试。").into_owned()).class("notice"));
            panel = panel.child(
                ui::button(tr("已核对结果，解除等待").into_owned())
                    .on_click(Msg::Action("schema-unknown-acknowledge".into())),
            );
        }
        if self.draft.creating {
            let disabled = self.busy()
                || self.summary.is_some()
                || self.restore_confirmation_required
                || self.query_transferred;
            panel = panel.child(
                ui::field(tr("对象名称").into_owned()).child(
                    ui::input()
                        .key("schema-target")
                        .aria(tr("对象名称").into_owned())
                        .variant(ui::InputVariant::Field)
                        .readonly(disabled)
                        .text(self.target_name.text.clone())
                        .cursor(self.target_name.cursor)
                        .anchor(self.target_name.anchor)
                        .on_focus(Msg::Focus("schema-target".into()))
                        .on_undo(Msg::Undo("schema-target".into()))
                        .on_edit(|e| {
                            Msg::Edit(
                                "schema-target".into(),
                                e.from,
                                e.to,
                                e.text.clone(),
                                e.cursor,
                                e.len,
                            )
                        }),
                ),
            );
        }
        if let Some(snapshot) = &self.snapshot {
            let completeness = match &snapshot.completeness.state {
                datapier_core::model::schema::CompletenessState::Complete => "完整",
                datapier_core::model::schema::CompletenessState::Incomplete => "不完整",
                datapier_core::model::schema::CompletenessState::Unknown => "未知",
                datapier_core::model::schema::CompletenessState::Unavailable => "不可用",
            };
            let mut facts = html::div()
                .class("advanced-facts")
                .child(fact("完整性", tr(completeness).into_owned()));
            for reason in &snapshot.completeness.reasons {
                facts = facts.child(fact("原因", reason.clone()));
            }
            for (name, capability) in &snapshot.capabilities {
                let label = match name.as_str() {
                    "check" => "CHECK 约束",
                    "expression_default" => "表达式默认值",
                    "functional_index" => "函数索引",
                    "descending_index" => "降序索引",
                    "index_visibility" => "索引可见性",
                    "invisible_column" => "不可见列",
                    _ => name,
                };
                let state = match capability.state.as_str() {
                    "unsupported" => tr("不支持").into_owned(),
                    "unknown" => tr("未知").into_owned(),
                    other => other.to_owned(),
                };
                let value = match &capability.reason {
                    Some(reason) => format!("{state} · {reason}"),
                    None => state,
                };
                facts = facts.child(fact(label, value));
            }
            panel = panel.child(facts);
        }
        if self.restore_confirmation_required {
            panel = panel.child(html::span(tr("请读取当前结构并确认恢复草稿").into_owned()));
            panel = panel.child(
                ui::button(tr("确认恢复").into_owned())
                    .on_click(Msg::Action("schema-restore-confirm".into())),
            );
        }
        if let Some(error) = &self.error {
            panel = panel.child(html::span(error.clone()).class("field-error"));
        }
        let busy = self.busy();
        let has_plan = self.summary.is_some();
        let table = matches!(self.definition.as_ref(), Some(SchemaDefinition::Table(_)));
        let show_plan_details = !table || self.section == "ddl_preview";
        if self.query_transferred {
            panel = panel
                .child(html::span(
                    tr("结构草稿已冻结，可返回继续编辑").into_owned(),
                ))
                .child(
                    ui::button(tr("恢复结构草稿编辑").into_owned())
                        .on_click(Msg::Action("schema-query-resume".into())),
                );
        }
        let can_submit = self.preview_complete
            && !self.query_transferred
            && self.summary.as_ref().is_some_and(|s| {
                s["complete"] == true
                    && s["submittable"] == true
                    && s["plan_digest"].as_str().is_some()
            });
        let mut actions = html::div().class("advanced-actions schema-sticky-actions");
        for (label, action, enabled) in [
            (
                "读取结构",
                "schema-describe",
                !self.draft.creating
                    && !has_plan
                    && (!self.dirty || self.restore_confirmation_required),
            ),
            (
                "生成计划",
                "schema-plan",
                !self.session.is_null()
                    && !has_plan
                    && self.definition.is_some()
                    && !self.restore_confirmation_required
                    && !self.query_transferred
                    && self.invalid_numbers.is_empty(),
            ),
            ("预览", "schema-preview", has_plan),
            ("提交", "schema-submit", can_submit),
            ("丢弃计划", "schema-discard", has_plan),
        ] {
            actions = actions.child(
                ui::button(tr(label).into_owned())
                    .variant(
                        if (action == "schema-plan" && !has_plan)
                            || (action == "schema-submit" && has_plan)
                        {
                            ui::ButtonVariant::Primary
                        } else {
                            ui::ButtonVariant::Default
                        },
                    )
                    .disabled(busy || !enabled)
                    .on_click(Msg::Action(action.into())),
            );
        }
        panel = panel.child(actions);
        if self.dirty || self.restore_confirmation_required {
            panel = panel.child(
                ui::button(tr("放弃结构草稿").into_owned())
                    .disabled(busy || has_plan)
                    .on_click(Msg::Action("schema-draft-discard".into())),
            );
        }
        if self.confirm_discard {
            panel = panel
                .child(html::p(tr("确认放弃结构草稿？").into_owned()))
                .child(
                    ui::button(tr("确认放弃").into_owned())
                        .disabled(busy || has_plan)
                        .on_click(Msg::Action("schema-draft-discard-confirm".into())),
                )
                .child(
                    ui::button(tr("取消").into_owned())
                        .on_click(Msg::Action("schema-draft-discard-cancel".into())),
                );
        }
        if self
            .snapshot
            .as_ref()
            .is_some_and(|snapshot| !snapshot.ddl.is_empty())
        {
            panel = panel.child(
                ui::button(tr("将原定义转到查询编辑器").into_owned())
                    .on_click(Msg::Action("schema-query-original".into())),
            );
        }
        if let Some(definition) = &self.definition
            && let Ok(value) = serde_json::to_value(definition)
        {
            let table = value.get("kind").and_then(Value::as_str) == Some("table");
            let disabled = self.pending.is_some()
                || self.execution_pending
                || self.summary.is_some()
                || self.query_transferred
                || self.restore_confirmation_required;
            panel = panel.child(
                html::p(tr("以下修改仅保存在结构草稿中，提交后才会作用于数据库。").into_owned())
                    .class("muted"),
            );
            if table {
                panel = panel.child(schema_tabs(&self.section, false));
                if self.section == "columns" {
                    if let Some(content) = value.get("columns") {
                        panel = panel.child(render_value(
                            content,
                            "/columns",
                            &mut self.editors.borrow_mut(),
                            disabled,
                            true,
                            self.selected_column.as_deref(),
                            SchemaChoices {
                                open_select: self.open_select.as_deref(),
                                root: Some(&value),
                            },
                        ));
                    }
                } else if matches!(self.section.as_str(), "indexes" | "foreign_keys" | "checks") {
                    panel = panel.child(render_object_grid(
                        &value,
                        &self.section,
                        &mut self.editors.borrow_mut(),
                        disabled,
                        self.selected_items.get(&self.section).map(String::as_str),
                        self.open_select.as_deref(),
                        SchemaGridContext {
                            references: &self.references,
                            database: self.draft.target.database.as_deref().unwrap_or(""),
                            table_name: &self.draft.target.name,
                        },
                    ));
                } else if self.section == "options" {
                    let mut options = serde_json::Map::new();
                    for key in ["engine", "charset", "collation", "comment", "options"] {
                        if let Some(item) = value.get(key) {
                            options.insert(key.to_owned(), item.clone());
                        }
                    }
                    panel = panel.child(html::div().class("schema-design-table-options").child(
                        render_value(
                            &Value::Object(options),
                            "",
                            &mut self.editors.borrow_mut(),
                            disabled,
                            true,
                            self.selected_column.as_deref(),
                            SchemaChoices {
                                open_select: self.open_select.as_deref(),
                                root: Some(&value),
                            },
                        ),
                    ));
                }
            } else {
                panel = panel.child(render_value(
                    &value,
                    "",
                    &mut self.editors.borrow_mut(),
                    disabled,
                    false,
                    self.selected_column.as_deref(),
                    SchemaChoices {
                        open_select: self.open_select.as_deref(),
                        root: Some(&value),
                    },
                ));
            }
            if !table || self.section == "options" {
                for (label, path, text) in [
                    (
                        "ALGORITHM",
                        "/@algorithm",
                        self.draft.algorithm.as_deref().unwrap_or(""),
                    ),
                    ("LOCK", "/@lock", self.draft.lock.as_deref().unwrap_or("")),
                ] {
                    panel = panel
                        .child(html::strong(tr(label).into_owned()))
                        .child(render_value(
                            &Value::String(text.to_owned()),
                            path,
                            &mut self.editors.borrow_mut(),
                            disabled,
                            table,
                            self.selected_column.as_deref(),
                            SchemaChoices {
                                open_select: self.open_select.as_deref(),
                                root: Some(&value),
                            },
                        ));
                }
            }
        }
        if self.confirm_submit && show_plan_details {
            let mut confirmation = html::div().class("advanced-section")
                .child(html::strong(tr("确认执行预览中的结构变更？").into_owned()))
                .child(fact("数据库", self.draft.target.database.clone().unwrap_or_default()))
                .child(fact("对象名称", self.draft.target.name.clone()))
                .child(html::p(tr("以下冻结的 DDL 将按顺序执行；已执行的变更不能自动回滚，请先确认备份和恢复方式。").into_owned()).class("notice"));
            for sql in &self.preview_sql {
                confirmation = confirmation.child(html::pre(sql.clone()).class("advanced-code"));
            }
            for reason in &self.destructive_reasons {
                confirmation = confirmation.child(html::p(reason.clone()).class("notice"));
            }
            confirmation = confirmation
                .child(
                    ui::button(tr("取消").into_owned())
                        .on_click(Msg::Action("schema-submit-cancel".into())),
                )
                .child(
                    ui::button(tr("确认执行").into_owned())
                        .variant(if self.destructive_reasons.is_empty() {
                            ui::ButtonVariant::Primary
                        } else {
                            ui::ButtonVariant::Danger
                        })
                        .disabled(busy || !can_submit)
                        .on_click(Msg::Action("schema-submit-confirm".into())),
                );
            panel = panel.child(confirmation);
        }
        if show_plan_details && self.query_sql().is_some() {
            panel = panel.child(
                ui::button(tr("转到查询编辑器").into_owned())
                    .on_click(Msg::Action("schema-query".into())),
            );
        }
        if show_plan_details {
            if let Some(summary) = &self.summary {
                let mut facts = html::div().class("advanced-facts schema-plan-summary");
                if let Some(state) = summary.get("state").and_then(Value::as_str) {
                    facts = facts.child(fact("状态", status(state)));
                }
                if let Some(total) = summary.get("total_items").and_then(Value::as_u64) {
                    facts = facts.child(fact("步骤", total.to_string()));
                }
                facts = facts
                    .child(fact(
                        "计划完整",
                        tr(if summary["complete"] == true {
                            "是"
                        } else {
                            "否"
                        })
                        .into_owned(),
                    ))
                    .child(fact(
                        "可提交",
                        tr(if summary["submittable"] == true {
                            "是"
                        } else {
                            "否"
                        })
                        .into_owned(),
                    ));
                panel = panel.child(facts);
                if self.preview.is_none() {
                    panel = panel.child(
                        html::p(tr("计划已生成，请使用上方预览按钮读取 DDL。").into_owned())
                            .class("notice"),
                    );
                } else if !self.preview_complete {
                    panel = panel.child(
                        html::p(tr("尚未读取全部预览页，请继续读取后再提交。").into_owned())
                            .class("notice"),
                    );
                }
            } else if table {
                let message = if self.execution.is_some() {
                    "计划已提交，请查看执行结果。"
                } else {
                    "尚未生成计划，请使用上方生成计划按钮查看 DDL。"
                };
                panel = panel.child(html::p(tr(message).into_owned()).class("notice"));
            }
        }
        if show_plan_details && let Some(preview) = &self.preview {
            let mut section = html::div()
                .class("advanced-section")
                .child(html::strong(tr("变更预览").into_owned()));
            // 视图 SELECT 与触发器正文在基线和冻结草稿中都存在；ALTER 等步骤不是新定义。
            if let (Some(snapshot), Some(definition)) = (&self.snapshot, &self.definition) {
                let bodies = match (&snapshot.definition, definition) {
                    (SchemaDefinition::View(old), SchemaDefinition::View(new)) => {
                        Some((&old.select, &new.select))
                    }
                    (SchemaDefinition::Trigger(old), SchemaDefinition::Trigger(new)) => {
                        Some((&old.body, &new.body))
                    }
                    _ => None,
                };
                if let Some((before, after)) = bodies {
                    section = section
                        .child(html::strong(tr("定义正文对比").into_owned()))
                        .child(text_diff(before, after, "definition.sql"));
                }
            }
            if let Some(steps) = preview.get("steps").and_then(Value::as_array) {
                for step in steps {
                    if let Some(sql) = step.get("sql").and_then(Value::as_str) {
                        section = section.child(html::pre(sql.to_owned()).class("advanced-code"));
                    }
                    for key in ["destructive_reasons", "warnings"] {
                        if let Some(reasons) = step.get(key).and_then(Value::as_array) {
                            for reason in reasons.iter().filter_map(Value::as_str) {
                                section = section.child(html::p(reason.to_owned()).class("notice"));
                            }
                        }
                    }
                }
            }
            if preview.get("next").is_some_and(|v| !v.is_null()) {
                section = section.child(
                    ui::button(tr("下一页").into_owned())
                        .on_click(Msg::Action("schema-preview-next".into())),
                );
            }
            panel = panel
                .child(section)
                .child(diagnostic("schema-preview-diagnostic", preview));
        }
        if let Some(execution) = &self.execution {
            let mut section = html::div()
                .class("advanced-section")
                .child(html::strong(tr("执行结果").into_owned()));
            if let Some(result) = execution.get("result") {
                if let Some(state) = result
                    .pointer("/summary/state")
                    .or_else(|| result.get("state"))
                    .and_then(Value::as_str)
                {
                    section = section.child(fact("状态", status(state)));
                }
                if let Some(steps) = result.pointer("/summary/steps").and_then(Value::as_array) {
                    for (index, step) in steps.iter().enumerate() {
                        if let Some(state) = step.get("state").and_then(Value::as_str) {
                            section = section.child(fact(
                                &format!("{} {}", tr("步骤"), index + 1),
                                status(state),
                            ));
                        }
                        for key in ["reason", "error"] {
                            if let Some(reason) = step.get(key).and_then(Value::as_str) {
                                section = section.child(fact("原因", reason));
                            }
                        }
                    }
                }
            } else {
                section = section.child(html::p(tr("处理中").into_owned()).class("muted"));
            }
            panel = panel
                .child(section)
                .child(diagnostic("schema-execution-diagnostic", execution));
        }
        panel.into()
    }
}

pub(crate) fn schema_tabs(selected: &str, readonly: bool) -> Node<Msg> {
    let mut tabs = html::div()
        .class("advanced-tabs schema-design-tabs")
        .attr("role", "tablist");
    for (id, label) in [
        ("columns", "字段"),
        ("indexes", "索引"),
        ("foreign_keys", "外键"),
        ("checks", "CHECK"),
        ("options", "表选项"),
    ]
    .into_iter()
    .chain((!readonly).then_some(("ddl_preview", "预览 DDL")))
    {
        let focus = format!("schema-section-{id}");
        let action = format!(
            "{}:{id}",
            if readonly {
                "schema-read-section"
            } else {
                "schema-section"
            }
        );
        tabs = tabs.child(
            html::button(tr(label).into_owned())
                .key(focus.clone())
                .class(if selected == id {
                    "advanced-tab active"
                } else {
                    "advanced-tab"
                })
                .attr("role", "tab")
                .attr("aria-selected", selected == id)
                .on_click(Msg::Action(action)),
        );
    }
    tabs.into()
}

pub(super) fn row_action(label: &str, icon: &str, action: String, disabled: bool) -> Node<Msg> {
    let label = tr(label).into_owned();
    ui::button("")
        .icon(icon.to_owned())
        .aria(label.clone())
        .title(label)
        .disabled(disabled)
        .on_click(Msg::Action(action))
        .into()
}
