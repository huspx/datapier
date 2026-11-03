//! 多 key 删除拥有独立 WATCH 连接和不可变目标集。
use super::*;
use crate::model::preview::{
    ChangePreview, PreviewBudget, PreviewItem, PreviewStatement, preview_json, preview_page,
};
use std::collections::HashSet;

pub(super) struct BatchEdit {
    conn: Conn,
    targets: Json,
    keys: Vec<Vec<u8>>,
}

fn delete_command(keys: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut command = Vec::with_capacity(keys.len() + 1);
    command.push(b"DEL".to_vec());
    command.extend(keys.iter().cloned());
    command
}

fn validate_input(p: &Json, targets: &Json, preview: bool) -> Result<()> {
    let object = p
        .as_object()
        .ok_or_else(|| AppError::invalid("无效批量删除参数"))?;
    if object.keys().any(|k| {
        !matches!(
            k.as_str(),
            "token" | "targets" | "operation" | "session_id" | "workspace_id"
        ) && !(preview && matches!(k.as_str(), "offset" | "limit"))
    }) || p.get("operation").and_then(Json::as_str) != Some("delete_keys")
        || p.get("targets") != Some(targets)
    {
        return Err(AppError::invalid("批量删除目标或操作不匹配"));
    }
    Ok(())
}

impl RedisSession {
    pub(super) fn is_batch_request(&self, p: &Json) -> Result<bool> {
        let id = uuid(p)?;
        Ok(self.batch_edits.contains_key(&id)
            || p.get("targets").is_some()
            || p.get("operation").is_some())
    }

    pub(super) async fn open_batch_delete(
        &mut self,
        p: &Json,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        self.profile.ensure_write()?;
        let object = p
            .as_object()
            .ok_or_else(|| AppError::invalid("无效批量删除参数"))?;
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "targets" | "session_id" | "workspace_id"))
        {
            return Err(AppError::invalid("批量删除只能指定 targets"));
        }
        let targets = p
            .get("targets")
            .and_then(Json::as_array)
            .filter(|targets| !targets.is_empty())
            .ok_or_else(|| AppError::invalid("批量删除目标不能为空"))?;
        let mut keys = Vec::with_capacity(targets.len());
        let mut kinds = Vec::with_capacity(targets.len());
        let mut seen = HashSet::with_capacity(targets.len());
        for value in targets {
            let target: ResourceTarget = serde_json::from_value(value.clone())?;
            if target
                .database
                .as_deref()
                .and_then(|db| db.parse::<u32>().ok())
                != Some(self.profile.redis_db())
            {
                return Err(AppError::invalid("目标数据库与连接不一致"));
            }
            if target.kind.is_empty() || target.kind == "database" {
                return Err(AppError::invalid("批量删除目标必须为 Redis key"));
            }
            let encoded = target
                .key_b64
                .as_ref()
                .ok_or_else(|| AppError::invalid("缺少 key_b64"))?;
            let key = STANDARD
                .decode(encoded)
                .map_err(|e| AppError::invalid(e.to_string()))?;
            if !seen.insert(key.clone()) {
                return Err(AppError::invalid("key 身份重复"));
            }
            keys.push(key);
            kinds.push(target.kind);
        }
        ensure_not_cancelled(cancel)?;
        let mut conn =
            connect_database(&self.client, &self.profile, self.secret.as_deref()).await?;
        let mut watch = Vec::with_capacity(keys.len() + 1);
        watch.push(b"WATCH".to_vec());
        watch.extend(keys.iter().cloned());
        let _: String = query(&mut conn, &watch).await?;
        let mut baselines = Vec::with_capacity(keys.len());
        for (key, expected_kind) in keys.iter().zip(&kinds) {
            ensure_not_cancelled(cancel)?;
            let (kind, ttl) = metadata(&mut conn, key).await?;
            if kind == "none" || ttl == -2 {
                return Err(AppError::conflict("目标 key 已不存在或过期"));
            }
            if &kind != expected_kind {
                return Err(AppError::conflict("目标 key 类型已变化"));
            }
            baselines.push(json!({"type":kind,"ttl_ms":ttl}));
        }
        ensure_not_cancelled(cancel)?;
        let id = Uuid::new_v4();
        let targets = Json::Array(targets.clone());
        self.batch_edits.insert(
            id,
            BatchEdit {
                conn,
                targets: targets.clone(),
                keys,
            },
        );
        Ok(json!({"token":id,"targets":targets,"operation":"delete_keys","baselines":baselines}))
    }

    pub(super) fn preview_batch_delete(
        &self,
        p: &Json,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        self.profile.ensure_write()?;
        ensure_not_cancelled(cancel)?;
        let edit = self
            .batch_edits
            .get(&uuid(p)?)
            .ok_or_else(|| AppError::invalid("编辑令牌已失效"))?;
        validate_input(p, &edit.targets, true)?;
        let (offset, end, next_offset) = preview_page(p, 1)?;
        let command = delete_command(&edit.keys);
        let mut budget = PreviewBudget::new();
        let mut items = Vec::new();
        if offset < end {
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
                index: Some(0),
                operation: "delete_keys".into(),
                statements: vec![
                    PreviewStatement {
                        role: "transaction",
                        text: "MULTI".into(),
                        parameters: vec![],
                    },
                    PreviewStatement {
                        role: "write",
                        text: "DEL".into(),
                        parameters,
                    },
                    PreviewStatement {
                        role: "transaction",
                        text: "EXEC".into(),
                        parameters: vec![],
                    },
                ],
            });
        }
        preview_json(ChangePreview {
            provider: "redis",
            transactional: true,
            total_changes: 1,
            offset,
            next_offset,
            items,
            notes: vec!["任一 key 变化或过期会导致整批 WATCH 冲突。".into()],
        })
    }

    pub(super) async fn submit_batch_delete(
        &mut self,
        p: &Json,
        sink: &ExecutionSink,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        self.profile.ensure_write()?;
        let mut edit = self
            .batch_edits
            .remove(&uuid(p)?)
            .ok_or_else(|| AppError::invalid("编辑令牌已失效"))?;
        validate_input(p, &edit.targets, false)?;
        let command = delete_command(&edit.keys);
        ensure_not_cancelled(cancel)?;
        let _: String = query(&mut edit.conn, &args(&["MULTI"])).await?;
        let _: String = query(&mut edit.conn, &command).await?;
        sink.mark_sent(true).await?;
        let result =
            redis::aio::ConnectionLike::req_packed_command(&mut edit.conn, &redis::cmd("EXEC"))
                .await
                .map_err(error_unknown)?;
        match result {
            redis::Value::Nil => Err(AppError::conflict("key 在读取基线后已变化")),
            redis::Value::Array(mut replies) if replies.len() == 1 => match replies.remove(0) {
                redis::Value::Int(affected) if affected >= 0 => Ok(
                    json!({"affected":affected,"items":[{"index":0,"status":"applied","result":affected}],"results":[]}),
                ),
                redis::Value::ServerError(error) => {
                    Err(AppError::new("database", format!("{error:?}")))
                }
                _ => Err(AppError::new("database", "无效 DEL 响应")),
            },
            _ => Err(AppError::new("database", "无效 EXEC 响应")),
        }
    }
}
