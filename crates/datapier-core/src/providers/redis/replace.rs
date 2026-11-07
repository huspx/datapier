//! Redis 批量替换：单连接 WATCH、完整内存基线与固定脚本。
use super::*;
use crate::execution::replace::CompiledReplacement;
use crate::model::replace::{ReplaceScope, ReplaceSpec};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

const MAX_KEYS: usize = 128;
const MAX_PLAN: usize = 32 * 1024 * 1024;
const MAX_WRITE: usize = 4 * 1024 * 1024;
const MAX_COMMANDS: usize = 1000;

// 所有 key 只通过 KEYS 传递。先检查类型、身份和权限，再逐条 pcall；
// Redis 脚本隔离执行，但运行错误不能回滚已成功的写入。
const SCRIPT: &str = r#"
local p=1
local function take() local v=ARGV[p]; p=p+1; return v end
local checks=tonumber(take())
for i=1,checks do
  local kind=take(); local k=KEYS[tonumber(take())]; local arg=take(); local expected=take()
  local r
  if kind=='type' then r=redis.call('TYPE',k).ok
  elseif kind=='exists' then r=tostring(redis.call('EXISTS',k))
  elseif kind=='hexists' then r=tostring(redis.call('HEXISTS',k,arg))
  elseif kind=='sismember' then r=tostring(redis.call('SISMEMBER',k,arg))
  elseif kind=='zscore' then r=redis.call('ZSCORE',k,arg) and '1' or '0'
  else return {'invalid_check',tostring(i)} end
  if r~=expected then return {'conflict',tostring(i)} end
end
local count=tonumber(take()); local commands={}
for i=1,count do
  local item=take(); local key=KEYS[tonumber(take())]; local n=tonumber(take()); local c={take(),key}
  for j=1,n do local v=take(); if c[1]=='RENAMENX' and j==1 then v=KEYS[tonumber(v)] end; c[#c+1]=v end
  commands[i]={item,c}
end
if redis.acl_check_cmd then
  for i=1,count do
    if not redis.acl_check_cmd(unpack(commands[i][2])) then return {'permission',tostring(i)} end
  end
end
local out={'ok'}
for i=1,count do
  local r=redis.pcall(unpack(commands[i][2]))
  if type(r)=='table' and r.err then
    out[1]='partial'; out[#out+1]=tostring(i); out[#out+1]=r.err; return out
  end
  if commands[i][2][1]=='HPEXPIREAT' and (type(r)~='table' or r[1]~=1) then
    out[1]='partial'; out[#out+1]=tostring(i); out[#out+1]='Hash field 到期或恢复期限失败'; return out
  end
  if commands[i][2][1]=='RENAMENX' and r~=1 then
    out[1]='partial'; out[#out+1]=tostring(i); out[#out+1]='目标 key 已存在'; return out
  end
  out[#out+1]=commands[i][1]
end
return out
"#;

#[derive(Clone)]
struct Entry {
    identity: Vec<u8>,
    value: Vec<u8>,
    expiry: Option<String>,
}
struct Source {
    key: Vec<u8>,
    kind: String,
    entries: Vec<Entry>,
}
struct Item {
    source: usize,
    entry: Option<usize>,
    role: String,
    before: Vec<u8>,
    after: Vec<u8>,
    matches: usize,
    selected: bool,
    target_exists: bool,
}

/// 计划独占 WATCH 物理连接，丢弃或提交后连接一同释放。
pub(super) struct ReplacePlan {
    conn: Conn,
    id: Uuid,
    revision: u64,
    spec: ReplaceSpec,
    sources: Vec<Source>,
    items: Vec<Item>,
    non_utf8: usize,
    unchanged: usize,
    inapplicable: usize,
}

fn charge(used: &mut usize, amount: usize, limit: usize) -> Result<()> {
    *used = used
        .checked_add(amount)
        .ok_or_else(|| AppError::invalid("计划内存超限"))?;
    if *used > limit {
        return Err(AppError::new(
            "resource_limit",
            "Redis 替换计划超过内存预算",
        ));
    }
    Ok(())
}

struct ItemCollector<'a> {
    items: Vec<Item>,
    compiled: &'a CompiledReplacement,
    used: &'a mut usize,
    limit: usize,
    non_utf8: usize,
    unchanged: usize,
}
impl ItemCollector<'_> {
    fn add(&mut self, source: usize, entry: Option<usize>, role: &str, raw: &[u8]) -> Result<()> {
        let text = match std::str::from_utf8(raw) {
            Ok(v) => v,
            Err(_) => {
                self.non_utf8 += 1;
                return Ok(());
            }
        };
        let result = self
            .compiled
            .apply(text, self.limit.saturating_sub(*self.used))?;
        if !result.changed {
            self.unchanged += 1;
            return Ok(());
        }
        charge(
            self.used,
            raw.len()
                .saturating_add(result.value.len())
                .saturating_add(256),
            self.limit,
        )?;
        self.items.push(Item {
            source,
            entry,
            role: role.into(),
            before: raw.to_vec(),
            after: result.value.into_bytes(),
            matches: result.matches,
            selected: true,
            target_exists: false,
        });
        Ok(())
    }
}

async fn hash_expiry_supported(conn: &mut Conn) -> Result<bool> {
    let reply: redis::Value = query(conn, &args(&["COMMAND", "INFO", "HPEXPIRETIME"])).await?;
    match reply {
        redis::Value::Array(v) if v.len() == 1 => Ok(!matches!(v[0], redis::Value::Nil)),
        _ => Err(AppError::new(
            "database",
            "无法确认 Redis Hash field 到期能力",
        )),
    }
}

async fn load_source(
    conn: &mut Conn,
    key: Vec<u8>,
    kind: String,
    used: &mut usize,
    limit: usize,
    cancel: &CancellationToken,
) -> Result<Source> {
    charge(used, key.len() + 256, limit)?;
    let mut entries = Vec::new();
    match kind.as_str() {
        "string" => {
            // GETRANGE 限制单次响应，避免先把超预算 String 整体解码到客户端。
            let length: usize = query(conn, &[b"STRLEN".to_vec(), key.clone()]).await?;
            charge(used, length.saturating_add(128), limit)?;
            let mut value = Vec::with_capacity(length);
            while value.len() < length {
                ensure_not_cancelled(cancel)?;
                let start = value.len();
                let end = start.saturating_add(256 * 1024).min(length) - 1;
                let chunk: Vec<u8> = query(
                    conn,
                    &[
                        b"GETRANGE".to_vec(),
                        key.clone(),
                        start.to_string().into_bytes(),
                        end.to_string().into_bytes(),
                    ],
                )
                .await?;
                if chunk.is_empty() {
                    return Err(AppError::conflict("读取期间 String 已变化"));
                }
                value.extend_from_slice(&chunk);
            }
            entries.push(Entry {
                identity: vec![],
                value,
                expiry: None,
            });
        }
        "list" => {
            let mut offset = 0usize;
            loop {
                ensure_not_cancelled(cancel)?;
                let rows: Vec<Vec<u8>> = query(
                    conn,
                    &[
                        b"LRANGE".to_vec(),
                        key.clone(),
                        offset.to_string().into_bytes(),
                        (offset + 127).to_string().into_bytes(),
                    ],
                )
                .await?;
                let n = rows.len();
                for value in rows {
                    charge(used, value.len() + 128, limit)?;
                    entries.push(Entry {
                        identity: offset.to_string().into_bytes(),
                        value,
                        expiry: None,
                    });
                    offset += 1;
                }
                if n < 128 {
                    break;
                }
            }
        }
        "hash" | "set" | "zset" => {
            let expiry_supported = if kind == "hash" {
                hash_expiry_supported(conn).await?
            } else {
                false
            };
            let command = match kind.as_str() {
                "hash" => b"HSCAN".as_slice(),
                "set" => b"SSCAN".as_slice(),
                _ => b"ZSCAN".as_slice(),
            };
            let mut cursor = 0u64;
            let mut seen = HashSet::new();
            loop {
                ensure_not_cancelled(cancel)?;
                let (next, rows): (u64, Vec<Vec<u8>>) = query(
                    conn,
                    &[
                        command.to_vec(),
                        key.clone(),
                        cursor.to_string().into_bytes(),
                        b"COUNT".to_vec(),
                        b"128".to_vec(),
                    ],
                )
                .await?;
                if kind != "set" && rows.len() % 2 != 0 {
                    return Err(AppError::new("database", "无效 Redis 扫描响应"));
                }
                let mut iter = rows.into_iter();
                while let Some(identity) = iter.next() {
                    let value = if kind == "set" {
                        vec![]
                    } else {
                        iter.next()
                            .ok_or_else(|| AppError::new("database", "无效扫描响应"))?
                    };
                    if seen.contains(&identity) {
                        continue;
                    }
                    charge(
                        used,
                        identity.len().saturating_mul(2) + value.len() + 256,
                        limit,
                    )?;
                    seen.insert(identity.clone());
                    let expiry = if expiry_supported {
                        let replies: Vec<i64> = query(
                            conn,
                            &[
                                b"HPEXPIRETIME".to_vec(),
                                key.clone(),
                                b"FIELDS".to_vec(),
                                b"1".to_vec(),
                                identity.clone(),
                            ],
                        )
                        .await?;
                        match replies.as_slice() {
                            [-1] => Some("-1".into()),
                            [n] if *n >= 0 => Some(n.to_string()),
                            [-2] => return Err(AppError::conflict("Hash field 已过期")),
                            _ => {
                                return Err(AppError::new(
                                    "database",
                                    "无法读取 Hash field 到期时间",
                                ));
                            }
                        }
                    } else {
                        None
                    };
                    entries.push(Entry {
                        identity,
                        value,
                        expiry,
                    });
                }
                cursor = next;
                if cursor == 0 {
                    break;
                }
            }
        }
        _ => return Err(AppError::invalid("不支持的 Redis 类型")),
    }
    Ok(Source { key, kind, entries })
}

fn plan_id(p: &Json) -> Result<Uuid> {
    param(p, "plan_id")
}

impl ReplacePlan {
    fn digest(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.id.as_bytes());
        hash.update(self.revision.to_be_bytes());
        hash.update(serde_json::to_vec(&self.spec).expect("替换条件可序列化"));
        for item in &self.items {
            hash.update([u8::from(item.selected)]);
            hash.update((item.before.len() as u64).to_be_bytes());
            hash.update(&item.before);
            hash.update((item.after.len() as u64).to_be_bytes());
            hash.update(&item.after);
        }
        format!("{:x}", hash.finalize())
    }

    // 按实际 Vec/String 容量计入保留结构；临时扫描去重表在准备结束时释放。
    fn retained_bytes(&self) -> usize {
        let mut bytes = std::mem::size_of::<Self>()
            + self.spec.pattern.capacity()
            + self.spec.replacement.capacity()
            + (self.spec.columns.capacity() + self.spec.roles.capacity())
                * std::mem::size_of::<String>()
            + self
                .spec
                .columns
                .iter()
                .map(String::capacity)
                .sum::<usize>()
            + self.spec.roles.iter().map(String::capacity).sum::<usize>()
            + self.sources.capacity() * std::mem::size_of::<Source>()
            + self.items.capacity() * std::mem::size_of::<Item>();
        for source in &self.sources {
            bytes = bytes.saturating_add(
                source.key.capacity()
                    + source.kind.capacity()
                    + source.entries.capacity() * std::mem::size_of::<Entry>(),
            );
            for entry in &source.entries {
                bytes = bytes.saturating_add(
                    entry.identity.capacity()
                        + entry.value.capacity()
                        + entry.expiry.as_ref().map_or(0, String::capacity),
                );
            }
        }
        for item in &self.items {
            bytes = bytes.saturating_add(
                item.role.capacity() + item.before.capacity() + item.after.capacity(),
            );
        }
        bytes
    }

    fn collision_indices(&self) -> HashSet<usize> {
        let mut collisions = HashSet::new();
        let source_keys: HashSet<&[u8]> = self.sources.iter().map(|s| s.key.as_slice()).collect();
        let identities: Vec<HashSet<&[u8]>> = self
            .sources
            .iter()
            .map(|s| s.entries.iter().map(|e| e.identity.as_slice()).collect())
            .collect();
        let mut destinations: HashMap<(usize, &str, &[u8]), usize> = HashMap::new();
        for (index, item) in self.items.iter().enumerate().filter(|(_, i)| i.selected) {
            if !matches!(item.role.as_str(), "key" | "hashfield" | "member") {
                continue;
            }
            let group = if item.role == "key" {
                usize::MAX
            } else {
                item.source
            };
            let exists = if item.role == "key" {
                item.target_exists || source_keys.contains(item.after.as_slice())
            } else {
                identities[item.source].contains(item.after.as_slice())
            };
            if exists {
                collisions.insert(index);
            }
            if let Some(old) = destinations.insert((group, &item.role, &item.after), index) {
                collisions.insert(index);
                collisions.insert(old);
            }
        }
        collisions
    }

    fn summary(&self) -> Json {
        let collisions = self.collision_indices();
        json!({"id":self.id,"plan_id":self.id,"revision":self.revision,"state":"ready","complete":true,"total_items":self.items.len(),"plan_digest":self.digest(),"digest":self.digest(),"retained_bytes":self.retained_bytes(),"submittable":collisions.is_empty() && self.items.iter().any(|i| i.selected),"non_utf8":self.non_utf8,"unchanged":self.unchanged,"inapplicable":self.inapplicable,"collisions":collisions.len()})
    }

    fn check_revision(&self, p: &Json) -> Result<()> {
        let revision = p
            .get("expected_revision")
            .or_else(|| p.get("revision"))
            .and_then(Json::as_u64)
            .ok_or_else(|| AppError::invalid("缺少 revision"))?;
        if revision != self.revision {
            return Err(AppError::conflict("替换计划 revision 已变化"));
        }
        Ok(())
    }
}

fn short_value(raw: &[u8]) -> Json {
    let end = raw.len().min(768);
    json!({"base64":STANDARD.encode(&raw[..end]),"byte_len":raw.len(),"complete":end == raw.len()})
}

impl RedisSession {
    pub(super) async fn replace_prepare(
        &mut self,
        p: &Json,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        self.profile.ensure_write()?;
        let limit = match p.get("_max_plan_bytes") {
            None => MAX_PLAN,
            Some(v) => serde_json::from_value::<usize>(v.clone())
                .map_err(|_| AppError::invalid("无效计划内存预算"))?,
        };
        if limit == 0 {
            return Err(AppError::new(
                "resource_limit",
                "Redis 替换计划内存预算为零",
            ));
        }
        let spec: ReplaceSpec = param(p, "spec")?;
        if spec.scope != ReplaceScope::Selected {
            return Err(AppError::invalid("Redis 替换仅支持已选 key"));
        }
        if spec.roles.is_empty()
            || spec.roles.iter().any(|r| {
                !matches!(
                    r.as_str(),
                    "key" | "string_value" | "hashfield" | "hash_value" | "list_value" | "member"
                )
            })
        {
            return Err(AppError::invalid("无效 Redis 替换角色"));
        }
        if spec.roles.iter().collect::<HashSet<_>>().len() != spec.roles.len() {
            return Err(AppError::invalid("替换角色重复"));
        }
        let compiled = CompiledReplacement::new(&spec)?;
        let targets = p
            .get("targets")
            .and_then(Json::as_array)
            .filter(|v| !v.is_empty() && v.len() <= MAX_KEYS)
            .ok_or_else(|| AppError::invalid("请指定 1 到 128 个 key"))?;
        let mut keys = Vec::with_capacity(targets.len());
        let mut seen = HashSet::new();
        for value in targets {
            let target: ResourceTarget = serde_json::from_value(value.clone())?;
            if target
                .database
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok())
                != Some(self.profile.redis_db())
            {
                return Err(AppError::invalid("目标数据库与连接不一致"));
            }
            let encoded = target
                .key_b64
                .as_ref()
                .ok_or_else(|| AppError::invalid("缺少原始 key"))?;
            let key = STANDARD
                .decode(encoded)
                .map_err(|e| AppError::invalid(e.to_string()))?;
            if !seen.insert(key.clone()) {
                return Err(AppError::invalid("key 身份重复"));
            }
            keys.push((key, target.kind));
        }
        ensure_not_cancelled(cancel)?;
        let mut conn =
            connect_database(&self.client, &self.profile, self.secret.as_deref()).await?;
        let mut watch = vec![b"WATCH".to_vec()];
        watch.extend(keys.iter().map(|(k, _)| k.clone()));
        let _: String = query(&mut conn, &watch).await?;
        let mut used = std::mem::size_of::<ReplacePlan>()
            + spec.pattern.capacity()
            + spec.replacement.capacity()
            + spec.columns.iter().map(String::capacity).sum::<usize>()
            + spec.roles.iter().map(String::capacity).sum::<usize>();
        charge(&mut used, 0, limit)?;
        let mut sources = Vec::with_capacity(keys.len());
        for (key, expected) in keys {
            ensure_not_cancelled(cancel)?;
            let (kind, ttl) = metadata(&mut conn, &key).await?;
            if kind == "none" || ttl == -2 {
                return Err(AppError::conflict("key 已不存在"));
            }
            if kind != expected {
                return Err(AppError::conflict("key 类型已变化"));
            }
            if !matches!(kind.as_str(), "string" | "hash" | "list" | "set" | "zset") {
                return Err(AppError::invalid("不支持的 Redis 类型"));
            }
            let needs_entries = spec.roles.iter().any(|role| {
                matches!(
                    (kind.as_str(), role.as_str()),
                    ("string", "string_value")
                        | ("hash", "hashfield" | "hash_value")
                        | ("list", "list_value")
                        | ("set" | "zset", "member")
                )
            });
            if needs_entries {
                sources.push(load_source(&mut conn, key, kind, &mut used, limit, cancel).await?);
            } else {
                charge(&mut used, key.len() + 256, limit)?;
                sources.push(Source {
                    key,
                    kind,
                    entries: Vec::new(),
                });
            }
        }
        let mut collector = ItemCollector {
            items: Vec::new(),
            compiled: &compiled,
            used: &mut used,
            limit,
            non_utf8: 0,
            unchanged: 0,
        };
        let mut inapplicable = 0;
        for (si, source) in sources.iter().enumerate() {
            for role in &spec.roles {
                if role == "key" {
                    collector.add(si, None, role, &source.key)?;
                    continue;
                }
                let applicable = matches!(
                    (source.kind.as_str(), role.as_str()),
                    ("string", "string_value")
                        | ("hash", "hashfield" | "hash_value")
                        | ("list", "list_value")
                        | ("set" | "zset", "member")
                );
                if !applicable {
                    inapplicable += 1;
                    continue;
                }
                for (ei, entry) in source.entries.iter().enumerate() {
                    let raw = if matches!(role.as_str(), "hashfield" | "member") {
                        &entry.identity
                    } else {
                        &entry.value
                    };
                    collector.add(si, Some(ei), role, raw)?;
                }
            }
        }
        let ItemCollector {
            mut items,
            non_utf8,
            unchanged,
            ..
        } = collector;
        // 所有派生 key 均加入同一 WATCH；即使被暂时排除，稍后重新勾选仍受保护。
        let mut watch = vec![b"WATCH".to_vec()];
        let mut destinations = HashSet::new();
        for item in items.iter().filter(|i| i.role == "key") {
            if destinations.insert(item.after.clone()) {
                watch.push(item.after.clone());
            }
        }
        if watch.len() > 1 {
            let _: String = query(&mut conn, &watch).await?;
        }
        for item in items.iter_mut().filter(|i| i.role == "key") {
            let exists: i64 = query(&mut conn, &[b"EXISTS".to_vec(), item.after.clone()]).await?;
            item.target_exists = exists != 0;
        }
        let id = Uuid::new_v4();
        let plan = ReplacePlan {
            conn,
            id,
            revision: 1,
            spec,
            sources,
            items,
            non_utf8,
            unchanged,
            inapplicable,
        };
        if plan.items.len() > 10_000 {
            return Err(AppError::new("resource_limit", "替换条目超过计划上限"));
        }
        if plan.retained_bytes() > limit {
            return Err(AppError::new(
                "resource_limit",
                "Redis 替换计划超过内存预算",
            ));
        }
        plan.script_request()?;
        let summary = plan.summary();
        self.replace_plans.insert(id, plan);
        Ok(
            json!({"summary":summary,"plan_id":id,"revision":1,"digest":self.replace_plans[&id].digest()}),
        )
    }

    pub(super) fn replace_read(&self, p: &Json, cancel: &CancellationToken) -> Result<Json> {
        ensure_not_cancelled(cancel)?;
        let plan = self
            .replace_plans
            .get(&plan_id(p)?)
            .ok_or_else(|| AppError::invalid("替换计划已失效"))?;
        plan.check_revision(p)?;
        if let Some(index) = p.get("item_id").and_then(Json::as_u64) {
            let item = plan
                .items
                .get(index as usize)
                .ok_or_else(|| AppError::invalid("替换条目不存在"))?;
            let offset = p.get("offset").and_then(Json::as_u64).unwrap_or(0) as usize;
            let raw = if p.get("side").and_then(Json::as_str) == Some("after") {
                &item.after
            } else {
                &item.before
            };
            if offset > raw.len() {
                return Err(AppError::invalid("无效偏移"));
            }
            let end = offset.saturating_add(256 * 1024).min(raw.len());
            return Ok(
                json!({"summary":plan.summary(),"item_id":index,"offset":offset,"base64":STANDARD.encode(&raw[offset..end]),"next_offset":if end < raw.len() { Some(end) } else { None }}),
            );
        }
        let (offset, end, next) = crate::model::preview::preview_page(p, plan.items.len())?;
        let collisions = plan.collision_indices();
        let items: Vec<Json> = plan.items.iter().enumerate().skip(offset).take(end.saturating_sub(offset)).map(|(index, item)| json!({"id":index,"role":item.role,"key":short_value(&plan.sources[item.source].key),"identity":item.entry.map(|e| short_value(&plan.sources[item.source].entries[e].identity)),"before":short_value(&item.before),"after":short_value(&item.after),"matches":item.matches,"selected":item.selected,"collision":collisions.contains(&index)})).collect();
        Ok(json!({"summary":plan.summary(),"offset":offset,"next_offset":next,"items":items}))
    }

    pub(super) fn replace_update(&mut self, p: &Json, cancel: &CancellationToken) -> Result<Json> {
        ensure_not_cancelled(cancel)?;
        let plan = self
            .replace_plans
            .get_mut(&plan_id(p)?)
            .ok_or_else(|| AppError::invalid("替换计划已失效"))?;
        plan.check_revision(p)?;
        let selections = p
            .get("selections")
            .and_then(Json::as_array)
            .ok_or_else(|| AppError::invalid("缺少 selections"))?;
        let mut changes = Vec::new();
        let mut seen = HashSet::new();
        for selection in selections {
            let id = selection
                .get("id")
                .and_then(Json::as_u64)
                .ok_or_else(|| AppError::invalid("缺少条目 id"))? as usize;
            let selected = selection
                .get("selected")
                .and_then(Json::as_bool)
                .ok_or_else(|| AppError::invalid("缺少 selected"))?;
            if id >= plan.items.len() || !seen.insert(id) {
                return Err(AppError::invalid("条目不存在或重复"));
            }
            changes.push((id, selected));
        }
        let previous: Vec<(usize, bool)> = changes
            .iter()
            .map(|(id, _)| (*id, plan.items[*id].selected))
            .collect();
        for (id, selected) in changes {
            plan.items[id].selected = selected;
        }
        if let Err(error) = plan.script_request() {
            for (id, selected) in previous {
                plan.items[id].selected = selected;
            }
            return Err(error);
        }
        plan.revision = plan
            .revision
            .checked_add(1)
            .ok_or_else(|| AppError::invalid("revision 超限"))?;
        Ok(json!({"summary":plan.summary()}))
    }

    pub(super) fn replace_discard(&mut self, p: &Json, cancel: &CancellationToken) -> Result<Json> {
        ensure_not_cancelled(cancel)?;
        let id = plan_id(p)?;
        Ok(json!({"discarded":self.replace_plans.remove(&id).is_some()}))
    }
}

struct Operation {
    key: usize,
    command: &'static str,
    args: Vec<Vec<u8>>,
    items: Vec<usize>,
}
struct Check {
    kind: &'static str,
    key: usize,
    arg: Vec<u8>,
    expected: Vec<u8>,
}

// 在复制完整值前按借用数据计算参数和 RESP 封装的保守上界。
struct PayloadBudget {
    bytes: usize,
    commands: usize,
}
impl PayloadBudget {
    fn arg(&mut self, len: usize) -> Result<()> {
        self.bytes = self
            .bytes
            .checked_add(len)
            .and_then(|n| n.checked_add(32))
            .ok_or_else(|| AppError::new("resource_limit", "Redis 写参数超限"))?;
        if self.bytes > MAX_WRITE {
            return Err(AppError::new("resource_limit", "Redis 写参数超过 4 MiB"));
        }
        Ok(())
    }
    fn check(&mut self, kind: &str, arg_len: usize, expected_len: usize) -> Result<()> {
        for len in [kind.len(), 20, arg_len, expected_len] {
            self.arg(len)?;
        }
        Ok(())
    }
    fn operation(&mut self, command: &str, lengths: &[usize]) -> Result<()> {
        self.commands += 1;
        if self.commands > MAX_COMMANDS {
            return Err(AppError::new("resource_limit", "Redis 写命令超过 1000 条"));
        }
        for len in [20, 20, 20, command.len()] {
            self.arg(len)?;
        }
        for &len in lengths {
            self.arg(len)?;
        }
        Ok(())
    }
}

fn preflight_payload(
    plan: &ReplacePlan,
    selected: &HashMap<(usize, Option<usize>, &str), (usize, &Item)>,
) -> Result<()> {
    let mut budget = PayloadBudget {
        bytes: 0,
        commands: 0,
    };
    for len in [4, SCRIPT.len(), 20, 20, 20] {
        budget.arg(len)?;
    }
    for (si, source) in plan.sources.iter().enumerate() {
        budget.arg(source.key.len())?;
        budget.check("type", 0, source.kind.len())?;
        for (ei, entry) in source.entries.iter().enumerate() {
            let get = |role| selected.get(&(si, Some(ei), role)).copied();
            let value = match source.kind.as_str() {
                "string" => get("string_value"),
                "hash" => get("hash_value"),
                "list" => get("list_value"),
                _ => None,
            };
            let identity = match source.kind.as_str() {
                "hash" => get("hashfield"),
                "set" | "zset" => get("member"),
                _ => None,
            };
            if value.is_none() && identity.is_none() {
                continue;
            }
            let value_len = value.map_or(entry.value.len(), |(_, item)| item.after.len());
            let dest_len = identity.map_or(entry.identity.len(), |(_, item)| item.after.len());
            match source.kind.as_str() {
                "string" => budget.operation("SET", &[value_len, 7])?,
                "list" => budget.operation("LSET", &[entry.identity.len(), value_len])?,
                "hash" => {
                    budget.check("hexists", entry.identity.len(), 1)?;
                    if identity.is_some() {
                        budget.check("hexists", dest_len, 1)?;
                    }
                    budget.operation("HSET", &[dest_len, value_len])?;
                    if let Some(expiry) = &entry.expiry
                        && expiry != "-1"
                    {
                        budget.operation("HPEXPIREAT", &[expiry.len(), 6, 1, dest_len])?;
                    }
                    if identity.is_some() {
                        budget.operation("HDEL", &[entry.identity.len()])?;
                    }
                }
                "set" | "zset" => {
                    let check = if source.kind == "set" {
                        "sismember"
                    } else {
                        "zscore"
                    };
                    budget.check(check, entry.identity.len(), 1)?;
                    budget.check(check, dest_len, 1)?;
                    if source.kind == "set" {
                        budget.operation("SADD", &[dest_len])?;
                        budget.operation("SREM", &[entry.identity.len()])?;
                    } else {
                        budget.operation("ZADD", &[entry.value.len(), dest_len])?;
                        budget.operation("ZREM", &[entry.identity.len()])?;
                    }
                }
                _ => return Err(AppError::invalid("不支持的 Redis 类型")),
            }
        }
    }
    for item in plan.items.iter().filter(|i| i.selected && i.role == "key") {
        budget.arg(item.after.len())?;
        budget.check("exists", 0, 1)?;
        budget.operation("RENAMENX", &[20])?;
    }
    Ok(())
}

impl ReplacePlan {
    fn script_request(&self) -> Result<(Vec<Vec<u8>>, Vec<Operation>)> {
        let mut selected: HashMap<(usize, Option<usize>, &str), (usize, &Item)> = HashMap::new();
        for (id, item) in self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| item.selected)
        {
            selected.insert((item.source, item.entry, item.role.as_str()), (id, item));
        }
        preflight_payload(self, &selected)?;
        let mut keys: Vec<Vec<u8>> = self.sources.iter().map(|s| s.key.clone()).collect();
        let mut checks = Vec::new();
        let mut operations = Vec::new();
        for (si, source) in self.sources.iter().enumerate() {
            let key = si + 1;
            checks.push(Check {
                kind: "type",
                key,
                arg: vec![],
                expected: source.kind.as_bytes().to_vec(),
            });
            for (ei, entry) in source.entries.iter().enumerate() {
                let get = |role| selected.get(&(si, Some(ei), role)).copied();
                let value = match source.kind.as_str() {
                    "string" => get("string_value"),
                    "hash" => get("hash_value"),
                    "list" => get("list_value"),
                    _ => None,
                };
                let identity = match source.kind.as_str() {
                    "hash" => get("hashfield"),
                    "set" | "zset" => get("member"),
                    _ => None,
                };
                let mut ids = Vec::new();
                if let Some((id, _)) = value {
                    ids.push(id);
                }
                if let Some((id, _)) = identity {
                    ids.push(id);
                }
                if ids.is_empty() {
                    continue;
                }
                let new_value = value
                    .map(|(_, i)| i.after.clone())
                    .unwrap_or_else(|| entry.value.clone());
                match source.kind.as_str() {
                    "string" => operations.push(Operation {
                        key,
                        command: "SET",
                        args: vec![new_value, b"KEEPTTL".to_vec()],
                        items: ids,
                    }),
                    "list" => operations.push(Operation {
                        key,
                        command: "LSET",
                        args: vec![entry.identity.clone(), new_value],
                        items: ids,
                    }),
                    "hash" => {
                        checks.push(Check {
                            kind: "hexists",
                            key,
                            arg: entry.identity.clone(),
                            expected: b"1".to_vec(),
                        });
                        let dest = identity
                            .map(|(_, i)| i.after.clone())
                            .unwrap_or_else(|| entry.identity.clone());
                        if identity.is_some() {
                            checks.push(Check {
                                kind: "hexists",
                                key,
                                arg: dest.clone(),
                                expected: b"0".to_vec(),
                            });
                        }
                        operations.push(Operation {
                            key,
                            command: "HSET",
                            args: vec![dest.clone(), new_value],
                            items: ids.clone(),
                        });
                        if let Some(expiry) = &entry.expiry
                            && expiry != "-1"
                        {
                            operations.push(Operation {
                                key,
                                command: "HPEXPIREAT",
                                args: vec![
                                    expiry.as_bytes().to_vec(),
                                    b"FIELDS".to_vec(),
                                    b"1".to_vec(),
                                    dest.clone(),
                                ],
                                items: ids.clone(),
                            });
                        }
                        if identity.is_some() {
                            operations.push(Operation {
                                key,
                                command: "HDEL",
                                args: vec![entry.identity.clone()],
                                items: ids,
                            });
                        }
                    }
                    "set" | "zset" => {
                        let dest = identity.expect("成员操作必须包含身份").1.after.clone();
                        let check = if source.kind == "set" {
                            "sismember"
                        } else {
                            "zscore"
                        };
                        checks.push(Check {
                            kind: check,
                            key,
                            arg: entry.identity.clone(),
                            expected: b"1".to_vec(),
                        });
                        checks.push(Check {
                            kind: check,
                            key,
                            arg: dest.clone(),
                            expected: b"0".to_vec(),
                        });
                        if source.kind == "set" {
                            operations.push(Operation {
                                key,
                                command: "SADD",
                                args: vec![dest],
                                items: ids.clone(),
                            });
                            operations.push(Operation {
                                key,
                                command: "SREM",
                                args: vec![entry.identity.clone()],
                                items: ids,
                            });
                        } else {
                            operations.push(Operation {
                                key,
                                command: "ZADD",
                                args: vec![entry.value.clone(), dest],
                                items: ids.clone(),
                            });
                            operations.push(Operation {
                                key,
                                command: "ZREM",
                                args: vec![entry.identity.clone()],
                                items: ids,
                            });
                        }
                    }
                    _ => return Err(AppError::invalid("不支持的 Redis 类型")),
                }
            }
        }
        // key 改名必须在该 key 的全部内容操作之后。
        for (id, item) in self
            .items
            .iter()
            .enumerate()
            .filter(|(_, i)| i.selected && i.role == "key")
        {
            keys.push(item.after.clone());
            let destination = keys.len();
            checks.push(Check {
                kind: "exists",
                key: destination,
                arg: vec![],
                expected: b"0".to_vec(),
            });
            operations.push(Operation {
                key: item.source + 1,
                command: "RENAMENX",
                args: vec![destination.to_string().into_bytes()],
                items: vec![id],
            });
        }
        if operations.len() > MAX_COMMANDS {
            return Err(AppError::new("resource_limit", "Redis 写命令超过 1000 条"));
        }
        let mut argv = vec![checks.len().to_string().into_bytes()];
        for check in checks {
            argv.extend([
                check.kind.as_bytes().to_vec(),
                check.key.to_string().into_bytes(),
                check.arg,
                check.expected,
            ]);
        }
        argv.push(operations.len().to_string().into_bytes());
        for (index, op) in operations.iter().enumerate() {
            argv.extend([
                index.to_string().into_bytes(),
                op.key.to_string().into_bytes(),
                op.args.len().to_string().into_bytes(),
                op.command.as_bytes().to_vec(),
            ]);
            argv.extend(op.args.iter().cloned());
        }
        let payload = keys
            .iter()
            .chain(argv.iter())
            .try_fold(0usize, |n, b| n.checked_add(b.len()))
            .ok_or_else(|| AppError::new("resource_limit", "Redis 写参数超限"))?;
        if payload > MAX_WRITE {
            return Err(AppError::new("resource_limit", "Redis 写参数超过 4 MiB"));
        }
        let mut request = vec![
            b"EVAL".to_vec(),
            SCRIPT.as_bytes().to_vec(),
            keys.len().to_string().into_bytes(),
        ];
        request.extend(keys);
        request.extend(argv);
        Ok((request, operations))
    }
}

impl RedisSession {
    pub(super) async fn replace_submit(
        &mut self,
        p: &Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        self.profile.ensure_write()?;
        ensure_not_cancelled(cancel)?;
        let id = plan_id(p)?;
        let plan = self
            .replace_plans
            .get(&id)
            .ok_or_else(|| AppError::invalid("替换计划已失效"))?;
        plan.check_revision(p)?;
        let digest = p
            .get("digest")
            .or_else(|| p.get("plan_digest"))
            .and_then(Json::as_str)
            .ok_or_else(|| AppError::invalid("缺少 digest"))?;
        if digest != plan.digest() {
            return Err(AppError::conflict("替换计划摘要不匹配"));
        }
        if !plan.collision_indices().is_empty() {
            return Err(AppError::conflict("替换目标身份碰撞"));
        }
        let (request, operations) = plan.script_request()?;
        if operations.is_empty() {
            return Err(AppError::invalid("没有选中可提交的替换"));
        }
        let mut plan = self.replace_plans.remove(&id).expect("计划已校验");
        let _: String = query(&mut plan.conn, &args(&["MULTI"])).await?;
        let _: String = query(&mut plan.conn, &request).await?;
        sink.mark_sent(true).await?;
        let reply =
            redis::aio::ConnectionLike::req_packed_command(&mut plan.conn, &redis::cmd("EXEC"))
                .await
                .map_err(|e| AppError::unknown(format!("EXEC 响应未完整取得: {e}")))?;
        let script_reply = match reply {
            redis::Value::Nil => return Err(AppError::conflict("WATCH 目标发生变化")),
            redis::Value::Array(mut values) if values.len() == 1 => values.remove(0),
            _ => return Err(AppError::unknown("无法解析 EXEC 响应")),
        };
        if let redis::Value::ServerError(error) = &script_reply {
            return Err(AppError::unknown(format!(
                "替换脚本未返回完整逐项结果: {error:?}"
            )));
        }
        let parts: Vec<String> = redis::from_redis_value(script_reply)
            .map_err(|_| AppError::unknown("无法解析替换脚本响应"))?;
        let status = parts
            .first()
            .map(String::as_str)
            .ok_or_else(|| AppError::unknown("替换脚本响应为空"))?;
        if status == "conflict" {
            return Err(AppError::conflict("替换目标已变化或过期"));
        }
        if status == "permission" {
            return Err(AppError::new("database", "Redis ACL 不允许替换所需命令"));
        }
        if status != "ok" && status != "partial" {
            return Err(AppError::unknown("未知替换脚本状态"));
        }
        let applied = if status == "ok" {
            operations.len()
        } else {
            if parts.len() < 3 {
                return Err(AppError::unknown("不完整替换脚本响应"));
            }
            parts[parts.len() - 2]
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .filter(|n| *n < operations.len())
                .ok_or_else(|| AppError::unknown("无效失败位置"))?
        };
        let mut totals = vec![0usize; plan.items.len()];
        let mut done = vec![0usize; plan.items.len()];
        for (index, op) in operations.iter().enumerate() {
            for &item in &op.items {
                totals[item] += 1;
                if index < applied {
                    done[item] += 1;
                }
            }
        }
        let items: Vec<Json> = plan.items.iter().enumerate().map(|(i, item)| json!({"id":i,"status":if !item.selected { "excluded" } else if done[i] == totals[i] { "applied" } else if done[i] != 0 { "partial" } else if status == "partial" && operations[applied].items.contains(&i) { "failed" } else { "not_executed" }})).collect();
        Ok(
            json!({"status":status,"applied_commands":applied,"total_commands":operations.len(),"error":if status == "partial" { parts.last().cloned() } else { None },"items":items}),
        )
    }
}
