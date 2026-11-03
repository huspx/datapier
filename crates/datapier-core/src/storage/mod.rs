//! SQLite 配置与结果缓存、外置字段文件和系统凭据访问。
//! 结果行的追加、分页投影和释放按结果 ID 串行化；字段引用不保活所属结果。
//! 凭据不写入 SQLite；原生读取使用有界独立线程，写入和删除在线程池完成。
mod archery;
mod migration;
mod profiles;
mod results;
mod tasks;
mod values;

use crate::model::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::TryStreamExt;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use sqlx::{
    Connection, Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
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
    archery_secrets: Arc<RwLock<HashMap<Uuid, (String, CredentialMode)>>>,
    archery_totp_secrets: Arc<RwLock<HashMap<Uuid, (String, CredentialMode)>>>,
    result_gates: Arc<StdMutex<HashMap<Uuid, ResultGate>>>,
    profile_gate: Arc<Mutex<()>>,
}

struct ResultGate {
    lock: Arc<Mutex<()>>,
    users: usize,
}

struct ResultGateRegistration {
    id: Uuid,
    gates: Arc<StdMutex<HashMap<Uuid, ResultGate>>>,
}

impl Drop for ResultGateRegistration {
    fn drop(&mut self) {
        let mut gates = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(gate) = gates.get_mut(&self.id) {
            gate.users -= 1;
            if gate.users == 0 {
                gates.remove(&self.id);
            }
        }
    }
}

struct ResultGuard {
    // 字段按声明顺序释放，先解锁再移除最后一个登记。
    _guard: tokio::sync::OwnedMutexGuard<()>,
    _registration: ResultGateRegistration,
}

impl Store {
    /// 打开或创建数据目录与数据库，并安装或检查当前 schema。
    /// 调用方负责确保目录权限；不完整或较新 schema 及 I/O 错误会使打开失败。
    pub async fn open(root: &Path) -> Result<Self> {
        tokio::fs::create_dir_all(root.join("values")).await?;
        let root = tokio::fs::canonicalize(root).await?;
        let pool = open_database(&root.join("datapier.sqlite3")).await?;
        let cache = open_database(&root.join("cache.sqlite3")).await?;
        migration::preflight_config(&pool).await?;
        preflight(&cache, true).await?;
        migrate(
            &cache,
            include_str!("../../migrations/0001_cache.sql"),
            true,
        )
        .await?;
        migration::migrate_config(&pool).await?;
        let store = Self {
            pool,
            cache,
            root: Arc::new(root),
            archery_secrets: Arc::new(RwLock::new(HashMap::new())),
            archery_totp_secrets: Arc::new(RwLock::new(HashMap::new())),
            secrets: Arc::new(RwLock::new(HashMap::new())),
            result_gates: Arc::new(StdMutex::new(HashMap::new())),
            profile_gate: Arc::new(Mutex::new(())),
        };
        store.profiles().await?;
        if !sqlx::query("PRAGMA foreign_key_check")
            .fetch_all(&store.pool)
            .await?
            .is_empty()
        {
            return Err(AppError::new("storage_version", "配置关联无效"));
        }
        Ok(store)
    }

    /// 读取应用全局设置；无效的持久化值会返回错误。
    pub async fn settings(&self) -> Result<AppSettings> {
        let payload: String = sqlx::query_scalar("SELECT payload FROM app_settings WHERE id=1")
            .fetch_one(&self.pool)
            .await?;
        Ok(serde_json::from_str(&payload)?)
    }

    /// 原子替换应用全局设置。
    pub async fn set_settings(&self, settings: &AppSettings) -> Result<()> {
        sqlx::query("UPDATE app_settings SET payload=? WHERE id=1")
            .bind(serde_json::to_string(settings)?)
            .execute(&self.pool)
            .await?;
        Ok(())
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
async fn preflight(pool: &SqlitePool, cache: bool) -> Result<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version > (if cache { 1 } else { 2 }) || version < 0 {
        return Err(AppError::new("storage_version", "数据目录来自较新版本"));
    }
    if version == 0 {
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'")
                .fetch_one(pool)
                .await?;
        if count != 0 {
            return Err(AppError::new(
                "storage_version",
                "空版本数据库含有部分 schema",
            ));
        }
        return Ok(());
    }
    let names: &[(&str, &str)] = if cache {
        &[
            ("table", "results"),
            ("table", "result_rows"),
            ("table", "value_files"),
            ("index", "results_execution"),
            ("index", "value_files_result"),
        ]
    } else {
        &[
            ("table", "profiles"),
            ("table", "workspaces"),
            ("table", "executions"),
            ("table", "app_settings"),
            ("index", "executions_workspace"),
        ]
    };
    for (kind, name) in names {
        let found: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type=? AND name=?")
                .bind(*kind)
                .bind(*name)
                .fetch_one(pool)
                .await?;
        if found != 1 {
            return Err(AppError::new("storage_version", "数据目录 schema 不完整"));
        }
    }
    let columns: &[(&str, &[&str])] = if cache {
        &[
            (
                "results",
                &[
                    "id",
                    "execution_id",
                    "columns",
                    "kind",
                    "row_count",
                    "complete",
                    "summary",
                ],
            ),
            ("result_rows", &["result_id", "row_no", "payload"]),
            ("value_files", &["id", "result_id"]),
        ]
    } else {
        &[
            ("workspaces", &["id", "revision", "payload"]),
            ("app_settings", &["id", "payload"]),
            (
                "executions",
                &[
                    "id",
                    "owner",
                    "workspace_id",
                    "method",
                    "params",
                    "state",
                    "sent",
                    "may_write",
                    "summary",
                    "error",
                    "created_at",
                    "updated_at",
                ],
            ),
        ]
    };
    for (table, fields) in columns {
        for field in *fields {
            let found: i64 =
                sqlx::query_scalar("SELECT count(*) FROM pragma_table_info(?) WHERE name=?")
                    .bind(*table)
                    .bind(*field)
                    .fetch_one(pool)
                    .await?;
            if found != 1 {
                return Err(AppError::new("storage_version", "数据目录 schema 不完整"));
            }
        }
    }
    let profile_columns = if cache {
        &[][..]
    } else if version == 2 {
        &[
            "id",
            "name",
            "payload",
            "credential_saved",
            "archery_server_id",
            "archery_instance_id",
        ][..]
    } else {
        &["id", "name", "payload", "credential_saved"][..]
    };
    for column in profile_columns {
        let found: i64 =
            sqlx::query_scalar("SELECT count(*) FROM pragma_table_info('profiles') WHERE name=?")
                .bind(*column)
                .fetch_one(pool)
                .await?;
        if found != 1 {
            return Err(AppError::new("storage_version", "连接表 schema 不完整"));
        }
    }
    if !cache && version == 1 {
        let extra: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE name IN ('archery_servers','profiles_archery_instance','profiles_archery_insert','profiles_archery_update')").fetch_one(pool).await?;
        let columns: i64 = sqlx::query_scalar("SELECT count(*) FROM pragma_table_info('profiles') WHERE name IN ('archery_server_id','archery_instance_id')").fetch_one(pool).await?;
        if extra != 0 || columns != 0 {
            return Err(AppError::new("storage_version", "配置 schema 升级不完整"));
        }
    }
    if !cache && version == 2 {
        for (kind, name) in [
            ("table", "archery_servers"),
            ("index", "profiles_archery_instance"),
            ("trigger", "profiles_archery_insert"),
            ("trigger", "profiles_archery_update"),
        ] {
            let found: i64 =
                sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type=? AND name=?")
                    .bind(kind)
                    .bind(name)
                    .fetch_one(pool)
                    .await?;
            if found != 1 {
                return Err(AppError::new("storage_version", "数据目录 schema 不完整"));
            }
        }
        for column in [
            "id",
            "name",
            "base_url",
            "username",
            "http_tls",
            "revision",
            "credential_saved",
        ] {
            let found: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM pragma_table_info('archery_servers') WHERE name=?",
            )
            .bind(column)
            .fetch_one(pool)
            .await?;
            if found != 1 {
                return Err(AppError::new("storage_version", "服务表 schema 不完整"));
            }
        }
        let execution_sql: String = sqlx::query_scalar(
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='executions'",
        )
        .fetch_one(pool)
        .await?;
        if !execution_sql.contains("wait_stopped") {
            return Err(AppError::new("storage_version", "任务状态 schema 不完整"));
        }
    }
    Ok(())
}

async fn migrate(pool: &SqlitePool, schema: &'static str, cache: bool) -> Result<()> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(pool)
        .await?;
    if version > 1 {
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
    // 基线必须包含完整对象集合，不能只凭版本号接受部分 schema。
    let required: &[(&str, &str)] = if cache {
        &[
            ("table", "results"),
            ("table", "result_rows"),
            ("table", "value_files"),
            ("index", "results_execution"),
            ("index", "value_files_result"),
        ]
    } else {
        &[
            ("table", "profiles"),
            ("table", "workspaces"),
            ("table", "executions"),
            ("table", "app_settings"),
            ("index", "executions_workspace"),
        ]
    };
    for (kind, name) in required {
        let exists: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sqlite_master WHERE type=? AND name=?")
                .bind(*kind)
                .bind(*name)
                .fetch_one(pool)
                .await?;
        if exists != 1 {
            return Err(AppError::new("storage_version", "数据目录 schema 不完整"));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/storage.rs"]
mod tests;
