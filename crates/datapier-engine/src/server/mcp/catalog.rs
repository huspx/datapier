use super::*;

struct CatalogScope<'a> {
    owner: Uuid,
    operation: &'a str,
    resource: Uuid,
}
struct HistoryPosition {
    offset: u64,
    skip: u64,
}

impl Engine {
    // 游标同时绑定规范化条件和位置；调用方仍须重复条件并重新授权。
    fn catalog_position(
        &self,
        owner: Uuid,
        op: &str,
        resource: Uuid,
        params: &Value,
        filters: &Value,
    ) -> Result<u64> {
        match self.mcp_cursor(owner, op, resource, params)? {
            Some(cursor) if cursor.params.get("filters") == Some(filters) => Ok(cursor.start),
            Some(_) => Err(denied("游标与请求条件不匹配")),
            None => Ok(0),
        }
    }

    async fn catalog_page(
        &self,
        scope: CatalogScope<'_>,
        filters: Value,
        start: u64,
        items: Vec<Value>,
        key: &str,
        extra: Value,
    ) -> Result<Value> {
        let CatalogScope {
            owner,
            operation: op,
            resource,
        } = scope;
        let limit = self.store.settings().await?.limits.response_bytes;
        let start = usize::try_from(start).map_err(|_| AppError::invalid("游标位置无效"))?;
        if start > items.len() {
            return Err(AppError::new("expired", "目录已变化，请重新读取"));
        }
        let mut out = extra.as_object().cloned().unwrap_or_default();
        out.insert(key.into(), json!([]));
        out.insert("next_cursor".into(), json!(Uuid::nil()));
        out.insert("has_more".into(), json!(true));
        let mut bytes = encoded_len(&out)?.saturating_add(2);
        let mut page = Vec::new();
        let mut index = start;
        let mut more = false;
        for item in items.into_iter().skip(start) {
            let size = encoded_len(&item)?.saturating_add(usize::from(!page.is_empty()));
            if bytes.saturating_add(size) > limit {
                if page.is_empty() {
                    return Err(AppError::new(
                        "response_limit",
                        "单个目录项超过响应字节限制",
                    ));
                }
                more = true;
                break;
            }
            bytes += size;
            page.push(item);
            index += 1;
        }
        let next = more.then(|| {
            self.mcp_next_cursor(
                owner,
                op,
                resource,
                index as u64,
                0,
                json!({"filters":filters}),
            )
        });
        out.insert(key.into(), Value::Array(page));
        out.insert("has_more".into(), json!(more));
        if out.contains_key("complete") {
            out.insert("complete".into(), json!(!more));
        }
        out.insert("next_cursor".into(), json!(next));
        Ok(Value::Object(out))
    }

    pub(super) async fn mcp_connections(&self, owner: Uuid, params: &Value) -> Result<Value> {
        let provider = optional::<String>(params, "provider")?;
        let source = optional::<String>(params, "source")?;
        let filters = json!({"provider":provider,"source":source});
        let start = self.catalog_position(owner, "connections", Uuid::nil(), params, &filters)?;
        let mut items = Vec::new();
        for profile in self.store.profiles().await? {
            let source_name = if profile.is_archery() {
                "archery"
            } else {
                "direct"
            };
            let provider_name = match profile.provider {
                ProviderKind::Mysql => "mysql",
                ProviderKind::Redis => "redis",
            };
            if provider.as_deref().is_some_and(|v| v != provider_name)
                || source.as_deref().is_some_and(|v| v != source_name)
                || self.mcp_profile(owner, profile.id).await.is_err()
            {
                continue;
            }
            let writable = if profile.provider == ProviderKind::Redis {
                self.mcp_allow(owner, &profile, profile.default_database(), true)
                    .await
                    .is_ok()
            } else {
                let databases = match &profile.connection {
                    model::ConnectionConfig::Direct(d) => match &d.visible_databases {
                        Some(databases) => databases.clone(),
                        None => self
                            .store
                            .mcp_rules(profile.id)
                            .await?
                            .into_iter()
                            .filter(|rule| rule.scope_kind == "database")
                            .map(|rule| rule.scope_key)
                            .chain(d.database.clone())
                            .collect(),
                    },
                    model::ConnectionConfig::Archery(a) => a.databases.clone(),
                };
                let mut any = matches!(&profile.connection, model::ConnectionConfig::Direct(d) if d.visible_databases.is_none())
                    && !self.store.settings().await?.mcp.fine_grained_access
                    && profile.ensure_write().is_ok();
                for db in databases {
                    if self
                        .mcp_allow(owner, &profile, Some(&db), true)
                        .await
                        .is_ok()
                    {
                        any = true;
                        break;
                    }
                }
                any
            };
            let auth_state = match profile.archery() {
                Some(connection) => {
                    self.mcp_archery_status(owner, connection.server_id)
                        .await?
                        .0
                }
                None => "ready".to_owned(),
            };
            items.push(json!({"connection_id":profile.id,"name":profile.name,"provider":profile.provider,"source":source_name,"access":if writable {"read_write"} else {"read_only"},"auth_state":auth_state}));
        }
        self.catalog_page(
            CatalogScope {
                owner,
                operation: "connections",
                resource: Uuid::nil(),
            },
            filters,
            start,
            items,
            "connections",
            json!({}),
        )
        .await
    }

    pub(super) async fn mcp_databases(
        self: &Arc<Self>,
        owner: Uuid,
        params: &Value,
    ) -> Result<Value> {
        let id: Uuid = model::param(params, "connection_id")?;
        let profile = self.mcp_profile(owner, id).await?;
        let refresh = optional::<bool>(params, "refresh")?.unwrap_or(false);
        let filters = json!({"refresh":refresh});
        let start = self.catalog_position(owner, "databases", id, params, &filters)?;
        if refresh && profile.is_archery() {
            let auth = self.mcp_auth(owner, &json!({"connection_id":id})).await?;
            if auth["state"] != "ready" {
                return Ok(auth);
            }
        }
        let discovery = self
            .dispatch_internal(
                owner,
                "database.list",
                json!({"profile_id":id,"refresh":refresh}),
            )
            .await?;
        let mut items = Vec::new();
        for name in discovery["databases"]
            .as_array()
            .ok_or_else(|| AppError::new("internal", "数据库发现结果无效"))?
        {
            let Some(name) = name.as_str() else {
                return Err(AppError::new("internal", "数据库发现结果无效"));
            };
            if self
                .mcp_allow(owner, &profile, Some(name), false)
                .await
                .is_ok()
            {
                items.push(json!(name));
            }
        }
        self.catalog_page(CatalogScope { owner, operation: "databases", resource: id }, filters, start, items, "databases", json!({"state":discovery["state"],"source":discovery["source"],"configured_count":discovery["configured_count"],"reason":discovery["reason"]})).await
    }

    pub(super) async fn mcp_open(self: &Arc<Self>, owner: Uuid, params: &Value) -> Result<Value> {
        let fields = params
            .as_object()
            .ok_or_else(|| AppError::invalid("参数必须为对象"))?;
        if fields
            .keys()
            .any(|k| k != "connection_id" && k != "database")
        {
            return Err(AppError::invalid(
                "open_session 只接受 connection_id 和 database",
            ));
        }
        let id: Uuid = model::param(params, "connection_id")?;
        let profile = self.mcp_profile(owner, id).await?;
        let requested = optional::<String>(params, "database")?;
        let database = requested.or_else(|| {
            if profile.provider == ProviderKind::Redis {
                Some(profile.redis_db().to_string())
            } else {
                profile.default_database().map(str::to_owned)
            }
        });
        self.mcp_allow(owner, &profile, database.as_deref(), false)
            .await?;
        if profile.is_archery() {
            let auth = self.mcp_auth(owner, &json!({"connection_id":id})).await?;
            if auth["state"] != "ready" {
                return Ok(auth);
            }
        }
        let mut request = json!({"profile_id":id});
        if let Some(database) = &database {
            if profile.provider == ProviderKind::Redis {
                request["redis_db"] = json!(
                    database
                        .parse::<u32>()
                        .map_err(|_| AppError::invalid("Redis DB 无效"))?
                );
            } else {
                request["database"] = json!(database);
            }
        }
        let opened = self
            .dispatch_internal(owner, "connection.open", request)
            .await?;
        let session_id: Uuid = model::param(&opened, "session_id")?;
        self.state.lock().unwrap().mcp.sessions.insert(
            session_id,
            SessionState {
                owner,
                generation: 0,
                pending: None,
                queued: None,
            },
        );
        Ok(
            json!({"session_id":session_id,"connection_id":id,"database":opened["database"],"redis_db":opened["redis_db"],"source":opened["source"],"capabilities":opened["capabilities"],"query_semantics":opened["query_semantics"]}),
        )
    }

    pub(super) async fn mcp_session(&self, owner: Uuid, params: &Value) -> Result<Value> {
        let id: Uuid = model::param(params, "session_id")?;
        let db = self.database(owner, params)?;
        let profile = self.mcp_profile(owner, db.profile.id).await?;
        if let Err(error) = self.mcp_refresh_session(owner, id).await {
            if error.code != "busy" {
                return Err(error);
            }
            let pending = self
                .state
                .lock()
                .unwrap()
                .mcp
                .sessions
                .get(&id)
                .filter(|s| s.owner == owner)
                .ok_or_else(|| denied("会话不属于当前客户端"))?
                .pending;
            return Ok(
                json!({"session_id":id,"connection_id":profile.id,"state":"busy","execution_id":pending,"database":null,"transaction":null}),
            );
        }
        let info = self.mcp_info(&db).await?;
        let database = info
            .get("database")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| {
                info.get("database")
                    .and_then(Value::as_u64)
                    .map(|n| n.to_string())
            })
            .or_else(|| db.database.clone());
        self.mcp_allow(owner, &profile, database.as_deref(), false)
            .await?;
        let pending = self
            .state
            .lock()
            .unwrap()
            .mcp
            .sessions
            .get(&id)
            .filter(|s| s.owner == owner)
            .ok_or_else(|| denied("会话不属于当前客户端"))?
            .pending;
        let capabilities = db.capabilities.lock().unwrap().clone();
        Ok(
            json!({"session_id":id,"connection_id":profile.id,"database":database,"transaction":info["transaction"],"state":if pending.is_some(){"busy"}else{"ready"},"execution_id":pending,"capabilities":capabilities}),
        )
    }

    pub(super) async fn mcp_close(self: &Arc<Self>, owner: Uuid, params: &Value) -> Result<Value> {
        let id: Uuid = model::param(params, "session_id")?;
        self.database(owner, params)?;
        if !self
            .state
            .lock()
            .unwrap()
            .mcp
            .sessions
            .get(&id)
            .is_some_and(|s| s.owner == owner)
        {
            return Err(denied("会话不属于当前客户端"));
        }
        let result = self
            .dispatch_internal(owner, "connection.close", json!({"session_id":id}))
            .await;
        let mut state = self.state.lock().unwrap();
        if !state.databases.contains_key(&id) {
            state.mcp.sessions.remove(&id);
            state.mcp.gates.remove(&id);
        }
        result
    }

    pub(super) async fn mcp_auth(self: &Arc<Self>, owner: Uuid, params: &Value) -> Result<Value> {
        let id: Uuid = model::param(params, "connection_id")?;
        let previous = self.mcp_auth_request_id(owner, id);
        let profile = self.mcp_profile(owner, id).await?;
        let Some(connection) = profile.archery() else {
            self.mcp_record_auth(owner, &profile, previous, "ready")
                .await?;
            return Ok(json!({"connection_id":id,"state":"ready","interactive":false}));
        };
        let server = self.store.archery_server(connection.server_id).await?;
        let (mut state, generation) = self.mcp_archery_status(owner, connection.server_id).await?;
        if state != "ready" {
            let attempt = self.state.lock().unwrap().mcp.auth_attempts.insert((
                owner,
                connection.server_id,
                server.revision,
                generation,
            ));
            if attempt {
                let _ = self.dispatch_internal(owner, "archery.auth.begin", json!({"server_id":connection.server_id,"expected_revision":server.revision,"operation_id":Uuid::new_v4()})).await;
                let (current, resulting_generation) =
                    self.mcp_archery_status(owner, connection.server_id).await?;
                state = current;
                if state != "ready" {
                    self.state.lock().unwrap().mcp.auth_attempts.insert((
                        owner,
                        connection.server_id,
                        server.revision,
                        resulting_generation,
                    ));
                    self.mcp_archery_release_controller(owner, connection.server_id);
                }
            }
        }
        self.mcp_record_auth(owner, &profile, previous, &state)
            .await?;
        Ok(
            json!({"connection_id":id,"state":state,"interactive":!self.mcp_online().is_empty(),"instruction":if state == "ready" {""} else {"请在使用同一数据目录的 TUI 中完成 Archery 密码或验证码认证"}}),
        )
    }

    pub(super) async fn mcp_resources(
        self: &Arc<Self>,
        owner: Uuid,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        let session_id: Uuid = model::param(&params, "session_id")?;
        let db = self.database(owner, &params)?;
        self.mcp_refresh_session(owner, session_id).await?;
        if !self
            .state
            .lock()
            .unwrap()
            .mcp
            .sessions
            .get(&session_id)
            .is_some_and(|s| s.owner == owner)
        {
            return Err(denied("会话不属于当前客户端"));
        }
        let list = method == "mcp.list_resources";
        let internal = if list {
            "resource.list"
        } else {
            "resource.describe"
        };
        let mut request =
            json!({"session_id":session_id,"workspace_id":self.mcp_context(owner)?.workspace});
        if list {
            for key in ["database", "pattern"] {
                if let Some(value) = params.get(key) {
                    request[key] = value.clone();
                }
            }
            if let Some(kind) = params.get("kind") {
                request["type"] = kind.clone();
            }
        } else {
            let target = params
                .get("target")
                .filter(|v| v.is_object())
                .ok_or_else(|| AppError::invalid("缺少目标资源"))?;
            if target
                .get("name")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
                && target.get("key_b64").and_then(Value::as_str).is_none()
            {
                return Err(AppError::invalid("目标资源名称不能为空"));
            }
            request["target"] = target.clone();
        }
        if db.profile.provider == ProviderKind::Redis {
            let info = self.mcp_info(&db).await?;
            let actual = info
                .get("browse_database")
                .and_then(Value::as_u64)
                .or_else(|| info.get("database").and_then(Value::as_u64))
                .map(|n| n.to_string())
                .or_else(|| db.database.clone())
                .ok_or_else(|| AppError::new("connection_closed", "Redis 数据库不可用"))?;
            let requested = if list {
                params.get("database")
            } else {
                params.pointer("/target/database")
            };
            if requested
                .and_then(Value::as_str)
                .is_some_and(|name| name != actual)
            {
                return Err(denied("目标数据库与会话不一致"));
            }
            self.mcp_allow(owner, &db.profile, Some(&actual), false)
                .await?;
        }
        let filters = json!({"profile_id":db.profile.id,"database":params.get("database"),"kind":params.get("kind"),"pattern":params.get("pattern")});
        let cursor = if list {
            self.mcp_cursor(owner, "resources", session_id, &params)?
        } else {
            None
        };
        if cursor
            .as_ref()
            .is_some_and(|c| c.params.get("filters") != Some(&filters))
        {
            return Err(denied("游标与请求条件不匹配"));
        }
        if db.profile.provider == ProviderKind::Redis
            && let Some(value) = cursor
                .as_ref()
                .and_then(|c| c.params.get("provider_cursor"))
                .filter(|v| !v.is_null())
        {
            request["cursor"] = value.clone();
        }
        let proof = self
            .mcp_make_proof(owner, &db, session_id, internal, &request)
            .await?;
        if list
            && db.profile.provider == ProviderKind::Mysql
            && request.get("database").is_none_or(Value::is_null)
        {
            let database = proof
                .read_databases
                .iter()
                .next()
                .ok_or_else(|| AppError::invalid("请选择目标数据库"))?;
            request["database"] = json!(database);
        }
        if !list && db.profile.provider == ProviderKind::Mysql {
            Self::mcp_freeze_target_database(&db, &mut request, &proof)?;
        }
        request["_access_proof"] = serialize(&proof)?;
        let mut result = self.dispatch_internal(owner, internal, request).await?;
        self.mcp_validate_proof(owner, &proof, true).await?;
        if !list {
            return Ok(result);
        }
        if db.profile.provider == ProviderKind::Redis {
            let next = result.get("cursor").filter(|v| !v.is_null()).cloned();
            let token = next.map(|next| {
                self.mcp_next_cursor(
                    owner,
                    "resources",
                    session_id,
                    0,
                    0,
                    json!({"filters":filters,"provider_cursor":next}),
                )
            });
            result["cursor"] = json!(token);
            result["next_cursor"] = json!(token);
            result["has_more"] = json!(token.is_some());
            return Ok(result);
        }
        let resources = result
            .get_mut("resources")
            .ok_or_else(|| AppError::new("internal", "目录结果无效"))?
            .take();
        let Value::Array(mut items) = resources else {
            return Err(AppError::new("internal", "目录结果无效"));
        };
        if let Some(kind) = params.get("kind").and_then(Value::as_str) {
            items.retain(|item| item["kind"].as_str() == Some(kind));
        }
        if let Some(pattern) = params.get("pattern").and_then(Value::as_str) {
            items.retain(|item| {
                item["name"]
                    .as_str()
                    .is_some_and(|name| catalog_match(name, pattern))
            });
        }
        self.catalog_page(
            CatalogScope {
                owner,
                operation: "resources",
                resource: session_id,
            },
            filters,
            cursor.map_or(0, |c| c.start),
            items,
            "resources",
            json!({"complete":true}),
        )
        .await
    }

    pub(super) async fn mcp_history(
        self: &Arc<Self>,
        owner: Uuid,
        params: &Value,
    ) -> Result<Value> {
        let id: Uuid = model::param(params, "connection_id")?;
        let profile = self.mcp_profile(owner, id).await?;
        let source: String = model::param(params, "source")?;
        let search = optional::<String>(params, "search")?.unwrap_or_default();
        let (page, size) = paging(params, 20, 1000)?;
        let first = page
            .checked_sub(1)
            .and_then(|n| n.checked_mul(size as u64))
            .ok_or_else(|| AppError::invalid("分页超出范围"))?;
        if source == "local" {
            if params.get("cursor").is_some() {
                return Err(AppError::invalid("本地历史不支持游标"));
            }
            let workspace = self.mcp_context(owner)?.workspace;
            let mut offset = 0u64;
            let mut count = 0u64;
            let mut items = Vec::new();
            loop {
                let candidates = self
                    .store
                    .history_candidates(&workspace, &search, offset, 500)
                    .await?;
                let n = candidates.len();
                for task in candidates {
                    let Some(raw) = self.store.execution_proof(task.id).await? else {
                        continue;
                    };
                    let Ok(proof) = serde_json::from_value::<AccessProof>(raw) else {
                        continue;
                    };
                    if proof.profile_id != id
                        || self.mcp_validate_proof(owner, &proof, false).await.is_err()
                    {
                        continue;
                    }
                    if count >= first && items.len() < size {
                        let detail = self.store.task_details(task.id).await?;
                        items.push(json!({"id":detail.id,"connection_id":id,"text":detail.text,"text_truncated":detail.execution_text_truncated,"state":detail.state,"created_at":detail.created_at,"started_at":detail.started_at,"elapsed_ms":detail.elapsed_ms,"may_write":detail.may_write,"affected_rows":detail.summary.get("affected_rows")}));
                    }
                    count += 1;
                }
                offset = offset
                    .checked_add(n as u64)
                    .ok_or_else(|| AppError::invalid("历史偏移超出范围"))?;
                if n < 500 {
                    break;
                }
            }
            return Ok(
                json!({"connection_id":id,"source":"local","items":items,"total":count,"page":page,"page_size":size,"has_more":first.saturating_add(items.len() as u64)<count}),
            );
        }
        if source != "platform" {
            return Err(AppError::invalid("历史来源必须为 local 或 platform"));
        }
        if !profile.is_archery() {
            return Err(AppError::new(
                "capability_unavailable",
                "仅 Archery 支持平台历史",
            ));
        }
        let filters = json!({"search":search,"page_size":size});
        let cursor = self.mcp_cursor(owner, "platform_history", id, params)?;
        if cursor.is_some() && params.get("page").is_some() {
            return Err(AppError::invalid("游标与页码不能同时使用"));
        }
        if cursor
            .as_ref()
            .is_some_and(|c| c.params.get("filters") != Some(&filters))
        {
            return Err(denied("游标与请求条件不匹配"));
        }
        let offset = cursor.as_ref().map_or(0, |c| c.start);
        let skip = cursor
            .as_ref()
            .and_then(|c| c.params.get("skip"))
            .and_then(Value::as_u64)
            .unwrap_or(first);
        let opened = self.mcp_open(owner, &json!({"connection_id":id})).await?;
        let Some(session_id) = opened
            .get("session_id")
            .and_then(Value::as_str)
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            return Ok(opened);
        };
        let result = self
            .mcp_platform_history(
                owner,
                &profile,
                session_id,
                &search,
                HistoryPosition { offset, skip },
                size,
            )
            .await;
        let closed = self
            .mcp_close(owner, &json!({"session_id":session_id}))
            .await;
        let (mut value, next_offset, skip, more) = match (result, closed) {
            (Err(error), _) => return Err(error),
            (Ok(_), Err(error)) => return Err(error),
            (Ok(value), Ok(_)) => value,
        };
        let next = more.then(|| {
            self.mcp_next_cursor(
                owner,
                "platform_history",
                id,
                next_offset,
                size,
                json!({"filters":filters,"skip":skip}),
            )
        });
        value["next_cursor"] = json!(next);
        Ok(value)
    }

    async fn mcp_platform_history(
        self: &Arc<Self>,
        owner: Uuid,
        profile: &Profile,
        session_id: Uuid,
        search: &str,
        position: HistoryPosition,
        size: usize,
    ) -> Result<(Value, u64, u64, bool)> {
        let HistoryPosition {
            mut offset,
            mut skip,
        } = position;
        let binding = profile
            .archery()
            .ok_or_else(|| AppError::invalid("连接来源无效"))?;
        let db = self.database(owner, &json!({"session_id":session_id}))?;
        let mut items = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut more = true;
        while more && Instant::now() < deadline && items.len() < size {
            let response = self.dispatch_internal(owner, "archery.query_history", json!({"session_id":session_id,"operation_id":Uuid::new_v4(),"offset":offset,"limit":100,"search":search})).await?;
            let rows = response["rows"]
                .as_array()
                .ok_or_else(|| AppError::new("remote_protocol", "平台历史格式无效"))?;
            for row in rows {
                offset = offset.saturating_add(1);
                let instance = row.get("instance_id").and_then(|value| {
                    value
                        .as_i64()
                        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
                });
                let name = row.get("instance_name").and_then(Value::as_str);
                if let Some(instance) = instance {
                    if instance != binding.instance_id
                        || name.is_some_and(|name| name != binding.instance_name)
                    {
                        continue;
                    }
                } else if name != Some(binding.instance_name.as_str()) {
                    continue;
                }
                let Some(database) = row
                    .get("db_name")
                    .or_else(|| row.get("database"))
                    .and_then(Value::as_str)
                else {
                    continue;
                };
                let Some(text) = ["sqllog", "sql_content", "sql", "text", "full_sql"]
                    .iter()
                    .find_map(|key| row.get(*key).and_then(Value::as_str))
                else {
                    continue;
                };
                if self
                    .mcp_allow(owner, profile, Some(database), false)
                    .await
                    .is_err()
                {
                    continue;
                }
                let request = json!({"database":database,"text":text});
                if self
                    .mcp_make_proof(owner, &db, session_id, "execution.start", &request)
                    .await
                    .is_err()
                {
                    continue;
                }
                if skip > 0 {
                    skip -= 1;
                    continue;
                }
                items.push(json!({"id":row.get("id"),"connection_id":profile.id,"instance_id":binding.instance_id,"database":database,"text":text,"created_at":row.get("created_at")}));
                if items.len() == size {
                    break;
                }
            }
            more = !rows.is_empty()
                && response["total"]
                    .as_u64()
                    .is_none_or(|total| offset < total);
        }
        Ok((
            json!({"connection_id":profile.id,"source":"platform","coverage":"filtered_platform_history","items":items,"total":null,"has_more":more}),
            offset,
            skip,
            more,
        ))
    }
}

fn catalog_match(name: &str, pattern: &str) -> bool {
    // '%' and '*' match any sequence; '_' and '?' match one character.
    let text: Vec<char> = name.chars().collect();
    let pattern: Vec<char> = pattern.chars().collect();
    let (mut i, mut j, mut star, mut mark) = (0, 0, None, 0);
    while i < text.len() {
        if j < pattern.len() && (pattern[j] == text[i] || pattern[j] == '?' || pattern[j] == '_') {
            i += 1;
            j += 1;
        } else if j < pattern.len() && (pattern[j] == '*' || pattern[j] == '%') {
            star = Some(j);
            j += 1;
            mark = i;
        } else if let Some(s) = star {
            mark += 1;
            i = mark;
            j = s + 1;
        } else {
            return false;
        }
    }
    while j < pattern.len() && (pattern[j] == '*' || pattern[j] == '%') {
        j += 1;
    }
    j == pattern.len()
}
