//! 项目目录、显式保存日志和 Stored ZIP 交换。所有接口均为阻塞文件操作。

mod platform;

use crate::model::{AppError, Result, project::*};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;
use zip::{CompressionMethod, ZipArchive, ZipWriter, write::SimpleFileOptions};

const MANIFEST: &str = "datapier.project.json";
const MAX_FILES: usize = 10_000;
const MAX_FILE: usize = 16 * 1024 * 1024;
const MAX_TOTAL: usize = 64 * 1024 * 1024;
const MAX_MANIFEST: usize = 1024 * 1024;

fn invalid(message: impl Into<String>) -> AppError {
    AppError::invalid(message)
}
fn conflict(message: impl Into<String>) -> AppError {
    AppError::new("recovery_conflict", message)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn zip_error(e: zip::result::ZipError) -> AppError {
    invalid(format!("ZIP: {e}"))
}
fn utf8(bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|e| invalid(format!("文件不是 UTF-8：{e}")))
}

// 文件系统可能将 Unicode 等价形式视为同一名称，必须在写入前拒绝。
fn path_key(path: &str) -> String {
    path.nfc()
        .collect::<String>()
        .to_lowercase()
        .nfc()
        .collect()
}

// 跨平台统一拒绝大小写碰撞、Windows 驱动器、保留名称和反斜杠。
fn validate_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.contains(['\\', ':', '\0', '<', '>', '"', '|', '?', '*'])
        || path.starts_with('/')
    {
        return Err(invalid(format!("非法项目路径：{path}")));
    }
    for part in path.split('/') {
        let upper = part.split('.').next().unwrap_or("").to_ascii_uppercase();
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with([' ', '.'])
            || part.chars().any(char::is_control)
            || matches!(
                upper.as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            )
            || part.eq_ignore_ascii_case(".git")
            || part.to_ascii_lowercase().starts_with(".datapier-")
        {
            return Err(invalid(format!("非法项目路径：{path}")));
        }
    }
    Ok(())
}

fn check_root(root: &Path) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid("项目根目录必须是实际目录"));
    }
    Ok(fs::canonicalize(root)?)
}

// 每个分量单独检查，拒绝已存在的符号链接和特殊文件。
fn safe_path(root: &Path, relative: &str, make_parents: bool) -> Result<PathBuf> {
    validate_path(relative)?;
    let mut path = root.to_path_buf();
    let parts: Vec<_> = relative.split('/').collect();
    for (index, part) in parts.iter().enumerate() {
        path.push(part);
        let last = index + 1 == parts.len();
        match fs::symlink_metadata(&path) {
            Ok(m)
                if m.file_type().is_symlink()
                    || (last && !m.is_file())
                    || (!last && !m.is_dir()) =>
            {
                return Err(invalid(format!("路径含符号链接或特殊文件：{relative}")));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if !last && make_parents {
                    fs::create_dir(&path)?;
                    #[cfg(unix)]
                    File::open(path.parent().ok_or_else(|| invalid("目录无父路径"))?)?
                        .sync_all()?;
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(path)
}
fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(invalid("项目文件超过大小限制"));
    }
    Ok(bytes)
}
fn read_optional(root: &Path, path: &str) -> Result<Option<Vec<u8>>> {
    let full = safe_path(root, path, false)?;
    match fs::symlink_metadata(&full) {
        Ok(_) => Ok(Some(read_limited(
            &full,
            if path == MANIFEST {
                MAX_MANIFEST
            } else {
                MAX_FILE
            },
        )?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}
fn required(root: &Path, path: &str) -> Result<String> {
    utf8(read_optional(root, path)?.ok_or_else(|| invalid(format!("缺少项目文件：{path}")))?)
}
fn sorted_json<T: serde::Serialize>(value: &T) -> Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(value)?))
}

fn validate_transfer(value: &serde_json::Value, aliases: &BTreeSet<String>) -> Result<()> {
    let spec: crate::model::transfer::TransferSpec = serde_json::from_value(value.clone())?;
    spec.validate()?;
    for endpoint in [&spec.source, &spec.destination].into_iter().flatten() {
        if endpoint.profile_id.is_some() {
            return Err(invalid("项目配置不能包含本机 profile ID"));
        }
        if endpoint.database.is_some() || endpoint.redis_db.is_some() {
            return Err(invalid("项目迁移的实际数据库必须保存在本机绑定中"));
        }
        let alias = endpoint
            .connection_alias
            .as_ref()
            .ok_or_else(|| invalid("项目连接必须指定别名"))?;
        if !aliases.contains(alias) {
            return Err(invalid(format!("未知连接别名：{alias}")));
        }
    }
    if spec.objects.iter().any(|object| {
        object
            .source
            .database
            .as_deref()
            .is_some_and(|db| !db.is_empty())
            || object
                .target
                .database
                .as_deref()
                .is_some_and(|db| !db.is_empty())
    }) || spec.inputs.iter().any(|input| {
        input
            .target
            .as_ref()
            .and_then(|target| target.database.as_deref())
            .is_some_and(|db| !db.is_empty())
    }) {
        return Err(invalid("项目迁移对象的实际数据库必须保存在本机绑定中"));
    }
    for path in spec
        .inputs
        .iter()
        .map(|i| &i.path)
        .chain(spec.output.iter().map(|o| &o.path))
    {
        let text = path.to_str().ok_or_else(|| invalid("路径必须是 UTF-8"))?;
        if text.starts_with("${") && text.ends_with('}') {
            let name = &text[2..text.len() - 1];
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                return Err(invalid("无效的具名路径参数"));
            }
        } else {
            validate_path(text)?;
        }
    }
    Ok(())
}

fn validate(manifest: &ProjectManifest, files: &BTreeMap<String, String>) -> Result<()> {
    validate_partial(manifest, files, false)
}

fn validate_partial(
    manifest: &ProjectManifest,
    files: &BTreeMap<String, String>,
    allow_missing: bool,
) -> Result<()> {
    if manifest.format != "datapier-project" || manifest.version != 1 {
        return Err(invalid("不支持的项目格式或版本"));
    }
    if manifest.id.is_nil() || manifest.name.trim().is_empty() {
        return Err(invalid("项目 ID 或名称无效"));
    }
    if manifest.display.is_some() {
        return Err(invalid("当前不支持共享 display 配置"));
    }
    let mut aliases = BTreeSet::new();
    for c in &manifest.connections {
        if c.alias.trim().is_empty()
            || !aliases.insert(c.alias.clone())
            || !matches!(c.provider.as_str(), "mysql" | "redis")
            || !matches!(c.source_kind.as_str(), "direct" | "archery")
        {
            return Err(invalid("连接别名重复或连接类型无效"));
        }
        if c.database
            .as_ref()
            .is_some_and(|v| !(v.is_string() || v.is_number() || v.is_null()))
        {
            return Err(invalid("逻辑数据库必须是名称或 DB 编号"));
        }
    }
    let mut ids = BTreeSet::new();
    let mut query_ids = BTreeSet::new();
    let mut paths = BTreeSet::new();
    paths.insert(path_key(MANIFEST));
    for q in &manifest.queries {
        validate_path(&q.path)?;
        let ext = if q.language == "sql" {
            ".sql"
        } else if q.language == "redis" {
            ".redis"
        } else {
            return Err(invalid("未知查询语言"));
        };
        if q.id.is_nil()
            || !ids.insert(q.id)
            || q.name.trim().is_empty()
            || !aliases.contains(&q.connection_alias)
            || !q.path.to_ascii_lowercase().ends_with(ext)
            || !paths.insert(path_key(&q.path))
        {
            return Err(invalid("查询 ID、别名或路径无效或重复"));
        }
        if q.database
            .as_ref()
            .is_some_and(|v| !(v.is_string() || v.is_number() || v.is_null()))
        {
            return Err(invalid("逻辑数据库必须是名称或 DB 编号"));
        }
        query_ids.insert(q.id);
    }
    for t in &manifest.transfers {
        validate_path(&t.path)?;
        if t.id.is_nil()
            || !ids.insert(t.id)
            || t.name.trim().is_empty()
            || !t.path.to_ascii_lowercase().ends_with(".json")
            || !paths.insert(path_key(&t.path))
        {
            return Err(invalid("transfer ID 或路径无效或重复"));
        }
        if let Some(text) = files.get(&t.path) {
            validate_transfer(&serde_json::from_str(text)?, &aliases)?;
        } else if !allow_missing {
            return Err(invalid(format!("缺少 transfer 文件：{}", t.path)));
        }
    }
    if !manifest.layout.resources.is_empty() {
        return Err(invalid("当前项目不支持未定义的资源 ID"));
    }
    if manifest
        .layout
        .tabs
        .iter()
        .chain(manifest.layout.splits.iter().flatten())
        .chain(manifest.layout.columns.keys())
        .any(|id| !query_ids.contains(id))
    {
        return Err(invalid("布局引用不存在的查询"));
    }
    let expected: BTreeSet<_> = manifest
        .queries
        .iter()
        .map(|q| q.path.as_str())
        .chain(manifest.transfers.iter().map(|t| t.path.as_str()))
        .collect();
    if (!allow_missing && files.len() != expected.len())
        || files.keys().any(|p| !expected.contains(p.as_str()))
    {
        return Err(invalid("项目文件与 manifest 引用不一致"));
    }
    let mut components = BTreeMap::<String, String>::new();
    for path in expected.iter().copied().chain(std::iter::once(MANIFEST)) {
        let mut prefix = String::new();
        let mut parts = path.split('/').peekable();
        while let Some(component) = parts.next() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            let folded = path_key(&prefix);
            if parts.peek().is_some() && paths.contains(&folded) {
                return Err(invalid("项目文件与目录路径冲突"));
            }
            if let Some(previous) = components.insert(folded, prefix.clone())
                && previous != prefix
            {
                return Err(invalid("目录路径大小写冲突"));
            }
        }
    }
    let manifest_len = sorted_json(manifest)?.len();
    let mut total = manifest_len;
    if manifest_len > MAX_MANIFEST || files.len() + 1 > MAX_FILES {
        return Err(invalid("项目清单或文件数超过限制"));
    }
    for text in files.values() {
        if text.len() > MAX_FILE {
            return Err(invalid("项目文件超过限制"));
        }
        total = total
            .checked_add(text.len())
            .ok_or_else(|| invalid("项目大小溢出"))?;
    }
    if total > MAX_TOTAL {
        return Err(invalid("项目总大小超过限制"));
    }
    Ok(())
}

/// 创建不存在的空项目目录，不创建连接或启动执行。
pub fn create(root: &Path, name: String) -> Result<ProjectSnapshot> {
    let manifest = ProjectManifest {
        format: "datapier-project".into(),
        version: 1,
        id: Uuid::new_v4(),
        name,
        connections: vec![],
        queries: vec![],
        transfers: vec![],
        layout: ProjectLayout::default(),
        display: None,
    };
    validate(&manifest, &BTreeMap::new())?;
    fs::create_dir(root)?;
    let file = root.join(MANIFEST);
    let mut out = OpenOptions::new().write(true).create_new(true).open(file)?;
    out.write_all(sorted_json(&manifest)?.as_bytes())?;
    out.sync_all()?;
    open(root)
}

/// 只加载受 manifest 引用的文本；不创建任何本机绑定。
pub fn open(path: &Path) -> Result<ProjectSnapshot> {
    open_with_baseline(path, None)
}

/// 外部读取保留删除事实；只允许原基线中的文件缺失，不伪造其正文。
pub fn open_external(path: &Path, baseline: &ProjectSnapshot) -> Result<ProjectSnapshot> {
    open_with_baseline(path, Some(baseline))
}

/// 恢复保存的冲突基线；不把磁盘变化接受为新基线，也不授权本机绑定。
pub fn restore_baseline(
    path: &Path,
    manifest: ProjectManifest,
    digests: BTreeMap<String, String>,
) -> Result<ProjectSnapshot> {
    validate_partial(&manifest, &BTreeMap::new(), true)?;
    let expected: BTreeSet<String> = manifest
        .queries
        .iter()
        .map(|q| q.path.clone())
        .chain(manifest.transfers.iter().map(|t| t.path.clone()))
        .chain(std::iter::once(MANIFEST.to_owned()))
        .collect();
    if digests.len() != expected.len()
        || digests.keys().any(|path| !expected.contains(path))
        || digests.values().any(|digest| {
            digest.len() != 64
                || !digest
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err(invalid("保存的项目基线摘要无效"));
    }
    let path = if path.file_name().is_some_and(|name| name == MANIFEST) {
        path.parent().ok_or_else(|| invalid("无效项目路径"))?
    } else {
        path
    };
    let root = check_root(path)?;
    let disk: ProjectManifest = serde_json::from_str(&required(&root, MANIFEST)?)?;
    if disk.id != manifest.id {
        return Err(AppError::new("project_conflict", "外部项目 ID 已改变"));
    }
    let mut files = BTreeMap::new();
    let mut total = 0usize;
    for path in expected.iter().filter(|path| path.as_str() != MANIFEST) {
        if let Some(bytes) = read_optional(&root, path)? {
            total = total
                .checked_add(bytes.len())
                .ok_or_else(|| invalid("项目大小溢出"))?;
            if total > MAX_TOTAL {
                return Err(invalid("项目总大小超过限制"));
            }
            if digests
                .get(path)
                .is_some_and(|digest| *digest == hash(&bytes))
            {
                files.insert(path.clone(), utf8(bytes)?);
            }
        }
    }
    Ok(ProjectSnapshot {
        root: root.to_string_lossy().into_owned(),
        manifest,
        files,
        digests,
    })
}

fn open_with_baseline(path: &Path, baseline: Option<&ProjectSnapshot>) -> Result<ProjectSnapshot> {
    let path = if path.file_name().is_some_and(|n| n == MANIFEST) {
        path.parent().ok_or_else(|| invalid("无效项目路径"))?
    } else {
        path
    };
    let root = check_root(path)?;
    let manifest_text = required(&root, MANIFEST)?;
    let manifest: ProjectManifest = serde_json::from_str(&manifest_text)?;
    if manifest
        .queries
        .len()
        .saturating_add(manifest.transfers.len())
        + 1
        > MAX_FILES
    {
        return Err(invalid("项目文件数超过限制"));
    }
    let mut total = manifest_text.len();
    let mut files = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for path in manifest
        .queries
        .iter()
        .map(|q| &q.path)
        .chain(manifest.transfers.iter().map(|t| &t.path))
    {
        if !seen.insert(path.clone()) {
            return Err(invalid("重复项目路径"));
        }
        let text = match read_optional(&root, path)? {
            Some(bytes) => utf8(bytes)?,
            None => {
                if baseline.is_some_and(|snapshot| snapshot.digests.contains_key(path)) {
                    continue;
                }
                return Err(invalid(format!("缺少项目文件：{path}")));
            }
        };
        total = total
            .checked_add(text.len())
            .ok_or_else(|| invalid("项目大小溢出"))?;
        if total > MAX_TOTAL {
            return Err(invalid("项目总大小超过限制"));
        }
        files.insert(path.clone(), text);
    }
    validate_partial(&manifest, &files, baseline.is_some())?;
    let mut digests: BTreeMap<String, String> = files
        .iter()
        .map(|(p, s)| (p.clone(), hash(s.as_bytes())))
        .collect();
    digests.insert(MANIFEST.into(), hash(manifest_text.as_bytes()));
    Ok(ProjectSnapshot {
        root: root.to_string_lossy().into_owned(),
        manifest,
        files,
        digests,
    })
}

/// 对用户选择的完整新清单和文本生成可审阅的差异。
pub fn preview(
    snapshot: &ProjectSnapshot,
    mut manifest: ProjectManifest,
    files: BTreeMap<String, String>,
) -> Result<ProjectSavePreview> {
    if manifest.id != snapshot.manifest.id {
        return Err(invalid("不能改变项目 ID"));
    }
    manifest.connections.sort_by(|a, b| a.alias.cmp(&b.alias));
    manifest.queries.sort_by_key(|q| q.id);
    manifest.transfers.sort_by_key(|t| t.id);
    validate(&manifest, &files)?;
    let root = check_root(Path::new(&snapshot.root))?;
    let mut new_files = files;
    new_files.insert(MANIFEST.into(), sorted_json(&manifest)?);
    let mut paths: BTreeSet<String> = snapshot.digests.keys().cloned().collect();
    paths.extend(new_files.keys().cloned());
    let mut changes = Vec::new();
    for path in paths {
        let current = read_optional(&root, &path)?;
        let current_digest = current.as_ref().map(|v| hash(v));
        let old_digest = snapshot.digests.get(&path).cloned();
        if current_digest != old_digest {
            return Err(AppError::new(
                "project_conflict",
                format!("文件已被外部修改：{path}"),
            ));
        }
        let new_content = new_files.remove(&path);
        let new_digest = new_content.as_ref().map(|s| hash(s.as_bytes()));
        if old_digest != new_digest {
            changes.push(ProjectFileChange {
                path,
                old_content: current.map(utf8).transpose()?,
                new_content,
                old_digest,
                new_digest,
            });
        }
    }
    // manifest 最后发布，内容文件在前，避免清单指向尚未发布的新版。
    changes.sort_by(|a, b| (a.path == MANIFEST, &a.path).cmp(&(b.path == MANIFEST, &b.path)));
    let baseline = snapshot.digests.clone();
    let digest = hash(&serde_json::to_vec(&(
        manifest.id,
        &snapshot.root,
        &baseline,
        &changes,
    ))?);
    Ok(ProjectSavePreview {
        project_id: manifest.id,
        root: snapshot.root.clone(),
        baseline,
        changes,
        digest,
    })
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| invalid("文件没有父目录"))?;
    let temp = parent.join(format!(".datapier-{}.tmp", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        // Windows 不允许仍持有文件句柄时可靠地替换路径。
        drop(file);
        fs::rename(&temp, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

fn stage_log(stage: &ProjectStage) -> Result<()> {
    atomic_write(
        &Path::new(&stage.identity.stage_dir).join("stage.json"),
        &serde_json::to_vec(stage)?,
    )
}

fn project_lock(root: &Path) -> Result<File> {
    let path = root.join(".datapier-lock");
    if let Ok(meta) = fs::symlink_metadata(&path)
        && (!meta.is_file() || meta.file_type().is_symlink())
    {
        return Err(invalid("项目锁路径不是普通文件"));
    }
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock.lock_exclusive()?;
    Ok(lock)
}

/// 将用户确认的旧新文本完整保存到 UUID stage；返回后须先由 Engine 持久登记再提交。
pub fn prepare_save(
    preview: &ProjectSavePreview,
    expected_digest: &str,
    staging_root: &Path,
) -> Result<ProjectStage> {
    prepare_save_in(
        preview,
        expected_digest,
        staging_root,
        Uuid::new_v4(),
        false,
    )
}

/// 仅供 Engine 已登记并绑定目录身份的保存事务使用。
pub fn prepare_save_in(
    preview: &ProjectSavePreview,
    expected_digest: &str,
    staging_root: &Path,
    id: Uuid,
    existing: bool,
) -> Result<ProjectStage> {
    if expected_digest != preview.digest
        || hash(&serde_json::to_vec(&(
            preview.project_id,
            &preview.root,
            &preview.baseline,
            &preview.changes,
        ))?) != preview.digest
    {
        return Err(invalid("保存预览摘要不匹配"));
    }
    let root = check_root(Path::new(&preview.root))?;
    let stage_root = check_root(staging_root)?;
    let _lock = project_lock(&root)?;
    for (path, digest) in &preview.baseline {
        if read_optional(&root, path)?.map(|b| hash(&b)).as_ref() != Some(digest) {
            return Err(AppError::new(
                "project_conflict",
                format!("文件已被外部修改：{path}"),
            ));
        }
    }
    for change in &preview.changes {
        let actual = read_optional(&root, &change.path)?.map(|b| hash(&b));
        if actual != change.old_digest {
            return Err(AppError::new(
                "project_conflict",
                format!("文件已被外部修改：{}", change.path),
            ));
        }
        if change.old_content.as_ref().map(|s| hash(s.as_bytes())) != change.old_digest
            || change.new_content.as_ref().map(|s| hash(s.as_bytes())) != change.new_digest
        {
            return Err(invalid("预览内容与摘要不一致"));
        }
    }
    let dir = stage_root.join(id.to_string());
    if !existing {
        fs::create_dir(&dir)?;
    }
    let meta = fs::symlink_metadata(&dir)?;
    if meta.file_type().is_symlink() || !meta.is_dir() || fs::read_dir(&dir)?.next().is_some() {
        return Err(conflict("stage 目录不是登记的空目录"));
    }
    let identity = ProjectStageIdentity {
        id,
        project_id: preview.project_id,
        root: preview.root.clone(),
        stage_dir: dir.to_string_lossy().into_owned(),
        revision: 1,
    };
    let mut stage = ProjectStage {
        identity,
        state: ProjectStageState::Preparing,
        preview: preview.clone(),
        applied: Vec::new(),
    };
    stage_log(&stage)?;
    for (index, change) in preview.changes.iter().enumerate() {
        for (suffix, content) in [("old", &change.old_content), ("new", &change.new_content)] {
            if let Some(content) = content {
                atomic_write(&dir.join(format!("{index}.{suffix}")), content.as_bytes())?;
            }
        }
    }
    stage.state = ProjectStageState::Ready;
    stage_log(&stage)?;
    Ok(stage)
}

/// 读取已登记或由 list_stages 发现的 stage；损坏的副本仍允许查看日志。
pub fn read_stage(staging_root: &Path, id: Uuid) -> Result<ProjectStage> {
    let base = check_root(staging_root)?;
    let dir = base.join(id.to_string());
    let metadata = fs::symlink_metadata(&dir)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid("stage 目录无效"));
    }
    let log = dir.join("stage.json");
    let meta = fs::symlink_metadata(&log)?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(invalid("stage 日志无效"));
    }
    let stage: ProjectStage = serde_json::from_slice(&read_limited(&log, MAX_TOTAL * 4)?)?;
    if stage.identity.id != id
        || Path::new(&stage.identity.stage_dir) != dir
        || stage.identity.project_id != stage.preview.project_id
        || stage.identity.root != stage.preview.root
        || hash(&serde_json::to_vec(&(
            stage.preview.project_id,
            &stage.preview.root,
            &stage.preview.baseline,
            &stage.preview.changes,
        ))?) != stage.preview.digest
    {
        return Err(invalid("stage 身份或摘要不匹配"));
    }
    for change in &stage.preview.changes {
        validate_path(&change.path)?;
    }
    Ok(stage)
}

/// 发现可领取的 UUID stage；领取及 revision 所有权由 Engine／Store 管理。
pub fn list_stages(staging_root: &Path) -> Result<Vec<ProjectStageIdentity>> {
    let root = check_root(staging_root)?;
    let mut identities = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(&name) else {
            continue;
        };
        if entry.path().join("import.json").exists() || !entry.path().join("stage.json").exists() {
            continue;
        }
        identities.push(read_stage(&root, id)?.identity);
    }
    identities.sort_by_key(|entry| entry.id);
    Ok(identities)
}

/// 按旧／新摘要核对后继续或回退；遇第三种内容时保留所有副本并标记冲突。
pub fn recover(
    staging_root: &Path,
    id: Uuid,
    action: ProjectRecoveryAction,
) -> Result<ProjectStage> {
    let mut stage = read_stage(staging_root, id)?;
    if stage.state == ProjectStageState::Preparing {
        // Preparing 日志已保存并校验全部正文；只重建缺失副本，不覆盖任何已有内容。
        for (index, change) in stage.preview.changes.iter().enumerate() {
            for (suffix, content, digest) in [
                ("old", &change.old_content, &change.old_digest),
                ("new", &change.new_content, &change.new_digest),
            ] {
                if let Some(text) = content {
                    if Some(hash(text.as_bytes())) != *digest {
                        return Err(conflict("stage 日志内容摘要不一致"));
                    }
                    let path =
                        Path::new(&stage.identity.stage_dir).join(format!("{index}.{suffix}"));
                    match fs::symlink_metadata(&path) {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            atomic_write(&path, text.as_bytes())?
                        }
                        Err(error) => return Err(error.into()),
                        Ok(_) => {}
                    }
                }
            }
        }
        stage.state = ProjectStageState::Ready;
        stage_log(&stage)?;
    }
    for (index, change) in stage.preview.changes.iter().enumerate() {
        for (suffix, content, digest) in [
            ("old", &change.old_content, &change.old_digest),
            ("new", &change.new_content, &change.new_digest),
        ] {
            let valid = if let Some(text) = content {
                let file = Path::new(&stage.identity.stage_dir).join(format!("{index}.{suffix}"));
                fs::symlink_metadata(&file)
                    .ok()
                    .filter(|m| m.is_file() && !m.file_type().is_symlink())
                    .and_then(|_| read_limited(&file, MAX_FILE).ok())
                    .is_some_and(|bytes| {
                        hash(&bytes) == hash(text.as_bytes()) && Some(hash(&bytes)) == *digest
                    })
            } else {
                digest.is_none()
            };
            if !valid {
                let path = change.path.clone();
                stage.state = ProjectStageState::RecoveryConflict;
                stage_log(&stage)?;
                return Err(conflict(format!("恢复副本缺失或损坏：{path}")));
            }
        }
    }
    let root = check_root(Path::new(&stage.identity.root))?;
    let _lock = project_lock(&root)?;
    let changed: BTreeSet<_> = stage
        .preview
        .changes
        .iter()
        .map(|c| c.path.as_str())
        .collect();
    for (path, digest) in &stage.preview.baseline {
        if !changed.contains(path.as_str())
            && read_optional(&root, path)?.map(|b| hash(&b)).as_ref() != Some(digest)
        {
            let path = path.clone();
            stage.state = ProjectStageState::RecoveryConflict;
            stage_log(&stage)?;
            return Err(conflict(format!("未修改的项目文件发生外部变化：{path}")));
        }
    }
    for change in &stage.preview.changes {
        let actual = read_optional(&root, &change.path)?.map(|v| hash(&v));
        if actual != change.old_digest && actual != change.new_digest {
            let path = change.path.clone();
            stage.state = ProjectStageState::RecoveryConflict;
            stage_log(&stage)?;
            return Err(conflict(format!("文件出现外部修改：{path}")));
        }
    }
    let changes: Vec<_> = match action {
        ProjectRecoveryAction::Continue => stage.preview.changes.clone(),
        ProjectRecoveryAction::Rollback => stage.preview.changes.iter().rev().cloned().collect(),
    };
    for change in changes {
        let desired = match action {
            ProjectRecoveryAction::Continue => &change.new_content,
            ProjectRecoveryAction::Rollback => &change.old_content,
        };
        let expected = match action {
            ProjectRecoveryAction::Continue => &change.old_digest,
            ProjectRecoveryAction::Rollback => &change.new_digest,
        };
        let desired_digest = desired.as_ref().map(|s| hash(s.as_bytes()));
        let current = read_optional(&root, &change.path)?.map(|v| hash(&v));
        if current == desired_digest {
            continue;
        }
        if current != *expected {
            stage.state = ProjectStageState::RecoveryConflict;
            stage_log(&stage)?;
            return Err(conflict(format!("发布时文件发生变化：{}", change.path)));
        }
        let path = safe_path(&root, &change.path, desired.is_some())?;
        if let Some(text) = desired {
            atomic_write(&path, text.as_bytes())?;
        } else {
            fs::remove_file(path)?;
        }
        stage.applied.push(change.path);
        stage_log(&stage)?;
    }
    stage.state = ProjectStageState::Published;
    stage_log(&stage)?;
    Ok(stage)
}

/// 提交前须先完成 stage 持久登记；同一实现也支持显式恢复。
pub fn commit(staging_root: &Path, id: Uuid) -> Result<ProjectStage> {
    recover(staging_root, id, ProjectRecoveryAction::Continue)
}

/// 导出仅包含 manifest 白名单中的内容，绝不覆盖现有目标。
pub fn export_archive(snapshot: &ProjectSnapshot, destination: &Path) -> Result<()> {
    let root = check_root(Path::new(&snapshot.root))?;
    let _lock = project_lock(&root)?;
    let current = open(&root)?;
    if current.digests != snapshot.digests {
        return Err(AppError::new("project_conflict", "项目文件已被外部修改"));
    }
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = check_root(parent)?;
    let name = destination
        .file_name()
        .ok_or_else(|| invalid("归档目标无效"))?;
    let destination = parent.join(name);
    let temp = parent.join(format!(".datapier-{}.zip", Uuid::new_v4()));
    let result = (|| -> Result<()> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .unix_permissions(0o600);
        zip.start_file(MANIFEST, options).map_err(zip_error)?;
        zip.write_all(&read_optional(&root, MANIFEST)?.ok_or_else(|| invalid("缺少 manifest"))?)?;
        for (path, content) in &current.files {
            zip.start_file(path, options).map_err(zip_error)?;
            zip.write_all(content.as_bytes())?;
        }
        let file = zip.finish().map_err(zip_error)?;
        file.sync_all()?;
        // hard_link 提供跨平台不覆盖发布，两个名字始终处于同一目录。
        fs::hard_link(&temp, &destination)?;
        fs::remove_file(&temp)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// 解包并完整校验到 UUID stage；Engine 必须登记返回身份后再发布。
pub fn prepare_import(
    archive: &Path,
    destination: &Path,
    staging_root: &Path,
) -> Result<ProjectImportStage> {
    prepare_import_in(
        archive,
        destination,
        staging_root,
        Uuid::new_v4(),
        false,
        |_| Ok(()),
    )
}

/// 仅供 Engine 已登记并绑定目录身份的导入事务使用。
pub fn prepare_import_in(
    archive: &Path,
    destination: &Path,
    staging_root: &Path,
    id: Uuid,
    existing: bool,
    before_payload: impl FnOnce(&ProjectImportStage) -> Result<()>,
) -> Result<ProjectImportStage> {
    let metadata = fs::symlink_metadata(archive)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(invalid("归档必须是普通文件"));
    }
    let mut zip = ZipArchive::new(File::open(archive)?).map_err(zip_error)?;
    if zip.is_empty() || zip.len() > MAX_FILES {
        return Err(invalid("归档文件数超限"));
    }
    if zip.has_duplicate_names() {
        return Err(invalid("归档包含重复路径"));
    }
    zip.validate_header_consistency().map_err(zip_error)?;
    let mut entries = BTreeMap::<String, String>::new();
    let mut folded = BTreeSet::new();
    let mut total = 0usize;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(zip_error)?;
        let name = entry.name().map_err(zip_error)?.into_owned();
        validate_path(&name)?;
        if !folded.insert(path_key(&name)) {
            return Err(invalid("归档路径大小写冲突"));
        }
        if entry.encrypted() || entry.is_dir() || entry.compression() != CompressionMethod::Stored {
            return Err(invalid("归档只支持未加密的 Stored 普通文件"));
        }
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o100000 {
                return Err(invalid("归档包含符号链接或特殊文件"));
            }
        }
        let limit = if name == MANIFEST {
            MAX_MANIFEST
        } else {
            MAX_FILE
        };
        let mut bytes = Vec::new();
        entry
            .by_ref()
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(invalid("归档单个文件超过限制"));
        }
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| invalid("归档大小溢出"))?;
        if total > MAX_TOTAL {
            return Err(invalid("归档总大小超过限制"));
        }
        entries.insert(name, utf8(bytes)?);
    }
    let manifest_text = entries
        .remove(MANIFEST)
        .ok_or_else(|| invalid("归档缺少 manifest"))?;
    let manifest: ProjectManifest = serde_json::from_str(&manifest_text)?;
    validate(&manifest, &entries)?;
    let parent = destination
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = check_root(parent)?;
    let name = destination
        .file_name()
        .ok_or_else(|| invalid("导入目标无效"))?;
    let destination = parent.join(name);
    if fs::symlink_metadata(&destination).is_ok() {
        return Err(invalid("导入目标已存在"));
    }
    let base = check_root(staging_root)?;
    let dir = base.join(id.to_string());
    if !existing {
        fs::create_dir(&dir)?;
    }
    let meta = fs::symlink_metadata(&dir)?;
    if meta.file_type().is_symlink() || !meta.is_dir() || fs::read_dir(&dir)?.next().is_some() {
        return Err(conflict("stage 目录不是登记的空目录"));
    }
    let mut digests: BTreeMap<String, String> = entries
        .iter()
        .map(|(path, text)| (path.clone(), hash(text.as_bytes())))
        .collect();
    digests.insert(MANIFEST.into(), hash(manifest_text.as_bytes()));
    let stage = ProjectImportStage {
        id,
        project_id: manifest.id,
        stage_dir: dir.to_string_lossy().into_owned(),
        destination: destination.to_string_lossy().into_owned(),
        revision: 1,
        digests,
        candidates: Vec::new(),
    };
    // 在第一个 payload 前固定白名单和摘要，崩溃后仍可核对已有副本。
    atomic_write(&dir.join("import.json"), &serde_json::to_vec(&stage)?)?;
    before_payload(&stage)?;
    let project = dir.join("project");
    fs::create_dir(&project)?;
    #[cfg(unix)]
    File::open(&dir)?.sync_all()?;
    for (path, text) in &entries {
        let path = safe_path(&project, path, true)?;
        atomic_write(&path, text.as_bytes())?;
    }
    atomic_write(&project.join(MANIFEST), manifest_text.as_bytes())?;
    let snapshot = open(&project)?;
    if snapshot.digests != stage.digests {
        return Err(conflict("导入 stage 摘要不一致"));
    }
    Ok(stage)
}

/// 读取可领取的导入 stage，校验目录身份和完整文件摘要。
pub fn read_import_stage(staging_root: &Path, id: Uuid) -> Result<ProjectImportStage> {
    let stage = read_import_journal(staging_root, id)?;
    let snapshot = open(&Path::new(&stage.stage_dir).join("project"))?;
    if snapshot.manifest.id != stage.project_id || snapshot.digests != stage.digests {
        return Err(conflict("导入 stage 文件发生变化"));
    }
    Ok(stage)
}

/// 读取导入意图，不要求 payload 已完成；调用方须逐项核对后才能清理。
pub fn read_import_journal(staging_root: &Path, id: Uuid) -> Result<ProjectImportStage> {
    let root = check_root(staging_root)?;
    let dir = root.join(id.to_string());
    let meta = fs::symlink_metadata(&dir)?;
    if meta.file_type().is_symlink() || !meta.is_dir() {
        return Err(invalid("导入 stage 目录无效"));
    }
    let log = dir.join("import.json");
    let meta = fs::symlink_metadata(&log)?;
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Err(invalid("导入 stage 日志无效"));
    }
    let stage: ProjectImportStage = serde_json::from_slice(&read_limited(&log, MAX_MANIFEST)?)?;
    if stage.id != id || Path::new(&stage.stage_dir) != dir {
        return Err(invalid("导入 stage 身份不匹配"));
    }
    for path in stage.digests.keys() {
        validate_path(path)?;
    }
    Ok(stage)
}

/// 登记后发布到不存在的目录；失败时保留完整 stage 供人工找回。
pub fn commit_import(staging_root: &Path, id: Uuid) -> Result<ProjectSnapshot> {
    let stage = read_import_stage(staging_root, id)?;
    commit_import_to(staging_root, id, Path::new(&stage.destination))
}

/// 明确选择另一不存在目录，找回先前中断的导入；不修改原目标。
pub fn commit_import_to(
    staging_root: &Path,
    id: Uuid,
    destination: &Path,
) -> Result<ProjectSnapshot> {
    let mut stage = read_import_stage(staging_root, id)?;
    let staged = open(&Path::new(&stage.stage_dir).join("project"))?;
    let parent = destination
        .parent()
        .ok_or_else(|| invalid("导入目标无父目录"))?;
    let parent = check_root(parent)?;
    let destination = parent.join(
        destination
            .file_name()
            .ok_or_else(|| invalid("导入目标无效"))?,
    );
    // 候选与目标同父目录，跨文件系统也可使用原子 no-replace rename。
    // 创建候选前先记录完整身份，失败时保留原 stage 和候选供领取检查。
    let candidate = parent.join(format!(".datapier-import-{id}-{}", Uuid::new_v4()));
    stage
        .candidates
        .push(candidate.to_string_lossy().into_owned());
    atomic_write(
        &Path::new(&stage.stage_dir).join("import.json"),
        &serde_json::to_vec(&stage)?,
    )?;
    fs::create_dir(&candidate)?;
    for (path, text) in &staged.files {
        let path = safe_path(&candidate, path, true)?;
        atomic_write(&path, text.as_bytes())?;
    }
    let manifest = required(
        Path::new(&stage.stage_dir).join("project").as_path(),
        MANIFEST,
    )?;
    atomic_write(&candidate.join(MANIFEST), manifest.as_bytes())?;
    let ready = open(&candidate)?;
    if ready.digests != stage.digests || ready.manifest.id != stage.project_id {
        return Err(conflict("导入候选目录发生变化"));
    }
    platform::rename_noreplace(&candidate, &destination)?;
    open(&destination)
}

/// 从已打开的单个查询或配置按 UTF-8 字节边界读取，单次最多 256 KiB。
pub fn read_file_chunk<'a>(
    snapshot: &'a ProjectSnapshot,
    path: &str,
    offset: usize,
    limit: usize,
) -> Result<(&'a str, Option<usize>)> {
    let text = snapshot
        .files
        .get(path)
        .ok_or_else(|| invalid("项目中没有此文件"))?;
    text_chunk(text, offset, limit)
}

fn text_chunk(text: &str, offset: usize, limit: usize) -> Result<(&str, Option<usize>)> {
    if offset > text.len() || !text.is_char_boundary(offset) || !(4..=256 * 1024).contains(&limit) {
        return Err(invalid("文本读取范围无效"));
    }
    let mut end = offset.saturating_add(limit).min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok((&text[offset..end], (end < text.len()).then_some(end)))
}

/// 借用差异页，调用方可只序列化路径、摘要和字节长度，不复制正文。
pub fn preview_page(
    preview: &ProjectSavePreview,
    offset: usize,
    limit: usize,
) -> Result<&[ProjectFileChange]> {
    if offset > preview.changes.len() || limit == 0 || limit > 100 {
        return Err(invalid("预览分页范围无效"));
    }
    Ok(&preview.changes[offset..offset.saturating_add(limit).min(preview.changes.len())])
}

/// 对指定差异的旧版或新版正文分块读取；None 表示该版本不存在。
pub fn preview_content_chunk(
    preview: &ProjectSavePreview,
    index: usize,
    new_content: bool,
    offset: usize,
    limit: usize,
) -> Result<Option<(&str, Option<usize>)>> {
    let change = preview
        .changes
        .get(index)
        .ok_or_else(|| invalid("预览索引无效"))?;
    let content = if new_content {
        &change.new_content
    } else {
        &change.old_content
    };
    content
        .as_deref()
        .map(|s| text_chunk(s, offset, limit))
        .transpose()
}

/// 枚举已经写入身份日志的归档导入 stage，供 Engine 领取和恢复。
pub fn list_import_stages(staging_root: &Path) -> Result<Vec<ProjectImportStage>> {
    let root = check_root(staging_root)?;
    let mut stages = Vec::new();
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(id) = Uuid::parse_str(&name) else {
            continue;
        };
        if entry.path().join("import.json").exists() {
            stages.push(read_import_journal(&root, id)?);
        }
    }
    stages.sort_by_key(|stage| stage.id);
    Ok(stages)
}

/// 显式清理已领取的保存 stage；任何未知、替换或修改的文件都会阻止清理。
pub fn cleanup_stage(staging_root: &Path, id: Uuid) -> Result<()> {
    let stage = read_stage(staging_root, id)?;
    let dir = Path::new(&stage.identity.stage_dir);
    let mut allowed = BTreeMap::new();
    for (index, change) in stage.preview.changes.iter().enumerate() {
        if let Some(digest) = &change.old_digest {
            allowed.insert(format!("{index}.old"), digest.clone());
        }
        if let Some(digest) = &change.new_digest {
            allowed.insert(format!("{index}.new"), digest.clone());
        }
    }
    let mut files = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| conflict("stage 含未知文件"))?;
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.file_type().is_symlink() || !meta.is_file() {
            return Err(conflict("stage 文件已被替换"));
        }
        if name == "stage.json" {
            continue;
        }
        let expected = allowed
            .get(&name)
            .ok_or_else(|| conflict("stage 含未知文件"))?;
        if hash(&read_limited(&entry.path(), MAX_FILE)?) != *expected {
            return Err(conflict("stage 文件已被修改"));
        }
        files.push(entry.path());
    }
    for path in files {
        fs::remove_file(path)?;
    }
    fs::remove_file(dir.join("stage.json"))?;
    fs::remove_dir(dir)?;
    Ok(())
}
