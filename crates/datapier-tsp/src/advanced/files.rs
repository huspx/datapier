//! 单值文件操作界面；路径和文件会话均由用户显式操作。
use crate::{Editor, Msg, i18n::tr};
use serde_json::{Value, json};
use tern_sdk::ui::{self, Node, html};

/// 外部文件生命周期由 Engine 管理；本地保留用于显示的路径和 revision。
#[derive(Clone, Default)]
pub(crate) struct FileUi {
    pub path: Editor,
    pub overwrite: bool,
    pub file_session: Option<String>,
    pub file_revision: u64,
    pub external_path: Option<String>,
    /// 仅保存 Engine 签发的一次性 handoff，程序与参数不进入 shell。
    pub terminal_handoff: Option<uuid::Uuid>,
    pub pending: bool,
    pub error: Option<String>,
}

impl FileUi {
    /// 构造用户明确选择路径和覆盖策略的导出请求。
    pub fn export(&self, value_session: &str, revision: u64) -> Value {
        json!({"value_session":value_session,"revision":revision,"path":self.path.text,"overwrite":self.overwrite})
    }

    /// 导入只更新值会话，stage 仍须单独确认。
    pub fn import(&self, value_session: &str, revision: u64) -> Value {
        json!({"value_session":value_session,"revision":revision,"path":self.path.text})
    }

    pub fn external_open(value_session: &str, revision: u64, format: &str) -> Value {
        json!({"value_session":value_session,"revision":revision,"format":format})
    }

    pub fn external_request(&self) -> Option<Value> {
        Some(json!({"file_session":self.file_session.as_ref()?,"revision":self.file_revision}))
    }

    /// keep 必须来自用户对文件保留或清理的明确选择。
    pub fn external_close(&self, keep: bool) -> Option<Value> {
        Some(
            json!({"file_session":self.file_session.as_ref()?,"revision":self.file_revision,"keep":keep}),
        )
    }

    /// 输入事件使用宿主的 UTF-16 偏移，并拒绝迟到事件。
    pub fn edit(&mut self, from: usize, to: usize, text: &str, cursor: usize, len: usize) -> bool {
        if self.pending
            || self.path.text.encode_utf16().count() != len
            || !self.path.edit(from..to, text)
        {
            return false;
        }
        self.path.set_selection(cursor, cursor);
        true
    }

    /// 只创建请求，调用方将响应路由回本会话并显示真实错误。
    pub fn action(
        &mut self,
        action: &str,
        value_session: &str,
        revision: u64,
        writable: bool,
        format: &str,
    ) -> Option<(&'static str, Value)> {
        if self.pending {
            return None;
        }
        match action {
            "advanced-file-overwrite" => {
                self.overwrite = !self.overwrite;
                None
            }
            "advanced-file-export" if !self.path.text.is_empty() => {
                Some(("value.file.export", self.export(value_session, revision)))
            }
            "advanced-file-import" if writable && !self.path.text.is_empty() => {
                Some(("value.file.import", self.import(value_session, revision)))
            }
            "advanced-external-open" if writable && self.file_session.is_none() => Some((
                "value.external.open",
                Self::external_open(value_session, revision, format),
            )),
            "advanced-external-launch" => self
                .external_request()
                .map(|params| ("value.external.launch", params)),
            "advanced-external-read" if writable => self
                .external_request()
                .map(|params| ("value.external.read", params)),
            "advanced-external-keep" => self
                .external_close(true)
                .map(|params| ("value.external.close", params)),
            "advanced-external-clean" => self
                .external_close(false)
                .map(|params| ("value.external.close", params)),
            _ => None,
        }
    }

    /// 文件准备成功后先展示真实路径，用户另行选择是否启动编辑器。
    pub fn open_response(&mut self, value: &Value) -> datapier_core::Result<()> {
        use datapier_core::model::AppError;
        let id = value
            .get("file_session")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("缺少文件会话"))?;
        let revision = value
            .get("revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| AppError::invalid("缺少文件版本"))?;
        let path = value
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("缺少编辑文件路径"))?;
        self.file_session = Some(id.to_owned());
        self.file_revision = revision;
        self.external_path = Some(path.to_owned());
        Ok(())
    }

    /// launch/read 返回新的文件版本；值版本属于独立的 value 会话。
    pub fn update_revision(&mut self, value: &Value) -> datapier_core::Result<()> {
        use datapier_core::model::AppError;
        let revision = value
            .get("file_revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| AppError::invalid("缺少文件版本"))?;
        if revision < self.file_revision {
            return Err(AppError::conflict("文件版本已变化"));
        }
        self.file_revision = revision;
        Ok(())
    }

    /// terminal 模式只返回受控 handoff，窗口桥由运行线程接续。
    pub fn launch_response(&mut self, value: &Value) -> datapier_core::Result<()> {
        use datapier_core::model::AppError;
        self.update_revision(value)?;
        if value.get("mode").and_then(Value::as_str) == Some("terminal") {
            if value.get("action").and_then(Value::as_str) != Some("value_editor") {
                return Err(AppError::invalid("终端编辑器交接动作无效"));
            }
            let handoff = value
                .get("handoff")
                .and_then(Value::as_str)
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .ok_or_else(|| AppError::invalid("终端编辑器交接无效"))?;
            self.terminal_handoff = Some(handoff);
        }
        Ok(())
    }

    /// 成功关闭后才丢弃引用；失败时保留路径和会话以便重试。
    pub fn closed(&mut self) {
        self.file_session = None;
        self.external_path = None;
        self.file_revision = 0;
        self.terminal_handoff = None;
    }
}

fn button(label: &str, action: &str, enabled: bool) -> Node<Msg> {
    ui::button(tr(label).into_owned())
        .key(action.to_owned())
        .disabled(!enabled)
        .on_click(Msg::Action(action.to_owned()))
        .into()
}

/// 所有动作仅生成界面事件，调用方使用 Engine 响应更新会话与错误。
pub(crate) fn view(state: &FileUi, writable: bool, value_ready: bool) -> Node<Msg> {
    let ready = value_ready && !state.pending;
    let mut panel = html::div().class("value-files");
    if let Some(error) = &state.error {
        panel = panel.child(html::p(error.clone()).class("notice"));
    }
    panel = panel.child(
        ui::field(tr("文件路径").into_owned()).child(
            ui::input()
                .variant(ui::InputVariant::Field)
                .key("input-advanced-file-path")
                .aria(tr("文件路径").into_owned())
                .text(state.path.text.clone())
                .cursor(state.path.cursor)
                .anchor(state.path.anchor)
                .on_focus(Msg::Focus("advanced-file-path".into()))
                .on_undo(Msg::Undo("advanced-file-path".into()))
                .on_edit(|e| {
                    Msg::Edit(
                        "advanced-file-path".into(),
                        e.from,
                        e.to,
                        e.text.clone(),
                        e.cursor,
                        e.len,
                    )
                }),
        ),
    );
    panel = panel.child(button(
        if state.overwrite {
            "覆盖：已启用"
        } else {
            "覆盖：未启用"
        },
        "advanced-file-overwrite",
        ready,
    ));
    panel = panel.child(button(
        "导出",
        "advanced-file-export",
        ready && !state.path.text.is_empty(),
    ));
    panel = panel.child(button(
        "导入",
        "advanced-file-import",
        ready && writable && !state.path.text.is_empty(),
    ));
    panel = panel.child(button(
        "准备外部编辑",
        "advanced-external-open",
        ready && writable && state.file_session.is_none(),
    ));
    if let Some(path) = &state.external_path {
        panel = panel.child(html::p(path.clone()).class("mono"));
        panel = panel.child(
            html::p(tr("启动编辑器不表示编辑结束，请明确读取修改。").into_owned()).class("muted"),
        );
        panel = panel.child(button("启动编辑器", "advanced-external-launch", ready));
        panel = panel.child(button(
            "读取修改",
            "advanced-external-read",
            ready && writable,
        ));
        panel = panel.child(button("保留文件", "advanced-external-keep", ready));
        panel = panel.child(button("结束并清理", "advanced-external-clean", ready));
    }
    panel.into()
}
