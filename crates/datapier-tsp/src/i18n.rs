//! 原生界面语言与静态消息。只翻译明确的界面文案，不处理数据或后端错误。
mod catalog;

use catalog::CATALOG;
pub(super) use datapier_core::locale::Language;
use datapier_core::locale::detect_system_language;
use datapier_core::model::LanguagePreference;
use std::borrow::Cow;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

// 偏好由 Engine 的全局设置同步；系统语言属于 UI 进程，不属于 Engine。
static PREFERENCE: AtomicU8 = AtomicU8::new(0);
static SYSTEM_LANGUAGE: LazyLock<AtomicU8> =
    LazyLock::new(|| AtomicU8::new(language_code(detect_system_language())));

fn system_slot() -> &'static AtomicU8 {
    &SYSTEM_LANGUAGE
}

fn language_code(language: Language) -> u8 {
    match language {
        Language::ZhCn => 1,
        Language::En => 0,
    }
}

/// 应用设置变化时调用；不创建或替换工作区状态。
pub(super) fn set_preference(preference: LanguagePreference) {
    let code = match preference {
        LanguagePreference::System => 0,
        LanguagePreference::ZhCn => 1,
        LanguagePreference::En => 2,
    };
    PREFERENCE.store(code, Ordering::Relaxed);
}

/// 检查系统语言变化；返回实际界面语言是否发生变化。
pub(super) fn refresh_system_language() -> bool {
    let before = resolved();
    system_slot().store(language_code(detect_system_language()), Ordering::Relaxed);
    before != resolved()
}

pub(super) fn resolved() -> Language {
    match PREFERENCE.load(Ordering::Relaxed) {
        1 => Language::ZhCn,
        2 => Language::En,
        _ if system_slot().load(Ordering::Relaxed) == 1 => Language::ZhCn,
        _ => Language::En,
    }
}

/// 英文及未知文案均可借用，调用方须仅传入已知 UI 文案。
pub(super) fn tr(text: &str) -> Cow<'_, str> {
    if resolved() == Language::En
        && let Some((_, english)) = CATALOG
            .iter()
            .flat_map(|entries| entries.iter())
            .find(|(chinese, _)| *chinese == text)
    {
        return Cow::Borrowed(english);
    }
    Cow::Borrowed(text)
}

/// 对明确的消息模板插值；变量直接写入，不会再次翻译或替换。
pub(super) fn format_message(template: &str, values: &[(&str, &str)]) -> String {
    let translated = tr(template);
    let mut output = String::with_capacity(translated.len());
    let mut rest = translated.as_ref();
    while let Some(open) = rest.find('{') {
        output.push_str(&rest[..open]);
        let suffix = &rest[open + 1..];
        let Some(close) = suffix.find('}') else {
            output.push_str(&rest[open..]);
            return output;
        };
        let name = &suffix[..close];
        if let Some((_, value)) = values.iter().find(|(key, _)| *key == name) {
            output.push_str(value);
        } else {
            output.push('{');
            output.push_str(name);
            output.push('}');
        }
        rest = &suffix[close + 1..];
    }
    output.push_str(rest);
    output
}

/// 消息参数区分界面词条与原始值，连接名、SQL、用户输入和数据库错误只能用 Raw。
#[derive(Clone, Debug)]
pub(super) enum UiArg {
    Key(&'static str),
    Raw(String),
    Message(Box<UiMessage>),
}

/// 保留消息来源，使语言切换时能重新渲染，同时不翻译数据库错误或用户数据。
#[derive(Clone, Debug, Default)]
pub(super) enum UiMessage {
    Key(&'static str),
    Template(&'static str, Vec<(&'static str, UiArg)>),
    Raw(String),
    // 仅供产生固定本地词条的编辑器／网格校验路径，禁止传入数据库错误或用户数据。
    Local(String),
    #[default]
    Empty,
    Lines(Vec<UiMessage>),
    Concat(Vec<UiMessage>),
}

impl UiMessage {
    pub(super) fn key(key: &'static str) -> Self {
        Self::Key(key)
    }

    pub(super) fn template(template: &'static str, values: Vec<(&'static str, UiArg)>) -> Self {
        Self::Template(template, values)
    }

    pub(super) fn raw(text: impl Into<String>) -> Self {
        Self::Raw(text.into())
    }

    pub(super) fn local(text: impl Into<String>) -> Self {
        Self::Local(text.into())
    }

    pub(super) fn is_empty(&self) -> bool {
        self.render().is_empty()
    }

    pub(super) fn clear(&mut self) {
        *self = Self::Empty;
    }

    /// 逐条保留来源，CLI 历史中的原始错误与本地提示可混合显示。
    pub(super) fn push_line(&mut self, message: Self) {
        match self {
            Self::Empty => *self = Self::Lines(vec![message]),
            Self::Lines(lines) => lines.push(message),
            _ => {
                let previous = std::mem::take(self);
                *self = Self::Lines(vec![previous, message]);
            }
        }
    }

    pub(super) fn render(&self) -> Cow<'_, str> {
        match self {
            Self::Key(key) => tr(key),
            Self::Raw(text) => Cow::Borrowed(text),
            Self::Local(text) => tr(text),
            Self::Empty => Cow::Borrowed(""),
            Self::Template(template, values) => {
                let rendered: Vec<_> = values
                    .iter()
                    .map(|(name, value)| {
                        (
                            *name,
                            match value {
                                UiArg::Key(key) => tr(key),
                                UiArg::Raw(text) => Cow::Borrowed(text.as_str()),
                                UiArg::Message(message) => message.render(),
                            },
                        )
                    })
                    .collect();
                let arguments: Vec<_> = rendered
                    .iter()
                    .map(|(name, value)| (*name, value.as_ref()))
                    .collect();
                Cow::Owned(format_message(template, &arguments))
            }
            Self::Lines(lines) => {
                let mut text = String::new();
                for (index, line) in lines.iter().enumerate() {
                    if index > 0 {
                        text.push('\n');
                    }
                    text.push_str(&line.render());
                }
                Cow::Owned(text)
            }
            Self::Concat(parts) => {
                let mut text = String::new();
                for part in parts {
                    text.push_str(&part.render());
                }
                Cow::Owned(text)
            }
        }
    }
}

impl From<&'static str> for UiMessage {
    fn from(key: &'static str) -> Self {
        Self::Key(key)
    }
}

impl From<String> for UiMessage {
    fn from(text: String) -> Self {
        Self::Raw(text)
    }
}

/// 本地 UI 进程所在平台的多选修饰键；远程宿主需以宿主能力为准。
pub(super) fn modifier_label() -> &'static str {
    if cfg!(target_os = "macos") {
        "⌘"
    } else {
        "Ctrl"
    }
}

/// 官方启动路径使用本地终端，因此按 UI 进程系统选择修饰键。
pub(super) fn multi_select_modifier(meta: bool, ctrl: bool) -> bool {
    if cfg!(target_os = "macos") {
        meta
    } else {
        ctrl
    }
}
