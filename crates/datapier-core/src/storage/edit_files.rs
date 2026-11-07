//! 已登记的外部编辑文件和不可变工作区草稿文件。
//!
//! Engine 必须在调用领取、读取、清理前检查活动客户端所有权；此处负责持久化
//! revision、工作区引用和文件系统边界，不恢复数据库编辑基线。
use super::*;
use crate::model::editing::{
    EditFilePurpose, EditFileRegistration, EditFileState, EditorFileRecord,
};
use std::io::{Read, Write};
mod draft;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
fn purpose_name(p: EditFilePurpose) -> &'static str {
    match p {
        EditFilePurpose::External => "external",
        EditFilePurpose::Draft => "draft",
    }
}
fn state_name(s: EditFileState) -> &'static str {
    match s {
        EditFileState::Preparing => "preparing",
        EditFileState::Ready => "ready",
        EditFileState::Opened => "opened",
        EditFileState::Returned => "returned",
        EditFileState::Orphaned => "orphaned",
        EditFileState::CleanupPending => "cleanup_pending",
    }
}
fn parse_enum<T: serde::de::DeserializeOwned>(s: &str) -> Result<T> {
    serde_json::from_value(Json::String(s.to_owned()))
        .map_err(|e| AppError::new("storage_version", e.to_string()))
}
pub(super) fn decode_record(row: &sqlx::sqlite::SqliteRow) -> Result<EditorFileRecord> {
    let id: String = row.try_get("id")?;
    let revision: i64 = row.try_get("revision")?;
    let draft_revision: i64 = row.try_get("draft_revision")?;
    let record = EditorFileRecord {
        id: Uuid::parse_str(&id).map_err(|e| AppError::new("storage_version", e.to_string()))?,
        workspace_id: row.try_get("workspace_id")?,
        profile_id: row.try_get("profile_id")?,
        relative_dir: row.try_get("relative_dir")?,
        purpose: parse_enum(row.try_get("purpose")?)?,
        target: serde_json::from_str(row.try_get("target_json")?)?,
        state: parse_enum(row.try_get("state")?)?,
        format: row.try_get("format")?,
        baseline_digest: row.try_get("baseline_digest")?,
        revision: u64::try_from(revision)
            .map_err(|_| AppError::new("storage_version", "文件版本无效"))?,
        draft_revision: u64::try_from(draft_revision)
            .map_err(|_| AppError::new("storage_version", "草稿版本无效"))?,
        created_at_ms: row.try_get("created_at_ms")?,
        updated_at_ms: row.try_get("updated_at_ms")?,
    };
    validate_record(&record)?;
    Ok(record)
}
fn validate_record(r: &EditorFileRecord) -> Result<()> {
    if r.workspace_id.is_empty()
        || r.profile_id.is_empty()
        || r.relative_dir != format!("edit-files/{}", r.id)
        || !matches!(
            r.format.as_str(),
            "txt" | "json" | "xml" | "bin" | "hex" | "intent"
        )
        || (r.format == "intent" && r.purpose != EditFilePurpose::Draft)
        || r.revision == 0
        || r.revision > i64::MAX as u64
        || r.draft_revision == 0
        || r.draft_revision > i64::MAX as u64
        || !r.target.is_object()
    {
        return Err(AppError::invalid("编辑文件登记无效"));
    }
    Ok(())
}
fn check_component(path: &Path, directory: bool) -> Result<std::fs::Metadata> {
    let meta = std::fs::symlink_metadata(path)?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return Err(AppError::new("permission_denied", "拒绝 reparse point"));
        }
    }
    if meta.file_type().is_symlink()
        || (directory && !meta.is_dir())
        || (!directory && !meta.is_file())
    {
        return Err(AppError::new(
            "permission_denied",
            "编辑路径不是普通文件或目录",
        ));
    }
    #[cfg(unix)]
    if directory {
        use std::os::unix::fs::MetadataExt;
        if meta.mode() & 0o077 != 0 {
            return Err(AppError::new("permission_denied", "编辑目录权限不安全"));
        }
    }
    Ok(meta)
}
fn ensure_dir(path: &Path) -> Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(e) => return Err(e.into()),
    }
    check_component(path, true)?;
    Ok(())
}
fn file_path(root: &Path, record: &EditorFileRecord) -> Result<PathBuf> {
    validate_record(record)?;
    let base = root.join("edit-files");
    check_component(&base, true)?;
    let dir = base.join(record.id.to_string());
    check_component(&dir, true)?;
    Ok(dir.join(format!("{}.{}", record.id, record.format)))
}
fn new_private_file(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    Ok(options.open(path)?)
}
fn read_stable_with<T>(
    path: &Path,
    limit: usize,
    read: impl FnOnce(&mut std::fs::File) -> Result<T>,
) -> Result<T> {
    let before_path = check_component(path, false)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x20000);
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(0x100);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000);
    }
    let mut file = options.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            AppError::new("file_busy", "编辑文件读取时正在保存")
        } else {
            e.into()
        }
    })?;
    let before = file.metadata()?;
    if !before.is_file() {
        return Err(AppError::new("permission_denied", "编辑文件不是普通文件"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if before.file_attributes() & 0x400 != 0 {
            return Err(AppError::new("permission_denied", "拒绝 reparse point"));
        }
    }
    if before.len() > limit as u64 {
        return Err(AppError::new("resource_limit", "编辑文件超过字节上限"));
    }
    let value = read(&mut file)?;
    let after = file.metadata()?;
    let after_path = match check_component(path, false) {
        Ok(meta) => meta,
        Err(error)
            if error.code == "io"
                && std::fs::symlink_metadata(path)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Err(AppError::new("file_busy", "编辑文件读取时正在保存"));
        }
        Err(error) => return Err(error),
    };
    if after.len() > limit as u64 {
        return Err(AppError::new("resource_limit", "编辑文件超过字节上限"));
    }
    let changed = before.len() != after.len()
        || before.modified()? != after.modified()?
        || after.modified()? != after_path.modified()?
        || before_path.len() != before.len()
        || before_path.modified()? != before.modified()?;
    #[cfg(unix)]
    let changed = {
        use std::os::unix::fs::MetadataExt;
        changed
            || before.dev() != before_path.dev()
            || before.ino() != before_path.ino()
            || after.dev() != after_path.dev()
            || after.ino() != after_path.ino()
            || before.ctime() != after.ctime()
            || before.ctime_nsec() != after.ctime_nsec()
    };
    if changed {
        return Err(AppError::new("file_busy", "编辑文件读取时正在保存"));
    }
    Ok(value)
}
fn read_stable(path: &Path, limit: usize) -> Result<Vec<u8>> {
    read_stable_with(path, limit, |file| {
        let mut bytes = Vec::new();
        file.take((limit as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            return Err(AppError::new("resource_limit", "编辑文件超过字节上限"));
        }
        Ok(bytes)
    })
}

// 标记只在已知编辑 section 顶层解释；section 内的用户 JSON 不是登记元数据。
fn intent_marker<'a>(value: &'a Json, kind: &str) -> Result<Option<&'a Json>> {
    let Some(marker) = value.get("$datapier_intent") else {
        return Ok(None);
    };
    let outer = value
        .as_object()
        .ok_or_else(|| AppError::invalid("意图文件标记无效"))?;
    let obj = marker
        .as_object()
        .ok_or_else(|| AppError::invalid("意图文件标记无效"))?;
    if outer.len() != 1
        || obj.len() != 4
        || obj.get("version").and_then(Json::as_u64) != Some(1)
        || obj.get("kind").and_then(Json::as_str) != Some(kind)
        || obj
            .get("edit_file_id")
            .and_then(Json::as_str)
            .and_then(|id| Uuid::parse_str(id).ok())
            .is_none()
        || obj
            .get("edit_file_revision")
            .and_then(Json::as_u64)
            .is_none_or(|rev| rev == 0 || rev > i64::MAX as u64)
    {
        return Err(AppError::invalid("意图文件标记类型或版本无效"));
    }
    Ok(Some(marker))
}

/// 只将已知描述符位置解释为登记元数据。
fn references(value: &Json, out: &mut std::collections::BTreeMap<Uuid, u64>) -> Result<()> {
    let id = value
        .get("edit_file_id")
        .and_then(Json::as_str)
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| AppError::invalid("草稿文件 ID 无效"))?;
    let rev = value
        .get("edit_file_revision")
        .and_then(Json::as_u64)
        .filter(|v| *v > 0 && *v <= i64::MAX as u64)
        .ok_or_else(|| AppError::invalid("草稿文件版本无效"))?;
    if out.insert(id, rev).is_some_and(|old| old != rev) {
        return Err(AppError::invalid("草稿文件引用版本不一致"));
    }
    Ok(())
}
fn descriptor_list(value: &Json, out: &mut std::collections::BTreeMap<Uuid, u64>) -> Result<()> {
    for descriptor in value
        .as_array()
        .ok_or_else(|| AppError::invalid("草稿文件列表无效"))?
    {
        references(descriptor, out)?;
    }
    Ok(())
}
fn refs(value: &Json) -> Result<std::collections::BTreeMap<Uuid, u64>> {
    let mut result = std::collections::BTreeMap::new();
    if let Some(files) = value.get("value_drafts") {
        descriptor_list(files, &mut result)?;
    }
    if let Some(tabs) = value.get("tabs").and_then(Json::as_array) {
        for tab in tabs {
            for kind in ["schema_draft", "replace_draft"] {
                if let Some(section) = tab.get(kind)
                    && let Some(marker) = intent_marker(section, kind)?
                {
                    references(marker, &mut result)?;
                }
            }
            if let Some(pending) = tab.get("pending") {
                for key in ["edit_files", "value_drafts"] {
                    if let Some(files) = pending.get(key) {
                        descriptor_list(files, &mut result)?;
                    }
                }
                if let Some(changes) = pending.get("redis").and_then(Json::as_array) {
                    for change in changes {
                        if change.get("edit_file_id").is_some()
                            || change.get("edit_file_revision").is_some()
                        {
                            references(change, &mut result)?;
                        }
                        if let Some(identity) = change.get("identity_edit_file") {
                            references(identity, &mut result)?;
                        }
                    }
                }
            }
        }
    }
    Ok(result)
}
fn bounded_payload_refs(
    payload: &str,
    budget: usize,
) -> Result<std::collections::BTreeMap<Uuid, u64>> {
    let mut estimate = crate::model::editing::JsonMemoryEstimate::default();
    estimate.feed(payload.as_bytes());
    if payload.len().saturating_add(estimate.heap_bound()) > budget {
        return Err(AppError::new("resource_limit", "工作区保存超过内存预算"));
    }
    refs(&serde_json::from_str(payload)?)
}

async fn workspace_has_reference(
    connection: &mut sqlx::SqliteConnection,
    workspace_id: &str,
    id: Uuid,
    budget: usize,
) -> Result<bool> {
    let limit = i64::try_from(budget).unwrap_or(i64::MAX);
    let row = sqlx::query("SELECT length(CAST(payload AS BLOB)), CASE WHEN length(CAST(payload AS BLOB))<=? THEN payload END FROM workspaces WHERE id=?")
        .bind(limit).bind(workspace_id).fetch_optional(connection).await?;
    let Some(row) = row else {
        return Ok(false);
    };
    let payload: Option<&str> = row.try_get(1)?;
    let payload =
        payload.ok_or_else(|| AppError::new("resource_limit", "工作区引用检查超过内存预算"))?;
    Ok(bounded_payload_refs(payload, budget)?.contains_key(&id))
}
impl Store {
    /// 提取工作区引用，供 Engine 在持有文件锁时核对归属。
    pub fn edit_file_references(state: &Json) -> Result<std::collections::BTreeMap<Uuid, u64>> {
        refs(state)
    }

    /// 在创建任何文件前提交登记；ID 和路径只能由 Store 生成。
    pub async fn register_edit_file(
        &self,
        registration: EditFileRegistration<'_>,
    ) -> Result<EditorFileRecord> {
        let EditFileRegistration {
            workspace_id,
            profile_id,
            purpose,
            target,
            format,
            baseline_digest,
            draft_revision,
        } = registration;
        let id = Uuid::new_v4();
        let record = EditorFileRecord {
            id,
            workspace_id: workspace_id.into(),
            profile_id: profile_id.into(),
            relative_dir: format!("edit-files/{id}"),
            purpose,
            target,
            state: EditFileState::Preparing,
            format: format.into(),
            baseline_digest: baseline_digest.into(),
            draft_revision,
            revision: 1,
            created_at_ms: now_ms(),
            updated_at_ms: now_ms(),
        };
        validate_record(&record)?;
        sqlx::query("INSERT INTO value_edit_files(id,workspace_id,profile_id,relative_dir,purpose,target_json,state,format,baseline_digest,draft_revision,revision,created_at_ms,updated_at_ms) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(id.to_string()).bind(workspace_id).bind(profile_id).bind(&record.relative_dir).bind(purpose_name(purpose)).bind(record.target.to_string()).bind("preparing").bind(format).bind(baseline_digest).bind(draft_revision as i64).bind(1i64).bind(record.created_at_ms).bind(record.updated_at_ms).execute(&self.pool).await?;
        Ok(record)
    }
    /// 获取登记，配置被删除时仍可访问。
    pub async fn edit_file(&self, id: Uuid) -> Result<EditorFileRecord> {
        let row = sqlx::query("SELECT * FROM value_edit_files WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| AppError::new("not_found", "编辑文件不存在"))?;
        decode_record(&row)
    }
    /// 检查已保存工作区是否引用此文件；清理仍须在事务内重新检查。
    pub async fn edit_file_referenced(&self, id: Uuid) -> Result<bool> {
        let record = self.edit_file(id).await?;
        let budget = self.settings().await?.limits.result_memory_bytes_per_client;
        let mut connection = self.pool.acquire().await?;
        workspace_has_reference(&mut connection, &record.workspace_id, id, budget).await
    }
    /// 按登记时间和 ID 稳定分页，限制单次返回数量。
    pub async fn list_edit_files(&self, offset: u64, limit: u64) -> Result<Vec<EditorFileRecord>> {
        if limit == 0 || limit > 500 || offset > i64::MAX as u64 {
            return Err(AppError::invalid("编辑文件分页参数无效"));
        }
        let rows = sqlx::query(
            "SELECT * FROM value_edit_files ORDER BY created_at_ms,id LIMIT ? OFFSET ?",
        )
        .bind(limit as i64)
        .bind(offset as i64)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(decode_record).collect()
    }
    /// 使用 CAS 转换文件状态；不允许将已发布草稿退回 preparing。
    pub async fn transition_edit_file(
        &self,
        id: Uuid,
        expected_revision: u64,
        state: EditFileState,
    ) -> Result<EditorFileRecord> {
        let old = self.edit_file(id).await?;
        let allowed = matches!(
            (old.purpose, old.state, state),
            (
                _,
                EditFileState::Preparing,
                EditFileState::Ready | EditFileState::Orphaned
            ) | (
                EditFilePurpose::External,
                EditFileState::Ready,
                EditFileState::Opened | EditFileState::Returned | EditFileState::Orphaned
            ) | (
                EditFilePurpose::External,
                EditFileState::Opened,
                EditFileState::Returned | EditFileState::Orphaned
            ) | (
                EditFilePurpose::External,
                EditFileState::Returned,
                EditFileState::Opened | EditFileState::Orphaned
            )
        );
        if old.revision != expected_revision || !allowed {
            return Err(AppError::conflict("编辑文件状态或版本已变化"));
        }
        let result = sqlx::query("UPDATE value_edit_files SET state=?,revision=revision+1,updated_at_ms=? WHERE id=? AND revision=? AND revision<?")
            .bind(state_name(state)).bind(now_ms()).bind(id.to_string()).bind(expected_revision as i64).bind(i64::MAX).execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            return Err(AppError::conflict("编辑文件版本已变化"));
        }
        self.edit_file(id).await
    }
    /// 领取遗留记录只更新版本；调用方必须先排除活跃 owner。
    pub async fn claim_edit_file(&self, id: Uuid, revision: u64) -> Result<EditorFileRecord> {
        let budget = self.settings().await?.limits.result_memory_bytes_per_client;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let workspace: Option<String> =
            sqlx::query_scalar("SELECT workspace_id FROM value_edit_files WHERE id=?")
                .bind(id.to_string())
                .fetch_optional(&mut *tx)
                .await?;
        if let Some(workspace) = workspace
            && workspace_has_reference(&mut tx, &workspace, id, budget).await?
        {
            return Err(AppError::conflict("编辑文件仍被工作区引用"));
        }
        let result = sqlx::query("UPDATE value_edit_files SET revision=revision+1,updated_at_ms=? WHERE id=? AND revision=? AND revision<? AND state IN ('preparing','orphaned','ready','cleanup_pending')")
            .bind(now_ms()).bind(id.to_string()).bind(i64::try_from(revision).map_err(|_| AppError::invalid("版本无效"))?).bind(i64::MAX).execute(&mut *tx).await?;
        if result.rows_affected() != 1 {
            return Err(AppError::conflict("编辑文件不可领取或版本已变化"));
        }
        tx.commit().await?;
        self.edit_file(id).await
    }
    /// 发布已登记内容；draft 使用临时文件和原子重命名，只能发布一次。
    pub async fn publish_edit_file(
        &self,
        id: Uuid,
        revision: u64,
        bytes: &[u8],
        limit: usize,
    ) -> Result<EditorFileRecord> {
        if bytes.len() > limit {
            return Err(AppError::new("resource_limit", "编辑文件超过字节上限"));
        }
        let record = self.edit_file(id).await?;
        if record.revision != revision || record.state != EditFileState::Preparing {
            return Err(AppError::conflict("编辑文件已经发布或版本已变化"));
        }
        let base = self.root.join("edit-files");
        ensure_dir(&base)?;
        let dir = base.join(id.to_string());
        ensure_dir(&dir)?;
        let dest = file_path(&self.root, &record)?;
        let path = if record.purpose == EditFilePurpose::Draft {
            dir.join(format!("{id}.pending"))
        } else {
            dest.clone()
        };
        let mut file = new_private_file(&path)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if record.purpose == EditFilePurpose::Draft {
            // hard_link 以 create-new 语义发布，避免并发发布覆盖已存在的不可变草稿。
            std::fs::hard_link(&path, &dest)?;
            std::fs::remove_file(&path)?;
        }
        self.transition_edit_file(id, revision, EditFileState::Ready)
            .await
    }
    /// 读取或导出用的有界字节；读取期间检测到替换或修改返回 file_busy。
    pub async fn read_edit_file(&self, id: Uuid, revision: u64, limit: usize) -> Result<Vec<u8>> {
        let record = self.edit_file(id).await?;
        if record.revision != revision || record.state == EditFileState::Preparing {
            return Err(AppError::conflict("编辑文件版本或状态无效"));
        }
        read_stable(&file_path(&self.root, &record)?, limit)
    }
    /// 导出到用户指定的新路径；以同目录临时文件发布，已有目标不会覆盖。
    pub async fn export_edit_file(
        &self,
        id: Uuid,
        revision: u64,
        limit: usize,
        destination: &Path,
    ) -> Result<()> {
        let bytes = self.read_edit_file(id, revision, limit).await?;
        let parent = destination
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let temporary = parent.join(format!(".datapier-edit-export-{}", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut file = new_private_file(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            std::fs::hard_link(&temporary, destination)?;
            Ok(())
        })();
        match std::fs::remove_file(&temporary) {
            Ok(()) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
            Err(e) if result.is_ok() => return Err(e.into()),
            Err(_) => (),
        }
        result
    }
    /// 返回可交给外部编辑器的绝对路径；不授予数据库写入权限。
    pub async fn edit_file_path(&self, id: Uuid, revision: u64) -> Result<PathBuf> {
        let record = self.edit_file(id).await?;
        if record.purpose != EditFilePurpose::External
            || record.revision != revision
            || record.state == EditFileState::Preparing
        {
            return Err(AppError::conflict("编辑文件版本或用途无效"));
        }
        let path = file_path(&self.root, &record)?;
        check_component(&path, false)?;
        Ok(path)
    }
    /// 保存工作区及文件引用的同一事务；旧引用解除后才标记可清理。
    pub async fn workspace_save_with_edit_files(
        &self,
        id: &str,
        expected_revision: u64,
        state: &Json,
    ) -> Result<u64> {
        self.workspace_save_with_edit_files_bounded(id, expected_revision, state, usize::MAX)
            .await
    }

    async fn workspace_save_with_edit_files_bounded(
        &self,
        id: &str,
        expected_revision: u64,
        state: &Json,
        memory_budget: usize,
    ) -> Result<u64> {
        let next = expected_revision
            .checked_add(1)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or_else(|| AppError::invalid("工作区版本溢出"))?;
        let new_refs = refs(state)?;
        // 在读取旧引用前取得写锁，避免并发保存/任务写入使 DEFERRED 快照无法升级。
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        let max_payload = i64::try_from(memory_budget).unwrap_or(i64::MAX);
        let old_size: Option<i64> =
            sqlx::query_scalar("SELECT length(CAST(payload AS BLOB)) FROM workspaces WHERE id=?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        if old_size.is_some_and(|size| size > max_payload) {
            return Err(AppError::new("resource_limit", "工作区保存超过内存预算"));
        }
        let old_payload: Option<String> =
            sqlx::query_scalar("SELECT payload FROM workspaces WHERE id=?")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await?;
        let old_refs = match old_payload {
            Some(payload) => bounded_payload_refs(&payload, memory_budget)?,
            None => Default::default(),
        };
        // 工作区 CAS 和文件引用切换共用同一写事务。
        let saved = if expected_revision == 0 {
            sqlx::query("INSERT INTO workspaces(id,revision,payload) VALUES(?,?,?) ON CONFLICT(id) DO NOTHING")
                .bind(id).bind(next as i64).bind(state.to_string()).execute(&mut *tx).await?
        } else {
            sqlx::query("UPDATE workspaces SET revision=?,payload=? WHERE id=? AND revision=?")
                .bind(next as i64)
                .bind(state.to_string())
                .bind(id)
                .bind(expected_revision as i64)
                .execute(&mut *tx)
                .await?
        };
        if saved.rows_affected() != 1 {
            return Err(AppError::conflict("工作区已在其他界面更新"));
        }
        for (file_id, revision) in &new_refs {
            let exists: Option<i64> = sqlx::query_scalar("SELECT 1 FROM value_edit_files WHERE id=? AND workspace_id=? AND purpose='draft' AND state='ready' AND revision=?")
                .bind(file_id.to_string()).bind(id).bind(*revision as i64).fetch_optional(&mut *tx).await?;
            if exists.is_none() {
                return Err(AppError::conflict("草稿文件不存在、未发布或版本已变化"));
            }
        }
        for (file_id, revision) in old_refs {
            if !new_refs.contains_key(&file_id) {
                sqlx::query("UPDATE value_edit_files SET state='cleanup_pending',revision=revision+1,updated_at_ms=? WHERE id=? AND workspace_id=? AND purpose='draft' AND state='ready' AND revision=? AND revision<?")
                    .bind(now_ms()).bind(file_id.to_string()).bind(id).bind(revision as i64).bind(i64::MAX).execute(&mut *tx).await?;
            }
        }
        tx.commit().await?;
        Ok(next)
    }

    /// 清理登记的单个文件。已保存工作区引用的 draft 不可清理；失败保留 pending。
    /// 调用方必须同时排除所有活动 owner。
    pub async fn cleanup_edit_file(&self, id: Uuid, revision: u64) -> Result<()> {
        let budget = self.settings().await?.limits.result_memory_bytes_per_client;
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query("UPDATE value_edit_files SET state='cleanup_pending',revision=revision+1,updated_at_ms=? WHERE id=? AND revision=? AND revision<?")
            .bind(now_ms()).bind(id.to_string()).bind(i64::try_from(revision).map_err(|_| AppError::invalid("版本无效"))?).bind(i64::MAX).execute(&mut *tx).await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::conflict("编辑文件版本已变化"));
        }
        let row = sqlx::query("SELECT * FROM value_edit_files WHERE id=?")
            .bind(id.to_string())
            .fetch_one(&mut *tx)
            .await?;
        let record = decode_record(&row)?;
        if record.purpose == EditFilePurpose::Draft
            && workspace_has_reference(&mut tx, &record.workspace_id, id, budget).await?
        {
            return Err(AppError::conflict("草稿文件仍被保存的工作区引用"));
        }
        tx.commit().await?;
        let base = self.root.join("edit-files");
        match check_component(&base, true) {
            Ok(_) => {
                let dir = base.join(id.to_string());
                match check_component(&dir, true) {
                    Ok(_) => {
                        for path in [
                            dir.join(format!("{id}.{}", record.format)),
                            dir.join(format!("{id}.pending")),
                        ] {
                            match check_component(&path, false) {
                                Ok(_) => std::fs::remove_file(path)?,
                                Err(e)
                                    if e.code == "io"
                                        && !path.exists()
                                        && std::fs::symlink_metadata(&path).is_err_and(|x| {
                                            x.kind() == std::io::ErrorKind::NotFound
                                        }) => {}
                                Err(e) => return Err(e),
                            }
                        }
                        std::fs::remove_dir(dir)?;
                    }
                    Err(e)
                        if e.code == "io"
                            && std::fs::symlink_metadata(&dir)
                                .is_err_and(|x| x.kind() == std::io::ErrorKind::NotFound) => {}
                    Err(e) => return Err(e),
                }
            }
            Err(e)
                if e.code == "io"
                    && std::fs::symlink_metadata(&base)
                        .is_err_and(|x| x.kind() == std::io::ErrorKind::NotFound) => {}
            Err(e) => return Err(e),
        }
        let deleted = sqlx::query(
            "DELETE FROM value_edit_files WHERE id=? AND state='cleanup_pending' AND revision=?",
        )
        .bind(id.to_string())
        .bind(record.revision as i64)
        .execute(&self.pool)
        .await?;
        if deleted.rows_affected() != 1 {
            return Err(AppError::conflict("文件清理时登记已变化"));
        }
        Ok(())
    }

    /// 启动时回收失去活动 owner 的文件；已保存的草稿引用始终保留。
    pub async fn recover_edit_files(&self) -> Result<()> {
        let budget = self.settings().await?.limits.result_memory_bytes_per_client;
        let mut tx = self.pool.begin_with("BEGIN IMMEDIATE").await?;
        sqlx::query("UPDATE value_edit_files SET state='orphaned',revision=revision+1,updated_at_ms=? WHERE purpose='external' AND state IN ('preparing','ready','opened','returned') AND revision<?")
            .bind(now_ms()).bind(i64::MAX).execute(&mut *tx).await?;
        // 登记、写入 pending、发布和工作区引用之间都可能崩溃。
        // 无引用的登记必须可领取，清理时会同时删除最终文件及 pending。
        let mut cursor = String::new();
        loop {
            let candidate: Option<(String, String)> = sqlx::query_as("SELECT id,workspace_id FROM value_edit_files WHERE purpose='draft' AND state IN ('preparing','ready','orphaned') AND revision<? AND id>? ORDER BY id LIMIT 1")
                .bind(i64::MAX).bind(&cursor).fetch_optional(&mut *tx).await?;
            let Some((id, workspace)) = candidate else {
                break;
            };
            let parsed = Uuid::parse_str(&id)
                .map_err(|error| AppError::new("storage_version", error.to_string()))?;
            let referenced =
                match workspace_has_reference(&mut tx, &workspace, parsed, budget).await {
                    Ok(value) => value,
                    Err(error) if error.code == "resource_limit" => true,
                    Err(error) => return Err(error),
                };
            if !referenced {
                sqlx::query("UPDATE value_edit_files SET state='cleanup_pending',revision=revision+1,updated_at_ms=? WHERE id=?")
                    .bind(now_ms()).bind(&id).execute(&mut *tx).await?;
            }
            cursor = id;
        }
        tx.commit().await?;
        Ok(())
    }
}
