//! 结构设计标签的会话、请求、草稿和任务路由。
use super::schema::{SchemaDraft, SchemaEditor, SchemaOperation, SchemaRequest};
use crate::{App, Backend, Msg, Profile, ResourceTarget, Result, Uuid, Value, i18n, json};
use datapier_core::model::schema::{
    SchemaDefinition, TableDefinition, TriggerDefinition, ViewDefinition,
};

#[derive(Clone)]
pub(crate) struct TriggerList {
    pub target: ResourceTarget,
    pub token: Option<Uuid>,
    pub resources: Vec<datapier_core::model::Resource>,
    pub cursor: Option<String>,
    pub error: Option<String>,
}

/// 新对象的初始值不包含 catalog；触发器所属表通过用户操作保存到草稿。
fn initial_definition(target: &ResourceTarget) -> Option<SchemaDefinition> {
    match target.kind.as_str() {
        "table" => Some(SchemaDefinition::Table(TableDefinition {
            columns: Vec::new(),
            indexes: Vec::new(),
            constraints: Vec::new(),
            engine: "InnoDB".into(),
            charset: None,
            collation: None,
            comment: String::new(),
            options: Vec::new(),
        })),
        "view" => Some(SchemaDefinition::View(ViewDefinition {
            select: String::new(),
            columns: Vec::new(),
            algorithm: "UNDEFINED".into(),
            definer: None,
            security: "DEFINER".into(),
            check_option: None,
            character_set_client: None,
            collation_connection: None,
        })),
        "trigger" => Some(SchemaDefinition::Trigger(TriggerDefinition {
            database: target.database.clone().unwrap_or_default(),
            name: target.name.clone(),
            table: String::new(),
            timing: "BEFORE".into(),
            event: "INSERT".into(),
            body: String::new(),
            definer: None,
            sql_mode: String::new(),
            character_set_client: None,
            collation_connection: None,
            order: None,
        })),
        _ => None,
    }
}

impl App {
    pub(crate) fn list_triggers(
        &mut self,
        backend: &Backend,
        target: ResourceTarget,
        cursor: Option<String>,
    ) {
        let Some(session) = self.session_id() else {
            self.status = i18n::UiMessage::key("请先连接数据库");
            return;
        };
        let token = Uuid::new_v4();
        if cursor.is_none() {
            self.workbench.trigger_list = Some(TriggerList {
                target: target.clone(),
                token: None,
                resources: Vec::new(),
                cursor: None,
                error: None,
            });
        }
        if let Some(list) = &mut self.workbench.trigger_list {
            list.token = Some(token);
            list.error = None;
        }
        self.request(
            backend,
            &format!("trigger-list/{token}"),
            "resource.list",
            json!({
                "session_id":session,"database":target.database,"kind":"trigger",
                "table":target.name,"cursor":cursor,"limit":200
            }),
        );
    }

    pub(crate) fn trigger_list_response(&mut self, token: &str, result: &Result<Value>) {
        let Some(list) = &mut self.workbench.trigger_list else {
            return;
        };
        if Uuid::parse_str(token).ok() != list.token {
            return;
        }
        list.token = None;
        match result {
            Ok(value) => {
                match serde_json::from_value::<datapier_core::model::ResourcePage>(value.clone()) {
                    Ok(page) => {
                        if !page.complete && page.cursor.is_none() {
                            list.error = Some(
                                i18n::tr("触发器列表不完整：权限不足或定义不可见").into_owned(),
                            );
                        }
                        list.resources.extend(page.resources);
                        list.cursor = page.cursor;
                    }
                    Err(error) => list.error = Some(error.to_string()),
                }
            }
            Err(error) => list.error = Some(format!("无法读取触发器列表或定义权限不足：{error}")),
        }
    }

    /// 新开独立会话的结构标签。新触发器可传入当前表名，保存为可恢复的操作。
    pub(crate) fn advanced_schema_open(
        &mut self,
        backend: &Backend,
        target: ResourceTarget,
        creating: bool,
        trigger_table: Option<String>,
    ) {
        let Some(profile) = self.profile.as_ref() else {
            self.status = i18n::UiMessage::key("请先连接数据库");
            return;
        };
        if profile.provider != datapier_core::model::ProviderKind::Mysql
            || target.database.as_deref().is_none_or(str::is_empty)
            || !matches!(target.kind.as_str(), "table" | "view" | "trigger")
        {
            self.status = i18n::UiMessage::key("结构设计需要 MySQL 数据库和有效对象");
            return;
        }
        let profile_revision = profile.revision.to_string();
        let initial = if creating {
            initial_definition(&target)
        } else {
            None
        };
        self.create_query_tab();
        self.tabs[self.active].title = if target.name.is_empty() {
            i18n::tr("新建结构").into_owned()
        } else {
            target.name.clone()
        };
        self.database = target.database.clone();
        self.target = Some(target.clone());
        self.view = "advanced-schema".into();
        let mut state = SchemaEditor::new(Value::Null, target, profile_revision, initial);
        if let Some(table) =
            trigger_table.filter(|_| creating && state.draft.target.kind == "trigger")
            && let Err(error) = state.apply_operation(SchemaOperation::Set {
                path: "/table".into(),
                value: Value::String(table),
            })
        {
            state.error = Some(error);
        }
        self.advanced_schema = Some(state);
        self.dirty = true;
        self.advanced_schema_connect(backend);
    }

    /// 从工作区的 SchemaDraft 恢复活动标签；不会恢复数据库基线或提交令牌。
    pub(crate) fn advanced_schema_restore(&mut self, backend: &Backend, draft: SchemaDraft) {
        let initial = if draft.creating {
            initial_definition(&draft.target)
        } else {
            None
        };
        if draft.creating && initial.is_none() {
            self.status = i18n::UiMessage::key("无法恢复未知结构类型");
            return;
        }
        self.database = draft.target.database.clone();
        self.target = Some(draft.target.clone());
        self.view = "advanced-schema".into();
        self.advanced_schema = Some(SchemaEditor::restore(
            Value::Null,
            String::new(),
            draft,
            initial,
        ));
        self.advanced_schema_connect(backend);
    }

    /// 活动和后台 tab 分别从对应运行态取得，不序列化快照、预览或请求。
    pub(crate) fn advanced_schema_draft(&self, index: usize) -> Option<SchemaDraft> {
        let state = if index == self.active {
            self.advanced_schema.as_ref()
        } else {
            self.tabs.get(index)?.runtime.advanced_schema.as_ref()
        }?;
        Some(state.draft.clone())
    }

    fn advanced_schema_connect(&mut self, backend: &Backend) {
        let Some(state) = self.advanced_schema.as_ref() else {
            return;
        };
        if state.connect_nonce.is_some() || !state.session.is_null() {
            return;
        }
        let Some(profile_id) = self.tabs.get(self.active).and_then(|tab| tab.profile_id) else {
            if let Some(state) = &mut self.advanced_schema {
                state.error = Some(i18n::tr("连接已不存在").into_owned());
            }
            return;
        };
        let database = state.draft.target.database.clone();
        let nonce = Uuid::new_v4();
        let tag = format!(
            "{}:{}:schema-connect/{nonce}",
            self.tabs[self.active].id, self.generation
        );
        match backend.request(
            tag,
            "connection.open",
            json!({"profile_id":profile_id,"database":database,"secret":null}),
        ) {
            Ok(()) => {
                if let Some(state) = &mut self.advanced_schema {
                    state.connect_nonce = Some(nonce);
                    state.error = None;
                }
            }
            Err(error) => {
                if let Some(state) = &mut self.advanced_schema {
                    state.error = Some(error.to_string());
                }
            }
        }
    }

    pub(crate) fn advanced_schema_send(&mut self, backend: &Backend, mut request: SchemaRequest) {
        request.params["workspace_id"] = json!(self.workspace_id);
        let tag = format!(
            "{}:{}:{}",
            self.tabs[self.active].id, self.generation, request.tag
        );
        if let Err(error) = backend.request(tag, request.method, request.params)
            && let Some(state) = &mut self.advanced_schema
        {
            state.response(&request.tag, &Err(error));
        }
    }

    /// 在通用 action 前调用；会由确认按钮显式触发提交。
    pub(crate) fn advanced_schema_action(&mut self, action: &str, backend: &Backend) -> bool {
        if action == "schema-connect" {
            self.advanced_schema_connect(backend);
            return true;
        }
        if matches!(action, "schema-query" | "schema-query-original") {
            let Some(state) = self.advanced_schema.as_ref() else {
                return true;
            };
            let sql = if action == "schema-query" {
                state.query_sql()
            } else {
                state
                    .snapshot
                    .as_ref()
                    .map(|snapshot| snapshot.ddl.clone())
                    .filter(|sql| !sql.is_empty())
            };
            if let Some(sql) = sql {
                let target = state.draft.target.clone();
                if let Some(state) = self.advanced_schema.as_mut() {
                    state.query_transferred = true;
                }
                self.create_query_tab();
                self.database = target.database.clone();
                self.tabs[self.active].browse.database = target.database.clone();
                self.tabs[self.active].title = format!("SQL · {}", target.name);
                self.tabs[self.active].query = crate::Editor::new(sql);
                self.view = "query".into();
                self.action("open", backend);
            }
            return true;
        }
        let Some(state) = &mut self.advanced_schema else {
            return false;
        };
        if action == "schema-query-resume" {
            state.query_transferred = false;
            return true;
        }
        if action == "schema-draft-discard-confirm" {
            if state.confirm_discard && !state.busy() && state.summary.is_none() {
                self.advanced_schema = None;
                self.view = "query".into();
                self.target = None;
                self.dirty = true;
            }
            return true;
        }
        if action.starts_with("schema-op:") {
            let consumed = state.action(action);
            if consumed && state.dirty {
                self.dirty = true;
            }
            return consumed;
        }
        let request = match action {
            "schema-describe" => state.describe(true),
            "schema-plan" => state.plan(),
            "schema-preview" => state.preview_page(0),
            "schema-preview-next" => state
                .preview
                .as_ref()
                .and_then(|v| v["next"].as_u64())
                .and_then(|offset| state.preview_page(offset)),
            "schema-discard" => {
                state.confirm_submit = false;
                state.discard()
            }
            "schema-submit" => {
                if state.preview.is_some() && !state.busy() {
                    state.confirm_submit = true;
                }
                None
            }
            "schema-submit-cancel" => {
                state.confirm_submit = false;
                None
            }
            "schema-submit-confirm" => {
                if !state.confirm_submit {
                    return true;
                }
                state.confirm_submit = false;
                state.submit()
            }
            "schema-draft-discard" => {
                if !state.busy() && state.summary.is_none() {
                    state.confirm_discard = true;
                }
                None
            }
            "schema-draft-discard-cancel" => {
                state.confirm_discard = false;
                None
            }
            "schema-unknown-acknowledge" => {
                if state.acknowledge_unknown() {
                    self.dirty = true;
                }
                None
            }
            "schema-restore-confirm" => {
                if let Err(error) = state.confirm_restore() {
                    state.error = Some(error);
                }
                None
            }
            _ => return false,
        };
        if let Some(request) = request {
            self.advanced_schema_send(backend, request);
        }
        true
    }

    /// 在普通字段分发前处理结构表单消息。
    pub(crate) fn advanced_schema_edit(&mut self, message: &Msg) -> bool {
        let Some(state) = &mut self.advanced_schema else {
            return false;
        };
        let handled = match message {
            Msg::Edit(name, from, to, text, cursor, len) => {
                state.edit(name, *from, *to, text, *cursor, *len)
            }
            Msg::Undo(name) => state.undo(name),
            _ => false,
        };
        if handled && state.dirty {
            self.dirty = true;
        }
        handled
    }

    /// tag 已由 App 验证 tab 和 generation；连接与每个 schema 请求再检查 nonce。
    pub(crate) fn advanced_schema_response(
        &mut self,
        tag: &str,
        result: &Result<Value>,
        backend: &Backend,
    ) -> bool {
        if let Some(token) = tag.strip_prefix("schema-connect/") {
            let Some(state) = &mut self.advanced_schema else {
                return true;
            };
            let Some(expected) = state.connect_nonce else {
                return true;
            };
            if Uuid::parse_str(token).ok() != Some(expected) {
                return true;
            }
            state.connect_nonce = None;
            match result {
                Ok(value) => {
                    let session = value.get("session_id").cloned();
                    let profile = value
                        .get("profile")
                        .cloned()
                        .and_then(|p| serde_json::from_value::<Profile>(p).ok());
                    if let (Some(session), Some(profile)) = (session, profile) {
                        state.profile_revision = profile.revision.to_string();
                        state.session = session;
                        self.profile = Some(profile);
                        self.session = Some(value.clone());
                        let request = if state.draft.creating {
                            None
                        } else {
                            state.describe(false)
                        };
                        if let Some(request) = request {
                            self.advanced_schema_send(backend, request);
                        }
                    } else {
                        state.error = Some(i18n::tr("数据库会话缺少有效连接信息").into_owned());
                    }
                }
                Err(error) => state.error = Some(error.to_string()),
            }
            return true;
        }
        if !tag.starts_with("schema/") {
            return false;
        }
        let Some(state) = &mut self.advanced_schema else {
            return true;
        };
        if !state.response(tag, result) {
            return true;
        }
        if let Some(task) = state
            .execution
            .as_ref()
            .and_then(|v| v.get("task_id"))
            .and_then(Value::as_str)
            && state.execution_pending
        {
            self.task = Some(task.to_owned());
        }
        let refresh = state.take_plan_refresh();
        if let Some(request) = refresh {
            self.advanced_schema_send(backend, request);
        }
        true
    }

    /// 在 execution.get 通用处理前调用；只消费结构标签当前任务。
    pub(crate) fn advanced_schema_task_response(
        &mut self,
        task: &str,
        result: &Result<Value>,
        backend: &Backend,
    ) -> bool {
        let Some(state) = &mut self.advanced_schema else {
            return false;
        };
        if !state.execution_pending
            || self.task.as_deref() != Some(task)
            || state
                .execution
                .as_ref()
                .and_then(|v| v.get("task_id"))
                .and_then(Value::as_str)
                != Some(task)
        {
            return false;
        }
        let value = match result {
            Ok(value) => value,
            Err(error) => {
                state.error = Some(error.to_string());
                return true;
            }
        };
        if value.get("id").and_then(Value::as_str) != Some(task) {
            return true;
        }
        let phase = value.get("state").and_then(Value::as_str).unwrap_or("");
        if let Some(execution) = state.execution.as_mut() {
            execution["result"] = value.clone();
        }
        if matches!(phase, "queued" | "running" | "cancelling") {
            return true;
        }
        if !matches!(
            phase,
            "succeeded" | "failed" | "cancelled" | "outcome_unknown" | "wait_stopped"
        ) {
            return true;
        }
        self.task = None;
        let outcome = value.pointer("/summary/state").and_then(Value::as_str);
        if matches!(phase, "outcome_unknown" | "wait_stopped") || outcome == Some("outcome_unknown")
        {
            state.outcome_unknown = true;
            state.error = Some(i18n::tr("执行结果待确认，请核对数据库").into_owned());
            return true;
        }
        let applied = phase == "succeeded"
            && outcome == Some("applied")
            && value
                .pointer("/summary/steps")
                .and_then(Value::as_array)
                .is_some_and(|steps| steps.iter().all(|step| step["state"] == "applied"));
        let execution_id = state
            .execution
            .as_ref()
            .and_then(|v| v.get("execution_id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(id) = execution_id
            && state.execution_finished(&id, applied)
        {
            self.dirty = true;
            let request = state.describe(true);
            if let Some(request) = request {
                self.advanced_schema_send(backend, request);
            }
        }
        true
    }
}
