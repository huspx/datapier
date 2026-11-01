//! Redis 单节点会话：按 DB 浏览、执行命令、乐观编辑和分批导入导出。
//! key、field 和 member 保留原始字节；扫描结果允许并发变化，不是快照。

mod batch_delete;
mod command;
mod edit;
mod transfer;

use crate::{ExecutionSink, model::*};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use redis::{
    Client, ConnectionAddr, IntoConnectionInfo, RedisConnectionInfo, aio::MultiplexedConnection,
};
use serde_json::{Value as Json, json};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

type Conn = MultiplexedConnection;
// WATCH 状态属于物理连接；令牌持有该连接直到提交、丢弃或会话关闭。
struct Edit {
    conn: Conn,
    key: Vec<u8>,
    kind: String,
    expires_at_ms: Option<i64>,
}
// 导入先写入有期限的临时 key，完成后才尝试发布到目标 key。
// 同一脚本先检查对象内的随机标记，再操作该对象；单独的标记 key 无法证明数据未被替换。
const IMPORT_OWNED: &str = r#"
local function owned(m)
    local k = ARGV[1]
    if redis.call('TYPE', KEYS[1]).ok ~= k then return false end
    if k == 'string' then
        return string.sub(redis.call('GET', KEYS[1]), 1, string.len(m)) == m
    elseif k == 'hash' then
        return redis.call('HGET', KEYS[1], m) == m
    elseif k == 'list' then
        return redis.call('LINDEX', KEYS[1], 0) == m
    elseif k == 'set' then
        return redis.call('SISMEMBER', KEYS[1], m) == 1
    else
        return redis.call('ZSCORE', KEYS[1], m) == '0'
    end
end
"#;
struct Import {
    key: Vec<u8>,
    temp: Vec<u8>,
    marker: Vec<u8>,
    pending_marker: Option<Vec<u8>>,
    kind: String,
    expires: Option<i64>,
    wrote: bool,
    valid: bool,
    count: u64,
}
fn import_cleanup_args(import: &Import) -> Vec<Vec<u8>> {
    let script = format!(
        "{IMPORT_OWNED}\nif owned(ARGV[2]) or (ARGV[3] ~= '' and owned(ARGV[3])) then return redis.call('DEL', KEYS[1]) end\nreturn 0"
    );
    vec![
        b"EVAL".to_vec(),
        script.into_bytes(),
        b"1".to_vec(),
        import.temp.clone(),
        import.kind.as_bytes().to_vec(),
        import.marker.clone(),
        import.pending_marker.clone().unwrap_or_default(),
    ]
}
// SQLite 缓存跨页保存去重结果及已输出位置，避免把全量 key 留在内存。
struct KeyScan {
    cursor: u64,
    complete: bool,
    pattern: Option<String>,
    kind: Option<String>,
    cache: sqlx::SqlitePool,
}
/// 持有一个 Redis DB 的普通连接，以及隔离的命令、编辑和扫描状态。
///
/// 命令和每个编辑分别建立物理连接：multiplexed 连接的 clone 仍共享
/// 服务端连接状态，无法隔离 SELECT、事务和 WATCH。
pub struct RedisSession {
    profile: Profile,
    client: Client,
    secret: Option<String>,
    conn: Conn,
    command_conn: Option<Conn>,
    command_dead: bool,
    edits: HashMap<Uuid, Edit>,
    batch_edits: HashMap<Uuid, batch_delete::BatchEdit>,
    imports: HashMap<Uuid, Import>,
    scans: HashMap<Uuid, KeyScan>,
}
fn bytes(v: &Value) -> Result<Vec<u8>> {
    match v {
        Value::Bytes(s) => STANDARD
            .decode(s)
            .map_err(|e| AppError::invalid(e.to_string())),
        Value::Text(s) => Ok(s.as_bytes().to_vec()),
        _ => Err(AppError::invalid("Redis 值必须为文本或字节")),
    }
}
fn val(b: Vec<u8>) -> Value {
    Value::Bytes(STANDARD.encode(b))
}
fn key(target: &ResourceTarget) -> Result<Vec<u8>> {
    match &target.key_b64 {
        Some(s) => STANDARD
            .decode(s)
            .map_err(|e| AppError::invalid(e.to_string())),
        None => Ok(target.name.as_bytes().to_vec()),
    }
}
fn target(p: &Json, profile: &Profile) -> Result<ResourceTarget> {
    let t: ResourceTarget = param(p, "target")?;
    if let Some(db) = &t.database
        && db.parse::<u32>().ok() != Some(profile.redis_db)
    {
        return Err(AppError::invalid("目标数据库与连接不一致"));
    }
    Ok(t)
}
fn uuid(p: &Json) -> Result<Uuid> {
    param(p, "token")
}
fn field(p: &Json, name: &str) -> Result<Vec<u8>> {
    bytes(&param::<Value>(p, name)?)
}
async fn query<T: redis::FromRedisValue>(conn: &mut Conn, args: &[Vec<u8>]) -> Result<T> {
    let mut cmd =
        redis::cmd(std::str::from_utf8(&args[0]).map_err(|e| AppError::invalid(e.to_string()))?);
    for a in &args[1..] {
        cmd.arg(a);
    }
    Ok(cmd.query_async(conn).await?)
}
fn args(parts: &[&str]) -> Vec<Vec<u8>> {
    parts.iter().map(|s| s.as_bytes().to_vec()).collect()
}
fn scan_insert(
    group: &str,
    keys: &[Vec<u8>],
    kinds: &[String],
    expected: Option<&str>,
) -> Option<sqlx::QueryBuilder<sqlx::Sqlite>> {
    let mut entries = keys
        .iter()
        .zip(kinds)
        .filter(|(_, kind)| {
            kind.as_str() != "none" && expected.is_none_or(|expected| expected == kind.as_str())
        })
        .peekable();
    entries.peek()?;
    let mut insert =
        sqlx::QueryBuilder::new("INSERT OR IGNORE INTO redis_key_scan(scan_id,key,kind) ");
    insert.push_values(entries, |mut row, (key, kind)| {
        row.push_bind(group.to_owned())
            .push_bind(key.clone())
            .push_bind(kind.clone());
    });
    Some(insert)
}
// 写命令发送后发生 I/O 错误时，无法判断服务端是否执行，不能自动重放。
fn error_unknown(e: redis::RedisError) -> AppError {
    if e.is_io_error() {
        AppError::unknown(e.to_string())
    } else {
        e.into()
    }
}
async fn send_write<T: redis::FromRedisValue>(
    conn: &mut Conn,
    a: &[Vec<u8>],
    sink: &ExecutionSink,
) -> Result<T> {
    sink.mark_sent(true).await?;
    let mut cmd =
        redis::cmd(std::str::from_utf8(&a[0]).map_err(|e| AppError::invalid(e.to_string()))?);
    for x in &a[1..] {
        cmd.arg(x);
    }
    cmd.query_async(conn).await.map_err(error_unknown)
}

impl RedisSession {
    /// 校验配置并建立普通连接；认证、TLS、证书读取或连接失败时返回错误。
    pub async fn connect(profile: Profile, secret: Option<String>) -> Result<Self> {
        profile.validate()?;
        let addr = if profile.tls.enabled {
            ConnectionAddr::TcpTls {
                host: profile.host.clone(),
                port: profile.port,
                insecure: false,
                tls_params: None,
            }
        } else {
            ConnectionAddr::Tcp(profile.host.clone(), profile.port)
        };
        let settings = RedisConnectionInfo::default().set_skip_set_lib_name();
        let info = "redis://localhost/"
            .into_connection_info()?
            .set_addr(addr)
            .set_redis_settings(settings);
        let client = if profile.tls.enabled {
            let root_cert = profile
                .tls
                .ca_path
                .as_ref()
                .map(std::fs::read)
                .transpose()?;
            let client_tls = match (&profile.tls.client_cert_path, &profile.tls.client_key_path) {
                (Some(c), Some(k)) => Some(redis::ClientTlsConfig {
                    client_cert: std::fs::read(c)?,
                    client_key: std::fs::read(k)?,
                }),
                (None, None) => None,
                _ => return Err(AppError::invalid("客户端证书和密钥必须同时提供")),
            };
            Client::build_with_tls(
                info,
                redis::TlsCertificates {
                    client_tls,
                    root_cert,
                },
            )?
        } else {
            if profile.tls.ca_path.is_some()
                || profile.tls.client_cert_path.is_some()
                || profile.tls.client_key_path.is_some()
            {
                return Err(AppError::invalid("TLS 未启用"));
            }
            Client::open(info)?
        };
        let conn = connect_database(&client, &profile, secret.as_deref()).await?;
        Ok(Self {
            profile,
            client,
            secret,
            conn,
            command_conn: None,
            command_dead: false,
            edits: HashMap::new(),
            batch_edits: HashMap::new(),
            imports: HashMap::new(),
            scans: HashMap::new(),
        })
    }
    /// 返回会话创建时的连接配置。
    pub fn profile(&self) -> &Profile {
        &self.profile
    }
    /// 释放编辑和命令连接，清理扫描缓存及导入临时 key，再关闭普通连接。
    /// 清理失败时仍尝试其余清理，并返回第一个错误。
    pub async fn close(&mut self) -> Result<()> {
        self.edits.clear();
        self.batch_edits.clear();
        self.command_conn = None;
        self.command_dead = true;
        let mut failure = None;
        for (id, scan) in std::mem::take(&mut self.scans) {
            if let Err(e) = sqlx::query("DELETE FROM redis_key_scan WHERE scan_id=?")
                .bind(id.to_string())
                .execute(&scan.cache)
                .await
                && failure.is_none()
            {
                failure = Some(e.into());
            }
        }
        let imports = std::mem::take(&mut self.imports);
        for (_, i) in imports {
            if let Err(e) = query::<i64>(&mut self.conn, &import_cleanup_args(&i)).await
                && failure.is_none()
            {
                failure = Some(e);
            }
        }
        let quit: Result<String> = query(&mut self.conn, &args(&["QUIT"])).await;
        if let Some(e) = failure {
            return Err(e);
        }
        quit?;
        Ok(())
    }
    /// 分派 Redis 操作；写入受配置的访问模式约束，取消不会保证撤销已发送命令。
    pub async fn call(
        &mut self,
        method: &str,
        p: Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        ensure_not_cancelled(cancel)?;
        match method {
            "connection.info" => {
                let info: String = query(&mut self.conn, &args(&["INFO", "server"])).await?;
                let version = info
                    .lines()
                    .find_map(|l| l.strip_prefix("redis_version:"))
                    .unwrap_or("")
                    .trim();
                Ok(json!({"server_version":version,"database":self.profile.redis_db}))
            }
            "resource.list" => self.list(&p, sink, cancel).await,
            "resource.describe" => {
                let t = target(&p, &self.profile)?;
                self.describe(t).await
            }
            "resource.browse" => {
                let t = target(&p, &self.profile)?;
                let k = key(&t)?;
                let type_args = vec![b"TYPE".to_vec(), k.clone()];
                record_browse_command(sink, &type_args).await?;
                let kind: String = query(&mut self.conn, &type_args).await?;
                let ttl_args = vec![b"PTTL".to_vec(), k.clone()];
                record_browse_command(sink, &ttl_args).await?;
                let ttl: i64 = query(&mut self.conn, &ttl_args).await?;
                let id = browse(&mut self.conn, &k, &kind, sink, cancel).await?;
                Ok(json!({"results":[id],"type":kind,"ttl_ms":ttl}))
            }
            "change.open" if p.get("targets").is_some() => self.open_batch_delete(&p, cancel).await,
            "change.open" => self.open_edit(&p, sink, cancel).await,
            "change.discard" => {
                let id = uuid(&p)?;
                if self.edits.remove(&id).is_none() && self.batch_edits.remove(&id).is_none() {
                    return Err(AppError::invalid("编辑令牌已失效"));
                }
                Ok(json!({"discarded":true}))
            }
            "change.preview" if self.is_batch_request(&p)? => self.preview_batch_delete(&p, cancel),
            "change.preview" => self.preview(&p, cancel),
            "change.submit" if self.is_batch_request(&p)? => {
                self.submit_batch_delete(&p, sink, cancel).await
            }
            "change.submit" => self.submit(&p, sink, cancel).await,
            "execution.run" => self.run(&p, sink, cancel).await,
            "export.meta" => self.export_meta(&p).await,
            "import.begin" => self.import_begin(&p, sink).await,
            "import.append" => self.import_append(&p, sink).await,
            "import.finish" => self.import_finish(&p, sink).await,
            "import.discard" => self.import_discard(&p, sink).await,
            _ => Err(AppError::invalid(format!("未知 Redis 方法 {method}"))),
        }
    }
    async fn list(
        &mut self,
        p: &Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        use futures_util::TryStreamExt;
        use sqlx::Row;
        let cursor = p.get("cursor").filter(|v| !v.is_null());
        let fresh = cursor.is_none();
        let id = if let Some(v) = cursor {
            v.as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(|| AppError::invalid("无效扫描令牌"))?
        } else {
            sqlx::query("CREATE TABLE IF NOT EXISTS redis_key_scan (seq INTEGER PRIMARY KEY AUTOINCREMENT, scan_id TEXT NOT NULL, key BLOB NOT NULL, kind TEXT NOT NULL, emitted INTEGER NOT NULL DEFAULT 0, UNIQUE(scan_id,key))").execute(&sink.store.cache).await?;
            sqlx::query("CREATE INDEX IF NOT EXISTS redis_key_scan_pending ON redis_key_scan(scan_id,emitted,seq)").execute(&sink.store.cache).await?;
            let id = Uuid::new_v4();
            self.scans.insert(
                id,
                KeyScan {
                    cursor: 0,
                    complete: false,
                    pattern: p.get("pattern").and_then(Json::as_str).map(str::to_owned),
                    kind: p.get("type").and_then(Json::as_str).map(str::to_owned),
                    cache: sink.store.cache.clone(),
                },
            );
            id
        };
        let limit = p
            .get("limit")
            .and_then(Json::as_u64)
            .unwrap_or(200)
            .clamp(1, 1000) as usize;
        let result: Result<ResourcePage> = async {
            let scan = self.scans.get_mut(&id).ok_or_else(|| AppError::invalid("扫描令牌已失效"))?;
            if p.get("pattern").and_then(Json::as_str).is_some_and(|s| Some(s) != scan.pattern.as_deref())
                || p.get("type").and_then(Json::as_str).is_some_and(|s| Some(s) != scan.kind.as_deref()) {
                return Err(AppError::invalid("扫描条件与令牌不匹配"));
            }
            let group = id.to_string();
            let mut resources = Vec::with_capacity(limit);
            let mut ttl_query = redis::pipe();
            let mut last = 0i64;
            let mut size = 0usize;
            // SCAN 可返回空批次和重复项；只有服务端 cursor 为零才算扫描结束。
            // 缓存中的唯一约束去重，分页分别受条数和序列化字节数限制。
            loop {
                ensure_not_cancelled(cancel)?;
                let mut cached = sqlx::query("SELECT seq,key,kind FROM redis_key_scan WHERE scan_id=? AND emitted=0 AND seq>? ORDER BY seq LIMIT ?")
                    .bind(&group).bind(last).bind((limit - resources.len()) as i64).fetch(&scan.cache);
                let mut page_full = false;
                while let Some(row) = cached.try_next().await? {
                    let seq: i64 = row.try_get(0)?;
                    let key: &[u8] = row.try_get(1)?;
                    let resource = Resource {
                        name: String::from_utf8_lossy(key).into_owned(), kind: row.try_get(2)?,
                        database: Some(self.profile.redis_db.to_string()), key_b64: Some(STANDARD.encode(key)), data: Json::Null,
                    };
                    // TTL 是当前页的读取值；预留最坏 i64 JSON 长度，仍遵守分页字节上限。
                    let bytes = serde_json::to_vec(&resource)?.len() + 1 + 32;
                    if size + bytes > 1024 * 1024 - 1024 {
                        if resources.is_empty() { return Err(AppError::new("result_too_large", "单个 Redis key 超过分页字节限制")); }
                        page_full = true;
                        break;
                    }
                    size += bytes;
                    last = seq;
                    ttl_query.cmd("PTTL").arg(key);
                    resources.push(resource);
                    if resources.len() == limit { page_full = true; break; }
                }
                drop(cached);
                if page_full || scan.complete { break; }
                let mut command = redis::cmd("SCAN");
                command.arg(scan.cursor).arg("COUNT").arg(limit);
                if let Some(pattern) = &scan.pattern { command.arg("MATCH").arg(pattern); }
                if let Some(kind) = &scan.kind { command.arg("TYPE").arg(kind); }
                let (next, keys): (u64, Vec<Vec<u8>>) = command.query_async(&mut self.conn).await?;
                if !keys.is_empty() {
                    let mut tx = scan.cache.begin().await?;
                    // 每批最多 768 个绑定参数，也兼容 SQLite 的 999 参数上限。
                    for keys in keys.chunks(256) {
                        ensure_not_cancelled(cancel)?;
                        let mut types = redis::pipe();
                        for key in keys { types.cmd("TYPE").arg(key); }
                        let kinds: Vec<String> = types.query_async(&mut self.conn).await?;
                        if kinds.len() != keys.len() { return Err(AppError::new("database", "Redis 类型回复数量不匹配")); }
                        if let Some(mut insert) = scan_insert(&group, keys, &kinds, scan.kind.as_deref()) {
                            insert.build().persistent(false).execute(&mut *tx).await?;
                        }
                    }
                    tx.commit().await?;
                }
                scan.cursor = next;
                scan.complete = next == 0;
            }
            if !resources.is_empty() {
                ensure_not_cancelled(cancel)?;
                let ttls: Vec<i64> = ttl_query.query_async(&mut self.conn).await?;
                if ttls.len() != resources.len() {
                    return Err(AppError::new("database", "Redis TTL 回复数量不匹配"));
                }
                for (resource, ttl) in resources.iter_mut().zip(ttls) {
                    resource.data = json!({"ttl_ms": ttl});
                }
            }
            let pending: Option<i64> = sqlx::query_scalar("SELECT 1 FROM redis_key_scan WHERE scan_id=? AND emitted=0 AND seq>? LIMIT 1")
                .bind(&group).bind(last).fetch_optional(&scan.cache).await?;
            let complete = scan.complete && pending.is_none();
            if last != 0 {
                sqlx::query("UPDATE redis_key_scan SET emitted=1 WHERE scan_id=? AND emitted=0 AND seq<=?")
                    .bind(&group).bind(last).execute(&scan.cache).await?;
            }
            Ok(ResourcePage { resources, cursor: if complete { None } else { Some(id.to_string()) }, complete })
        }.await;
        if (fresh && result.is_err() || result.as_ref().is_ok_and(|page| page.complete))
            && let Some(scan) = self.scans.remove(&id)
        {
            let cleanup = sqlx::query("DELETE FROM redis_key_scan WHERE scan_id=?")
                .bind(id.to_string())
                .execute(&scan.cache)
                .await;
            if result.is_ok() {
                cleanup?;
            }
        }
        Ok(serde_json::to_value(result?)?)
    }
    async fn describe(&mut self, t: ResourceTarget) -> Result<Json> {
        let k = key(&t)?;
        let (kind, ttl) = metadata(&mut self.conn, &k).await?;
        let cmd = match kind.as_str() {
            "string" => "STRLEN",
            "hash" => "HLEN",
            "list" => "LLEN",
            "set" => "SCARD",
            "zset" => "ZCARD",
            "none" => return Ok(json!({"target":t,"type":kind,"ttl_ms":ttl,"length":0})),
            _ => return Err(AppError::invalid("不支持的 Redis 类型")),
        };
        let n: i64 = query(&mut self.conn, &[cmd.as_bytes().to_vec(), k]).await?;
        Ok(json!({"target":t,"type":kind,"ttl_ms":ttl,"length":n}))
    }
    async fn open_edit(
        &mut self,
        p: &Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        self.profile.ensure_write()?;
        let t = target(p, &self.profile)?;
        let k = key(&t)?;
        // 独立物理连接先 WATCH，再读取类型、期限和数据，确保基线受监视。
        let mut conn =
            connect_database(&self.client, &self.profile, self.secret.as_deref()).await?;
        let _: String = query(&mut conn, &[b"WATCH".to_vec(), k.clone()]).await?;
        let (kind, ttl, expires_at_ms) = watched_metadata(&mut conn, &k).await?;
        let result = browse(&mut conn, &k, &kind, sink, cancel).await?;
        let id = Uuid::new_v4();
        self.edits.insert(
            id,
            Edit {
                conn,
                key: k,
                kind: kind.clone(),
                expires_at_ms,
            },
        );
        Ok(
            json!({"token":id,"target":t,"type":kind,"ttl_ms":ttl,"result_id":result,"results":[result]}),
        )
    }
}
async fn connect_database(
    client: &Client,
    profile: &Profile,
    secret: Option<&str>,
) -> Result<Conn> {
    // 每次调用新建连接，SELECT 只影响该连接，不使用 multiplexed clone 隔离 DB。
    let mut c = client.get_multiplexed_async_connection().await?;
    if let Some(s) = secret {
        let mut a = vec![b"AUTH".to_vec()];
        if !profile.username.is_empty() {
            a.push(profile.username.as_bytes().to_vec());
        }
        a.push(s.as_bytes().to_vec());
        let _: String = query(&mut c, &a).await?;
    } else if !profile.username.is_empty() {
        return Err(AppError::invalid("ACL 用户需要凭据"));
    }
    if profile.redis_db != 0 {
        let _: String = query(
            &mut c,
            &[
                b"SELECT".to_vec(),
                profile.redis_db.to_string().into_bytes(),
            ],
        )
        .await?;
    }
    Ok(c)
}
async fn metadata(c: &mut Conn, k: &[u8]) -> Result<(String, i64)> {
    let kind: String = query(c, &[b"TYPE".to_vec(), k.to_vec()]).await?;
    let ttl: i64 = query(c, &[b"PTTL".to_vec(), k.to_vec()]).await?;
    Ok((kind, ttl))
}
async fn watched_metadata(c: &mut Conn, k: &[u8]) -> Result<(String, i64, Option<i64>)> {
    // 在服务端同一脚本中取得 TTL 与时间，保存绝对到期时间，避免编辑耗时延长 TTL。
    let script = "local kind=redis.call('TYPE',KEYS[1]).ok; local ttl=redis.call('PTTL',KEYS[1]); if ttl<0 then return {kind,ttl,false} end; local now=redis.call('TIME'); return {kind,ttl,tostring(now[1]*1000+math.floor(now[2]/1000)+ttl)}";
    let (kind, ttl, expiry): (String, i64, Option<String>) = query(
        c,
        &[
            b"EVAL".to_vec(),
            script.as_bytes().to_vec(),
            b"1".to_vec(),
            k.to_vec(),
        ],
    )
    .await?;
    let expiry = expiry
        .map(|s| {
            s.parse::<i64>()
                .map_err(|_| AppError::new("database", "无效 Redis 到期时间"))
        })
        .transpose()?;
    Ok((kind, ttl, expiry))
}
// 命令参数逐项使用 Base64，保留任意 Redis 字节且避免包含认证命令。
async fn record_browse_command(sink: &ExecutionSink, args: &[Vec<u8>]) -> Result<()> {
    if args
        .iter()
        .try_fold(0usize, |sum, arg| sum.checked_add(arg.len()))
        .is_none_or(|n| n > 40000)
    {
        return sink
            .store
            .append_execution_text(sink.execution_id, "准备发送的命令过长，参数未记录\n", true)
            .await;
    }
    let encoded: Vec<String> = args.iter().map(|arg| STANDARD.encode(arg)).collect();
    let line = format!(
        "准备发送的命令（参数 Base64）: {}\n",
        serde_json::to_string(&encoded)?
    );
    sink.store
        .append_execution_text(sink.execution_id, &line, false)
        .await
}

async fn browse(
    c: &mut Conn,
    k: &[u8],
    kind: &str,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
) -> Result<Uuid> {
    let cols: Vec<Column> = match kind {
        "string" => vec![Column::new("value", "bytes")],
        "hash" => vec![Column::new("field", "bytes"), Column::new("value", "bytes")],
        "list" => vec![
            Column::new("index", "integer"),
            Column::new("value", "bytes"),
        ],
        "set" => vec![Column::new("member", "bytes")],
        "zset" => vec![
            Column::new("member", "bytes"),
            Column::new("score", "float"),
        ],
        "none" => vec![],
        _ => return Err(AppError::invalid("不支持的 Redis 类型")),
    };
    let id = sink.start(cols, "redis").await?;
    match kind {
        "string" => {
            let args = vec![b"GET".to_vec(), k.to_vec()];
            record_browse_command(sink, &args).await?;
            let b: Option<Vec<u8>> = query(c, &args).await?;
            let b = b.ok_or_else(|| AppError::conflict("浏览期间 key 已不存在"))?;
            sink.append(id, vec![vec![val(b)]]).await?;
        }
        "list" => {
            let mut off = 0i64;
            loop {
                ensure_not_cancelled(cancel)?;
                let args = vec![
                    b"LRANGE".to_vec(),
                    k.to_vec(),
                    off.to_string().into_bytes(),
                    (off + 199).to_string().into_bytes(),
                ];
                record_browse_command(sink, &args).await?;
                let values: Vec<Vec<u8>> = query(c, &args).await?;
                if values.is_empty() {
                    break;
                }
                let n = values.len();
                let rows = values
                    .into_iter()
                    .enumerate()
                    .map(|(i, v)| vec![Value::Integer((off + i as i64).to_string()), val(v)])
                    .collect();
                sink.append(id, rows).await?;
                off += n as i64;
                if n < 200 {
                    break;
                }
            }
        }
        "hash" | "set" | "zset" => {
            // 各类 SCAN 可能重复返回成员，去重键使用 field/member 的原始字节。
            sqlx::query("CREATE TABLE IF NOT EXISTS redis_scan_seen (result_id TEXT NOT NULL, value BLOB NOT NULL, PRIMARY KEY(result_id,value))").execute(&sink.store.cache).await?;
            let group = id.to_string();
            let scan: Result<()> = async {
                let command = match kind {
                    "hash" => "HSCAN",
                    "set" => "SSCAN",
                    _ => "ZSCAN",
                };
                let mut cursor = 0u64;
                loop {
                    ensure_not_cancelled(cancel)?;
                    let args = vec![
                        command.as_bytes().to_vec(),
                        k.to_vec(),
                        cursor.to_string().into_bytes(),
                        b"COUNT".to_vec(),
                        b"200".to_vec(),
                    ];
                    record_browse_command(sink, &args).await?;
                    let (next, values): (u64, Vec<Vec<u8>>) = query(c, &args).await?;
                    if kind != "set" && values.len() % 2 != 0 {
                        return Err(AppError::new("database", "无效扫描响应"));
                    }
                    let mut rows = Vec::new();
                    let mut iter = values.into_iter();
                    while let Some(member) = iter.next() {
                        let companion = if kind == "set" { None } else { iter.next() };
                        let added = sqlx::query(
                            "INSERT OR IGNORE INTO redis_scan_seen(result_id,value) VALUES (?,?)",
                        )
                        .bind(&group)
                        .bind(&member)
                        .execute(&sink.store.cache)
                        .await?
                        .rows_affected();
                        if added == 0 {
                            continue;
                        }
                        let mut row = vec![val(member)];
                        if let Some(v) = companion {
                            row.push(if kind == "hash" {
                                val(v)
                            } else {
                                Value::Float(
                                    String::from_utf8(v)
                                        .map_err(|e| AppError::new("database", e.to_string()))?,
                                )
                            });
                        }
                        rows.push(row);
                    }
                    sink.append(id, rows).await?;
                    cursor = next;
                    if cursor == 0 {
                        break;
                    }
                }
                Ok(())
            }
            .await;
            let cleanup = sqlx::query("DELETE FROM redis_scan_seen WHERE result_id = ?")
                .bind(&group)
                .execute(&sink.store.cache)
                .await;
            scan?;
            cleanup?;
        }
        _ => {}
    }
    let args = vec![b"TYPE".to_vec(), k.to_vec()];
    record_browse_command(sink, &args).await?;
    let actual: String = query(c, &args).await?;
    if actual != kind {
        return Err(AppError::conflict("浏览期间 key 类型或存在状态发生变化")
            .with_details(json!({"expected_type":kind,"actual_type":actual})));
    }
    sink.finish(id, json!({"complete":true,"redis_type":kind}))
        .await?;
    Ok(id)
}

fn redis_value(v: redis::Value) -> Json {
    match v {
        redis::Value::Nil => serde_json::to_value(Value::Missing).unwrap(),
        redis::Value::Int(n) => serde_json::to_value(Value::Integer(n.to_string())).unwrap(),
        redis::Value::BulkString(b) => serde_json::to_value(val(b)).unwrap(),
        redis::Value::SimpleString(s) => serde_json::to_value(Value::Text(s)).unwrap(),
        redis::Value::Okay => serde_json::to_value(Value::Text("OK".into())).unwrap(),
        redis::Value::Array(a) => Json::Array(a.into_iter().map(redis_value).collect()),
        other => json!(format!("{other:?}")),
    }
}
