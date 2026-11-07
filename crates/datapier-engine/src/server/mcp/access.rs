//! 访问范围证明与发送前复核；人工批准不改变授权范围。
use super::*;

fn canonical(name: &str, info: &Value) -> String {
    if info
        .get("lower_case_table_names")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        == 0
    {
        name.into()
    } else {
        name.to_lowercase()
    }
}
fn sql_context(info: &Value, database: Option<&str>) -> SqlContext {
    let modes = info.get("sql_mode").and_then(Value::as_str).unwrap_or("");
    let has = |name: &str| modes.split(',').any(|mode| mode.eq_ignore_ascii_case(name));
    SqlContext {
        database: database.map(str::to_owned).or_else(|| {
            info.get("database")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }),
        no_backslash_escapes: has("NO_BACKSLASH_ESCAPES"),
        ansi_quotes: has("ANSI_QUOTES"),
        ignore_space: has("IGNORE_SPACE"),
        lower_case_table_names: info
            .get("lower_case_table_names")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u8,
    }
}
impl Engine {
    pub(super) async fn mcp_profile(&self, owner: Uuid, id: Uuid) -> Result<Profile> {
        let context = self.mcp_context(owner)?;
        if !context.allow_connections.is_empty() && !context.allow_connections.contains(&id) {
            return Err(denied("连接不在当前 MCP 启动白名单内"));
        }
        let profile = self.store.profile(id).await?;
        if profile.provider == ProviderKind::Mysql {
            let selected = match &profile.connection {
                model::ConnectionConfig::Direct(direct) => direct.visible_databases.as_ref(),
                model::ConnectionConfig::Archery(connection) => Some(&connection.databases),
            };
            if selected.is_some_and(Vec::is_empty) {
                return Err(denied("连接没有可见数据库"));
            }
        }
        let settings = self.store.settings().await?;
        if settings.mcp.fine_grained_access {
            let rules = self.store.mcp_rules(id).await?;
            if !rules.iter().any(|rule| {
                rule.level != McpAccessLevel::Disabled
                    && match profile.provider {
                        ProviderKind::Mysql => {
                            rule.scope_kind == "database"
                                && match &profile.connection {
                                    model::ConnectionConfig::Direct(direct) => direct
                                        .visible_databases
                                        .as_ref()
                                        .is_none_or(|dbs| dbs.contains(&rule.scope_key)),
                                    model::ConnectionConfig::Archery(connection) => {
                                        connection.databases.contains(&rule.scope_key)
                                    }
                                }
                        }
                        ProviderKind::Redis => {
                            rule.scope_kind == "instance" && rule.scope_key == "*"
                        }
                    }
            }) {
                return Err(denied("连接未授予 Agent 访问权限"));
            }
        }
        Ok(profile)
    }
    pub(super) async fn mcp_allow(
        &self,
        owner: Uuid,
        profile: &Profile,
        database: Option<&str>,
        write: bool,
    ) -> Result<()> {
        self.mcp_allow_mode(owner, profile, database, write, false)
            .await
    }
    async fn mcp_allow_mode(
        &self,
        owner: Uuid,
        profile: &Profile,
        database: Option<&str>,
        write: bool,
        fold: bool,
    ) -> Result<()> {
        let equal = |left: &str, right: &str| {
            if fold {
                left.chars()
                    .flat_map(char::to_lowercase)
                    .eq(right.chars().flat_map(char::to_lowercase))
            } else {
                left == right
            }
        };
        // profile 参数来自已冻结请求也必须用当前配置重新求交集。
        let current = self.mcp_profile(owner, profile.id).await?;
        if identity(&current) != identity(profile) {
            return Err(denied("连接身份已变化"));
        }
        if write {
            current.ensure_write()?;
        }
        let scope = match current.provider {
            ProviderKind::Mysql => {
                let db = database.ok_or_else(|| AppError::invalid("请选择数据库"))?;
                if ["information_schema", "performance_schema", "mysql", "sys"]
                    .iter()
                    .any(|name| db.eq_ignore_ascii_case(name))
                {
                    return Err(denied("系统目录请通过结构化资源工具访问"));
                }
                let visible = match &current.connection {
                    model::ConnectionConfig::Direct(direct) => direct.visible_databases.as_ref(),
                    model::ConnectionConfig::Archery(connection) => Some(&connection.databases),
                };
                if visible.is_some_and(|names| !names.iter().any(|name| equal(name, db))) {
                    return Err(denied("数据库不在保存的可见范围内"));
                }
                ("database", db)
            }
            ProviderKind::Redis => {
                if let Some(connection) = current.archery()
                    && database.is_none_or(|db| !connection.databases.iter().any(|name| name == db))
                {
                    return Err(denied("Redis DB 不在平台连接选择范围内"));
                }
                ("instance", "*")
            }
        };
        let settings = self.store.settings().await?;
        if settings.mcp.fine_grained_access {
            let level = self
                .store
                .mcp_rules(current.id)
                .await?
                .into_iter()
                .find(|rule| rule.scope_kind == scope.0 && equal(&rule.scope_key, scope.1))
                .map(|rule| rule.level)
                .unwrap_or(McpAccessLevel::Disabled);
            if level == McpAccessLevel::Disabled || (write && level != McpAccessLevel::ReadWrite) {
                return Err(denied("Agent 对目标没有所需权限"));
            }
        }
        Ok(())
    }
    pub(super) async fn mcp_validate_proof(
        &self,
        owner: Uuid,
        proof: &AccessProof,
        frozen: bool,
    ) -> Result<Profile> {
        if proof.version != 1 {
            return Err(denied("访问范围证明版本不可用"));
        }
        let profile = self.mcp_profile(owner, proof.profile_id).await?;
        if identity(&profile) != proof.identity {
            return Err(denied("执行目标身份已变化"));
        }
        if frozen
            && (profile.revision != proof.profile_revision
                || self.store.settings().await?.revision != proof.policy_revision)
        {
            return Err(AppError::new(
                "invalidated",
                "配置或策略已变化，请重新发起命令",
            ));
        }
        let fold = proof
            .context
            .get("lower_case_table_names")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            != 0;
        for db in &proof.read_databases {
            self.mcp_allow_mode(owner, &profile, Some(db), false, fold)
                .await?;
        }
        for db in &proof.write_databases {
            self.mcp_allow_mode(owner, &profile, Some(db), true, fold)
                .await?;
        }
        if proof.may_write && proof.write_databases.is_empty() {
            profile.ensure_write()?;
        }
        Ok(profile)
    }
    pub(in crate::server) async fn mcp_observation_allowed(
        &self,
        task: &model::TaskRecord,
    ) -> Result<bool> {
        let Some(raw) = self.store.execution_proof(task.id).await? else {
            return Ok(false);
        };
        let Ok(proof) = serde_json::from_value::<AccessProof>(raw) else {
            return Ok(false);
        };
        Ok(self
            .mcp_validate_proof(task.owner, &proof, false)
            .await
            .is_ok())
    }
    pub(super) async fn mcp_info(&self, db: &Arc<Database>) -> Result<Value> {
        let mut session = db
            .session
            .try_lock()
            .map_err(|_| AppError::new("busy", "原会话有未完成执行或读取"))?;
        session
            .call(
                "connection.info",
                json!({}),
                &ExecutionSink::new(self.store.clone(), Uuid::nil()),
                &CancellationToken::new(),
            )
            .await
    }
    pub(super) async fn mcp_refresh_session(&self, owner: Uuid, session_id: Uuid) -> Result<()> {
        let pending = self
            .state
            .lock()
            .unwrap()
            .mcp
            .sessions
            .get(&session_id)
            .filter(|session| session.owner == owner)
            .and_then(|session| session.pending);
        if let Some(id) = pending {
            let mut task = match self.task(id).await {
                Ok(task) => task,
                Err(error) if error.code == "not_found" => {
                    // 自动清理只移除终态历史；旧引用不能阻塞仍存活的数据库会话。
                    if let Some(session) =
                        self.state.lock().unwrap().mcp.sessions.get_mut(&session_id)
                        && session.pending == Some(id)
                    {
                        session.pending = None;
                    }
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            if task.state == "approval_required" && self.mcp_expire_task(&task).await? {
                task = self.task(id).await?;
            }
            if matches!(
                task.state.as_str(),
                "queued" | "running" | "cancelling" | "approval_required"
            ) {
                return Err(AppError::new("busy", "会话已有执行或待审批请求"));
            }
            if let Some(session) = self.state.lock().unwrap().mcp.sessions.get_mut(&session_id)
                && session.pending == Some(id)
            {
                session.pending = None;
            }
        }
        Ok(())
    }
    pub(super) fn mcp_freeze_target_database(
        db: &Database,
        params: &mut Value,
        proof: &AccessProof,
    ) -> Result<()> {
        let database = params
            .pointer("/target/database")
            .and_then(Value::as_str)
            .or_else(|| params.get("database").and_then(Value::as_str))
            .or_else(|| proof.context.get("database").and_then(Value::as_str))
            .or(db.database.as_deref())
            .ok_or_else(|| AppError::invalid("请选择目标数据库"))?
            .to_owned();
        let target = params
            .get_mut("target")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| AppError::invalid("缺少目标资源"))?;
        target.insert("database".into(), json!(database));
        Ok(())
    }

    pub(super) async fn mcp_make_proof(
        &self,
        owner: Uuid,
        db: &Arc<Database>,
        session_id: Uuid,
        method: &str,
        params: &Value,
    ) -> Result<AccessProof> {
        let mut session = db
            .session
            .try_lock()
            .map_err(|_| AppError::new("busy", "原会话正忙"))?;
        self.mcp_prove(owner, db, session_id, method, params, &mut session)
            .await
    }
    async fn mcp_prove(
        &self,
        owner: Uuid,
        db: &Arc<Database>,
        session_id: Uuid,
        method: &str,
        params: &Value,
        session: &mut ProviderSession,
    ) -> Result<AccessProof> {
        let profile = self.mcp_profile(owner, db.profile.id).await?;
        if identity(&profile) != identity(&db.profile) {
            return Err(denied("现有会话的连接身份已过期"));
        }
        let settings = self.store.settings().await?;
        let sink = ExecutionSink::new(self.store.clone(), Uuid::nil());
        let cancel = CancellationToken::new();
        let info = session
            .call("connection.info", json!({}), &sink, &cancel)
            .await?;
        let generation = {
            let mut state = self.state.lock().unwrap();
            let entry = state
                .mcp
                .sessions
                .entry(session_id)
                .or_insert(SessionState {
                    owner,
                    generation: 0,
                    pending: None,
                    queued: None,
                });
            if entry.owner != owner {
                return Err(denied("会话归属已变化"));
            }
            if info.get("transaction") == Some(&json!(false)) {
                entry.queued = None;
            }
            entry.generation
        };
        let mut proof = AccessProof {
            version: 1,
            profile_id: profile.id,
            session_id,
            identity: identity(&profile),
            profile_revision: profile.revision,
            policy_revision: settings.revision,
            generation,
            // 探测到的 capability 会随元数据请求变化，不属于会话／授权状态。
            context: json!({"database":info["database"],"sql_mode":info["sql_mode"],"lower_case_table_names":info["lower_case_table_names"],"transaction":info["transaction"],"server_version":info["server_version"],"source":info["source"],"query_semantics":info["query_semantics"]}),
            read_databases: BTreeSet::new(),
            write_databases: BTreeSet::new(),
            objects: Vec::new(),
            definitions: Vec::new(),
            dangerous: false,
            transaction_actions: Vec::new(),
            may_write: false,
            reason: String::new(),
        };
        let target_database = params
            .pointer("/target/database")
            .and_then(Value::as_str)
            .or_else(|| params.get("database").and_then(Value::as_str))
            .or_else(|| info.get("database").and_then(Value::as_str))
            .or(db.database.as_deref());
        if profile.provider == ProviderKind::Redis {
            let current = info
                .get("database")
                .and_then(Value::as_u64)
                .map(|db| db.to_string())
                .or_else(|| target_database.map(str::to_owned));
            let current = current
                .ok_or_else(|| AppError::new("connection_closed", "Redis 命令会话不可用"))?;
            proof.read_databases.insert(current.clone());
            if matches!(method, "execution.start" | "execution.run") {
                let text: String = model::param(params, "text")?;
                let analyzed = datapier_core::providers::redis::analyze_command(&text)?;
                proof.may_write = analyzed["may_write"].as_bool().unwrap_or(true);
                proof.dangerous = analyzed["dangerous"].as_bool().unwrap_or(true);
                if proof.may_write {
                    proof.write_databases.insert(current);
                }
                if let Some(databases) = analyzed["databases"].as_array() {
                    for database in databases {
                        let value = database
                            .as_u64()
                            .ok_or_else(|| denied("无法证明 Redis DB"))?
                            .to_string();
                        proof.read_databases.insert(value.clone());
                        if proof.may_write {
                            proof.write_databases.insert(value);
                        }
                    }
                }
                if analyzed["commands"].as_array().is_some_and(|commands| {
                    commands.iter().any(|command| command["name"] == "EXEC")
                }) {
                    let queued = self
                        .state
                        .lock()
                        .unwrap()
                        .mcp
                        .sessions
                        .get(&session_id)
                        .and_then(|entry| entry.queued.clone());
                    if let Some(queued) = queued {
                        proof.read_databases.extend(queued.read_databases);
                        proof.write_databases.extend(queued.write_databases);
                        proof.dangerous |= queued.dangerous;
                    }
                }
            }
        } else {
            let context = sql_context(
                &info,
                if profile.is_archery() {
                    target_database
                } else {
                    None
                },
            );
            if matches!(
                method,
                "execution.start" | "execution.run" | "archery.sql_check"
            ) {
                let text: String = model::param(params, "text")?;
                let access = analyze_mysql(&text, &context)?;
                proof.read_databases = access.read_databases;
                proof.write_databases = access.write_databases;
                proof.objects = access.objects;
                proof.dangerous = access.dangerous;
                proof.transaction_actions = access.transaction_actions;
                if proof
                    .transaction_actions
                    .iter()
                    .any(|action| matches!(action.as_str(), "commit" | "implicit_commit"))
                    && let Some(queued) = self
                        .state
                        .lock()
                        .unwrap()
                        .mcp
                        .sessions
                        .get(&session_id)
                        .and_then(|entry| entry.queued.as_ref())
                {
                    proof
                        .read_databases
                        .extend(queued.read_databases.iter().cloned());
                    proof
                        .write_databases
                        .extend(queued.write_databases.iter().cloned());
                    proof.dangerous |= queued.dangerous;
                }
                if let Some(database) = access.change_database {
                    proof.read_databases.insert(database);
                }
                proof.may_write = !proof.write_databases.is_empty();
            } else if let Some(database) = target_database {
                proof.read_databases.insert(canonical(database, &info));
                if let Some(name) = params.pointer("/target/name").and_then(Value::as_str) {
                    proof.objects.push(ObjectAccess {
                        database: canonical(database, &info),
                        name: canonical(name, &info),
                        write: false,
                    });
                }
            } else if !matches!(method, "connection.info" | "database.list") {
                return Err(AppError::invalid("请选择目标数据库"));
            }
            if method == "archery.sql_check" {
                proof.read_databases.append(&mut proof.write_databases);
                proof
                    .objects
                    .iter_mut()
                    .for_each(|object| object.write = false);
                proof.may_write = false;
                proof.dangerous = false;
            } else {
                self.mcp_dependencies(owner, &profile, session, &context, &mut proof)
                    .await?;
            }
        }
        proof.reason = if proof.dangerous {
            "命令包含结构变更、无范围写入或高风险管理操作"
        } else if proof.may_write {
            "命令会写入目标数据"
        } else {
            "只读操作"
        }
        .into();
        self.mcp_validate_proof(owner, &proof, true).await?;
        Ok(proof)
    }
    async fn mcp_describe_dependency(
        &self,
        owner: Uuid,
        session: &mut ProviderSession,
        params: Value,
        cancel: &CancellationToken,
    ) -> Result<Value> {
        // 平台元数据同样记录实际发送与完整响应，不能使用不存在的 execution ID。
        let workspace = self.mcp_context(owner)?.workspace;
        let record = self
            .store
            .create_task(
                owner,
                &workspace,
                "mcp.scope.inspect",
                params.clone(),
                false,
            )
            .await?;
        self.store
            .update_task(record.id, "running", Value::Null, None)
            .await?;
        let sink = ExecutionSink::new(self.store.clone(), record.id);
        let result = session
            .call("resource.describe", params, &sink, cancel)
            .await;
        let summary = self.store.task(record.id).await?.summary;
        self.store
            .update_task(
                record.id,
                if result.is_ok() {
                    "succeeded"
                } else {
                    "failed"
                },
                summary,
                result.as_ref().err().cloned(),
            )
            .await?;
        result
    }

    async fn mcp_dependencies(
        &self,
        owner: Uuid,
        profile: &Profile,
        session: &mut ProviderSession,
        context: &SqlContext,
        proof: &mut AccessProof,
    ) -> Result<()> {
        let mut pending: Vec<_> = proof
            .objects
            .iter()
            .cloned()
            .map(|object| (object, Vec::<(String, String, bool)>::new()))
            .collect();
        let mut seen = BTreeSet::new();
        let cancel = CancellationToken::new();
        while let Some((object, mut path)) = pending.pop() {
            self.mcp_allow_mode(
                owner,
                profile,
                Some(&object.database),
                object.write,
                context.lower_case_table_names != 0,
            )
            .await?;
            let key = (object.database.clone(), object.name.clone(), object.write);
            if path.contains(&key) {
                return Err(denied("间接对象依赖存在循环，无法证明范围"));
            }
            if !seen.insert(key.clone()) {
                continue;
            }
            if seen.len() > 4096 || path.len() > 64 {
                return Err(denied("对象依赖超过可证明范围"));
            }
            path.push(key);
            let sink = ExecutionSink::new(self.store.clone(), Uuid::nil());
            let item = if profile.is_archery() {
                // Archery 只读访问不存在写触发器；固定 describe 取得完整对象 DDL 后分析视图。
                let value = self.mcp_describe_dependency(owner, session, json!({"target":{"database":object.database,"name":object.name,"kind":"unknown"}}), &cancel).await?;
                let kind = value
                    .pointer("/target/kind")
                    .and_then(Value::as_str)
                    .ok_or_else(|| denied("平台未提供对象种类"))?;
                let ddl = value["ddl"]
                    .as_str()
                    .ok_or_else(|| denied("平台未提供完整 DDL"))?;
                if !matches!(kind, "table" | "view") {
                    return Err(denied("平台对象种类不可证明"));
                }
                json!({"database":object.database,"name":object.name,"write":false,"kind":kind,"ddl":ddl,"complete":true})
            } else {
                let inspected = session
                    .call(
                        "policy.inspect",
                        json!({"objects":[object]}),
                        &sink,
                        &cancel,
                    )
                    .await?;
                inspected["objects"]
                    .as_array()
                    .and_then(|objects| objects.first())
                    .cloned()
                    .ok_or_else(|| denied("对象元数据不可见"))?
            };
            if item["complete"] != true {
                return Err(denied("对象定义或间接依赖不可见，无法证明访问范围"));
            }
            let mut local_context = context.clone();
            local_context.database = Some(object.database.clone());
            let mut accesses = Vec::<SqlAccess>::new();
            if item["kind"] == "view" {
                if object.write {
                    return Err(denied("无法证明视图的实际写入范围"));
                }
                let access = if profile.is_archery() {
                    datapier_core::policy::analyze_view_ddl(
                        item["ddl"]
                            .as_str()
                            .ok_or_else(|| denied("视图定义不可见"))?,
                        &local_context,
                    )?
                } else {
                    analyze_mysql(
                        item["view_definition"]
                            .as_str()
                            .ok_or_else(|| denied("视图定义不可见"))?,
                        &local_context,
                    )?
                };
                if !access.write_databases.is_empty() {
                    return Err(denied("视图定义包含未知副作用"));
                }
                accesses.push(access);
            }
            if object.write {
                if let Some(triggers) = item["triggers"].as_array() {
                    for trigger in triggers {
                        accesses.push(analyze_mysql(
                            trigger["definition"]
                                .as_str()
                                .ok_or_else(|| denied("触发器定义不可见"))?,
                            &local_context,
                        )?);
                    }
                }
                if let Some(keys) = item["foreign_keys"].as_array() {
                    for key in keys {
                        let mut dependency: ObjectAccess = serde_json::from_value(key.clone())?;
                        if context.lower_case_table_names != 0 {
                            dependency.database = dependency.database.to_lowercase();
                            dependency.name = dependency.name.to_lowercase();
                        }
                        if dependency.write {
                            proof.write_databases.insert(dependency.database.clone());
                        } else {
                            proof.read_databases.insert(dependency.database.clone());
                        }
                        let dependency_key = (
                            dependency.database.clone(),
                            dependency.name.clone(),
                            dependency.write,
                        );
                        // 外键回边已经在当前路径证明；视图和触发器循环仍须拒绝。
                        if !path.contains(&dependency_key) {
                            pending.push((dependency, path.clone()));
                        }
                    }
                }
            }
            for access in accesses {
                proof.read_databases.extend(access.read_databases);
                proof.write_databases.extend(access.write_databases);
                proof.dangerous |= access.dangerous;
                pending.extend(
                    access
                        .objects
                        .into_iter()
                        .map(|dependency| (dependency, path.clone())),
                );
            }
            proof.definitions.push(item);
        }
        proof.may_write |= !proof.write_databases.is_empty();
        Ok(())
    }
    pub(in crate::server) async fn mcp_revalidate_result(
        &self,
        owner: Uuid,
        db: &Arc<Database>,
        execution_id: Uuid,
        session: &mut ProviderSession,
    ) -> Result<()> {
        if self.mcp_context(owner)?.kind != ClientKind::Mcp {
            return Ok(());
        }
        let raw = self
            .store
            .execution_proof(execution_id)
            .await?
            .ok_or_else(|| denied("结果缺少访问证明"))?;
        let frozen: AccessProof =
            serde_json::from_value(raw).map_err(|_| denied("结果访问证明无效"))?;
        let profile = self.mcp_validate_proof(owner, &frozen, false).await?;
        let proof_db = self.database(owner, &json!({"session_id":frozen.session_id}))?;
        if !Arc::ptr_eq(db, &proof_db) || db.profile.id != frozen.profile_id {
            return Err(denied("结果会话与访问证明不匹配"));
        }
        let mut fresh = frozen.clone();
        fresh.definitions.clear();
        let context = sql_context(&frozen.context, None);
        self.mcp_dependencies(owner, &profile, session, &context, &mut fresh)
            .await
            .map_err(|_| AppError::new("invalidated", "结果对象依赖不可再证明，请重新执行查询"))?;
        if fresh.definitions != frozen.definitions
            || fresh.read_databases != frozen.read_databases
            || fresh.write_databases != frozen.write_databases
        {
            return Err(AppError::new(
                "invalidated",
                "结果对象依赖已变化，请重新执行查询",
            ));
        }
        Ok(())
    }

    pub(in crate::server) async fn mcp_before_send(
        &self,
        owner: Uuid,
        db: &Arc<Database>,
        method: &str,
        params: &Value,
        session: &mut ProviderSession,
    ) -> Result<()> {
        if self.mcp_context(owner)?.kind != ClientKind::Mcp {
            return Ok(());
        }
        let Some(frozen) = params.get("_access_proof") else {
            if matches!(method, "connection.info" | "database.list") {
                self.mcp_profile(owner, db.profile.id).await?;
                return Ok(());
            }
            return Err(denied("执行缺少冻结访问范围证明"));
        };
        let frozen: AccessProof = serde_json::from_value(frozen.clone())?;
        self.mcp_validate_proof(owner, &frozen, true).await?;
        let fresh = self
            .mcp_prove(owner, db, frozen.session_id, method, params, session)
            .await?;
        if fresh.context != frozen.context
            || fresh.generation != frozen.generation
            || fresh.read_databases != frozen.read_databases
            || fresh.write_databases != frozen.write_databases
            || fresh.definitions != frozen.definitions
            || fresh.dangerous != frozen.dangerous
        {
            return Err(AppError::new(
                "invalidated",
                "物理会话或对象依赖已变化，请重新发起命令",
            ));
        }
        let mut state = self.state.lock().unwrap();
        if let Some(entry) = state.mcp.sessions.get_mut(&frozen.session_id) {
            entry.generation = entry
                .generation
                .checked_add(1)
                .ok_or_else(|| AppError::new("resource_limit", "会话代次已用尽"))?;
            if db.profile.provider == ProviderKind::Redis
                && matches!(method, "execution.start" | "execution.run")
            {
                let analyzed = datapier_core::providers::redis::analyze_command(
                    params["text"]
                        .as_str()
                        .ok_or_else(|| AppError::invalid("缺少命令"))?,
                )?;
                for command in analyzed["commands"]
                    .as_array()
                    .ok_or_else(|| denied("无法证明命令队列"))?
                {
                    match command["name"].as_str().unwrap_or("") {
                        "MULTI" => entry.queued = Some(fresh.clone()),
                        "EXEC" | "DISCARD" => entry.queued = None,
                        _ => {
                            if let Some(queued) = &mut entry.queued {
                                queued
                                    .read_databases
                                    .extend(fresh.read_databases.iter().cloned());
                                queued
                                    .write_databases
                                    .extend(fresh.write_databases.iter().cloned());
                                queued.dangerous |= fresh.dangerous;
                            }
                        }
                    }
                }
            } else if db.profile.provider == ProviderKind::Mysql
                && matches!(method, "execution.start" | "execution.run")
            {
                if fresh.context["transaction"] == true
                    && let Some(queued) = &mut entry.queued
                {
                    queued
                        .read_databases
                        .extend(fresh.read_databases.iter().cloned());
                    queued
                        .write_databases
                        .extend(fresh.write_databases.iter().cloned());
                    queued.dangerous |= fresh.dangerous;
                }
                for action in &fresh.transaction_actions {
                    match action.as_str() {
                        "begin" => entry.queued = Some(fresh.clone()),
                        "commit" | "rollback" | "implicit_commit" => entry.queued = None,
                        _ => (),
                    }
                }
            }
        }
        Ok(())
    }
}
