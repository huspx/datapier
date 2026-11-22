//! 项目文件与可领取遗留资源的 Engine 会话入口。
mod artifacts;
mod save;

use super::*;
use datapier_core::{
    model::project::{ProjectManifest, ProjectRecoveryAction, ProjectSavePreview, ProjectSnapshot},
    project,
    storage::transfer::{ArtifactIdentity, BindingOverrides, ProjectBinding, TransferArtifact},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

const MIB: usize = 1024 * 1024;
const PROJECT_BYTES: usize = 128 * MIB;
const PLAN_BYTES: usize = 320 * MIB;
const WORK_BYTES: usize = 256 * MIB;
const OWNER_BYTES: usize = 768 * MIB;
const TOTAL_BYTES: usize = 1536 * MIB;
const PLAN_LIFETIME: Duration = Duration::from_secs(600);

#[derive(Default)]
struct Budget {
    bytes: usize,
    owners: HashMap<Uuid, usize>,
    project_counts: HashMap<Uuid, usize>,
    plan_counts: HashMap<Uuid, usize>,
    projects: usize,
    plans: usize,
}
struct Lease {
    budget: Arc<Mutex<Budget>>,
    owner: Uuid,
    bytes: usize,
}
impl Drop for Lease {
    fn drop(&mut self) {
        let mut budget = self.budget.lock().unwrap();
        budget.bytes -= self.bytes;
        let used = budget
            .owners
            .get_mut(&self.owner)
            .expect("项目预算所有者存在");
        *used -= self.bytes;
        if *used == 0 {
            budget.owners.remove(&self.owner);
        }
        if self.bytes == PROJECT_BYTES {
            budget.projects -= 1;
            let count = budget
                .project_counts
                .get_mut(&self.owner)
                .expect("项目计数存在");
            *count -= 1;
            if *count == 0 {
                budget.project_counts.remove(&self.owner);
            }
        }
        if self.bytes == PLAN_BYTES {
            budget.plans -= 1;
            let count = budget
                .plan_counts
                .get_mut(&self.owner)
                .expect("计划计数存在");
            *count -= 1;
            if *count == 0 {
                budget.plan_counts.remove(&self.owner);
            }
        }
    }
}
struct ChargedSnapshot {
    snapshot: ProjectSnapshot,
    _memory: Arc<super::transfer::MemoryReservation>,
}
impl std::ops::Deref for ChargedSnapshot {
    type Target = ProjectSnapshot;
    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}
struct ProjectSlot {
    snapshot: Arc<ChargedSnapshot>,
    restored_baseline: bool,
    _lease: Arc<Lease>,
}
impl std::ops::Deref for ProjectSlot {
    type Target = ProjectSnapshot;
    fn deref(&self) -> &Self::Target {
        &self.snapshot
    }
}
struct PlanEntry {
    owner: Uuid,
    workspace: String,
    project_id: Uuid,
    created: Instant,
    slot: Arc<AsyncMutex<SavePlan>>,
}
struct RecoveryEntry {
    owner: Uuid,
    created: Instant,
    slot: Arc<AsyncMutex<RecoveryPlan>>,
}
struct Upload {
    owner: Uuid,
    workspace: String,
    project_id: Uuid,
    baseline: BTreeMap<String, String>,
    revision: u64,
    created: Instant,
    files: BTreeMap<String, String>,
    bytes: usize,
    _lease: Arc<Lease>,
}
fn defer_drop<T: Send + 'static>(value: T) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn_blocking(move || drop(value));
    } else {
        drop(value);
    }
}

#[derive(Default)]
pub(super) struct ProjectState {
    projects: HashMap<(Uuid, String, Uuid), Arc<AsyncMutex<ProjectSlot>>>,
    plans: HashMap<Uuid, PlanEntry>,
    recoveries: HashMap<Uuid, RecoveryEntry>,
    artifacts: HashMap<Uuid, Arc<AsyncMutex<()>>>,
    uploads: HashMap<Uuid, Upload>,
    bindings: HashMap<(Uuid, String), Arc<AsyncMutex<()>>>,
    budget: Arc<Mutex<Budget>>,
}
impl ProjectState {
    fn prune(&mut self) {
        let expired: Vec<_> = self
            .plans
            .iter()
            .filter(|(_, p)| {
                p.created.elapsed() >= PLAN_LIFETIME && Arc::strong_count(&p.slot) == 1
            })
            .map(|(id, _)| *id)
            .collect();
        let retired: Vec<_> = expired
            .into_iter()
            .filter_map(|id| self.plans.remove(&id))
            .collect();
        if !retired.is_empty() {
            defer_drop(retired);
        }
        self.recoveries.retain(|_, plan| {
            plan.created.elapsed() < PLAN_LIFETIME || Arc::strong_count(&plan.slot) > 1
        });
        self.artifacts.retain(|_, gate| Arc::strong_count(gate) > 1);
        self.uploads
            .retain(|_, upload| upload.created.elapsed() < PLAN_LIFETIME);
    }
    fn reserve(&mut self, owner: Uuid, bytes: usize) -> Result<Arc<Lease>> {
        self.prune();
        let mut budget = self.budget.lock().unwrap();
        let owner_used = budget.owners.get(&owner).copied().unwrap_or(0);
        if bytes > OWNER_BYTES.saturating_sub(owner_used)
            || bytes > TOTAL_BYTES.saturating_sub(budget.bytes)
        {
            return Err(AppError::new(
                "resource_limit",
                "项目内存预算不足，请关闭项目或丢弃保存预览",
            ));
        }
        if bytes == PROJECT_BYTES
            && (budget.projects >= 8
                || budget.project_counts.get(&owner).copied().unwrap_or(0) >= 2)
        {
            return Err(AppError::new("resource_limit", "打开项目数超过限制"));
        }
        if bytes == PLAN_BYTES
            && (budget.plans >= 4 || budget.plan_counts.get(&owner).copied().unwrap_or(0) >= 1)
        {
            return Err(AppError::new(
                "resource_limit",
                "保存预览数超过限制，请先提交、丢弃或替换",
            ));
        }
        budget.bytes += bytes;
        *budget.owners.entry(owner).or_default() += bytes;
        if bytes == PROJECT_BYTES {
            budget.projects += 1;
            *budget.project_counts.entry(owner).or_default() += 1;
        }
        if bytes == PLAN_BYTES {
            budget.plans += 1;
            *budget.plan_counts.entry(owner).or_default() += 1;
        }
        Ok(Arc::new(Lease {
            budget: self.budget.clone(),
            owner,
            bytes,
        }))
    }
    fn insert_recovery(&mut self, id: Uuid, plan: RecoveryPlan) -> Result<()> {
        self.prune();
        if self.recoveries.len() >= 64
            || self
                .recoveries
                .values()
                .filter(|entry| entry.owner == plan.owner)
                .count()
                >= 8
        {
            return Err(AppError::new("resource_limit", "恢复计划数超过限制"));
        }
        self.recoveries.insert(
            id,
            RecoveryEntry {
                owner: plan.owner,
                created: Instant::now(),
                slot: Arc::new(AsyncMutex::new(plan)),
            },
        );
        Ok(())
    }
    /// 客户端离开时解除句柄；正在执行的任务持有 lease 直到任务结束。
    pub(super) fn remove_owner(&mut self, owner: Uuid) {
        let project_keys: Vec<_> = self
            .projects
            .keys()
            .filter(|(client, _, _)| *client == owner)
            .cloned()
            .collect();
        let plan_keys: Vec<_> = self
            .plans
            .iter()
            .filter(|(_, plan)| plan.owner == owner)
            .map(|(id, _)| *id)
            .collect();
        let projects: Vec<_> = project_keys
            .into_iter()
            .filter_map(|key| self.projects.remove(&key))
            .collect();
        let plans: Vec<_> = plan_keys
            .into_iter()
            .filter_map(|id| self.plans.remove(&id))
            .collect();
        self.recoveries.retain(|_, plan| plan.owner != owner);
        self.uploads.retain(|_, upload| upload.owner != owner);
        if !projects.is_empty() || !plans.is_empty() {
            defer_drop((projects, plans));
        }
        self.prune();
    }
}
struct SavePlan {
    owner: Uuid,
    workspace: String,
    snapshot: Arc<AsyncMutex<ProjectSlot>>,
    preview: ProjectSavePreview,
    used: bool,
    _lease: Arc<Lease>,
    memory: Arc<super::transfer::MemoryReservation>,
}
struct RecoveryPlan {
    owner: Uuid,
    artifact_id: Uuid,
    artifact_revision: u64,
    digest: String,
    action: ProjectRecoveryAction,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingInput {
    connection_alias: String,
    profile_id: Uuid,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    redis_db: Option<u32>,
    #[serde(default)]
    paths: BTreeMap<String, PathBuf>,
    revision: u64,
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| AppError::new("internal", format!("项目文件任务失败：{e}")))?
}
async fn blocking_leased<T: Send + 'static>(
    lease: Arc<Lease>,
    work: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    blocking(move || {
        let _lease = lease;
        work()
    })
    .await
}
// 校验每个已存在的路径分量，避免项目内部符号链接把相对文件指向外部。
fn resolve_relative_path(root: &Path, relative: &Path) -> Result<PathBuf> {
    let text = relative
        .to_str()
        .ok_or_else(|| AppError::invalid("项目路径必须是 UTF-8"))?;
    if text.is_empty()
        || text.contains(['\\', ':'])
        || relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(AppError::invalid("项目文件路径必须是安全的相对路径"));
    }
    let mut target = root.to_path_buf();
    let mut parts = relative.components().peekable();
    while let Some(part) = parts.next() {
        target.push(part.as_os_str());
        match std::fs::symlink_metadata(&target) {
            Ok(meta)
                if meta.file_type().is_symlink() || (parts.peek().is_some() && !meta.is_dir()) =>
            {
                return Err(AppError::new(
                    "binding_required",
                    "项目路径包含符号链接或特殊文件",
                ));
            }
            Ok(meta) if parts.peek().is_none() && !meta.is_file() => {
                return Err(AppError::new("binding_required", "项目路径不是普通文件"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(target)
}
fn summaries(snapshot: &ProjectSnapshot) -> Vec<Value> {
    snapshot
        .files
        .iter()
        .map(|(path, text)| json!({"path":path,"bytes":text.len()}))
        .collect()
}
fn snapshot_json(snapshot: &ProjectSnapshot) -> Value {
    json!({"project_id":snapshot.manifest.id,"manifest":snapshot.manifest,"root":snapshot.root,"digests":snapshot.digests,"file_summaries":summaries(snapshot),"files":{}})
}
fn change_summaries(preview: &ProjectSavePreview, comparisons: bool) -> Vec<Value> {
    // 正文与摘要共用 IPC 额度；只发送有界 UTF-8 前缀，且明确标记截断。
    let mut remaining = 64 * 1024;
    preview.changes.iter().map(|c| {
        let mut item = json!({"path":c.path,"old_digest":c.old_digest,"new_digest":c.new_digest,"old_bytes":c.old_content.as_ref().map(String::len),"new_bytes":c.new_content.as_ref().map(String::len)});
        if comparisons && c.path.to_ascii_lowercase().ends_with(".sql") && remaining >= 8 {
            let limit = (remaining / 2).min(8 * 1024);
            fn excerpt(content: Option<&str>, limit: usize) -> (&str, bool) {
                content.map(|text| {
                    let mut end = text.len().min(limit);
                    while !text.is_char_boundary(end) { end -= 1; }
                    (&text[..end], end == text.len())
                }).unwrap_or(("", true))
            }
            let (old, old_complete) = excerpt(c.old_content.as_deref(), limit);
            let (new, new_complete) = excerpt(c.new_content.as_deref(), limit);
            remaining -= old.len() + new.len();
            item["comparison"] = json!({"before":old,"after":new,"complete":old_complete && new_complete});
        }
        item
    }).collect()
}
fn digest_bytes(bytes: &[u8]) -> String {
    format!("{:x}", base16ct::HexDisplay(&Sha256::digest(bytes)))
}

#[cfg(windows)]
fn windows_entry_identity(path: &Path, directory: bool) -> std::io::Result<String> {
    use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_READ_ATTRIBUTES,
        GetFileInformationByHandle,
    };

    let file = std::fs::OpenOptions::new()
        .access_mode(FILE_READ_ATTRIBUTES)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)?;
    let mut information = std::mem::MaybeUninit::<BY_HANDLE_FILE_INFORMATION>::uninit();
    // SAFETY: file 在调用期间持有有效句柄，information 是大小和对齐正确的可写缓冲区。
    if unsafe { GetFileInformationByHandle(file.as_raw_handle(), information.as_mut_ptr()) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: API 成功时已完整初始化 BY_HANDLE_FILE_INFORMATION。
    let information = unsafe { information.assume_init() };
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        || (information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
    {
        return Err(std::io::Error::other("项目路径类型或身份已变化"));
    }
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok(format!("{}:{index}", information.dwVolumeSerialNumber))
}

// 目录被删除并在同一路径重建时不能继承旧目录的授权。
pub(super) fn binding_root_identity(root: &str) -> Result<String> {
    let metadata = std::fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::new("binding_required", "项目目录身份已变化"));
    }
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    };
    #[cfg(windows)]
    let identity = windows_entry_identity(Path::new(root), true)
        .map_err(|error| AppError::new("binding_required", error.to_string()))?;
    #[cfg(not(any(unix, windows)))]
    return Err(AppError::new("unsupported", "本平台无法确认项目目录身份"));
    #[cfg(any(unix, windows))]
    Ok(format!("{root}\0{identity}"))
}

fn stage_entry_identity(path: &Path, metadata: &std::fs::Metadata) -> Option<String> {
    #[cfg(not(windows))]
    let _ = path;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("{}:{}", metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        windows_entry_identity(path, metadata.is_dir()).ok()
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        None
    }
}

// 对外部工作树逐分量检查；不将符号链接替换当作普通文件。
fn current_digest(root: &Path, relative: &str) -> Result<Option<String>> {
    let mut path = root.to_path_buf();
    for part in relative.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains(['\\', ':']) {
            return Err(AppError::invalid("项目路径无效"));
        }
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(AppError::new("recovery_conflict", "项目路径被符号链接替换"));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        }
    }
    let meta = std::fs::symlink_metadata(&path)?;
    if !meta.is_file() || meta.len() > 16 * 1024 * 1024 {
        return Err(AppError::new(
            "recovery_conflict",
            "项目文件类型或大小发生变化",
        ));
    }
    let mut file = std::fs::File::open(path)?;
    let mut sha = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        sha.update(&buffer[..n]);
    }
    Ok(Some(format!("{:x}", base16ct::HexDisplay(&sha.finalize()))))
}
fn artifact_view(artifact: &TransferArtifact) -> Value {
    json!({"id":artifact.id,"run_id":artifact.run_id,"owner_client_id":artifact.owner_client_id,"kind":artifact.kind,"state":artifact.state,"revision":artifact.revision,"identity":{"task_id":artifact.identity.task_id,"path":artifact.identity.path,"canonical_path":artifact.identity.canonical_path,"file_id":artifact.identity.file_id,"size":artifact.identity.size,"sha256":artifact.identity.sha256,"kind":artifact.identity.kind,"export_name":artifact.identity.export_name,"media_type":artifact.identity.media_type}})
}
fn write_new(path: &Path, data: &[u8]) -> Result<()> {
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    if let Err(error) = output.write_all(data).and_then(|_| output.sync_all()) {
        drop(output);
        let _ = std::fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}

fn verified_file(identity: &ArtifactIdentity) -> Result<PathBuf> {
    let path = identity
        .path
        .as_ref()
        .ok_or_else(|| AppError::invalid("资源未登记路径"))?;
    let canonical = identity
        .canonical_path
        .as_ref()
        .ok_or_else(|| AppError::new("recovery_conflict", "资源缺少规范路径，无法验证身份"))?;
    let expected_hash = identity
        .sha256
        .as_ref()
        .ok_or_else(|| AppError::new("recovery_conflict", "资源缺少内容摘要，无法验证身份"))?;
    let expected_size = identity
        .size
        .ok_or_else(|| AppError::new("recovery_conflict", "资源缺少长度，无法验证身份"))?;
    let expected_id = identity
        .file_id
        .as_ref()
        .ok_or_else(|| AppError::new("recovery_conflict", "资源缺少文件身份，无法安全处理"))?;
    let observed = datapier_core::execution::data_files::output_snapshot(path, true)?;
    if observed.canonical_path != *canonical
        || observed.file_id.as_ref() != Some(expected_id)
        || observed.size != expected_size
        || observed.sha256.as_ref() != Some(expected_hash)
    {
        return Err(AppError::new(
            "recovery_conflict",
            "资源文件身份或内容已变化",
        ));
    }
    Ok(path.clone())
}

fn growing_partial(artifact: &TransferArtifact) -> bool {
    artifact.state == "preparing"
        && matches!(
            artifact.kind.as_str(),
            "export_partial" | "report" | "input_index"
        )
}
// 崩溃可能发生在写入与封口登记之间；只重新核对原文件身份，内容必须由领取者明确确认。
fn inspect_partial(identity: &ArtifactIdentity) -> Result<ArtifactIdentity> {
    let path = identity
        .path
        .as_ref()
        .ok_or_else(|| AppError::invalid("资源未登记路径"))?;
    let snapshot = datapier_core::execution::data_files::output_snapshot(path, true)?;
    if identity.file_id.is_none()
        || identity.file_id != snapshot.file_id
        || identity.canonical_path.as_ref() != Some(&snapshot.canonical_path)
    {
        return Err(AppError::new(
            "recovery_conflict",
            "未完成文件已被替换，不能重新登记",
        ));
    }
    let mut observed = identity.clone();
    observed.size = Some(snapshot.size);
    observed.sha256 = snapshot.sha256;
    Ok(observed)
}
fn cleanup_import(stage: &model::project::ProjectImportStage) -> Result<()> {
    for candidate in &stage.candidates {
        match std::fs::symlink_metadata(candidate) {
            Ok(_) => {
                return Err(AppError::new(
                    "recovery_conflict",
                    "导入候选目录仍存在，请先核对",
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let dir = PathBuf::from(&stage.stage_dir);
    let project_dir = dir.join("project");
    let mut files = BTreeSet::from([dir.join("import.json")]);
    let mut directories = BTreeSet::from([dir.clone()]);
    if project_dir.exists() {
        directories.insert(project_dir.clone());
    }
    for (path, digest) in &stage.digests {
        let file = project_dir.join(path);
        let mut parent = file.parent();
        while let Some(p) = parent {
            if p == dir {
                break;
            }
            match std::fs::symlink_metadata(p) {
                Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                    directories.insert(p.to_path_buf());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(_) => return Err(AppError::new("recovery_conflict", "导入目录已被替换")),
                Err(error) => return Err(error.into()),
            }
            parent = p.parent();
        }
        match current_digest(&project_dir, path)? {
            Some(actual) if actual == *digest => {
                files.insert(file);
            }
            None => {}
            Some(_) => return Err(AppError::new("recovery_conflict", "导入副本已变化")),
        }
    }
    let lock = project_dir.join(".datapier-lock");
    match std::fs::symlink_metadata(&lock) {
        Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() && meta.len() == 0 => {
            files.insert(lock);
        }
        Ok(_) => return Err(AppError::new("recovery_conflict", "stage 锁文件已被替换")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    for directory in &directories {
        let meta = std::fs::symlink_metadata(directory)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(AppError::new("recovery_conflict", "stage 目录已被替换"));
        }
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)?;
            if meta.file_type().is_symlink()
                || (!files.contains(&path) && !directories.contains(&path))
                || (!meta.is_file() && !meta.is_dir())
            {
                return Err(AppError::new("recovery_conflict", "stage 含未知或替换文件"));
            }
        }
    }
    for file in files {
        std::fs::remove_file(file)?;
    }
    let mut directories: Vec<_> = directories.into_iter().collect();
    directories.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for directory in directories {
        std::fs::remove_dir(directory)?;
    }
    Ok(())
}
fn stage_root(engine: &Engine) -> PathBuf {
    engine.store.root.join("project-stages")
}
async fn ensure_stage_root(root: PathBuf) -> Result<PathBuf> {
    blocking(move || {
        std::fs::create_dir_all(&root)?;
        let meta = std::fs::symlink_metadata(&root)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(AppError::invalid("项目 staging 目录无效"));
        }
        Ok(std::fs::canonicalize(root)?)
    })
    .await
}
fn registered_stage(artifact: &TransferArtifact, root: &Path) -> Result<Uuid> {
    if artifact.kind != "project_stage" {
        return Err(AppError::invalid("资源不是项目 stage"));
    }
    let path = artifact
        .identity
        .path
        .as_ref()
        .ok_or_else(|| AppError::invalid("项目 stage 缺少路径"))?;
    if path.parent() != Some(root) {
        return Err(AppError::invalid("项目 stage 路径与登记不一致"));
    }
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| AppError::invalid("项目 stage 路径无效"))?;
    if artifact.identity.file_id.as_deref() != Some(name) {
        return Err(AppError::invalid("项目 stage 身份不一致"));
    }
    if let Some(expected) = &artifact.identity.stage_dir_identity {
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
            Ok(_) => {
                let actual = binding_root_identity(
                    path.to_str()
                        .ok_or_else(|| AppError::invalid("stage 路径不是 UTF-8"))?,
                )?;
                if &actual != expected {
                    return Err(AppError::new("recovery_conflict", "stage 目录身份已变化"));
                }
            }
        }
    }
    Uuid::parse_str(name).map_err(|_| AppError::invalid("项目 stage ID 无效"))
}
fn owned(artifact: &TransferArtifact, owner: Uuid, revision: u64) -> Result<()> {
    if artifact.owner_client_id != Some(owner) || artifact.revision != revision {
        return Err(AppError::new("conflict", "资源所有者或版本已变化"));
    }
    Ok(())
}

impl Engine {
    fn artifact_running(&self, artifact: &TransferArtifact) -> bool {
        let state = self.state.lock().unwrap();
        [artifact.run_id, artifact.identity.task_id]
            .into_iter()
            .flatten()
            .any(|id| state.tasks.contains_key(&id))
    }

    pub(super) fn project_binding_gate(
        &self,
        project_id: Uuid,
        group: &str,
    ) -> Arc<AsyncMutex<()>> {
        self.state
            .lock()
            .unwrap()
            .projects
            .bindings
            .entry((project_id, group.to_owned()))
            .or_default()
            .clone()
    }
    fn artifact_gate(&self, id: Uuid) -> Arc<AsyncMutex<()>> {
        let mut state = self.state.lock().unwrap();
        state
            .projects
            .artifacts
            .retain(|_, gate| Arc::strong_count(gate) > 1);
        state.projects.artifacts.entry(id).or_default().clone()
    }
    fn project_slot(
        &self,
        owner: Uuid,
        workspace: &str,
        id: Uuid,
    ) -> Result<Arc<AsyncMutex<ProjectSlot>>> {
        self.state
            .lock()
            .unwrap()
            .projects
            .projects
            .get(&(owner, workspace.to_owned(), id))
            .cloned()
            .ok_or_else(|| AppError::new("not_found", "当前客户端未打开项目"))
    }
    fn reserve_project(&self, owner: Uuid, bytes: usize) -> Result<Arc<Lease>> {
        let mut state = self.state.lock().unwrap();
        if !state.clients.contains(&owner) {
            return Err(AppError::new("disconnected", "客户端会话已关闭"));
        }
        state.projects.reserve(owner, bytes)
    }
    async fn register_project_stage(
        &self,
        owner: Uuid,
        path: &str,
        project_root: &str,
        stage_id: Uuid,
        kind: &str,
        digest: &str,
    ) -> Result<TransferArtifact> {
        let identity = ArtifactIdentity {
            path: Some(path.into()),
            canonical_path: Some(project_root.into()),
            file_id: Some(stage_id.to_string()),
            kind: Some(kind.into()),
            stage_dir_identity: Some(binding_root_identity(path)?),
            ownership_digest: Some(digest.into()),
            ..Default::default()
        };
        let id = self
            .store
            .transfer_register_artifact(
                owner,
                None,
                "project_stage",
                &serde_json::to_value(identity)?,
            )
            .await?;
        let revision = self
            .store
            .transfer_update_artifact(id, owner, Some(1), "ready", None)
            .await?;
        let result = self.store.transfer_artifact(id).await?;
        if result.revision != revision {
            return Err(AppError::new("conflict", "stage 登记版本变化"));
        }
        Ok(result)
    }
    async fn begin_project_stage(
        &self,
        owner: Uuid,
        root: &Path,
        project_root: &str,
        kind: &str,
        digest: Option<&str>,
    ) -> Result<(Uuid, TransferArtifact)> {
        let stage_id = Uuid::new_v4();
        let path = root.join(stage_id.to_string());
        let mut identity = ArtifactIdentity {
            path: Some(path.clone()),
            canonical_path: Some(project_root.into()),
            file_id: Some(stage_id.to_string()),
            kind: Some(format!("{kind}_preparing")),
            ownership_digest: digest.map(str::to_owned),
            ..Default::default()
        };
        // SQLite 先记录将创建的名字；没有目录身份的意图不能据此删除磁盘内容。
        let artifact_id = self
            .store
            .transfer_register_artifact(
                owner,
                None,
                "project_stage",
                &serde_json::to_value(&identity)?,
            )
            .await?;
        let stage_path = path.clone();
        #[cfg(unix)]
        let parent = root.to_path_buf();
        let stage_identity = blocking(move || {
            std::fs::create_dir(&stage_path)?;
            #[cfg(unix)]
            std::fs::File::open(&parent)?.sync_all()?;
            binding_root_identity(
                stage_path
                    .to_str()
                    .ok_or_else(|| AppError::invalid("stage 路径不是 UTF-8"))?,
            )
        })
        .await?;
        identity.stage_dir_identity = Some(stage_identity);
        self.store
            .transfer_update_artifact(
                artifact_id,
                owner,
                Some(1),
                "preparing",
                Some(&serde_json::to_value(identity)?),
            )
            .await?;
        Ok((stage_id, self.store.transfer_artifact(artifact_id).await?))
    }
    async fn project_bindings_all(
        &self,
        project_id: Uuid,
        group: &str,
    ) -> Result<Vec<ProjectBinding>> {
        let mut bindings = Vec::new();
        let mut offset = 0;
        loop {
            let page = self
                .store
                .project_bindings(project_id, group, offset, 100)
                .await?;
            if page.is_empty() {
                break;
            }
            offset += page.len() as u64;
            bindings.extend(page);
        }
        Ok(bindings)
    }
    async fn project_binding_digest(&self, project_id: Uuid, group: &str) -> Result<String> {
        Ok(digest_bytes(&serde_json::to_vec(
            &self.project_bindings_all(project_id, group).await?,
        )?))
    }
    async fn confirmed_project_bindings(
        &self,
        snapshot: &ProjectSlot,
        project_id: Uuid,
        group: &str,
    ) -> Result<Vec<ProjectBinding>> {
        if snapshot.restored_baseline {
            return Err(AppError::new(
                "project_conflict",
                "请先核对并接受磁盘项目，恢复基线不能授权执行",
            ));
        }
        let bindings = self.project_bindings_all(project_id, group).await?;
        let digest = digest_bytes(&serde_json::to_vec(&bindings)?);
        if !self
            .store
            .project_binding_confirmed(
                project_id,
                &binding_root_identity(&snapshot.root)?,
                group,
                &digest,
            )
            .await?
        {
            return Err(AppError::new(
                "binding_required",
                "请先明确确认此目录的本机绑定",
            ));
        }
        Ok(bindings)
    }
    /// 在本机绑定组中解析共享 alias 和路径参数，修改仅属于本次运行内存。
    pub(super) async fn resolve_project_spec(
        &self,
        owner: Uuid,
        project_id: Uuid,
        binding_group: &str,
        spec: &mut model::transfer::TransferSpec,
    ) -> Result<()> {
        self.active(owner)?;
        let slots: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .projects
            .projects
            .iter()
            .filter(|((client, _, id), _)| *client == owner && *id == project_id)
            .map(|(_, slot)| slot.clone())
            .collect();
        if slots.len() != 1 {
            return Err(AppError::new(
                "binding_required",
                "项目未打开或在多个工作区中有歧义",
            ));
        }
        let snapshot = slots[0].lock().await;
        let bindings = self
            .confirmed_project_bindings(&snapshot, project_id, binding_group)
            .await?;
        let profiles = self.store.profiles().await?;
        let mut paths = BTreeMap::<String, PathBuf>::new();
        for (is_target, endpoint) in [(false, &mut spec.source), (true, &mut spec.destination)] {
            let Some(endpoint) = endpoint else { continue };
            let Some(alias) = endpoint.connection_alias.as_deref() else {
                continue;
            };
            let logical = snapshot
                .manifest
                .connections
                .iter()
                .find(|c| c.alias == alias)
                .ok_or_else(|| AppError::new("binding_required", "项目中不存在连接别名"))?;
            let binding = bindings
                .iter()
                .find(|b| b.connection_alias == alias)
                .ok_or_else(|| {
                    AppError::new("binding_required", format!("连接 {alias} 尚未绑定"))
                })?;
            let previously_resolved = endpoint.profile_id.is_some();
            if endpoint
                .profile_id
                .is_some_and(|id| id != binding.profile_id)
            {
                return Err(AppError::new("conflict", "连接绑定已改变，请重新准备"));
            }
            if previously_resolved
                && ((binding.overrides.database.is_some()
                    && endpoint.database != binding.overrides.database)
                    || (binding.overrides.redis_db.is_some()
                        && endpoint.redis_db != binding.overrides.redis_db))
            {
                return Err(AppError::new("conflict", "数据库绑定已改变，请重新准备"));
            }
            let profile = profiles
                .iter()
                .find(|p| p.id == binding.profile_id)
                .ok_or_else(|| AppError::new("binding_required", "绑定的连接已删除"))?;
            let provider = match profile.provider {
                model::ProviderKind::Mysql => "mysql",
                model::ProviderKind::Redis => "redis",
            };
            if profile.provider != spec.provider
                || logical.provider != provider
                || (logical.source_kind == "archery") != profile.is_archery()
            {
                return Err(AppError::invalid("绑定的数据库类型或来源已变化"));
            }
            if is_target && profile.is_archery() {
                return Err(AppError::new(
                    "permission_denied",
                    "Archery 不能作为迁移写入目标",
                ));
            }
            endpoint.profile_id = Some(binding.profile_id);
            if binding.overrides.database.is_some() {
                endpoint.database = binding.overrides.database.clone();
            }
            if binding.overrides.redis_db.is_some() {
                endpoint.redis_db = binding.overrides.redis_db;
            }
            for (name, path) in &binding.overrides.paths {
                if let Some(old) = paths.insert(name.clone(), path.clone())
                    && old != *path
                {
                    return Err(AppError::invalid("不同连接的同名路径参数不一致"));
                }
            }
        }
        // 共享配置中的空库名由本机绑定填充，避免把实际库名写进项目。
        for object in &mut spec.objects {
            if object.source.database.as_deref().is_none_or(str::is_empty) {
                object
                    .source
                    .database
                    .clone_from(&spec.source.as_ref().and_then(|e| e.database.clone()));
            }
            if object.target.database.as_deref().is_none_or(str::is_empty) {
                object
                    .target
                    .database
                    .clone_from(&spec.destination.as_ref().and_then(|e| e.database.clone()));
            }
        }
        for input in &mut spec.inputs {
            if let Some(target) = &mut input.target
                && target.database.as_deref().is_none_or(str::is_empty)
            {
                target
                    .database
                    .clone_from(&spec.destination.as_ref().and_then(|e| e.database.clone()));
            }
        }
        let root = PathBuf::from(&snapshot.root);
        for path in spec
            .inputs
            .iter_mut()
            .map(|i| &mut i.path)
            .chain(spec.output.iter_mut().map(|o| &mut o.path))
        {
            let text = path
                .to_str()
                .ok_or_else(|| AppError::invalid("项目路径必须是 UTF-8"))?;
            if text.starts_with("${") && text.ends_with('}') {
                let name = &text[2..text.len() - 1];
                *path = paths.get(name).cloned().ok_or_else(|| {
                    AppError::new("binding_required", format!("路径参数 {name} 尚未绑定"))
                })?;
            } else if path.is_absolute() {
                if !paths
                    .values()
                    .any(|bound| bound.as_path() == path.as_path())
                {
                    let relative = path
                        .strip_prefix(&root)
                        .map_err(|_| {
                            AppError::new("binding_required", "绝对路径不属于当前项目或路径绑定")
                        })?
                        .to_path_buf();
                    let root = root.clone();
                    *path = blocking(move || resolve_relative_path(&root, &relative)).await?;
                }
            } else {
                let relative = path.clone();
                let root = root.clone();
                *path = blocking(move || resolve_relative_path(&root, &relative)).await?;
            }
        }
        Ok(())
    }
    pub(super) async fn project_rpc(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.active(owner)?;
        let ws = workspace(&params).to_owned();
        match method {
            "project.create" | "project.open" => {
                let path: PathBuf = model::param(&params, "path")?;
                let name: Option<String> = optional(&params, "name")?;
                let create = method == "project.create";
                let baseline_manifest: Option<ProjectManifest> =
                    optional(&params, "baseline_manifest")?;
                let baseline_digests: Option<BTreeMap<String, String>> =
                    optional(&params, "baseline_digests")?;
                if baseline_manifest.is_some() != baseline_digests.is_some()
                    || (create && baseline_manifest.is_some())
                {
                    return Err(AppError::invalid("恢复项目必须同时提供基线清单和摘要"));
                }
                let restored_baseline = baseline_manifest.is_some();
                let lease = self.reserve_project(owner, PROJECT_BYTES)?;
                let worker_lease = lease.clone();
                let snapshot = blocking(move || {
                    let _lease = worker_lease;
                    if create {
                        project::create(
                            &path,
                            name.ok_or_else(|| AppError::invalid("缺少项目名称"))?,
                        )
                    } else if let (Some(manifest), Some(digests)) =
                        (baseline_manifest, baseline_digests)
                    {
                        project::restore_baseline(&path, manifest, digests)
                    } else {
                        project::open(&path)
                    }
                })
                .await?;
                let memory = Arc::new(
                    super::transfer::MemoryReservation::reserve(
                        &self.store,
                        owner,
                        datapier_core::execution::migration::encoded_size(&snapshot)?,
                    )
                    .await?,
                );
                let response = snapshot_json(&snapshot);
                let id = snapshot.manifest.id;
                let mut state = self.state.lock().unwrap();
                if !state.clients.contains(&owner) {
                    return Err(AppError::new("disconnected", "客户端会话已关闭"));
                }
                let key = (owner, ws, id);
                if state.projects.projects.contains_key(&key) {
                    return Err(AppError::new(
                        "busy",
                        "项目已打开，请先关闭，避免丢失未保存内容",
                    ));
                }
                state.projects.projects.insert(
                    key,
                    Arc::new(AsyncMutex::new(ProjectSlot {
                        snapshot: Arc::new(ChargedSnapshot {
                            snapshot,
                            _memory: memory,
                        }),
                        restored_baseline,
                        _lease: lease,
                    })),
                );
                Ok(response)
            }
            "project.close" => {
                let id: Uuid = model::param(&params, "project_id")?;
                let discard: bool = optional(&params, "discard_previews")?.unwrap_or(false);
                let mut state = self.state.lock().unwrap();
                state.projects.prune();
                let key = (owner, ws.clone(), id);
                let slot = state
                    .projects
                    .projects
                    .get(&key)
                    .ok_or_else(|| AppError::new("not_found", "当前客户端未打开项目"))?;
                let plans: Vec<_> = state
                    .projects
                    .plans
                    .iter()
                    .filter(|(_, p)| p.owner == owner && p.workspace == ws && p.project_id == id)
                    .map(|(id, p)| (*id, Arc::strong_count(&p.slot)))
                    .collect();
                if !plans.is_empty() && !discard {
                    return Err(AppError::new(
                        "unsaved_changes",
                        "项目有未提交的保存预览；请先保存或明确丢弃",
                    ));
                }
                if plans.iter().any(|(_, count)| *count != 1)
                    || Arc::strong_count(slot) != 1 + plans.len()
                {
                    return Err(AppError::new("busy", "项目正在使用"));
                }
                let retired: Vec<_> = plans
                    .into_iter()
                    .filter_map(|(id, _)| state.projects.plans.remove(&id))
                    .collect();
                state.projects.uploads.retain(|_, upload| {
                    !(upload.owner == owner && upload.workspace == ws && upload.project_id == id)
                });
                let project = state.projects.projects.remove(&key);
                drop(state);
                defer_drop((retired, project));
                Ok(json!({"closed":true}))
            }
            "project.save.discard" => {
                let id: Uuid = model::param(&params, "plan_id")?;
                let mut state = self.state.lock().unwrap();
                state.projects.prune();
                let entry = state
                    .projects
                    .plans
                    .get(&id)
                    .ok_or_else(|| AppError::new("not_found", "保存计划不存在或已过期"))?;
                if entry.owner != owner || entry.workspace != ws {
                    return Err(AppError::new(
                        "permission_denied",
                        "保存计划不属于当前客户端",
                    ));
                }
                if Arc::strong_count(&entry.slot) != 1 {
                    return Err(AppError::new("busy", "保存计划正在使用"));
                }
                let retired = state.projects.plans.remove(&id);
                drop(state);
                defer_drop(retired);
                Ok(json!({"discarded":true}))
            }
            "project.external.read" | "project.external.accept" => {
                let id: Uuid = model::param(&params, "project_id")?;
                let slot = self.project_slot(owner, &ws, id)?;
                let mut current = slot.lock().await;
                let root = current.root.clone();
                let baseline = current.snapshot.clone();
                let external = blocking(move || {
                    project::open_external(std::path::Path::new(&root), &baseline)
                })
                .await?;
                if external.manifest.id != id {
                    return Err(AppError::new("project_conflict", "外部项目 ID 已改变"));
                }
                if method == "project.external.read" {
                    if let Some(path) = optional::<String>(&params, "path")? {
                        let offset = optional(&params, "offset")?.unwrap_or(0);
                        let limit = optional(&params, "limit")?.unwrap_or(64 * 1024);
                        if !external.files.contains_key(&path)
                            && current.digests.contains_key(&path)
                        {
                            return Ok(
                                json!({"text":null,"next":null,"deleted":true,"digests":external.digests}),
                            );
                        }
                        let (text, next) =
                            project::read_file_chunk(&external, &path, offset, limit)?;
                        return Ok(
                            json!({"text":text,"next":next,"deleted":false,"digests":external.digests}),
                        );
                    }
                    return Ok(snapshot_json(&external));
                }
                let expected: BTreeMap<String, String> = model::param(&params, "digests")?;
                if external.digests != expected {
                    return Err(AppError::new(
                        "project_conflict",
                        "外部项目在查看后再次改变，请重新查看",
                    ));
                }
                let memory = Arc::new(
                    super::transfer::MemoryReservation::reserve(
                        &self.store,
                        owner,
                        datapier_core::execution::migration::encoded_size(&external)?,
                    )
                    .await?,
                );
                let response = snapshot_json(&external);
                current.snapshot = Arc::new(ChargedSnapshot {
                    snapshot: external,
                    _memory: memory,
                });
                current.restored_baseline = false;
                Ok(response)
            }
            "project.file.read" => {
                let id: Uuid = model::param(&params, "project_id")?;
                let path: String = model::param(&params, "path")?;
                let offset: usize = optional(&params, "offset")?.unwrap_or(0);
                let limit: usize = optional(&params, "limit")?.unwrap_or(64 * 1024);
                let slot = self.project_slot(owner, &ws, id)?;
                let snapshot = slot.lock().await;
                let (text, next) = project::read_file_chunk(&snapshot, &path, offset, limit)?;
                Ok(json!({"text":text,"next":next}))
            }
            "project.save.upload.begin"
            | "project.save.upload.chunk"
            | "project.save.upload.cancel"
            | "project.save.prepare"
            | "project.save.commit" => self.project_save_rpc(owner, method, params, ws).await,
            "project.query.resolve" => {
                let id: Uuid = model::param(&params, "project_id")?;
                let query_id: Uuid = model::param(&params, "query_id")?;
                let group: String = model::param(&params, "binding_group")?;
                let expected_root: String = model::param(&params, "expected_root")?;
                let gate = self.project_binding_gate(id, &group);
                let _guard = gate.lock().await;
                let slot = self.project_slot(owner, &ws, id)?;
                let snapshot = slot.lock().await;
                if snapshot.root != expected_root {
                    return Err(AppError::new(
                        "project_conflict",
                        "项目目录已改变，请重新打开查询",
                    ));
                }
                let bindings = self
                    .confirmed_project_bindings(&snapshot, id, &group)
                    .await?;
                let query = snapshot
                    .manifest
                    .queries
                    .iter()
                    .find(|query| query.id == query_id)
                    .ok_or_else(|| AppError::new("not_found", "项目查询不存在"))?;
                let logical = snapshot
                    .manifest
                    .connections
                    .iter()
                    .find(|connection| connection.alias == query.connection_alias)
                    .ok_or_else(|| AppError::new("binding_required", "项目查询连接别名不存在"))?;
                let binding = bindings
                    .iter()
                    .find(|binding| binding.connection_alias == logical.alias)
                    .ok_or_else(|| AppError::new("binding_required", "项目查询连接尚未绑定"))?;
                let profile = self.store.profile(binding.profile_id).await?;
                let provider = match profile.provider {
                    model::ProviderKind::Mysql => "mysql",
                    model::ProviderKind::Redis => "redis",
                };
                if logical.provider != provider
                    || (logical.source_kind == "archery") != profile.is_archery()
                    || (provider == "mysql" && query.language != "sql")
                    || (provider == "redis" && query.language != "redis")
                {
                    return Err(AppError::new(
                        "binding_required",
                        "查询语言、数据库类型或来源与绑定不一致",
                    ));
                }
                let logical_database = query
                    .database
                    .as_ref()
                    .filter(|value| !value.is_null())
                    .or_else(|| logical.database.as_ref().filter(|value| !value.is_null()));
                let mut database = binding.overrides.database.clone();
                let mut redis_db = binding.overrides.redis_db;
                if provider == "mysql" && database.is_none() {
                    database = logical_database.map(|value| match value {
                        Value::String(value) => value.clone(),
                        other => other.to_string(),
                    });
                } else if provider == "redis" && redis_db.is_none() {
                    redis_db = logical_database
                        .map(|value| {
                            let text = value
                                .as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| value.to_string());
                            text.parse::<u32>()
                                .map_err(|_| AppError::invalid("Redis 逻辑数据库编号无效"))
                        })
                        .transpose()?;
                }
                Ok(
                    json!({"root":snapshot.root,"query":query,"profile_id":profile.id,"database":database,"redis_db":redis_db,"binding_group":group}),
                )
            }
            "project.bindings.get" => {
                let id: Uuid = model::param(&params, "project_id")?;
                let group: String = model::param(&params, "binding_group")?;
                let gate = self.project_binding_gate(id, &group);
                let _guard = gate.lock().await;
                let slot = self.project_slot(owner, &ws, id)?;
                let root = binding_root_identity(&slot.lock().await.root)?;
                let binding_digest = self.project_binding_digest(id, &group).await?;
                let confirmed = self
                    .store
                    .project_binding_confirmed(id, &root, &group, &binding_digest)
                    .await?;
                let offset = optional(&params, "offset")?.unwrap_or(0u64);
                let limit = optional(&params, "limit")?.unwrap_or(100u64);
                let bindings = self
                    .store
                    .project_bindings(id, &group, offset, limit)
                    .await?;
                let next_offset = offset + bindings.len() as u64;
                let next = if !bindings.is_empty()
                    && !self
                        .store
                        .project_bindings(id, &group, next_offset, 1)
                        .await?
                        .is_empty()
                {
                    Some(next_offset)
                } else {
                    None
                };
                Ok(
                    json!({"bindings":bindings,"next":next,"confirmed":confirmed,"binding_digest":binding_digest}),
                )
            }
            "project.bindings.confirm" => {
                let id: Uuid = model::param(&params, "project_id")?;
                let group: String = model::param(&params, "binding_group")?;
                let expected: String = model::param(&params, "binding_digest")?;
                let gate = self.project_binding_gate(id, &group);
                let _guard = gate.lock().await;
                let slot = self.project_slot(owner, &ws, id)?;
                let snapshot = slot.lock().await;
                if snapshot.restored_baseline {
                    return Err(AppError::new("project_conflict", "请先核对并接受外部项目"));
                }
                let root = binding_root_identity(&snapshot.root)?;
                let digest = self.project_binding_digest(id, &group).await?;
                if expected != digest {
                    return Err(AppError::new("conflict", "绑定已改变，请重新查看"));
                }
                self.store
                    .project_confirm_binding(id, &root, &group, &digest)
                    .await?;
                Ok(json!({"confirmed":true,"binding_digest":digest}))
            }
            "project.bindings.save" => {
                let id: Uuid = model::param(&params, "project_id")?;
                let group: String = model::param(&params, "binding_group")?;
                let gate = self.project_binding_gate(id, &group);
                let _binding_guard = gate.lock().await;
                let inputs: Vec<BindingInput> = model::param(&params, "bindings")?;
                let slot = self.project_slot(owner, &ws, id)?;
                let snapshot = slot.lock().await;
                if snapshot.restored_baseline {
                    return Err(AppError::new("project_conflict", "请先核对并接受外部项目"));
                }
                let profiles = self.store.profiles().await?;
                let mut seen = BTreeSet::new();
                let mut bindings = Vec::new();
                for input in inputs {
                    if !seen.insert(input.connection_alias.clone()) {
                        return Err(AppError::invalid("绑定别名重复"));
                    }
                    let logical = snapshot
                        .manifest
                        .connections
                        .iter()
                        .find(|c| c.alias == input.connection_alias)
                        .ok_or_else(|| AppError::invalid("绑定引用未知连接别名"))?;
                    let profile = profiles
                        .iter()
                        .find(|p| p.id == input.profile_id)
                        .ok_or_else(|| AppError::invalid("绑定 profile 不存在"))?;
                    let provider = match profile.provider {
                        model::ProviderKind::Mysql => "mysql",
                        model::ProviderKind::Redis => "redis",
                    };
                    if logical.provider != provider
                        || (logical.source_kind == "archery") != profile.is_archery()
                    {
                        return Err(AppError::invalid("绑定的数据库类型或来源与逻辑连接不一致"));
                    }
                    bindings.push(ProjectBinding {
                        project_id: id,
                        binding_group: group.clone(),
                        connection_alias: input.connection_alias,
                        profile_id: input.profile_id,
                        overrides: BindingOverrides {
                            database: input.database,
                            redis_db: input.redis_db,
                            paths: input.paths,
                        },
                        revision: input.revision,
                    });
                }
                let bindings = self.store.project_save_bindings(&bindings).await?;
                Ok(json!({"bindings":bindings}))
            }
            "project.export"
            | "project.import"
            | "project.stages.discover"
            | "transfer.artifact.list"
            | "transfer.artifact.get"
            | "transfer.artifact.claim"
            | "project.recovery.read"
            | "project.recovery.prepare"
            | "project.recovery.commit"
            | "transfer.artifact.export"
            | "transfer.artifact.cleanup" => {
                self.project_artifact_rpc(owner, method, params, ws).await
            }
            _ => Err(AppError::new("method_not_found", "未知项目方法")),
        }
    }
}

#[cfg(all(test, windows))]
#[path = "../../tests/unit/server/project_identity.rs"]
mod windows_identity_tests;
