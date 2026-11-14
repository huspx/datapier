use super::*;

impl App {
    pub(crate) fn workspace_tree_items(&self) -> Vec<(String, usize, Option<bool>)> {
        let mut items = Vec::new();
        let Some(profile) = self.profile.as_ref() else {
            return items;
        };
        if self.is_redis() {
            if !self.workbench.tree || !self.redis_tree_matches_resources() || self.tabs.is_empty()
            {
                return items;
            }
            let db = self.tabs[self.active]
                .browse
                .redis_db
                .unwrap_or_else(|| profile.redis_db());
            let rows = self
                .workbench
                .redis_tree
                .rows(&self.workbench.redis_expansion, Some((profile.id, db)));
            for row in &rows {
                let (action, expanded) = match &row.kind {
                    crate::redis_tree::Kind::Directory { prefix, expanded } => (
                        format!("redis-tree:{}", crate::redis_tree::encoded(prefix)),
                        Some(*expanded),
                    ),
                    crate::redis_tree::Kind::Key { index } => {
                        let r = &self.resources[*index];
                        (
                            format!(
                                "redis-key-click:{}:{}:{index}:{}",
                                self.tabs[self.active].id,
                                self.generation,
                                r.key_b64.as_deref().unwrap_or("!")
                            ),
                            None,
                        )
                    }
                };
                items.push((action, row.depth, expanded));
            }
            return items;
        }
        let Some(tree) = self.navigation.databases.get(&profile.id) else {
            return items;
        };
        let needle = self.field("object_filter").to_lowercase();
        let visible = self
            .profiles
            .iter()
            .find(|saved| saved.id == profile.id)
            .and_then(navigation_visible_databases);
        for database in &tree.databases {
            if visible.is_some_and(|names| !names.contains(&database.name)) {
                continue;
            }
            let node = tree.nodes.get(&database.name);
            let expanded = node.is_some_and(|node| node.expanded);
            items.push((
                format!("navigation-db:{}", database.name),
                0,
                Some(expanded),
            ));
            let Some(node) = node.filter(|node| node.expanded) else {
                continue;
            };
            for kind in ["table", "view", "unknown"] {
                let matching: Vec<_> = node
                    .resources
                    .iter()
                    .enumerate()
                    .filter(|(_, resource)| {
                        resource.kind == kind && resource.name.to_lowercase().contains(&needle)
                    })
                    .collect();
                if matching.is_empty() {
                    continue;
                }
                let expanded = !node.collapsed_kinds.contains(kind);
                items.push((
                    format!("navigation-group:{kind}:{}", database.name),
                    1,
                    Some(expanded),
                ));
                if expanded {
                    for (index, _) in matching {
                        items.push((
                            format!("navigation-resource:{}:{index}", database.name),
                            2,
                            None,
                        ));
                    }
                }
            }
        }
        items
    }

    pub(crate) fn workspace_composite_key(
        &mut self,
        key: &tern_sdk::Key,
        backend: &Backend,
    ) -> bool {
        if key.ctrl || key.meta || key.alt {
            return false;
        }
        let Some(index) = self
            .focus
            .strip_prefix("workspace-tree-")
            .and_then(|s| s.parse::<usize>().ok())
        else {
            return false;
        };
        let items = self.workspace_tree_items();
        let Some((action, depth, expanded)) = items.get(index) else {
            return false;
        };
        let next = match key.name.as_str() {
            "up" => index.saturating_sub(1),
            "down" => (index + 1).min(items.len() - 1),
            "home" => 0,
            "end" => items.len() - 1,
            "right" if *expanded == Some(false) => {
                let action = action.clone();
                self.action(&action, backend);
                index
            }
            "right"
                if *expanded == Some(true)
                    && items.get(index + 1).is_some_and(|item| item.1 > *depth) =>
            {
                index + 1
            }
            "right" => index,
            "left" if *expanded == Some(true) => {
                let action = action.clone();
                self.action(&action, backend);
                index
            }
            "left" => (0..index)
                .rev()
                .find(|&i| items[i].1 < *depth)
                .unwrap_or(index),
            _ => return false,
        };
        self.focus = format!("workspace-tree-{next}");
        self.workbench.focus_request = Some(format!("input-workspace-tree-{next}"));
        true
    }
    pub(crate) fn navigation_apply_queued_section(&mut self) {
        let tab = self.tabs[self.active].id;
        if let Some(queued) = self.navigation.queued_sections.remove(&tab)
            && self.target.as_ref().is_some_and(|target| {
                target.database == queued.target.database
                    && target.name == queued.target.name
                    && target.kind == queued.target.kind
                    && target.key_b64 == queued.target.key_b64
            })
        {
            self.workbench.section = queued.section;
        }
    }

    pub(crate) fn navigation_form_open(&mut self, profile: Option<&Profile>) {
        let selected = profile.and_then(navigation_visible_databases);
        self.navigation.form = DatabaseForm {
            selected: selected.into_iter().flatten().cloned().collect(),
            all: selected.is_none(),
            ..DatabaseForm::default()
        };
        self.navigation.form_redis_discovery = RedisDiscovery::default();
        self.field_set("database_search", "");
        self.navigation.pending.retain(|_, p| {
            !matches!(
                p,
                Pending::Form { .. }
                    | Pending::RedisDiscovery {
                        profile_id: None,
                        ..
                    }
            )
        });
    }
    pub(crate) fn navigation_form_changed(&mut self) {
        self.navigation.pending.retain(|_, p| {
            !matches!(
                p,
                Pending::Form { .. }
                    | Pending::RedisDiscovery {
                        profile_id: None,
                        ..
                    }
            )
        });
        self.navigation.form_redis_discovery = RedisDiscovery::default();
        self.navigation.form.candidates.clear();
        self.navigation.form.loaded = false;
        self.navigation.form.loading = false;
        self.navigation.form.error = None;
    }
    pub(crate) fn navigation_invalidate_profile(&mut self, id: Uuid) {
        self.navigation.databases.remove(&id);
        self.navigation.redis_discovery.remove(&id);
        self.navigation.pending.retain(|_, p| !matches!(p, Pending::RedisDiscovery { profile_id: Some(profile), .. } if *profile == id));
        self.navigation
            .pending
            .retain(|_, p| !matches!(p, Pending::Tree { profile, .. } if *profile == id));
    }

    pub(crate) fn navigation_form_selection(&self) -> Option<Vec<String>> {
        (!self.navigation.form.all).then(|| self.navigation.form.selected.iter().cloned().collect())
    }

    pub(crate) fn navigation_connection_state(&self, id: Uuid) -> (&'static str, Option<String>) {
        let mut connecting = false;
        let mut error = None;
        for (index, tab) in self.tabs.iter().enumerate() {
            if tab.profile_id != Some(id) {
                continue;
            }
            let (session, workbench) = if index == self.active {
                (&self.session, &self.workbench)
            } else {
                (&tab.runtime.session, &tab.runtime.workbench)
            };
            if session.is_some() {
                return ("已连接", None);
            }
            connecting |= workbench.connecting;
            if error.is_none() {
                error = workbench.connection_error.clone();
            }
        }
        if connecting {
            ("连接中", None)
        } else if error.is_some() {
            ("连接失败", error)
        } else {
            ("未连接", None)
        }
    }

    pub(super) fn navigation_request(
        &mut self,
        backend: &Backend,
        pending: Pending,
        method: &str,
        params: Value,
    ) {
        let token = Uuid::new_v4();
        match backend.request(format!("navigation/{token}"), method, params) {
            Ok(()) => {
                self.navigation.pending.insert(token, pending);
            }
            Err(error) => self.navigation_finish(pending, Err(error), backend),
        }
    }

    pub(crate) fn navigation_load_tree(&mut self, backend: &Backend, refresh: bool) {
        let (Some(profile), Some(session)) = (self.profile.as_ref(), self.session_id()) else {
            return;
        };
        if profile.provider != datapier_core::model::ProviderKind::Mysql {
            self.redis_discover(backend, false);
            return;
        }
        let id = profile.id;
        if refresh {
            // 刷新保留展开意图，但旧目录请求不能再覆盖这一轮数据。
            self.navigation.pending.retain(
                |_, pending| !matches!(pending, Pending::Tree { profile, .. } if *profile == id),
            );
        }
        let visible = navigation_visible_databases(
            self.profiles.iter().find(|p| p.id == id).unwrap_or(profile),
        );
        let mut expanded = Vec::new();
        let tree = self.navigation.databases.entry(id).or_default();
        if !refresh && (tree.loading || tree.loaded) {
            return;
        }
        if refresh {
            for (database, node) in &mut tree.nodes {
                node.loaded = false;
                node.error = None;
                node.resources.clear();
                node.loading =
                    node.expanded && visible.is_none_or(|databases| databases.contains(database));
                if node.loading {
                    expanded.push(database.clone());
                }
            }
        }
        tree.loading = true;
        tree.error = None;
        self.navigation_request(
            backend,
            Pending::Tree {
                profile: id,
                database: None,
                session: json!(session),
            },
            "resource.list",
            json!({"session_id":session,"database":null,"limit":200}),
        );
        for database in expanded {
            self.navigation_request(
                backend,
                Pending::Tree {
                    profile: id,
                    database: Some(database.clone()),
                    session: session.clone(),
                },
                "resource.list",
                json!({"session_id":session,"database":database,"limit":200}),
            );
        }
    }

    pub(super) fn navigation_group_action(&mut self, name: &str) -> bool {
        if let Some((kind, database)) = name
            .strip_prefix("navigation-group:")
            .and_then(|value| value.split_once(':'))
        {
            if !matches!(kind, "table" | "view" | "unknown") {
                return true;
            }
            let Some(node) = self
                .profile
                .as_ref()
                .and_then(|profile| self.navigation.databases.get_mut(&profile.id))
                .and_then(|tree| tree.nodes.get_mut(database))
            else {
                return true;
            };
            if !node.collapsed_kinds.remove(kind) {
                node.collapsed_kinds.insert(kind.into());
            }
            return true;
        }
        false
    }

    pub(super) fn navigation_tree_action(&mut self, name: &str, backend: &Backend) -> bool {
        if let Some(database) = name.strip_prefix("navigation-db-refresh:") {
            let (Some(profile), Some(session)) = (self.profile.as_ref(), self.session_id()) else {
                return true;
            };
            let id = profile.id;
            if navigation_visible_databases(
                self.profiles.iter().find(|p| p.id == id).unwrap_or(profile),
            )
            .is_some_and(|visible| !visible.iter().any(|item| item == database))
            {
                return true;
            }
            if !self
                .navigation
                .databases
                .get(&id)
                .is_some_and(|tree| tree.nodes.contains_key(database))
            {
                return true;
            }
            self.navigation.pending.retain(|_, pending| !matches!(pending,
                Pending::Tree { profile, database: Some(pending_database), .. } if *profile == id && pending_database == database));
            let node = self
                .navigation
                .databases
                .get_mut(&id)
                .unwrap()
                .nodes
                .get_mut(database)
                .unwrap();
            node.loading = true;
            node.loaded = false;
            node.error = None;
            node.expanded = true;
            self.navigation_request(
                backend,
                Pending::Tree {
                    profile: id,
                    database: Some(database.into()),
                    session: session.clone(),
                },
                "resource.list",
                json!({"session_id":session,"database":database,"limit":200}),
            );
            return true;
        }
        if let Some(database) = name.strip_prefix("navigation-db:") {
            let (Some(profile), Some(session)) = (self.profile.as_ref(), self.session_id()) else {
                return true;
            };
            let id = profile.id;
            let visible = self.profiles.iter().find(|p| p.id == id).unwrap_or(profile);
            if navigation_visible_databases(visible)
                .is_some_and(|v| !v.iter().any(|v| v == database))
            {
                return true;
            }
            if profile.is_archery()
                && profile.provider == datapier_core::model::ProviderKind::Mysql
                && self.database.as_deref() != Some(database)
            {
                let new_tab = self.target.is_some()
                    || self.task.is_some()
                    || self.operation_pending
                    || self.has_live_drafts()
                    || self.workbench.inline.is_some();
                if new_tab {
                    self.create_query_tab();
                }
                self.close_result_info(false);
                self.database = Some(database.to_owned());
                self.tabs[self.active].browse.database = self.database.clone();
                self.generation = self.generation.wrapping_add(1);
                self.workbench.sql_mode = None;
                self.dirty = true;
                if new_tab {
                    self.view = "query".into();
                    self.action("open", backend);
                }
            }
            let node = self
                .navigation
                .databases
                .entry(id)
                .or_default()
                .nodes
                .entry(database.to_string())
                .or_default();
            node.expanded = !node.expanded;
            if node.expanded && !node.loaded && !node.loading {
                node.loading = true;
                node.error = None;
                self.navigation_request(
                    backend,
                    Pending::Tree {
                        profile: id,
                        database: Some(database.into()),
                        session: json!(session),
                    },
                    "resource.list",
                    json!({"session_id":session,"database":database,"limit":200}),
                );
            }
            return true;
        }
        if let Some(rest) = name.strip_prefix("navigation-triggers:") {
            if let Some((database, index)) = rest.rsplit_once(':') {
                let resource = self
                    .profile
                    .as_ref()
                    .and_then(|profile| self.navigation.databases.get(&profile.id))
                    .and_then(|tree| tree.nodes.get(database))
                    .and_then(|node| {
                        index
                            .parse::<usize>()
                            .ok()
                            .and_then(|index| node.resources.get(index))
                    })
                    .filter(|resource| resource.kind == "table")
                    .cloned();
                if let Some(resource) = resource {
                    self.list_triggers(
                        backend,
                        ResourceTarget {
                            database: Some(database.into()),
                            name: resource.name,
                            key_b64: None,
                            kind: "table".into(),
                        },
                        None,
                    );
                }
            }
            return true;
        }
        let resource_action = if let Some(rest) = name.strip_prefix("navigation-resource-section:")
        {
            rest.split_once(':').and_then(|(section, rest)| {
                matches!(section, "data" | "structure" | "ddl").then_some((section, rest))
            })
        } else {
            name.strip_prefix("navigation-resource:")
                .map(|rest| ("data", rest))
        };
        if let Some((section, rest)) = resource_action {
            if let Some((database, index)) = rest.rsplit_once(':') {
                if self.profile.as_ref().is_none_or(|profile| {
                    let saved = self
                        .profiles
                        .iter()
                        .find(|saved| saved.id == profile.id)
                        .unwrap_or(profile);
                    navigation_visible_databases(saved)
                        .is_some_and(|visible| !visible.iter().any(|item| item == database))
                }) {
                    return true;
                }
                let resource = self
                    .profile
                    .as_ref()
                    .and_then(|p| self.navigation.databases.get(&p.id))
                    .and_then(|tree| tree.nodes.get(database))
                    .and_then(|node| {
                        index
                            .parse::<usize>()
                            .ok()
                            .and_then(|i| node.resources.get(i))
                    })
                    .cloned();
                if let Some(resource) = resource {
                    if !self.finish_inline(true) {
                        return true;
                    }
                    let target = ResourceTarget {
                        database: Some(database.into()),
                        name: resource.name,
                        key_b64: resource.key_b64,
                        kind: resource.kind,
                    };
                    if let Some(index) = self.tabs.iter().enumerate().find_map(|(i, tab)| {
                        let current = if i == self.active {
                            &self.target
                        } else {
                            &tab.browse.target
                        };
                        (tab.profile_id == self.profile.as_ref().map(|p| p.id)
                            && current.as_ref().is_some_and(|t| {
                                t.database == target.database
                                    && t.name == target.name
                                    && t.kind == target.kind
                            }))
                        .then_some(i)
                    }) {
                        self.select_tab(index);
                        self.view = "data".into();
                        // 工作区只恢复资源上下文；该 tab 的数据库会话必须重新建立。
                        if self.session_id().is_none() {
                            self.navigation.queued_sections.insert(
                                self.tabs[self.active].id,
                                QueuedSection {
                                    target: target.clone(),
                                    section: section.into(),
                                },
                            );
                            self.action("open", backend);
                        } else {
                            self.action(&format!("section:{section}"), backend);
                        }
                    } else if self.target.is_some()
                        || self.task.is_some()
                        || self.operation_pending
                        || self.has_live_drafts()
                        || !self.tabs[self.active].query.text.is_empty()
                    {
                        self.action("new-tab", backend);
                        self.workbench.queued_target = Some(target.clone());
                        self.navigation.queued_sections.insert(
                            self.tabs[self.active].id,
                            QueuedSection {
                                target,
                                section: section.into(),
                            },
                        );
                        self.action("open", backend);
                    } else {
                        self.load_target(target, backend);
                        self.action(&format!("section:{section}"), backend);
                    }
                }
            }
            return true;
        }
        false
    }
}
