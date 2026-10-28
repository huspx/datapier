//! Redis 单节点会话：按 DB 浏览、执行命令、乐观编辑和分批导入导出。
//! key、field 和 member 保留原始字节；扫描结果允许并发变化，不是快照。

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
                let (kind, ttl) = metadata(&mut self.conn, &k).await?;
                let id = browse(&mut self.conn, &k, &kind, sink, cancel).await?;
                Ok(json!({"results":[id],"type":kind,"ttl_ms":ttl}))
            }
            "change.open" => self.open_edit(&p, sink, cancel).await,
            "change.discard" => {
                let id = uuid(&p)?;
                if self.edits.remove(&id).is_none() {
                    return Err(AppError::invalid("编辑令牌已失效"));
                }
                Ok(json!({"discarded":true}))
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
                    let bytes = serde_json::to_vec(&resource)?.len() + 1;
                    if size + bytes > 1024 * 1024 - 1024 {
                        if resources.is_empty() { return Err(AppError::new("result_too_large", "单个 Redis key 超过分页字节限制")); }
                        page_full = true;
                        break;
                    }
                    size += bytes;
                    last = seq;
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
            let b: Option<Vec<u8>> = query(c, &[b"GET".to_vec(), k.to_vec()]).await?;
            let b = b.ok_or_else(|| AppError::conflict("浏览期间 key 已不存在"))?;
            sink.append(id, vec![vec![val(b)]]).await?;
        }
        "list" => {
            let mut off = 0i64;
            loop {
                ensure_not_cancelled(cancel)?;
                let values: Vec<Vec<u8>> = query(
                    c,
                    &[
                        b"LRANGE".to_vec(),
                        k.to_vec(),
                        off.to_string().into_bytes(),
                        (off + 199).to_string().into_bytes(),
                    ],
                )
                .await?;
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
                    let (next, values): (u64, Vec<Vec<u8>>) = query(
                        c,
                        &[
                            command.as_bytes().to_vec(),
                            k.to_vec(),
                            cursor.to_string().into_bytes(),
                            b"COUNT".to_vec(),
                            b"200".to_vec(),
                        ],
                    )
                    .await?;
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
    let actual: String = query(c, &[b"TYPE".to_vec(), k.to_vec()]).await?;
    if actual != kind {
        return Err(AppError::conflict("浏览期间 key 类型或存在状态发生变化")
            .with_details(json!({"expected_type":kind,"actual_type":actual})));
    }
    sink.finish(id, json!({"complete":true,"redis_type":kind}))
        .await?;
    Ok(id)
}

impl RedisSession {
    async fn submit(
        &mut self,
        p: &Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        self.profile.ensure_write()?;
        let id = uuid(p)?;
        let t = target(p, &self.profile)?;
        // 提交一开始就消费令牌；失败或冲突后不能复用过期基线。
        let mut edit = self
            .edits
            .remove(&id)
            .ok_or_else(|| AppError::invalid("编辑令牌已失效"))?;
        if key(&t)? != edit.key {
            return Err(AppError::invalid("编辑目标不匹配"));
        }
        let changes = p
            .get("changes")
            .and_then(Json::as_array)
            .ok_or_else(|| AppError::invalid("缺少 changes"))?;
        let mut commands: Vec<(usize, Vec<Vec<u8>>)> = Vec::with_capacity(changes.len() + 1);
        let mut ttl_changes = Vec::new();
        let mut explicit_ttl: Option<Vec<Vec<u8>>> = None;
        let mut final_key = edit.key.clone();
        for (position, change) in changes.iter().enumerate() {
            let op = change
                .get("operation")
                .and_then(Json::as_str)
                .ok_or_else(|| AppError::invalid("缺少操作"))?;
            if op == "set_ttl" {
                let ttl = match change.get("ttl_ms") {
                    Some(Json::Null) => vec![b"PERSIST".to_vec()],
                    Some(v) => vec![
                        b"PEXPIRE".to_vec(),
                        v.as_u64()
                            .ok_or_else(|| AppError::invalid("无效 TTL"))?
                            .to_string()
                            .into_bytes(),
                    ],
                    None => return Err(AppError::invalid("缺少 TTL")),
                };
                explicit_ttl = Some(ttl);
                ttl_changes.push(position);
                continue;
            }
            if matches!(op, "rename" | "delete_key")
                && changes[position + 1..]
                    .iter()
                    .any(|c| c.get("operation").and_then(Json::as_str) != Some("set_ttl"))
            {
                return Err(AppError::invalid("重命名或删除之后只能设置 TTL"));
            }
            let k = edit.key.clone();
            // List 删除用独一无二的标记替换指定 index 后再删除，避免误删重复值。
            let cmd=match op {
   "set_string" if edit.kind=="string"||edit.kind=="none" => vec![b"SET".to_vec(),k,field(change,"value")?,b"KEEPTTL".to_vec()],
   "hash_set" if edit.kind=="hash"=>vec![b"HSET".to_vec(),k,field(change,"field")?,field(change,"value")?],
   "hash_delete" if edit.kind=="hash"=>vec![b"HDEL".to_vec(),k,field(change,"field")?],
   "list_set" if edit.kind=="list"=>vec![b"LSET".to_vec(),k,index(change)?,field(change,"value")?],
   "list_insert" if edit.kind=="list"=>vec![b"EVAL".to_vec(),b"local n=redis.call('LLEN',KEYS[1]); local i=tonumber(ARGV[1]); if not i or i<0 or i>n then return redis.error_reply('index out of range') end; if i==n then return redis.call('RPUSH',KEYS[1],ARGV[2]) end; local last=redis.call('LINDEX',KEYS[1],n-1); redis.call('RPUSH',KEYS[1],last); for j=n-1,i+1,-1 do redis.call('LSET',KEYS[1],j,redis.call('LINDEX',KEYS[1],j-1)) end; redis.call('LSET',KEYS[1],i,ARGV[2]); return n+1".to_vec(),b"1".to_vec(),k,index(change)?,field(change,"value")?],
   "list_delete" if edit.kind=="list"=>vec![b"EVAL".to_vec(),b"local n=redis.call('LLEN',KEYS[1]); local i=tonumber(ARGV[1]); if not i or i<0 or i>=n then return redis.error_reply('index out of range') end; if redis.call('LPOS',KEYS[1],ARGV[2]) then return redis.error_reply('temporary marker exists') end; redis.call('LSET',KEYS[1],i,ARGV[2]); return redis.call('LREM',KEYS[1],1,ARGV[2])".to_vec(),b"1".to_vec(),k,index(change)?,format!("\0datapier:{}",Uuid::new_v4()).into_bytes()],
   "set_add" if edit.kind=="set"=>vec![b"SADD".to_vec(),k,field(change,"member")?],
   "set_remove" if edit.kind=="set"=>vec![b"SREM".to_vec(),k,field(change,"member")?],
   "zset_set" if edit.kind=="zset"=>{let s=change.get("score").and_then(Json::as_str).ok_or_else(||AppError::invalid("缺少 score"))?; if s.parse::<f64>().map_or(true,|n|n.is_nan()){return Err(AppError::invalid("无效 score"));} vec![b"ZADD".to_vec(),k,s.as_bytes().to_vec(),field(change,"member")?]},
   "zset_remove" if edit.kind=="zset"=>vec![b"ZREM".to_vec(),k,field(change,"member")?],
   "delete_key"=>vec![b"DEL".to_vec(),k],
   "rename"=>{let dest=STANDARD.decode(change.get("new_key_b64").and_then(Json::as_str).ok_or_else(||AppError::invalid("缺少新 key"))?).map_err(|e|AppError::invalid(e.to_string()))?; if dest==k{return Err(AppError::invalid("新旧 key 相同"));} let _:String=query(&mut edit.conn,&[b"WATCH".to_vec(),dest.clone()]).await?; let exists:i64=query(&mut edit.conn,&[b"EXISTS".to_vec(),dest.clone()]).await?; if exists!=0{return Err(AppError::conflict("目标 key 已存在"));} final_key=dest.clone(); vec![b"RENAMENX".to_vec(),k,dest]},
   _=>return Err(AppError::invalid(format!("操作 {op} 不适用于当前类型"))) };
            commands.push((position, cmd));
        }
        // 没有显式设置 TTL 时恢复基线的绝对到期时间；永久 key 无需附加期限命令。
        let expiry = explicit_ttl.clone().or_else(|| {
            edit.expires_at_ms
                .map(|expiry| vec![b"PEXPIREAT".to_vec(), expiry.to_string().into_bytes()])
        });
        let has_expiry = expiry.is_some();
        if let Some(expiry) = expiry {
            let script = "local k=KEYS[1]; if KEYS[2]~=KEYS[1] and redis.call('EXISTS',KEYS[2])==1 then k=KEYS[2] end; if redis.call('EXISTS',k)==0 then return -1 end; return redis.call(ARGV[1],k,unpack(ARGV,2))";
            let mut command = vec![
                b"EVAL".to_vec(),
                script.as_bytes().to_vec(),
                b"2".to_vec(),
                edit.key.clone(),
                final_key,
            ];
            command.extend(expiry);
            commands.push((usize::MAX, command));
        }
        ensure_not_cancelled(cancel)?;
        let _: String = query(&mut edit.conn, &args(&["MULTI"])).await?;
        for (_, cmd) in &commands {
            let _: String = query(&mut edit.conn, cmd).await?;
        }
        sink.mark_sent(true).await?;
        let result =
            redis::aio::ConnectionLike::req_packed_command(&mut edit.conn, &redis::cmd("EXEC"))
                .await
                .map_err(error_unknown)?;
        // EXEC 的 Nil 表示 WATCH 冲突；数组中的单项错误不意味着其他命令回滚。
        match result {
            redis::Value::Nil => Err(AppError::conflict("key 在读取基线后已变化")),
            redis::Value::Array(replies) => {
                if replies.len() != commands.len() {
                    return Err(AppError::new("database", "无效 EXEC 响应长度"));
                }
                let mut items: Vec<Json> = (0..changes.len())
                    .map(|index| json!({"index":index,"status":"applied","superseded":true}))
                    .collect();
                let mut expiry_error = None;
                for ((index, _), reply) in commands.into_iter().zip(replies) {
                    if index == usize::MAX {
                        match reply {
                            redis::Value::ServerError(e) => expiry_error = Some(format!("{e:?}")),
                            redis::Value::Int(-1) if !ttl_changes.is_empty() => {
                                expiry_error = Some("设置 TTL 时 key 不存在".into())
                            }
                            other => {
                                if let Some(&index) = ttl_changes.last() {
                                    items[index] = json!({"index":index,"status":"applied","result":redis_value(other)});
                                }
                            }
                        }
                        continue;
                    }
                    items[index] = match reply {
                        redis::Value::ServerError(e) => {
                            json!({"index":index,"status":"error","error":format!("{e:?}")})
                        }
                        other => {
                            json!({"index":index,"status":"applied","result":redis_value(other)})
                        }
                    };
                }
                if let Some(error) = expiry_error {
                    let index = ttl_changes
                        .last()
                        .copied()
                        .or_else(|| changes.len().checked_sub(1));
                    if let Some(index) = index {
                        let previous = items[index].clone();
                        items[index] = json!({"index":index,"status":"error","error":error,"previous_result":previous,"value_may_have_changed":true});
                    } else {
                        return Err(AppError::new("database", error));
                    }
                }
                Ok(json!({"items":items,"results":[],"ttl_checked":has_expiry}))
            }
            _ => Err(AppError::new("database", "无效 EXEC 响应")),
        }
    }
}
fn index(p: &Json) -> Result<Vec<u8>> {
    let i = p
        .get("index")
        .and_then(Json::as_i64)
        .filter(|i| *i >= 0)
        .ok_or_else(|| AppError::invalid("无效 list 索引"))?;
    Ok(i.to_string().into_bytes())
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

impl RedisSession {
    async fn export_meta(&mut self, p: &Json) -> Result<Json> {
        let t = target(p, &self.profile)?;
        let k = key(&t)?;
        // 用 Redis 服务端时间换算绝对期限，导入时不会因传输时间延长寿命。
        let script = "local t=redis.call('TYPE',KEYS[1]).ok; local ttl=redis.call('PTTL',KEYS[1]); if ttl==-2 then return {t, false} end; if ttl==-1 then return {t, false} end; local now=redis.call('TIME'); return {t, tostring(now[1]*1000+math.floor(now[2]/1000)+ttl)}";
        let (kind, expires): (String, Option<String>) = query(
            &mut self.conn,
            &[
                b"EVAL".to_vec(),
                script.as_bytes().to_vec(),
                b"1".to_vec(),
                k,
            ],
        )
        .await?;
        if kind == "none" {
            return Err(AppError::new("not_found", "key 不存在"));
        }
        Ok(json!({"type":kind,"expires_at_ms":expires}))
    }
    async fn import_begin(&mut self, p: &Json, sink: &ExecutionSink) -> Result<Json> {
        self.profile.ensure_write()?;
        let t = target(p, &self.profile)?;
        let k = key(&t)?;
        let kind = param::<String>(p, "type")?;
        if !matches!(kind.as_str(), "string" | "hash" | "list" | "set" | "zset") {
            return Err(AppError::invalid("不支持的类型"));
        }
        let expires = match p.get("expires_at_ms") {
            None | Some(Json::Null) => None,
            Some(Json::String(s)) => Some(
                s.parse::<i64>()
                    .map_err(|_| AppError::invalid("无效过期时间"))?,
            ),
            _ => return Err(AppError::invalid("无效过期时间")),
        };
        let exists: i64 = query(&mut self.conn, &[b"EXISTS".to_vec(), k.clone()]).await?;
        if exists != 0 {
            let (sec, micro): (i64, i64) = query(&mut self.conn, &args(&["TIME"])).await?;
            if expires.is_none_or(|e| e > sec * 1000 + micro / 1000) {
                return Err(AppError::conflict("目标 key 已存在"));
            }
        }
        let id = Uuid::new_v4();
        let temp = format!("__datapier_import_{id}").into_bytes();
        // 标记保存在临时对象本身，避免单独的所有权 key 在对象被替换后仍然有效。
        let marker = Uuid::new_v4().to_string().into_bytes();
        // 在发送创建请求前记账：响应丢失时 close 仍可按标记清理。
        self.imports.insert(
            id,
            Import {
                key: k,
                temp: temp.clone(),
                marker: marker.clone(),
                pending_marker: None,
                kind: kind.clone(),
                expires,
                wrote: false,
                valid: false,
                count: 0,
            },
        );
        let script = r#"
if redis.call('EXISTS', KEYS[1]) ~= 0 then return 0 end
local k, m = ARGV[1], ARGV[2]
if k == 'string' then redis.call('SET', KEYS[1], m)
elseif k == 'hash' then redis.call('HSET', KEYS[1], m, m)
elseif k == 'list' then redis.call('RPUSH', KEYS[1], m)
elseif k == 'set' then redis.call('SADD', KEYS[1], m)
else redis.call('ZADD', KEYS[1], 0, m) end
redis.call('PEXPIRE', KEYS[1], 86400000)
return 1
"#;
        let created: i64 = send_write(
            &mut self.conn,
            &[
                b"EVAL".to_vec(),
                script.as_bytes().to_vec(),
                b"1".to_vec(),
                temp,
                kind.into_bytes(),
                marker,
            ],
            sink,
        )
        .await?;
        if created == 0 {
            self.imports.remove(&id);
            return Err(AppError::conflict("临时 key 已存在"));
        }
        self.imports.get_mut(&id).unwrap().valid = true;
        Ok(json!({"token":id}))
    }
    async fn import_append(&mut self, p: &Json, sink: &ExecutionSink) -> Result<Json> {
        self.profile.ensure_write()?;
        let id = uuid(p)?;
        let import = self
            .imports
            .get_mut(&id)
            .ok_or_else(|| AppError::invalid("导入令牌已失效"))?;
        if !import.valid {
            return Err(AppError::invalid("导入令牌已失效"));
        }
        let rows: Vec<Vec<Value>> = param(p, "rows")?;
        if rows.len() > 500 {
            return Err(AppError::invalid("批次超过 500 行"));
        }
        let mut commands = Vec::with_capacity(rows.len());
        for row in rows {
            let k = import.temp.clone();
            let cmd = match (import.kind.as_str(), row.as_slice()) {
                ("string", [v]) => {
                    if import.wrote || !commands.is_empty() {
                        return Err(AppError::invalid("String 只能有一行"));
                    }
                    vec![b"SET".to_vec(), k, bytes(v)?]
                }
                ("hash", [f, v]) => vec![b"HSET".to_vec(), k, bytes(f)?, bytes(v)?],
                ("list", [Value::Integer(i) | Value::Unsigned(i), v]) => {
                    if i.parse::<u64>().ok() != Some(import.count + commands.len() as u64) {
                        return Err(AppError::invalid("List 索引不连续"));
                    }
                    vec![b"RPUSH".to_vec(), k, bytes(v)?]
                }
                ("set", [m]) => vec![b"SADD".to_vec(), k, bytes(m)?],
                ("zset", [m, Value::Float(s)]) => {
                    vec![b"ZADD".to_vec(), k, s.as_bytes().to_vec(), bytes(m)?]
                }
                _ => return Err(AppError::invalid("导入行不符合 Redis 类型")),
            };
            commands.push(cmd);
        }
        let script = format!(
            r#"{IMPORT_OWNED}
if not owned(ARGV[2]) then return redis.error_reply('temporary key invalid') end
local k, m, n = ARGV[1], ARGV[2], ARGV[3]
local member = ARGV[6]
if k == 'zset' then member = ARGV[7] end
local rotate = (k == 'hash' or k == 'set' or k == 'zset') and member == m
if rotate then
    if (k == 'hash' and redis.call('HEXISTS', KEYS[1], n) == 1)
        or (k == 'set' and redis.call('SISMEMBER', KEYS[1], n) == 1)
        or (k == 'zset' and redis.call('ZSCORE', KEYS[1], n)) then
        return redis.error_reply('temporary marker collision')
    end
    if k == 'hash' then redis.call('HSET', KEYS[1], n, n); redis.call('HDEL', KEYS[1], m)
    elseif k == 'set' then redis.call('SADD', KEYS[1], n); redis.call('SREM', KEYS[1], m)
    else redis.call('ZADD', KEYS[1], 0, n); redis.call('ZREM', KEYS[1], m) end
end
local r
if k == 'string' then r = redis.pcall('SET', KEYS[1], m .. ARGV[6], 'KEEPTTL')
else r = redis.pcall(unpack(ARGV, 4)) end
if type(r) == 'table' and r.err then
    if rotate then
        if k == 'hash' then redis.call('HSET', KEYS[1], m, m); redis.call('HDEL', KEYS[1], n)
        elseif k == 'set' then redis.call('SADD', KEYS[1], m); redis.call('SREM', KEYS[1], n)
        else redis.call('ZADD', KEYS[1], 0, m); redis.call('ZREM', KEYS[1], n) end
    end
    return r
end
redis.call('PEXPIRE', KEYS[1], 86400000)
return rotate and 2 or 1
"#
        );
        for (index, cmd) in commands.iter().enumerate() {
            // 用户数据可能恰好等于标记；原子更换标记后再写入，不能丢弃该数据。
            let collides = match import.kind.as_str() {
                "hash" | "set" => cmd[2] == import.marker,
                "zset" => cmd[3] == import.marker,
                _ => false,
            };
            let next_marker = if collides {
                Uuid::new_v4().to_string().into_bytes()
            } else {
                Vec::new()
            };
            let mut wrapped = vec![
                b"EVAL".to_vec(),
                script.as_bytes().to_vec(),
                b"1".to_vec(),
                import.temp.clone(),
                import.kind.as_bytes().to_vec(),
                import.marker.clone(),
                next_marker.clone(),
            ];
            wrapped.extend(cmd.iter().cloned());
            // 丢响应时旧标记或新标记均可能有效，清理须保留两者。
            if collides {
                import.pending_marker = Some(next_marker.clone());
            }
            let res: Result<i64> = send_write(&mut self.conn, &wrapped, sink).await;
            match res {
                Ok(code) => {
                    if code == 2 {
                        import.marker = next_marker;
                    }
                    import.pending_marker = None;
                    import.wrote = true;
                    import.count += 1;
                }
                Err(e) => {
                    import.valid = false;
                    return Err(e.with_details(json!({"completed":index,"token":id})));
                }
            }
        }
        Ok(json!({"appended":commands.len()}))
    }
    async fn import_finish(&mut self, p: &Json, sink: &ExecutionSink) -> Result<Json> {
        self.profile.ensure_write()?;
        let id = uuid(p)?;
        let import = self
            .imports
            .get_mut(&id)
            .ok_or_else(|| AppError::invalid("导入令牌已失效"))?;
        if !import.valid {
            return Err(AppError::invalid("导入令牌已失效"));
        }
        if !import.wrote {
            return Err(AppError::invalid("不能导入空集合"));
        }
        // 验证标记、移除标记及发布处于同一脚本，替换对象不能被提交或到期清理。
        let script = format!(
            r#"{IMPORT_OWNED}
if not owned(ARGV[2]) then return -2 end
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
local expiry = tonumber(ARGV[3])
if expiry and expiry <= now then redis.call('DEL', KEYS[1]); return -1 end
if redis.call('EXISTS', KEYS[2]) ~= 0 then return 0 end
local k, m = ARGV[1], ARGV[2]
if k == 'string' then
    local v = redis.call('GET', KEYS[1])
    redis.call('SET', KEYS[1], string.sub(v, string.len(m) + 1), 'KEEPTTL')
elseif k == 'hash' then redis.call('HDEL', KEYS[1], m)
elseif k == 'list' then redis.call('LPOP', KEYS[1])
elseif k == 'set' then redis.call('SREM', KEYS[1], m)
else redis.call('ZREM', KEYS[1], m) end
if expiry then redis.call('PEXPIREAT', KEYS[1], ARGV[3])
else redis.call('PERSIST', KEYS[1]) end
return redis.call('RENAMENX', KEYS[1], KEYS[2])
"#
        );
        let expiry = import.expires.map(|n| n.to_string()).unwrap_or_default();
        let a = vec![
            b"EVAL".to_vec(),
            script.as_bytes().to_vec(),
            b"2".to_vec(),
            import.temp.clone(),
            import.key.clone(),
            import.kind.as_bytes().to_vec(),
            import.marker.clone(),
            expiry.into_bytes(),
        ];
        let code: i64 = match send_write(&mut self.conn, &a, sink).await {
            Ok(v) => v,
            Err(e) => {
                import.valid = false;
                return Err(e);
            }
        };
        match code {
            1 => {
                self.imports.remove(&id);
                Ok(json!({"published":true}))
            }
            -1 => {
                self.imports.remove(&id);
                Ok(json!({"skipped":true}))
            }
            0 => {
                import.valid = false;
                Err(AppError::conflict("目标 key 已存在"))
            }
            _ => {
                import.valid = false;
                Err(AppError::invalid("导入临时 key 已失效"))
            }
        }
    }
    async fn import_discard(&mut self, p: &Json, sink: &ExecutionSink) -> Result<Json> {
        self.profile.ensure_write()?;
        let id = uuid(p)?;
        let import = self
            .imports
            .get(&id)
            .ok_or_else(|| AppError::invalid("导入令牌已失效"))?;
        let _: i64 = send_write(&mut self.conn, &import_cleanup_args(import), sink).await?;
        self.imports.remove(&id);
        Ok(json!({"discarded":true}))
    }
}

impl RedisSession {
    async fn run(
        &mut self,
        p: &Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        let text = param::<String>(p, "text")?;
        let commands = parse_commands(&text)?;
        if commands.is_empty() {
            return Err(AppError::invalid("命令为空"));
        }
        for a in &commands {
            let name = String::from_utf8_lossy(&a[0]).to_ascii_uppercase();
            if matches!(
                name.as_str(),
                "SUBSCRIBE"
                    | "PSUBSCRIBE"
                    | "SSUBSCRIBE"
                    | "MONITOR"
                    | "SYNC"
                    | "PSYNC"
                    | "QUIT"
                    | "RESET"
                    | "HELLO"
            ) {
                return Err(AppError::invalid("此连接模式不受支持"));
            }
            if self.profile.access_mode == AccessMode::ReadOnly && !read_command(a) {
                return Err(AppError::new(
                    "permission_denied",
                    format!("只读连接不允许命令 {name}"),
                ));
            }
        }
        if self.command_dead {
            return Err(AppError::invalid("命令连接已失效，请重新连接"));
        }
        // 命令连接单独建立：用户命令可能修改 SELECT 或事务等连接状态。
        if self.command_conn.is_none() {
            self.command_conn =
                Some(connect_database(&self.client, &self.profile, self.secret.as_deref()).await?);
        }
        let mut results = Vec::new();
        for a in commands {
            ensure_not_cancelled(cancel)?;
            let may_write = !read_command(&a);
            sink.mark_sent(may_write).await?;
            let mut command = redis::cmd(
                std::str::from_utf8(&a[0]).map_err(|_| AppError::invalid("命令名必须是 UTF-8"))?,
            );
            for v in &a[1..] {
                command.arg(v);
            }
            // 停止等待后丢弃连接；已发送的写命令可能仍在服务端执行。
            let mut conn = self.command_conn.take().unwrap();
            let response = tokio::select! { result=redis::aio::ConnectionLike::req_packed_command(&mut conn,&command)=>result, _=cancel.cancelled()=>{self.command_dead=true; return Err(if may_write {AppError::unknown("已发送命令，停止等待后写入结果未知")} else {AppError::cancelled()});} };
            let value = match response {
                Ok(redis::Value::ServerError(e)) => {
                    self.command_conn = Some(conn);
                    return Err(AppError::new("database", format!("{e:?}")));
                }
                Ok(v) => v,
                Err(e) => {
                    self.command_dead = true;
                    return Err(if may_write {
                        error_unknown(e)
                    } else {
                        e.into()
                    });
                }
            };
            self.command_conn = Some(conn);
            let id = sink
                .start(vec![Column::new("reply", "redis")], "redis_command")
                .await?;
            sink.append(id, vec![vec![Value::Json(redis_value(value))]])
                .await?;
            sink.finish(id, json!({"complete":true})).await?;
            results.push(id);
        }
        Ok(json!({"results":results}))
    }
}
fn read_command(a: &[Vec<u8>]) -> bool {
    let n = String::from_utf8_lossy(&a[0]).to_ascii_uppercase();
    matches!(
        n.as_str(),
        "PING"
            | "ECHO"
            | "GET"
            | "MGET"
            | "GETRANGE"
            | "STRLEN"
            | "EXISTS"
            | "TYPE"
            | "TTL"
            | "PTTL"
            | "SCAN"
            | "HSCAN"
            | "SSCAN"
            | "ZSCAN"
            | "HGET"
            | "HMGET"
            | "HGETALL"
            | "HEXISTS"
            | "HLEN"
            | "HKEYS"
            | "HVALS"
            | "HSTRLEN"
            | "LLEN"
            | "LINDEX"
            | "LRANGE"
            | "LPOS"
            | "SCARD"
            | "SISMEMBER"
            | "SMISMEMBER"
            | "SMEMBERS"
            | "SRANDMEMBER"
            | "SDIFF"
            | "SINTER"
            | "SUNION"
            | "ZCARD"
            | "ZSCORE"
            | "ZMSCORE"
            | "ZRANGE"
            | "ZREVRANGE"
            | "ZRANGEBYSCORE"
            | "ZREVRANGEBYSCORE"
            | "ZRANGEBYLEX"
            | "ZREVRANGEBYLEX"
            | "ZCOUNT"
            | "ZLEXCOUNT"
            | "ZRANK"
            | "ZREVRANK"
            | "INFO"
            | "DBSIZE"
            | "TIME"
            | "SELECT"
    )
}
fn parse_commands(text: &str) -> Result<Vec<Vec<Vec<u8>>>> {
    let input = text.as_bytes();
    let mut all = Vec::new();
    let mut line = Vec::new();
    let mut word = Vec::new();
    let mut active = false;
    let mut quote = 0u8;
    let mut i = 0;
    while i < input.len() {
        let b = input[i];
        i += 1;
        if quote == 0 && (b == b' ' || b == b'\t' || b == b'\r' || b == b'\n') {
            if active {
                line.push(std::mem::take(&mut word));
                active = false;
            }
            if b == b'\n' && !line.is_empty() {
                all.push(std::mem::take(&mut line));
            }
            continue;
        }
        if b == b'\'' || b == b'"' {
            if quote == 0 {
                quote = b;
                active = true;
                continue;
            }
            if quote == b {
                quote = 0;
                continue;
            }
        }
        if b == b'\\' {
            if i >= input.len() {
                return Err(AppError::invalid("转义不完整"));
            }
            let c = input[i];
            i += 1;
            if quote == b'\'' && c != b'\'' {
                word.push(b'\\');
                word.push(c);
            } else if c == b'x' {
                if i + 1 >= input.len() {
                    return Err(AppError::invalid("十六进制转义不完整"));
                }
                let h = |c: u8| -> Option<u8> {
                    match c {
                        b'0'..=b'9' => Some(c - b'0'),
                        b'a'..=b'f' => Some(c - b'a' + 10),
                        b'A'..=b'F' => Some(c - b'A' + 10),
                        _ => None,
                    }
                };
                let v = h(input[i])
                    .zip(h(input[i + 1]))
                    .ok_or_else(|| AppError::invalid("无效十六进制转义"))?;
                word.push(v.0 * 16 + v.1);
                i += 2;
            } else {
                word.push(match c {
                    b'n' => b'\n',
                    b'r' => b'\r',
                    b't' => b'\t',
                    b'b' => 8,
                    b'a' => 7,
                    _ => c,
                });
            }
            active = true;
            continue;
        }
        word.push(b);
        active = true;
    }
    if quote != 0 {
        return Err(AppError::invalid("引号未闭合"));
    }
    if active {
        line.push(word);
    }
    if !line.is_empty() {
        all.push(line);
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn command_parser_keeps_binary_and_quoted_newlines() {
        let commands = parse_commands("SET \"a\\x00b\" \"line1\nline2\"\nGET 'a\\'b'").unwrap();
        assert_eq!(
            commands,
            vec![
                vec![b"SET".to_vec(), b"a\0b".to_vec(), b"line1\nline2".to_vec()],
                vec![b"GET".to_vec(), b"a'b".to_vec()]
            ]
        );
    }
    #[test]
    fn read_only_commands_do_not_accept_scripts_or_transactions() {
        assert!(read_command(&args(&["GET", "key"])));
        assert!(!read_command(&args(&["EVAL", "return 1", "0"])));
        assert!(!read_command(&args(&["MULTI"])));
        assert!(!read_command(&args(&["SORT", "key", "STORE", "out"])));
    }
}
