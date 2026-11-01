use super::*;

impl RedisSession {
    pub(super) fn preview(&self, p: &Json, cancel: &CancellationToken) -> Result<Json> {
        use crate::model::preview::{
            ChangePreview, PreviewBudget, PreviewItem, PreviewStatement, preview_json, preview_page,
        };
        self.profile.ensure_write()?;
        ensure_not_cancelled(cancel)?;
        let id = uuid(p)?;
        let t = target(p, &self.profile)?;
        let edit = self
            .edits
            .get(&id)
            .ok_or_else(|| AppError::invalid("编辑令牌已失效"))?;
        if key(&t)? != edit.key {
            return Err(AppError::invalid("编辑目标不匹配"));
        }
        let changes = p
            .get("changes")
            .and_then(Json::as_array)
            .ok_or_else(|| AppError::invalid("缺少 changes"))?;
        let (offset, end, next_offset) = preview_page(p, changes.len())?;
        let ChangePlan { commands, .. } = change_plan(edit, changes, true)?;
        let mut budget = PreviewBudget::new();
        let mut items = Vec::new();
        for (index, change) in changes
            .iter()
            .enumerate()
            .filter(|(index, _)| *index >= offset && *index < end)
        {
            ensure_not_cancelled(cancel)?;
            let operation = change
                .get("operation")
                .and_then(Json::as_str)
                .ok_or_else(|| AppError::invalid("缺少操作"))?;
            let statements = commands
                .iter()
                .filter(|(position, _)| *position == index)
                .map(|(_, command)| {
                    let parameters = command[1..]
                        .iter()
                        .enumerate()
                        .map(|(i, bytes)| {
                            let dynamic = operation == "list_delete" && i + 2 == command.len();
                            let shown = if dynamic {
                                "提交时生成的随机标记".as_bytes()
                            } else {
                                bytes.as_slice()
                            };
                            let kind = if std::str::from_utf8(shown).is_ok() {
                                "text"
                            } else {
                                "bytes"
                            };
                            budget.parameter(kind, shown, dynamic)
                        })
                        .collect();
                    PreviewStatement {
                        role: "write",
                        text: String::from_utf8_lossy(&command[0]).into_owned(),
                        parameters,
                    }
                })
                .collect();
            items.push(PreviewItem {
                index: Some(index),
                operation: operation.to_owned(),
                statements,
            });
        }
        if next_offset.is_none()
            && let Some((_, command)) = commands
                .iter()
                .find(|(position, _)| *position == usize::MAX)
        {
            let parameters = command[1..]
                .iter()
                .map(|bytes| {
                    let kind = if std::str::from_utf8(bytes).is_ok() {
                        "text"
                    } else {
                        "bytes"
                    };
                    budget.parameter(kind, bytes, false)
                })
                .collect();
            items.push(PreviewItem {
                index: None,
                operation: "ttl".into(),
                statements: vec![PreviewStatement {
                    role: "ttl",
                    text: "EVAL".into(),
                    parameters,
                }],
            });
        }
        preview_json(ChangePreview { provider: "redis", transactional: true, total_changes: changes.len(), offset, next_offset, items, notes: vec![
            "MULTI/EXEC 中单项错误不会回滚其他命令；TTL 失败时值可能已经改变。".into(),
            "重命名目标仅在提交时 WATCH 并检查是否存在，预览尚未确认目标。".into(),
            "List 删除的随机标记在提交时生成。多个显式 TTL 仅最后一个有效；未设置时恢复基线绝对期限。".into(),
        ] })
    }
    pub(super) async fn submit(
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
        let ChangePlan {
            commands,
            ttl_changes,
            has_expiry,
        } = change_plan(&edit, changes, false)?;
        for (_, cmd) in &commands {
            if cmd[0] == b"RENAMENX" {
                let dest = &cmd[2];
                let _: String = query(&mut edit.conn, &[b"WATCH".to_vec(), dest.clone()]).await?;
                let exists: i64 =
                    query(&mut edit.conn, &[b"EXISTS".to_vec(), dest.clone()]).await?;
                if exists != 0 {
                    return Err(AppError::conflict("目标 key 已存在"));
                }
            }
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
struct ChangePlan {
    commands: Vec<(usize, Vec<Vec<u8>>)>,
    ttl_changes: Vec<usize>,
    has_expiry: bool,
}

fn change_plan(edit: &Edit, changes: &[Json], preview: bool) -> Result<ChangePlan> {
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
   "list_delete" if edit.kind=="list"=>vec![b"EVAL".to_vec(),b"local n=redis.call('LLEN',KEYS[1]); local i=tonumber(ARGV[1]); if not i or i<0 or i>=n then return redis.error_reply('index out of range') end; if redis.call('LPOS',KEYS[1],ARGV[2]) then return redis.error_reply('temporary marker exists') end; redis.call('LSET',KEYS[1],i,ARGV[2]); return redis.call('LREM',KEYS[1],1,ARGV[2])".to_vec(),b"1".to_vec(),k,index(change)?,if preview { Vec::new() } else { format!("\0datapier:{}",Uuid::new_v4()).into_bytes() }],
   "set_add" if edit.kind=="set"=>vec![b"SADD".to_vec(),k,field(change,"member")?],
   "set_remove" if edit.kind=="set"=>vec![b"SREM".to_vec(),k,field(change,"member")?],
   "zset_set" if edit.kind=="zset"=>{let s=change.get("score").and_then(Json::as_str).ok_or_else(||AppError::invalid("缺少 score"))?; crate::execution::transfer::validate_redis_score(s)?; vec![b"ZADD".to_vec(),k,s.as_bytes().to_vec(),field(change,"member")?]},
   "zset_remove" if edit.kind=="zset"=>vec![b"ZREM".to_vec(),k,field(change,"member")?],
   "delete_key"=>vec![b"DEL".to_vec(),k],
   "rename"=>{let dest=STANDARD.decode(change.get("new_key_b64").and_then(Json::as_str).ok_or_else(||AppError::invalid("缺少新 key"))?).map_err(|e|AppError::invalid(e.to_string()))?; if dest==k{return Err(AppError::invalid("新旧 key 相同"));} final_key=dest.clone(); vec![b"RENAMENX".to_vec(),k,dest]},
   _=>return Err(AppError::invalid(format!("操作 {op} 不适用于当前类型"))) };
        commands.push((position, cmd));
    }
    // 没有显式设置 TTL 时恢复基线的绝对到期时间；永久 key 无需附加期限命令。
    let expiry = explicit_ttl.or_else(|| {
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
    Ok(ChangePlan {
        commands,
        ttl_changes,
        has_expiry,
    })
}
fn index(p: &Json) -> Result<Vec<u8>> {
    let i = p
        .get("index")
        .and_then(Json::as_i64)
        .filter(|i| *i >= 0)
        .ok_or_else(|| AppError::invalid("无效 list 索引"))?;
    Ok(i.to_string().into_bytes())
}
