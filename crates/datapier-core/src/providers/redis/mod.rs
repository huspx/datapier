//! Redis 单节点会话：按 DB 浏览、执行命令、乐观编辑和分批导入导出。
//! key、field 和 member 保留原始字节；扫描结果允许并发变化，不是快照。

mod batch_delete;
pub(crate) mod command;
mod edit;
mod replace;
mod transfer;
mod values;

use crate::{ExecutionSink, model::*};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use redis::{
    Client, ConnectionAddr, IntoConnectionInfo, RedisConnectionInfo, aio::MultiplexedConnection,
};
use serde_json::{Value as Json, json};
use std::collections::{HashMap, HashSet, VecDeque};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

/// 使用执行器同一解析器分析命令；不支持证明访问范围的命令会被拒绝。
pub fn analyze_command(text: &str) -> Result<Json> {
    let parsed = command::parse_commands(text)?;
    if parsed.is_empty() {
        return Err(AppError::invalid("命令为空"));
    }
    let mut commands = Vec::new();
    let mut may_write = false;
    let mut dangerous = false;
    let mut databases = Vec::new();
    let mut change_database = None;
    for args in parsed {
        let name = std::str::from_utf8(&args[0])
            .map_err(|_| AppError::invalid("命令名必须是 UTF-8"))?
            .to_ascii_uppercase();
        let is_read = command::read_command(&args);
        let is_control = matches!(
            name.as_str(),
            "MULTI" | "EXEC" | "DISCARD" | "WATCH" | "UNWATCH"
        );
        let is_write = matches!(
            name.as_str(),
            "SET"
                | "SETNX"
                | "SETEX"
                | "PSETEX"
                | "MSET"
                | "MSETNX"
                | "DEL"
                | "UNLINK"
                | "EXPIRE"
                | "PEXPIRE"
                | "EXPIREAT"
                | "PEXPIREAT"
                | "PERSIST"
                | "INCR"
                | "INCRBY"
                | "INCRBYFLOAT"
                | "DECR"
                | "DECRBY"
                | "APPEND"
                | "SETRANGE"
                | "GETSET"
                | "GETDEL"
                | "GETEX"
                | "HSET"
                | "HSETNX"
                | "HMSET"
                | "HDEL"
                | "HINCRBY"
                | "HINCRBYFLOAT"
                | "LPUSH"
                | "RPUSH"
                | "LPUSHX"
                | "RPUSHX"
                | "LPOP"
                | "RPOP"
                | "LSET"
                | "LINSERT"
                | "LREM"
                | "LTRIM"
                | "SADD"
                | "SREM"
                | "SPOP"
                | "SMOVE"
                | "SDIFFSTORE"
                | "SINTERSTORE"
                | "SUNIONSTORE"
                | "ZADD"
                | "ZREM"
                | "ZINCRBY"
                | "ZREMRANGEBYRANK"
                | "ZREMRANGEBYSCORE"
                | "ZREMRANGEBYLEX"
                | "ZUNIONSTORE"
                | "ZINTERSTORE"
                | "RENAME"
                | "RENAMENX"
                | "COPY"
                | "FLUSHDB"
        );
        if !is_read && !is_control && !is_write {
            return Err(AppError::new(
                "permission_denied",
                format!("无法证明 Redis 命令 {name} 的访问范围"),
            ));
        }
        if name == "COPY"
            && args
                .iter()
                .skip(3)
                .any(|arg| arg.eq_ignore_ascii_case(b"DB"))
        {
            return Err(AppError::new("permission_denied", "不允许跨 DB COPY"));
        }
        let db = if name == "SELECT" {
            if args.len() != 2 {
                return Err(AppError::invalid("SELECT 需要一个 DB 编号"));
            }
            let db = std::str::from_utf8(&args[1])
                .ok()
                .and_then(|s| s.parse::<u32>().ok())
                .ok_or_else(|| AppError::invalid("无效 Redis DB"))?;
            databases.push(db);
            change_database = Some(db);
            Some(db)
        } else {
            None
        };
        let command_dangerous = matches!(name.as_str(), "FLUSHDB" | "DEL" | "UNLINK");
        may_write |= is_write || name == "EXEC";
        dangerous |= command_dangerous;
        commands.push(json!({"name":name,"may_write":is_write || name == "EXEC","dangerous":command_dangerous,"select_database":db}));
    }
    Ok(
        json!({"may_write":may_write,"dangerous":dangerous,"databases":databases,"change_database":change_database,"commands":commands}),
    )
}
/// 判断 Redis 命令文本是否包含 AUTH 或 HELLO 的 AUTH 参数；解析失败时返回错误。
pub fn redis_contains_auth(text: &str) -> Result<bool> {
    Ok(command::parse_commands(text)?.iter().any(|args| {
        args[0].eq_ignore_ascii_case(b"AUTH")
            || (args[0].eq_ignore_ascii_case(b"HELLO") && {
                let mut options = args[1..].iter();
                let mut found = false;
                while let Some(option) = options.next() {
                    if option.eq_ignore_ascii_case(b"AUTH") {
                        found = true;
                        break;
                    }
                    if option.eq_ignore_ascii_case(b"SETNAME") {
                        options.next();
                    }
                }
                found
            })
    }))
}

type Conn = MultiplexedConnection;
// WATCH 状态属于物理连接；令牌持有该连接直到提交、丢弃或会话关闭。
struct Edit {
    conn: Conn,
    key: Vec<u8>,
    kind: String,
    result_id: Uuid,
    expires_at_ms: Option<i64>,
    field_expiries: HashMap<Vec<u8>, Option<i64>>,
    field_expiry_supported: Option<bool>,
    field_budget: usize,
    field_owner: Uuid,
    field_store: crate::Store,
    field_handle: Uuid,
}

impl Drop for Edit {
    fn drop(&mut self) {
        if self.field_budget == 0 {
            return;
        }
        // 提交 future 在发送前被取消时也可能丢弃已消费的令牌；不能遗留预算。
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let store = self.field_store.clone();
            let id = self.field_handle;
            drop(runtime.spawn(async move {
                // release_auxiliary 当前只删除内存登记，不涉及可能失败的外部 I/O。
                let _ = store.release_auxiliary(id).await;
            }));
        }
    }
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
// 原始字节用于去重，待输出的超量 SCAN 响应保留到下一逻辑批次。
struct KeyScan {
    cursor: u64,
    complete: bool,
    limited: bool,
    pattern: Option<String>,
    kind: Option<String>,
    seen: HashSet<Vec<u8>>,
    pending: VecDeque<(Vec<u8>, String)>,
    bytes: usize,
    store: crate::Store,
    execution_id: Uuid,
}
/// 持有一个 Redis DB 的普通连接，以及隔离的命令、编辑和扫描状态。
///
/// 命令和每个编辑分别建立物理连接：multiplexed 连接的 clone 仍共享
/// 服务端连接状态，无法隔离 SELECT、事务和 WATCH。
pub struct RedisSession {
    profile: Profile,
    client: Client,
    secret: Option<String>,
    conn: Option<Conn>,
    browser_conn: Option<Conn>,
    command_db: u32,
    command_transaction: Option<Vec<Option<u32>>>,
    command_conn: Option<Conn>,
    command_dead: bool,
    edits: HashMap<Uuid, Edit>,
    batch_edits: HashMap<Uuid, batch_delete::BatchEdit>,
    replace_plans: HashMap<Uuid, replace::ReplacePlan>,
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
        && db.parse::<u32>().ok() != Some(profile.redis_db())
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
        let direct = profile.direct()?;
        let addr = if direct.tls.enabled {
            ConnectionAddr::TcpTls {
                host: direct.host.clone(),
                port: direct.port,
                insecure: false,
                tls_params: None,
            }
        } else {
            ConnectionAddr::Tcp(direct.host.clone(), direct.port)
        };
        let settings = RedisConnectionInfo::default().set_skip_set_lib_name();
        let info = "redis://localhost/"
            .into_connection_info()?
            .set_addr(addr)
            .set_redis_settings(settings);
        let client = if direct.tls.enabled {
            let root_cert = direct.tls.ca_path.as_ref().map(std::fs::read).transpose()?;
            let client_tls = match (&direct.tls.client_cert_path, &direct.tls.client_key_path) {
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
            if direct.tls.ca_path.is_some()
                || direct.tls.client_cert_path.is_some()
                || direct.tls.client_key_path.is_some()
            {
                return Err(AppError::invalid("TLS 未启用"));
            }
            Client::open(info)?
        };
        let command_db = profile.redis_db();
        let conn = connect_database(&client, &profile, secret.as_deref()).await?;
        Ok(Self {
            profile,
            client,
            secret,
            browser_conn: None,
            command_db,
            command_transaction: None,
            conn: Some(conn),
            command_conn: None,
            command_dead: false,
            edits: HashMap::new(),
            batch_edits: HashMap::new(),
            replace_plans: HashMap::new(),
            imports: HashMap::new(),
            scans: HashMap::new(),
        })
    }
    /// 返回会话创建时的连接配置。
    pub fn profile(&self) -> &Profile {
        &self.profile
    }
    /// 返回命令连接当前选择的 DB；浏览连接始终使用配置中的 DB。
    pub fn command_db(&self) -> u32 {
        self.command_db
    }
    /// 释放编辑和命令连接，清理扫描缓存及导入临时 key，再关闭普通连接。
    /// 清理失败时仍尝试其余清理，并返回第一个错误。
    pub async fn close(&mut self) -> Result<()> {
        let mut failure = None;
        for (_, mut edit) in std::mem::take(&mut self.edits) {
            match edit.field_store.release_auxiliary(edit.field_handle).await {
                Ok(()) => edit.field_budget = 0,
                Err(error) => {
                    if failure.is_none() {
                        failure = Some(error);
                    }
                }
            }
        }
        self.batch_edits.clear();
        self.replace_plans.clear();
        self.browser_conn = None;
        self.command_conn = None;
        self.command_dead = true;
        for (id, scan) in std::mem::take(&mut self.scans) {
            if let Err(error) = scan.store.release_auxiliary(id).await
                && failure.is_none()
            {
                failure = Some(error);
            }
        }
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        let Some(mut conn) = self.conn.take() else {
            return failure.map_or(Ok(()), Err);
        };
        let imports = std::mem::take(&mut self.imports);
        for (_, i) in imports {
            match tokio::time::timeout_at(
                deadline,
                query::<i64>(&mut conn, &import_cleanup_args(&i)),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    if failure.is_none() {
                        failure = Some(e);
                    }
                }
                Err(_) => {
                    if failure.is_none() {
                        failure = Some(AppError::new("timeout", "清理 Redis 导入超时"));
                    }
                    break;
                }
            }
        }
        if let Some(e) = failure {
            return Err(e);
        }
        drop(conn);
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
        if self.conn.is_none() {
            return Err(AppError::new("connection_closed", "连接已关闭"));
        }
        if method == "database.list" {
            let override_count = self.profile.direct()?.redis_databases_override;
            let (source, result) = if let Some(count) = override_count {
                ("manual_override", Ok(count))
            } else {
                let timeout = sink.store.settings().await?.limits.connect_timeout_seconds;
                let probe = async {
                    let mut conn =
                        connect_database(&self.client, &self.profile, self.secret.as_deref())
                            .await?;
                    let values: Vec<String> =
                        query(&mut conn, &args(&["CONFIG", "GET", "databases"])).await?;
                    if values.len() != 2 || !values[0].eq_ignore_ascii_case("databases") {
                        return Err(AppError::invalid("Redis databases 配置响应无效"));
                    }
                    values[1]
                        .parse::<u32>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| AppError::invalid("Redis databases 数量无效"))
                };
                (
                    "server_config",
                    match tokio::time::timeout(
                        std::time::Duration::from_secs(timeout as u64),
                        probe,
                    )
                    .await
                    {
                        Ok(result) => result,
                        Err(_) => Err(AppError::new("timeout", "Redis DB 探测超时")),
                    },
                )
            };
            return Ok(match result {
                Ok(count) if count > 0 => {
                    json!({"state":"known","source":source,"databases":(0..count).map(|n| n.to_string()).collect::<Vec<_>>(),"configured_count":count,"reason":null})
                }
                Ok(_) => {
                    json!({"state":"unknown","source":source,"databases":[],"configured_count":null,"reason":"Redis databases 数量无效"})
                }
                Err(error) => {
                    json!({"state":"unknown","source":source,"databases":[],"configured_count":null,"reason":error.to_string()})
                }
            });
        }
        if matches!(
            method,
            "connection.info" | "resource.list" | "resource.describe" | "resource.browse"
        ) {
            // 未完成的 multiplexed 响应仍可能留在驱动中；取消时丢弃独立物理连接。
            let mut conn = match self.browser_conn.take() {
                Some(conn) => conn,
                None => tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(AppError::cancelled()),
                    result = connect_database(&self.client, &self.profile, self.secret.as_deref()) => result?,
                },
            };
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(AppError::cancelled()),
                result = self.read_resource(method, &p, sink, cancel, &mut conn) => result,
            };
            self.browser_conn = Some(conn);
            return result;
        }
        match method {
            "replace.prepare" => self.replace_prepare(&p, cancel).await,
            "replace.read" => self.replace_read(&p, cancel),
            "replace.update" => self.replace_update(&p, cancel),
            "replace.submit" => self.replace_submit(&p, sink, cancel).await,
            "replace.discard" => self.replace_discard(&p, cancel),
            "value.snapshot" => self.value_snapshot(&p).await,
            "value.verify" => {
                self.value_verify(&p).await?;
                Ok(json!({"verified":true}))
            }
            "change.open" if p.get("targets").is_some() => self.open_batch_delete(&p, cancel).await,
            "change.open" => self.open_edit(&p, sink, cancel).await,
            "change.discard" => {
                let id = uuid(&p)?;
                if let Some(mut edit) = self.edits.remove(&id) {
                    edit.field_store
                        .release_auxiliary(edit.field_handle)
                        .await?;
                    edit.field_budget = 0;
                } else if self.batch_edits.remove(&id).is_none() {
                    return Err(AppError::invalid("编辑令牌已失效"));
                }
                Ok(json!({"discarded":true}))
            }
            "change.preview" if self.is_batch_request(&p)? => self.preview_batch_delete(&p, cancel),
            "change.preview" => self.preview(&p, cancel).await,
            "change.submit" if self.is_batch_request(&p)? => {
                self.submit_batch_delete(&p, sink, cancel).await
            }
            "change.submit" => self.submit(&p, sink, cancel).await,
            "execution.run" => self.run(&p, sink, cancel).await,
            "export.meta" => self.export_meta(&p).await,
            "export.read" => self.export_read(&p).await,
            "import.begin" => self.import_begin(&p, sink).await,
            "import.append" => self.import_append(&p, sink).await,
            "import.finish" => self.import_finish(&p, sink).await,
            "import.discard" => self.import_discard(&p, sink).await,
            _ => Err(AppError::invalid(format!("未知 Redis 方法 {method}"))),
        }
    }
    async fn read_resource(
        &mut self,
        method: &str,
        p: &Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
        conn: &mut Conn,
    ) -> Result<Json> {
        match method {
            "connection.info" => {
                let info: String = query(conn, &args(&["INFO", "server"])).await?;
                let version = info
                    .lines()
                    .find_map(|line| line.strip_prefix("redis_version:"))
                    .unwrap_or("")
                    .trim();
                Ok(
                    json!({"server_version":version,"database":if self.command_dead { None } else { Some(self.command_db) },"browse_database":self.profile.redis_db(),"transaction":self.command_transaction.is_some()}),
                )
            }
            "resource.list" => self.list(p, sink, cancel, conn).await,
            "resource.describe" => Self::describe(conn, target(p, &self.profile)?).await,
            "resource.browse" => {
                let t = target(p, &self.profile)?;
                let k = key(&t)?;
                let type_args = vec![b"TYPE".to_vec(), k.clone()];
                record_browse_command(sink, &type_args).await?;
                let kind: String = query(conn, &type_args).await?;
                let ttl_args = vec![b"PTTL".to_vec(), k.clone()];
                record_browse_command(sink, &ttl_args).await?;
                let ttl: i64 = query(conn, &ttl_args).await?;
                let id = browse(conn, &k, &kind, sink, cancel, true).await?;
                Ok(json!({"results":[id],"type":kind,"ttl_ms":ttl}))
            }
            _ => unreachable!(),
        }
    }
    async fn list(
        &mut self,
        p: &Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
        conn: &mut Conn,
    ) -> Result<Json> {
        let settings = sink.store.settings().await?;
        let budget = settings.limits.result_memory_bytes_per_client as usize;
        let response_budget = settings.limits.response_bytes as usize;
        let cursor = p.get("cursor").filter(|v| !v.is_null());
        let fresh = cursor.is_none();
        let id = if let Some(v) = cursor {
            v.as_str()
                .and_then(|s| Uuid::parse_str(s).ok())
                .ok_or_else(|| AppError::invalid("无效扫描令牌"))?
        } else {
            let id = Uuid::new_v4();
            self.scans.insert(
                id,
                KeyScan {
                    cursor: 0,
                    complete: false,
                    limited: false,
                    pattern: p.get("pattern").and_then(Json::as_str).map(str::to_owned),
                    kind: p.get("type").and_then(Json::as_str).map(str::to_owned),
                    seen: HashSet::new(),
                    pending: VecDeque::new(),
                    bytes: 0,
                    store: sink.store.clone(),
                    execution_id: sink.execution_id,
                },
            );
            id
        };
        let limit = p
            .get("limit")
            .and_then(Json::as_u64)
            .unwrap_or(settings.paging.redis_key_batch as u64)
            .clamp(1, 1000) as usize;
        let other_bytes: usize = self
            .scans
            .iter()
            .filter(|(key, _)| **key != id)
            .map(|(_, scan)| scan.bytes)
            .sum();
        let budget = budget.saturating_sub(other_bytes);
        let result: Result<ResourcePage> = async {
            let scan = self
                .scans
                .get_mut(&id)
                .ok_or_else(|| AppError::invalid("扫描令牌已失效"))?;
            if p.get("pattern")
                .and_then(Json::as_str)
                .is_some_and(|s| Some(s) != scan.pattern.as_deref())
                || p.get("type")
                    .and_then(Json::as_str)
                    .is_some_and(|s| Some(s) != scan.kind.as_deref())
            {
                return Err(AppError::invalid("扫描条件与令牌不匹配"));
            }
            let mut resources = Vec::with_capacity(limit);
            let mut size = 0usize;
            loop {
                ensure_not_cancelled(cancel)?;
                while let Some((key, kind)) = scan.pending.front() {
                    let resource = Resource {
                        name: String::from_utf8_lossy(key).into_owned(),
                        kind: kind.clone(),
                        database: Some(self.profile.redis_db().to_string()),
                        key_b64: Some(STANDARD.encode(key)),
                        data: Json::Null,
                    };
                    let bytes = serde_json::to_vec(&resource)?.len() + 33;
                    if size.saturating_add(bytes) > response_budget.saturating_sub(1024) {
                        if resources.is_empty() {
                            return Err(AppError::new(
                                "result_too_large",
                                "单个 Redis key 超过分页字节限制",
                            ));
                        }
                        break;
                    }
                    size += bytes;
                    resources.push(resource);
                    scan.pending.pop_front();
                    if resources.len() == limit {
                        break;
                    }
                }
                if resources.len() == limit
                    || !scan.pending.is_empty()
                    || scan.complete
                    || scan.limited
                {
                    break;
                }
                let mut command = redis::cmd("SCAN");
                command.arg(scan.cursor).arg("COUNT").arg(limit);
                if let Some(pattern) = &scan.pattern {
                    command.arg("MATCH").arg(pattern);
                }
                if let Some(kind) = &scan.kind {
                    command.arg("TYPE").arg(kind);
                }
                let (next, keys): (u64, Vec<Vec<u8>>) = command.query_async(&mut *conn).await?;
                let mut types = redis::pipe();
                for key in &keys {
                    types.cmd("TYPE").arg(key);
                }
                let kinds: Vec<String> = if keys.is_empty() {
                    Vec::new()
                } else {
                    types.query_async(&mut *conn).await?
                };
                if kinds.len() != keys.len() {
                    return Err(AppError::new("database", "Redis 类型回复数量不匹配"));
                }
                let extra = keys
                    .iter()
                    .zip(&kinds)
                    .filter(|(key, kind)| {
                        kind.as_str() != "none"
                            && scan
                                .kind
                                .as_deref()
                                .is_none_or(|expected| expected == kind.as_str())
                            && !scan.seen.contains(*key)
                    })
                    .fold(0usize, |size, (key, kind)| {
                        size.saturating_add(
                            key.len()
                                .saturating_mul(2)
                                .saturating_add(kind.len())
                                .saturating_add(128),
                        )
                    });
                let reserved = scan.bytes.saturating_add(extra);
                if reserved > budget {
                    scan.limited = true;
                } else if let Err(error) = scan
                    .store
                    .reserve_auxiliary(scan.execution_id, id, reserved)
                    .await
                {
                    if error.code != "resource_limit" {
                        return Err(error);
                    }
                    scan.limited = true;
                } else {
                    scan.bytes = reserved;
                    for (key, kind) in keys.into_iter().zip(kinds) {
                        if kind == "none"
                            || scan
                                .kind
                                .as_deref()
                                .is_some_and(|expected| expected != kind)
                            || !scan.seen.insert(key.clone())
                        {
                            continue;
                        }
                        scan.pending.push_back((key, kind));
                    }
                }
                scan.cursor = next;
                scan.complete = next == 0 && !scan.limited;
            }
            if !resources.is_empty() {
                let mut ttl_query = redis::pipe();
                for resource in &resources {
                    let key = STANDARD
                        .decode(resource.key_b64.as_ref().unwrap())
                        .map_err(|e| AppError::invalid(e.to_string()))?;
                    ttl_query.cmd("PTTL").arg(key);
                }
                let ttls: Vec<i64> = ttl_query.query_async(&mut *conn).await?;
                if ttls.len() != resources.len() {
                    return Err(AppError::new("database", "Redis TTL 回复数量不匹配"));
                }
                for (resource, ttl) in resources.iter_mut().zip(ttls) {
                    resource.data = json!({"ttl_ms":ttl});
                }
            }
            let complete = scan.complete && scan.pending.is_empty();
            Ok(ResourcePage {
                resources,
                cursor: if complete || (scan.limited && scan.pending.is_empty()) {
                    None
                } else {
                    Some(id.to_string())
                },
                complete,
            })
        }
        .await;
        let limited = self.scans.get(&id).is_some_and(|scan| scan.limited);
        if ((fresh && result.is_err()) || result.as_ref().is_ok_and(|page| page.cursor.is_none()))
            && let Some(scan) = self.scans.remove(&id)
        {
            scan.store.release_auxiliary(id).await?;
        }
        let mut value = serde_json::to_value(result?)?;
        value["resource_limited"] = json!(limited);
        Ok(value)
    }
    async fn describe(conn: &mut Conn, t: ResourceTarget) -> Result<Json> {
        let k = key(&t)?;
        let (kind, ttl) = metadata(conn, &k).await?;
        let cmd = match kind.as_str() {
            "string" => "STRLEN",
            "hash" => "HLEN",
            "list" => "LLEN",
            "set" => "SCARD",
            "zset" => "ZCARD",
            "none" => return Ok(json!({"target":t,"type":kind,"ttl_ms":ttl,"length":0})),
            _ => return Err(AppError::invalid("不支持的 Redis 类型")),
        };
        let n: i64 = query(conn, &[cmd.as_bytes().to_vec(), k]).await?;
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
        let result = browse(&mut conn, &k, &kind, sink, cancel, false).await?;
        let id = Uuid::new_v4();
        let field_owner = sink.store.task(sink.execution_id).await?.owner;
        self.edits.insert(
            id,
            Edit {
                conn,
                key: k,
                kind: kind.clone(),
                result_id: result,
                expires_at_ms,
                field_expiries: HashMap::new(),
                field_expiry_supported: None,
                field_budget: 0,
                field_owner,
                field_store: sink.store.clone(),
                field_handle: id,
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
    let direct = profile.direct()?;
    if let Some(s) = secret {
        let mut a = vec![b"AUTH".to_vec()];
        if !direct.username.is_empty() {
            a.push(direct.username.as_bytes().to_vec());
        }
        a.push(s.as_bytes().to_vec());
        let _: String = query(&mut c, &a).await?;
    } else if !direct.username.is_empty() {
        return Err(AppError::invalid("ACL 用户需要凭据"));
    }
    if direct.redis_db != 0 {
        let _: String = query(
            &mut c,
            &[b"SELECT".to_vec(), direct.redis_db.to_string().into_bytes()],
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
    demand_driven: bool,
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
    let page_size = sink
        .store
        .settings()
        .await?
        .paging
        .redis_value_rows
        .clamp(1, 1000);
    let mut produced = 0u64;
    sink.store
        .update_result_summary(
            id,
            json!({"retrieval_mode":"single_execution_stream","has_more":true,"redis_type":kind}),
        )
        .await?;
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
                    (off + page_size as i64 - 1).to_string().into_bytes(),
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
                produced += n as u64;
                if n < page_size {
                    break;
                }
                if demand_driven && !sink.wait_demand(id, produced, cancel).await? {
                    return Err(AppError::cancelled());
                }
            }
        }
        "hash" | "set" | "zset" => {
            let mut seen = HashSet::new();
            let mut seen_bytes = 0usize;
            let budget = sink
                .store
                .settings()
                .await?
                .limits
                .result_memory_bytes_per_client as usize;
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
                        page_size.to_string().into_bytes(),
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
                        if seen.contains(&member) {
                            continue;
                        }
                        seen_bytes = seen_bytes.saturating_add(member.len()).saturating_add(64);
                        if seen_bytes > budget {
                            return Err(AppError::new("resource_limit", "Redis 扫描超过内存预算"));
                        }
                        sink.store
                            .reserve_auxiliary(sink.execution_id, id, seen_bytes)
                            .await?;
                        seen.insert(member.clone());
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
                    let mut chunks = rows.chunks(page_size).peekable();
                    while let Some(chunk) = chunks.next() {
                        sink.append(id, chunk.to_vec()).await?;
                        produced += chunk.len() as u64;
                        if demand_driven
                            && (next != 0 || chunks.peek().is_some())
                            && !sink.wait_demand(id, produced, cancel).await?
                        {
                            return Err(AppError::cancelled());
                        }
                    }
                    cursor = next;
                    if cursor == 0 {
                        break;
                    }
                }
                Ok(())
            }
            .await;
            sink.store.release_auxiliary(id).await?;
            scan?;
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
    sink.finish(
        id,
        json!({"complete":true,"has_more":false,"redis_type":kind}),
    )
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
