use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::Resource;
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Default)]
pub(super) struct Expansion(BTreeMap<(Uuid, u32), BTreeSet<Vec<u8>>>);

impl Expansion {
    pub fn contains(&self, profile: Uuid, db: u32, prefix: &[u8]) -> bool {
        self.0
            .get(&(profile, db))
            .is_some_and(|prefixes| prefixes.contains(prefix))
    }

    pub fn toggle(&mut self, profile: Uuid, db: u32, encoded_prefix: &str) {
        let Ok(prefix) = STANDARD.decode(encoded_prefix) else {
            return;
        };
        let prefixes = self.0.entry((profile, db)).or_default();
        if !prefixes.remove(prefix.as_slice()) {
            prefixes.insert(prefix);
        }
    }
}

pub(super) enum Kind<'a> {
    Directory { prefix: &'a [u8], expanded: bool },
    Key { index: usize },
}

pub(super) struct Row<'a> {
    pub label: &'a [u8],
    pub depth: usize,
    pub kind: Kind<'a>,
}

pub(super) struct Tree {
    keys: Vec<Vec<u8>>,
}

impl Tree {
    pub fn from_resources(resources: &[Resource]) -> Self {
        Self {
            keys: resources
                .iter()
                .map(|resource| {
                    resource
                        .key_b64
                        .as_deref()
                        .and_then(|value| STANDARD.decode(value).ok())
                        .unwrap_or_else(|| resource.name.as_bytes().to_vec())
                })
                .collect(),
        }
    }

    // 每个 key 只持有一份字节；目录借用前缀，遇到折叠目录即停止拆分。
    // 可见行使用扁平序列，构造、渲染和释放均不随 key 深度递归。
    pub fn rows(&self, expansion: &Expansion, owner: Option<(Uuid, u32)>) -> Vec<Row<'_>> {
        let mut rows = BTreeMap::new();
        for (index, bytes) in self.keys.iter().enumerate() {
            let mut start = 0;
            let mut depth = 0;
            let mut collapsed = false;
            for (offset, _) in bytes.iter().enumerate().filter(|(_, byte)| **byte == b':') {
                let prefix = &bytes[..=offset];
                let expanded =
                    owner.is_some_and(|(profile, db)| expansion.contains(profile, db, prefix));
                rows.entry((prefix, false)).or_insert(Row {
                    label: &bytes[start..offset],
                    depth,
                    kind: Kind::Directory { prefix, expanded },
                });
                if !expanded {
                    collapsed = true;
                    break;
                }
                start = offset + 1;
                depth += 1;
            }
            if !collapsed {
                rows.insert(
                    (bytes.as_slice(), true),
                    Row {
                        label: &bytes[start..],
                        depth,
                        kind: Kind::Key { index },
                    },
                );
            }
        }
        rows.into_values().collect()
    }
}

pub(super) fn encoded(bytes: &[u8]) -> String {
    STANDARD.encode(bytes)
}

pub(super) fn display(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) if !text.starts_with("base64:") => text.to_owned(),
        _ => format!("base64:{}", encoded(bytes)),
    }
}

#[cfg(test)]
#[path = "../tests/unit/redis_tree.rs"]
mod tests;
