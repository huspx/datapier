use crate::{App, ResourceTarget, backend::Backend};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Default)]
pub(super) struct RedisKeySelection {
    scope: Option<(Uuid, u32)>,
    keys: BTreeSet<String>,
    anchor: Option<String>,
}

impl App {
    pub(super) fn redis_key_action_index(&self, identity: &str) -> Option<usize> {
        let mut fields = identity.splitn(4, ':');
        let tab = Uuid::parse_str(fields.next()?).ok()?;
        let generation = fields.next()?.parse::<u64>().ok()?;
        let index = fields.next()?.parse::<usize>().ok()?;
        let key = fields.next()?;
        (self.tabs.get(self.active)?.id == tab
            && self.generation == generation
            && self.resources.get(index)?.key_b64.as_deref() == Some(key))
        .then_some(index)
    }

    fn redis_selection_scope(&self) -> Option<(Uuid, u32)> {
        self.profile
            .as_ref()
            .filter(|_| self.is_redis())
            .map(|p| (p.id, p.redis_db()))
    }

    fn redis_visible_key_indices(&self) -> Vec<usize> {
        if self.workbench.tree {
            let tree = crate::redis_tree::Tree::from_resources(&self.resources);
            let rows = tree.rows(
                &self.workbench.redis_expansion,
                self.redis_selection_scope(),
            );
            let pages = rows.len().div_ceil(crate::workbench::KEY_PAGE_SIZE).max(1);
            let start = self.workbench.key_page.min(pages - 1) * crate::workbench::KEY_PAGE_SIZE;
            rows[start..(start + crate::workbench::KEY_PAGE_SIZE).min(rows.len())]
                .iter()
                .filter_map(|row| match &row.kind {
                    crate::redis_tree::Kind::Key { index } => Some(*index),
                    _ => None,
                })
                .collect()
        } else {
            let pages = self
                .resources
                .len()
                .div_ceil(crate::workbench::KEY_PAGE_SIZE)
                .max(1);
            let start = self.workbench.key_page.min(pages - 1) * crate::workbench::KEY_PAGE_SIZE;
            (start..(start + crate::workbench::KEY_PAGE_SIZE).min(self.resources.len())).collect()
        }
    }
    pub(super) fn redis_prune_key_selection(&mut self) {
        let scope = self.redis_selection_scope();
        if self.redis_keys.scope != scope {
            self.redis_keys = RedisKeySelection {
                scope,
                ..Default::default()
            };
        }
        let visible: BTreeSet<_> = self
            .redis_visible_key_indices()
            .into_iter()
            .filter_map(|i| self.resources.get(i)?.key_b64.clone())
            .collect();
        self.redis_keys.keys.retain(|key| visible.contains(key));
        if self
            .redis_keys
            .anchor
            .as_ref()
            .is_some_and(|key| !visible.contains(key))
        {
            self.redis_keys.anchor = None;
        }
    }

    pub(super) fn redis_key_selected(&self, index: usize) -> bool {
        self.redis_keys.scope == self.redis_selection_scope()
            && self
                .resources
                .get(index)
                .and_then(|r| r.key_b64.as_ref())
                .is_some_and(|key| self.redis_keys.keys.contains(key))
    }

    pub(super) fn selected_redis_key_targets(&self) -> Vec<ResourceTarget> {
        if self.redis_keys.scope != self.redis_selection_scope() {
            return Vec::new();
        }
        self.redis_visible_key_indices()
            .into_iter()
            .filter_map(|i| {
                let r = self.resources.get(i)?;
                let key = r.key_b64.as_ref()?;
                self.redis_keys.keys.contains(key).then(|| ResourceTarget {
                    database: r
                        .database
                        .clone()
                        .or_else(|| self.profile.as_ref().map(|p| p.redis_db().to_string())),
                    name: r.name.clone(),
                    key_b64: Some(key.clone()),
                    kind: r.kind.clone(),
                })
            })
            .collect()
    }

    pub(super) fn selected_redis_key_count(&self) -> usize {
        if self.redis_keys.scope == self.redis_selection_scope() {
            self.redis_keys.keys.len()
        } else {
            0
        }
    }

    pub(super) fn redis_key_click(
        &mut self,
        index: usize,
        shift: bool,
        primary: bool,
        backend: &Backend,
    ) {
        self.redis_prune_key_selection();
        let Some(key) = self.resources.get(index).and_then(|r| r.key_b64.clone()) else {
            return;
        };
        let visible = self.redis_visible_key_indices();
        if !visible.contains(&index) {
            return;
        }
        if shift {
            let anchor = self.redis_keys.anchor.as_ref().and_then(|key| {
                visible
                    .iter()
                    .position(|&i| self.resources[i].key_b64.as_ref() == Some(key))
            });
            let end = visible.iter().position(|&i| i == index).unwrap();
            let start = anchor.unwrap_or(end);
            if !primary {
                self.redis_keys.keys.clear();
            }
            for &i in &visible[start.min(end)..=start.max(end)] {
                if let Some(key) = &self.resources[i].key_b64 {
                    self.redis_keys.keys.insert(key.clone());
                }
            }
        } else if primary {
            if !self.redis_keys.keys.remove(&key) {
                self.redis_keys.keys.insert(key.clone());
            }
            self.redis_keys.anchor = Some(key);
        } else {
            self.redis_keys.keys.clear();
            self.redis_keys.keys.insert(key.clone());
            self.redis_keys.anchor = Some(key);
            self.select_resource(index, backend);
        }
    }

    pub(super) fn redis_key_menu(
        &mut self,
        index: usize,
        action: &str,
        backend: &Backend,
    ) -> Option<String> {
        self.redis_prune_key_selection();
        let key = self.resources.get(index)?.key_b64.clone()?;
        if !self.redis_keys.keys.contains(&key) {
            self.redis_keys.keys.clear();
            self.redis_keys.keys.insert(key.clone());
            self.redis_keys.anchor = Some(key);
        }
        match action {
            "open" => self.select_resource(index, backend),
            "delete" => self.action("redis-key-delete-request", backend),
            "copy" => {
                let mut values = Vec::new();
                for target in self.selected_redis_key_targets() {
                    let Some(encoded) = target.key_b64 else {
                        self.status = crate::i18n::UiMessage::key("Redis key 身份无效");
                        return None;
                    };
                    let Ok(bytes) = STANDARD.decode(&encoded) else {
                        self.status = crate::i18n::UiMessage::key("Redis key 身份无效");
                        return None;
                    };
                    values.push(match String::from_utf8(bytes) {
                        Ok(text) => serde_json::Value::String(text),
                        Err(_) => serde_json::json!({"base64": encoded}),
                    });
                }
                return serde_json::to_string(&values).ok();
            }
            _ => {}
        }
        None
    }
}

#[cfg(test)]
#[path = "../tests/unit/redis_key_selection.rs"]
mod tests;
