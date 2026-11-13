//! 结构草稿保存在 Engine 内，预览和提交复用同一份不可变 DDL 计划。
use super::*;
use datapier_core::model::schema::{SchemaEditIntent, SchemaSnapshot};

async fn schema_read_with_timeout<T>(
    cancel: &CancellationToken,
    deadline: Option<tokio::time::Instant>,
    read: impl Future<Output = Result<T>>,
) -> Result<T> {
    match deadline {
        Some(deadline) => tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(AppError::cancelled()),
            result = read => result,
            _ = tokio::time::sleep_until(deadline) => {
                // 元数据使用独立连接；直接丢弃 future，避免等待协议清理阻塞编辑锁。
                cancel.cancel();
                Err(AppError::new("timeout", "查询超时"))
            }
        },
        None => tokio::select! {
            _ = cancel.cancelled() => Err(AppError::cancelled()),
            result = read => result,
        },
    }
}

fn summary(id: Uuid, draft: &SchemaDraft) -> Value {
    json!({"id":id,"plan_id":id,"revision":draft.revision,
        "state":if draft.execution.is_some() {"submitted"} else {"ready"},
        "complete":true,"total_items":draft.plan.steps.len(),
        "plan_digest":draft.plan.plan_digest,"digest":draft.plan.plan_digest,
        "source":draft.source,"target":draft.plan.intent.target,
        "submittable":draft.writable && draft.execution.is_none() && !draft.plan.steps.is_empty(),
        "execution_id":draft.execution})
}

impl Engine {
    pub(super) async fn schema_rpc(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        match method {
            "schema.describe" => {
                let session_id = model::param(&params, "session_id")?;
                let db = self.editing_profile(owner, session_id).await?;
                let target: model::ResourceTarget = model::param(&params, "target")?;
                let owner_cancel = self.editing_cancel(owner)?;
                if db.profile.provider == model::ProviderKind::Mysql && !db.profile.is_archery() {
                    let fallback = self.store.settings().await?.limits.query_timeout_seconds;
                    let timeout = mysql_query_timeout(&db.profile, fallback);
                    let cancel = owner_cancel.child_token();
                    schema_read_with_timeout(
                        &cancel,
                        Some(tokio::time::Instant::now() + Duration::from_secs(timeout)),
                        self.describe_edit_schema(owner, db, target, &cancel),
                    )
                    .await
                } else {
                    self.describe_edit_schema(owner, db, target, &owner_cancel)
                        .await
                }
            }
            "schema.plan" => self.prepare_schema(owner, params).await,
            "schema.preview" => self.preview_schema(owner, &params).await,
            "schema.submit" => self.submit_schema(owner, params).await,
            "schema.discard" => self.discard_schema(owner, &params).await,
            _ => Err(AppError::new("method_not_found", "未知结构编辑操作")),
        }
    }

    async fn describe_edit_schema(
        self: &Arc<Self>,
        owner: Uuid,
        db: Arc<Database>,
        target: model::ResourceTarget,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        let mut session = tokio::select! {
            _ = cancel.cancelled() => return Err(AppError::cancelled()),
            session = db.session.lock() => session,
        };
        match &mut *session {
            ProviderSession::Mysql(mysql) => {
                mysql
                    .schema_describe(json!({"target":target}), cancel)
                    .await
            }
            ProviderSession::Archery(archery) => {
                // Archery 的元数据完整性必须随来源保留，不以空属性重建现有对象。
                let record = self
                    .store
                    .create_task(
                        owner,
                        "default",
                        "schema.describe",
                        json!({"target":target}),
                        false,
                    )
                    .await?;
                let sink = ExecutionSink::new(self.store.clone(), record.id);
                let result = archery
                    .call("resource.describe", json!({"target":target}), &sink, cancel)
                    .await;
                let safe_summary = match &result {
                    Ok(_) => json!({"described":true}),
                    Err(error) => json!({"code":error.code}),
                };
                self.store
                    .update_task(
                        record.id,
                        if result.is_ok() {
                            "succeeded"
                        } else {
                            "failed"
                        },
                        safe_summary,
                        result.as_ref().err().cloned(),
                    )
                    .await?;
                super::schema::archery_snapshot(target, result?)
            }
            _ => Err(AppError::new("unsupported_type", "结构设计仅适用于 MySQL")),
        }
    }

    fn schema_gate(&self, owner: Uuid, id: Uuid) -> Result<Arc<AsyncMutex<()>>> {
        let state = self.state.lock().unwrap();
        let draft = state
            .editing
            .schemas
            .get(&id)
            .ok_or_else(|| AppError::new("handle_expired", "结构计划已失效"))?;
        if draft.owner != owner {
            return Err(AppError::new("permission_denied", "结构计划属于其他客户端"));
        }
        Ok(draft.gate.clone())
    }

    async fn prepare_schema(self: &Arc<Self>, owner: Uuid, params: Value) -> Result<Value> {
        let session_id: Uuid = model::param(&params, "session_id")?;
        let db = self.editing_profile(owner, session_id).await?;
        if db.profile.provider != model::ProviderKind::Mysql {
            return Err(AppError::invalid("结构设计仅适用于 MySQL"));
        }
        let cancel = self.editing_cancel(owner)?.child_token();
        let deadline = if db.profile.is_archery() {
            None
        } else {
            let fallback = self.store.settings().await?.limits.query_timeout_seconds;
            let timeout = mysql_query_timeout(&db.profile, fallback);
            Some(tokio::time::Instant::now() + Duration::from_secs(timeout))
        };
        self.prepare_schema_inner(owner, params, session_id, db, &cancel, deadline)
            .await
    }

    async fn prepare_schema_inner(
        self: &Arc<Self>,
        owner: Uuid,
        params: Value,
        session_id: Uuid,
        db: Arc<Database>,
        cancel: &CancellationToken,
        deadline: Option<tokio::time::Instant>,
    ) -> Result<Value> {
        let mut intent: SchemaEditIntent = model::param(&params, "intent")?;
        if intent.target.kind.eq_ignore_ascii_case("database") {
            if !matches!(&db.profile.connection, model::ConnectionConfig::Direct(_)) {
                return Err(AppError::new("permission_denied", "仅直连支持新建数据库"));
            }
            db.profile.ensure_write()?;
            if intent.target.database.as_deref() != Some(intent.target.name.as_str()) {
                return Err(AppError::invalid("数据库目标不匹配"));
            }
        }
        let previous_id: Option<Uuid> = optional(&params, "plan_id")?;
        let id = previous_id.unwrap_or_else(Uuid::new_v4);
        let gate = match previous_id {
            Some(id) => self.schema_gate(owner, id)?,
            None => Arc::new(AsyncMutex::new(())),
        };
        let _guard =
            schema_read_with_timeout(cancel, deadline, async { Ok(gate.lock().await) }).await?;
        let profile_gate = self.editing_gate(db.profile.id);
        let _profile_guard =
            schema_read_with_timeout(cancel, deadline, async { Ok(profile_gate.lock().await) })
                .await?;
        let expected: Option<u64> = optional(&params, "expected_revision")?;
        let revision = if previous_id.is_some() {
            let state = self.state.lock().unwrap();
            let old = state
                .editing
                .schemas
                .get(&id)
                .ok_or_else(|| AppError::new("handle_expired", "结构计划已失效"))?;
            if old.session_id != session_id
                || Some(old.revision) != expected
                || old.execution.is_some()
            {
                return Err(AppError::new("stale_revision", "结构草稿已变化或已经提交"));
            }
            old.revision
                .checked_add(1)
                .ok_or_else(|| AppError::invalid("结构草稿版本溢出"))?
        } else {
            1
        };
        let metadata = async {
            if let Some(baseline) = &intent.baseline {
                let actual: SchemaSnapshot = serde_json::from_value(
                    self.describe_edit_schema(owner, db.clone(), intent.target.clone(), cancel)
                        .await?,
                )?;
                if actual.fingerprint != baseline.fingerprint {
                    return Err(AppError::new(
                        "schema_conflict",
                        "数据库结构已变化，请重新读取并确认草稿",
                    ));
                }
                intent.baseline = Some(actual);
            } else if !db.profile.is_archery() {
                match self
                    .describe_edit_schema(owner, db.clone(), intent.target.clone(), cancel)
                    .await
                {
                    Ok(_) => return Err(AppError::new("schema_conflict", "新建对象的名称已存在")),
                    Err(error) if error.code == "not_found" => (),
                    Err(error) => return Err(error),
                }
            }
            if db.profile.is_archery() {
                // 平台未提供服务端版本时，只能生成不依赖高级版本属性的本地 SQL 草稿。
                intent.server_version = intent
                    .baseline
                    .as_ref()
                    .map(|b| b.server_version.clone())
                    .unwrap_or_default();
                intent.sql_mode = intent
                    .baseline
                    .as_ref()
                    .map(|b| b.sql_mode.clone())
                    .unwrap_or_default();
            } else {
                let mut provider = db.session.lock().await;
                let ProviderSession::Mysql(mysql) = &mut *provider else {
                    return Err(AppError::invalid("非 MySQL 会话"));
                };
                let (version, mode, charset, collation) = mysql.schema_context(cancel).await?;
                intent.server_version = version;
                intent.sql_mode = mode.clone();
                match intent.definition.as_mut() {
                    Some(model::schema::SchemaDefinition::View(view)) => {
                        view.character_set_client = Some(charset);
                        view.collation_connection = Some(collation);
                    }
                    Some(model::schema::SchemaDefinition::Trigger(trigger)) => {
                        trigger.sql_mode = mode;
                        trigger.character_set_client = Some(charset);
                        trigger.collation_connection = Some(collation);
                    }
                    _ => {}
                }
            }
            Ok(())
        };
        schema_read_with_timeout(cancel, deadline, metadata).await?;
        intent.profile_revision = db.profile.revision.to_string();
        let plan = datapier_core::providers::mysql::schema::build_plan(&intent)?;
        let settings = self.store.settings().await?;
        let bytes = serde_json::to_vec(&plan)?.len();
        if bytes > settings.editing.max_plan_bytes
            || plan.steps.len() > settings.editing.max_plan_items
        {
            return Err(AppError::new("resource_limit", "结构计划超过编辑预算"));
        }
        // 单条 DDL 若不能完整预览则在准备阶段拒绝，不能只显示前缀后发送完整语句。
        for step in &plan.steps {
            if serde_json::to_vec(step)?.len() + 4096 > settings.limits.response_bytes {
                return Err(AppError::new(
                    "resource_limit",
                    "单条 DDL 超过完整预览预算，请缩小结构变更",
                ));
            }
        }
        self.store
            .reserve_auxiliary_for_owner(owner, id, bytes)
            .await?;
        let draft = SchemaDraft {
            owner,
            session_id,
            profile_id: db.profile.id,
            profile_revision: db.profile.revision,
            workspace: workspace(&params).to_owned(),
            revision,
            plan: Arc::new(plan),
            source: if db.profile.is_archery() {
                "archery"
            } else {
                "direct"
            },
            writable: db.profile.ensure_write().is_ok(),
            gate: gate.clone(),
            execution: None,
            touched: Instant::now(),
        };
        let mut response = summary(id, &draft);
        if serde_json::to_vec(&draft.plan)?.len() + 4096 <= settings.limits.response_bytes {
            response["plan"] = serialize(&*draft.plan)?;
        }
        self.active(owner)?;
        {
            let mut state = self.state.lock().unwrap();
            state.editing.coordination.stale_plans.remove(&id);
            state.editing.schemas.insert(id, draft);
        }
        self.emit_editing(owner, "edit_plan.changed", id, revision, "ready");
        Ok(response)
    }

    async fn preview_schema(&self, owner: Uuid, params: &Value) -> Result<Value> {
        let id = model::param(params, "plan_id")?;
        let revision: u64 = model::param(params, "revision")?;
        self.editing_plan_current(id)?;
        let gate = self.schema_gate(owner, id)?;
        let _guard = gate.lock().await;
        let settings = self.store.settings().await?;
        let (mut response, plan, profile_id, profile_revision) = {
            let mut state = self.state.lock().unwrap();
            let draft = state
                .editing
                .schemas
                .get_mut(&id)
                .ok_or_else(|| AppError::new("handle_expired", "结构计划已失效"))?;
            if draft.revision != revision {
                return Err(AppError::new("stale_revision", "预览版本已变化"));
            }
            if draft.touched.elapsed().as_secs() >= settings.limits.result_idle_seconds {
                return Err(AppError::new(
                    "handle_expired",
                    "结构预览已过期，请重新生成计划",
                ));
            }
            draft.touched = Instant::now();
            (
                summary(id, draft),
                draft.plan.clone(),
                draft.profile_id,
                draft.profile_revision,
            )
        };
        if self.store.profile(profile_id).await?.revision != profile_revision {
            return Err(AppError::new("schema_conflict", "连接设置已变化"));
        }
        let offset = optional::<usize>(params, "offset")?.unwrap_or(0);
        if offset > plan.steps.len() {
            return Err(AppError::invalid("预览偏移超出计划"));
        }
        let mut steps = Vec::new();
        let mut end = offset;
        while end < plan.steps.len() && steps.len() < settings.editing.preview_rows {
            steps.push(serialize(&plan.steps[end])?);
            response["steps"] = Value::Array(steps.clone());
            if serde_json::to_vec(&response)?.len() + 1024 > settings.limits.response_bytes {
                steps.pop();
                break;
            }
            end += 1;
        }
        if end == offset && offset < plan.steps.len() {
            return Err(AppError::new("resource_limit", "单步预览超过响应预算"));
        }
        response["steps"] = Value::Array(steps);
        response["offset"] = json!(offset);
        response["next"] = json!((end < plan.steps.len()).then_some(end));
        Ok(response)
    }

    async fn submit_schema(self: &Arc<Self>, owner: Uuid, params: Value) -> Result<Value> {
        let id = model::param(&params, "plan_id")?;
        let revision: u64 = model::param(&params, "revision")?;
        let digest: String = model::param(&params, "digest")?;
        self.editing_plan_current(id)?;
        let gate = self.schema_gate(owner, id)?;
        let _guard = gate.lock().await;
        let (session_id, plan, workspace_id, profile_id, profile_revision, execution) = {
            let state = self.state.lock().unwrap();
            let draft = state
                .editing
                .schemas
                .get(&id)
                .ok_or_else(|| AppError::new("handle_expired", "结构计划已失效"))?;
            if draft.revision != revision || draft.plan.plan_digest != digest {
                return Err(AppError::new("stale_revision", "提交内容与预览不一致"));
            }
            (
                draft.session_id,
                draft.plan.clone(),
                draft.workspace.clone(),
                draft.profile_id,
                draft.profile_revision,
                draft.execution,
            )
        };
        if let Some(execution) = execution {
            return Ok(json!({"task_id":execution,"execution_id":execution}));
        }
        let db = self.editing_profile(owner, session_id).await?;
        if db.profile.id != profile_id || db.profile.revision != profile_revision {
            return Err(AppError::new("schema_conflict", "计划所属连接已变化"));
        }
        db.profile.ensure_write()?;
        let work_handle = Uuid::new_v4();
        // 执行参数、provider 反序列化与计划复核同时存活，不能只计长期保存的计划。
        let bytes = serde_json::to_vec(&*plan)?.len();
        let work_bytes = bytes
            .saturating_mul(6)
            .saturating_add(self.store.settings().await?.limits.response_bytes);
        self.store
            .reserve_auxiliary_for_owner(owner, work_handle, work_bytes)
            .await?;
        let response = match self.start(owner, "schema.execute", json!({"session_id":session_id,"workspace_id":workspace_id,"target":plan.intent.target,"plan":&*plan,"plan_id":id,"_editing_work_budget":work_handle}), true).await {
            Ok(response) => response,
            Err(error) => { self.store.release_auxiliary(work_handle).await?; return Err(error); }
        };
        let execution: Uuid = model::param(&response, "task_id")?;
        if let Some(draft) = self.state.lock().unwrap().editing.schemas.get_mut(&id) {
            draft.execution = Some(execution);
        }
        self.emit_editing(owner, "edit_plan.changed", id, revision, "submitted");
        Ok(json!({"task_id":execution,"execution_id":execution}))
    }

    async fn discard_schema(&self, owner: Uuid, params: &Value) -> Result<Value> {
        let id = model::param(params, "plan_id")?;
        let revision: u64 = model::param(params, "revision")?;
        let gate = self.schema_gate(owner, id)?;
        let _guard = gate.lock().await;
        {
            let mut state = self.state.lock().unwrap();
            let draft = state
                .editing
                .schemas
                .get(&id)
                .ok_or_else(|| AppError::new("handle_expired", "结构计划已失效"))?;
            if draft.revision != revision {
                return Err(AppError::new("stale_revision", "草稿版本已变化"));
            }
            if draft
                .execution
                .is_some_and(|task| state.tasks.contains_key(&task))
            {
                return Err(AppError::new("busy", "结构计划仍在执行，请先等待或取消"));
            }
            state.editing.schemas.remove(&id);
        }
        self.store.release_auxiliary(id).await?;
        self.emit_editing(owner, "edit_plan.changed", id, revision, "discarded");
        Ok(json!({"discarded":true}))
    }
}

fn archery_snapshot(target: model::ResourceTarget, raw: Value) -> Result<Value> {
    use datapier_core::model::schema::*;
    use sha2::{Digest, Sha256};
    let old: model::TableSchema = serde_json::from_value(raw)?;
    let columns = old
        .columns
        .into_iter()
        .map(|column| ColumnDefinition {
            id: column.name.clone(),
            name: column.name,
            native_type: column.type_name,
            nullable: column.nullable,
            default: column
                .default_value
                .map(ColumnDefault::Literal)
                .unwrap_or(ColumnDefault::Unspecified),
            on_update: None,
            auto_increment: column.auto_increment,
            generated: None,
            charset: None,
            collation: None,
            visible: true,
            comment: column.comment.unwrap_or_default(),
            extra: Vec::new(),
        })
        .collect();
    let snapshot = SchemaSnapshot {
        target, server_version:String::new(), sql_mode:String::new(),
        definition:SchemaDefinition::Table(TableDefinition { columns,indexes:Vec::new(),constraints:Vec::new(),engine:old.engine,charset:None,collation:None,comment:String::new(),options:Vec::new() }),
        fingerprint:format!("{:x}", Sha256::digest(old.ddl.as_bytes())), ddl:old.ddl,
        dependencies:Vec::new(), completeness:Completeness { state:CompletenessState::Incomplete,reasons:vec!["Archery 未提供可证明完整的结构上下文；可复制原 DDL 或独立草拟 SQL，不能据此重建现有对象".into()] },
        capabilities:Default::default(),
    };
    serialize(snapshot)
}
