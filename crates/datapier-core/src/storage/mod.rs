//! SQLite 配置、会话内结果和系统凭据访问。
//! 凭据不写入 SQLite；原生读取使用有界独立线程，写入和删除在线程池完成。
mod archery;
mod authorization;
mod edit_files;
mod migration;
mod profiles;
mod results;
mod retired_cache;
mod tasks;

use crate::model::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::{
    Connection, Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, RwLock};
use uuid::Uuid;

/// 克隆共享配置连接池、凭据与有界内存结果。
#[derive(Clone)]
pub struct Store {
    pub pool: SqlitePool,
    pub root: Arc<PathBuf>,
    // 与 Store 同寿命固定根目录身份，清理时不重新解析可能被替换的绝对路径。
    retired_cache_root: Arc<cap_std::fs::Dir>,
    secrets: Arc<RwLock<HashMap<Uuid, String>>>,
    archery_secrets: Arc<RwLock<HashMap<Uuid, (String, CredentialMode)>>>,
    archery_totp_secrets: Arc<RwLock<HashMap<Uuid, (String, CredentialMode)>>>,
    results: Arc<Mutex<results::ResultStore>>,
    profile_gate: Arc<Mutex<()>>,
}

impl Store {
    /// 打开或创建数据目录与数据库，并安装或检查当前 schema。
    /// 调用方负责确保目录权限；不完整或较新 schema 及 I/O 错误会使打开失败。
    /// 打开失败时，返回前等待已建立的连接池关闭，释放数据库文件句柄。
    pub async fn open(root: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(root).await?;
        let root = tokio::fs::canonicalize(root).await?;
        let retired_cache_root =
            cap_std::fs::Dir::open_ambient_dir(&root, cap_std::ambient_authority())?;
        let pool = open_database(&root.join("datapier.sqlite3")).await?;
        let initialized: Result<()> = async {
            migration::preflight_config(&pool).await?;
            migration::migrate_config(&pool).await?;
            Ok(())
        }
        .await;
        if let Err(error) = initialized {
            // Pool 的 drop 不等待 SQLite worker 退出，Windows 会继续占用文件。
            pool.close().await;
            return Err(error);
        }
        let store = Self {
            pool,
            root: Arc::new(root),
            retired_cache_root: Arc::new(retired_cache_root),
            archery_secrets: Arc::new(RwLock::new(HashMap::new())),
            archery_totp_secrets: Arc::new(RwLock::new(HashMap::new())),
            secrets: Arc::new(RwLock::new(HashMap::new())),
            results: Arc::new(Mutex::new(results::ResultStore::default())),
            profile_gate: Arc::new(Mutex::new(())),
        };
        let validated: Result<()> = async {
            store.profiles().await?;
            if !sqlx::query("PRAGMA foreign_key_check")
                .fetch_all(&store.pool)
                .await?
                .is_empty()
            {
                return Err(AppError::new("storage_version", "配置关联无效"));
            }
            Ok(())
        }
        .await;
        if let Err(error) = validated {
            store.pool.close().await;
            return Err(error);
        }
        Ok(store)
    }

    /// 读取应用全局设置；无效的持久化值会返回错误。
    pub async fn settings(&self) -> Result<AppSettings> {
        let (payload, revision): (String, i64) =
            sqlx::query_as("SELECT payload,revision FROM app_settings WHERE id=1")
                .fetch_one(&self.pool)
                .await?;
        let mut settings: AppSettings = serde_json::from_str(&payload)?;
        settings.revision = revision as u64;
        settings.validate()?;
        Ok(settings)
    }

    /// 原子替换应用全局设置。
    pub async fn set_settings(&self, settings: &AppSettings) -> Result<()> {
        settings.validate()?;
        let next = settings
            .revision
            .checked_add(1)
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or_else(|| AppError::invalid("设置版本溢出"))?;
        let updated =
            sqlx::query("UPDATE app_settings SET payload=?,revision=? WHERE id=1 AND revision=?")
                .bind(serde_json::to_string(settings)?)
                .bind(next as i64)
                .bind(settings.revision as i64)
                .execute(&self.pool)
                .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::conflict("设置已更新，请重新载入"));
        }
        self.cleanup_history().await?;
        Ok(())
    }

    /// 只读取工作区版本，供切换前检查；不加载可能很大的编辑快照。
    pub async fn workspace_revision(&self, id: &str) -> Result<u64> {
        let revision: Option<i64> =
            sqlx::query_scalar("SELECT revision FROM workspaces WHERE id=?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?;
        u64::try_from(revision.unwrap_or(0)).map_err(|_| AppError::invalid("工作区版本无效"))
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
            .filter(|next| *next <= i64::MAX as u64)
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

// 系统授权等待无法中断。读取不修改外部状态，独立线程避免阻止 Runtime 退出；
// 许可跟随原生调用，取消等待后也不会不断创建新的阻塞线程。
async fn read_keyring(account: String) -> Result<Vec<u8>> {
    static READS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let permit = READS
        .acquire()
        .await
        .map_err(|e| AppError::new("credentials_unavailable", e.to_string()))?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("datapier-keyring".into())
        .spawn(move || {
            let _permit = permit;
            let result = keyring::v1::Entry::new("io.datapier.credentials", &account)
                .and_then(|entry| entry.get_secret());
            let _ = sender.send(result);
        })
        .map_err(|e| AppError::new("credentials_unavailable", e.to_string()))?;
    receiver
        .await
        .map_err(|e| AppError::new("credentials_unavailable", e.to_string()))?
        .map_err(|e| {
            AppError::new(
                "credentials_unavailable",
                format!("系统凭据库不可用，请重新输入连接凭据：{e}"),
            )
        })
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

#[cfg(test)]
#[path = "../../tests/unit/storage.rs"]
mod tests;
