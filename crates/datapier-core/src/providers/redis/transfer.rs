use super::*;
fn supports_field_expiry(info: &str) -> Result<bool> {
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

impl RedisSession {
    pub(super) async fn export_meta(&mut self, p: &Json) -> Result<Json> {
        let t = target(p, &self.profile)?;
        let k = key(&t)?;
        // 用 Redis 服务端时间换算绝对期限，导入时不会因传输时间延长寿命。
        let script = "local t=redis.call('TYPE',KEYS[1]).ok; local ttl=redis.call('PTTL',KEYS[1]); if ttl==-2 then return {t, false} end; if ttl==-1 then return {t, false} end; local now=redis.call('TIME'); return {t, tostring(now[1]*1000+math.floor(now[2]/1000)+ttl)}";
        let (kind, expires): (String, Option<String>) = query(
            self.conn.as_mut().unwrap(),
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
    pub(super) async fn export_read(&mut self, p: &Json) -> Result<Json> {
        let t = target(p, &self.profile)?;
        let k = key(&t)?;
        let kind: String = param(p, "type")?;
        let position = p.get("cursor").and_then(Json::as_u64).unwrap_or(0);
        let conn = self.conn.as_mut().unwrap();
        let actual: String = query(conn, &[b"TYPE".to_vec(), k.clone()]).await?;
        if actual != kind {
            return Err(AppError::conflict("导出期间 key 类型发生变化"));
        }
        let field_expiry_supported = if kind == "hash" {
            let info: String = query(conn, &args(&["INFO", "server"])).await?;
            supports_field_expiry(&info)?
        } else {
            false
        };
        let (rows, next): (Vec<Vec<Value>>, Option<u64>) = match kind.as_str() {
            "string" => {
                let end = position
                    .checked_add(49151)
                    .ok_or_else(|| AppError::invalid("Redis 字段偏移无效"))?;
                let value: Vec<u8> = query(
                    conn,
                    &[
                        b"GETRANGE".to_vec(),
                        k,
                        position.to_string().into_bytes(),
                        end.to_string().into_bytes(),
                    ],
                )
                .await?;
                let next = if value.len() == 49152 {
                    Some(position + 49152)
                } else {
                    None
                };
                (vec![vec![val(value)]], next)
            }
            "list" => {
                let values: Vec<Vec<u8>> = query(
                    conn,
                    &[
                        b"LRANGE".to_vec(),
                        k,
                        position.to_string().into_bytes(),
                        (position + 99).to_string().into_bytes(),
                    ],
                )
                .await?;
                let count = values.len();
                let rows = values
                    .into_iter()
                    .enumerate()
                    .map(|(i, value)| {
                        vec![
                            Value::Integer((position + i as u64).to_string()),
                            val(value),
                        ]
                    })
                    .collect();
                (
                    rows,
                    if count == 100 {
                        Some(position + 100)
                    } else {
                        None
                    },
                )
            }
            "hash" | "set" | "zset" => {
                let cmd = match kind.as_str() {
                    "hash" => "HSCAN",
                    "set" => "SSCAN",
                    _ => "ZSCAN",
                };
                let (cursor, values): (u64, Vec<Vec<u8>>) = query(
                    conn,
                    &[
                        cmd.as_bytes().to_vec(),
                        k.clone(),
                        position.to_string().into_bytes(),
                        b"COUNT".to_vec(),
                        b"100".to_vec(),
                    ],
                )
                .await?;
                if kind != "set" && values.len() % 2 != 0 {
                    return Err(AppError::new("database", "无效扫描响应"));
                }
                let mut rows = Vec::new();
                let mut iter = values.into_iter();
                while let Some(member) = iter.next() {
                    let mut row = vec![val(member.clone())];
                    if kind != "set" {
                        let value = iter.next().unwrap();
                        row.push(if kind == "hash" {
                            val(value)
                        } else {
                            Value::Float(
                                String::from_utf8(value)
                                    .map_err(|e| AppError::new("database", e.to_string()))?,
                            )
                        });
                        if kind == "hash" {
                            let expiry = if field_expiry_supported {
                                let expiry: Vec<i64> = query(
                                    conn,
                                    &[
                                        b"HPEXPIRETIME".to_vec(),
                                        k.clone(),
                                        b"FIELDS".to_vec(),
                                        b"1".to_vec(),
                                        member,
                                    ],
                                )
                                .await?;
                                *expiry.first().ok_or_else(|| {
                                    AppError::new("database", "缺少 Hash 字段期限")
                                })?
                            } else {
                                -1
                            };
                            if expiry < -1 {
                                return Err(AppError::conflict("导出期间 Hash 字段消失"));
                            }
                            row.push(Value::Integer(expiry.to_string()));
                        }
                    }
                    rows.push(row);
                }
                (rows, if cursor == 0 { None } else { Some(cursor) })
            }
            _ => return Err(AppError::invalid("不支持的 Redis 类型")),
        };
        Ok(json!({"rows":rows,"next":next}))
    }
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
        let exists: i64 = query(
            self.conn.as_mut().unwrap(),
            &[b"EXISTS".to_vec(), k.clone()],
        )
        .await?;
        if exists != 0 {
            let (sec, micro): (i64, i64) =
                query(self.conn.as_mut().unwrap(), &args(&["TIME"])).await?;
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
            return Err(AppError::conflict("临时 key 已存在"));
        }
        self.imports.get_mut(&id).unwrap().valid = true;
        Ok(json!({"token":id}))
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
-- The destination is absent and scripts are atomic, so these probes cannot
-- change data. They check the same command and key used after RENAMENX.
local probe
if k == 'string' then
    probe = redis.pcall('GET', KEYS[2])
    if type(probe) == 'table' and probe.err then return probe end
    probe = redis.pcall('SET', KEYS[2], '', 'XX', 'KEEPTTL')
elseif k == 'hash' then probe = redis.pcall('HDEL', KEYS[2], m)
elseif k == 'list' then probe = redis.pcall('LPOP', KEYS[2])
elseif k == 'set' then probe = redis.pcall('SREM', KEYS[2], m)
else probe = redis.pcall('ZREM', KEYS[2], m) end
if type(probe) == 'table' and probe.err then return probe end
if expiry then probe = redis.pcall('PEXPIREAT', KEYS[2], ARGV[3])
else probe = redis.pcall('PERSIST', KEYS[2]) end
if type(probe) == 'table' and probe.err then return probe end
local published = redis.pcall('RENAMENX', KEYS[1], KEYS[2])
if type(published) == 'table' and published.err then return published end
if published == 0 then return 0 end
local dst = KEYS[2]
local result
if k == 'string' then
    local v = redis.pcall('GET', dst)
    if type(v) == 'table' and v.err then return -3 end
    result = redis.pcall('SET', dst, string.sub(v, string.len(m) + 1), 'KEEPTTL')
elseif k == 'hash' then result = redis.pcall('HDEL', dst, m)
elseif k == 'list' then result = redis.pcall('LPOP', dst)
elseif k == 'set' then result = redis.pcall('SREM', dst, m)
else result = redis.pcall('ZREM', dst, m) end
if type(result) == 'table' and result.err then return -3 end
if expiry then result = redis.pcall('PEXPIREAT', dst, ARGV[3])
else result = redis.pcall('PERSIST', dst) end
if type(result) == 'table' and result.err then return -3 end
return 1
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
        let code: i64 = match send_write(self.conn.as_mut().unwrap(), &a, sink).await {
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
            -3 => {
                import.temp = import.key.clone();
                import.valid = false;
                Err(AppError::new(
                    "import_failed",
                    "发布后未能完成清理，目标保留有限期限",
                ))
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
        let _: i64 = send_write(
            self.conn.as_mut().unwrap(),
            &import_cleanup_args(import),
            sink,
        )
        .await?;
        self.imports.remove(&id);
        Ok(json!({"discarded":true}))
    }
}
