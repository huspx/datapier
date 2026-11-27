use fs2::FileExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Deserialize)]
struct Active {
    binary: PathBuf,
    binary_hash: String,
    data_dir: PathBuf,
    plugin_dir: PathBuf,
}

fn active_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or("固定入口没有父目录")?;
    #[cfg(unix)]
    let directory = directory.join(".datapier");
    Ok(directory.join("active.json"))
}

fn read_active(path: &Path) -> Result<Active, Box<dyn std::error::Error>> {
    Ok(serde_json::from_slice(&std::fs::read(path)?)?)
}

fn same_dir(path: &Path, expected: &Path) -> bool {
    path.is_absolute() && std::fs::canonicalize(path).is_ok_and(|p| p == expected)
}

fn check_data_dir(expected: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(path) = std::env::var_os("DATAPIER_DATA_DIR")
        && !same_dir(Path::new(&path), expected)
    {
        return Err("DATAPIER_DATA_DIR 与固定入口不一致".into());
    }
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--" {
            break;
        }
        if arg == "--data-dir-b64" || arg.to_string_lossy().starts_with("--data-dir-b64=") {
            return Err("固定入口不接受 --data-dir-b64，请使用登记的数据目录".into());
        }
        if arg == "--data-dir" {
            let value = args.next().ok_or("--data-dir 缺少参数")?;
            if !same_dir(Path::new(&value), expected) {
                return Err("--data-dir 与固定入口不一致".into());
            }
        } else if let Some(value) = arg.to_str().and_then(|v| v.strip_prefix("--data-dir="))
            && !same_dir(Path::new(value), expected)
        {
            return Err("--data-dir 与固定入口不一致".into());
        }
    }
    Ok(())
}

fn prepare() -> Result<(Active, std::fs::File), Box<dyn std::error::Error>> {
    let path = active_path()?;
    let first = read_active(&path)?;
    if !first.data_dir.is_absolute() {
        return Err("安装记录的数据目录不是绝对路径".into());
    }
    let root = std::fs::canonicalize(&first.data_dir)?;
    check_data_dir(&root)?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("update-admission.lock"))?;
    FileExt::try_lock_shared(&lock)?;
    if root.join("update-gate.json").exists() {
        return Err("更新尚未完成，须先恢复或人工处理".into());
    }
    let active = read_active(&path)?;
    if !same_dir(&active.data_dir, &root) {
        return Err("固定入口的数据目录已变化，请重启".into());
    }
    let manifest = std::fs::read_to_string(active.plugin_dir.join("release-bin-sha256.txt"))?;
    if !manifest.lines().any(|line| {
        line.split_once("  ")
            .is_some_and(|(hash, _)| hash == active.binary_hash)
    }) {
        return Err("插件与固定入口版本不一致，请通过受管切换修复".into());
    }
    if !active.binary.is_absolute() || !active.binary.is_file() {
        return Err("安装记录中的程序不是可用的绝对路径".into());
    }
    if std::fs::canonicalize(&active.binary)? == std::fs::canonicalize(std::env::current_exe()?)? {
        return Err("固定入口不能指向自身".into());
    }
    let mut file = std::fs::File::open(&active.binary)?;
    let identity = file.metadata()?;
    if !identity.is_file() {
        return Err("固定入口目标不是普通文件".into());
    }
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let size = file.read(&mut buffer)?;
        if size == 0 {
            break;
        }
        hash.update(&buffer[..size]);
    }
    if format!("{:x}", base16ct::HexDisplay(&hash.finalize())) != active.binary_hash {
        return Err("固定入口程序摘要不匹配".into());
    }
    let current = std::fs::metadata(&active.binary)?;
    if current.len() != identity.len() || current.modified()? != identity.modified()? {
        return Err("固定入口程序在校验期间发生变化".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if current.dev() != identity.dev() || current.ino() != identity.ino() {
            return Err("固定入口程序身份已变化".into());
        }
    }
    // SAFETY: 启动子进程前 launcher 只有一个线程。
    unsafe { std::env::set_var("DATAPIER_DATA_DIR", &root) };
    Ok((active, lock))
}

#[cfg(windows)]
mod windows {
    use std::ffi::OsStr;
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::windows::{ffi::OsStrExt, io::AsRawHandle};
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::{
        CloseHandle, DUPLICATE_SAME_ACCESS, DuplicateHandle, HANDLE, INVALID_HANDLE_VALUE,
        WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    use windows_sys::Win32::System::Threading::{
        CreateProcessW, DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT,
        GetCurrentProcess, GetExitCodeProcess, INFINITE, InitializeProcThreadAttributeList,
        PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_INFORMATION,
        STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute, WaitForSingleObject,
    };

    struct Handle(HANDLE);
    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: Handle 独占一个有效的非空 Windows 句柄。
            unsafe { CloseHandle(self.0) };
        }
    }

    struct Attributes(*mut core::ffi::c_void);
    impl Drop for Attributes {
        fn drop(&mut self) {
            // SAFETY: 属性列表已初始化，且底层缓冲区仍然存活。
            unsafe { DeleteProcThreadAttributeList(self.0) };
        }
    }

    fn wide(value: &OsStr) -> Vec<u16> {
        value.encode_wide().chain(Some(0)).collect()
    }

    // Windows CRT 解析 argv 时，引号前及结尾的反斜杠需要加倍。
    fn append_arg(line: &mut Vec<u16>, arg: &OsStr) {
        line.push(b'"' as u16);
        let mut slashes = 0;
        for ch in arg.encode_wide() {
            if ch == b'\\' as u16 {
                slashes += 1;
                continue;
            }
            let count = if ch == b'"' as u16 {
                slashes * 2 + 1
            } else {
                slashes
            };
            line.extend(std::iter::repeat_n(b'\\' as u16, count));
            line.push(ch);
            slashes = 0;
        }
        line.extend(std::iter::repeat_n(b'\\' as u16, slashes * 2));
        line.push(b'"' as u16);
    }

    fn job() -> io::Result<Handle> {
        // SAFETY: 空安全属性及名称创建匿名且不可继承的 Job。
        let raw = unsafe { CreateJobObjectW(null(), null()) };
        if raw.is_null() {
            return Err(io::Error::last_os_error());
        }
        let job = Handle(raw);
        // SAFETY: 全零是此 Windows 结构的有效初始表示。
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
        // Engine 在客户端间共享，必须脱离 MCP 所属 Job；其他后代也会脱离，MCP 本身仍受约束。
        limits.BasicLimitInformation.LimitFlags =
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_SILENT_BREAKAWAY_OK;
        // SAFETY: limits 指向已初始化且大小正确的结构。
        if unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(job)
    }

    fn inherited_stream(id: u32) -> io::Result<Handle> {
        // SAFETY: id 指定标准流。句柄复制到同一进程，副本明确允许继承。
        let raw = unsafe { GetStdHandle(id) };
        let fallback = if raw.is_null() || raw == INVALID_HANDLE_VALUE {
            Some(
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("NUL")?,
            )
        } else {
            None
        };
        let raw = fallback.as_ref().map_or(raw, AsRawHandle::as_raw_handle);
        let mut inherited = null_mut();
        // SAFETY: 复制时标准流或 NUL 仍有效，输出指针指向可写的句柄。
        unsafe {
            let current = GetCurrentProcess();
            if DuplicateHandle(
                current,
                raw,
                current,
                &mut inherited,
                0,
                1,
                DUPLICATE_SAME_ACCESS,
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(Handle(inherited))
    }

    fn run() -> Result<u32, Box<dyn std::error::Error>> {
        let (active, _admission) = super::prepare()?;
        let binary = active.binary;
        let job = job()?;
        let stdin = inherited_stream(STD_INPUT_HANDLE)?;
        let stdout = inherited_stream(STD_OUTPUT_HANDLE)?;
        let stderr = inherited_stream(STD_ERROR_HANDLE)?;
        let streams = [stdin.0, stdout.0, stderr.0];
        let mut size = 0usize;
        // SAFETY: 首次传入空指针只查询属性列表所需长度。
        unsafe { InitializeProcThreadAttributeList(null_mut(), 2, 0, &mut size) };
        if size == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut storage = vec![0usize; size.div_ceil(size_of::<usize>())];
        let list = storage.as_mut_ptr().cast();
        // SAFETY: storage 长度和对齐满足要求，并在后续调用期间存活。
        if unsafe { InitializeProcThreadAttributeList(list, 2, 0, &mut size) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        let _attributes = Attributes(list);
        // 创建时原子加入 Job，避免 launcher 在创建与分配之间终止时留下暂停的子进程。
        // SAFETY: job.0 是有效句柄，保持存活至子进程退出。
        if unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                &job.0 as *const _ as *const _,
                size_of::<HANDLE>(),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        // SAFETY: 三个可继承句柄及数组在 CreateProcessW 返回前保持有效。
        if unsafe {
            UpdateProcThreadAttribute(
                list,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                streams.as_ptr().cast(),
                size_of::<[HANDLE; 3]>(),
                null_mut(),
                null(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        let mut line = Vec::new();
        append_arg(&mut line, binary.as_os_str());
        for arg in std::env::args_os().skip(1) {
            line.push(b' ' as u16);
            append_arg(&mut line, &arg);
        }
        line.push(0);
        let application = wide(binary.as_os_str());
        // SAFETY: 全零是这两个 Windows 结构的有效初始值。
        let mut startup: STARTUPINFOEXW = unsafe { zeroed() };
        startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        startup.StartupInfo.hStdInput = stdin.0;
        startup.StartupInfo.hStdOutput = stdout.0;
        startup.StartupInfo.hStdError = stderr.0;
        startup.lpAttributeList = list;
        let mut process: PROCESS_INFORMATION = unsafe { zeroed() };
        // SAFETY: 指针在调用期间有效，命令行可写且以 NUL 结尾；可继承句柄传递 stdio。
        if unsafe {
            CreateProcessW(
                application.as_ptr(),
                line.as_mut_ptr(),
                null(),
                null(),
                1,
                EXTENDED_STARTUPINFO_PRESENT,
                null(),
                null(),
                &startup.StartupInfo,
                &mut process,
            )
        } == 0
        {
            return Err(io::Error::last_os_error().into());
        }
        drop((stdin, stdout, stderr));
        let child = Handle(process.hProcess);
        let _thread = Handle(process.hThread);
        // SAFETY: child 拥有有效进程句柄，等待及获取退出码期间不会释放。
        if unsafe { WaitForSingleObject(child.0, INFINITE) } != WAIT_OBJECT_0 {
            return Err(io::Error::last_os_error().into());
        }
        let mut code = 0;
        // SAFETY: code 可写且 child 是已退出进程的有效句柄。
        if unsafe { GetExitCodeProcess(child.0, &mut code) } == 0 {
            return Err(io::Error::last_os_error().into());
        }
        Ok(code)
    }

    pub fn main() {
        match run() {
            Ok(code) => std::process::exit(code as i32),
            Err(error) => {
                eprintln!("datapier launcher: {error}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(windows)]
fn main() {
    windows::main();
}

#[cfg(unix)]
mod unix {
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    use std::sync::atomic::{AtomicI32, Ordering};

    static CHILD: AtomicI32 = AtomicI32::new(0);
    extern "C" fn forward(signal: libc::c_int) {
        let child = CHILD.load(Ordering::Relaxed);
        if child > 0 {
            // SAFETY: kill 是异步信号安全操作，child 是已启动的进程 ID。
            unsafe { libc::kill(child, signal) };
        }
    }

    fn run() -> Result<i32, Box<dyn std::error::Error>> {
        let (active, _admission) = super::prepare()?;
        let signals = [libc::SIGTERM, libc::SIGINT, libc::SIGHUP, libc::SIGQUIT];
        // 发布子进程 PID 前阻塞终止信号，子进程在 exec 前恢复原屏蔽集，避免遗漏转发。
        // SAFETY: 使用前由 sigemptyset 初始化 sigset_t。
        let mut set: libc::sigset_t = unsafe { std::mem::zeroed() };
        let mut previous: libc::sigset_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::sigemptyset(&mut set);
            for signal in signals {
                libc::sigaddset(&mut set, signal);
            }
            if libc::sigprocmask(libc::SIG_BLOCK, &set, &mut previous) != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            for signal in signals {
                if libc::signal(signal, forward as *const () as libc::sighandler_t) == libc::SIG_ERR
                {
                    return Err(std::io::Error::last_os_error().into());
                }
            }
        }
        let mut command = std::process::Command::new(active.binary);
        command.args(std::env::args_os().skip(1));
        // SAFETY: pre_exec 只调用异步信号安全函数，使用复制的屏蔽集。
        unsafe {
            command.pre_exec(move || {
                for signal in signals {
                    if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                if libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn()?;
        CHILD.store(child.id() as i32, Ordering::Relaxed);
        // SAFETY: previous 已由 sigprocmask 初始化，适用于当前线程。
        if unsafe { libc::sigprocmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) } != 0 {
            let _ = child.kill();
            let _ = child.wait();
            return Err(std::io::Error::last_os_error().into());
        }
        let status = child.wait()?;
        CHILD.store(0, Ordering::Relaxed);
        Ok(status
            .code()
            .unwrap_or_else(|| 128 + status.signal().unwrap_or(0)))
    }

    pub fn main() {
        match run() {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("datapier launcher: {error}");
                std::process::exit(1);
            }
        }
    }
}

#[cfg(unix)]
fn main() {
    unix::main();
}
