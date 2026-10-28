//! SQLite 配置与结果缓存、外置字段文件和系统凭据访问。
//! 结果行的追加、分页投影和释放按结果 ID 串行化；字段引用不保活所属结果。
//! 凭据不写入 SQLite，keyring 调用在线程池执行。

use crate::model::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::TryStreamExt;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{Mutex, RwLock},
};
use uuid::Uuid;

/// 共享的持久化句柄；克隆共享连接池、会话内凭据和结果操作锁。
#[derive(Clone)]
pub struct Store {
    pub pool: SqlitePool,
    pub cache: SqlitePool,
    pub root: Arc<PathBuf>,
    secrets: Arc<RwLock<HashMap<Uuid, String>>>,
    result_gates: Arc<Mutex<HashMap<Uuid, Arc<Mutex<()>>>>>,
    profile_gate: Arc<Mutex<()>>,
}

impl Store {
    /// 打开或创建数据目录与数据库，并执行受支持版本的迁移。
    /// 调用方负责确保目录权限；较新 schema 或 I/O 错误会使打开失败。
    pub async fn open(root: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(root.join("values")).await?;
        let root = tokio::fs::canonicalize(root).await?;
        let pool = open_database(&root.join("datapier.sqlite3")).await?;
        let cache = open_database(&root.join("cache.sqlite3")).await?;
        migrate(
            &pool,
            include_str!("../../migrations/0001_config.sql"),
            false,
        )
        .await?;
        migrate(
            &cache,
            include_str!("../../migrations/0001_cache.sql"),
            true,
        )
        .await?;
        Ok(Self {
            pool,
            cache,
            root: Arc::new(root),
            secrets: Arc::new(RwLock::new(HashMap::new())),
            result_gates: Arc::new(Mutex::new(HashMap::new())),
            profile_gate: Arc::new(Mutex::new(())),
        })
    }

    /// 按名称和 ID 返回保存的连接配置，不包含凭据。
    pub async fn profiles(&self) -> Result<Vec<Profile>> {
        let rows = sqlx::query("SELECT payload FROM profiles ORDER BY name,id")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| serde_json::from_str(r.get::<&str, _>(0)).map_err(Into::into))
            .collect()
    }

    /// 保存连接配置和可选凭据；返回是否已标记为保存到系统凭据库。
    /// 旧凭据无法删除时拒绝降级，保留原有追踪状态。
    pub async fn save_profile(
        &self,
        profile: &Profile,
        secret: Option<String>,
        persist: bool,
    ) -> Result<bool> {
        profile.validate()?;
        // 调用方取消等待不能中断 keyring 与 SQLite 间的所有权交接。
        let store = self.clone();
        let profile = profile.clone();
        tokio::spawn(async move { store.save_profile_inner(&profile, secret, persist).await })
            .await
            .map_err(|e| AppError::new("credentials", e.to_string()))?
    }

    async fn save_profile_inner(
        &self,
        profile: &Profile,
        secret: Option<String>,
        persist: bool,
    ) -> Result<bool> {
        let _guard = self.profile_gate.lock().await;
        let old: Option<i64> =
            sqlx::query_scalar("SELECT credential_saved FROM profiles WHERE id=?")
                .bind(profile.id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let mut saved = old == Some(1);
        let mut tracked = saved;
        if let Some(secret) = secret {
            if persist {
                // 在写 keyring 之前登记可能存在的条目，避免后续 SQLite 错误丢失清理依据。
                sqlx::query("INSERT INTO profiles(id,name,payload,credential_saved) VALUES(?,?,?,1) ON CONFLICT(id) DO UPDATE SET credential_saved=1")
                    .bind(profile.id.to_string()).bind(&profile.name).bind(serde_json::to_string(profile)?)
                    .execute(&self.pool).await?;
                tracked = true;
                let account = self.credential_account(profile.id);
                let to_save = secret.clone();
                saved = tokio::task::spawn_blocking(move || {
                    keyring::v1::Entry::new("io.datapier.credentials", &account)?
                        .set_secret(to_save.as_bytes())
                })
                .await
                .map_err(|e| AppError::new("credentials", e.to_string()))?
                .is_ok();
            } else {
                saved = false;
            }
            if !saved && old == Some(1) {
                self.delete_credential(profile.id).await?;
                tracked = false;
            }
            self.secrets.write().await.insert(profile.id, secret);
        }
        sqlx::query("INSERT INTO profiles(id,name,payload,credential_saved) VALUES(?,?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,payload=excluded.payload,credential_saved=excluded.credential_saved")
            .bind(profile.id.to_string()).bind(&profile.name).bind(serde_json::to_string(profile)?)
            .bind(i64::from(tracked || saved)).execute(&self.pool).await?;
        Ok(saved)
    }

    /// 优先读取会话内凭据；已标记持久化时从系统凭据库读取并缓存。
    /// 凭据库不可用返回错误，未保存凭据返回 `None`。
    pub async fn secret(&self, id: Uuid) -> Result<Option<String>> {
        if let Some(secret) = self.secrets.read().await.get(&id).cloned() {
            return Ok(Some(secret));
        }
        let saved: Option<i64> =
            sqlx::query_scalar("SELECT credential_saved FROM profiles WHERE id=?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        if saved != Some(1) {
            return Ok(None);
        }
        let account = self.credential_account(id);
        let secret = tokio::task::spawn_blocking(move || {
            keyring::v1::Entry::new("io.datapier.credentials", &account)?.get_secret()
        })
        .await
        .map_err(|e| AppError::new("credentials", e.to_string()))?
        .map_err(|e| {
            AppError::new(
                "credentials_unavailable",
                format!("系统凭据库不可用，请重新输入连接凭据：{e}"),
            )
        })?;
        let secret =
            String::from_utf8(secret).map_err(|e| AppError::new("credentials", e.to_string()))?;
        self.secrets.write().await.insert(id, secret.clone());
        Ok(Some(secret))
    }

    /// 删除配置和会话内凭据；标记为持久化的凭据删除失败时保留配置。
    pub async fn delete_profile(&self, id: Uuid) -> Result<()> {
        let store = self.clone();
        tokio::spawn(async move { store.delete_profile_inner(id).await })
            .await
            .map_err(|e| AppError::new("credentials", e.to_string()))?
    }

    async fn delete_profile_inner(&self, id: Uuid) -> Result<()> {
        let _guard = self.profile_gate.lock().await;
        let saved: Option<i64> =
            sqlx::query_scalar("SELECT credential_saved FROM profiles WHERE id=?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        if saved == Some(1) {
            self.delete_credential(id).await?;
        }
        sqlx::query("DELETE FROM profiles WHERE id=?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        self.secrets.write().await.remove(&id);
        Ok(())
    }

    async fn delete_credential(&self, id: Uuid) -> Result<()> {
        let account = self.credential_account(id);
        let result = tokio::task::spawn_blocking(move || {
            keyring::v1::Entry::new("io.datapier.credentials", &account)?.delete_credential()
        })
        .await
        .map_err(|e| AppError::new("credentials", e.to_string()))?;
        match result {
            Ok(()) | Err(keyring::v1::Error::NoEntry) => Ok(()),
            Err(e) => Err(AppError::new(
                "credentials",
                format!("凭据删除失败，连接配置保留：{e}"),
            )),
        }
    }

    fn credential_account(&self, id: Uuid) -> String {
        let digest = Sha256::digest(self.root.to_string_lossy().as_bytes());
        format!("{:x}:{id}", digest)
    }

    /// 加载工作区快照及版本；不存在时返回版本零和空状态。
    pub async fn workspace_load(&self, id: &str) -> Result<Json> {
        let row = sqlx::query("SELECT revision,payload FROM workspaces WHERE id=?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        match row {
            Some(row) => Ok(
                json!({"revision":row.get::<i64,_>(0),"state":serde_json::from_str::<Json>(row.get::<&str,_>(1))?}),
            ),
            None => Ok(json!({"revision":0,"state":{}})),
        }
    }

    /// 按预期版本保存快照并返回新版本；并发更新导致版本不匹配时返回冲突。
    pub async fn workspace_save(
        &self,
        id: &str,
        expected_revision: u64,
        state: Json,
    ) -> Result<u64> {
        let next = expected_revision
            .checked_add(1)
            .ok_or_else(|| AppError::invalid("工作区版本溢出"))?;
        let mut tx = self.pool.begin().await?;
        let result = if expected_revision == 0 {
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
        if result.rows_affected() != 1 {
            return Err(AppError::conflict(
                "工作区已在其他界面更新，请重新载入后合并",
            ));
        }
        tx.commit().await?;
        Ok(next)
    }

    /// 创建排队中的任务记录并返回其状态，调用方提供的参数会持久化。
    pub async fn create_task(
        &self,
        owner: Uuid,
        workspace: &str,
        method: &str,
        params: Json,
        may_write: bool,
    ) -> Result<TaskRecord> {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO executions(id,owner,workspace_id,method,params,state,sent,may_write,summary) VALUES(?,?,?,?,?,'queued',0,?,'null')")
            .bind(id.to_string()).bind(owner.to_string()).bind(workspace).bind(method).bind(params.to_string())
            .bind(i64::from(may_write)).execute(&self.pool).await?;
        self.task(id).await
    }

    /// 读取任务及关联的缓存结果 ID；任务不存在时返回 `not_found`。
    pub async fn task(&self, id: Uuid) -> Result<TaskRecord> {
        let row = sqlx::query("SELECT owner,workspace_id,method,state,sent,may_write,summary,error FROM executions WHERE id=?")
            .bind(id.to_string()).fetch_optional(&self.pool).await?.ok_or_else(|| AppError::new("not_found", "任务不存在"))?;
        let owner = Uuid::parse_str(row.get::<&str, _>(0))
            .map_err(|e| AppError::new("storage", e.to_string()))?;
        let error: Option<String> = row.get(7);
        Ok(TaskRecord {
            id,
            owner,
            workspace_id: row.get(1),
            method: row.get(2),
            state: row.get(3),
            sent: row.get::<i64, _>(4) != 0,
            may_write: row.get::<i64, _>(5) != 0,
            results: self.result_ids(id).await?,
            summary: serde_json::from_str(row.get::<&str, _>(6))?,
            error: error.map(|e| serde_json::from_str(&e)).transpose()?,
        })
    }

    /// 返回工作区最近的最多 500 条任务。
    pub async fn tasks(&self, workspace: &str) -> Result<Vec<TaskRecord>> {
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM executions WHERE workspace_id=? ORDER BY created_at DESC,rowid DESC LIMIT 500")
            .bind(workspace).fetch_all(&self.pool).await?;
        let mut tasks = Vec::with_capacity(ids.len());
        for id in ids {
            tasks.push(self.task(parse_id(&id)?).await?);
        }
        Ok(tasks)
    }

    /// 持久化任务状态、摘要和可选错误。
    pub async fn update_task(
        &self,
        id: Uuid,
        state: &str,
        summary: Json,
        error: Option<AppError>,
    ) -> Result<()> {
        sqlx::query("UPDATE executions SET state=?,summary=?,error=?,updated_at=unixepoch('subsec') WHERE id=?")
            .bind(state).bind(summary.to_string()).bind(error.map(|e| serde_json::to_string(&e)).transpose()?)
            .bind(id.to_string()).execute(&self.pool).await?;
        Ok(())
    }
    /// 持久化发送阶段；`may_write` 只会从假变真。
    pub async fn mark_sent(&self, id: Uuid, may_write: bool) -> Result<()> {
        sqlx::query("UPDATE executions SET sent=1,may_write=MAX(may_write,?),updated_at=unixepoch('subsec') WHERE id=?")
            .bind(i64::from(may_write)).bind(id.to_string()).execute(&self.pool).await?;
        Ok(())
    }
    /// 将中断任务恢复为取消或待确认；已发送且可能写入的任务需人工核对。
    pub async fn recover_tasks(&self) -> Result<()> {
        let unknown = serde_json::to_string(&AppError::unknown(
            "Engine 已中断，数据库写入结果需人工核对",
        ))?;
        let interrupted = serde_json::to_string(&AppError::new(
            "interrupted",
            "Engine 已中断，原会话不可恢复",
        ))?;
        sqlx::query("UPDATE executions SET state=CASE WHEN sent=1 AND may_write=1 THEN 'outcome_unknown' ELSE 'cancelled' END,error=CASE WHEN sent=1 AND may_write=1 THEN ? ELSE ? END WHERE state IN ('queued','running','cancelling')")
            .bind(unknown).bind(interrupted).execute(&self.pool).await?;
        Ok(())
    }
    /// 返回工作区最近最多 500 条执行历史。
    pub async fn history(&self, workspace: &str) -> Result<Json> {
        let rows = sqlx::query("SELECT id,params,state,created_at FROM executions WHERE workspace_id=? AND method IN ('execution.start','execution.run') ORDER BY created_at DESC,rowid DESC LIMIT 500")
            .bind(workspace).fetch_all(&self.pool).await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let params: Json = serde_json::from_str(row.get::<&str, _>(1))?;
            items.push(json!({"id":row.get::<&str,_>(0),"text":params.get("text"),"state":row.get::<&str,_>(2),"created_at":row.get::<f64,_>(3)}));
        }
        Ok(json!({"items":items,"limit":500}))
    }

    /// 创建未完成的缓存结果，列定义与执行 ID 随结果保存。
    pub async fn start_result(
        &self,
        execution_id: Uuid,
        columns: Vec<Column>,
        kind: &str,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO results(id,execution_id,columns,kind,row_count,complete,summary) VALUES(?,?,?,?,0,0,'null')")
            .bind(id.to_string()).bind(execution_id.to_string()).bind(serde_json::to_string(&columns)?)
            .bind(kind).execute(&self.cache).await?;
        Ok(id)
    }
    // 文件和 SQLite 行的修改共用结果锁，避免追加、投影和释放彼此交错。
    async fn result_guard(&self, id: Uuid) -> tokio::sync::OwnedMutexGuard<()> {
        let gate = self
            .result_gates
            .lock()
            .await
            .entry(id)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();
        gate.lock_owned().await
    }
    /// 追加一批行；大值可能先写入外置文件，再将行写入缓存数据库。
    pub async fn append_rows(&self, id: Uuid, rows: Vec<Vec<Value>>) -> Result<()> {
        // 脱离调用方的等待后仍持有结果锁，直到文件写入和行提交结束。
        let store = self.clone();
        tokio::spawn(async move { store.append_rows_inner(id, rows).await })
            .await
            .map_err(|e| AppError::new("storage", e.to_string()))?
    }

    async fn append_rows_inner(&self, id: Uuid, rows: Vec<Vec<Value>>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let _guard = self.result_guard(id).await;
        self.result_meta(id).await?;
        let mut payloads = Vec::with_capacity(rows.len());
        for mut row in rows {
            let mut payload = serde_json::to_vec(&row)?;
            if payload.len() > 64 * 1024 {
                drop(payload);
                for value in &mut row {
                    let raw = serde_json::to_vec(value)?;
                    if raw.len() > 64 * 1024 {
                        *value = self.externalize(id, value, raw, 4096).await?;
                    }
                }
                payload = serde_json::to_vec(&row)?;
            }
            payloads.push(payload);
        }
        let mut tx = self.cache.begin().await?;
        let start: i64 = sqlx::query_scalar("SELECT row_count FROM results WHERE id=?")
            .bind(id.to_string())
            .fetch_one(&mut *tx)
            .await?;
        let count = payloads.len();
        for (i, payload) in payloads.into_iter().enumerate() {
            sqlx::query("INSERT INTO result_rows(result_id,row_no,payload) VALUES(?,?,?)")
                .bind(id.to_string())
                .bind(start + i as i64)
                .bind(payload)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE results SET row_count=row_count+? WHERE id=?")
            .bind(count as i64)
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    // 保存类型化值的 JSON 字节，而非展示文本，供后续按需完整还原。
    async fn externalize(
        &self,
        result_id: Uuid,
        value: &Value,
        raw: Vec<u8>,
        preview_size: usize,
    ) -> Result<Value> {
        self.externalize_file(result_id, Uuid::new_v4(), value, raw, preview_size)
            .await
    }

    async fn externalize_file(
        &self,
        result_id: Uuid,
        id: Uuid,
        value: &Value,
        raw: Vec<u8>,
        preview_size: usize,
    ) -> Result<Value> {
        let original_type = value_type(value);
        let preview = String::from_utf8_lossy(&raw[..raw.len().min(preview_size)]).into_owned();
        // 先持久化归属：写入失败或 future 被取消时仍可由 release_result 重试清理。
        sqlx::query("INSERT INTO value_files(id,result_id) VALUES(?,?)")
            .bind(id.to_string())
            .bind(result_id.to_string())
            .execute(&self.cache)
            .await?;
        // 文件异步操作由持锁任务等待 flush，取消外层调用不会提前释放结果锁。
        let mut file =
            tokio::fs::File::create(self.root.join("values").join(id.to_string())).await?;
        file.write_all(&raw).await?;
        file.flush().await?;
        Ok(Value::Large {
            id,
            original_type: original_type.into(),
            bytes: raw.len() as u64,
            preview,
        })
    }
    /// 标记结果完整并保存摘要。
    pub async fn finish_result(&self, id: Uuid, summary: Json) -> Result<()> {
        let _guard = self.result_guard(id).await;
        sqlx::query("UPDATE results SET complete=1,summary=? WHERE id=?")
            .bind(summary.to_string())
            .bind(id.to_string())
            .execute(&self.cache)
            .await?;
        Ok(())
    }
    /// 读取结果元数据；结果已释放或不存在时返回 `not_found`。
    pub async fn result_meta(&self, id: Uuid) -> Result<ResultMeta> {
        let row = sqlx::query(
            "SELECT execution_id,columns,kind,row_count,complete,summary FROM results WHERE id=?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.cache)
        .await?
        .ok_or_else(|| AppError::new("not_found", "结果已释放或不存在"))?;
        Ok(ResultMeta {
            id,
            execution_id: parse_id(row.get::<&str, _>(0))?,
            columns: serde_json::from_str(row.get::<&str, _>(1))?,
            kind: row.get(2),
            row_count: row.get::<i64, _>(3) as u64,
            complete: row.get::<i64, _>(4) != 0,
            summary: serde_json::from_str(row.get::<&str, _>(5))?,
        })
    }
    /// 按创建顺序读取执行关联的结果 ID。
    pub async fn result_ids(&self, task: Uuid) -> Result<Vec<Uuid>> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM results WHERE execution_id=? ORDER BY rowid")
                .bind(task.to_string())
                .fetch_all(&self.cache)
                .await?;
        ids.iter().map(|id| parse_id(id)).collect()
    }
    /// 按起始行取页，行数限制在 1..=1000，字节预算限制在 1 KiB..=1 MiB。
    /// 预算计入元数据；单行过宽时可将字段外置，仍无法容纳则返回错误。
    pub async fn result_page(
        &self,
        id: Uuid,
        start: u64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<ResultPage> {
        let store = self.clone();
        tokio::spawn(async move {
            store
                .result_page_inner(id, start, max_rows, max_bytes)
                .await
        })
        .await
        .map_err(|e| AppError::new("storage", e.to_string()))?
    }

    async fn result_page_inner(
        &self,
        id: Uuid,
        start: u64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<ResultPage> {
        let _guard = self.result_guard(id).await;
        let result = self.result_meta(id).await?;
        let max_rows = max_rows.clamp(1, 1000);
        let max_bytes = max_bytes.clamp(1024, 1024 * 1024);
        let mut rows = Vec::new();
        let mut bytes = serde_json::to_vec(&result)?.len() + 128;
        if bytes > max_bytes {
            return Err(AppError::new(
                "result_too_wide",
                "列定义超过页大小，请增加页预算或减少查询列数",
            ));
        }
        let mut first_over_budget = None;
        let mut stream = sqlx::query("SELECT payload FROM result_rows WHERE result_id=? AND row_no>=? ORDER BY row_no LIMIT ?")
            .bind(id.to_string()).bind(start as i64).bind(max_rows as i64).fetch(&self.cache);
        while let Some(row) = stream.try_next().await? {
            let payload: Vec<u8> = row.get(0);
            if bytes + payload.len() > max_bytes {
                if rows.is_empty() {
                    first_over_budget = Some(payload);
                }
                break;
            }
            bytes += payload.len();
            rows.push(serde_json::from_slice(&payload)?);
        }
        // 外置文件登记需要另一条连接，必须先释放读取连接，避免并发分页耗尽连接池。
        drop(stream);
        if let Some(payload) = first_over_budget {
            let mut values: Vec<Value> = serde_json::from_slice(&payload)?;
            // 单行超过预算时先去掉预览，再将能缩小编码尺寸的值转为引用。
            for value in &mut values {
                if let Value::Large { preview, .. } = value {
                    preview.clear();
                }
            }
            let mut size = serde_json::to_vec(&values)?.len();
            if bytes + size > max_bytes {
                for value in &mut values {
                    if matches!(value, Value::Large { .. }) {
                        continue;
                    }
                    let raw = serde_json::to_vec(value)?;
                    let reference = Value::Large {
                        id: Uuid::nil(),
                        original_type: value_type(value).into(),
                        bytes: raw.len() as u64,
                        preview: String::new(),
                    };
                    let reference_size = serde_json::to_vec(&reference)?.len();
                    if reference_size >= raw.len() {
                        continue;
                    }
                    size = size - raw.len() + reference_size;
                    *value = self.externalize(id, value, raw, 0).await?;
                    if bytes + size <= max_bytes {
                        break;
                    }
                }
            }
            sqlx::query("UPDATE result_rows SET payload=? WHERE result_id=? AND row_no=?")
                .bind(serde_json::to_vec(&values)?)
                .bind(id.to_string())
                .bind(start as i64)
                .execute(&self.cache)
                .await?;
            if bytes + size > max_bytes {
                return Err(AppError::new(
                    "result_too_wide",
                    "单行引用和列定义超过页大小，请减少查询列数",
                ));
            }
            bytes += size;
            rows.push(values);
            // 投影首行后重新读取其余行，保持原有的行数和字节预算。
            if max_rows > 1 {
                let mut stream = sqlx::query("SELECT payload FROM result_rows WHERE result_id=? AND row_no>=? ORDER BY row_no LIMIT ?")
                    .bind(id.to_string()).bind((start + 1) as i64).bind((max_rows - 1) as i64).fetch(&self.cache);
                while let Some(row) = stream.try_next().await? {
                    let payload: Vec<u8> = row.get(0);
                    if bytes + payload.len() > max_bytes {
                        break;
                    }
                    bytes += payload.len();
                    rows.push(serde_json::from_slice(&payload)?);
                }
            }
        }
        let end = start + rows.len() as u64;
        let next = if end < result.row_count {
            Some(end)
        } else {
            None
        };
        Ok(ResultPage {
            result,
            rows,
            start,
            next,
        })
    }
    /// 读取外置字段的完整类型化值；引用文件不存在或损坏时返回错误。
    ///
    /// 调用方需保证所属结果尚未释放；字段 ID 不是独立于结果生命周期的持久标识。
    pub async fn read_value(&self, id: Uuid) -> Result<Value> {
        self.ensure_value_available(id).await?;
        let raw = tokio::fs::read(self.root.join("values").join(id.to_string())).await?;
        Ok(serde_json::from_slice(&raw)?)
    }
    /// 按字节读取外置字段的 JSON 编码，返回 Base64 分块和下一偏移。
    /// 单块上限为 256 KiB；偏移超过文件长度时返回输入错误。
    ///
    /// 引用不阻止结果释放，读取与释放并发时可能返回文件已不存在的 I/O 错误。
    pub async fn large_value(&self, id: Uuid, offset: u64, max_bytes: usize) -> Result<Json> {
        self.ensure_value_available(id).await?;
        let mut file = tokio::fs::File::open(self.root.join("values").join(id.to_string())).await?;
        let total = file.metadata().await?.len();
        if offset > total {
            return Err(AppError::invalid("字段分块偏移超出范围"));
        }
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let length = ((total - offset) as usize).min(max_bytes.clamp(1, 256 * 1024));
        let mut buffer = vec![0; length];
        file.read_exact(&mut buffer).await?;
        let end = offset + length as u64;
        Ok(
            json!({"bytes_b64":STANDARD.encode(buffer),"offset":offset,"next":if end<total {Some(end)} else {None},"total":total}),
        )
    }
    /// 逻辑释放结果后删除外置文件；失败时保留文件清理记录，可用同一 ID 重试。
    pub async fn release_result(&self, id: Uuid) -> Result<()> {
        let _guard = self.result_guard(id).await;
        let mut tx = self.cache.begin().await?;
        // schema 1 的文件没有登记；在删除行之前将其纳入同一事务。
        let mut after = -1_i64;
        loop {
            let rows = sqlx::query("SELECT row_no,payload FROM result_rows WHERE result_id=? AND row_no>? ORDER BY row_no LIMIT 128")
                .bind(id.to_string()).bind(after).fetch_all(&mut *tx).await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                after = row.get(0);
                let values: Vec<Value> = serde_json::from_slice(row.get::<&[u8], _>(1))?;
                for value in values {
                    if let Value::Large { id: value_id, .. } = value {
                        sqlx::query("INSERT OR IGNORE INTO value_files(id,result_id) VALUES(?,?)")
                            .bind(value_id.to_string())
                            .bind(id.to_string())
                            .execute(&mut *tx)
                            .await?;
                    }
                }
            }
        }
        sqlx::query("DELETE FROM result_rows WHERE result_id=?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM results WHERE id=?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        loop {
            let file: Option<String> = sqlx::query_scalar(
                "SELECT id FROM value_files WHERE result_id=? ORDER BY id LIMIT 1",
            )
            .bind(id.to_string())
            .fetch_optional(&self.cache)
            .await?;
            let Some(file) = file else {
                break;
            };
            match tokio::fs::remove_file(self.root.join("values").join(&file)).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            sqlx::query("DELETE FROM value_files WHERE id=?")
                .bind(file)
                .execute(&self.cache)
                .await?;
        }
        // 仅在没有等待者持有锁引用时移除门，避免并发操作取得不同的锁。
        let mut gates = self.result_gates.lock().await;
        if gates
            .get(&id)
            .is_some_and(|gate| Arc::strong_count(gate) == 2)
        {
            gates.remove(&id);
        }
        Ok(())
    }
    async fn ensure_value_available(&self, id: Uuid) -> Result<()> {
        let released: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM value_files v WHERE v.id=? AND NOT EXISTS (SELECT 1 FROM results r WHERE r.id=v.result_id))")
            .bind(id.to_string()).fetch_one(&self.cache).await?;
        if released {
            return Err(AppError::new("not_found", "结果已释放或不存在"));
        }
        Ok(())
    }
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Missing => "missing",
        Value::Bool(_) => "bool",
        Value::Integer(_) => "integer",
        Value::Unsigned(_) => "unsigned",
        Value::Decimal(_) => "decimal",
        Value::Float(_) => "float",
        Value::Text(_) => "text",
        Value::Bytes(_) => "bytes",
        Value::Date(_) => "date",
        Value::Time(_) => "time",
        Value::DateTime(_) => "date_time",
        Value::Json(_) => "json",
        Value::Array(_) => "array",
        Value::Map(_) => "map",
        Value::Large { .. } => "large",
    }
}

fn parse_id(text: &str) -> Result<Uuid> {
    Uuid::parse_str(text).map_err(|e| AppError::new("storage", e.to_string()))
}
async fn open_database(path: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .foreign_keys(true)
        .busy_timeout(Duration::from_secs(10));
    Ok(SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await?)
}
async fn migrate(pool: &SqlitePool, schema: &'static str, cache: bool) -> Result<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version > if cache { 2 } else { 1 } {
        return Err(AppError::new(
            "storage_version",
            "数据目录来自较新版本，拒绝降级打开",
        ));
    }
    if version == 0 {
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(schema).execute(&mut *tx).await?;
        sqlx::query("PRAGMA user_version=1")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    if cache && version < 2 {
        let mut tx = pool.begin().await?;
        sqlx::raw_sql(include_str!("../../migrations/0002_cache.sql"))
            .execute(&mut *tx)
            .await?;
        sqlx::query("PRAGMA user_version=2")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
