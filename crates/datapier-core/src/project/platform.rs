//! 平台提供的不覆盖目录原子发布。
use crate::model::{AppError, Result};
use std::path::Path;

#[cfg(unix)]
fn c_path(path: &Path) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| AppError::invalid("路径包含 NUL"))
}

#[cfg(target_os = "macos")]
pub(super) fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    unsafe extern "C" {
        fn renamex_np(
            from: *const std::ffi::c_char,
            to: *const std::ffi::c_char,
            flags: std::ffi::c_uint,
        ) -> std::ffi::c_int;
    }
    let from = c_path(from)?;
    let to = c_path(to)?;
    // SAFETY: 两个 CString 始终存活且以 NUL 结尾，RENAME_EXCL 禁止替换已有路径。
    let result = unsafe { renamex_np(from.as_ptr(), to.as_ptr(), 0x00000004) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(target_os = "linux")]
pub(super) fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    unsafe extern "C" {
        fn renameat2(
            olddirfd: std::ffi::c_int,
            old: *const std::ffi::c_char,
            newdirfd: std::ffi::c_int,
            new: *const std::ffi::c_char,
            flags: std::ffi::c_uint,
        ) -> std::ffi::c_int;
    }
    let from = c_path(from)?;
    let to = c_path(to)?;
    // SAFETY: 路径是有效的 NUL 结尾 CString；AT_FDCWD 和 RENAME_NOREPLACE 为 Linux ABI 常量。
    let result = unsafe { renameat2(-100, from.as_ptr(), -100, to.as_ptr(), 1) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(windows)]
pub(super) fn rename_noreplace(from: &Path, to: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileW(from: *const u16, to: *const u16) -> i32;
    }
    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    if from[..from.len() - 1].contains(&0) || to[..to.len() - 1].contains(&0) {
        return Err(AppError::invalid("路径包含 NUL"));
    }
    // SAFETY: 两个 UTF-16 数组均以 NUL 结尾并在调用期间存活；MoveFileW 不替换目标。
    let result = unsafe { MoveFileW(from.as_ptr(), to.as_ptr()) };
    if result != 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error().into())
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(super) fn rename_noreplace(_from: &Path, _to: &Path) -> Result<()> {
    Err(AppError::new("unsupported", "本平台不支持安全发布项目目录"))
}
