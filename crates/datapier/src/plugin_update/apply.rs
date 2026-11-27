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

pub(super) fn digest(path: &Path) -> Result<String> {
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

fn runtime_file(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            "engine.lock"
                | "engine.json"
                | "update-admission.lock"
                | "update-owner.lock"
                | "update-gate.json"
        )
    ) || name
        .to_str()
        .is_some_and(|name| name.starts_with("update-gate-") || name.starts_with("update-receipt-"))
}

fn restore_data(source: &Path, root: &Path) -> Result<()> {
    // 回滚复制内容时保留准入锁和 Engine 实例锁的文件身份。
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !runtime_file(&entry.file_name()) {
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        if runtime_file(&entry.file_name()) {
            continue;
        }
        let destination = root.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &destination, false)?;
        } else {
            fs::copy(entry.path(), &destination)?;
            File::open(destination)?.sync_all()?;
        }
    }
    fixed_entry::sync_parent(&root.join("update-gate.json"))
}

fn copy_tree(source: &Path, destination: &Path, data_root: bool) -> Result<()> {
    fs::create_dir(destination)?;
    fs::set_permissions(destination, fs::metadata(source)?.permissions())?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let name = entry.file_name();
        if data_root && runtime_file(&name) {
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

fn verify_stage(stage: &Path, state: &State, manual: bool) -> Result<PathBuf> {
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
    if !manual {
        super::ensure_target_version(ver)?;
    }
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
    let launcher_manifest = stage.join("datapier/release-launcher-sha256.txt");
    if launcher_manifest.try_exists()? || version(ver) >= Some([0, 5, 2]) {
        let name = if cfg!(windows) {
            "datapier-launcher.exe"
        } else {
            "datapier-launcher"
        };
        fixed_entry::verify(
            &candidates[0].with_file_name(name),
            &launcher_manifest,
            &target,
        )?;
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

#[derive(Serialize, Deserialize)]
struct EntryTransition {
    old: fixed_entry::Active,
    new: fixed_entry::Active,
}

struct Entries {
    fixed: Option<fixed_entry::Entry>,
    managed: Option<fixed_entry::Entry>,
}

impl Entries {
    fn lock(state_dir: &Path) -> Result<Self> {
        Ok(Self {
            fixed: fixed_entry::Entry::lock(false)?,
            managed: fixed_entry::Entry::managed(state_dir, false)?,
        })
    }

    fn switch(&self, gate: &Value, commit: bool) -> Result<()> {
        for (key, entry) in [
            ("fixed_entry", self.fixed.as_ref()),
            ("managed_entry", self.managed.as_ref()),
        ] {
            if gate[key].is_null() {
                continue;
            }
            let transition: EntryTransition = serde_json::from_value(gate[key].clone())?;
            let entry = entry.ok_or_else(|| AppError::invalid("MCP 入口登记目录已丢失"))?;
            let (from, to) = if commit {
                (&transition.old, &transition.new)
            } else {
                (&transition.new, &transition.old)
            };
            entry.switch(from, to)?;
        }
        Ok(())
    }

    fn confirm_managed(&self, state_dir: &Path, plugin: &Path, root: &Path) -> Result<Value> {
        let settings = managed_entry::readback(state_dir, root, self.managed.as_ref())?
            .ok_or_else(|| {
                AppError::new("update_mcp_readback", "没有可确认的 Tern 受管 MCP 配置")
            })?;
        let entry = self
            .managed
            .as_ref()
            .ok_or_else(|| AppError::invalid("Tern 受管入口已丢失"))?;
        let target = target().ok_or_else(|| AppError::invalid("不支持的平台"))?;
        entry.verify_installed(plugin, root, &target)?;
        Ok(settings)
    }

    async fn clean(
        &self,
        state_dir: &Path,
        plugin_dir: &Path,
        root: &Path,
        stage: Option<&Path>,
    ) -> Result<()> {
        let stages = managed_entry::track(state_dir, stage)?;
        let settings = self.confirm_managed(state_dir, plugin_dir, root)?;
        let mut active = Vec::new();
        for entry in [&self.fixed, &self.managed].into_iter().flatten() {
            active.extend(entry.transition_from(plugin_dir, root)?);
        }
        let protected: Vec<_> = active.iter().map(|entry| entry.binary.as_path()).collect();
        managed_entry::clean(state_dir, &stages, &settings, &protected).await
    }
}

fn plan_entry(
    entry: Option<&fixed_entry::Entry>,
    plugin: &Path,
    root: &Path,
    binary: &Path,
    binary_hash: &str,
) -> Result<Option<EntryTransition>> {
    let Some(entry) = entry else {
        return Ok(None);
    };
    entry
        .transition_from(plugin, root)?
        .map(|old| {
            let new = entry.next(&old, binary, binary_hash.to_owned())?;
            Ok(EntryTransition { old, new })
        })
        .transpose()
}

pub(super) async fn clean_committed(
    state: &State,
    plugin_dir: &Path,
    state_dir: &Path,
    root: &Path,
) -> Result<()> {
    let plugin_dir = fs::canonicalize(plugin_dir)?;
    let root = datapier_engine::update_data_root(root)?;
    let _install = lock_install(&plugin_dir)?;
    let _admission = lock_admission(&root)?;
    if root.join("update-gate.json").try_exists()? {
        return Err(AppError::new(
            "update_recovery_required",
            "请先恢复未完成的更新",
        ));
    }
    let _engine = try_exclusive(&root.join("engine.lock"), "update_engine_busy")?;
    let entries = Entries::lock(state_dir)?;
    let installed_stage = if state.state == "installed" {
        state.staged_dir.as_deref()
    } else {
        None
    };
    entries
        .clean(state_dir, &plugin_dir, &root, installed_stage)
        .await
}

struct RollbackDirectories<'a> {
    root: &'a Path,
    backup: &'a Path,
    plugin_dir: &'a Path,
    state_dir: &'a Path,
    entries: &'a Entries,
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
        entries,
    } = directories;
    gate["phase"] = Value::String("rolling_back".into());
    gate["target_version"] = Value::String(env!("CARGO_PKG_VERSION").into());
    gate["binary_hash"] = Value::String(digest(&std::env::current_exe()?)?);
    fixed_entry::write_record(&root.join("update-gate.json"), gate)?;
    restore_plugin(backup, plugin_dir).await?;
    if gate["restore_data"].as_bool() == Some(true) {
        restore_data(&backup.join("data"), root)?;
    }
    entries.switch(gate, false)?;
    let receipt = root.join(format!("update-receipt-{id}-rolling_back"));
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    while gate["manual"].as_bool() != Some(true) && !receipt.exists() {
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
    if !gate["managed_entry"].is_null()
        && let Err(error) = entries.confirm_managed(state_dir, plugin_dir, root)
    {
        restored
            .message
            .push_str(&format!("；无法确认 Tern MCP 配置：{error}"));
    }
    save(state_dir, &mut restored)?;
    gate["fixed_entry_rolled_back"] = Value::Bool(true);
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
    remove_receipt(&receipt)?;
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

fn remove_receipt(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn try_exclusive(path: &Path, code: &'static str) -> Result<File> {
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock.try_lock_exclusive()
        .map_err(|error| AppError::new(code, error.to_string()))?;
    Ok(lock)
}

pub(super) fn lock_admission(root: &Path) -> Result<File> {
    try_exclusive(&root.join("update-admission.lock"), "update_busy")
}

fn lock_install(plugin_dir: &Path) -> Result<File> {
    let parent = plugin_dir
        .parent()
        .ok_or_else(|| AppError::invalid("plugin 安装目录无效"))?;
    try_exclusive(&parent.join(".datapier-update.lock"), "update_busy")
}

pub(super) struct Manual<'a> {
    pub(super) restore_data: Option<&'a Path>,
}

pub(super) async fn run(
    state: &State,
    root: &Path,
    plugin_dir: &Path,
    state_dir: &Path,
    inherited: Option<(File, File)>,
    handoff_id: Option<uuid::Uuid>,
    manual: Option<Manual<'_>>,
) -> Result<()> {
    let target_version = state
        .target_version
        .as_deref()
        .ok_or_else(|| AppError::invalid("缺少目标版本"))?;
    if manual.is_none() {
        super::ensure_target_version(target_version)?;
    }
    let root = datapier_engine::update_data_root(root)?;
    let plugin_dir = fs::canonicalize(plugin_dir)?;
    let state_dir = fs::canonicalize(state_dir)?;
    // 锁位于安装目录的父目录，安装器替换 plugin 时不会替换锁文件。
    let _install_lock = lock_install(&plugin_dir)?;
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
    let binary = verify_stage(&stage, state, manual.is_some())?;
    let binary_hash = digest(&binary)?;
    let admission = lock_admission(&root)?;
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
            try_exclusive(&root.join("engine.lock"), "update_engine_busy")?
        }
        _ => return Err(AppError::invalid("锁交接参数不完整")),
    };
    let id = handoff_id.unwrap_or_else(uuid::Uuid::new_v4);
    let entries = Entries::lock(&state_dir)?;
    let fixed = plan_entry(
        entries.fixed.as_ref(),
        &plugin_dir,
        &root,
        &binary,
        &binary_hash,
    )?;
    let managed = plan_entry(
        entries.managed.as_ref(),
        &plugin_dir,
        &root,
        &binary,
        &binary_hash,
    )?;
    if (fixed.is_some() && version(target_version) < Some([0, 2, 0]))
        || (managed.is_some() && version(target_version) < Some([0, 5, 1]))
    {
        return Err(AppError::invalid("目标版本不支持已登记的 MCP 启动参数"));
    }
    match managed_entry::readback(&state_dir, &root, entries.managed.as_ref()) {
        Ok(Some(_)) if managed.is_none() => {
            return Err(AppError::invalid("Tern 受管 MCP 的入口登记已丢失"));
        }
        // 旧程序尚未登记固定入口时，支持迁移的新插件将在提交后重新登记。
        Err(error)
            if error.code == "update_mcp_entry" && version(target_version) >= Some([0, 5, 2]) => {}
        Err(error) => return Err(error),
        _ => {}
    }
    let restore_from = manual.as_ref().and_then(|manual| manual.restore_data);
    if manual.is_some() {
        super::manual::check_schema(restore_from.unwrap_or(&root), target_version).await?;
    }
    let restored_data = stage.join(format!("restore-data-{id}"));
    if let Some(source) = restore_from {
        // 冻结显式选中的备份并验证快照，原工作目录的完整快照另外保留供失败恢复。
        copy_tree(source, &restored_data, true)?;
        super::manual::check_schema(&restored_data, target_version).await?;
    }
    let backup = state_dir.join(format!("backup-{id}"));
    fs::create_dir(&backup)?;
    copy_tree(&root, &backup.join("data"), true)?;
    copy_tree(&plugin_dir, &backup.join("plugin"), false)?;
    #[cfg(unix)]
    {
        File::open(&backup)?.sync_all()?;
        File::open(&state_dir)?.sync_all()?;
    }
    let mut gate = serde_json::json!({"id":id.to_string(),"phase":"prepared","target_version":target_version,
        "plugin_dir":plugin_dir,"backup":backup,"binary_hash":binary_hash,"stage":stage,
        "fixed_entry":fixed,"managed_entry":managed,"manual":manual.is_some(),"restore_data":restore_from.is_some()});
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
    if restore_from.is_some()
        && let Err(error) = restore_data(&restored_data, &root)
    {
        return Err(rollback(
            RollbackDirectories {
                root: &root,
                backup: &backup,
                plugin_dir: &plugin_dir,
                state_dir: &state_dir,
                entries: &entries,
            },
            state,
            &mut gate,
            id,
            &error.to_string(),
        )
        .await?);
    }
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
                entries: &entries,
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
    // 旧于 0.5.0 的 plugin 没有窗口回执协议，只能核对安装树和已校验的程序。
    while version(target_version) >= Some([0, 5, 0]) && !receipt.exists() {
        if tokio::time::Instant::now() >= deadline {
            return Err(rollback(
                RollbackDirectories {
                    root: &root,
                    backup: &backup,
                    plugin_dir: &plugin_dir,
                    state_dir: &state_dir,
                    entries: &entries,
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
    let switched = (|| {
        if !installed_matches(&stage, &plugin_dir)? {
            return Err(AppError::invalid("实际安装与候选 plugin 不一致"));
        }
        entries.switch(&gate, true)
    })();
    if let Err(error) = switched {
        return Err(rollback(
            RollbackDirectories {
                root: &root,
                backup: &backup,
                plugin_dir: &plugin_dir,
                state_dir: &state_dir,
                entries: &entries,
            },
            state,
            &mut gate,
            id,
            &error.to_string(),
        )
        .await?);
    }
    // 提交状态先落盘，之后才能解除新协议启动门禁和实例锁。
    let mut committed = state.clone();
    committed.state = "installed".into();
    committed.apply_after_exit = false;
    committed.message = "更新已安装".into();
    committed.cache_cleanup_error = None;
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
    remove_receipt(&receipt)?;
    fs::remove_file(root.join("update-gate.json"))?;
    #[cfg(unix)]
    File::open(&root)?.sync_all()?;
    if let Err(error) = entries
        .clean(&state_dir, &plugin_dir, &root, Some(&stage))
        .await
    {
        committed.cache_cleanup_error = Some(error.to_string());
        committed.message = format!("更新已安装，旧程序缓存回收已暂停：{error}");
        eprintln!("更新已安装，旧程序缓存未回收：{error}");
        if let Err(error) = save(&state_dir, &mut committed) {
            eprintln!("无法保存缓存回收状态：{error}");
        }
    }
    drop(engine_lock);
    drop(admission);
    Ok(())
}

pub(super) async fn recover(root: &Path, plugin: &Path, state_dir: &Path) -> Result<()> {
    let root = datapier_engine::update_data_root(root)?;
    let plugin = fs::canonicalize(plugin)?;
    let _install = lock_install(&plugin)?;
    let _admission = lock_admission(&root)?;
    let _engine = try_exclusive(&root.join("engine.lock"), "update_engine_busy")?;
    let gate_path = root.join("update-gate.json");
    let mut gate: Value = match fs::read(&gate_path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let id = gate["id"]
        .as_str()
        .and_then(|s| uuid::Uuid::parse_str(s).ok())
        .ok_or_else(|| AppError::invalid("更新门禁身份无效"))?;
    let phase = gate["phase"].as_str().unwrap_or_default();
    if phase == "handoff" {
        return Err(AppError::new(
            "update_recovery_required",
            "交接中断，请先核对原实例和更新所有者",
        ));
    }
    if serde_json::from_value::<PathBuf>(gate["plugin_dir"].clone())? != plugin {
        return Err(AppError::invalid("plugin 安装不属于本次更新"));
    }
    let backup = state_dir.join(format!("backup-{id}"));
    if serde_json::from_value::<PathBuf>(gate["backup"].clone())? != backup {
        return Err(AppError::invalid("更新备份位置不匹配"));
    }
    let entries = Entries::lock(state_dir)?;
    match phase {
        "prepared" => {}
        "committed" => {
            let restored = gate["fixed_entry_rolled_back"].as_bool() == Some(true)
                || (gate["fixed_entry"].is_null()
                    && gate["managed_entry"].is_null()
                    && same_tree(&backup.join("plugin"), &plugin)?);
            if !restored {
                let stage: PathBuf = serde_json::from_value(gate["stage"].clone())?;
                if !stage.starts_with(state_dir) || !installed_matches(&stage, &plugin)? {
                    return Err(AppError::invalid("已提交的 plugin 与暂存包不一致"));
                }
            } else if !same_tree(&backup.join("plugin"), &plugin)? {
                return Err(AppError::invalid("已回滚的 plugin 与备份不一致"));
            }
            entries.switch(&gate, !restored)?;
        }
        "installing" | "rolling_back" => {
            // 已重新持有所有锁。原版本在备份时已可用，恢复后重新走正常启动校验。
            restore_plugin(&backup, &plugin).await?;
            if gate["restore_data"].as_bool() == Some(true) {
                restore_data(&backup.join("data"), &root)?;
            }
            entries.switch(&gate, false)?;
            gate["fixed_entry_rolled_back"] = Value::Bool(true);
            gate["phase"] = Value::String("committed".into());
            fixed_entry::write_record(&gate_path, &gate)?;
        }
        _ => return Err(AppError::invalid("无法识别待恢复的更新阶段")),
    }
    for phase in ["rolling_back", "installing"] {
        remove_receipt(&root.join(format!("update-receipt-{id}-{phase}")))?;
    }
    if !gate["managed_entry"].is_null()
        && let Err(error) = entries.confirm_managed(state_dir, &plugin, &root)
    {
        eprintln!("更新已恢复，无法确认 Tern MCP 配置：{error}");
    }
    fs::remove_file(&gate_path)?;
    fixed_entry::sync_parent(&gate_path)
}
