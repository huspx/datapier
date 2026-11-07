use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::Resource;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{Hash, Hasher};
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

// Nodes and sibling links are flat so even exceptionally deep keys can be
// built, visited, cloned and dropped without recursive calls.
#[derive(Clone, Default)]
pub(super) struct Tree {
    keys: Vec<Vec<u8>>,
    nodes: Vec<Node>,
    directories: HashMap<(usize, u64), usize>,
    first: Option<usize>,
    last: Option<usize>,
}

#[derive(Clone)]
struct Node {
    key: usize,
    start: usize,
    end: usize,
    directory: bool,
    first: Option<usize>,
    last: Option<usize>,
    next: Option<usize>,
    hash_next: Option<usize>,
}

impl Tree {
    #[cfg(test)]
    pub fn from_resources(resources: &[Resource]) -> Self {
        let mut tree = Self::default();
        tree.append(resources);
        tree
    }

    pub fn reset(&mut self, resources: &[Resource]) {
        *self = Self::default();
        self.append(resources);
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn append(&mut self, resources: &[Resource]) {
        for resource in resources {
            let bytes = resource
                .key_b64
                .as_deref()
                .and_then(|value| STANDARD.decode(value).ok())
                .unwrap_or_else(|| resource.name.as_bytes().to_vec());
            let key = self.keys.len();
            self.keys.push(bytes);
            // usize::MAX denotes the root, which is not itself a row.
            let mut parent = usize::MAX;
            let mut start = 0;
            for end in 0..self.keys[key].len() {
                if self.keys[key][end] != b':' {
                    continue;
                }
                let mut hasher = std::collections::hash_map::DefaultHasher::new();
                self.keys[key][start..end].hash(&mut hasher);
                let lookup = (parent, hasher.finish());
                let mut found = self.directories.get(&lookup).copied();
                while let Some(index) = found {
                    let node = &self.nodes[index];
                    if self.keys[node.key][node.start..node.end] == self.keys[key][start..end] {
                        break;
                    }
                    found = node.hash_next;
                }
                parent = match found {
                    Some(index) => index,
                    None => {
                        let index = self.push_node(
                            parent,
                            Node {
                                key,
                                start,
                                end,
                                directory: true,
                                first: None,
                                last: None,
                                next: None,
                                hash_next: self.directories.get(&lookup).copied(),
                            },
                        );
                        self.directories.insert(lookup, index);
                        index
                    }
                };
                start = end + 1;
            }
            self.push_node(
                parent,
                Node {
                    key,
                    start,
                    end: self.keys[key].len(),
                    directory: false,
                    first: None,
                    last: None,
                    next: None,
                    hash_next: None,
                },
            );
        }
    }

    fn push_node(&mut self, parent: usize, node: Node) -> usize {
        let index = self.nodes.len();
        self.nodes.push(node);
        let last = if parent == usize::MAX {
            self.last
        } else {
            self.nodes[parent].last
        };
        if let Some(last) = last {
            self.nodes[last].next = Some(index);
        } else if parent == usize::MAX {
            self.first = Some(index);
        } else {
            self.nodes[parent].first = Some(index);
        }
        if parent == usize::MAX {
            self.last = Some(index);
        } else {
            self.nodes[parent].last = Some(index);
        }
        index
    }

    pub fn rows(&self, expansion: &Expansion, owner: Option<(Uuid, u32)>) -> Vec<Row<'_>> {
        let mut rows = Vec::new();
        let mut pending = Vec::new();
        if let Some(first) = self.first {
            pending.push((first, 0));
        }
        while let Some((index, depth)) = pending.pop() {
            let node = &self.nodes[index];
            if let Some(next) = node.next {
                pending.push((next, depth));
            }
            let bytes = &self.keys[node.key];
            let kind = if node.directory {
                let prefix = &bytes[..=node.end];
                let expanded =
                    owner.is_some_and(|(profile, db)| expansion.contains(profile, db, prefix));
                if expanded && let Some(first) = node.first {
                    pending.push((first, depth + 1));
                }
                Kind::Directory { prefix, expanded }
            } else {
                Kind::Key { index: node.key }
            };
            rows.push(Row {
                label: &bytes[node.start..node.end],
                depth,
                kind,
            });
        }
        rows
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
