use super::*;

impl App {
    pub(crate) fn workbench_redis_edit(&mut self, backend: &Backend) {
        if self
            .tabs
            .get(self.active)
            .is_some_and(|tab| self.redis_key_delete_tab_pending(tab.id))
        {
            self.status = UiMessage::key("请先提交或丢弃当前草稿");
            return;
        }
        if !self.is_redis()
            || self.profile.as_ref().is_none_or(|p| {
                p.access_mode != AccessMode::ReadWrite || !p.capabilities().edit.is_supported()
            })
            || !self.browse_result
            || self.edit_token.is_none()
        {
            self.status = UiMessage::key("当前 Redis 资源没有可用的编辑基线");
            return;
        }
        if !self.tabs[self.active].pending.is_null() && !self.tabs[self.active].pending_live {
            self.status = UiMessage::key("请先恢复或丢弃已保存的编辑意图");
            return;
        }
        let Some(kind) = self
            .schema
            .as_ref()
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
        else {
            self.status = UiMessage::key("缺少 Redis 类型，请重新打开资源");
            return;
        };
        let Some(grid) = self.grid.as_ref() else {
            self.status = UiMessage::key("没有 Redis 结果");
            return;
        };
        let Some(cell) = grid.focus else {
            self.status = UiMessage::key("请先选择一行");
            return;
        };
        let large = (0..grid.page.result.columns.len())
            .map(|column| Cell {
                row: cell.row,
                column,
            })
            .find(|candidate| matches!(grid.value(*candidate), Some(DataValue::Large { .. })));
        if let Some(large) = large {
            self.workbench.redis_loading = Some(cell);
            if let Some(grid) = &mut self.grid {
                grid.focus = Some(large);
            }
            self.open_cell(backend);
            if self.large.is_none() {
                self.workbench.redis_loading = None;
            }
            return;
        }
        let get = |column| {
            grid.value(Cell {
                row: cell.row,
                column,
            })
            .ok_or_else(|| UiMessage::key("Redis 行缺少字段"))
            .and_then(redis_field)
        };
        let parsed = (|| -> std::result::Result<_, UiMessage> {
            Ok(match kind {
                "string" => ("set_string", vec![("redis_value", get(0)?)], None),
                "hash" => {
                    let field = get(0)?;
                    (
                        "hash_set",
                        vec![("redis_field", field.clone()), ("redis_value", get(1)?)],
                        Some(("field", field)),
                    )
                }
                "list" => {
                    let index = get(0)?;
                    (
                        "list_set",
                        vec![("redis_index", index.clone()), ("redis_value", get(1)?)],
                        Some(("index", index)),
                    )
                }
                "set" => {
                    let member = get(0)?;
                    (
                        "set_add",
                        vec![("redis_member", member.clone())],
                        Some(("member", member)),
                    )
                }
                "zset" => {
                    let member = get(0)?;
                    (
                        "zset_set",
                        vec![("redis_member", member.clone()), ("redis_score", get(1)?)],
                        Some(("member", member)),
                    )
                }
                _ => return Err(UiMessage::key("当前 Redis 类型不能编辑行")),
            })
        })();
        match parsed {
            Ok((operation, fields, identity)) => {
                self.field_set("operation", operation);
                for (name, value) in fields {
                    self.field_set(name, value);
                }
                self.workbench.redis_identity = identity.map(|(name, value)| (name.into(), value));
                self.workbench.overlay = Some("redis".into());
                let field = if operation == "hash_set" {
                    "redis_field"
                } else if matches!(operation, "set_add" | "zset_set") {
                    "redis_member"
                } else {
                    "redis_value"
                };
                self.focus = field.into();
                self.workbench.focus_request = Some(format!("input-{field}"));
            }
            Err(error) => self.status = error,
        }
    }

    pub(super) fn workbench_redis_stage(&mut self) -> bool {
        if self
            .tabs
            .get(self.active)
            .is_some_and(|tab| self.redis_key_delete_tab_pending(tab.id))
        {
            self.status = UiMessage::key("请先提交或丢弃当前草稿");
            return true;
        }
        if !self.is_redis()
            || self.profile.as_ref().is_none_or(|p| {
                p.access_mode != AccessMode::ReadWrite || !p.capabilities().edit.is_supported()
            })
            || !self.browse_result
            || self.edit_token.is_none()
        {
            self.status = UiMessage::key("编辑基线已失效，请重新打开资源");
            return true;
        }
        if !self.tabs[self.active].pending.is_null() && !self.tabs[self.active].pending_live {
            self.status = UiMessage::key("请先恢复或丢弃已保存的编辑意图");
            return true;
        }
        let operation = self.field("operation").to_owned();
        if operation == "set_ttl" {
            match self.workbench.ttl_policy {
                RedisTtlPolicy::Keep => {
                    self.workbench.overlay = None;
                    self.status = UiMessage::key("已保留现有 TTL");
                    return true;
                }
                RedisTtlPolicy::Persist => {}
                RedisTtlPolicy::Set if self.field("redis_ttl").parse::<u64>().is_err() => {
                    self.field_error("redis_ttl", UiMessage::key("请输入有效 TTL（毫秒）"));
                    return true;
                }
                RedisTtlPolicy::Set => {}
            }
        }
        if operation == "zset_set" {
            let raw = self.field("redis_score");
            let valid = raw.parse::<f64>().is_ok_and(|score| {
                score.is_finite()
                    || (score.is_infinite()
                        && matches!(raw.to_ascii_lowercase().as_str(), "inf" | "+inf" | "-inf"))
            });
            if !valid {
                self.field_error("redis_score", UiMessage::key("无效 score"));
                return true;
            }
        }
        let fields: &[&str] = match operation.as_str() {
            "set_string" | "list_set" | "list_insert" => &["redis_value"],
            "hash_set" => &["redis_field", "redis_value"],
            "hash_delete" => &["redis_field"],
            "set_add" | "set_remove" | "zset_set" | "zset_remove" => &["redis_member"],
            "rename" => &["redis_key"],
            _ => &[],
        };
        for &field in fields {
            if redis_bytes(self.field(field)).is_err() {
                self.field_error(field, UiMessage::key("无效 Base64 编码"));
                return true;
            }
        }
        let mut change = match self.redis_operation() {
            Ok(change) => change,
            Err(error) => {
                self.status = error;
                return true;
            }
        };
        let start = self.redis_changes.len();
        self.workbench
            .redis_undo_groups
            .retain(|&(_, end)| end <= start);
        if let Some((field, original)) = self.workbench.redis_identity.clone() {
            if field == "index" {
                if !matches!(
                    operation.as_str(),
                    "list_set" | "list_insert" | "list_delete"
                ) || self.field("redis_index") != original
                {
                    self.status = UiMessage::key("List 索引不可修改，请重新选择行");
                    return true;
                }
                let original = match original.parse::<i64>() {
                    Ok(index) => index,
                    Err(error) => {
                        self.status = UiMessage::raw(error.to_string());
                        return true;
                    }
                };
                match self.redis_list_index(original, operation == "list_insert") {
                    Ok(index) => change["index"] = json!(index),
                    Err(error) => {
                        self.status = error;
                        return true;
                    }
                }
            } else {
                let current = if field == "field" {
                    self.field("redis_field")
                } else {
                    self.field("redis_member")
                };
                let old = match redis_bytes(&original) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        self.status = UiMessage::raw(error);
                        return true;
                    }
                };
                let new = match redis_bytes(current) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        self.status = UiMessage::raw(error);
                        return true;
                    }
                };
                if old != new {
                    let remove = match operation.as_str() {
                        "hash_set" => "hash_delete",
                        "set_add" => "set_remove",
                        "zset_set" => "zset_remove",
                        _ => {
                            self.status = UiMessage::key("Redis 编辑操作已改变，请重新选择行");
                            return true;
                        }
                    };
                    let mut removal = json!({"operation":remove});
                    removal[field.as_str()] =
                        json!({"type":"bytes", "value": STANDARD.encode(old)});
                    self.redis_changes.push(removal);
                }
            }
        }
        self.redis_changes.push(change);
        self.redis_changes_revision = self.redis_changes_revision.wrapping_add(1);
        self.draft_revision = self.draft_revision.wrapping_add(1);
        self.workbench
            .redis_undo_groups
            .push((start, self.redis_changes.len()));
        self.detail = serde_json::to_string_pretty(&self.redis_changes).unwrap_or_default();
        self.set_detail_context("草稿详情");
        self.dirty = true;
        self.workbench.redis_identity = None;
        self.workbench.overlay = None;
        true
    }
}
