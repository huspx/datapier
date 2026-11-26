//! 离线 Engine 场景的更新门禁及备份；活动实例不能通过抢锁伪装成交接。
use super::*;
use std::{fs::File, io::Read};

#[cfg(target_os = "macos")]
pub(super) async fn prepare_handoff(
    root: &Path,
    state_dir: &Path,
    id: uuid::Uuid,
) -> Result<(File, File)> {
    use std::os::{fd::FromRawFd, unix::fs::MetadataExt};
    // SAFETY: 此隐藏入口由 Engine 将唯一继承的锁描述符设置为 stdin。
    let lock = unsafe { File::from_raw_fd(0) };
    let inherited = lock.metadata()?;
    let original = fs::metadata(root.join("engine.lock"))?;
    if inherited.dev() != original.dev() || inherited.ino() != original.ino() {
        return Err(AppError::new(
            "update_handoff_failed",
            "继承描述符不是原实例锁",
        ));
    }
    lock.try_lock_exclusive()
        .map_err(|error| AppError::new("update_handoff_failed", error.to_string()))?;
    let status = tokio::process::Command::new(std::env::current_exe()?)
        .arg("--data-dir")
        .arg(root)
        .arg("update-lock-probe")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await?;
    if !status.success() {
        return Err(AppError::new(
            "update_handoff_failed",
            "第三进程未证实锁排他性",
        ));
    }
    let gate: Value = serde_json::from_slice(&fs::read(root.join("update-gate.json"))?)?;
    if gate["phase"] != "handoff" || gate["id"] != id.to_string() {
        return Err(AppError::new("update_handoff_failed", "交接身份不匹配"));
    }
    let owner = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("update-owner.lock"))?;
    owner
        .try_lock_exclusive()
        .map_err(|error| AppError::new("update_handoff_failed", error.to_string()))?;
    let ready = state_dir.join(format!("handoff-{id}.ready"));
    let done = state_dir.join(format!("handoff-{id}.done"));
    let receipt = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&ready)?;
    receipt.sync_all()?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    while !done.exists() {
        if tokio::time::Instant::now() >= deadline {
            return Err(AppError::new(
                "update_handoff_failed",
                "源 Engine 未完成资源释放",
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    fs::remove_file(ready)?;
    // 源进程释放自身描述符后再次独立争锁，证明存活副本仍在排斥候选。
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        tokio::process::Command::new(std::env::current_exe()?)
            .arg("--data-dir")
            .arg(root)
            .arg("update-lock-probe")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status(),
    )
    .await
    .map_err(|_| AppError::new("update_handoff_failed", "锁交接复核超时"))??;
    if !status.success() {
        return Err(AppError::new(
            "update_handoff_failed",
            "源进程退出后实例锁未保持",
        ));
    }
    fs::remove_file(done)?;
    Ok((lock, owner))
}

#[cfg(not(target_os = "macos"))]
pub(super) async fn prepare_handoff(
    _root: &Path,
    _state_dir: &Path,
    _id: uuid::Uuid,
) -> Result<(File, File)> {
    Err(AppError::new("update_unavailable", "当前平台未验证锁交接"))
}

fn digest(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(format!("{:x}", base16ct::HexDisplay(&hash.finalize())))
}

fn copy_tree(source: &Path, destination: &Path, data_root: bool) -> Result<()> {
    fs::create_dir(destination)?;
    fs::set_permissions(destination, fs::metadata(source)?.permissions())?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if data_root
            && matches!(
                name.to_str(),
                Some(
                    "engine.lock"
                        | "engine.json"
                        | "update-admission.lock"
                        | "update-owner.lock"
                        | "update-gate.json"
                )
            )
        {
            continue;
        }
        let metadata = entry.file_type()?;
        let target = destination.join(&name);
        if metadata.is_symlink() {
            return Err(AppError::invalid("备份目录包含符号链接，无法安全备份"));
        }
        if metadata.is_dir() {
            copy_tree(&entry.path(), &target, false)?;
        } else if metadata.is_file() {
            fs::copy(entry.path(), &target)?;
            File::open(&target)?.sync_all()?;
        } else {
            return Err(AppError::invalid("备份目录包含不支持的文件类型"));
        }
    }
    #[cfg(unix)]
    File::open(destination)?.sync_all()?;
    Ok(())
}

fn file_count(path: &Path) -> Result<usize> {
    let mut count = 0;
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            count += file_count(&entry.path())?;
        } else if kind.is_file() {
            count += 1;
        } else {
            return Err(AppError::invalid("暂存目录包含特殊文件"));
        }
    }
    Ok(count)
}

fn verify_stage(stage: &Path, state: &State) -> Result<PathBuf> {
    let archive = stage.join("plugin.zip");
    if Some(digest(&archive)?.as_str()) != state.plugin_digest.as_deref() {
        return Err(AppError::invalid("暂存 plugin 摘要已改变"));
    }
    let mut zip =
        zip::ZipArchive::new(File::open(archive)?).map_err(|e| AppError::invalid(e.to_string()))?;
    if file_count(&stage.join("datapier"))? != zip.len() {
        return Err(AppError::invalid("暂存 plugin 包含额外文件"));
    }
    for index in 0..zip.len() {
        let mut item = zip
            .by_index(index)
            .map_err(|e| AppError::invalid(e.to_string()))?;
        let path = item
            .enclosed_name()
            .ok_or_else(|| AppError::invalid("暂存归档路径无效"))?;
        let mut hash = Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let n = item.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        let expected = format!("{:x}", base16ct::HexDisplay(&hash.finalize()));
        let disk = stage.join(path);
        if fs::symlink_metadata(&disk)?.file_type().is_symlink() || digest(&disk)? != expected {
            return Err(AppError::invalid("暂存 plugin 文件已改变"));
        }
    }
    let ver = state
        .target_version
        .as_deref()
        .ok_or_else(|| AppError::invalid("缺少目标版本"))?;
    let manifest = fs::read_to_string(stage.join("datapier/plugin.toml"))?;
    let versions: Vec<_> = manifest
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "version").then(|| value.trim().trim_matches('"'))
        })
        .collect();
    if versions != [ver] {
        return Err(AppError::invalid("暂存 plugin 与目标版本不匹配"));
    }
    super::ensure_target_version(ver)?;
    let target = target().ok_or_else(|| AppError::invalid("不支持的平台"))?;
    let cache = stage.join("cache").join(ver);
    let mut candidates = Vec::new();
    for entry in fs::read_dir(cache)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(&format!("{target}-"))
        {
            candidates.push(entry.path().join(if cfg!(windows) {
                "datapier.exe"
            } else {
                "datapier"
            }));
        }
    }
    if candidates.len() != 1 {
        return Err(AppError::invalid("暂存程序不唯一"));
    }
    let hash = digest(&candidates[0])?;
    let sums = fs::read_to_string(stage.join("datapier/release-bin-sha256.txt"))?;
    if sums
        .lines()
        .filter(|line| *line == format!("{hash}  {target}"))
        .count()
        != 1
    {
        return Err(AppError::invalid("暂存程序摘要不匹配"));
    }
    Ok(candidates.remove(0))
}

fn same_tree(source: &Path, target: &Path) -> Result<bool> {
    if !target.is_dir() || file_count(source)? != file_count(target)? {
        return Ok(false);
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let path = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if !same_tree(&entry.path(), &path)? {
                return Ok(false);
            }
        } else if !kind.is_file()
            || !path.is_file()
            || fs::symlink_metadata(&path)?.file_type().is_symlink()
            || digest(&entry.path())? != digest(&path)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn restore_plugin(backup: &Path, plugin_dir: &Path) -> Result<()> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::process::Command::new("tern")
            .arg("plugin")
            .arg("install")
            .arg(backup.join("plugin"))
            .arg("--force")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| AppError::new("update_recovery_required", "恢复旧 plugin 超时"))??;
    if !output.status.success() || !same_tree(&backup.join("plugin"), plugin_dir)? {
        return Err(AppError::new(
            "update_recovery_required",
            "无法确认旧 plugin 已恢复",
        ));
    }
    Ok(())
}

struct RollbackDirectories<'a> {
    root: &'a Path,
    backup: &'a Path,
    plugin_dir: &'a Path,
    state_dir: &'a Path,
}

async fn rollback(
    directories: RollbackDirectories<'_>,
    state: &State,
    gate: &mut Value,
    id: uuid::Uuid,
    reason: &str,
) -> Result<AppError> {
    let RollbackDirectories {
        root,
        backup,
        plugin_dir,
        state_dir,
    } = directories;
    gate["phase"] = Value::String("rolling_back".into());
    gate["target_version"] = Value::String(env!("CARGO_PKG_VERSION").into());
    gate["binary_hash"] = Value::String(digest(&std::env::current_exe()?)?);
    let temporary = root.join(format!("update-gate-{id}.rollback"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, &*gate)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, root.join("update-gate.json"))?;
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    restore_plugin(backup, plugin_dir).await?;
    let receipt = root.join(format!("update-receipt-{id}-rolling_back"));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    while !receipt.exists() {
        if tokio::time::Instant::now() >= deadline {
            return Err(AppError::new(
                "update_recovery_required",
                "旧 plugin 文件已恢复，但未收到旧窗口加载确认",
            ));
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    let mut restored = state.clone();
    restored.state = "available".into();
    restored.apply_after_exit = false;
    restored.staged_dir = None;
    restored.message = format!("更新失败，已恢复旧版本：{reason}");
    save(state_dir, &mut restored)?;
    gate["phase"] = Value::String("committed".into());
    let temporary = root.join(format!("update-gate-{id}.rollback-commit"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    serde_json::to_writer(&mut file, &*gate)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, root.join("update-gate.json"))?;
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    fs::remove_file(receipt)?;
    fs::remove_file(root.join("update-gate.json"))?;
    #[cfg(unix)]
    File::open(root)?.sync_all()?;
    Ok(AppError::new("update_rolled_back", restored.message))
}

fn installed_matches(stage: &Path, plugin_dir: &Path) -> Result<bool> {
    let mut archive = zip::ZipArchive::new(File::open(stage.join("plugin.zip"))?)
        .map_err(|e| AppError::invalid(e.to_string()))?;
    if file_count(plugin_dir)? != archive.len() {
        return Ok(false);
    }
    for index in 0..archive.len() {
        let mut item = archive
            .by_index(index)
            .map_err(|e| AppError::invalid(e.to_string()))?;
        let name = item
            .enclosed_name()
            .ok_or_else(|| AppError::invalid("归档路径无效"))?;
        let relative = name
            .strip_prefix("datapier")
            .map_err(|_| AppError::invalid("归档路径无效"))?;
        let path = plugin_dir.join(relative);
        if !path.is_file() || fs::symlink_metadata(&path)?.file_type().is_symlink() {
            return Ok(false);
        }
        let mut hash = Sha256::new();
        let mut buffer = [0; 65536];
        loop {
            let n = item.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        if digest(&path)? != format!("{:x}", base16ct::HexDisplay(&hash.finalize())) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// 新 VM 使用已校验目标程序调用 status 时记录加载确认；阻断保持到安装进程提交。
pub(super) fn acknowledge(root: &Path, plugin_dir: &Path) -> Result<bool> {
    let path = root.join("update-gate.json");
    if !path.exists() {
        return Ok(false);
    }
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let gate: Value = serde_json::from_slice(&bytes)?;
    if gate["phase"].as_str() == Some("handoff") {
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("update-owner.lock"))?;
        let admission = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("update-admission.lock"))?;
        if admission.try_lock_exclusive().is_err() || owner.try_lock_exclusive().is_err() {
            return Ok(true);
        }
        return Err(AppError::new(
            "update_recovery_required",
            "交接所有者已失联，旧版锁保护不再连续",
        ));
    }
    if gate["phase"].as_str() == Some("prepared") {
        let admission = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("update-admission.lock"))?;
        if admission.try_lock_exclusive().is_err() {
            return Ok(true);
        }
        let engine = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("engine.lock"))?;
        if engine.try_lock_exclusive().is_err() {
            return Ok(true);
        }
        let current: Value = serde_json::from_slice(&fs::read(&path)?)?;
        if current["phase"] == "prepared" && current["id"] == gate["id"] {
            // 未启动安装时没有改写任何业务数据或 plugin；重取两把锁后可撤销本阶段。
            fs::remove_file(&path)?;
            return Ok(false);
        }
        return Ok(true);
    }
    if gate["phase"].as_str() == Some("committed") {
        let admission = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("update-admission.lock"))?;
        if admission.try_lock_exclusive().is_err() {
            return Ok(true);
        }
        return Err(AppError::new(
            "update_recovery_required",
            "提交后更新进程失联，须人工核查",
        ));
    }
    let phase = gate["phase"].as_str().unwrap_or("");
    if phase != "installing" && phase != "rolling_back" {
        return Err(AppError::new(
            "update_recovery_required",
            "更新提交阶段未完成，须人工核查",
        ));
    }
    let owner_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("update-admission.lock"))?;
    if owner_lock.try_lock_exclusive().is_ok() {
        return Err(AppError::new(
            "update_recovery_required",
            "更新进程已退出，旧版锁保护不再连续；须人工核查",
        ));
    }
    let expected = gate["plugin_dir"]
        .as_str()
        .ok_or_else(|| AppError::invalid("更新门禁无效"))?;
    let target_version = gate["target_version"]
        .as_str()
        .ok_or_else(|| AppError::invalid("更新门禁无效"))?;
    let binary_hash = gate["binary_hash"]
        .as_str()
        .ok_or_else(|| AppError::invalid("更新门禁无效"))?;
    let id = gate["id"]
        .as_str()
        .ok_or_else(|| AppError::invalid("更新门禁无效"))?;
    let stage = gate["stage"]
        .as_str()
        .ok_or_else(|| AppError::invalid("更新门禁无效"))?;
    if uuid::Uuid::parse_str(id).is_err() {
        return Err(AppError::invalid("更新身份无效"));
    }
    if env!("CARGO_PKG_VERSION") != target_version
        || fs::canonicalize(plugin_dir)? != Path::new(expected)
        || digest(&std::env::current_exe()?)? != binary_hash
    {
        return Ok(true);
    }
    let manifest = fs::read_to_string(plugin_dir.join("plugin.toml"))?;
    if !manifest
        .lines()
        .any(|line| line.trim() == format!("version = \"{target_version}\""))
    {
        return Ok(true);
    }
    if phase == "installing" {
        if !installed_matches(Path::new(stage), plugin_dir)? {
            return Ok(true);
        }
    } else {
        let backup = gate["backup"]
            .as_str()
            .ok_or_else(|| AppError::invalid("更新门禁无效"))?;
        if !same_tree(&Path::new(backup).join("plugin"), plugin_dir)? {
            return Ok(true);
        }
    }
    let receipt = root.join(format!("update-receipt-{id}-{phase}"));
    match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(receipt)
    {
        Ok(file) => file.sync_all()?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    Ok(true)
}

pub(super) async fn run(
    state: &State,
    root: &Path,
    plugin_dir: &Path,
    state_dir: &Path,
    inherited: Option<(File, File)>,
    handoff_id: Option<uuid::Uuid>,
) -> Result<()> {
    let target_version = state
        .target_version
        .as_deref()
        .ok_or_else(|| AppError::invalid("缺少目标版本"))?;
    super::ensure_target_version(target_version)?;
    let root = datapier_engine::update_data_root(root)?;
    let plugin_dir = fs::canonicalize(plugin_dir)?;
    let state_dir = fs::canonicalize(state_dir)?;
    // 锁位于安装目录的父目录，安装器替换 plugin 时不会替换锁文件。
    let install_parent = plugin_dir
        .parent()
        .ok_or_else(|| AppError::invalid("plugin 安装目录无效"))?;
    let install_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(install_parent.join(".datapier-update.lock"))?;
    install_lock
        .try_lock_exclusive()
        .map_err(|error| AppError::new("update_busy", error.to_string()))?;
    if state_dir.starts_with(&root)
        || root.starts_with(&state_dir)
        || plugin_dir.starts_with(&root)
        || root.starts_with(&plugin_dir)
        || state_dir.starts_with(&plugin_dir)
        || plugin_dir.starts_with(&state_dir)
    {
        return Err(AppError::invalid("更新状态、plugin 和数据目录必须相互独立"));
    }
    let stage = state
        .staged_dir
        .as_ref()
        .ok_or_else(|| AppError::invalid("更新尚未准备"))?;
    let stage = fs::canonicalize(stage)?;
    if !stage.starts_with(&state_dir) {
        return Err(AppError::invalid("暂存目录无效"));
    }
    let binary = verify_stage(&stage, state)?;
    let binary_hash = digest(&binary)?;
    let admission = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join("update-admission.lock"))?;
    admission
        .try_lock_exclusive()
        .map_err(|e| AppError::new("update_busy", e.to_string()))?;
    let (inherited, _owner) = match inherited {
        Some((lock, owner)) => (Some(lock), Some(owner)),
        None => (None, None),
    };
    let engine_lock = match (inherited, handoff_id) {
        (Some(lock), Some(id)) => {
            let gate: Value = serde_json::from_slice(&fs::read(root.join("update-gate.json"))?)?;
            if gate["phase"] != "handoff" || gate["id"] != id.to_string() {
                return Err(AppError::new(
                    "update_recovery_required",
                    "交接门禁身份不匹配",
                ));
            }
            lock
        }
        (None, None) => {
            if root.join("update-gate.json").exists() {
                return Err(AppError::new(
                    "update_recovery_required",
                    "已有未完成更新，须人工核查",
                ));
            }
            let lock = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(root.join("engine.lock"))?;
            lock.try_lock_exclusive()
                .map_err(|_| AppError::new("update_engine_busy", "Engine 仍在运行"))?;
            lock
        }
        _ => return Err(AppError::invalid("锁交接参数不完整")),
    };
    let id = handoff_id.unwrap_or_else(uuid::Uuid::new_v4);
    let backup = state_dir.join(format!("backup-{id}"));
    fs::create_dir(&backup)?;
    copy_tree(&root, &backup.join("data"), true)?;
    copy_tree(&plugin_dir, &backup.join("plugin"), false)?;
    #[cfg(unix)]
    {
        File::open(&backup)?.sync_all()?;
        File::open(&state_dir)?.sync_all()?;
    }
    let target_version = state
        .target_version
        .as_deref()
        .ok_or_else(|| AppError::invalid("缺少目标版本"))?;
    let mut gate = serde_json::json!({"id":id.to_string(),"phase":"prepared","target_version":target_version,
        "plugin_dir":plugin_dir,"backup":backup,"binary_hash":binary_hash,"stage":stage});
    let gate_path = root.join("update-gate.json");
    let prepared_path = root.join(format!("update-gate-{id}.prepared"));
    let mut marker = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&prepared_path)?;
    serde_json::to_writer(&mut marker, &gate)?;
    marker.sync_all()?;
    drop(marker);
    fs::rename(prepared_path, &gate_path)?;
    #[cfg(unix)]
    File::open(&root)?.sync_all()?;
    gate["phase"] = Value::String("installing".into());
    let temporary = root.join(format!("update-gate-{id}.tmp"));
    let mut replacement = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    serde_json::to_writer(&mut replacement, &gate)?;
    replacement.sync_all()?;
    drop(replacement);
    fs::rename(temporary, gate_path)?;
    #[cfg(unix)]
    File::open(&root)?.sync_all()?;
    let install = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::process::Command::new("tern")
            .arg("plugin")
            .arg("install")
            .arg(stage.join("datapier"))
            .arg("--force")
            .kill_on_drop(true)
            .output(),
    )
    .await;
    let problem = match install {
        Ok(Ok(output)) if output.status.success() => None,
        Ok(Ok(output)) => Some(String::from_utf8_lossy(&output.stderr).into_owned()),
        Ok(Err(error)) => {
            return Err(AppError::new(
                "update_recovery_required",
                format!("安装进程异常：{error}；安装状态无法确认，保留门禁"),
            ));
        }
        Err(_) => {
            return Err(AppError::new(
                "update_recovery_required",
                "安装超时，后台写入状态无法确认；保留门禁",
            ));
        }
    };
    if let Some(problem) = problem {
        return Err(rollback(
            RollbackDirectories {
                root: &root,
                backup: &backup,
                plugin_dir: &plugin_dir,
                state_dir: &state_dir,
            },
            state,
            &mut gate,
            id,
            &problem,
        )
        .await?);
    }
    let receipt = root.join(format!("update-receipt-{id}-installing"));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    while !receipt.exists() {
        if tokio::time::Instant::now() >= deadline {
            return Err(rollback(
                RollbackDirectories {
                    root: &root,
                    backup: &backup,
                    plugin_dir: &plugin_dir,
                    state_dir: &state_dir,
                },
                state,
                &mut gate,
                id,
                "未收到新窗口加载确认",
            )
            .await?);
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
    // 提交状态先落盘，之后才能解除新协议启动门禁和实例锁。
    let mut committed = state.clone();
    committed.state = "installed".into();
    committed.apply_after_exit = false;
    committed.message = "更新已安装".into();
    save(&state_dir, &mut committed)?;
    gate["phase"] = Value::String("committed".into());
    let commit_path = root.join(format!("update-gate-{id}.commit"));
    let mut commit = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&commit_path)?;
    serde_json::to_writer(&mut commit, &gate)?;
    commit.sync_all()?;
    drop(commit);
    fs::rename(commit_path, root.join("update-gate.json"))?;
    #[cfg(unix)]
    File::open(&root)?.sync_all()?;
    fs::remove_file(receipt)?;
    fs::remove_file(root.join("update-gate.json"))?;
    #[cfg(unix)]
    File::open(&root)?.sync_all()?;
    drop(engine_lock);
    drop(admission);
    Ok(())
}
