//! Tern 受管入口的持久设置读回及更新暂存回收。
use super::*;
use std::io;

fn read_optional(path: &Path) -> Result<Option<Value>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn settings(plugin_dir: &Path) -> Result<Option<Value>> {
    let config = plugin_dir
        .parent()
        .filter(|path| path.ends_with("plugins"))
        .and_then(Path::parent)
        .ok_or_else(|| AppError::invalid("无法确认 Tern 配置目录"))?;
    read_optional(&config.join("settings.json"))
}

fn matches_launch(entry: &Value, startup: &Value) -> bool {
    startup.is_object()
        && entry["command"] == startup["command"]
        && entry["transport"] == startup["transport"]
        && entry["args"] == startup["args"]
        && entry["env"]["DATAPIER_DATA_DIR"] == startup["data_dir"]
}

/// 返回确实由 Datapier 管理的持久设置。用户同名条目由用户维护。
pub(super) fn readback(
    state_dir: &Path,
    plugin_dir: &Path,
    root: &Path,
    entry: Option<&fixed_entry::Entry>,
) -> Result<Option<Value>> {
    let Some(kv) = read_optional(&state_dir.join("kv.json"))? else {
        return Ok(None);
    };
    let Some(managed) = kv.get("mcp_managed") else {
        return Ok(None);
    };
    if managed["host"] != "local"
        || !managed["data_dir"]
            .as_str()
            .and_then(|path| fs::canonicalize(path).ok())
            .is_some_and(|path| path == root)
    {
        return Ok(None);
    }
    let unconfirmed = || AppError::new("update_mcp_readback", "无法读回本机 Tern 受管 MCP 配置");
    let settings = settings(plugin_dir)?.ok_or_else(unconfirmed)?;
    let entries = settings["agents"]["mcp"]
        .as_array()
        .ok_or_else(unconfirmed)?;
    let mut entries = entries.iter().filter(|entry| entry["name"] == "datapier");
    let Some(current) = entries.next() else {
        return Ok(None);
    };
    if entries.next().is_some() {
        return Err(unconfirmed());
    }
    if !["startup", "pending"]
        .iter()
        .any(|key| matches_launch(current, &managed[*key]))
    {
        return Ok(None);
    }
    let command = current["command"].as_str().map(Path::new);
    if entry.is_none_or(|entry| Some(entry.command()) != command) {
        return Err(AppError::new(
            "update_mcp_entry",
            "请在本机 Tern 窗口运行当前 Datapier 插件，等待受管 MCP 登记到固定入口后再切换",
        ));
    }
    Ok(Some(settings))
}

const STAGE_PREFIX: &str = "managed-stage-";

pub(super) fn stage_path(state_dir: &Path) -> PathBuf {
    state_dir.join(format!("{STAGE_PREFIX}{}", uuid::Uuid::new_v4()))
}

pub(super) fn create_stage(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    #[cfg(not(unix))]
    fs::create_dir(path)?;
    Ok(())
}

fn stage_id(state_dir: &Path, path: &Path) -> Option<uuid::Uuid> {
    if path.parent() != Some(state_dir) {
        return None;
    }
    let name = path.file_name()?.to_str()?;
    uuid::Uuid::parse_str(name.strip_prefix(STAGE_PREFIX)?).ok()
}

fn references_stage(value: &Value, basename: &str) -> bool {
    match value {
        Value::String(value) => value.contains(basename),
        Value::Array(values) => values.iter().any(|value| references_stage(value, basename)),
        Value::Object(values) => values
            .values()
            .any(|value| references_stage(value, basename)),
        _ => false,
    }
}

pub(super) fn track(state_dir: &Path, stage: Option<&Path>) -> Result<Vec<uuid::Uuid>> {
    let record = state_dir.join("managed-stages.json");
    let mut stages: Vec<uuid::Uuid> = read_optional(&record)?
        .map(serde_json::from_value)
        .transpose()?
        .unwrap_or_default();
    if let Some(id) = stage.and_then(|path| stage_id(state_dir, path))
        && stages.first() != Some(&id)
    {
        stages.retain(|previous| *previous != id);
        stages.insert(0, id);
        fixed_entry::write_record(&record, &stages)?;
    }
    Ok(stages)
}

/// 只回收本实现成功提交过的专用暂存缓存。共享安装缓存的外部引用无法枚举。
pub(super) async fn clean(
    state_dir: &Path,
    stages: &[uuid::Uuid],
    settings: &Value,
    protected: &[&Path],
) -> Result<()> {
    if stages.len() <= 2 {
        return Ok(());
    }
    let running = std::env::current_exe()?.canonicalize()?;
    let mut kept = stages[..2].to_vec();
    for &id in &stages[2..] {
        let name = format!("{STAGE_PREFIX}{id}");
        let path = state_dir.join(&name).join("cache");
        if running.starts_with(&path)
            || protected.iter().any(|binary| binary.starts_with(&path))
            || references_stage(settings, &name)
        {
            kept.push(id);
            continue;
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                kept.push(id);
                continue;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        }
        if fs::canonicalize(&path)? != path {
            kept.push(id);
            continue;
        }
        // 更新已排空同一数据目录的客户端；另一个数据目录可能仍从此处运行。
        if !cfg!(target_os = "macos") {
            kept.push(id);
            continue;
        }
        let processes = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::process::Command::new("/usr/sbin/lsof")
                .args(["-t", "+D"])
                .arg(&path)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| AppError::new("update_cache", "程序缓存进程检查超时"))??;
        // macOS 的 lsof 即使返回 1，也可能已输出引用此目录的 PID。
        if !processes.stdout.is_empty() {
            kept.push(id);
            continue;
        }
        if processes.status.code() != Some(1) || !processes.stderr.is_empty() {
            return Err(AppError::new(
                "update_cache",
                "无法确认程序缓存是否仍在使用",
            ));
        }
        if self::settings(state_dir)?.as_ref() != Some(settings) {
            return Err(AppError::new("update_cache", "回收期间 Tern 配置发生变化"));
        }
        fs::remove_dir_all(&path)?;
    }
    if kept != stages {
        fixed_entry::write_record(&state_dir.join("managed-stages.json"), &kept)?;
    }
    Ok(())
}
