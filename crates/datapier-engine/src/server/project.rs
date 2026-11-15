//! 项目文件与可领取遗留资源的 Engine 会话入口。
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
fn change_summaries(preview: &ProjectSavePreview) -> Vec<Value> {
    preview.changes.iter().map(|c| json!({"path":c.path,"old_digest":c.old_digest,"new_digest":c.new_digest,"old_bytes":c.old_content.as_ref().map(String::len),"new_bytes":c.new_content.as_ref().map(String::len)})).collect()
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
fn binding_root_identity(root: &str) -> Result<String> {
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
            "project.save.upload.begin" => {
                let project_id: Uuid = model::param(&params, "project_id")?;
                let slot = self.project_slot(owner, &ws, project_id)?;
                let baseline = slot.lock().await.digests.clone();
                let lease = self.reserve_project(owner, WORK_BYTES)?;
                let upload_id = Uuid::new_v4();
                let mut state = self.state.lock().unwrap();
                if !state.clients.contains(&owner)
                    || !state
                        .projects
                        .projects
                        .get(&(owner, ws.clone(), project_id))
                        .is_some_and(|current| Arc::ptr_eq(current, &slot))
                {
                    return Err(AppError::new("disconnected", "项目已关闭"));
                }
                state.projects.uploads.insert(
                    upload_id,
                    Upload {
                        owner,
                        workspace: ws,
                        project_id,
                        baseline,
                        revision: 1,
                        created: Instant::now(),
                        files: BTreeMap::new(),
                        bytes: 0,
                        _lease: lease,
                    },
                );
                Ok(json!({"upload_id":upload_id,"revision":1}))
            }
            "project.save.upload.chunk" | "project.save.upload.cancel" => {
                let upload_id: Uuid = model::param(&params, "upload_id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let mut state = self.state.lock().unwrap();
                state.projects.prune();
                let upload = state
                    .projects
                    .uploads
                    .get_mut(&upload_id)
                    .ok_or_else(|| AppError::new("not_found", "上传不存在或已过期"))?;
                if upload.owner != owner || upload.workspace != ws || upload.revision != revision {
                    return Err(AppError::new("conflict", "上传所有者、工作区或版本不匹配"));
                }
                if method == "project.save.upload.cancel" {
                    state.projects.uploads.remove(&upload_id);
                    return Ok(json!({"cancelled":true}));
                }
                let path: String = model::param(&params, "path")?;
                let offset: usize = model::param(&params, "offset")?;
                let text: String = model::param(&params, "text")?;
                if path.is_empty()
                    || path.len() > 4096
                    || text.len() > 256 * 1024
                    || upload.bytes.saturating_add(text.len()) > 64 * MIB
                {
                    return Err(AppError::new("resource_limit", "上传分段或项目大小超限"));
                }
                if !upload.files.contains_key(&path) && (upload.files.len() >= 9_999 || offset != 0)
                {
                    return Err(AppError::new("resource_limit", "上传文件数或偏移无效"));
                }
                let file = upload.files.entry(path).or_default();
                if file.len() != offset || file.len().saturating_add(text.len()) > 16 * MIB {
                    return Err(AppError::new("conflict", "上传偏移或文件大小无效"));
                }
                file.push_str(&text);
                upload.bytes += text.len();
                upload.revision = upload
                    .revision
                    .checked_add(1)
                    .ok_or_else(|| AppError::invalid("上传版本溢出"))?;
                upload.created = Instant::now();
                Ok(json!({"revision":upload.revision,"offset":file.len()}))
            }
            "project.save.prepare" => {
                let id: Uuid = model::param(&params, "project_id")?;
                if self
                    .project_slot(owner, &ws, id)?
                    .lock()
                    .await
                    .restored_baseline
                {
                    return Err(AppError::new("project_conflict", "请先核对并接受外部项目"));
                }
                let manifest: ProjectManifest = model::param(&params, "manifest")?;
                let upload_id: Option<Uuid> = optional(&params, "upload_id")?;
                let (patch, upload_lease): (BTreeMap<String, String>, Option<Arc<Lease>>) =
                    if let Some(upload_id) = upload_id {
                        let revision: u64 = model::param(&params, "revision")?;
                        let slot = self.project_slot(owner, &ws, id)?;
                        let snapshot = slot.lock().await;
                        let mut state = self.state.lock().unwrap();
                        state.projects.prune();
                        let upload = state
                            .projects
                            .uploads
                            .get(&upload_id)
                            .ok_or_else(|| AppError::new("not_found", "上传不存在或已过期"))?;
                        if upload.owner != owner
                            || upload.workspace != ws
                            || upload.project_id != id
                            || upload.revision != revision
                            || upload.baseline != snapshot.digests
                        {
                            return Err(AppError::new(
                                "conflict",
                                "上传所有者、版本或项目基线已变化",
                            ));
                        }
                        let Upload { files, _lease, .. } =
                            state.projects.uploads.remove(&upload_id).unwrap();
                        (files, Some(_lease))
                    } else {
                        (model::param(&params, "files")?, None)
                    };
                let replace: Option<Uuid> = optional(&params, "replace_plan_id")?;
                if let Some(previous) = replace {
                    let retired = {
                        let mut state = self.state.lock().unwrap();
                        let entry =
                            state.projects.plans.get(&previous).ok_or_else(|| {
                                AppError::new("not_found", "待替换的保存预览不存在")
                            })?;
                        if entry.owner != owner
                            || entry.workspace != ws
                            || entry.project_id != id
                            || Arc::strong_count(&entry.slot) != 1
                        {
                            return Err(AppError::new("busy", "保存预览不属于此项目或正在使用"));
                        }
                        state.projects.plans.remove(&previous)
                    };
                    blocking(move || {
                        drop(retired);
                        Ok(())
                    })
                    .await?;
                }
                let lease = self.reserve_project(owner, PLAN_BYTES)?;
                drop(upload_lease);
                let slot = self.project_slot(owner, &ws, id)?;
                let snapshot = slot.lock().await.snapshot.clone();
                let included: BTreeSet<_> = manifest
                    .queries
                    .iter()
                    .map(|q| q.path.clone())
                    .chain(manifest.transfers.iter().map(|t| t.path.clone()))
                    .collect();
                if patch.keys().any(|path| !included.contains(path)) {
                    return Err(AppError::invalid("保存内容包含未被新清单引用的文件"));
                }
                let mut files = snapshot.files.clone();
                files.extend(patch);
                files.retain(|path, _| included.contains(path));
                // 同时预留预览和未来快照，避免文件已发布后才发现没有读回额度。
                let future_bytes =
                    datapier_core::execution::migration::encoded_size(&(&manifest, &files))?
                        .checked_add(files.keys().map(|path| path.len() + 96).sum::<usize>())
                        .and_then(|bytes| bytes.checked_add(snapshot.root.len() + 8192))
                        .ok_or_else(|| AppError::new("resource_limit", "项目快照额度溢出"))?;
                let worker_lease = lease.clone();
                let preview = blocking(move || {
                    let _lease = worker_lease;
                    project::preview(&snapshot, manifest, files)
                })
                .await?;
                let retained = datapier_core::execution::migration::encoded_size(&preview)?
                    .checked_add(future_bytes)
                    .ok_or_else(|| AppError::new("resource_limit", "项目预览额度溢出"))?;
                let memory = Arc::new(
                    super::transfer::MemoryReservation::reserve(&self.store, owner, retained)
                        .await?,
                );
                let plan_id = Uuid::new_v4();
                let response = json!({"plan_id":plan_id,"revision":1,"digest":preview.digest,"changes":change_summaries(&preview)});
                let mut state = self.state.lock().unwrap();
                if !state.clients.contains(&owner)
                    || !state
                        .projects
                        .projects
                        .get(&(owner, ws.clone(), id))
                        .is_some_and(|current| Arc::ptr_eq(current, &slot))
                {
                    return Err(AppError::new("disconnected", "项目已关闭或客户端已断开"));
                }
                let plan = SavePlan {
                    owner,
                    workspace: ws.clone(),
                    snapshot: slot,
                    preview,
                    used: false,
                    _lease: lease,
                    memory,
                };
                state.projects.plans.insert(
                    plan_id,
                    PlanEntry {
                        owner,
                        workspace: ws,
                        project_id: id,
                        created: Instant::now(),
                        slot: Arc::new(AsyncMutex::new(plan)),
                    },
                );
                Ok(response)
            }
            "project.save.commit" => {
                let id: Uuid = model::param(&params, "plan_id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let digest: String = model::param(&params, "digest")?;
                let plan_slot = {
                    let mut state = self.state.lock().unwrap();
                    state.projects.prune();
                    state
                        .projects
                        .plans
                        .get(&id)
                        .map(|entry| entry.slot.clone())
                        .ok_or_else(|| AppError::new("not_found", "保存计划不存在或已过期"))?
                };
                let mut plan = plan_slot.lock().await;
                if plan.owner != owner
                    || plan.workspace != ws
                    || plan.used
                    || revision != 1
                    || digest != plan.preview.digest
                {
                    return Err(AppError::new(
                        "conflict",
                        "保存计划所有者、版本或摘要不匹配",
                    ));
                }
                {
                    let mut state = self.state.lock().unwrap();
                    if !state.clients.contains(&owner)
                        || !state
                            .projects
                            .plans
                            .get(&id)
                            .is_some_and(|entry| Arc::ptr_eq(&entry.slot, &plan_slot))
                    {
                        return Err(AppError::new("conflict", "保存计划已失效"));
                    }
                    state.projects.plans.remove(&id);
                }
                plan.used = true;
                let snapshot_slot = plan.snapshot.clone();
                let mut snapshot = snapshot_slot.lock().await;
                if snapshot.manifest.id != plan.preview.project_id
                    || snapshot.root != plan.preview.root
                    || snapshot.digests != plan.preview.baseline
                {
                    return Err(AppError::new("conflict", "项目快照已变化"));
                }
                let stage_gate =
                    self.project_binding_gate(Uuid::nil(), "project-stage-registration");
                let _stage_guard = stage_gate.lock().await;
                let root = ensure_stage_root(stage_root(self)).await?;
                let (stage_id, preparing) = self
                    .begin_project_stage(owner, &root, &snapshot.root, "save", Some(&digest))
                    .await?;
                let preview = plan.preview.clone();
                let expected = digest.clone();
                let stage_root = root.clone();
                let worker_lease = plan._lease.clone();
                let stage = blocking(move || {
                    let _lease = worker_lease;
                    project::prepare_save_in(&preview, &expected, &stage_root, stage_id, true)
                })
                .await?;
                let mut ready_identity = preparing.identity.clone();
                ready_identity.kind = Some("save".into());
                self.store
                    .transfer_update_artifact(
                        preparing.id,
                        owner,
                        Some(preparing.revision),
                        "ready",
                        Some(&serde_json::to_value(ready_identity)?),
                    )
                    .await?;
                let artifact = self.store.transfer_artifact(preparing.id).await?;
                drop(_stage_guard);
                let gate = self.artifact_gate(artifact.id);
                let _guard = gate.lock().await;
                owned(
                    &self.store.transfer_artifact(artifact.id).await?,
                    owner,
                    artifact.revision,
                )?;
                let stage_id = stage.identity.id;
                let result = blocking_leased(plan._lease.clone(), move || {
                    project::commit(&root, stage_id)
                })
                .await;
                match result {
                    Ok(_) => {
                        self.store
                            .transfer_update_artifact(
                                artifact.id,
                                owner,
                                Some(artifact.revision),
                                "published",
                                None,
                            )
                            .await?;
                        let path = PathBuf::from(&snapshot.root);
                        let refreshed =
                            blocking_leased(plan._lease.clone(), move || project::open(&path))
                                .await?;
                        plan.memory
                            .resize(
                                owner,
                                datapier_core::execution::migration::encoded_size(&refreshed)?,
                            )
                            .await?;
                        snapshot.snapshot = Arc::new(ChargedSnapshot {
                            snapshot: refreshed,
                            _memory: plan.memory.clone(),
                        });
                        Ok(
                            json!({"project_id":snapshot.manifest.id,"state":"published","manifest":snapshot.manifest,"digests":snapshot.digests,"artifact_id":artifact.id}),
                        )
                    }
                    Err(error) => {
                        if error.code == "recovery_conflict" {
                            self.store
                                .transfer_update_artifact(
                                    artifact.id,
                                    owner,
                                    Some(artifact.revision),
                                    "recovery_conflict",
                                    None,
                                )
                                .await?;
                        }
                        Err(error)
                    }
                }
            }
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
            "project.export" => {
                let id: Uuid = model::param(&params, "project_id")?;
                let path: PathBuf = model::param(&params, "path")?;
                let slot = self.project_slot(owner, &ws, id)?;
                let lease = self.reserve_project(owner, WORK_BYTES)?;
                let snapshot = slot.lock().await.snapshot.clone();
                blocking(move || {
                    let _lease = lease;
                    project::export_archive(&snapshot, &path)
                })
                .await?;
                Ok(json!({"state":"published"}))
            }
            "project.import" => {
                let archive: PathBuf = model::param(&params, "path")?;
                let destination: PathBuf = model::param(&params, "destination")?;
                let project_lease = self.reserve_project(owner, PROJECT_BYTES)?;
                let work_lease = self.reserve_project(owner, WORK_BYTES)?;
                let stage_gate =
                    self.project_binding_gate(Uuid::nil(), "project-stage-registration");
                let _stage_guard = stage_gate.lock().await;
                let root = ensure_stage_root(stage_root(self)).await?;
                let (stage_id, preparing) = self
                    .begin_project_stage(
                        owner,
                        &root,
                        &destination.to_string_lossy(),
                        "import",
                        None,
                    )
                    .await?;
                let staging = root.clone();
                let worker_lease = work_lease.clone();
                let store = self.store.clone();
                let mut journal_identity = preparing.identity.clone();
                let artifact_id = preparing.id;
                let expected_revision = preparing.revision;
                let runtime = tokio::runtime::Handle::current();
                let stage = blocking(move || {
                    let _lease = worker_lease;
                    project::prepare_import_in(
                        &archive,
                        &destination,
                        &staging,
                        stage_id,
                        true,
                        |stage| {
                            let bytes = serde_json::to_vec(stage)?;
                            journal_identity.sha256 = Some(digest_bytes(&bytes));
                            journal_identity.canonical_path =
                                Some(stage.destination.clone().into());
                            journal_identity.ownership_digest =
                                stage.digests.get("datapier.project.json").cloned();
                            runtime.block_on(store.transfer_update_artifact(
                                artifact_id,
                                owner,
                                Some(expected_revision),
                                "preparing",
                                Some(&serde_json::to_value(journal_identity)?),
                            ))?;
                            Ok(())
                        },
                    )
                })
                .await?;
                let preparing = self.store.transfer_artifact(preparing.id).await?;
                let mut ready_identity = preparing.identity.clone();
                ready_identity.kind = Some("import".into());
                self.store
                    .transfer_update_artifact(
                        preparing.id,
                        owner,
                        Some(preparing.revision),
                        "ready",
                        Some(&serde_json::to_value(ready_identity)?),
                    )
                    .await?;
                let artifact = self.store.transfer_artifact(preparing.id).await?;
                drop(_stage_guard);
                if self.state.lock().unwrap().projects.projects.contains_key(&(
                    owner,
                    ws.clone(),
                    stage.project_id,
                )) {
                    return Err(AppError::new(
                        "busy",
                        "项目已打开；导入 stage 已保留，请先关闭项目",
                    ));
                }
                let gate = self.artifact_gate(artifact.id);
                let _guard = gate.lock().await;
                owned(
                    &self.store.transfer_artifact(artifact.id).await?,
                    owner,
                    artifact.revision,
                )?;
                let stage_id = stage.id;
                let worker_lease = work_lease.clone();
                let result = blocking(move || {
                    let _lease = worker_lease;
                    project::commit_import(&root, stage_id)
                })
                .await;
                match result {
                    Ok(snapshot) => {
                        self.store
                            .transfer_update_artifact(
                                artifact.id,
                                owner,
                                Some(artifact.revision),
                                "published",
                                None,
                            )
                            .await?;
                        let response = snapshot_json(&snapshot);
                        let memory = Arc::new(
                            super::transfer::MemoryReservation::reserve(
                                &self.store,
                                owner,
                                datapier_core::execution::migration::encoded_size(&snapshot)?,
                            )
                            .await?,
                        );
                        let mut state = self.state.lock().unwrap();
                        if !state.clients.contains(&owner) {
                            return Err(AppError::new(
                                "disconnected",
                                "导入已发布，但客户端已断开",
                            ));
                        }
                        let key = (owner, ws, snapshot.manifest.id);
                        if state.projects.projects.contains_key(&key) {
                            return Err(AppError::new(
                                "busy",
                                "导入已发布，项目已在当前工作区打开",
                            ));
                        }
                        state.projects.projects.insert(
                            key,
                            Arc::new(AsyncMutex::new(ProjectSlot {
                                snapshot: Arc::new(ChargedSnapshot {
                                    snapshot,
                                    _memory: memory,
                                }),
                                restored_baseline: false,
                                _lease: project_lease,
                            })),
                        );
                        Ok(response)
                    }
                    Err(error) => Err(error),
                }
            }
            "project.stages.discover" => {
                let work_lease = self.reserve_project(owner, WORK_BYTES)?;
                let stage_gate =
                    self.project_binding_gate(Uuid::nil(), "project-stage-registration");
                let _guard = stage_gate.lock().await;
                let root = ensure_stage_root(stage_root(self)).await?;
                let scan_root = root.clone();
                let (saves, imports, conflicts) = blocking_leased(work_lease.clone(), move || {
                    let mut saves = Vec::new();
                    let mut imports = Vec::new();
                    let mut conflicts = Vec::new();
                    for entry in std::fs::read_dir(&scan_root)? {
                        let entry = entry?;
                        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue };
                        let Ok(id) = Uuid::parse_str(&name) else { continue };
                        let dir = entry.path();
                        if dir.join("import.json").exists() {
                            match project::read_import_journal(&scan_root, id) {
                                Ok(stage) => imports.push(stage),
                                Err(error) => conflicts.push(json!({"stage_id":id,"path":dir,"file":"import.json","code":error.code,"message":error.message})),
                            }
                        } else if dir.join("stage.json").exists() {
                            match project::read_stage(&scan_root, id) {
                                Ok(stage) => saves.push(stage.identity),
                                Err(error) => conflicts.push(json!({"stage_id":id,"path":dir,"file":"stage.json","code":error.code,"message":error.message})),
                            }
                        }
                    }
                    Ok((saves, imports, conflicts))
                }).await?;
                let mut known = BTreeSet::new();
                let mut offset = 0;
                loop {
                    let page = self.store.transfer_artifacts(None, offset, 100).await?;
                    if page.is_empty() {
                        break;
                    }
                    offset += page.len() as u64;
                    for artifact in page {
                        if artifact.kind == "project_stage"
                            && let Some(id) = artifact.identity.file_id
                        {
                            known.insert(id);
                        }
                    }
                }
                let mut registered = Vec::new();
                for identity in saves {
                    if known.contains(&identity.id.to_string()) {
                        continue;
                    }
                    known.insert(identity.id.to_string());
                    let stage_root = root.clone();
                    let id = identity.id;
                    let stage = blocking_leased(work_lease.clone(), move || {
                        project::read_stage(&stage_root, id)
                    })
                    .await?;
                    registered.push(artifact_view(
                        &self
                            .register_project_stage(
                                owner,
                                &identity.stage_dir,
                                &identity.root,
                                id,
                                "save",
                                &stage.preview.digest,
                            )
                            .await?,
                    ));
                }
                for stage in imports {
                    if known.contains(&stage.id.to_string()) {
                        continue;
                    }
                    known.insert(stage.id.to_string());
                    let digest = stage
                        .digests
                        .get("datapier.project.json")
                        .ok_or_else(|| AppError::invalid("导入 stage 缺少 manifest 摘要"))?;
                    let check_root = root.clone();
                    let stage_id = stage.id;
                    let complete = blocking_leased(work_lease.clone(), move || {
                        Ok(project::read_import_stage(&check_root, stage_id).is_ok())
                    })
                    .await?;
                    if complete {
                        registered.push(artifact_view(
                            &self
                                .register_project_stage(
                                    owner,
                                    &stage.stage_dir,
                                    &stage.destination,
                                    stage.id,
                                    "import",
                                    digest,
                                )
                                .await?,
                        ));
                    } else {
                        let identity = ArtifactIdentity {
                            path: Some(stage.stage_dir.clone().into()),
                            canonical_path: Some(stage.destination.clone().into()),
                            file_id: Some(stage.id.to_string()),
                            kind: Some("import_preparing".into()),
                            stage_dir_identity: Some(binding_root_identity(&stage.stage_dir)?),
                            ownership_digest: Some(digest.clone()),
                            sha256: Some(digest_bytes(&serde_json::to_vec(&stage)?)),
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
                        registered.push(artifact_view(&self.store.transfer_artifact(id).await?));
                    }
                }
                let scan_root = root.clone();
                let names = blocking_leased(work_lease.clone(), move || -> Result<Vec<String>> {
                    let mut names = Vec::new();
                    for entry in std::fs::read_dir(scan_root)? {
                        let entry = entry?;
                        if let Some(name) = entry.file_name().to_str()
                            && Uuid::parse_str(name).is_ok()
                        {
                            names.push(name.to_owned());
                        }
                    }
                    Ok(names)
                })
                .await?;
                for name in names {
                    if known.contains(&name) {
                        continue;
                    }
                    let identity = ArtifactIdentity {
                        path: Some(root.join(&name)),
                        file_id: Some(name),
                        kind: Some("orphan_preparing".into()),
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
                    registered.push(artifact_view(&self.store.transfer_artifact(id).await?));
                }
                Ok(json!({"artifacts":registered,"conflicts":conflicts}))
            }
            "transfer.artifact.list" => {
                let offset = optional(&params, "offset")?.unwrap_or(0u64);
                let limit = optional(&params, "limit")?.unwrap_or(100u64);
                let artifacts = self.store.transfer_artifacts(None, offset, limit).await?;
                let next_offset = offset + artifacts.len() as u64;
                let next = if !artifacts.is_empty()
                    && !self
                        .store
                        .transfer_artifacts(None, next_offset, 1)
                        .await?
                        .is_empty()
                {
                    Some(next_offset)
                } else {
                    None
                };
                Ok(
                    json!({"artifacts":artifacts.iter().map(artifact_view).collect::<Vec<_>>(),"next":next}),
                )
            }
            "transfer.artifact.get" => {
                let id: Uuid = model::param(&params, "id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let gate = self.artifact_gate(id);
                let _guard = gate.lock().await;
                let artifact = self.store.transfer_artifact(id).await?;
                if artifact.revision != revision {
                    return Err(AppError::new("conflict", "资源版本已变化"));
                }
                let running = self.artifact_running(&artifact);
                if running {
                    return Err(AppError::new("busy", "资源关联任务仍在运行"));
                }
                let active = artifact
                    .owner_client_id
                    .is_some_and(|client| self.state.lock().unwrap().clients.contains(&client));
                if active && artifact.owner_client_id != Some(owner) {
                    return Err(AppError::new("busy", "资源所有者仍活跃"));
                }
                let mut response = json!({"artifact":artifact_view(&artifact)});
                if !active && growing_partial(&artifact) {
                    let identity = artifact.identity.clone();
                    response["observed_identity"] =
                        serde_json::to_value(blocking(move || inspect_partial(&identity)).await?)?;
                    response["requires_partial_confirmation"] = json!(true);
                }
                Ok(response)
            }
            "transfer.artifact.claim" => {
                let id: Uuid = model::param(&params, "id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let gate = self.artifact_gate(id);
                let _guard = gate.lock().await;
                let artifact = self.store.transfer_artifact(id).await?;
                // 客户端租约结束不代表已发送的数据库操作终止；writer 收尾前保持原归属。
                let running = self.artifact_running(&artifact);
                if running {
                    return Err(AppError::new("busy", "资源关联任务仍在运行，不能领取"));
                }
                let active = artifact
                    .owner_client_id
                    .is_some_and(|client| self.state.lock().unwrap().clients.contains(&client));
                let observed = if growing_partial(&artifact) {
                    if active {
                        return Err(AppError::new("busy", "文件仍由活跃客户端处理"));
                    }
                    let expected: Option<String> = optional(&params, "confirm_partial_sha256")?;
                    let expected = expected.ok_or_else(|| {
                        AppError::new("confirmation_required", "请先查看并确认当前未完成文件")
                    })?;
                    let identity = artifact.identity.clone();
                    let observed = blocking(move || inspect_partial(&identity)).await?;
                    if observed.sha256.as_deref() != Some(expected.as_str()) {
                        return Err(AppError::new(
                            "recovery_conflict",
                            "未完成文件在确认后又发生变化",
                        ));
                    }
                    Some(observed)
                } else {
                    None
                };
                let claimed = self
                    .store
                    .transfer_claim_artifact(id, owner, revision, artifact.owner_client_id, active)
                    .await?;
                if let Some(identity) = observed {
                    self.store
                        .transfer_update_artifact(
                            id,
                            owner,
                            Some(claimed),
                            "cleanup_pending",
                            Some(&serde_json::to_value(identity)?),
                        )
                        .await?;
                }
                Ok(json!({"artifact":artifact_view(&self.store.transfer_artifact(id).await?)}))
            }
            "project.recovery.read" | "project.recovery.prepare" => {
                let work_lease = self.reserve_project(owner, WORK_BYTES)?;
                let id: Uuid = model::param(&params, "id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let gate = self.artifact_gate(id);
                let _guard = gate.lock().await;
                let artifact = self.store.transfer_artifact(id).await?;
                owned(&artifact, owner, revision)?;
                let stage_gate =
                    self.project_binding_gate(Uuid::nil(), "project-stage-registration");
                let _stage_guard = stage_gate.lock().await;
                let root = ensure_stage_root(stage_root(self)).await?;
                let stage_id = registered_stage(&artifact, &root)?;
                if artifact
                    .identity
                    .kind
                    .as_deref()
                    .is_some_and(|kind| kind.ends_with("_preparing"))
                {
                    if method != "project.recovery.read" {
                        return Err(AppError::new(
                            "recovery_conflict",
                            "stage 尚未完整准备，只能查看或清理",
                        ));
                    }
                    let path = root.join(stage_id.to_string());
                    let (entries, observed_identity, journal_sha256) = blocking_leased(work_lease.clone(), move || -> Result<(Vec<Value>, Option<String>, Option<String>)> {
                        match std::fs::symlink_metadata(&path) {
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), None, None)),
                            Err(e) => return Err(e.into()),
                            Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => return Err(AppError::new("recovery_conflict", "stage 目录已被替换")),
                            Ok(_) => {}
                        }
                        let identity = binding_root_identity(path.to_str().ok_or_else(|| AppError::invalid("stage 路径不是 UTF-8"))?)?;
                        let journal = path.join("import.json");
                        let journal_digest = match std::fs::symlink_metadata(&journal) {
                            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
                            Err(e) => return Err(e.into()),
                            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() || meta.len() > MIB as u64 => None,
                            Ok(_) => Some(digest_bytes(&std::fs::read(journal)?)),
                        };
                        let mut entries = Vec::new();
                        for entry in std::fs::read_dir(path)? {
                            let entry = entry?;
                            let entry_path = entry.path();
                            let meta = std::fs::symlink_metadata(&entry_path)?;
                            let kind = if meta.file_type().is_symlink() { "symlink" } else if meta.is_file() { "file" } else if meta.is_dir() { "directory" } else { "other" };
                            entries.push(json!({"path":entry.file_name().to_string_lossy(),"type":kind,"size":meta.len(),"file_id":stage_entry_identity(&entry_path, &meta)}));
                        }
                        entries.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
                        Ok((entries, Some(identity), journal_digest))
                    }).await?;
                    return Ok(
                        json!({"id":id,"revision":revision,"state":"preparing","root":artifact.identity.canonical_path,"files":entries,"directory_identity_bound":artifact.identity.stage_dir_identity.is_some(),"observed_directory_identity":observed_identity,"journal_sha256":journal_sha256}),
                    );
                }
                if artifact.identity.kind.as_deref() == Some("import") {
                    let stage = blocking_leased(work_lease.clone(), move || {
                        project::read_import_stage(&root, stage_id)
                    })
                    .await?;
                    let stored = stage
                        .digests
                        .get("datapier.project.json")
                        .ok_or_else(|| AppError::invalid("导入 stage 缺少 manifest 摘要"))?;
                    if artifact.identity.ownership_digest.as_ref() != Some(stored)
                        || artifact.identity.canonical_path.as_deref()
                            != Some(Path::new(&stage.destination))
                    {
                        return Err(AppError::new(
                            "recovery_conflict",
                            "导入 stage 身份与登记不一致",
                        ));
                    }
                    let files: Vec<_> = stage.digests.iter().map(|(path, digest)| json!({"path":path,"old_digest":null,"new_digest":digest,"current_digest":null,"status":"staged"})).collect();
                    if method == "project.recovery.read" {
                        return Ok(
                            json!({"id":id,"revision":revision,"project_id":stage.project_id,"state":artifact.state,"root":stage.destination,"files":files}),
                        );
                    }
                    let action: ProjectRecoveryAction = model::param(&params, "action")?;
                    let plan_id = Uuid::new_v4();
                    let digest =
                        digest_bytes(&serde_json::to_vec(&(id, revision, stored, &action))?);
                    {
                        let mut state = self.state.lock().unwrap();
                        if !state.clients.contains(&owner) {
                            return Err(AppError::new("disconnected", "客户端会话已关闭"));
                        }
                        state.projects.insert_recovery(
                            plan_id,
                            RecoveryPlan {
                                owner,
                                artifact_id: id,
                                artifact_revision: revision,
                                digest: digest.clone(),
                                action,
                            },
                        )?;
                    }
                    return Ok(
                        json!({"plan_id":plan_id,"revision":1,"digest":digest,"changes":files}),
                    );
                }
                if artifact.identity.kind.as_deref() != Some("save") {
                    return Err(AppError::invalid("未知项目 stage 类型"));
                }
                let stage = blocking_leased(work_lease.clone(), move || {
                    project::read_stage(&root, stage_id)
                })
                .await?;
                if artifact.identity.ownership_digest.as_deref()
                    != Some(stage.preview.digest.as_str())
                    || artifact.identity.canonical_path.as_deref()
                        != Some(Path::new(&stage.identity.root))
                {
                    return Err(AppError::new("recovery_conflict", "stage 身份与登记不一致"));
                }
                if method == "project.recovery.prepare" {
                    let action: ProjectRecoveryAction = model::param(&params, "action")?;
                    let plan_id = Uuid::new_v4();
                    let digest = digest_bytes(&serde_json::to_vec(&(
                        id,
                        revision,
                        &stage.preview.digest,
                        &action,
                    ))?);
                    let changes = change_summaries(&stage.preview);
                    {
                        let mut state = self.state.lock().unwrap();
                        if !state.clients.contains(&owner) {
                            return Err(AppError::new("disconnected", "客户端会话已关闭"));
                        }
                        state.projects.insert_recovery(
                            plan_id,
                            RecoveryPlan {
                                owner,
                                artifact_id: id,
                                artifact_revision: revision,
                                digest: digest.clone(),
                                action,
                            },
                        )?;
                    }
                    Ok(json!({"plan_id":plan_id,"revision":1,"digest":digest,"changes":changes}))
                } else {
                    let files = blocking_leased(work_lease.clone(), move || {
                        let mut files = Vec::new();
                        for change in &stage.preview.changes {
                            let (current, status) = match current_digest(Path::new(&stage.identity.root), &change.path) {
                                Ok(current) => { let status = if current == change.old_digest { "old" } else if current == change.new_digest { "new" } else { "conflict" }; (current, status) },
                                Err(error) if error.code == "recovery_conflict" => (None, "conflict"),
                                Err(error) => return Err(error),
                            };
                            files.push(json!({"path":change.path,"old_digest":change.old_digest,"new_digest":change.new_digest,"current_digest":current,"status":status}));
                        }
                        Ok(json!({"id":id,"revision":revision,"project_id":stage.identity.project_id,"state":stage.state,"root":stage.identity.root,"files":files}))
                    }).await?;
                    Ok(files)
                }
            }
            "project.recovery.commit" => {
                let work_lease = self.reserve_project(owner, WORK_BYTES)?;
                let plan_id: Uuid = model::param(&params, "plan_id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let digest: String = model::param(&params, "digest")?;
                let slot = {
                    let mut state = self.state.lock().unwrap();
                    state.projects.prune();
                    state
                        .projects
                        .recoveries
                        .get(&plan_id)
                        .map(|entry| entry.slot.clone())
                        .ok_or_else(|| AppError::new("not_found", "恢复计划不存在或已过期"))?
                };
                let plan = slot.lock().await;
                if plan.owner != owner || revision != 1 || digest != plan.digest {
                    return Err(AppError::new(
                        "conflict",
                        "恢复计划所有者、版本或摘要不匹配",
                    ));
                }
                {
                    let mut state = self.state.lock().unwrap();
                    if !state.clients.contains(&owner)
                        || !state
                            .projects
                            .recoveries
                            .get(&plan_id)
                            .is_some_and(|entry| Arc::ptr_eq(&entry.slot, &slot))
                    {
                        return Err(AppError::new("conflict", "恢复计划已失效"));
                    }
                    state.projects.recoveries.remove(&plan_id);
                }
                let gate = self.artifact_gate(plan.artifact_id);
                let _guard = gate.lock().await;
                let artifact = self.store.transfer_artifact(plan.artifact_id).await?;
                owned(&artifact, owner, plan.artifact_revision)?;
                let root = ensure_stage_root(stage_root(self)).await?;
                let stage_id = registered_stage(&artifact, &root)?;
                if artifact.identity.kind.as_deref() == Some("import") {
                    let root_for_check = root.clone();
                    let stage = blocking_leased(work_lease.clone(), move || {
                        project::read_import_stage(&root_for_check, stage_id)
                    })
                    .await?;
                    if artifact.identity.ownership_digest.as_ref()
                        != stage.digests.get("datapier.project.json")
                        || artifact.identity.canonical_path.as_deref()
                            != Some(Path::new(&stage.destination))
                    {
                        return Err(AppError::new(
                            "recovery_conflict",
                            "导入 stage 身份与登记不一致",
                        ));
                    }
                    let project_id = stage.project_id;
                    let state = match plan.action.clone() {
                        ProjectRecoveryAction::Continue => {
                            blocking_leased(work_lease.clone(), move || {
                                project::commit_import(&root, stage_id)
                            })
                            .await?;
                            "published"
                        }
                        ProjectRecoveryAction::Rollback => {
                            blocking_leased(work_lease.clone(), move || cleanup_import(&stage))
                                .await?;
                            "cleaned"
                        }
                    };
                    self.store
                        .transfer_update_artifact(
                            artifact.id,
                            owner,
                            Some(artifact.revision),
                            state,
                            None,
                        )
                        .await?;
                    return Ok(json!({"state":state,"project_id":project_id}));
                }
                let root_for_check = root.clone();
                let stage = blocking_leased(work_lease.clone(), move || {
                    project::read_stage(&root_for_check, stage_id)
                })
                .await?;
                if artifact.identity.ownership_digest.as_deref()
                    != Some(stage.preview.digest.as_str())
                    || artifact.identity.canonical_path.as_deref()
                        != Some(Path::new(&stage.identity.root))
                {
                    return Err(AppError::new("recovery_conflict", "stage 身份与登记不一致"));
                }
                let action = plan.action.clone();
                let result = blocking_leased(work_lease.clone(), move || {
                    project::recover(&root, stage_id, action)
                })
                .await;
                match result {
                    Ok(stage) => {
                        self.store
                            .transfer_update_artifact(
                                artifact.id,
                                owner,
                                Some(artifact.revision),
                                "published",
                                None,
                            )
                            .await?;
                        Ok(json!({"state":stage.state,"project_id":stage.identity.project_id}))
                    }
                    Err(error) => {
                        if error.code == "recovery_conflict" {
                            self.store
                                .transfer_update_artifact(
                                    artifact.id,
                                    owner,
                                    Some(artifact.revision),
                                    "recovery_conflict",
                                    None,
                                )
                                .await?;
                        }
                        Err(error)
                    }
                }
            }
            "transfer.artifact.export" | "transfer.artifact.cleanup" => {
                let work_lease = self.reserve_project(owner, WORK_BYTES)?;
                let id: Uuid = model::param(&params, "id")?;
                let revision: u64 = model::param(&params, "revision")?;
                let gate = self.artifact_gate(id);
                let _guard = gate.lock().await;
                let artifact = self.store.transfer_artifact(id).await?;
                owned(&artifact, owner, revision)?;
                let stage_gate = (artifact.kind == "project_stage")
                    .then(|| self.project_binding_gate(Uuid::nil(), "project-stage-registration"));
                let _stage_guard = if let Some(gate) = &stage_gate {
                    Some(gate.lock().await)
                } else {
                    None
                };
                let running = self.artifact_running(&artifact);
                let live_writer = growing_partial(&artifact)
                    && artifact
                        .owner_client_id
                        .is_some_and(|id| self.state.lock().unwrap().clients.contains(&id));
                if running || live_writer {
                    return Err(AppError::new("busy", "资源仍在写入，不能导出或清理"));
                }
                if artifact.kind == "redis_temporary" {
                    if method == "transfer.artifact.export" {
                        return Err(AppError::new("unsupported", "不支持导出 Redis 临时对象"));
                    }
                    if matches!(
                        artifact.state.as_str(),
                        "published" | "outcome_unknown" | "cleaned"
                    ) {
                        return Err(AppError::invalid("已发布、已清理或结果未知的资源不能清理"));
                    }
                    let identity = datapier_core::providers::redis::RedisTemporaryIdentity::new(
                        &artifact.identity,
                    )?;
                    let profile_id = artifact.identity.profile_id.ok_or_else(|| {
                        AppError::new("recovery_conflict", "Redis 临时对象缺少连接配置")
                    })?;
                    let sessions: Vec<_> = self
                        .state
                        .lock()
                        .unwrap()
                        .databases
                        .values()
                        .filter(|database| database.profile.id == profile_id)
                        .cloned()
                        .collect();
                    for database in sessions {
                        let session = database
                            .session
                            .try_lock()
                            .map_err(|_| AppError::new("busy", "Redis 会话正在使用"))?;
                        if let ProviderSession::Redis(redis) = &*session
                            && redis.owns_temporary_artifact(id)
                        {
                            return Err(AppError::new("busy", "Redis 临时对象仍在写入"));
                        }
                    }
                    let redis_db = artifact.identity.redis_db.ok_or_else(|| {
                        AppError::new("recovery_conflict", "Redis 临时对象缺少数据库")
                    })?;
                    let profile_lock = self.profile_lock(profile_id);
                    let _profile_guard = profile_lock.lock().await;
                    let mut profile = self.store.profile(profile_id).await?;
                    if profile.provider != model::ProviderKind::Redis || profile.is_archery() {
                        return Err(AppError::new(
                            "permission_denied",
                            "清理需要直连 Redis 写入权限",
                        ));
                    }
                    profile.ensure_write()?;
                    if profile
                        .direct()?
                        .visible_databases
                        .as_ref()
                        .is_some_and(|allowed| !allowed.contains(&redis_db.to_string()))
                    {
                        return Err(AppError::new(
                            "permission_denied",
                            "Redis 数据库不在可见范围",
                        ));
                    }
                    profile.direct_mut()?.redis_db = redis_db;
                    let secret = match optional::<String>(&params, "secret")? {
                        Some(secret) => Some(secret),
                        None => self.store.secret(profile_id).await?,
                    };
                    let timeout = Duration::from_secs(
                        self.store.settings().await?.limits.connect_timeout_seconds,
                    );
                    let mut session = tokio::time::timeout(
                        timeout,
                        datapier_core::providers::redis::RedisSession::connect(profile, secret),
                    )
                    .await
                    .map_err(|_| AppError::new("timeout", "连接超时"))??;
                    let pending_revision = self
                        .store
                        .transfer_update_artifact(
                            id,
                            owner,
                            Some(revision),
                            "cleanup_pending",
                            None,
                        )
                        .await?;
                    // 提前登记发送事实；崩溃或丢失回包后，不允许自动重发。
                    let sent_revision = self
                        .store
                        .transfer_update_artifact(
                            id,
                            owner,
                            Some(pending_revision),
                            "outcome_unknown",
                            None,
                        )
                        .await?;
                    let result =
                        tokio::time::timeout(timeout, session.cleanup_temporary(&identity)).await;
                    match result {
                        Ok(Ok(())) => {
                            // 仅本次 EVAL 已明确回包时，允许确认结果未知状态。
                            self.store
                                .transfer_confirm_redis_cleanup(id, owner, sent_revision, "cleaned")
                                .await?;
                            return Ok(json!({"state":"cleaned"}));
                        }
                        Ok(Err(error)) if error.code == "recovery_conflict" => {
                            self.store
                                .transfer_confirm_redis_cleanup(
                                    id,
                                    owner,
                                    sent_revision,
                                    "recovery_conflict",
                                )
                                .await?;
                            return Err(error);
                        }
                        Ok(Err(error)) => {
                            return Err(AppError::unknown("Redis 清理结果未知，请人工核对")
                                .with_details(
                                    json!({"cause_code":error.code,"cause_message":error.message}),
                                ));
                        }
                        Err(_) => {
                            return Err(AppError::unknown("Redis 清理超时，结果未知，请人工核对"));
                        }
                    }
                }
                if method == "transfer.artifact.cleanup"
                    && (artifact.state == "outcome_unknown"
                        || (artifact.state == "published" && artifact.kind != "project_stage"))
                {
                    return Err(AppError::invalid("已发布或结果未知的资源不能清理"));
                }
                let export_path: Option<PathBuf> = optional(&params, "path")?;
                if method == "transfer.artifact.export" && export_path.is_none() {
                    return Err(AppError::invalid("缺少导出目标"));
                }
                let cleanup_revision = if method == "transfer.artifact.cleanup" {
                    Some(
                        self.store
                            .transfer_update_artifact(
                                id,
                                owner,
                                Some(revision),
                                "cleanup_pending",
                                None,
                            )
                            .await?,
                    )
                } else {
                    None
                };
                if artifact.kind == "project_stage" {
                    let root = ensure_stage_root(stage_root(self)).await?;
                    let stage_id = registered_stage(&artifact, &root)?;
                    let kind = artifact
                        .identity
                        .kind
                        .clone()
                        .ok_or_else(|| AppError::invalid("stage 类型缺失"))?;
                    let expected = artifact.identity.ownership_digest.clone();
                    let project_root = artifact.identity.canonical_path.clone();
                    if kind.ends_with("_preparing") {
                        if method != "transfer.artifact.cleanup" {
                            return Err(AppError::invalid("未完成 stage 只能查看或清理"));
                        }
                        let identity = artifact.identity.clone();
                        let check_root = root.clone();
                        let confirm_identity: Option<String> =
                            optional(&params, "confirm_stage_dir_identity")?;
                        let confirm_journal: Option<String> =
                            optional(&params, "confirm_journal_sha256")?;
                        blocking_leased(work_lease.clone(), move || {
                            let dir = check_root.join(stage_id.to_string());
                            match std::fs::symlink_metadata(&dir) {
                                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                    return Ok(());
                                }
                                Err(error) => return Err(error.into()),
                                Ok(_) if identity.stage_dir_identity.is_none() => {
                                    let actual =
                                        binding_root_identity(dir.to_str().ok_or_else(|| {
                                            AppError::invalid("stage 路径不是 UTF-8")
                                        })?)?;
                                    if confirm_identity.as_deref() != Some(actual.as_str()) {
                                        return Err(AppError::new(
                                            "confirmation_required",
                                            "请核对并确认 stage 目录身份",
                                        ));
                                    }
                                }
                                Ok(_) => {}
                            }
                            if kind == "save_preparing" && dir.join("stage.json").exists() {
                                let stage = project::read_stage(&check_root, stage_id)?;
                                if identity.ownership_digest.as_deref()
                                    != Some(stage.preview.digest.as_str())
                                    || identity.canonical_path.as_deref()
                                        != Some(Path::new(&stage.identity.root))
                                {
                                    return Err(AppError::new(
                                        "recovery_conflict",
                                        "保存 stage 意图不匹配",
                                    ));
                                }
                                project::cleanup_stage(&check_root, stage_id)
                            } else if kind == "import_preparing" && dir.join("import.json").exists()
                            {
                                let journal = dir.join("import.json");
                                let meta = std::fs::symlink_metadata(&journal)?;
                                if meta.file_type().is_symlink()
                                    || !meta.is_file()
                                    || meta.len() > MIB as u64
                                {
                                    return Err(AppError::new("recovery_conflict", "导入日志无效"));
                                }
                                let bytes = std::fs::read(&journal)?;
                                let observed = digest_bytes(&bytes);
                                if identity.sha256.is_none() {
                                    if confirm_journal.as_deref() != Some(observed.as_str()) {
                                        return Err(AppError::new(
                                            "confirmation_required",
                                            "请核对并确认尚未登记的导入日志",
                                        ));
                                    }
                                    let entries: Vec<_> =
                                        std::fs::read_dir(&dir)?.collect::<std::io::Result<_>>()?;
                                    if entries.len() != 1 || entries[0].file_name() != "import.json"
                                    {
                                        return Err(AppError::new(
                                            "recovery_conflict",
                                            "stage 包含未知内容",
                                        ));
                                    }
                                    project::read_import_journal(&check_root, stage_id)?;
                                    std::fs::remove_file(journal)?;
                                    std::fs::remove_dir(dir)?;
                                    return Ok(());
                                }
                                if identity.sha256.as_deref() != Some(observed.as_str()) {
                                    return Err(AppError::new(
                                        "recovery_conflict",
                                        "导入日志摘要已变化",
                                    ));
                                }
                                let stage = project::read_import_journal(&check_root, stage_id)?;
                                cleanup_import(&stage)
                            } else {
                                // 只删除已绑定身份的空目录；未知文件导致 remove_dir 失败并保留。
                                std::fs::remove_dir(dir)?;
                                Ok(())
                            }
                        })
                        .await?;
                    } else if kind == "save" {
                        let root_for_check = root.clone();
                        let stage = blocking_leased(work_lease.clone(), move || {
                            project::read_stage(&root_for_check, stage_id)
                        })
                        .await?;
                        if expected.as_deref() != Some(stage.preview.digest.as_str())
                            || project_root.as_deref() != Some(Path::new(&stage.identity.root))
                        {
                            return Err(AppError::new(
                                "recovery_conflict",
                                "stage 身份与登记不一致",
                            ));
                        }
                        if method == "transfer.artifact.export" {
                            let index: usize = model::param(&params, "index")?;
                            let side: String = model::param(&params, "side")?;
                            let change = stage
                                .preview
                                .changes
                                .get(index)
                                .ok_or_else(|| AppError::invalid("文件索引无效"))?;
                            let digest = match side.as_str() {
                                "old" => &change.old_digest,
                                "new" => &change.new_digest,
                                _ => return Err(AppError::invalid("side 必须为 old 或 new")),
                            }
                            .clone()
                            .ok_or_else(|| AppError::invalid("指定版本不存在"))?;
                            let input = Path::new(&stage.identity.stage_dir)
                                .join(format!("{index}.{side}"));
                            let output = export_path.unwrap();
                            blocking_leased(work_lease.clone(), move || {
                                let meta = std::fs::symlink_metadata(&input)?;
                                if meta.file_type().is_symlink()
                                    || !meta.is_file()
                                    || meta.len() > 16 * 1024 * 1024
                                {
                                    return Err(AppError::new("recovery_conflict", "恢复副本无效"));
                                }
                                let mut bytes = Vec::new();
                                std::fs::File::open(input)?
                                    .take(16 * 1024 * 1024 + 1)
                                    .read_to_end(&mut bytes)?;
                                if bytes.len() > 16 * 1024 * 1024 || digest_bytes(&bytes) != digest
                                {
                                    return Err(AppError::new(
                                        "recovery_conflict",
                                        "恢复副本已变化",
                                    ));
                                }
                                write_new(&output, &bytes)
                            })
                            .await?;
                        } else {
                            blocking_leased(work_lease.clone(), move || {
                                project::cleanup_stage(&root, stage_id)
                            })
                            .await?;
                        }
                    } else if kind == "import" {
                        let root_for_check = root.clone();
                        let stage = blocking_leased(work_lease.clone(), move || {
                            project::read_import_stage(&root_for_check, stage_id)
                        })
                        .await?;
                        if expected.as_ref() != stage.digests.get("datapier.project.json")
                            || project_root.as_deref() != Some(Path::new(&stage.destination))
                        {
                            return Err(AppError::new(
                                "recovery_conflict",
                                "导入 stage 身份与登记不一致",
                            ));
                        }
                        if method == "transfer.artifact.export" {
                            let output = export_path.unwrap();
                            blocking_leased(work_lease.clone(), move || {
                                let snapshot =
                                    project::open(&Path::new(&stage.stage_dir).join("project"))?;
                                project::export_archive(&snapshot, &output)
                            })
                            .await?;
                        } else {
                            blocking_leased(work_lease.clone(), move || cleanup_import(&stage))
                                .await?;
                        }
                    } else {
                        return Err(AppError::invalid("未知项目 stage 类型"));
                    }
                } else {
                    let identity = artifact.identity.clone();
                    if method == "transfer.artifact.export" {
                        let output = export_path.unwrap();
                        blocking_leased(work_lease.clone(), move || {
                            let input = verified_file(&identity)?;
                            let mut source = std::fs::File::open(&input)?;
                            let mut dest = std::fs::OpenOptions::new()
                                .write(true)
                                .create_new(true)
                                .open(output)?;
                            std::io::copy(&mut source, &mut dest)?;
                            dest.sync_all()?;
                            verified_file(&identity)?;
                            Ok(())
                        })
                        .await?;
                    } else {
                        blocking_leased(work_lease.clone(), move || {
                            let path = verified_file(&identity)?;
                            std::fs::remove_file(path)?;
                            Ok(())
                        })
                        .await?;
                    }
                }
                if method == "transfer.artifact.cleanup" {
                    self.store
                        .transfer_update_artifact(id, owner, cleanup_revision, "cleaned", None)
                        .await?;
                    Ok(json!({"state":"cleaned"}))
                } else {
                    Ok(json!({"state":"published"}))
                }
            }
            _ => Err(AppError::new("method_not_found", "未知项目方法")),
        }
    }
}

#[cfg(all(test, windows))]
#[path = "../../tests/unit/server/project_identity.rs"]
mod windows_identity_tests;
