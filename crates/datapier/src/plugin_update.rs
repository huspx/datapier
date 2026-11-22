//! 有界的 Release 检查；更新状态由 OS 锁串行化，供多个窗口共享。
mod apply;
use datapier_core::{AppError, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const INTERVAL: u64 = 24 * 60 * 60;
const API: &str = "https://api.github.com/repos/huspx/datapier/releases/latest";

/// 第三进程独立打开同一锁文件，用于证明继承的锁仍排斥新候选。
pub fn probe(root: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(root.join("engine.lock"))?;
    match file.try_lock_exclusive() {
        Err(error) if error.raw_os_error() == fs2::lock_contended_error().raw_os_error() => Ok(()),
        Ok(()) => Err(AppError::new(
            "update_handoff_failed",
            "实例锁未排斥独立进程",
        )),
        Err(error) => Err(error.into()),
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    last_attempt: u64,
    last_success: u64,
    rate_limit_until: u64,
    etag: Option<String>,
    target_version: Option<String>,
    plugin_digest: Option<String>,
    binary_digest: Option<String>,
    staged_dir: Option<PathBuf>,
    worker_started_at: u64,
    apply_after_exit: bool,
    state: String,
    message: String,
    progress: Option<DownloadProgress>,
    progress_revision: u64,
    download_source: Option<String>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
enum DownloadPhase {
    Plugin,
    Program,
}

#[derive(Clone, Serialize, Deserialize)]
struct DownloadProgress {
    phase: DownloadPhase,
    received_bytes: u64,
    total_bytes: Option<u64>,
}

fn now() -> Result<u64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| AppError::new("clock", e.to_string()))?
        .as_secs())
}

fn version(value: &str) -> Option<[u64; 3]> {
    let mut parts = value.split('.');
    let mut result = [0; 3];
    for part in &mut result {
        let text = parts.next()?;
        if text.is_empty()
            || (text.len() > 1 && text.starts_with('0'))
            || !text.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        *part = text.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(result)
}

fn target() -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "aarch64" => "aarch64",
        "x86_64" => "x86_64",
        _ => return None,
    };
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        "linux" => "unknown-linux-gnu",
        "windows" => "pc-windows-msvc",
        _ => return None,
    };
    Some(format!("{arch}-{os}"))
}

fn save(dir: &Path, state: &mut State) -> Result<()> {
    // 进度和后续安装状态共用发布序号，避免迟到状态覆盖更新后的界面。
    state.progress_revision = state.progress_revision.saturating_add(1);
    let temporary = dir.join(format!(".update-state-{}", uuid::Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        serde_json::to_writer(&mut file, state)?;
        file.sync_all()?;
        fs::rename(&temporary, dir.join("update-state.json"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn output(state: &State, root: &Path, ok: bool) -> Value {
    let next = state
        .last_attempt
        .saturating_add(INTERVAL)
        .max(state.rate_limit_until);
    serde_json::json!({"ok":ok,"state":if state.state.is_empty() {"unchecked"} else {&state.state},
        "current_version":env!("CARGO_PKG_VERSION"),"target_version":state.target_version,
        "message":state.message,"next_check_at":if next == INTERVAL {None} else {Some(next)},
        "gate_blocked":root.join("update-gate.json").exists(),"data_dir":root,
        "automatic_install_supported":cfg!(target_os = "macos"),
        "apply_after_exit":state.apply_after_exit,"prepared":matches!(state.state.as_str(), "ready" | "waiting") && state.staged_dir.is_some(),
        "progress":state.progress,"progress_revision":state.progress_revision,"download_source":state.download_source})
}

/// 返回单个 JSON；失败也返回结构化状态，不向 stdout 输出诊断文本。
pub async fn run(
    action: &str,
    plugin_dir: PathBuf,
    state_dir: PathBuf,
    root: PathBuf,
    force: bool,
    worker: bool,
    handoff_id: Option<uuid::Uuid>,
) -> Value {
    match run_inner(
        action,
        &plugin_dir,
        &state_dir,
        &root,
        force,
        worker,
        handoff_id,
    )
    .await
    {
        Ok(value) => value,
        Err(error) => {
            serde_json::json!({"ok":false,"state":"failed","current_version":env!("CARGO_PKG_VERSION"),"target_version":null,"message":error.to_string(),"next_check_at":null,"gate_blocked":root.join("update-gate.json").exists(),"automatic_install_supported":cfg!(target_os = "macos")})
        }
    }
}

async fn run_inner(
    action: &str,
    plugin_dir: &Path,
    dir: &Path,
    root: &Path,
    force: bool,
    worker: bool,
    handoff_id: Option<uuid::Uuid>,
) -> Result<Value> {
    if handoff_id.is_some() && (!worker || action != "apply") {
        return Err(AppError::invalid("锁交接仅供更新 worker 使用"));
    }
    let inherited = match handoff_id {
        Some(id) => Some(apply::prepare_handoff(root, dir, id).await?),
        None => None,
    };
    if action == "status" && apply::acknowledge(root, plugin_dir)? {
        return Ok(
            serde_json::json!({"ok":true,"state":"installing","current_version":env!("CARGO_PKG_VERSION"),"target_version":null,"message":"正在等待更新提交","next_check_at":null,"gate_blocked":true,"automatic_install_supported":cfg!(target_os = "macos")}),
        );
    }
    fs::create_dir_all(dir)?;
    let dir = fs::canonicalize(dir)?;
    let dir = dir.as_path();
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("update.lock"))?;
    let locked = lock.try_lock_exclusive();
    if locked.is_err() && action == "status" {
        let path = dir.join("update-state.json");
        if let Ok(bytes) = fs::read(path)
            && let Ok(state) = serde_json::from_slice::<State>(&bytes)
        {
            // 状态读取不等待下载或校验；安装期间仍须阻止新 VM 提前发布路径。
            let mut value = output(&state, root, true);
            if state.state == "installing" {
                value["gate_blocked"] = Value::Bool(true);
            }
            return Ok(value);
        }
    }
    if locked.is_err() {
        lock.lock_exclusive()?;
    }
    let path = dir.join("update-state.json");
    let mut state: State = if path.exists() {
        serde_json::from_slice(&fs::read(&path)?)?
    } else {
        State::default()
    };
    if let Some(id) = handoff_id {
        let bytes = fs::read(&path)?;
        let actual = format!("{:x}", base16ct::HexDisplay(&Sha256::digest(&bytes)));
        let gate: Value = serde_json::from_slice(&fs::read(root.join("update-gate.json"))?)?;
        if gate["phase"] != "handoff"
            || gate["id"] != id.to_string()
            || gate["state_digest"] != actual
        {
            return Err(AppError::new(
                "update_recovery_required",
                "交接期间更新状态发生变化",
            ));
        }
    }
    if action == "status" {
        if matches!(state.state.as_str(), "downloading" | "verifying") {
            // 已拿到更新锁，且记录仍处于传输阶段，说明原下载者已退出。
            state.state = "failed".into();
            state.message = "更新下载已中断，请重新下载".into();
            state.progress = None;
            save(dir, &mut state)?;
            return Ok(output(&state, root, false));
        }
        if state.state == "installing" {
            if now()?.saturating_sub(state.worker_started_at) > 30 {
                state.state = "failed".into();
                state.message = "更新进程未持有锁，需重新检查状态".into();
                save(dir, &mut state)?;
                return Ok(output(&state, root, false));
            }
            let mut value = output(&state, root, true);
            value["gate_blocked"] = Value::Bool(true);
            return Ok(value);
        }
        return Ok(output(&state, root, true));
    }
    // 源码目录没有发布清单；linked 目录以及显式本地程序都不能覆盖。
    let linked = fs::symlink_metadata(plugin_dir)?.file_type().is_symlink();
    if linked
        || std::env::var_os("DATAPIER_BINARY").is_some()
        || !plugin_dir.join("release-sha256.txt").is_file()
    {
        state.state = "development".into();
        state.message = "开发安装不自动下载或替换".into();
        save(dir, &mut state)?;
        return Ok(output(&state, root, true));
    }
    let manifest = fs::read_to_string(plugin_dir.join("plugin.toml"))?;
    let installed: Vec<_> = manifest
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "version").then(|| value.trim().trim_matches('"'))
        })
        .collect();
    if installed != [env!("CARGO_PKG_VERSION")] {
        return Err(AppError::new(
            "update_incompatible",
            "当前程序与 plugin 版本不一致",
        ));
    }
    if action == "schedule" {
        if !cfg!(target_os = "macos") {
            return Err(AppError::new(
                "update_unavailable",
                "当前平台尚未验证自动安装",
            ));
        }
        if !matches!(state.state.as_str(), "ready" | "waiting") || state.staged_dir.is_none() {
            return Err(AppError::invalid("没有已准备的更新"));
        }
        state.apply_after_exit = true;
        state.message = "已安排退出后安装更新".into();
        save(dir, &mut state)?;
        return Ok(output(&state, root, true));
    }
    if action == "download" {
        // 同一目标的并发下载会依次取得更新锁，后到者复用已经通过校验的暂存包。
        if matches!(state.state.as_str(), "ready" | "waiting")
            && state.staged_dir.as_deref().is_some_and(Path::is_dir)
        {
            return Ok(output(&state, root, true));
        }
        state.download_source = Some(if force { "automatic" } else { "manual" }.into());
        match download(&mut state, dir).await {
            Ok(staged_dir) => {
                state.staged_dir = Some(staged_dir);
                state.state = "ready".into();
                state.message = "更新已下载并校验，等待退出后安装".into();
                save(dir, &mut state)?;
                return Ok(output(&state, root, true));
            }
            Err(error) => {
                state.state = "failed".into();
                state.message = error.to_string();
                save(dir, &mut state)?;
                return Ok(output(&state, root, false));
            }
        }
    }
    if action == "apply" {
        if !worker {
            if !cfg!(target_os = "macos") {
                return Err(AppError::new(
                    "update_unavailable",
                    "当前平台尚未验证自动安装",
                ));
            }
            state.state = "installing".into();
            state.worker_started_at = now()?;
            state.message = "正在启动更新进程".into();
            save(dir, &mut state)?;
            let mut command = std::process::Command::new(std::env::current_exe()?);
            command
                .arg("--data-dir")
                .arg(root)
                .arg("plugin-update")
                .arg("--action")
                .arg("apply")
                .arg("--plugin-dir")
                .arg(plugin_dir)
                .arg("--state-dir")
                .arg(dir)
                .arg("--worker")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                // SAFETY: 子进程在 exec 前仅调用 async-signal-safe 的 setsid。
                unsafe {
                    command.pre_exec(|| {
                        if libc::setsid() < 0 {
                            Err(std::io::Error::last_os_error())
                        } else {
                            Ok(())
                        }
                    });
                }
            }
            if let Err(error) = command.spawn() {
                state.state = "failed".into();
                state.message = error.to_string();
                save(dir, &mut state)?;
                return Ok(output(&state, root, false));
            }
            return Ok(output(&state, root, true));
        }
        return match apply::run(&state, root, plugin_dir, dir, inherited, handoff_id).await {
            Ok(()) => {
                state.state = "installed".into();
                state.apply_after_exit = false;
                state.message = "更新已安装".into();
                save(dir, &mut state)?;
                Ok(output(&state, root, true))
            }
            Err(error) if error.code == "update_engine_busy" && handoff_id.is_none() => {
                let id = uuid::Uuid::new_v4();
                let state_digest = format!(
                    "{:x}",
                    base16ct::HexDisplay(&Sha256::digest(fs::read(&path)?))
                );
                let options = datapier_engine::ClientOptions {
                    kind: datapier_core::model::ClientKind::WindowBridge,
                    start_engine: false,
                    ..Default::default()
                };
                let result = async {
                    let client = datapier_engine::Client::connect_with(root, options).await?;
                    let value = client.request("update.handoff", serde_json::json!({"handoff_id":id,"state_dir":dir,"plugin_dir":plugin_dir,"state_digest":state_digest})).await?;
                    if value["accepted"].as_bool() != Some(true) { return Err(AppError::new("update_handoff_failed", "Engine 未确认锁交接")); }
                    Ok::<(), AppError>(())
                }.await;
                match result {
                    Ok(()) => Ok(output(&state, root, true)),
                    Err(error) => {
                        let waiting =
                            matches!(error.code.as_str(), "update_busy" | "update_engine_busy");
                        state.state = if waiting { "waiting" } else { "failed" }.into();
                        if !waiting {
                            state.apply_after_exit = false;
                            state.staged_dir = None;
                        }
                        state.message = error.to_string();
                        save(dir, &mut state)?;
                        Ok(output(&state, root, false))
                    }
                }
            }
            Err(error) if error.code == "update_rolled_back" => {
                state.state = "available".into();
                state.apply_after_exit = false;
                state.staged_dir = None;
                state.message = error.message.clone();
                save(dir, &mut state)?;
                Ok(output(&state, root, false))
            }
            Err(error) => {
                let waiting = matches!(error.code.as_str(), "update_busy" | "update_engine_busy");
                state.state = if waiting { "waiting" } else { "failed" }.into();
                if !waiting {
                    state.apply_after_exit = false;
                    state.staged_dir = None;
                }
                state.message = error.to_string();
                save(dir, &mut state)?;
                Ok(output(&state, root, false))
            }
        };
    }
    let time = now()?;
    if time < state.rate_limit_until
        || (!force && time < state.last_attempt.saturating_add(INTERVAL))
    {
        return Ok(output(&state, root, true));
    }
    // 先持久化尝试时间，即使网络请求失败也不会被其他窗口立即重复。
    state.last_attempt = time;
    state.state = "checking".into();
    state.message.clear();
    save(dir, &mut state)?;
    let result = check(&mut state, time).await;
    if let Err(error) = result {
        state.state = "failed".into();
        state.message = error.to_string();
        save(dir, &mut state)?;
        return Ok(output(&state, root, false));
    }
    save(dir, &mut state)?;
    Ok(output(&state, root, true))
}

async fn check(state: &mut State, time: u64) -> Result<()> {
    let client = reqwest::Client::builder()
        .https_only(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| AppError::new("network", e.to_string()))?;
    let mut request = client
        .get(API)
        .header("User-Agent", "datapier-updater")
        .header("Accept", "application/vnd.github+json");
    if let Some(etag) = &state.etag {
        request = request.header("If-None-Match", etag);
    }
    let mut response = request
        .send()
        .await
        .map_err(|e| AppError::new("network", e.to_string()))?;
    if response.status() == reqwest::StatusCode::NOT_MODIFIED {
        state.last_success = time;
        if state.target_version.as_deref().and_then(version) <= version(env!("CARGO_PKG_VERSION")) {
            state.target_version = None;
            state.staged_dir = None;
            state.apply_after_exit = false;
        }
        state.state = if state.staged_dir.is_some() {
            "ready"
        } else if state.target_version.is_some() {
            "available"
        } else {
            "current"
        }
        .into();
        state.message = "Release 未变化".into();
        return Ok(());
    }
    if response.status() == reqwest::StatusCode::FORBIDDEN
        || response.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        let reset = response
            .headers()
            .get("x-ratelimit-reset")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(time.saturating_add(INTERVAL));
        let retry = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(|seconds| time.saturating_add(seconds))
            .unwrap_or(0);
        state.rate_limit_until = reset.max(retry).max(time.saturating_add(1));
        return Err(AppError::new("rate_limited", "GitHub 请求受限"));
    }
    if !response.status().is_success() {
        return Err(AppError::new(
            "network",
            format!("GitHub HTTP {}", response.status()),
        ));
    }
    let etag = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| AppError::new("network", e.to_string()))?
    {
        if bytes.len().saturating_add(chunk.len()) > 1024 * 1024 {
            return Err(AppError::invalid("Release 响应过大"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let release: Value = serde_json::from_slice(&bytes)?;
    if release["draft"].as_bool() != Some(false) || release["prerelease"].as_bool() != Some(false) {
        return Err(AppError::invalid("Release 不是正式版本"));
    }
    let tag = release["tag_name"]
        .as_str()
        .and_then(|s| s.strip_prefix('v'))
        .ok_or_else(|| AppError::invalid("Release 标签无效"))?;
    let candidate = version(tag).ok_or_else(|| AppError::invalid("Release 版本无效"))?;
    let current =
        version(env!("CARGO_PKG_VERSION")).ok_or_else(|| AppError::invalid("当前版本无效"))?;
    let target = target().ok_or_else(|| AppError::invalid("不支持的平台"))?;
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    let names = [
        format!("datapier-plugin-{tag}.zip"),
        format!("datapier-{tag}-{target}.{extension}"),
    ];
    let assets = release["assets"]
        .as_array()
        .ok_or_else(|| AppError::invalid("Release 附件缺失"))?;
    for name in &names {
        let matching: Vec<_> = assets
            .iter()
            .filter(|item| item["name"].as_str() == Some(name))
            .collect();
        if matching.len() != 1
            || !matching[0]["digest"].as_str().is_some_and(|s| {
                s.strip_prefix("sha256:")
                    .is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            })
        {
            return Err(AppError::invalid(format!(
                "Release 附件或 SHA-256 缺失：{name}"
            )));
        }
    }
    let plugin_digest = assets
        .iter()
        .find(|item| item["name"].as_str() == Some(&names[0]))
        .and_then(|item| item["digest"].as_str())
        .and_then(|value| value.strip_prefix("sha256:"));
    let binary_digest = assets
        .iter()
        .find(|item| item["name"].as_str() == Some(&names[1]))
        .and_then(|item| item["digest"].as_str())
        .and_then(|value| value.strip_prefix("sha256:"));
    // 退出安装只针对用户选择过的同一候选；新 Release 不能继承旧意图或暂存目录。
    if candidate <= current
        || state.target_version.as_deref() != Some(tag)
        || state.plugin_digest.as_deref() != plugin_digest
        || state.binary_digest.as_deref() != binary_digest
    {
        state.staged_dir = None;
        state.apply_after_exit = false;
    }
    state.plugin_digest = plugin_digest.map(str::to_owned);
    state.binary_digest = binary_digest.map(str::to_owned);
    state.last_success = time;
    state.etag = etag;
    state.target_version = (candidate > current).then(|| tag.to_owned());
    state.state = if state.staged_dir.is_some() {
        "ready"
    } else if state.target_version.is_some() {
        "available"
    } else {
        "current"
    }
    .into();
    state.message = if state.target_version.is_some() {
        "发现新版本"
    } else {
        "当前已是最新版本"
    }
    .into();
    Ok(())
}

async fn download_archive(
    client: &reqwest::Client,
    url: &str,
    path: &Path,
    expected_hash: &str,
    phase: DownloadPhase,
    state: &mut State,
    dir: &Path,
) -> Result<()> {
    state.state = "downloading".into();
    state.message = "正在下载更新".into();
    state.progress = Some(DownloadProgress {
        phase,
        received_bytes: 0,
        total_bytes: None,
    });
    save(dir, state)?;
    let mut response = client
        .get(url)
        .header("User-Agent", "datapier-updater")
        .send()
        .await
        .map_err(|e| AppError::new("network", e.to_string()))?;
    if !response.status().is_success() {
        return Err(AppError::new(
            "network",
            format!("下载 HTTP {}", response.status()),
        ));
    }
    let total = response.content_length().filter(|size| *size > 0);
    state.progress.as_mut().unwrap().total_bytes = total;
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut last_report = std::time::Instant::now();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| AppError::new("network", e.to_string()))?
    {
        size = size.saturating_add(chunk.len() as u64);
        if matches!(phase, DownloadPhase::Plugin) && size > 128 * 1024 * 1024 {
            return Err(AppError::invalid("plugin 包过大"));
        }
        file.write_all(&chunk)?;
        hash.update(&chunk);
        let progress = state.progress.as_mut().unwrap();
        progress.received_bytes = size;
        if progress.total_bytes.is_some_and(|total| size > total) {
            progress.total_bytes = None;
        }
        if last_report.elapsed() >= std::time::Duration::from_millis(500) {
            save(dir, state)?;
            last_report = std::time::Instant::now();
        }
    }
    file.sync_all()?;
    if let Some(progress) = state.progress.as_mut()
        && progress.total_bytes.is_some_and(|total| total != size)
    {
        progress.total_bytes = None;
    }
    // 收完字节不代表归档可信；校验及安装器完成后才发布 ready。
    state.state = "verifying".into();
    state.message = "正在校验更新".into();
    save(dir, state)?;
    if format!("{:x}", base16ct::HexDisplay(&hash.finalize())) != expected_hash.to_ascii_lowercase()
    {
        return Err(AppError::invalid("下载归档 SHA-256 不匹配"));
    }
    Ok(())
}

// 发布 API 的 digest 绑定整个 plugin 包；包内二进制清单必须与同一 Release 的附件摘要一致。
async fn download(state: &mut State, dir: &Path) -> Result<PathBuf> {
    state.apply_after_exit = false;
    state.staged_dir = None;
    state.progress = None;
    let stage = dir.join(format!("stage-{}", uuid::Uuid::new_v4()));
    fs::create_dir(&stage)?;
    let result = async {
        let ver = state
            .target_version
            .clone()
            .ok_or_else(|| AppError::invalid("没有可下载的目标版本"))?;
        if version(&ver).is_none() {
            return Err(AppError::invalid("目标版本无效"));
        }
        let plugin_hash = state
            .plugin_digest
            .clone()
            .ok_or_else(|| AppError::invalid("缺少 plugin 摘要"))?;
        let binary_hash = state
            .binary_digest
            .clone()
            .ok_or_else(|| AppError::invalid("缺少程序归档摘要"))?;
        let target = target().ok_or_else(|| AppError::invalid("不支持的平台"))?;
        let ext = if cfg!(windows) { "zip" } else { "tar.gz" };
        let binary_name = format!("datapier-{ver}-{target}.{ext}");
        let base = format!("https://github.com/huspx/datapier/releases/download/v{ver}");
        let url = format!("{base}/datapier-plugin-{ver}.zip");
        let archive_path = stage.join("plugin.zip");
        let program_archive = stage.join(format!("program.{ext}"));
        let client = reqwest::Client::builder()
            .https_only(true)
            .timeout(std::time::Duration::from_secs(300))
            .build()
            .map_err(|e| AppError::new("network", e.to_string()))?;
        download_archive(
            &client,
            &url,
            &archive_path,
            &plugin_hash,
            DownloadPhase::Plugin,
            state,
            dir,
        )
        .await?;
        let mut zip = zip::ZipArchive::new(fs::File::open(&archive_path)?)
            .map_err(|e| AppError::invalid(e.to_string()))?;
        let mut names = std::collections::HashSet::new();
        let mut total = 0u64;
        for index in 0..zip.len() {
            let mut entry = zip
                .by_index(index)
                .map_err(|e| AppError::invalid(e.to_string()))?;
            let name = entry
                .name()
                .map_err(|e| AppError::invalid(e.to_string()))?
                .into_owned();
            if !names.insert(name.clone())
                || !name.starts_with("datapier/")
                || name.contains('\\')
                || entry.is_dir()
                || entry
                    .unix_mode()
                    .is_some_and(|mode| mode & 0o170000 == 0o120000)
            {
                return Err(AppError::invalid("plugin 归档成员无效或重复"));
            }
            let relative = entry
                .enclosed_name()
                .ok_or_else(|| AppError::invalid("plugin 归档路径无效"))?;
            total = total.saturating_add(entry.size());
            if total > 128 * 1024 * 1024 {
                return Err(AppError::invalid("plugin 解包内容过大"));
            }
            let destination = stage.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(destination)?;
            std::io::copy(&mut entry, &mut output)?;
        }
        drop(zip);
        let plugin = stage.join("datapier");
        let manifest = fs::read_to_string(plugin.join("plugin.toml"))?;
        let versions: Vec<_> = manifest
            .lines()
            .filter_map(|line| {
                let (key, value) = line.split_once('=')?;
                (key.trim() == "version").then(|| value.trim().trim_matches('"'))
            })
            .collect();
        if versions != [ver.as_str()]
            || !manifest
                .lines()
                .any(|line| line.trim() == "id = \"datapier\"")
        {
            return Err(AppError::invalid("plugin manifest 与 Release 不匹配"));
        }
        let expected = format!("{}  {}", binary_hash.to_ascii_lowercase(), binary_name);
        let sums = fs::read_to_string(plugin.join("release-sha256.txt"))?;
        if sums.lines().filter(|line| *line == expected).count() != 1 {
            return Err(AppError::invalid("程序归档摘要与 Release 不匹配"));
        }
        if fs::read_to_string(plugin.join("release-url.txt"))?.trim() != base {
            return Err(AppError::invalid("plugin 下载地址与 Release 不匹配"));
        }
        download_archive(
            &client,
            &format!("{base}/{binary_name}"),
            &program_archive,
            &binary_hash,
            DownloadPhase::Program,
            state,
            dir,
        )
        .await?;
        // 复用发布安装器验证唯一程序成员、程序 SHA-256 和 --version；只写入本轮暂存目录。
        let mut command = if cfg!(windows) {
            let mut cmd = tokio::process::Command::new("powershell.exe");
            cmd.args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ])
            .arg(plugin.join("install/windows.ps1"));
            cmd
        } else {
            let mut cmd = tokio::process::Command::new("/bin/sh");
            cmd.arg(plugin.join("install/unix.sh"));
            cmd
        };
        let cache = stage.join("cache");
        command
            .env("DATAPIER_CACHE_DIR", &cache)
            .env("DATAPIER_RELEASE_URL", &base)
            .env("DATAPIER_UPDATE_ARCHIVE", &program_archive)
            .env_remove("DATAPIER_BINARY")
            .kill_on_drop(true);
        let result = tokio::time::timeout(std::time::Duration::from_secs(300), command.output())
            .await
            .map_err(|_| AppError::new("update", "程序校验超时"))??;
        if !result.status.success() {
            return Err(AppError::new(
                "update",
                format!("程序准备失败：{}", String::from_utf8_lossy(&result.stderr)),
            ));
        }
        let response: Value = serde_json::from_slice(&result.stdout)?;
        let cwd = response["cwd"]
            .as_str()
            .ok_or_else(|| AppError::invalid("程序准备结果无效"))?;
        let cache = fs::canonicalize(cache)?;
        let cwd = fs::canonicalize(cwd)?;
        if response["ok"].as_bool() != Some(true)
            || !cwd.starts_with(cache)
            || !cwd.to_string_lossy().contains(&target)
        {
            return Err(AppError::invalid("程序缓存路径与目标不匹配"));
        }
        fs::remove_file(&program_archive)?;
        Ok(stage.clone())
    }
    .await;
    if result.is_err() {
        let _ = fs::remove_dir_all(&stage);
    }
    result
}

#[cfg(test)]
#[path = "../tests/unit/plugin_update.rs"]
mod tests;
