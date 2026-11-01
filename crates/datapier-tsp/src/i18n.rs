//! 原生界面语言与静态消息。只翻译明确的界面文案，不处理数据或后端错误。
mod catalog;

use catalog::CATALOG;
use datapier_core::model::LanguagePreference;
use std::borrow::Cow;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Language {
    ZhCn,
    En,
}

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
        && let Some((_, english)) = CATALOG.iter().find(|(chinese, _)| *chinese == text)
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

// 按首选项单独判断；脚本覆盖地域，不能从后续语言推断用户偏好。
fn normalize_locale(raw: &str) -> Language {
    let raw = raw.trim().split(['.', '@']).next().unwrap_or("");
    let mut parts = raw.split(['-', '_']);
    if !parts
        .next()
        .is_some_and(|part| part.eq_ignore_ascii_case("zh"))
    {
        return Language::En;
    }
    let mut script = None;
    let mut region = None;
    for part in parts {
        if part.len() == 4 && part.bytes().all(|b| b.is_ascii_alphabetic()) {
            if script.is_some() {
                return Language::En;
            }
            script = Some(part);
        } else if (part.len() == 2 && part.bytes().all(|b| b.is_ascii_alphabetic()))
            || (part.len() == 3 && part.bytes().all(|b| b.is_ascii_digit()))
        {
            if region.is_some() {
                return Language::En;
            }
            region = Some(part);
        } else {
            return Language::En;
        }
    }
    if let Some(script) = script {
        return if script.eq_ignore_ascii_case("hans") {
            Language::ZhCn
        } else {
            Language::En
        };
    }
    match region {
        None => Language::ZhCn,
        Some(region) if region.eq_ignore_ascii_case("cn") || region.eq_ignore_ascii_case("sg") => {
            Language::ZhCn
        }
        _ => Language::En,
    }
}

#[cfg(target_os = "macos")]
fn detect_system_language() -> Language {
    use std::ffi::{CStr, c_char, c_void};
    type CfRef = *const c_void;
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFLocaleCopyPreferredLanguages() -> CfRef;
        fn CFArrayGetCount(array: CfRef) -> isize;
        fn CFArrayGetValueAtIndex(array: CfRef, index: isize) -> CfRef;
        fn CFStringGetCString(string: CfRef, buffer: *mut c_char, size: isize, encoding: u32)
        -> u8;
        fn CFRelease(object: CfRef);
    }
    // SAFETY: CoreFoundation 返回的数组持有其字符串；释放前读取首项并复制到本地缓冲区。
    unsafe {
        let languages = CFLocaleCopyPreferredLanguages();
        if languages.is_null() {
            return Language::En;
        }
        let mut buffer = [0 as c_char; 512];
        let language = if CFArrayGetCount(languages) > 0 {
            let first = CFArrayGetValueAtIndex(languages, 0);
            if !first.is_null()
                && CFStringGetCString(
                    first,
                    buffer.as_mut_ptr(),
                    buffer.len() as isize,
                    0x0800_0100,
                ) != 0
            {
                CStr::from_ptr(buffer.as_ptr())
                    .to_str()
                    .map(normalize_locale)
                    .unwrap_or(Language::En)
            } else {
                Language::En
            }
        } else {
            Language::En
        };
        CFRelease(languages);
        language
    }
}

#[cfg(target_os = "windows")]
fn detect_system_language() -> Language {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetUserDefaultUILanguage() -> u16;
        fn LCIDToLocaleName(locale: u32, name: *mut u16, count: i32, flags: u32) -> i32;
    }
    let mut name = [0u16; 85];
    // SAFETY: 缓冲区可写且长度与传入长度一致；LANGID 可作为默认排序的 LCID。
    let count = unsafe {
        LCIDToLocaleName(
            GetUserDefaultUILanguage() as u32,
            name.as_mut_ptr(),
            name.len() as i32,
            0,
        )
    };
    if count <= 1 {
        return Language::En;
    }
    String::from_utf16(&name[..count as usize - 1])
        .map(|name| normalize_locale(&name))
        .unwrap_or(Language::En)
}

#[cfg(not(any(target_os = "macos", target_os = "windows")))]
fn detect_system_language() -> Language {
    // POSIX 消息类别遵循 LC_ALL、LC_MESSAGES、LANG 优先级。
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(value) = std::env::var_os(key).filter(|value| !value.is_empty()) {
            return value.to_str().map(normalize_locale).unwrap_or(Language::En);
        }
    }
    Language::En
}

#[cfg(test)]
#[path = "../tests/unit/i18n.rs"]
mod tests;
