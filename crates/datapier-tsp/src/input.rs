//! 字段焦点、模态输入与 Redis 表单命令编码。
use super::*;

impl App {
    pub(super) fn redis_operation(&self) -> std::result::Result<Value, i18n::UiMessage> {
        use base64::Engine as _;
        let value = |name: &str| -> std::result::Result<Value, i18n::UiMessage> {
            let s = self.field(name);
            if let Some(data) = s.strip_prefix("base64:") {
                base64::engine::general_purpose::STANDARD
                    .decode(data)
                    .map_err(|e| i18n::UiMessage::raw(e.to_string()))?;
                Ok(json!({"type":"bytes","value":data}))
            } else {
                Ok(json!({"type":"text","value":s}))
            }
        };
        let op = self.field("operation");
        Ok(match op {
            "set_string" => json!({"operation":op,"value":value("redis_value")?}),
            "hash_set" => {
                json!({"operation":op,"field":value("redis_field")?,"value":value("redis_value")?})
            }
            "hash_delete" => json!({"operation":op,"field":value("redis_field")?}),
            "list_set" | "list_insert" => {
                json!({"operation":op,"index":self.field("redis_index").parse::<i64>().map_err(|e|i18n::UiMessage::raw(e.to_string()))?,"value":value("redis_value")?})
            }
            "list_delete" => {
                json!({"operation":op,"index":self.field("redis_index").parse::<i64>().map_err(|e|i18n::UiMessage::raw(e.to_string()))?})
            }
            "set_add" | "set_remove" | "zset_remove" => {
                json!({"operation":op,"member":value("redis_member")?})
            }
            "zset_set" => {
                json!({"operation":op,"member":value("redis_member")?,"score":self.field("redis_score")})
            }
            "rename" => {
                let name = self.field("redis_key");
                let encoded = if let Some(encoded) = name.strip_prefix("base64:") {
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .map_err(|e| i18n::UiMessage::raw(e.to_string()))?;
                    encoded.to_string()
                } else {
                    base64::engine::general_purpose::STANDARD.encode(name.as_bytes())
                };
                json!({"operation":op,"new_key_b64":encoded})
            }
            "delete_key" => json!({"operation":op}),
            "set_ttl" => {
                let ttl = if self.workbench.ttl_policy == workbench::RedisTtlPolicy::Persist
                    || self.field("redis_ttl").is_empty()
                {
                    None
                } else {
                    Some(
                        self.field("redis_ttl")
                            .parse::<u64>()
                            .map_err(|e| i18n::UiMessage::raw(e.to_string()))?,
                    )
                };
                json!({"operation":op,"ttl_ms":ttl})
            }
            _ => return Err(i18n::UiMessage::key("未知 Redis 操作")),
        })
    }
    // 模态层的实际节点决定字段归属，避免为条件渲染的表单维护第二份名单。
    pub(super) fn modal_allows_field(&self, name: &str) -> bool {
        let Some(kind) = self
            .workbench
            .overlay
            .as_deref()
            .filter(|kind| *kind != "key-type")
        else {
            return true;
        };
        fn contains(node: &Node<Msg>, key: &str) -> bool {
            node.props().get("key").and_then(Value::as_str) == Some(key)
                || node.child_nodes().iter().any(|child| contains(child, key))
        }
        let overlay = views::overlay(self, kind);
        contains(&overlay, name) || contains(&overlay, &format!("input-{name}"))
    }

    pub(super) fn reconcile_modal_focus(&mut self) {
        self.track_modal_draft();
        if self
            .workbench
            .field_error
            .as_ref()
            .is_some_and(|error| error.overlay != self.workbench.overlay)
        {
            self.workbench.field_error = None;
        }
        if self
            .workbench
            .overlay
            .as_deref()
            .is_none_or(|kind| kind == "key-type")
        {
            return;
        }
        if let Some(name) = self
            .workbench
            .focus_request
            .as_deref()
            .map(|key| key.strip_prefix("input-").unwrap_or(key))
            && self.modal_allows_field(name)
        {
            self.focus = name.to_owned();
        } else if !self.modal_allows_field(&self.focus) {
            self.focus.clear();
            self.workbench.focus_request = Some(String::new());
        }
    }

    pub(super) fn dispatch_guarded_editor_message(&mut self, message: Msg, backend: &Backend) {
        let editing = match &message {
            Msg::Edit(name, from, to, text, ..) => {
                (from != to || !text.is_empty()) && self.draft_input_field(name)
            }
            _ => false,
        };
        if !editing || self.draft_scope_ready() {
            self.dispatch_editor_message(message);
        } else {
            self.guard_draft_scope(backend, workbench::DraftScopeAction::Edit(message));
        }
    }

    pub(super) fn dispatch_editor_message(&mut self, message: Msg) {
        self.track_modal_draft();
        if self.advanced_schema_edit(&message) || self.advanced_value_edit(&message) {
            return;
        }
        if self.workbench.overlay.as_deref() == Some("profile") && self.connection_form_busy() {
            return;
        }
        match message {
            Msg::Edit(name, from, to, text, cursor, len) if self.modal_allows_field(&name) => {
                if let Some(ed) = self.editor(&name)
                    && ed.text.encode_utf16().count() == len
                    && ed.edit(from..to, &text)
                {
                    ed.set_selection(cursor, cursor);
                    if from != to || !text.is_empty() {
                        self.changed_field(&name);
                    } else {
                        self.dirty = true;
                    }
                }
            }
            Msg::Undo(name) if self.modal_allows_field(&name) => {
                if let Some(ed) = self.editor(&name)
                    && ed.undo()
                {
                    self.changed_field(&name);
                }
            }
            _ => {}
        }
    }

    pub(super) fn input_key(&mut self, key: &tern_sdk::Key) {
        self.track_modal_draft();
        if self.workbench.overlay.as_deref() == Some("profile") && self.connection_form_busy() {
            return;
        }
        if !self.modal_allows_field(&self.focus) {
            return;
        }
        let name = self.focus.clone();
        let edit = match key.name.as_str() {
            "backspace" => Some(EditKey::Backspace),
            "delete" => Some(EditKey::Delete),
            "left" => Some(EditKey::Left),
            "right" => Some(EditKey::Right),
            "up" => Some(EditKey::Up),
            "down" => Some(EditKey::Down),
            "home" => Some(EditKey::Home),
            "end" => Some(EditKey::End),
            "enter" => Some(EditKey::Text("\n")),
            "paste" => key.text.as_deref().map(EditKey::Text),
            "a" if key.ctrl || key.meta => Some(EditKey::SelectAll),
            "z" if key.ctrl || key.meta => Some(EditKey::Undo),
            _ => key.typed().map(EditKey::Text),
        };
        let Some(edit) = edit else {
            return;
        };
        if let Some(schema) = self.advanced_schema.as_mut()
            && schema.key(&name, edit, key.shift)
        {
            self.dirty |= schema.dirty;
            return;
        }
        if let Some(editor) = self.editor(&name) {
            let previous_len = editor.text.len();
            // 处理了按键不代表文本被修改；光标、选区与空删除不能改变 NULL／默认模式。
            let text_edit = matches!(&edit, EditKey::Text(text) if !text.is_empty() || editor.cursor != editor.anchor)
                || matches!(&edit, EditKey::Undo);
            if editor.key(edit, key.shift) {
                if text_edit || editor.text.len() != previous_len {
                    self.changed_field(&name);
                } else {
                    self.dirty = true;
                }
            }
        }
    }
    pub(super) fn take_focus_request(&mut self) -> Option<String> {
        let key = self.workbench.focus_request.take()?;
        if let Some(field) = key.strip_prefix("input-") {
            self.focus = field.into();
        } else if key == "query-editor" {
            self.focus = "query".into();
        } else if key == "cell-editor" {
            self.focus = "cell".into();
        } else if key == "grid" {
            self.focus = "grid".into();
            return Some(String::new());
        }
        Some(key)
    }
}
// SDK 按区域、祖先 key 与子节点序号派生 id；焦点与活动标签 reveal 只沿当前 View 找路径，
// 不复制整棵树，也不把业务字段名误当作终端中的节点 id。
pub(super) fn focus_node_id(view: &View<Msg>, key: &str) -> Option<String> {
    fn find(node: &Node<Msg>, key: &str, path: &mut String) -> bool {
        use std::fmt::Write as _;
        if node.props().get("key").and_then(Value::as_str) == Some(key) {
            return true;
        }
        for (index, child) in node.child_nodes().iter().enumerate() {
            let length = path.len();
            match child.props().get("key") {
                Some(Value::String(child_key)) => {
                    path.push('.');
                    path.push_str(child_key);
                }
                Some(Value::Number(child_key)) => {
                    let _ = write!(path, ".{child_key}");
                }
                _ => {
                    let _ = write!(path, ".{index}");
                }
            }
            if find(child, key, path) {
                return true;
            }
            path.truncate(length);
        }
        false
    }
    for (region, node) in ["main", "dock", "layer"].into_iter().zip(view.regions()) {
        if let Some(node) = node {
            let mut path = region.to_owned();
            match node.props().get("key") {
                Some(Value::String(root_key)) => {
                    path.push('.');
                    path.push_str(root_key);
                }
                Some(Value::Number(root_key)) => {
                    use std::fmt::Write as _;
                    let _ = write!(path, ".{root_key}");
                }
                _ => {}
            }
            if find(node, key, &mut path) {
                return Some(path);
            }
        }
    }
    None
}
