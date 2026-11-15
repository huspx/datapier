use super::*;
pub(super) fn supports_field_expiry(info: &str) -> Result<bool> {
    let version = info
        .lines()
        .find_map(|line| line.strip_prefix("redis_version:"))
        .ok_or_else(|| AppError::new("database", "无法确定 Redis 字段期限能力"))?;
    let mut parts = version.split('.');
    let major: u32 = parts
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| AppError::new("database", "无效 Redis 版本"))?;
    let minor: u32 = parts
        .next()
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| AppError::new("database", "无效 Redis 版本"))?;
    Ok(major > 7 || major == 7 && minor >= 4)
}

#[test]
fn redis_field_expiry_version_support() {
    assert!(!supports_field_expiry("redis_version:6.2.14\r\n").unwrap());
    assert!(!supports_field_expiry("redis_version:7.2.5\r\n").unwrap());
    assert!(supports_field_expiry("redis_version:7.4.0\r\n").unwrap());
    assert!(supports_field_expiry("redis_version:8.0.0\r\n").unwrap());
    assert!(supports_field_expiry("unknown").is_err());
}

use crate::model::transfer::ConflictPolicy;

/// 已验证的导入临时对象身份，可用于崩溃后的显式清理。
pub struct RedisTemporaryIdentity {
    args: Vec<Vec<u8>>,
}

impl RedisTemporaryIdentity {
    pub fn new(identity: &crate::storage::transfer::ArtifactIdentity) -> Result<Self> {
        let invalid = || AppError::new("recovery_conflict", "Redis 临时对象登记身份无效");
        let decode = |value: Option<&str>| -> Result<Vec<u8>> {
            STANDARD
                .decode(value.ok_or_else(invalid)?)
                .map_err(|_| invalid())
        };
        let temp = decode(identity.temp_b64.as_deref())?;
        let marker = decode(identity.marker_b64.as_deref())?;
        let pending = identity
            .pending_marker_b64
            .as_deref()
            .map(|value| decode(Some(value)))
            .transpose()?;
        let valid_marker = |value: &[u8]| {
            std::str::from_utf8(value)
                .ok()
                .and_then(|s| Uuid::parse_str(s).ok())
                .is_some()
        };
        let valid_temp = std::str::from_utf8(&temp)
            .ok()
            .and_then(|s| s.strip_prefix("__datapier_import_"))
            .and_then(|s| Uuid::parse_str(s).ok())
            .is_some();
        let kind = identity.kind.as_deref().ok_or_else(invalid)?;
        if !valid_temp
            || !valid_marker(&marker)
            || pending.as_deref().is_some_and(|m| !valid_marker(m))
            || !matches!(kind, "string" | "hash" | "list" | "set" | "zset")
        {
            return Err(invalid());
        }
        Ok(Self {
            args: import_cleanup_args_parts(&temp, kind, &marker, pending.as_deref()),
        })
    }
}

impl RedisSession {
    /// 活跃导入持有临时对象时，恢复操作不能接管它。
    pub fn owns_temporary_artifact(&self, id: Uuid) -> bool {
        self.imports.values().any(|import| import.artifact_id == id)
    }

    /// 调用者必须在发送前持久化结果未知的事实；发生 I/O 错误后不能自动重发。
    pub async fn cleanup_temporary(&mut self, identity: &RedisTemporaryIdentity) -> Result<()> {
        self.profile.ensure_write()?;
        let mut cmd = redis::cmd("EVAL");
        for arg in &identity.args[1..] {
            cmd.arg(arg);
        }
        let result: i64 = cmd
            .query_async(
                self.conn
                    .as_mut()
                    .ok_or_else(|| AppError::new("disconnected", "Redis 连接已关闭"))?,
            )
            .await
            .map_err(error_unknown)?;
        match result {
            0 | 1 => Ok(()),
            -1 => Err(AppError::new(
                "recovery_conflict",
                "Redis 临时对象归属不匹配",
            )),
            _ => Err(AppError::new("database", "无效的 Redis 清理响应")),
        }
    }
}

impl RedisSession {
    pub(super) async fn import_begin(&mut self, p: &Json, sink: &ExecutionSink) -> Result<Json> {
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
        let conflict: ConflictPolicy = match p.get("conflict_policy") {
            Some(value) => serde_json::from_value(value.clone())
                .map_err(|e| AppError::invalid(e.to_string()))?,
            None => ConflictPolicy::Insert,
        };
        // WATCH 必须占有独立物理连接，直至最终 EXEC 或丢弃。
        let mut watch_conn =
            connect_database(&self.client, &self.profile, self.secret.as_deref()).await?;
        let _: String = query(&mut watch_conn, &[b"WATCH".to_vec(), k.clone()]).await?;
        let _: i64 = query(&mut watch_conn, &[b"EXISTS".to_vec(), k.clone()]).await?;
        let id = Uuid::new_v4();
        let temp = format!("__datapier_import_{id}").into_bytes();
        // 标记保存在临时对象本身，避免单独的所有权 key 在对象被替换后仍然有效。
        let marker = Uuid::new_v4().to_string().into_bytes();
        // 在发送创建请求前记账：响应丢失时 close 仍可按标记清理。
        let artifact_id = sink
            .register_transfer_artifact(
                "redis_temporary",
                json!({
                    "profile_id": self.profile.id, "redis_db": self.profile.redis_db(),
                    "temp_b64": STANDARD.encode(&temp), "marker_b64": STANDARD.encode(&marker),
                    "pending_marker_b64": null, "kind": kind,
                }),
            )
            .await?;
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
                watch_conn,
                conflict,
                artifact_id,
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
            self.conn.as_mut().unwrap(),
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
            sink.finish_transfer_artifact(artifact_id, "cleaned")
                .await?;
            return Err(AppError::conflict("临时 key 已存在"));
        }
        self.imports.get_mut(&id).unwrap().valid = true;
        Ok(
            json!({"token":id,"temporary_key_b64":STANDARD.encode(&self.imports[&id].temp),"marker_b64":STANDARD.encode(&self.imports[&id].marker),"type":self.imports[&id].kind}),
        )
    }
    pub(super) async fn import_append(&mut self, p: &Json, sink: &ExecutionSink) -> Result<Json> {
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
                ("hash", [f, v, Value::Integer(expiry)]) => {
                    if expiry.parse::<i64>().ok().is_none_or(|n| n < -1) {
                        return Err(AppError::invalid("无效 Hash 字段期限"));
                    }
                    vec![
                        b"HSET".to_vec(),
                        k,
                        bytes(f)?,
                        bytes(v)?,
                        expiry.as_bytes().to_vec(),
                    ]
                }
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
        let mut batch_bytes = 0usize;
        for command in &commands {
            for value in command.iter().skip(2) {
                if value.len() > crate::model::transfer::MAX_VALUE_BYTES {
                    return Err(AppError::invalid("Redis 单值超过迁移限制"));
                }
                batch_bytes = batch_bytes
                    .checked_add(value.len())
                    .ok_or_else(|| AppError::invalid("Redis 批次过大"))?;
            }
        }
        if commands.len() > 1 && batch_bytes > crate::model::transfer::MAX_BATCH_BYTES {
            return Err(AppError::invalid("Redis 批次超过字节限制"));
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
elseif k == 'hash' then
    r = redis.pcall(unpack(ARGV, 4, #ARGV - 1))
    if not (type(r) == 'table' and r.err) and ARGV[#ARGV] ~= '-1' then
        r = redis.pcall('HPEXPIREAT', KEYS[1], ARGV[#ARGV], 'FIELDS', 1, ARGV[6])
        if type(r) == 'table' and not r.err and r[1] ~= 1 then
            r = {{err = 'hash field expired during import'}}
        end
    end
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
                sink.update_transfer_artifact(
                    import.artifact_id,
                    json!({
                        "profile_id": self.profile.id, "redis_db": self.profile.redis_db(),
                        "temp_b64": STANDARD.encode(&import.temp),
                        "marker_b64": STANDARD.encode(&import.marker),
                        "pending_marker_b64": STANDARD.encode(&next_marker), "kind": import.kind,
                    }),
                )
                .await?;
            }
            let res: Result<i64> = send_write(self.conn.as_mut().unwrap(), &wrapped, sink).await;
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
    pub(super) async fn import_finish(&mut self, p: &Json, sink: &ExecutionSink) -> Result<Json> {
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
        // 自重命名检查发布命令对各 key 的权限，不改变数据且兼容 Redis 6.2。
        // 临时值先完成清理和期限设置，最后一次操作才原子替换目标。
        let script = format!(
            r#"{IMPORT_OWNED}
if not owned(ARGV[2]) then return -2 end
local t = redis.call('TIME')
local now = t[1] * 1000 + math.floor(t[2] / 1000)
local expiry = tonumber(ARGV[3])
if expiry and expiry <= now then redis.call('DEL', KEYS[1]); return -1 end
local exists = redis.call('EXISTS', KEYS[2]) ~= 0
if exists and ARGV[4] ~= 'overwrite' then
    if ARGV[4] == 'skip' then redis.call('DEL', KEYS[1]); return -3 end
    return 0
end
local cmd = ARGV[4] == 'overwrite' and 'RENAME' or 'RENAMENX'
local probe = redis.pcall(cmd, KEYS[1], KEYS[1])
if type(probe) == 'table' and probe.err then return probe end
probe = redis.pcall(cmd, KEYS[2], KEYS[2])
if type(probe) == 'table' and probe.err and (exists or not string.find(string.lower(probe.err), 'no such key', 1, true)) then return probe end
local result
if expiry then result = redis.pcall('PEXPIREAT', KEYS[1], ARGV[3])
else result = redis.pcall('PERSIST', KEYS[1]) end
if type(result) == 'table' and result.err then return result end
local k, m = ARGV[1], ARGV[2]
if k == 'string' then
    local v = redis.call('GET', KEYS[1])
    result = redis.pcall('SET', KEYS[1], string.sub(v, string.len(m) + 1), 'KEEPTTL')
elseif k == 'hash' then result = redis.pcall('HDEL', KEYS[1], m)
elseif k == 'list' then result = redis.pcall('LPOP', KEYS[1])
elseif k == 'set' then result = redis.pcall('SREM', KEYS[1], m)
else result = redis.pcall('ZREM', KEYS[1], m) end
if type(result) == 'table' and result.err then return result end
if cmd == 'RENAME' then
    redis.call(cmd, KEYS[1], KEYS[2])
    return 1
end
return redis.call(cmd, KEYS[1], KEYS[2])
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
            match import.conflict {
                ConflictPolicy::Insert => b"insert".to_vec(),
                ConflictPolicy::Skip => b"skip".to_vec(),
                ConflictPolicy::Overwrite => b"overwrite".to_vec(),
            },
        ];
        let _: String = query(&mut import.watch_conn, &args(&["MULTI"])).await?;
        let _: String = query(&mut import.watch_conn, &a).await?;
        let result: Result<Option<Vec<i64>>> =
            send_write(&mut import.watch_conn, &args(&["EXEC"]), sink).await;
        let code = match result {
            Ok(Some(values)) if values.len() == 1 => values[0],
            Ok(None) => {
                import.valid = false;
                return Err(AppError::conflict("目标 key 在构建期间已变化"));
            }
            Ok(_) => {
                import.valid = false;
                return Err(AppError::new("database", "无效 EXEC 响应"));
            }
            Err(e) => {
                import.valid = false;
                if e.code == "outcome_unknown" {
                    sink.finish_transfer_artifact(import.artifact_id, "outcome_unknown")
                        .await?;
                }
                return Err(e);
            }
        };
        match code {
            1 => {
                let artifact_id = import.artifact_id;
                self.imports.remove(&id);
                sink.finish_transfer_artifact(artifact_id, "published")
                    .await?;
                Ok(json!({"published":true}))
            }
            -1 => {
                let artifact_id = import.artifact_id;
                self.imports.remove(&id);
                sink.finish_transfer_artifact(artifact_id, "cleaned")
                    .await?;
                Ok(json!({"skipped":true,"reason":"expired"}))
            }
            -3 => {
                let artifact_id = import.artifact_id;
                self.imports.remove(&id);
                sink.finish_transfer_artifact(artifact_id, "cleaned")
                    .await?;
                Ok(json!({"skipped":true,"reason":"exists"}))
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
    pub(super) async fn import_discard(&mut self, p: &Json, sink: &ExecutionSink) -> Result<Json> {
        self.profile.ensure_write()?;
        let id = uuid(p)?;
        let import = self
            .imports
            .get(&id)
            .ok_or_else(|| AppError::invalid("导入令牌已失效"))?;
        let artifact_id = import.artifact_id;
        let result: Result<i64> = send_write(
            self.conn.as_mut().unwrap(),
            &import_cleanup_args(import),
            sink,
        )
        .await;
        match result {
            Ok(-1) => {
                self.imports.remove(&id);
                sink.finish_transfer_artifact(artifact_id, "recovery_conflict")
                    .await?;
                return Err(AppError::new(
                    "recovery_conflict",
                    "Redis 临时对象归属不匹配",
                ));
            }
            Ok(_) => {
                self.imports.remove(&id);
                sink.finish_transfer_artifact(artifact_id, "cleaned")
                    .await?;
            }
            Err(e) => {
                sink.finish_transfer_artifact(artifact_id, "cleanup_pending")
                    .await?;
                return Err(e);
            }
        }
        Ok(json!({"discarded":true}))
    }
}
