//! 受 WATCH 保护的 Redis 单值读取；连接及令牌仍由普通编辑会话持有。
use super::*;

// 在服务端同一脚本中限制返回值，避免先测量后读取期间值增长造成无界分配。
const READ_VALUE: &str = r#"
local kind = redis.call('TYPE', KEYS[1]).ok
if kind ~= ARGV[1] then return redis.error_reply('value target type changed') end
local role, identity, limit = ARGV[2], ARGV[3], tonumber(ARGV[4])
local result
local field_expiry = -1
local score = false
if role == 'string' and kind == 'string' then
  if redis.call('STRLEN', KEYS[1]) > limit then return redis.error_reply('value exceeds budget') end
  result = redis.call('GET', KEYS[1])
elseif role == 'hash_value' and kind == 'hash' then
  if redis.call('HSTRLEN', KEYS[1], identity) > limit then return redis.error_reply('value exceeds budget') end
  result = redis.call('HGET', KEYS[1], identity)
  if result then
    local major = tonumber(ARGV[5])
    if major >= 8 then
      local expiry = redis.pcall('HPEXPIRETIME', KEYS[1], 'FIELDS', 1, identity)
      if type(expiry) ~= 'table' or type(expiry[1]) ~= 'number' or expiry[1] < -1 then
        return redis.error_reply('cannot read field expiration')
      end
      field_expiry = expiry[1]
    end
  end
elseif role == 'list_value' and kind == 'list' then
  result = redis.call('LINDEX', KEYS[1], identity)
elseif role == 'set_member' and kind == 'set' then
  if redis.call('SISMEMBER', KEYS[1], identity) == 1 then result = identity end
elseif role == 'zset_member' and kind == 'zset' then
  score = redis.call('ZSCORE', KEYS[1], identity)
  if score then result = identity end
else
  return redis.error_reply('invalid value role')
end
if not result then return {false, field_expiry, false} end
if string.len(result) > limit then return redis.error_reply('value exceeds budget') end
return {result, field_expiry, score}
"#;

impl RedisSession {
    /// 在既有 WATCH 连接上读取完整值；令牌必须先于任何基线读取创建。
    pub async fn value_snapshot(&mut self, params: &Json) -> Result<Json> {
        let id = uuid(params)?;
        let target = target(params, &self.profile)?;
        let edit = self
            .edits
            .get_mut(&id)
            .ok_or_else(|| AppError::conflict("编辑令牌已失效"))?;
        snapshot(edit, id, target, params).await
    }

    /// 只读检查原 WATCH 的冲突标记，核对基线时始终保留原监视。
    pub async fn value_verify(&mut self, params: &Json) -> Result<()> {
        let id = uuid(params)?;
        let target = target(params, &self.profile)?;
        let expected: Value = param(params, "before")?;
        let edit = self
            .edits
            .get(&id)
            .ok_or_else(|| AppError::conflict("编辑令牌已失效"))?;
        if key(&target)? != edit.key {
            return Err(AppError::invalid("编辑目标不匹配"));
        }
        let result_id: Uuid = param(params, "result_id")?;
        if edit.result_id != result_id {
            return Err(AppError::invalid("结果与编辑令牌不匹配"));
        }
        // 从表中移出令牌：取消、连接错误或任何冲突都会丢弃无法证明有效的 WATCH。
        let mut edit = self.edits.remove(&id).expect("已检查编辑令牌");
        let info: String = query(&mut edit.conn, &args(&["CLIENT", "INFO"])).await?;
        let flags = info
            .split_ascii_whitespace()
            .find_map(|field| field.strip_prefix("flags="))
            .ok_or_else(|| AppError::new("database", "CLIENT INFO 缺少客户端状态"))?;
        if flags.contains('d') {
            return Err(AppError::conflict("key 在读取基线后已变化"));
        }
        // 校验期间发生的后续修改仍会标记同一个 WATCH，由再次回填或最终 EXEC 拒绝。
        // Redis 6.2 以 TIME + PTTL 计算绝对期限，允许毫秒取整造成的一毫秒误差。
        let (kind, _, expires_at_ms) = watched_metadata(&mut edit.conn, &edit.key).await?;
        let expiry_matches = match (edit.expires_at_ms, expires_at_ms) {
            (None, None) => true,
            (Some(before), Some(after)) => before.abs_diff(after) <= 1,
            _ => false,
        };
        if kind != edit.kind || !expiry_matches {
            return Err(AppError::conflict("key 类型或过期时间已变化"));
        }
        let actual = snapshot(&mut edit, id, target, params).await?;
        let current: Value = param(&actual, "value")?;
        if current != expected {
            return Err(AppError::conflict("值在编辑期间已变化"));
        }
        if params.get("field_expires_at_ms") != actual.get("field_expires_at_ms")
            || params.get("score") != actual.get("score")
        {
            return Err(AppError::conflict("成员分值或字段过期时间已变化"));
        }
        self.edits.insert(id, edit);
        Ok(())
    }

    /// 在原 WATCH 连接上检查新成员身份；真正提交时仍由 WATCH 保护该判断。
    pub async fn value_check_replacement(
        &mut self,
        params: &Json,
        replacement: &Value,
    ) -> Result<()> {
        let id = uuid(params)?;
        let target = target(params, &self.profile)?;
        let old = field(params, "member")?;
        let new = bytes(replacement)?;
        let edit = self
            .edits
            .get_mut(&id)
            .ok_or_else(|| AppError::conflict("编辑令牌已失效"))?;
        if key(&target)? != edit.key {
            return Err(AppError::invalid("编辑目标不匹配"));
        }
        if old == new {
            return Ok(());
        }
        let command = match edit.kind.as_str() {
            "set" => b"SISMEMBER".as_slice(),
            "zset" => b"ZSCORE".as_slice(),
            _ => return Err(AppError::invalid("目标不是集合成员")),
        };
        let result: redis::Value =
            query(&mut edit.conn, &[command.to_vec(), edit.key.clone(), new]).await?;
        let exists = match result {
            redis::Value::Nil | redis::Value::Int(0) => false,
            redis::Value::Int(1) | redis::Value::BulkString(_) => true,
            _ => return Err(AppError::new("database", "成员碰撞检查返回无效结果")),
        };
        if exists {
            return Err(AppError::conflict("目标成员已存在"));
        }
        Ok(())
    }
}

async fn snapshot(
    edit: &mut Edit,
    id: Uuid,
    target: ResourceTarget,
    params: &Json,
) -> Result<Json> {
    let mut role: String = param(params, "role")?;
    if role == "string_value" {
        role = "string".into();
    }
    if role == "member" {
        let kind = edit.kind.as_str();
        role = match kind {
            "set" => "set_member",
            "zset" => "zset_member",
            _ => return Err(AppError::invalid("成员类型无效")),
        }
        .into();
    }
    let limit: usize = param(params, "max_bytes")?;
    if limit == 0 {
        return Err(AppError::new("resource_limit", "单值内存预算已用尽"));
    }
    let identity = match role.as_str() {
        "string" => Vec::new(),
        "hash_value" => field(params, "field")?,
        "list_value" => {
            let index: u64 = param(params, "index")?;
            if index > i64::MAX as u64 {
                return Err(AppError::invalid("List index 超出范围"));
            }
            index.to_string().into_bytes()
        }
        "set_member" | "zset_member" => field(params, "member")?,
        _ => return Err(AppError::invalid("不支持的 Redis 值角色")),
    };
    if key(&target)? != edit.key {
        return Err(AppError::invalid("编辑目标不匹配"));
    }
    let result_id: Uuid = param(params, "result_id")?;
    if edit.result_id != result_id {
        return Err(AppError::invalid("结果与编辑令牌不匹配"));
    }
    let major = if role == "hash_value" {
        let info: String = query(&mut edit.conn, &args(&["INFO", "server"])).await?;
        info.lines()
            .find_map(|line| {
                line.strip_prefix("redis_version:")
                    .and_then(|v| v.split('.').next())
                    .and_then(|v| v.parse::<u32>().ok())
            })
            .ok_or_else(|| AppError::new("database", "无法确定 Redis 版本以保护字段过期时间"))?
    } else {
        0
    };
    let (raw, field_expiry, score): (Option<Vec<u8>>, i64, Option<String>) = query(
        &mut edit.conn,
        &[
            b"EVAL".to_vec(),
            READ_VALUE.as_bytes().to_vec(),
            b"1".to_vec(),
            edit.key.clone(),
            edit.kind.as_bytes().to_vec(),
            role.as_bytes().to_vec(),
            identity,
            limit.to_string().into_bytes(),
            major.to_string().into_bytes(),
        ],
    )
    .await?;
    let raw = raw.ok_or_else(|| AppError::conflict("值不存在或已变化"))?;
    Ok(
        json!({"value": val(raw), "token": id, "target": target, "role": role, "score": score, "field_expires_at_ms": if field_expiry < 0 { None } else { Some(field_expiry) }}),
    )
}
