//! 引用表元数据按编辑会话和稳定外键标识隔离。
use std::collections::{BTreeMap, BTreeSet};

use crate::{Result, Uuid, Value, json};
use datapier_core::model::schema::{ConstraintDefinition, SchemaDefinition};
use datapier_core::model::{ResourcePage, ResourceTarget, TableSchema};

use super::{SchemaEditor, SchemaRequest};

#[derive(Clone, Default)]
pub(crate) struct ReferenceEntry {
    pub database: String,
    pub table: String,
    pub tables: Vec<String>,
    pub columns: Vec<String>,
    pub tables_loading: bool,
    pub columns_loading: bool,
    pub tables_error: Option<String>,
    pub columns_error: Option<String>,
    pub tables_complete: bool,
    pub columns_complete: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Tables,
    Columns,
}

#[derive(Clone)]
struct Pending {
    id: String,
    database: String,
    table: String,
    session: Value,
    kind: Kind,
    cursors: BTreeSet<String>,
}

#[derive(Clone, Default)]
pub(crate) struct ReferenceState {
    pub(crate) entries: BTreeMap<String, ReferenceEntry>,
    pending: BTreeMap<Uuid, Pending>,
    session: Value,
    target_database: Option<String>,
    target_table: String,
}

fn foreign_keys(editor: &SchemaEditor) -> BTreeMap<String, (String, String)> {
    let Some(SchemaDefinition::Table(table)) = editor.definition.as_ref() else {
        return BTreeMap::new();
    };
    table
        .constraints
        .iter()
        .filter_map(|constraint| {
            if let ConstraintDefinition::ForeignKey {
                id,
                referenced_database,
                referenced_table,
                ..
            } = constraint
            {
                let database = if referenced_database.is_empty() {
                    editor.draft.target.database.clone().unwrap_or_default()
                } else {
                    referenced_database.clone()
                };
                Some((id.clone(), (database, referenced_table.clone())))
            } else {
                None
            }
        })
        .collect()
}

impl ReferenceState {
    /// 上下文变化后清除缓存和未完成请求。
    pub(crate) fn sync(&mut self, editor: &SchemaEditor) {
        if self.session != editor.session
            || self.target_database != editor.draft.target.database
            || self.target_table != editor.draft.target.name
        {
            self.entries.clear();
            self.pending.clear();
            self.session = editor.session.clone();
            self.target_database = editor.draft.target.database.clone();
            self.target_table = editor.draft.target.name.clone();
        }
        let keys = foreign_keys(editor);
        self.entries.retain(|id, entry| {
            keys.get(id)
                .is_some_and(|(db, table)| entry.database == *db && entry.table == *table)
        });
        self.pending.retain(|_, request| {
            keys.get(&request.id)
                .is_some_and(|(db, table)| request.database == *db && request.table == *table)
        });
    }

    pub(crate) fn load(
        &mut self,
        editor: &SchemaEditor,
        id: &str,
        part: &str,
    ) -> Option<SchemaRequest> {
        self.sync(editor);
        let kind = match part {
            "tables" => Kind::Tables,
            "columns" => Kind::Columns,
            _ => return None,
        };
        let (database, table) = foreign_keys(editor).remove(id)?;
        if editor.session.is_null()
            || database.is_empty()
            || (kind == Kind::Columns && table.is_empty())
        {
            return None;
        }
        if kind == Kind::Columns
            && editor.draft.target.database.as_deref() == Some(database.as_str())
            && editor.draft.target.name == table
        {
            return None;
        }
        let entry = self
            .entries
            .entry(id.to_owned())
            .or_insert_with(|| ReferenceEntry {
                database: database.clone(),
                table: table.clone(),
                ..Default::default()
            });
        match kind {
            Kind::Tables if entry.tables_loading => return None,
            Kind::Columns if entry.columns_loading => return None,
            Kind::Tables => {
                entry.tables.clear();
                entry.tables_loading = true;
                entry.tables_complete = false;
                entry.tables_error = None;
            }
            Kind::Columns => {
                entry.columns.clear();
                entry.columns_loading = true;
                entry.columns_complete = false;
                entry.columns_error = None;
            }
        }
        self.request(
            Pending {
                id: id.to_owned(),
                database,
                table,
                session: editor.session.clone(),
                kind,
                cursors: BTreeSet::new(),
            },
            None,
        )
    }

    fn request(&mut self, pending: Pending, cursor: Option<String>) -> Option<SchemaRequest> {
        let token = Uuid::new_v4();
        let (method, params) = match pending.kind {
            Kind::Tables => (
                "resource.list",
                json!({"session_id":pending.session,"database":pending.database,"cursor":cursor,"limit":200}),
            ),
            Kind::Columns => (
                "resource.describe",
                json!({"session_id":pending.session,"target":ResourceTarget { database: Some(pending.database.clone()), name: pending.table.clone(), key_b64: None, kind: "table".into() }}),
            ),
        };
        self.pending.insert(token, pending);
        Some(SchemaRequest {
            tag: format!("schema-ref/{token}"),
            method,
            params,
        })
    }

    pub(crate) fn response(
        &mut self,
        editor: &SchemaEditor,
        token: &str,
        result: &Result<Value>,
    ) -> Option<SchemaRequest> {
        self.sync(editor);
        let token = Uuid::parse_str(token).ok()?;
        let mut pending = self.pending.remove(&token)?;
        if pending.session != editor.session {
            return None;
        }
        let entry = self.entries.get_mut(&pending.id)?;
        let outcome: std::result::Result<Option<String>, String> = match result {
            Err(error) => Err(error.to_string()),
            Ok(value) => match pending.kind {
                Kind::Tables => match serde_json::from_value::<ResourcePage>(value.clone()) {
                    Ok(page) => {
                        entry.tables.extend(
                            page.resources
                                .into_iter()
                                .filter(|r| r.kind == "table" || r.kind == "unknown")
                                .map(|r| r.name),
                        );
                        if page.complete {
                            Ok(None)
                        } else if let Some(cursor) = page.cursor.filter(|cursor| !cursor.is_empty())
                        {
                            if pending.cursors.insert(cursor.clone()) {
                                Ok(Some(cursor))
                            } else {
                                Err("资源列表游标重复，无法继续读取".into())
                            }
                        } else {
                            Err("资源列表不完整，无法继续读取".into())
                        }
                    }
                    Err(error) => Err(error.to_string()),
                },
                Kind::Columns => match serde_json::from_value::<TableSchema>(value.clone()) {
                    Ok(schema)
                        if schema.target.database.as_deref() == Some(pending.database.as_str())
                            && schema.target.name == pending.table =>
                    {
                        entry.columns = schema
                            .columns
                            .into_iter()
                            .map(|column| column.name)
                            .collect();
                        Ok(None)
                    }
                    Ok(_) => Err("返回的表结构与请求目标不一致".into()),
                    Err(error) => Err(error.to_string()),
                },
            },
        };
        match outcome {
            Ok(Some(cursor)) => self.request(pending, Some(cursor)),
            Ok(None) => {
                match pending.kind {
                    Kind::Tables => {
                        entry.tables_loading = false;
                        entry.tables_complete = true;
                    }
                    Kind::Columns => {
                        entry.columns_loading = false;
                        entry.columns_complete = true;
                    }
                }
                None
            }
            Err(error) => {
                match pending.kind {
                    Kind::Tables => {
                        entry.tables_loading = false;
                        entry.tables_error = Some(error);
                    }
                    Kind::Columns => {
                        entry.columns_loading = false;
                        entry.columns_error = Some(error);
                    }
                }
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/advanced/schema_reference.rs"]
mod tests;
