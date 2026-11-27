use super::*;
use std::time::{Duration, Instant};

pub(super) mod analysis;
use analysis::{Analysis, Position};

const MAX_ITEMS: usize = 50;
// 为候选、分析快照和目录键预留空间，目录副本与这些对象合计不超过 2 MiB。
const MAX_CACHE: usize = 2 * 1024 * 1024 - 256 * 1024;
const CACHE_TTL: Duration = Duration::from_secs(60);

fn value_bytes(value: &Value) -> usize {
    match value {
        Value::String(text) => text.capacity(),
        Value::Array(items) => items
            .capacity()
            .saturating_mul(std::mem::size_of::<Value>())
            .saturating_add(items.iter().map(value_bytes).sum::<usize>()),
        Value::Object(items) => items
            .iter()
            .map(|(key, value)| {
                key.capacity()
                    .saturating_add(128)
                    .saturating_add(value_bytes(value))
            })
            .sum(),
        _ => 0,
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    tab: Uuid,
    revision: u64,
    generation: u64,
    cursor: usize,
    anchor: usize,
    session: Value,
    database: Option<String>,
    sql_mode: Option<(bool, bool)>,
    server_version: Option<String>,
    lower_case_table_names: Option<u8>,
    archery: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct CatalogKey {
    scope: &'static str,
    database: Option<String>,
    relation: Option<String>,
    prefix: String,
}

#[derive(Clone)]
struct Page {
    key: CatalogKey,
    items: Vec<Value>,
    initial: bool,
    loaded: Instant,
    bytes: usize,
    completeness: String,
    reason: Option<String>,
    next_cursor: Option<String>,
}

#[derive(Clone)]
struct Candidate {
    name: String,
    folded_name: String,
    insert: String,
    detail: String,
    kind: String,
}

fn candidate_order(a: &Candidate, b: &Candidate, prefix: &str) -> std::cmp::Ordering {
    (!a.name.starts_with(prefix))
        .cmp(&(!b.name.starts_with(prefix)))
        .then_with(|| a.folded_name.cmp(&b.folded_name))
        .then_with(|| a.name.cmp(&b.name))
        .then_with(|| a.detail.cmp(&b.detail))
}

// 只保留当前窗口和一个探测项；ordinal 区分排序键相同的候选。
fn offer_candidate(
    candidates: &mut Vec<(Candidate, usize)>,
    candidate: Candidate,
    ordinal: &mut usize,
    after: Option<&(Candidate, usize)>,
    prefix: &str,
) {
    let index = *ordinal;
    *ordinal += 1;
    let compare = |a: &(Candidate, usize), b: &(Candidate, usize)| {
        candidate_order(&a.0, &b.0, prefix).then_with(|| a.1.cmp(&b.1))
    };
    let entry = (candidate, index);
    if after.is_some_and(|after| compare(&entry, after).is_le()) {
        return;
    }
    let position = candidates.partition_point(|item| compare(item, &entry).is_le());
    if position <= MAX_ITEMS {
        candidates.insert(position, entry);
        if candidates.len() > MAX_ITEMS + 1 {
            candidates.pop();
        }
    }
}

#[derive(Clone)]
struct Snapshot {
    id: Uuid,
    stamp: Stamp,
    analysis: Analysis,
    candidates: Vec<Candidate>,
    selected: usize,
    after: Option<(Candidate, usize)>,
    ordinals: Vec<usize>,
    more_items: bool,
}

#[derive(Clone, Default)]
pub(super) struct CompletionState {
    observed: Option<Stamp>,
    current: Option<Snapshot>,
    pages: Vec<Page>,
    deadline: Option<Instant>,
    pending: Option<(Uuid, Stamp, CatalogKey, bool)>,
    continuation: Option<(CatalogKey, String)>,
    cancel: Option<(Value, Uuid, Uuid)>,
    catalog_generation: Option<u64>,
    request_generation: u64,
    dismissed: bool,
    message: Option<String>,
}

fn keyword_candidates(prefix: &str, server_version: Option<&str>) -> Vec<Candidate> {
    const WORDS: &[&str] = &[
        "SELECT", "FROM", "WHERE", "JOIN", "LEFT", "RIGHT", "INNER", "OUTER", "ON", "AS", "AND",
        "OR", "NOT", "NULL", "IS", "IN", "LIKE", "ORDER", "BY", "GROUP", "HAVING", "LIMIT",
        "OFFSET", "INSERT", "INTO", "VALUES", "UPDATE", "SET", "DELETE", "CREATE", "ALTER", "DROP",
        "TABLE", "DATABASE", "INDEX", "VIEW", "SHOW", "DESCRIBE", "EXPLAIN", "WITH", "UNION",
        "ALL", "DISTINCT", "CASE", "WHEN", "THEN", "ELSE", "END", "BEGIN", "COMMIT", "ROLLBACK",
    ];
    // 仅包含 MySQL 5.7 起均可用的函数；版本未知时也不会推荐未知版本能力。
    const FUNCTIONS: &[(&str, &str, &str)] = &[
        ("COUNT", "COUNT(expr)", "聚合函数"),
        ("SUM", "SUM(expr)", "聚合函数"),
        ("AVG", "AVG(expr)", "聚合函数"),
        ("MIN", "MIN(expr)", "聚合函数"),
        ("MAX", "MAX(expr)", "聚合函数"),
        ("CONCAT", "CONCAT(str, ...)", "字符串函数"),
        ("SUBSTRING", "SUBSTRING(str, pos, len)", "字符串函数"),
        ("LOWER", "LOWER(str)", "字符串函数"),
        ("UPPER", "UPPER(str)", "字符串函数"),
        ("ABS", "ABS(number)", "数值函数"),
        ("ROUND", "ROUND(number, decimals)", "数值函数"),
        ("NOW", "NOW()", "日期函数"),
        ("DATE_FORMAT", "DATE_FORMAT(date, format)", "日期函数"),
        ("COALESCE", "COALESCE(value, ...)", "空值处理函数"),
        ("IFNULL", "IFNULL(value, fallback)", "空值处理函数"),
    ];
    let prefix = prefix.to_lowercase();
    let mut items: Vec<Candidate> = WORDS
        .iter()
        .filter(|name| name.to_lowercase().starts_with(&prefix))
        .map(|name| Candidate {
            name: (*name).to_owned(),
            folded_name: name.to_lowercase(),
            insert: (*name).to_owned(),
            kind: "关键词".into(),
            detail: String::new(),
        })
        .collect();
    let supported = server_version
        .and_then(|version| {
            let mut parts = version.split('.');
            Some((
                parts.next()?.parse::<u32>().ok()?,
                parts.next()?.parse::<u32>().ok()?,
            ))
        })
        .is_none_or(|version| version >= (5, 7));
    if supported {
        items.extend(
            FUNCTIONS
                .iter()
                .filter(|(name, _, _)| name.to_lowercase().starts_with(&prefix))
                .map(|(name, signature, description)| Candidate {
                    name: (*name).to_owned(),
                    folded_name: name.to_lowercase(),
                    insert: (*name).to_owned(),
                    kind: "函数".into(),
                    detail: format!("{} · {}", signature, crate::i18n::tr(description)),
                }),
        );
    }
    items
}

fn quoted(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn keys(
    analysis: &Analysis,
    database: Option<&str>,
    lower_case_table_names: Option<u8>,
    archery: bool,
) -> Vec<CatalogKey> {
    let mut result = Vec::new();
    let mut add = |scope, db: Option<String>, relation: Option<String>| {
        let key = CatalogKey {
            scope,
            database: db,
            relation,
            prefix: analysis.prefix.clone(),
        };
        if !result.contains(&key) {
            result.push(key);
        }
    };
    match analysis.position {
        Position::Relation => {
            if let Some(db) = database {
                add("relations", Some(db.to_owned()), None);
            }
            add("databases", None, None);
        }
        Position::QualifiedRelation => {
            if let Some(db) = &analysis.qualifier {
                add("relations", Some(db.clone()), None);
            }
        }
        Position::QualifiedColumn | Position::Expression | Position::InsertColumn => {
            for relation in &analysis.relations {
                if matches!(analysis.position, Position::QualifiedColumn)
                    && !analysis.qualifier.as_deref().is_some_and(|q| {
                        q == relation.alias.as_str()
                            || (matches!(lower_case_table_names, Some(1 | 2))
                                && q.eq_ignore_ascii_case(&relation.alias))
                    })
                {
                    continue;
                }
                if let Some(db) = relation.database.as_deref().or(database) {
                    add("columns", Some(db.to_owned()), Some(relation.name.clone()));
                }
            }
        }
    }
    // 平台查询不为未限定字段逐表取结构；输入明确的别名后才读取该对象。
    if archery
        && !matches!(analysis.position, Position::QualifiedColumn)
        && result.iter().filter(|key| key.scope == "columns").count() > 1
    {
        result.retain(|key| key.scope != "columns");
    }
    result
}

fn rebuild(state: &mut CompletionState, database: Option<&str>) {
    let Some(snapshot) = state.current.as_mut() else {
        return;
    };
    snapshot.id = Uuid::new_v4();
    let analysis = &snapshot.analysis;
    let mut candidates = Vec::new();
    let mut ordinal = 0;
    if matches!(analysis.position, Position::Expression) {
        for candidate in
            keyword_candidates(&analysis.prefix, snapshot.stamp.server_version.as_deref())
        {
            offer_candidate(
                &mut candidates,
                candidate,
                &mut ordinal,
                snapshot.after.as_ref(),
                &analysis.prefix,
            );
        }
    }
    for key in keys(
        analysis,
        database,
        snapshot.stamp.lower_case_table_names,
        snapshot.stamp.archery,
    ) {
        if let Some(page) = state
            .pages
            .iter()
            .find(|page| page.key == key && page.loaded.elapsed() < CACHE_TTL)
        {
            for item in &page.items {
                let Some(name) = item.get("name").and_then(Value::as_str) else {
                    continue;
                };
                if !name
                    .to_lowercase()
                    .starts_with(&analysis.prefix.to_lowercase())
                {
                    continue;
                }
                if key.scope == "columns"
                    && analysis
                        .excluded_columns
                        .iter()
                        .any(|column| column.eq_ignore_ascii_case(name))
                {
                    continue;
                }
                let kind = match item.get("kind").and_then(Value::as_str) {
                    Some("database") => "库",
                    Some("table") => "表",
                    Some("view") => "视图",
                    Some("column") => "字段",
                    _ => "对象",
                };
                let source = [key.database.as_deref(), key.relation.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(".");
                let data_type = item.get("data_type").and_then(Value::as_str).unwrap_or("");
                let comment = item.get("comment").and_then(Value::as_str).unwrap_or("");
                let detail = [source.as_str(), data_type, comment]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ");
                let detail: String = if detail.chars().count() > 300 {
                    format!("{}…", detail.chars().take(300).collect::<String>())
                } else {
                    detail
                };
                let matching = analysis.relations.iter().filter(|r| {
                    Some(r.name.as_str()) == key.relation.as_deref()
                        && r.database.as_deref().or(database) == key.database.as_deref()
                });
                if key.scope == "columns"
                    && matches!(analysis.position, Position::Expression)
                    && analysis.relations.len() > 1
                {
                    for relation in matching {
                        offer_candidate(
                            &mut candidates,
                            Candidate {
                                name: name.to_owned(),
                                folded_name: name.to_lowercase(),
                                insert: format!("{}.{}", quoted(&relation.alias), quoted(name)),
                                detail: format!("{} · {}", relation.alias, detail),
                                kind: kind.to_owned(),
                            },
                            &mut ordinal,
                            snapshot.after.as_ref(),
                            &analysis.prefix,
                        );
                    }
                } else {
                    offer_candidate(
                        &mut candidates,
                        Candidate {
                            name: name.to_owned(),
                            folded_name: name.to_lowercase(),
                            insert: quoted(name),
                            detail,
                            kind: kind.to_owned(),
                        },
                        &mut ordinal,
                        snapshot.after.as_ref(),
                        &analysis.prefix,
                    );
                }
            }
        }
    }
    snapshot.more_items = candidates.len() > MAX_ITEMS;
    candidates.truncate(MAX_ITEMS);
    (snapshot.candidates, snapshot.ordinals) = candidates.into_iter().unzip();
    snapshot.selected = snapshot
        .selected
        .min(snapshot.candidates.len().saturating_sub(1));
}

impl App {
    fn completion_stamp(&self) -> Option<Stamp> {
        let tab = self.tabs.get(self.active)?;
        if self.focus != "query"
            || self.is_redis()
            || self.workbench.overlay.is_some()
            || self.operation_pending
            || self.task.is_some()
            || tab.query.anchor != tab.query.cursor
        {
            return None;
        }
        Some(Stamp {
            tab: tab.id,
            revision: self.query_edit_epoch,
            generation: self.generation,
            cursor: tab.query.cursor,
            anchor: tab.query.anchor,
            session: self.session_id()?,
            database: self.database.clone(),
            sql_mode: self
                .workbench
                .sql_mode
                .map(|mode| (mode.no_backslash_escapes, mode.ansi_quotes)),
            server_version: self.workbench.server_version.clone(),
            lower_case_table_names: self.workbench.lower_case_table_names,
            archery: self
                .profile
                .as_ref()
                .is_some_and(|profile| profile.is_archery()),
        })
    }

    pub(super) fn completion_invalidate(&mut self) {
        self.completion_close();
        let state = &mut self.workbench.query_ui.completion;
        state.pages.clear();
        state.catalog_generation = None;
        state.observed = None;
    }

    pub(super) fn completion_close(&mut self) {
        let state = &mut self.workbench.query_ui.completion;
        state.current = None;
        state.deadline = None;
        state.continuation = None;
        if let Some((id, stamp, _, _)) = state.pending.take() {
            state.cancel = Some((stamp.session, stamp.tab, id));
        }
        state.dismissed = true;
    }

    pub(super) fn completion_update(&mut self, manual: bool) -> bool {
        let stamp = self.completion_stamp();
        if !manual && self.workbench.query_ui.completion.observed == stamp {
            return false;
        }
        let Some(stamp) = stamp else {
            let state = &mut self.workbench.query_ui.completion;
            let changed = state.current.is_some();
            if let Some((id, stamp, _, _)) = state.pending.take() {
                state.cancel = Some((stamp.session, stamp.tab, id));
            }
            state.current = None;
            state.deadline = None;
            state.continuation = None;
            state.observed = None;
            state.pages.clear();
            state.catalog_generation = None;
            return changed;
        };
        if self
            .workbench
            .query_ui
            .completion
            .observed
            .as_ref()
            .is_some_and(|old| {
                old.tab != stamp.tab
                    || old.generation != stamp.generation
                    || old.session != stamp.session
            })
        {
            self.workbench.query_ui.completion.pages.clear();
            self.workbench.query_ui.completion.catalog_generation = None;
        }
        self.workbench.query_ui.completion.observed = Some(stamp.clone());
        let query = &self.tabs[self.active].query;
        let context = match self.sql_context() {
            Ok(context) => context,
            Err(_) => {
                self.completion_close();
                return false;
            }
        };
        let analyzed =
            analysis::analyze(&query.text, query.cursor, context, self.database.as_deref());
        let Some(analysis) = analyzed else {
            self.completion_close();
            return true;
        };
        let show_empty = matches!(
            analysis.position,
            Position::Relation
                | Position::QualifiedRelation
                | Position::QualifiedColumn
                | Position::InsertColumn
        );
        if analysis.prefix.is_empty() && !manual && !show_empty {
            self.completion_close();
            return true;
        }
        let Some(start) = editor::utf16_to_byte(&query.text, analysis.range.start) else {
            return false;
        };
        let Some(end) = editor::utf16_to_byte(&query.text, analysis.range.end) else {
            return false;
        };
        if analysis.prefix.len() > 1024 || end.saturating_sub(start) > 4096 {
            self.completion_close();
            return true;
        }
        let state = &mut self.workbench.query_ui.completion;
        // 续页仅服务当前浏览，重新打开时不能把它当成目录首屏。
        state
            .pages
            .retain(|page| page.initial && page.loaded.elapsed() < CACHE_TTL);
        state.observed = Some(stamp.clone());
        state.dismissed = false;
        state.message = None;
        state.continuation = None;
        if let Some((id, previous, _, _)) = state.pending.take() {
            state.cancel = Some((previous.session, previous.tab, id));
        }
        state.current = Some(Snapshot {
            id: Uuid::new_v4(),
            stamp,
            analysis,
            candidates: Vec::new(),
            selected: 0,
            after: None,
            ordinals: Vec::new(),
            more_items: false,
        });
        state.deadline = Some(
            Instant::now()
                + if manual {
                    Duration::ZERO
                } else {
                    Duration::from_millis(150)
                },
        );
        rebuild(state, self.database.as_deref());
        true
    }

    pub(super) fn completion_unsubscribe(&mut self, backend: &Backend) {
        let state = &mut self.workbench.query_ui.completion;
        if let Some((id, stamp, _, _)) = state.pending.take() {
            state.cancel = Some((stamp.session, stamp.tab, id));
        }
        if let Some((session, tab, id)) = state.cancel.take()
            && let Err(error) = backend.request(
                format!("completion.unsubscribe/{}", Uuid::new_v4()),
                "completion.unsubscribe",
                json!({"session_id":session,"editor_id":tab,"request_id":id}),
            )
        {
            self.status = i18n::UiMessage::raw(error.to_string());
        }
    }

    pub(super) fn completion_poll(&mut self, backend: &Backend) -> bool {
        let mut changed = self.completion_update(false);
        if self.workbench.query_ui.completion.cancel.is_some() {
            self.completion_unsubscribe(backend);
        }
        let state = &mut self.workbench.query_ui.completion;
        if state.pending.is_some()
            || state
                .deadline
                .is_none_or(|deadline| Instant::now() < deadline)
        {
            return changed;
        }
        state.deadline = None;
        let Some(snapshot) = state.current.as_ref() else {
            return changed;
        };
        let continuation = state.continuation.take();
        let (key, cursor) = if let Some((key, cursor)) = continuation {
            (key, Some(cursor))
        } else {
            let Some(key) = keys(
                &snapshot.analysis,
                self.database.as_deref(),
                snapshot.stamp.lower_case_table_names,
                snapshot.stamp.archery,
            )
            .into_iter()
            .find(|key| {
                !state
                    .pages
                    .iter()
                    .any(|page| &page.key == key && page.loaded.elapsed() < CACHE_TTL)
            }) else {
                return changed;
            };
            (key, None)
        };
        let request_id = Uuid::new_v4();
        let stamp = snapshot.stamp.clone();
        let Some(generation) = state.request_generation.checked_add(1) else {
            state.message = Some(crate::i18n::tr("补全请求代次已耗尽").into_owned());
            return true;
        };
        state.request_generation = generation;
        let continuing = cursor.is_some();
        let params = json!({"session_id":stamp.session,"editor_id":stamp.tab,"request_id":request_id,"context_generation":generation,
            "scope":key.scope,"database":key.database,"relation":key.relation,"prefix":key.prefix,"cursor":cursor,"limit":200});
        match backend.request(
            format!("{}:{}:completion/{request_id}", stamp.tab, stamp.generation),
            "completion.catalog",
            params,
        ) {
            Ok(()) => {
                state.pending = Some((request_id, stamp, key, continuing));
                changed = true;
            }
            Err(error) => {
                state.message = Some(error.to_string());
                changed = true;
            }
        }
        changed
    }

    pub(super) fn completion_response(&mut self, response: Response) {
        let current_stamp = self.completion_stamp();
        let state = &mut self.workbench.query_ui.completion;
        if !state
            .pending
            .as_ref()
            .is_some_and(|(id, _, _, _)| response.tag == format!("completion/{id}"))
        {
            return;
        }
        let Some((id, stamp, key, continuing)) = state.pending.take() else {
            return;
        };
        let request_generation = state.request_generation;
        if state.observed.as_ref() != Some(&stamp)
            || current_stamp.as_ref() != Some(&stamp)
            || state.dismissed
        {
            return;
        }
        match response.result {
            Ok(value)
                if value["request_id"].as_str() == Some(id.to_string().as_str())
                    && value["context_generation"].as_u64() == Some(request_generation)
                    && value["catalog_generation"].as_u64().is_some() =>
            {
                let generation = value["catalog_generation"].as_u64().unwrap();
                if state
                    .catalog_generation
                    .is_some_and(|old| old != generation)
                {
                    if continuing {
                        state.message =
                            Some(crate::i18n::tr("补全目录已失效，请重新触发补全").into_owned());
                        state.pages.clear();
                        if let Some(snapshot) = state.current.as_mut() {
                            snapshot.id = Uuid::new_v4();
                            snapshot.candidates.clear();
                            snapshot.more_items = false;
                        }
                        state.catalog_generation = None;
                        return;
                    }
                    state.pages.clear();
                }
                state.catalog_generation = Some(generation);
                let items = value["items"].as_array().cloned().unwrap_or_default();
                let bytes = items
                    .capacity()
                    .saturating_mul(std::mem::size_of::<Value>())
                    .saturating_add(items.iter().map(value_bytes).sum::<usize>());
                // 每个目录键只保留当前页；续页替换旧页，避免按需浏览累积整个目录。
                state.pages.retain(|page| page.key != key);
                if bytes <= MAX_CACHE {
                    let mut evicted = false;
                    while state.pages.iter().map(|page| page.bytes).sum::<usize>() + bytes
                        > MAX_CACHE
                        && !state.pages.is_empty()
                    {
                        state.pages.remove(0);
                        evicted = true;
                    }
                    state.pages.push(Page {
                        key,
                        items,
                        initial: !continuing,
                        loaded: Instant::now(),
                        bytes,
                        completeness: value["completeness"]
                            .as_str()
                            .unwrap_or("unknown")
                            .to_owned(),
                        reason: value["reason"].as_str().map(str::to_owned),
                        next_cursor: value["next_cursor"].as_str().map(str::to_owned),
                    });
                    if let Some(snapshot) = state.current.as_mut() {
                        snapshot.after = None;
                        snapshot.selected = 0;
                    }
                    rebuild(state, self.database.as_deref());
                    if evicted {
                        // 目录工作集超过预算时停止级联，避免不同对象互相淘汰并无限重读。
                        state.message = Some(crate::i18n::tr("补全目录超过内存限制").into_owned());
                    } else {
                        state.deadline = Some(Instant::now());
                    }
                } else {
                    state.message = Some(crate::i18n::tr("补全目录超过内存限制").into_owned());
                }
            }
            Ok(_) => {
                state.message = Some(crate::i18n::tr("补全响应无效").into_owned());
            }
            Err(error) => {
                state.message = Some(error.to_string());
            }
        }
    }
}

pub(super) fn suggestions(app: &App) -> Option<Node<Msg>> {
    let state = &app.workbench.query_ui.completion;
    let snapshot = state.current.as_ref()?;
    if app.completion_stamp().as_ref() != Some(&snapshot.stamp) {
        return None;
    }
    let mut rows = ui::table()
        .key(format!("completion-candidates-{}", snapshot.id))
        .col(
            ui::Column::new(
                "candidate",
                crate::i18n::tr("SQL 补全（仅供参考）").into_owned(),
            )
            .grow(1.0),
        );
    for (index, item) in snapshot.candidates.iter().enumerate() {
        let label = format!("{} · {}", item.insert, crate::i18n::tr(&item.kind));
        rows = rows.row(ui::TableRow::new(index.to_string()).cell("candidate", label));
    }
    let mut content = html::div()
        .class("completion")
        .child(html::div().class("completion-candidates").child(rows));
    if let Some(item) = snapshot
        .candidates
        .get(snapshot.selected)
        .filter(|item| !item.detail.is_empty())
    {
        content = content.child(html::div().child(html::span(item.detail.clone())));
    }
    content = content.child(html::span(
        crate::i18n::tr("宿主选区不可见，补全接受已禁用").into_owned(),
    ));
    if state.pending.is_some() {
        content = content.child(html::span(crate::i18n::tr("正在加载补全目录").into_owned()));
    }
    if let Some(message) = &state.message {
        content = content.child(html::span(message.clone()));
    }
    let current_keys = keys(
        &snapshot.analysis,
        app.database.as_deref(),
        snapshot.stamp.lower_case_table_names,
        snapshot.stamp.archery,
    );
    for page in state
        .pages
        .iter()
        .filter(|page| current_keys.contains(&page.key))
    {
        if page.completeness != "complete" && page.next_cursor.is_none() {
            let text = page.reason.as_deref().unwrap_or("请缩小补全前缀");
            content = content.child(html::span(crate::i18n::tr(text).into_owned()));
        }
    }
    Some(content.into())
}

#[cfg(test)]
#[path = "../tests/unit/completion.rs"]
mod tests;
