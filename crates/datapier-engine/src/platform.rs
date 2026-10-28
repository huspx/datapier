//! 集中处理私有数据目录、进程脱离和 IPC 的平台差异。
//!
//! 路径散列仅缩短端点名；实例互斥依靠数据目录中的 OS 锁，访问隔离依靠目录
//! 权限或当前用户 SID 的 DACL。这里不把 PID、socket 文件或管道名称当作存活证明。

use datapier_core::{AppError, Result};
use std::{
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::io::{AsyncRead, AsyncWrite};

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type Stream = Box<dyn Io>;

/// 创建并规范化用户专属目录；已有不安全目录报错，不替用户修改其原有权限。
pub fn root(path: &Path) -> Result<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)?;
        let root = std::fs::canonicalize(path)?;
        let metadata = std::fs::metadata(&root)?;
        // SAFETY: geteuid 不接收指针或访问 Rust 内存，只读取当前进程的有效 UID。
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(AppError::new(
                "permission_denied",
                format!(
                    "数据目录 {} 不属于当前用户或允许其他用户访问；请使用当前用户专属的私有目录",
                    root.display()
                ),
            ));
        }
        Ok(root)
    }
    #[cfg(windows)]
    {
        windows::private_root(path)
    }
}
/// 为已规范化的数据目录构造当前用户的短端点名，并检查 Unix 运行目录权限。
///
/// 调用前须先经过 root；同目录的候选进程仍须争取实例锁，不能仅凭路径散列启动服务。
pub fn endpoint(root: &Path) -> Result<String> {
    let mut hash = std::collections::hash_map::DefaultHasher::new();
    root.hash(&mut hash);
    let id = hash.finish();
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt};
        // SAFETY: geteuid 无参数且无需额外资源初始化，返回值仅用于目录归属校验。
        let uid = unsafe { libc::geteuid() };
        let dir = PathBuf::from("/tmp").join(format!("datapier-{uid}"));
        match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        let meta = std::fs::symlink_metadata(&dir)?;
        if !meta.is_dir() || meta.uid() != uid || meta.mode() & 0o077 != 0 {
            return Err(AppError::new("permission_denied", "IPC 目录权限不安全"));
        }
        Ok(dir
            .join(format!("{id:016x}.sock"))
            .to_string_lossy()
            .into_owned())
    }
    #[cfg(windows)]
    {
        Ok(format!(r"\\.\pipe\datapier-{}-{id:016x}", windows::sid()?))
    }
}
/// 启动 Engine 候选并隔离客户端 stdio；成功只表示进程已创建，不表示握手就绪。
pub fn spawn(root: &Path) -> Result<()> {
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("engine")
        .arg("--data-dir")
        .arg(root)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: fork 后的回调仅调用 async-signal-safe 的 setsid 并读取 errno，
        // 不获取 Rust 锁、不分配内存，也不访问来自其他线程的借用。
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // Win32 CREATE_NO_WINDOW：Engine 不需要新控制台，也不能继续持有 UI 的 PTY。
        cmd.creation_flags(0x08000000);
    }
    cmd.spawn()?;
    Ok(())
}
#[cfg(unix)]
pub struct Listener(tokio::net::UnixListener);
#[cfg(unix)]
impl Listener {
    /// 绑定私有 UDS；调用方必须已持有实例锁，才可删除上次崩溃留下的 socket。
    pub fn bind(name: &str) -> Result<Self> {
        match std::fs::remove_file(name) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        let listener = tokio::net::UnixListener::bind(name)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(name, std::fs::Permissions::from_mode(0o600))?;
        Ok(Self(listener))
    }
    pub async fn accept(&mut self) -> Result<Stream> {
        Ok(Box::new(self.0.accept().await?.0))
    }
}
#[cfg(unix)]
pub async fn connect(name: &str) -> Result<Stream> {
    Ok(Box::new(tokio::net::UnixStream::connect(name).await?))
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr::null_mut};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree},
        Security::{
            ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                ConvertStringSidToSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
            },
            DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
            GetTokenInformation, OWNER_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::CreateDirectoryW,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    // Convert* 分配的缓冲区由 LocalFree 释放；禁止复制此所有权包装。
    struct Local(*mut c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            // SAFETY: 非空指针仅来自成功的 Win32 LocalAlloc 系列输出，且由本包装独占释放。
            unsafe {
                if !self.0.is_null() {
                    LocalFree(self.0);
                }
            }
        }
    }
    fn wide(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }
    fn unsafe_directory(path: &Path) -> AppError {
        AppError::new(
            "permission_denied",
            format!(
                "数据目录 {} 的所有者或访问权限不安全；请使用仅当前用户可访问的私有目录",
                path.display()
            ),
        )
    }
    fn create_private(path: &Path, user: &str) -> Result<()> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
            Err(error) => return Err(error.into()),
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            create_private(parent, user)?;
        }
        // SAFETY: SDDL 与路径均以 NUL 结尾，descriptor 成功后立即交给 Local 管理；
        // SECURITY_ATTRIBUTES 和其引用的描述符覆盖 CreateDirectoryW 的整个同步调用。
        unsafe {
            let sddl: Vec<u16> = format!("O:{user}D:P(A;OICI;GA;;;{user})")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor = null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let _owned = Local(descriptor);
            let attrs = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            if CreateDirectoryW(wide(path).as_ptr(), &attrs) == 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
        }
        Ok(())
    }
    // current_sid 由 private_root 中的 Local 持有，整个递归验证期间保持有效。
    fn verify_acl(path: &Path, current_sid: *mut c_void, root: bool) -> Result<()> {
        // SAFETY: Win32 返回的 owner、ACL 和 ACE 都借用于 descriptor，在 _owned
        // 释放前使用；先检查空指针及 ACE 类型、长度，再按 ACCESS_ALLOWED_ACE 读取。
        unsafe {
            let mut owner = null_mut();
            let mut acl: *mut ACL = null_mut();
            let mut descriptor = null_mut();
            let status = GetNamedSecurityInfoW(
                wide(path).as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                null_mut(),
                &mut acl,
                null_mut(),
                &mut descriptor,
            );
            if status != 0 {
                return Err(std::io::Error::from_raw_os_error(status as i32).into());
            }
            let _owned = Local(descriptor);
            if owner.is_null() || EqualSid(owner, current_sid) == 0 || acl.is_null() {
                return Err(unsafe_directory(path));
            }
            let mut control = 0u16;
            let mut revision = 0u32;
            if GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            // SE_DACL_PROTECTED（0x1000）禁止根目录从宽松父目录继承访问许可。
            if root && control & 0x1000 == 0 {
                return Err(unsafe_directory(path));
            }
            let mut inheritable = false;
            for index in 0..(*acl).AceCount as u32 {
                let mut ace = null_mut();
                if GetAce(acl, index, &mut ace) == 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                let header = &*(ace as *const ACE_HEADER);
                if header.AceType != 0
                    || (header.AceSize as usize) < std::mem::size_of::<ACCESS_ALLOWED_ACE>()
                {
                    return Err(unsafe_directory(path));
                }
                let allowed = &*(ace as *const ACCESS_ALLOWED_ACE);
                if allowed.Header.AceType != 0
                    || EqualSid(
                        (&allowed.SidStart as *const u32 as *mut u32).cast(),
                        current_sid,
                    ) == 0
                {
                    return Err(unsafe_directory(path));
                }
                // OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE（0x03）使新文件和子目录同样受保护。
                if allowed.Header.AceFlags & 0x03 == 0x03 {
                    inheritable = true;
                }
            }
            if root && !inheritable {
                return Err(unsafe_directory(path));
            }
            Ok(())
        }
    }
    fn verify_tree(path: &Path, current_sid: *mut c_void, is_root: bool) -> Result<()> {
        let metadata = std::fs::symlink_metadata(path)?;
        if metadata.file_type().is_symlink() {
            return Err(unsafe_directory(path));
        }
        verify_acl(path, current_sid, is_root)?;
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path)? {
                verify_tree(&entry?.path(), current_sid, false)?;
            }
        }
        Ok(())
    }
    pub fn private_root(path: &Path) -> Result<PathBuf> {
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()?.join(path)
        };
        let user = sid()?;
        create_private(&absolute, &user)?;
        let root = std::fs::canonicalize(&absolute)?;
        if !std::fs::metadata(&root)?.is_dir() {
            return Err(unsafe_directory(&root));
        }
        // SAFETY: SID 文本来自 sid()，NUL 结尾缓冲区覆盖转换调用；Local 保持原生 SID
        // 有效直到 verify_tree 的全部递归调用结束，然后只释放一次。
        unsafe {
            let user_wide: Vec<u16> = user.encode_utf16().chain(Some(0)).collect();
            let mut current_sid = null_mut();
            if ConvertStringSidToSidW(user_wide.as_ptr(), &mut current_sid) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let _owned = Local(current_sid);
            verify_tree(&root, current_sid, true)?;
        }
        Ok(root)
    }
    pub fn sid() -> Result<String> {
        // SAFETY: token 是本函数独占的查询句柄，GetTokenInformation 失败及成功路径均关闭。
        // 按返回字节数向上取整的 usize 缓冲区满足 TOKEN_USER 的容量和对齐要求；SID 指针
        // 在 data 存活时读取。转换出的 NUL 结尾字符串复制为 Rust String 后再 LocalFree。
        unsafe {
            let mut token = null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut size = 0;
            GetTokenInformation(token, TokenUser, null_mut(), 0, &mut size);
            let mut data = vec![
                0usize;
                (size as usize + std::mem::size_of::<usize>() - 1)
                    / std::mem::size_of::<usize>()
            ];
            if GetTokenInformation(token, TokenUser, data.as_mut_ptr().cast(), size, &mut size) == 0
            {
                CloseHandle(token);
                return Err(std::io::Error::last_os_error().into());
            }
            CloseHandle(token);
            let mut string = null_mut();
            if ConvertSidToStringSidW(
                (*(data.as_ptr() as *const TOKEN_USER)).User.Sid,
                &mut string,
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut len = 0;
            while *string.add(len) != 0 {
                len += 1;
            }
            let result = String::from_utf16_lossy(std::slice::from_raw_parts(string, len));
            LocalFree(string.cast());
            Ok(result)
        }
    }
    pub fn pipe(
        name: &str,
        first: bool,
    ) -> Result<tokio::net::windows::named_pipe::NamedPipeServer> {
        // SAFETY: SDDL 描述符和 SECURITY_ATTRIBUTES 覆盖管道创建调用的完整生命周期；
        // 创建成功后的句柄归 Tokio 所有，调用结束后才能释放临时描述符。
        unsafe {
            let sddl: Vec<u16> = format!("D:P(A;;GA;;;{})", sid()?)
                .encode_utf16()
                .chain(Some(0))
                .collect();
            let mut descriptor: *mut c_void = null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                null_mut(),
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut attrs = SECURITY_ATTRIBUTES {
                nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: descriptor,
                bInheritHandle: 0,
            };
            let result = tokio::net::windows::named_pipe::ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    (&mut attrs as *mut SECURITY_ATTRIBUTES).cast(),
                );
            LocalFree(descriptor.cast());
            result.map_err(Into::into)
        }
    }
}
#[cfg(windows)]
pub struct Listener {
    name: String,
    next: tokio::net::windows::named_pipe::NamedPipeServer,
}
#[cfg(windows)]
impl Listener {
    pub fn bind(name: &str) -> Result<Self> {
        Ok(Self {
            name: name.into(),
            next: windows::pipe(name, true)?,
        })
    }
    /// 接受一个客户端，并在交出旧实例前先建立下一实例，避免管道名出现无人监听的空档。
    pub async fn accept(&mut self) -> Result<Stream> {
        self.next.connect().await?;
        let next = windows::pipe(&self.name, false)?;
        Ok(Box::new(std::mem::replace(&mut self.next, next)))
    }
}
#[cfg(windows)]
pub async fn connect(name: &str) -> Result<Stream> {
    Ok(Box::new(
        tokio::net::windows::named_pipe::ClientOptions::new().open(name)?,
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[test]
    fn creates_private_directories_and_rejects_existing_shared_directory_without_chmod() {
        let base =
            std::env::temp_dir().join(format!("datapier-private-test-{}", uuid::Uuid::new_v4()));
        let private = base.join("new").join("data");
        let canonical = root(&private).unwrap();
        assert_eq!(std::fs::metadata(&canonical).unwrap().mode() & 0o777, 0o700);
        assert_eq!(
            std::fs::metadata(base.join("new")).unwrap().mode() & 0o777,
            0o700
        );
        let shared = base.join("shared");
        std::fs::create_dir(&shared).unwrap();
        std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(root(&shared).unwrap_err().code, "permission_denied");
        assert_eq!(std::fs::metadata(&shared).unwrap().mode() & 0o777, 0o755);
        std::fs::remove_dir_all(base).unwrap();
    }
}
