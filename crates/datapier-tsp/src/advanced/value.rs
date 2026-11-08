//! 高级值编辑界面的本地状态。实际值、版本和权限由 Engine 持有。
use crate::{Editor, Msg, i18n::tr};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use tern_sdk::ui::{self, Node, html};

const VALUE_KINDS: &[(&str, &str)] = &[
    ("text", "文本"),
    ("bytes", "二进制"),
    ("json", "JSON"),
    ("bool", "布尔值"),
    ("integer", "整数"),
    ("unsigned", "无符号整数"),
    ("decimal", "精确小数"),
    ("float", "浮点数"),
    ("date", "日期"),
    ("time", "时间"),
    ("datetime", "日期时间"),
];

/// 所有文本模式共享同一个原始值，切换模式本身不改变内容。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Mode {
    #[default]
    Text,
    Hex,
    Json,
    Xml,
    Image,
}

impl Mode {
    fn action(self) -> &'static str {
        match self {
            Self::Text => "advanced-value-mode:text",
            Self::Hex => "advanced-value-mode:hex",
            Self::Json => "advanced-value-mode:json",
            Self::Xml => "advanced-value-mode:xml",
            Self::Image => "advanced-value-mode:image",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Text => "文本",
            Self::Hex => "Hex",
            Self::Json => "JSON",
            Self::Xml => "XML",
            Self::Image => "图片",
        }
    }
}

/// 局部编辑器只持有当前窗口；完整值始终由 Engine 会话维护。
#[derive(Clone, Default)]
pub(crate) struct ValueUi {
    pub session: Option<String>,
    pub revision: u64,
    pub kind: String,
    pub is_null: bool,
    pub allow_null: bool,
    pub native_kind: Option<String>,
    pub null_kind: Option<String>,
    pub total: u64,
    pub offset: u64,
    pub writable: bool,
    pub max_text_bytes: usize,
    pub max_value_bytes: usize,
    pub mode: Mode,
    pub editor: Editor,
    pub original_window: Vec<u8>,
    pub pending: bool,
    pub error: Option<String>,
    pub validation: Option<String>,
    /// 仅由运行线程在显式预览响应后注册的派生 PNG blob。
    pub preview_id: Option<String>,
    pub retired_previews: Vec<(String, u64)>,
    pub preview_total: u64,
    pub preview_bytes: Vec<u8>,
    pub loading_bytes: Vec<u8>,
    pub format_undo: Option<Editor>,
    pub request: Option<(uuid::Uuid, String)>,
    pub source: Option<Value>,
    pub image_blob: Option<String>,
}

impl ValueUi {
    /// 编辑文本不能是截断前缀；Hex 则只替换当前明确窗口。
    pub fn editable(&self) -> bool {
        self.writable
            && !self.pending
            && self.session.is_some()
            && self.mode != Mode::Image
            && if self.is_null {
                self.null_kind.is_some()
            } else {
                self.mode == Mode::Hex
                    || (self.offset == 0
                        && self.total <= self.max_text_bytes as u64
                        && self.original_window.len() as u64 == self.total)
            }
    }

    /// 当前窗口修改必须先应用到值会话，加入草稿仍不代表提交数据库。
    pub(crate) fn can_stage(&self) -> bool {
        self.writable && self.session.is_some() && !self.pending && !self.dirty()
    }

    /// 清除和旧 revision 关联的派生预览，避免迟到图片覆盖当前值。
    pub fn invalidate_preview(&mut self) {
        self.image_blob = None;
        if let Some(id) = self.preview_id.take() {
            self.retired_previews.push((id, self.revision));
        }
        self.preview_bytes.clear();
        self.preview_total = 0;
        self.validation = None;
    }

    /// 返回窗口原字节，不将显示字符串作为二进制的来源。
    pub fn set_window(&mut self, offset: u64, bytes: Vec<u8>) -> datapier_core::Result<()> {
        use datapier_core::model::AppError;
        if offset
            .checked_add(bytes.len() as u64)
            .is_none_or(|end| end > self.total)
        {
            return Err(AppError::invalid("值分块范围无效"));
        }
        let text = if self.mode == Mode::Hex {
            use std::fmt::Write as _;
            let mut output = String::with_capacity(bytes.len().saturating_mul(3));
            for (index, byte) in bytes.iter().enumerate() {
                if index != 0 {
                    output.push(' ');
                }
                let _ = write!(output, "{byte:02X}");
            }
            output
        } else if self.mode == Mode::Image {
            String::new()
        } else {
            std::str::from_utf8(&bytes)
                .map_err(|error| AppError::invalid(error.to_string()))?
                .to_owned()
        };
        self.offset = offset;
        self.original_window = bytes;
        if let Some(mut previous) = self.format_undo.take() {
            let len = previous.text.encode_utf16().count();
            if previous.edit(0..len, &text) {
                self.editor = previous;
            } else {
                self.editor = Editor::new(text);
            }
        } else {
            self.editor = Editor::new(text);
        }
        Ok(())
    }

    /// 将当前窗口的明确用户修改编码为 patch 内容。
    pub fn edited_bytes(&self) -> datapier_core::Result<Vec<u8>> {
        if self.mode == Mode::Hex {
            datapier_core::value::parse_hex(&self.editor.text, self.max_value_bytes)
        } else {
            Ok(self.editor.text.as_bytes().to_vec())
        }
    }

    /// 尚未 patch 的内容不得在切换视图、读取或 stage 时丢失。
    pub fn dirty(&self) -> bool {
        if self.is_null {
            return !self.editor.text.is_empty();
        }
        if self.mode == Mode::Image {
            return false;
        }
        self.edited_bytes()
            .map_or(true, |bytes| bytes != self.original_window)
    }

    /// 输入使用 SDK 的 UTF-16 坐标，拒绝迟到或非法编辑。
    pub fn edit(&mut self, from: usize, to: usize, text: &str, cursor: usize, len: usize) -> bool {
        if !self.editable()
            || self.editor.text.encode_utf16().count() != len
            || !self.editor.edit(from..to, text)
        {
            return false;
        }
        self.editor.set_selection(cursor, cursor);
        self.invalidate_preview();
        true
    }

    pub fn undo(&mut self) -> bool {
        if !self.editable() || !self.editor.undo() {
            return false;
        }
        self.invalidate_preview();
        true
    }

    /// 返回要交给 Backend 的 RPC；调用方按 tab 和 generation 过滤响应。
    pub fn action(&mut self, action: &str) -> datapier_core::Result<Option<(&'static str, Value)>> {
        use datapier_core::model::AppError;
        if self.pending {
            return Ok(None);
        }
        if let Some(kind) = action.strip_prefix("advanced-value-null-kind:") {
            if !self.is_null
                || !self.writable
                || !matches!(
                    kind,
                    "bool"
                        | "integer"
                        | "unsigned"
                        | "decimal"
                        | "float"
                        | "text"
                        | "bytes"
                        | "date"
                        | "time"
                        | "datetime"
                        | "json"
                )
            {
                return Ok(None);
            }
            if self
                .native_kind
                .as_deref()
                .is_some_and(|original| original != kind)
            {
                return Err(AppError::invalid("新值类型与原值不一致"));
            }
            if self.dirty() {
                return Err(AppError::invalid("请先应用当前修改"));
            }
            self.null_kind = Some(kind.to_owned());
            self.mode = match kind {
                "bytes" => Mode::Hex,
                "json" => Mode::Json,
                _ => Mode::Text,
            };
            self.editor = Editor::default();
            return Ok(None);
        }
        let mode = match action {
            "advanced-value-mode:text" => Some(Mode::Text),
            "advanced-value-mode:hex" => Some(Mode::Hex),
            "advanced-value-mode:json" => Some(Mode::Json),
            "advanced-value-mode:xml" => Some(Mode::Xml),
            "advanced-value-mode:image" => Some(Mode::Image),
            _ => None,
        };
        if let Some(mode) = mode {
            if mode == self.mode {
                return Ok(None);
            }
            if self.dirty() {
                return Err(AppError::invalid("请先应用当前修改"));
            }
            self.mode = mode;
            self.invalidate_preview();
            self.loading_bytes.clear();
            if self.is_null || mode == Mode::Image {
                return Ok(None);
            }
            return Ok(self.read_request(0, self.read_size()));
        }
        if matches!(
            action,
            "advanced-value-previous"
                | "advanced-value-next"
                | "advanced-value-stage"
                | "advanced-value-validate"
                | "advanced-value-format"
                | "advanced-value-preview-image"
                | "advanced-value-set-null"
        ) && self.dirty()
        {
            return Err(AppError::invalid("请先应用当前修改"));
        }
        Ok(match action {
            "advanced-value-set-null" if self.writable && self.allow_null && !self.is_null => {
                self.session.as_ref().map(|session| {
                    (
                        "value.set_null",
                        json!({"value_session":session,"revision":self.revision}),
                    )
                })
            }
            "advanced-value-replace-null" if self.is_null && self.editable() => {
                let kind = self
                    .null_kind
                    .as_ref()
                    .ok_or_else(|| AppError::invalid("请先选择值类型"))?;
                let session = self
                    .session
                    .as_ref()
                    .ok_or_else(|| AppError::invalid("值会话不存在"))?;
                Some((
                    "value.replace",
                    json!({"value_session":session,"revision":self.revision,"kind":kind,"data_b64":STANDARD.encode(self.edited_bytes()?)}),
                ))
            }
            "advanced-value-patch" => Some(self.patch_request()?),
            "advanced-value-stage" => self.stage_request(),
            "advanced-value-previous" => self.read_request(self.offset.saturating_sub(4096), 4096),
            "advanced-value-next" => {
                self.read_request(self.offset + self.original_window.len() as u64, 4096)
            }
            "advanced-value-validate" => self.preview_request(match self.mode {
                Mode::Json => "json",
                Mode::Xml => "xml",
                _ => return Ok(None),
            }),
            "advanced-value-format" => {
                if !self.editable() {
                    return Ok(None);
                }
                let mode = match self.mode {
                    Mode::Json => "json_format",
                    Mode::Xml => "xml_format",
                    _ => return Ok(None),
                };
                self.format_undo = Some(self.editor.clone());
                self.preview_request(mode)
            }
            "advanced-value-preview-image" => {
                self.invalidate_preview();
                self.preview_request("image")
            }
            _ => None,
        })
    }

    /// 校验只展示 Engine 的结果；用户内容和诊断不经翻译。
    pub fn validation_response(&mut self, value: &Value) -> datapier_core::Result<()> {
        use datapier_core::model::AppError;
        let valid = value
            .get("valid")
            .and_then(Value::as_bool)
            .ok_or_else(|| AppError::invalid("校验响应缺少结果"))?;
        let errors = value
            .get("errors")
            .and_then(Value::as_array)
            .ok_or_else(|| AppError::invalid("校验响应缺少错误列表"))?;
        self.validation = Some(if valid {
            tr("校验通过").into_owned()
        } else {
            errors
                .iter()
                .map(|error| {
                    error
                        .as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| error.to_string())
                })
                .collect::<Vec<_>>()
                .join("\n")
        });
        Ok(())
    }

    pub fn read_request(&self, offset: u64, max_bytes: usize) -> Option<(&'static str, Value)> {
        Some((
            "value.read",
            json!({"value_session":self.session.as_ref()?,"revision":self.revision,"offset":offset,"max_bytes":max_bytes}),
        ))
    }

    /// 只替换当前窗口；十六进制按字节解析，文本以原始 UTF-8 写回。
    pub fn patch_request(&self) -> datapier_core::Result<(&'static str, Value)> {
        use datapier_core::model::AppError;
        if self.is_null || !self.editable() {
            return Err(AppError::invalid("当前值不可编辑"));
        }
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| AppError::invalid("值会话不存在"))?;
        let bytes = self.edited_bytes()?;
        Ok((
            "value.patch",
            json!({"value_session":session,"revision":self.revision,"offset":self.offset,"delete_bytes":self.original_window.len(),"data_b64":STANDARD.encode(bytes),"text":self.mode != Mode::Hex}),
        ))
    }

    pub fn stage_request(&self) -> Option<(&'static str, Value)> {
        if !self.writable || self.pending || self.dirty() {
            return None;
        }
        Some((
            "value.stage",
            json!({"value_session":self.session.as_ref()?,"revision":self.revision,"draft_revision":self.source.as_ref()?.get("draft_revision")?.as_u64()?}),
        ))
    }

    pub fn preview_request(&self, mode: &str) -> Option<(&'static str, Value)> {
        if self.pending {
            return None;
        }
        Some((
            "value.preview",
            json!({"value_session":self.session.as_ref()?,"revision":self.revision,"mode":mode}),
        ))
    }

    /// 应用 Engine 的会话摘要，并为尚未返回的内容生成后续读取请求。
    pub fn summary_response(
        &mut self,
        value: &Value,
    ) -> datapier_core::Result<Option<(&'static str, Value)>> {
        use datapier_core::model::AppError;
        let field = |name: &str| AppError::invalid(format!("值会话响应缺少 {name}"));
        let session = value
            .get("value_session")
            .and_then(Value::as_str)
            .ok_or_else(|| field("value_session"))?;
        let revision = value
            .get("revision")
            .and_then(Value::as_u64)
            .ok_or_else(|| field("revision"))?;
        let total = value
            .get("total_bytes")
            .and_then(Value::as_u64)
            .ok_or_else(|| field("total_bytes"))?;
        let writable = value
            .get("writable")
            .and_then(Value::as_bool)
            .ok_or_else(|| field("writable"))?;
        let kind = value
            .get("kind")
            .and_then(Value::as_str)
            .ok_or_else(|| field("kind"))?;
        let is_null = value
            .get("is_null")
            .and_then(Value::as_bool)
            .ok_or_else(|| field("is_null"))?;
        if self
            .session
            .as_deref()
            .is_some_and(|current| current != session)
        {
            return Err(AppError::invalid("值会话响应不匹配"));
        }
        if self.session.is_some() && revision < self.revision {
            return Err(AppError::conflict("值会话响应已过期"));
        }
        self.invalidate_preview();
        self.session = Some(session.to_owned());
        self.revision = revision;
        self.total = total;
        self.writable = writable;
        self.kind = kind.to_owned();
        if !is_null && self.native_kind.is_none() {
            self.native_kind = Some(kind.to_owned());
        }
        self.null_kind = None;
        self.is_null = is_null;
        self.loading_bytes.clear();
        self.original_window.clear();
        self.editor = Editor::default();
        self.offset = 0;
        if is_null {
            return Ok(None);
        }
        if let Some(data) = value.get("data_b64").and_then(Value::as_str) {
            let offset = value.get("offset").and_then(Value::as_u64).unwrap_or(0);
            self.consume_value_chunk(offset, data)
        } else {
            Ok(self.read_request(0, self.read_size()))
        }
    }

    fn read_size(&self) -> usize {
        if self.mode == Mode::Hex { 4096 } else { 262144 }
    }

    fn consume_value_chunk(
        &mut self,
        offset: u64,
        encoded: &str,
    ) -> datapier_core::Result<Option<(&'static str, Value)>> {
        use datapier_core::model::AppError;
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|e| AppError::invalid(e.to_string()))?;
        if self.mode != Mode::Hex && self.total <= self.max_text_bytes as u64 {
            if offset != self.loading_bytes.len() as u64
                || offset.saturating_add(bytes.len() as u64) > self.total
            {
                return Err(AppError::invalid("值分块不连续"));
            }
            self.loading_bytes.extend_from_slice(&bytes);
            if (self.loading_bytes.len() as u64) < self.total {
                if bytes.is_empty() {
                    return Err(AppError::invalid("值分块为空"));
                }
                return Ok(self.read_request(self.loading_bytes.len() as u64, self.read_size()));
            }
            let mut complete = std::mem::take(&mut self.loading_bytes);
            if std::str::from_utf8(&complete).is_err() {
                self.mode = Mode::Hex;
                self.error = Some(tr("值不是有效 UTF-8，已按 Hex 显示字节。").into_owned());
                complete.truncate(4096);
            }
            self.set_window(0, complete)?;
        } else {
            // 超限的文本必须切到 Hex 分块，避免截断的 UTF-8 被当成可编辑全文。
            if self.mode != Mode::Hex {
                self.mode = Mode::Hex;
            }
            let mut bytes = bytes;
            bytes.truncate(4096);
            self.set_window(offset, bytes)?;
        }
        Ok(None)
    }

    /// 接收 value.read；派生图片和原值各自核对会话、revision 与偏移。
    pub fn read_response(
        &mut self,
        value: &Value,
    ) -> datapier_core::Result<Option<(&'static str, Value)>> {
        use datapier_core::model::AppError;
        let id = value
            .get("value_session")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("读取响应缺少值会话"))?;
        if value.get("revision").and_then(Value::as_u64) != Some(self.revision) {
            return Err(AppError::conflict("值读取版本已变化"));
        }
        let offset = value
            .get("offset")
            .and_then(Value::as_u64)
            .ok_or_else(|| AppError::invalid("读取响应缺少偏移"))?;
        let encoded = value
            .get("data_b64")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("读取响应缺少内容"))?;
        if self.preview_id.as_deref() == Some(id) {
            let bytes = STANDARD
                .decode(encoded)
                .map_err(|e| AppError::invalid(e.to_string()))?;
            if offset != self.preview_bytes.len() as u64
                || offset.saturating_add(bytes.len() as u64) > self.preview_total
                || (bytes.is_empty() && offset < self.preview_total)
            {
                return Err(AppError::invalid("图片分块不连续"));
            }
            self.preview_bytes.extend_from_slice(&bytes);
            if (self.preview_bytes.len() as u64) < self.preview_total {
                return Ok(Some((
                    "value.read",
                    json!({"value_session":id,"revision":self.revision,"offset":self.preview_bytes.len(),"max_bytes":262144}),
                )));
            }
            return Ok(None);
        }
        if self.session.as_deref() != Some(id) {
            return Err(AppError::invalid("值读取会话不匹配"));
        }
        self.consume_value_chunk(offset, encoded)
    }

    /// 图片必须经派生句柄分块读取，再由 UI 线程显式注册 PNG blob。
    pub fn image_response(
        &mut self,
        value: &Value,
    ) -> datapier_core::Result<(&'static str, Value)> {
        use datapier_core::model::AppError;
        if value.get("revision").and_then(Value::as_u64) != Some(self.revision)
            || value.get("mime").and_then(Value::as_str) != Some("image/png")
        {
            return Err(AppError::invalid("图片预览版本或类型无效"));
        }
        let id = value
            .get("preview_id")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("缺少图片预览句柄"))?;
        let total = value
            .get("total_bytes")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0 && *n <= 16 * 1024 * 1024)
            .ok_or_else(|| AppError::invalid("图片预览长度无效"))?;
        self.invalidate_preview();
        self.preview_id = Some(id.to_owned());
        self.preview_total = total;
        self.preview_bytes.clear();
        Ok((
            "value.read",
            json!({"value_session":id,"revision":self.revision,"offset":0,"max_bytes":262144}),
        ))
    }

    /// 只有完整派生 PNG 才可交给 Session::blob；调用前仍需核对活动 tab 和 revision。
    pub fn take_preview_png(&mut self) -> Option<Vec<u8>> {
        if self.preview_id.is_none()
            || self.preview_total == 0
            || self.preview_bytes.len() as u64 != self.preview_total
        {
            return None;
        }
        if let Some(id) = self.preview_id.take() {
            self.retired_previews.push((id, self.revision));
        }
        self.preview_total = 0;
        Some(std::mem::take(&mut self.preview_bytes))
    }
}

fn button(label: &str, action: &str, enabled: bool) -> Node<Msg> {
    ui::button(tr(label).into_owned())
        .variant(if action == "advanced-value-stage" {
            ui::ButtonVariant::Primary
        } else {
            ui::ButtonVariant::Default
        })
        .key(action.to_owned())
        .disabled(!enabled)
        .on_click(Msg::Action(action.to_owned()))
        .into()
}

pub(crate) fn footer(state: &ValueUi) -> html::El<Msg> {
    html::div()
        .class("modal-footer")
        .child(html::span("").class("spacer"))
        .child(button("关闭", "advanced-value-close", !state.pending))
        .child(button(
            "加入草稿",
            "advanced-value-stage",
            state.can_stage(),
        ))
}

/// 渲染当前值的窗口与操作；调用方路由动作及编辑器输入。
pub(crate) fn view(state: &ValueUi) -> Node<Msg> {
    let mut panel = html::div().class("value-editor advanced-panel");
    let mut modes = html::div().class("advanced-tabs").attr("role", "tablist");
    for mode in [Mode::Text, Mode::Hex, Mode::Json, Mode::Xml, Mode::Image] {
        modes = modes.child(
            html::button(tr(mode.label()).into_owned())
                .class(if state.mode == mode {
                    "advanced-tab active"
                } else {
                    "advanced-tab"
                })
                .attr("role", "tab")
                .attr("aria-selected", state.mode == mode)
                .attr("disabled", state.pending)
                .on_click(Msg::Action(mode.action().into())),
        );
    }
    panel = panel.child(modes);
    if state.pending {
        panel = panel.child(html::p(tr("处理中").into_owned()).class("muted"));
    }
    if !state.writable {
        panel = panel.child(html::p(tr("只读").into_owned()).class("notice"));
    }
    if let Some(error) = &state.error {
        panel = panel.child(html::p(error.clone()).class("notice"));
    }
    if let Some(message) = &state.validation {
        panel = panel.child(html::p(message.clone()).class("notice"));
    }
    if state.is_null {
        panel = panel.child(html::p(tr("NULL").into_owned()).class("notice"));
        if state.writable {
            panel = panel.child(
                html::p(tr("选择新值类型并输入内容；空内容不等于 NULL。").into_owned())
                    .class("muted"),
            );
            for &(kind, label) in VALUE_KINDS {
                let enabled = !state.pending
                    && state
                        .native_kind
                        .as_deref()
                        .is_none_or(|original| original == kind);
                panel = panel.child(button(
                    label,
                    &format!("advanced-value-null-kind:{kind}"),
                    enabled,
                ));
            }
            if let Some(kind) = &state.null_kind {
                let label = VALUE_KINDS
                    .iter()
                    .find_map(|(value, label)| (*value == kind).then_some(*label))
                    .unwrap_or("未知");
                panel = panel.child(
                    html::p(crate::i18n::format_message(
                        "新值类型：{kind}",
                        &[("kind", &tr(label))],
                    ))
                    .class("muted"),
                );
                panel = panel.child(
                    ui::editor()
                        .key("input-advanced-value")
                        .aria(tr("值").into_owned())
                        .readonly(!state.editable())
                        .text(state.editor.text.clone())
                        .cursor(state.editor.cursor)
                        .anchor(state.editor.anchor)
                        .max_lines(16u32)
                        .on_focus(Msg::Focus("advanced-value".into()))
                        .on_undo(Msg::Undo("advanced-value".into()))
                        .on_edit(|e| {
                            Msg::Edit(
                                "advanced-value".into(),
                                e.from,
                                e.to,
                                e.text.clone(),
                                e.cursor,
                                e.len,
                            )
                        }),
                );
                panel = panel.child(button(
                    "应用新值",
                    "advanced-value-replace-null",
                    state.editable(),
                ));
            }
        }
        return panel.into();
    }
    if state.allow_null && state.writable {
        panel = panel.child(button(
            "设为 NULL",
            "advanced-value-set-null",
            !state.pending,
        ));
    }
    if state.mode == Mode::Image {
        panel = panel.child(html::p(tr("生成的图片会被宿主缓存。").into_owned()).class("muted"));
        panel = panel.child(button(
            "生成预览",
            "advanced-value-preview-image",
            !state.pending,
        ));
        if let Some(blob) = &state.image_blob {
            panel = panel.child(
                ui::image()
                    .blob(blob.clone())
                    .alt(tr("图片预览").into_owned()),
            );
        }
    } else {
        panel = panel.child(
            html::p(format!(
                "{}–{} / {}",
                state.offset,
                state.offset + state.original_window.len() as u64,
                state.total
            ))
            .class("muted"),
        );
        if state.mode != Mode::Hex
            && (state.offset != 0 || state.total > state.max_text_bytes as u64)
        {
            panel = panel.child(
                html::p(tr("值超过文本编辑上限，请使用分块读取或文件操作。").into_owned())
                    .class("notice"),
            );
        }
        panel = panel.child(
            ui::editor()
                .key("input-advanced-value")
                .aria(tr("值").into_owned())
                .readonly(!state.editable())
                .text(state.editor.text.clone())
                .cursor(state.editor.cursor)
                .anchor(state.editor.anchor)
                .max_lines(16u32)
                .on_focus(Msg::Focus("advanced-value".into()))
                .on_undo(Msg::Undo("advanced-value".into()))
                .on_edit(|e| {
                    Msg::Edit(
                        "advanced-value".into(),
                        e.from,
                        e.to,
                        e.text.clone(),
                        e.cursor,
                        e.len,
                    )
                }),
        );
        if state.dirty() {
            panel = panel.child(html::p(tr("请先应用当前修改").into_owned()).class("muted"));
        }
        let mut actions = html::div().class("advanced-actions");
        if matches!(state.mode, Mode::Json | Mode::Xml) {
            actions = actions.child(button("校验", "advanced-value-validate", !state.pending));
            actions = actions.child(button("格式化", "advanced-value-format", state.editable()));
        }
        actions = actions.child(button(
            "上一页",
            "advanced-value-previous",
            !state.pending && state.offset > 0,
        ));
        actions = actions.child(button(
            "下一页",
            "advanced-value-next",
            !state.pending && state.offset + (state.original_window.len() as u64) < state.total,
        ));
        actions = actions.child(button("应用修改", "advanced-value-patch", state.editable()));
        panel = panel.child(actions);
    }
    panel.into()
}

#[cfg(test)]
#[path = "../../tests/unit/advanced_value.rs"]
mod tests;
