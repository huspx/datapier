//! 系统首选语言解析，供配置界面与原生工作台共用。

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Language {
    ZhCn,
    En,
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

/// 返回当前系统首选语言在应用中的映射。
#[cfg(target_os = "macos")]
pub fn detect_system_language() -> Language {
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

/// 返回当前系统首选语言在应用中的映射。
#[cfg(target_os = "windows")]
pub fn detect_system_language() -> Language {
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

/// 返回当前系统首选语言在应用中的映射。
#[cfg(not(any(target_os = "macos", target_os = "windows")))]
pub fn detect_system_language() -> Language {
    // POSIX 消息类别遵循 LC_ALL、LC_MESSAGES、LANG 优先级。
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Some(value) = std::env::var_os(key).filter(|value| !value.is_empty()) {
            return value.to_str().map(normalize_locale).unwrap_or(Language::En);
        }
    }
    Language::En
}

#[cfg(test)]
#[path = "../tests/unit/locale.rs"]
mod tests;
