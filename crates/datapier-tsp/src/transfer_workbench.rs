use super::*;

/// 每个标签独立持有准备轮次；输入变化后绝不提交旧计划。
#[derive(Clone, Default)]
pub(super) struct TransferWorkbench {
    pub epoch: u64,
    pub pending: Option<(Uuid, u64, String)>,
    pub plan: Option<Value>,
    pub run: Option<Value>,
    pub items: Option<Value>,
    pub run_offset: u64,
    pub run_pages: Vec<u64>,
    pub run_spec: Option<Value>,
    pending_spec: Option<Value>,
    pub plan_page: Option<Value>,
    pub plan_section: String,
    pub plan_offset: u64,
    pub plan_pages: Vec<u64>,
    pub chunk_offset: u64,
    pub chunk_text: String,
    pub obsolete_plan: Option<Value>,
    pub cleanup_wait: bool,
    pub queued_prepare: Option<(String, Value)>,
    pub submitted_plan: bool,
    pub presets: Vec<Value>,
    pub preset_offset: u64,
    pub preset_next: bool,
    pub stage: String,
    pub selected_preset: Option<Value>,
    loaded_spec: Option<Value>,
    pub files: usize,
    pub objects: usize,
    pub object_filters: Vec<usize>,
    pub confirmations: std::collections::BTreeSet<String>,
    pub project_mode: bool,
}
impl TransferWorkbench {
    pub fn invalidate(&mut self) {
        self.epoch = self.epoch.wrapping_add(1);
        // 已提交的计划仍由 Engine 保留；再次准备前也要显式释放其预算。
        if let Some(plan) = self.plan.take() {
            self.obsolete_plan = Some(plan);
        }
        self.submitted_plan = false;
        self.queued_prepare = None;
        self.confirmations.clear();
    }
    pub fn busy(&self) -> bool {
        self.pending.is_some()
            || self.cleanup_wait
            || self.queued_prepare.is_some()
            || self.obsolete_plan.is_some()
            || (self.plan.is_some() && !self.submitted_plan)
            || self.run.as_ref().is_some_and(|run| {
                matches!(
                    run["state"].as_str(),
                    None | Some("queued") | Some("running") | Some("cancelling")
                )
            })
    }
}
impl App {
    pub(super) fn transfer_key(&self, name: &str) -> String {
        format!("transfer-{}-{name}", self.tabs[self.active].id)
    }
    fn transfer_value(&self, name: &str) -> &str {
        self.field(&self.transfer_key(name))
    }
    fn transfer_set(&mut self, name: &str, value: impl Into<String>) {
        let key = self.transfer_key(name);
        self.field_set(&key, value);
    }
    fn transfer_field_error(&mut self, (field, message): (String, String)) {
        self.workbench.transfer.stage = if field.starts_with("object-")
            || matches!(
                field.as_str(),
                "redis-keys" | "source-prefix" | "target-prefix" | "pattern"
            ) {
            "objects"
        } else if matches!(
            field.as_str(),
            "definer-map" | "preset-name" | "project-path" | "include-schema" | "include-data"
        ) {
            "policy"
        } else {
            "source"
        }
        .into();
        self.field_error(&self.transfer_key(&field), i18n::UiMessage::local(message));
    }
    fn transfer_send(&mut self, backend: &Backend, method: &str, params: Value) {
        let token = Uuid::new_v4();
        if !matches!(
            method,
            "transfer.plan.read"
                | "transfer.run.get"
                | "transfer.run.items"
                | "execution.get"
                | "transfer.discard"
        ) && !(method == "execution.cancel"
            && self
                .workbench
                .transfer
                .obsolete_plan
                .as_ref()
                .is_some_and(|plan| plan["state"] == "preparing"))
        {
            self.status.clear();
        }
        let epoch = self.workbench.transfer.epoch;
        self.workbench.transfer.pending = Some((token, epoch, method.into()));
        let tag = format!(
            "{}:{}:transfer-ui/{token}",
            self.tabs[self.active].id, self.generation
        );
        if let Err(error) = backend.request(tag, method, params) {
            self.workbench.transfer.pending = None;
            self.status = i18n::UiMessage::raw(error.to_string());
        }
    }
    fn transfer_release_or_prepare(&mut self, backend: &Backend) {
        if let Some(plan) = self.workbench.transfer.obsolete_plan.clone() {
            if self.workbench.transfer.run.as_ref().is_some_and(|run| {
                matches!(
                    run["state"].as_str(),
                    None | Some("queued" | "running" | "cancelling")
                )
            }) {
                return;
            }
            if plan["state"] == "preparing"
                && !self.workbench.transfer.cleanup_wait
                && let Some(task) = plan.get("task_id").filter(|v| !v.is_null())
            {
                self.transfer_send(backend, "execution.cancel", json!({"task_id":task}));
                return;
            }
            if self.workbench.transfer.cleanup_wait {
                self.transfer_send(backend, "execution.get", json!({"task_id":plan["task_id"]}));
            } else {
                self.transfer_send(backend,"transfer.discard",json!({"plan_id":plan.get("id").or_else(||plan.get("plan_id")),"revision":plan["revision"]}));
            }
        } else if let Some((method, params)) = self.workbench.transfer.queued_prepare.take() {
            self.transfer_send(backend, &method, params);
        }
    }
    pub(super) fn transfer_response(
        &mut self,
        tag: &str,
        result: &Result<Value>,
        backend: &Backend,
    ) -> bool {
        let Some(token) = tag
            .strip_prefix("transfer-ui/")
            .and_then(|s| Uuid::parse_str(s).ok())
        else {
            return false;
        };
        let Some((expected, epoch, method)) = self.workbench.transfer.pending.take() else {
            return true;
        };
        if token != expected {
            self.workbench.transfer.pending = Some((expected, epoch, method));
            return true;
        }
        let stale = epoch != self.workbench.transfer.epoch;
        if stale
            && !matches!(
                method.as_str(),
                "execution.cancel" | "execution.get" | "transfer.discard"
            )
        {
            match (method.as_str(), result) {
                (
                    "transfer.prepare" | "transfer.retry.prepare" | "transfer.plan.update",
                    Ok(value),
                ) => {
                    self.workbench.transfer.obsolete_plan = Some(value.clone());
                }
                ("transfer.submit", Ok(value)) => {
                    self.workbench.transfer.run_spec = self.workbench.transfer.pending_spec.take();
                    self.workbench.transfer.run = Some(value.clone());
                }
                ("transfer.submit", Err(_)) => {
                    self.workbench.transfer.pending_spec = None;
                }
                _ => {}
            }
            if self.workbench.transfer.queued_prepare.is_some() {
                self.transfer_release_or_prepare(backend);
            }
            return true;
        }
        match result {
            Err(error) => {
                if method == "transfer.discard" && error.code == "not_found" {
                    // 过期计划可被其他准备请求清理；注册项已不存在即已完成本次释放。
                    self.workbench.transfer.obsolete_plan = None;
                    self.workbench.transfer.cleanup_wait = false;
                    self.transfer_release_or_prepare(backend);
                    return true;
                }
                if method == "transfer.plan.read"
                    && let Some(plan) = self.workbench.transfer.plan.as_mut()
                {
                    plan["state"] = json!("read_failed");
                }
                if method == "transfer.run.get"
                    && let Some(run) = self.workbench.transfer.run.as_mut()
                {
                    run["state"] = json!("read_failed");
                }
                if method == "execution.cancel"
                    && self
                        .workbench
                        .transfer
                        .obsolete_plan
                        .as_ref()
                        .is_some_and(|plan| plan["state"] == "preparing")
                {
                    // 准备任务可能在取消请求到达前结束；查询终态后才能释放计划。
                    self.workbench.transfer.cleanup_wait = true;
                } else if matches!(method.as_str(), "execution.get" | "transfer.discard") {
                    self.workbench.transfer.cleanup_wait = false;
                    self.workbench.transfer.queued_prepare = None;
                }
                self.status = i18n::UiMessage::raw(error.to_string());
            }
            Ok(value) => match method.as_str() {
                "transfer.prepare" | "transfer.plan.update" | "transfer.retry.prepare" => {
                    if method == "transfer.plan.update" {
                        self.workbench.transfer.obsolete_plan = None;
                    }
                    self.workbench.transfer.plan = Some(value.clone());
                    self.workbench.transfer.stage = "preview".into();
                    self.workbench.transfer.plan_page = None;
                    self.workbench.transfer.plan_section = "summary".into();
                    self.workbench.transfer.chunk_offset = 0;
                    self.workbench.transfer.chunk_text.clear();
                }
                "transfer.plan.read" => {
                    if let Some(page) = value.get("page") {
                        if let Some(chunk) = page.get("chunk") {
                            if let Some(data) = chunk.get("data").and_then(Value::as_str) {
                                self.workbench.transfer.chunk_text.push_str(data);
                            }
                            if let Some(next) = chunk.get("next").and_then(Value::as_u64) {
                                self.workbench.transfer.chunk_offset = next;
                            } else {
                                let parsed = serde_json::from_str::<Value>(
                                    &self.workbench.transfer.chunk_text,
                                );
                                self.workbench.transfer.chunk_offset = 0;
                                self.workbench.transfer.chunk_text.clear();
                                match parsed {
                                    Ok(item) if self.workbench.transfer.plan_section == "spec" => {
                                        if let Some(plan) = self.workbench.transfer.plan.as_mut() {
                                            plan["spec"] = item;
                                        }
                                    }
                                    Ok(item) => {
                                        self.workbench.transfer.plan_page = Some(
                                            json!({"items":[item],"offset":page["offset"],"total":page["total"],"next_offset":page["next_offset"]}),
                                        )
                                    }
                                    Err(error) => {
                                        self.status = i18n::UiMessage::raw(error.to_string())
                                    }
                                }
                            }
                        } else if self.workbench.transfer.plan_section == "spec" {
                            if let Some(spec) = page["items"]
                                .as_array()
                                .and_then(|items| items.first())
                                .cloned()
                                && let Some(plan) = self.workbench.transfer.plan.as_mut()
                            {
                                plan["spec"] = spec;
                            }
                        } else {
                            self.workbench.transfer.plan_page = Some(page.clone());
                        }
                    } else {
                        self.workbench.transfer.plan = Some(value.clone());
                    }
                }
                "transfer.submit" => {
                    self.workbench.transfer.submitted_plan = true;
                    self.workbench.transfer.run_spec = self.workbench.transfer.pending_spec.take();
                    self.workbench.transfer.run = Some(value.clone());
                    self.workbench.transfer.stage = "report".into();
                }
                "transfer.run.get" => {
                    let mut updated = value.clone();
                    if let (Some(old), Some(new)) = (
                        self.workbench
                            .transfer
                            .run
                            .as_ref()
                            .and_then(Value::as_object),
                        updated.as_object_mut(),
                    ) {
                        for key in ["task_id", "run_id"] {
                            if !new.contains_key(key)
                                && let Some(v) = old.get(key)
                            {
                                new.insert(key.into(), v.clone());
                            }
                        }
                    }
                    self.workbench.transfer.run = Some(updated);
                    if self.workbench.transfer.queued_prepare.is_some()
                        && !matches!(
                            self.workbench
                                .transfer
                                .run
                                .as_ref()
                                .and_then(|run| run["state"].as_str()),
                            None | Some("queued" | "running" | "cancelling")
                        )
                    {
                        self.transfer_release_or_prepare(backend);
                    }
                }
                "transfer.run.items" => self.workbench.transfer.items = Some(value.clone()),
                "transfer.preset.list" => {
                    self.workbench.transfer.presets = value
                        .get("items")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    self.workbench.transfer.preset_next =
                        self.workbench.transfer.presets.len() == 100;
                }
                "transfer.preset.save" => {
                    self.workbench.transfer.selected_preset = Some(value.clone())
                }
                "transfer.preset.delete" => {
                    self.workbench.transfer.selected_preset = None;
                    self.workbench.transfer.presets.clear();
                }
                "execution.cancel"
                    if self
                        .workbench
                        .transfer
                        .obsolete_plan
                        .as_ref()
                        .is_some_and(|plan| plan["state"] == "preparing") =>
                {
                    self.workbench.transfer.cleanup_wait = true;
                }
                "execution.get" if self.workbench.transfer.obsolete_plan.is_some() => {
                    if !matches!(
                        value["state"].as_str(),
                        Some("queued" | "running" | "cancelling")
                    ) {
                        self.workbench.transfer.cleanup_wait = false;
                        if let Some(plan) = self.workbench.transfer.obsolete_plan.as_mut() {
                            plan["state"] = json!("cancelled");
                        }
                        self.transfer_release_or_prepare(backend);
                    }
                }
                "transfer.discard" => {
                    self.workbench.transfer.obsolete_plan = None;
                    self.workbench.transfer.cleanup_wait = false;
                    self.transfer_release_or_prepare(backend);
                }
                _ => self.status = i18n::UiMessage::raw(value.to_string()),
            },
        }
        true
    }
    fn transfer_spec(&self) -> std::result::Result<Value, (String, String)> {
        let get = |key: &str| self.transfer_value(key).trim().to_owned();
        let mode = get("mode");
        let provider = get("provider");
        if matches!(mode.as_str(), "sql_dump" | "sql_restore") && provider != "mysql" {
            return Err(("provider".into(), "SQL 备份恢复仅用于 MySQL".into()));
        }
        if get("include-schema") != "true" && get("include-data") != "true" {
            return Err(("include-schema".into(), "至少选择结构或数据".into()));
        }
        let endpoint = |prefix: &str| -> std::result::Result<Value, (String, String)> {
            if self.workbench.transfer.project_mode {
                let alias = get(&format!("{prefix}-alias"));
                if alias.is_empty() {
                    return Err((format!("{prefix}-alias"), "请选择项目连接别名".into()));
                }
                return Ok(json!({"connection_alias":alias}));
            }
            let profile = get(&format!("{prefix}-profile"));
            let id = Uuid::parse_str(&profile)
                .map_err(|_| (format!("{prefix}-profile"), "请选择来源和目标连接".into()))?;
            let database = get(&format!("{prefix}-database"));
            let redis = get(&format!("{prefix}-redis-db"));
            let redis_db = if redis.is_empty() {
                None
            } else {
                Some(redis.parse::<u32>().map_err(|_| {
                    (
                        format!("{prefix}-redis-db"),
                        "Redis DB 必须为非负整数".into(),
                    )
                })?)
            };
            Ok(
                json!({"profile_id":id,"database":if database.is_empty(){None}else{Some(database)},"redis_db":redis_db}),
            )
        };
        let source = if matches!(mode.as_str(), "copy" | "export" | "sql_dump") {
            Some(endpoint("source")?)
        } else {
            None
        };
        let destination = if matches!(mode.as_str(), "copy" | "import" | "sql_restore") {
            Some(endpoint("destination")?)
        } else {
            None
        };
        let format = get("format");
        let mut inputs = Vec::new();
        if matches!(mode.as_str(), "import" | "sql_restore") {
            for i in 0..self.workbench.transfer.files {
                let path = get(&format!("file-{i}"));
                if path.is_empty() {
                    return Err((format!("file-{i}"), "输入文件路径不能为空".into()));
                }
                let table = get(&format!("file-target-{i}"));
                let database = get("destination-database");
                let target = if table.is_empty() {
                    Value::Null
                } else {
                    json!({"database":database,"name":table,"kind":"table","key_b64":null})
                };
                let mappings = parse_mapping(&get(&format!("file-mapping-{i}")))
                    .map_err(|e| (format!("file-mapping-{i}"), e))?;
                if mappings
                    .values()
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
                    != mappings.len()
                {
                    return Err((format!("file-mapping-{i}"), "目标列为空或重复".into()));
                }
                let range = get(&format!("file-range-{i}"));
                let ranges: Vec<Value> = if range.is_empty() {
                    Vec::new()
                } else {
                    range.split(',').map(|part| {
                        let (first, last) = part.trim().split_once('-').ok_or("记录范围使用 起始-结束")?;
                        Ok(json!({"first":first.trim().parse::<u64>().map_err(|_| "无效记录范围")?,"last":last.trim().parse::<u64>().map_err(|_| "无效记录范围")?}))
                    }).collect::<std::result::Result<_, &str>>().map_err(|e| (format!("file-range-{i}"), e.to_owned()))?
                };
                if ranges.iter().any(|r| {
                    r["first"].as_u64() == Some(0) || r["last"].as_u64() < r["first"].as_u64()
                }) {
                    return Err((
                        format!("file-range-{i}"),
                        "记录范围须从 1 开始且结束不早于起始".into(),
                    ));
                }
                let types = parse_types(&get(&format!("file-types-{i}")))
                    .map_err(|e| (format!("file-types-{i}"), e))?;
                let mut defaults = BTreeMap::new();
                for column in &types {
                    let name = column["name"].as_str().unwrap_or("");
                    let kind = self.transfer_value(&column_default_key(i, name, "kind"));
                    if kind.is_empty() {
                        continue;
                    }
                    let default = match kind {
                        "unspecified" | "null" => json!({"kind":kind}),
                        "literal" | "expression" => {
                            json!({"kind":kind,"value":self.transfer_value(&column_default_key(i, name, "value"))})
                        }
                        _ => {
                            return Err((
                                column_default_key(i, name, "kind"),
                                "无效的列默认值类型".into(),
                            ));
                        }
                    };
                    defaults.insert(name.to_owned(), default);
                }
                let file_format = get(&format!("file-format-{i}"));
                // JSON 字符串数组可无损表达包含逗号或空白的列名。
                let list = |key: &str| -> std::result::Result<Vec<String>, (String, String)> {
                    let text = self.transfer_value(&format!("file-{key}-{i}"));
                    if text.trim().is_empty() {
                        Ok(Vec::new())
                    } else {
                        serde_json::from_str(text).map_err(|_| {
                            (
                                format!("file-{key}-{i}"),
                                "CSV 列列表必须是 JSON 字符串数组".into(),
                            )
                        })
                    }
                };
                let csv = datapier_core::model::transfer::CsvOptions {
                    encoding: get(&format!("file-encoding-{i}")),
                    delimiter: self
                        .transfer_value(&format!("file-delimiter-{i}"))
                        .to_owned(),
                    header: get(&format!("file-header-{i}")) == "true",
                    columns: list("columns")?,
                    null_marker: self
                        .transfer_value(&format!("file-null-marker-{i}"))
                        .to_owned(),
                    backslash_escape: get(&format!("file-backslash-{i}")) == "true",
                    binary_columns: list("binary-columns")?,
                };
                if mode == "import" && file_format == "csv" {
                    csv.validate().map_err(|error| {
                        let key = if error.message.contains("分隔符") {
                            "delimiter"
                        } else if error.message.contains("编码") {
                            "encoding"
                        } else {
                            "columns"
                        };
                        (format!("file-{key}-{i}"), error.message)
                    })?;
                }
                inputs.push(json!({"path":path,"format":if mode=="sql_restore" {"sql"} else {&file_format},"target":target,"column_mapping":mappings,"columns":types,"column_defaults":defaults,"record_ranges":ranges,"sync_schema":get(&format!("file-sync-{i}"))=="true","csv":csv}));
            }
        }
        let output = if matches!(mode.as_str(), "export" | "sql_dump") {
            if get("output").is_empty() {
                return Err(("output".into(), "输出文件路径不能为空".into()));
            }
            Some(json!({"path":get("output"),"format":if mode=="sql_dump" {"sql"} else {&format}}))
        } else {
            None
        };
        let mut objects = Vec::new();
        for i in 0..self.workbench.transfer.objects {
            let source_name = get(&format!("object-source-{i}"));
            if source_name.is_empty() {
                continue;
            }
            let target_name = get(&format!("object-target-{i}"));
            let source_db = match get(&format!("object-source-db-{i}")) {
                db if db.is_empty() => get("source-database"),
                db => db,
            };
            let target_db = match get(&format!("object-target-db-{i}")) {
                db if db.is_empty() => get("destination-database"),
                db => db,
            };
            if self.workbench.transfer.project_mode
                && (!source_db.is_empty() || !target_db.is_empty())
            {
                return Err((
                    format!(
                        "object-{}-db-{i}",
                        if source_db.is_empty() {
                            "target"
                        } else {
                            "source"
                        }
                    ),
                    "项目迁移的实际数据库请通过本机绑定填写".into(),
                ));
            }
            let kind = get(&format!("object-kind-{i}"));
            let mappings = parse_mapping(&get(&format!("object-mapping-{i}")))
                .map_err(|e| (format!("object-mapping-{i}"), e))?;
            if mappings
                .values()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != mappings.len()
            {
                return Err((format!("object-mapping-{i}"), "目标列为空或重复".into()));
            }
            let columns: Vec<_> = mappings
                .into_iter()
                .map(|(source, target)| json!({"source":source,"target":target}))
                .collect();
            let types = parse_types(&get(&format!("object-types-{i}")))
                .map_err(|e| (format!("object-types-{i}"), e))?;
            let mut filters = Vec::new();
            for j in 0..self
                .workbench
                .transfer
                .object_filters
                .get(i)
                .copied()
                .unwrap_or(1)
            {
                let column = get(&format!("object-filter-column-{i}-{j}"));
                if column.is_empty() {
                    continue;
                }
                let operator = get(&format!("object-filter-op-{i}-{j}"));
                let value_type = get(&format!("object-filter-type-{i}-{j}"));
                let text = self.transfer_value(&format!("object-filter-value-{i}-{j}"));
                if !matches!(operator.as_str(), "is_null" | "is_not_null") {
                    let valid = match value_type.as_str() {
                        "integer" => text.parse::<i64>().is_ok(),
                        "unsigned" => text.parse::<u64>().is_ok(),
                        "decimal" => text.parse::<bigdecimal::BigDecimal>().is_ok(),
                        "float" => text.parse::<f64>().is_ok_and(f64::is_finite),
                        _ => true,
                    };
                    if !valid {
                        return Err((
                            format!("object-filter-value-{i}-{j}"),
                            "过滤值与选择的数值类型不匹配".into(),
                        ));
                    }
                }
                let value = if matches!(operator.as_str(), "is_null" | "is_not_null") {
                    Value::Null
                } else if value_type == "bool" {
                    json!({"type":"bool","value":text.trim().parse::<bool>().map_err(|_| (format!("object-filter-value-{i}-{j}"), "布尔过滤值须为 true 或 false".into()))?})
                } else if value_type == "json" {
                    let value: Value = serde_json::from_str(text).map_err(|_| {
                        (
                            format!("object-filter-value-{i}-{j}"),
                            "JSON 过滤值无效".into(),
                        )
                    })?;
                    json!({"type":"json","value":value})
                } else if value_type == "bytes" {
                    use base64::{Engine as _, engine::general_purpose::STANDARD};
                    STANDARD.decode(text).map_err(|_| {
                        (format!("object-filter-value-{i}-{j}"), "Base64 无效".into())
                    })?;
                    json!({"type":"bytes","value":text})
                } else if value_type == "null" {
                    json!({"type":"null"})
                } else {
                    json!({"type":value_type,"value":text})
                };
                filters.push(json!({"column":column,"operator":operator,"value":value}));
            }
            objects.push(json!({"source":{"database":source_db,"name":source_name,"kind":kind,"key_b64":null},"target":{"database":target_db,"name":if target_name.is_empty(){source_name.clone()}else{target_name},"kind":kind,"key_b64":null},"columns":columns,"target_columns":types,"filters":filters,"create_if_missing":get(&format!("object-create-{i}"))=="true","sync_schema":get(&format!("object-sync-{i}"))=="true"}));
        }
        // 空 Base64 是合法的空字节 key；选择模式决定空文本的含义。
        let keys: Vec<_> = if get("redis-selection") == "keys" {
            self.transfer_value("redis-keys")
                .split(',')
                .map(|s| s.trim().to_owned())
                .collect()
        } else {
            Vec::new()
        };
        let prefix = get("source-prefix");
        let target_prefix = get("target-prefix");
        if prefix.is_empty() != target_prefix.is_empty() {
            return Err((
                if prefix.is_empty() {
                    "source-prefix"
                } else {
                    "target-prefix"
                }
                .into(),
                "源前缀和目标前缀必须同时指定".into(),
            ));
        }
        if provider == "redis" {
            use base64::{Engine as _, engine::general_purpose::STANDARD};
            for (field, values) in [
                (
                    "redis-keys",
                    keys.iter().map(String::as_str).collect::<Vec<_>>(),
                ),
                ("source-prefix", vec![prefix.as_str()]),
                ("target-prefix", vec![target_prefix.as_str()]),
            ] {
                if values
                    .into_iter()
                    .any(|value| STANDARD.decode(value).is_err())
                {
                    return Err((field.into(), "Base64 无效".into()));
                }
            }
        }
        let definer_mode = get("definer-mode");
        let definer = if definer_mode == "map" {
            json!({"mode":"map","accounts":parse_mapping(&get("definer-map")).map_err(|e| ("definer-map".into(), e))?})
        } else {
            json!({"mode":definer_mode})
        };
        let mut spec = json!({"mode":mode,"provider":provider,"source":source,"destination":destination,"inputs":inputs,"output":output,"objects":objects,"redis":{"keys_b64":keys,"pattern":if get("redis-selection")=="keys" || get("pattern").is_empty(){None}else{Some(get("pattern"))},"source_prefix_b64":if prefix.is_empty(){None}else{Some(prefix)},"target_prefix_b64":if target_prefix.is_empty(){None}else{Some(target_prefix)}},"options":{"conflict":get("conflict"),"conversion_errors":get("conversion"),"include_schema":get("include-schema")=="true","include_data":get("include-data")=="true","create_missing":get("create-missing")=="true","definer":definer,"existing_triggers_confirmed":get("triggers-confirmed")=="true"}});
        if let Some(base) = &self.workbench.transfer.loaded_spec {
            retain_unedited_spec(&mut spec, base);
        }
        Ok(spec)
    }
    fn transfer_object_defaults(&mut self, i: usize) {
        for (key, value) in [
            ("source", ""),
            ("target", ""),
            ("source-db", ""),
            ("target-db", ""),
            ("mapping", ""),
            ("types", ""),
            ("kind", "table"),
            ("create", "false"),
            ("sync", "false"),
        ] {
            self.transfer_set(&format!("object-{key}-{i}"), value);
        }
        if self.workbench.transfer.object_filters.len() <= i {
            self.workbench.transfer.object_filters.resize(i + 1, 0);
        }
        self.workbench.transfer.object_filters[i] = 1;
        self.transfer_filter_defaults(i, 0);
    }
    fn transfer_filter_defaults(&mut self, i: usize, j: usize) {
        for (key, value) in [
            ("column", ""),
            ("op", "eq"),
            ("type", "text"),
            ("value", ""),
        ] {
            self.transfer_set(&format!("object-filter-{key}-{i}-{j}"), value);
        }
    }
    pub(super) fn transfer_load_preset(&mut self, preset: Value) {
        self.dirty = true;
        let spec = &preset["spec"];
        self.transfer_set(
            "source-alias",
            spec["source"]["connection_alias"].as_str().unwrap_or(""),
        );
        self.transfer_set(
            "destination-alias",
            spec["destination"]["connection_alias"]
                .as_str()
                .unwrap_or(""),
        );
        for key in ["mode", "provider"] {
            if let Some(value) = spec[key].as_str() {
                self.transfer_set(key, value);
            }
        }
        self.transfer_set(
            "definer-mode",
            spec["options"]["definer"]["mode"]
                .as_str()
                .unwrap_or("current_user"),
        );
        self.transfer_set(
            "definer-map",
            format_map(&spec["options"]["definer"]["accounts"]),
        );
        // 触发器确认针对当前目标及本次执行，不能从共享预设继承。
        self.transfer_set("triggers-confirmed", "false");
        for (key, path) in [
            ("source-profile", &spec["source"]["profile_id"]),
            ("destination-profile", &spec["destination"]["profile_id"]),
            ("source-database", &spec["source"]["database"]),
            ("destination-database", &spec["destination"]["database"]),
            ("format", &spec["output"]["format"]),
            ("output", &spec["output"]["path"]),
            ("pattern", &spec["redis"]["pattern"]),
            ("conflict", &spec["options"]["conflict"]),
            ("conversion", &spec["options"]["conversion_errors"]),
        ] {
            self.transfer_set(
                key,
                path.as_str().unwrap_or(match key {
                    "format" => "csv",
                    "conflict" => "insert",
                    "conversion" => "stop",
                    _ => "",
                }),
            );
        }
        for (key, value) in [
            ("source-redis-db", &spec["source"]["redis_db"]),
            ("destination-redis-db", &spec["destination"]["redis_db"]),
            ("include-schema", &spec["options"]["include_schema"]),
            ("include-data", &spec["options"]["include_data"]),
            ("create-missing", &spec["options"]["create_missing"]),
        ] {
            self.transfer_set(
                key,
                if value.is_null() {
                    match key {
                        "include-schema" | "include-data" => "true".into(),
                        "create-missing" => "false".into(),
                        _ => String::new(),
                    }
                } else {
                    value.to_string()
                },
            );
        }
        self.transfer_set(
            "redis-selection",
            if spec["redis"]["keys_b64"]
                .as_array()
                .is_some_and(|keys| !keys.is_empty())
            {
                "keys"
            } else {
                "pattern"
            },
        );
        self.transfer_set(
            "redis-keys",
            spec["redis"]["keys_b64"]
                .as_array()
                .map(|keys| {
                    keys.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default(),
        );
        self.transfer_set(
            "source-prefix",
            spec["redis"]["source_prefix_b64"].as_str().unwrap_or(""),
        );
        self.transfer_set(
            "target-prefix",
            spec["redis"]["target_prefix_b64"].as_str().unwrap_or(""),
        );
        let inputs = spec["inputs"].as_array().cloned().unwrap_or_default();
        self.workbench.transfer.files = inputs.len().max(1);
        for i in 0..self.workbench.transfer.files {
            let item = inputs.get(i).cloned().unwrap_or(Value::Null);
            self.transfer_set(&format!("file-{i}"), item["path"].as_str().unwrap_or(""));
            self.transfer_set(
                &format!("file-target-{i}"),
                item["target"]["name"].as_str().unwrap_or(""),
            );
            self.transfer_set(
                &format!("file-mapping-{i}"),
                format_map(&item["column_mapping"]),
            );
            self.transfer_set(&format!("file-types-{i}"), format_types(&item["columns"]));
            if let Some(columns) = item["columns"].as_array() {
                for column in columns {
                    if let Some(name) = column["name"].as_str() {
                        let default = &item["column_defaults"][name];
                        self.transfer_set(
                            &column_default_key(i, name, "kind"),
                            default["kind"].as_str().unwrap_or(""),
                        );
                        self.transfer_set(
                            &column_default_key(i, name, "value"),
                            default["value"].as_str().unwrap_or(""),
                        );
                    }
                }
            }
            let range = item["record_ranges"]
                .as_array()
                .map(|ranges| {
                    ranges
                        .iter()
                        .map(|r| format!("{}-{}", r["first"], r["last"]))
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            self.transfer_set(&format!("file-range-{i}"), range);
            self.transfer_set(
                &format!("file-format-{i}"),
                item["format"].as_str().unwrap_or("csv"),
            );
            self.transfer_set(
                &format!("file-encoding-{i}"),
                item["csv"]["encoding"].as_str().unwrap_or("utf-8"),
            );
            self.transfer_set(
                &format!("file-delimiter-{i}"),
                item["csv"]["delimiter"].as_str().unwrap_or(","),
            );
            for (key, value) in [
                (
                    "header",
                    item["csv"]["header"].as_bool().unwrap_or(true).to_string(),
                ),
                (
                    "backslash",
                    item["csv"]["backslash_escape"]
                        .as_bool()
                        .unwrap_or(true)
                        .to_string(),
                ),
                (
                    "null-marker",
                    item["csv"]["null_marker"]
                        .as_str()
                        .unwrap_or("\\N")
                        .to_owned(),
                ),
                (
                    "columns",
                    item["csv"]["columns"]
                        .as_array()
                        .map(|v| json!(v).to_string())
                        .unwrap_or_else(|| "[]".into()),
                ),
                (
                    "binary-columns",
                    item["csv"]["binary_columns"]
                        .as_array()
                        .map(|v| json!(v).to_string())
                        .unwrap_or_else(|| "[]".into()),
                ),
            ] {
                self.transfer_set(&format!("file-{key}-{i}"), value);
            }
            self.transfer_set(
                &format!("file-sync-{i}"),
                if item["sync_schema"] == true {
                    "true"
                } else {
                    "false"
                },
            );
        }
        let objects = spec["objects"].as_array().cloned().unwrap_or_default();
        self.workbench.transfer.objects = objects.len().max(1);
        for i in 0..self.workbench.transfer.objects {
            self.transfer_object_defaults(i);
            let object = objects.get(i).cloned().unwrap_or(Value::Null);
            self.transfer_set(
                &format!("object-create-{i}"),
                object["create_if_missing"]
                    .as_bool()
                    .unwrap_or(false)
                    .to_string(),
            );
            self.transfer_set(
                &format!("object-sync-{i}"),
                object["sync_schema"].as_bool().unwrap_or(false).to_string(),
            );
            self.transfer_set(
                &format!("object-source-{i}"),
                object["source"]["name"].as_str().unwrap_or(""),
            );
            self.transfer_set(
                &format!("object-target-{i}"),
                object["target"]["name"].as_str().unwrap_or(""),
            );
            self.transfer_set(
                &format!("object-source-db-{i}"),
                object["source"]["database"].as_str().unwrap_or(""),
            );
            self.transfer_set(
                &format!("object-target-db-{i}"),
                object["target"]["database"].as_str().unwrap_or(""),
            );
            self.transfer_set(
                &format!("object-kind-{i}"),
                object["source"]["kind"].as_str().unwrap_or("table"),
            );
            if let Some(filters) = object["filters"].as_array() {
                self.workbench.transfer.object_filters[i] = filters.len().max(1);
                for (j, filter) in filters.iter().enumerate() {
                    self.transfer_set(
                        &format!("object-filter-column-{i}-{j}"),
                        filter["column"].as_str().unwrap_or(""),
                    );
                    self.transfer_set(
                        &format!("object-filter-op-{i}-{j}"),
                        filter["operator"].as_str().unwrap_or("eq"),
                    );
                    self.transfer_set(
                        &format!("object-filter-type-{i}-{j}"),
                        filter["value"]["type"].as_str().unwrap_or("text"),
                    );
                    let value = &filter["value"]["value"];
                    let text = if filter["value"]["type"] == "json" {
                        value.to_string()
                    } else {
                        value.as_str().map(str::to_owned).unwrap_or_else(|| {
                            if value.is_null() {
                                String::new()
                            } else {
                                value.to_string()
                            }
                        })
                    };
                    self.transfer_set(&format!("object-filter-value-{i}-{j}"), text);
                }
            }
            let mapping: BTreeMap<String, String> = object["columns"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|c| {
                    Some((
                        c["source"].as_str()?.to_owned(),
                        c["target"].as_str()?.to_owned(),
                    ))
                })
                .collect();
            let mapping = serde_json::to_string(&mapping).unwrap_or_default();
            self.transfer_set(&format!("object-mapping-{i}"), mapping);
            self.transfer_set(
                &format!("object-types-{i}"),
                format_types(&object["target_columns"]),
            );
        }
        self.transfer_set("preset-name", preset["name"].as_str().unwrap_or(""));
        self.workbench.transfer.invalidate();
        self.workbench.transfer.loaded_spec = Some(spec.clone());
        self.workbench.transfer.selected_preset = Some(preset);
    }
    pub(super) fn transfer_action(&mut self, name: &str, backend: &Backend) -> bool {
        if let Some(run_id) = name.strip_prefix("transfer-report-open:") {
            if Uuid::parse_str(run_id).is_ok() {
                self.transfer_action("transfer-new:copy", backend);
                self.workbench.transfer.run = Some(json!({"run_id":run_id}));
                self.workbench.transfer.stage = "report".into();
                self.transfer_send(backend, "transfer.run.get", json!({"run_id":run_id}));
            }
            return true;
        }
        if let Some(context) = name.strip_prefix("transfer-redis:") {
            let parts: Vec<_> = context.split(':').collect();
            if parts.len() == 3 {
                let mode = parts[0];
                let key = parts[1];
                let database = parts[2];
                self.transfer_action(&format!("transfer-new:{mode}"), backend);
                self.transfer_set("redis-selection", "keys");
                self.transfer_set("redis-keys", key);
                self.transfer_set("pattern", "");
                self.transfer_set("source-redis-db", database);
                self.transfer_set("destination-redis-db", database);
            }
            return true;
        }
        if let Some(context) = name.strip_prefix("transfer-context:") {
            use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
            let parts: Vec<_> = context.split(':').collect();
            if parts.len() == 4 {
                let mode = parts[0];
                let db = URL_SAFE_NO_PAD
                    .decode(parts[2])
                    .ok()
                    .and_then(|v| String::from_utf8(v).ok())
                    .unwrap_or_default();
                let table = URL_SAFE_NO_PAD
                    .decode(parts[3])
                    .ok()
                    .and_then(|v| String::from_utf8(v).ok())
                    .unwrap_or_default();
                let profile = Uuid::parse_str(parts[1])
                    .ok()
                    .and_then(|id| self.profiles.iter().find(|p| p.id == id))
                    .cloned();
                self.transfer_action(&format!("transfer-new:{mode}"), backend);
                if let Some(profile) = profile {
                    self.transfer_set("source-profile", profile.id.to_string());
                    self.transfer_set("destination-profile", profile.id.to_string());
                    self.transfer_set(
                        "provider",
                        if profile.provider == datapier_core::model::ProviderKind::Redis {
                            "redis"
                        } else {
                            "mysql"
                        },
                    );
                }
                if !db.is_empty() {
                    self.transfer_set("source-database", &db);
                    self.transfer_set("destination-database", &db);
                }
                if !table.is_empty() {
                    self.transfer_set("file-target-0", &table);
                    self.transfer_set("object-source-0", &table);
                    self.transfer_set("object-target-0", &table);
                }
            }
            return true;
        }
        if let Some(mode) = name
            .strip_prefix("transfer-new:")
            .or_else(|| name.strip_prefix("transfer-project-new:"))
        {
            if name.starts_with("transfer-project-new:")
                && (self.workbench.transfer.busy() || self.workbench.transfer.plan.is_some())
            {
                self.status = i18n::UiMessage::key("请求处理中，请稍候");
                return true;
            }
            self.dirty = true;
            if name.starts_with("transfer-project-new:") {
                self.workbench.transfer = TransferWorkbench::default();
            }
            let profile = self.profile.clone();
            let redis_db = profile
                .as_ref()
                .map(|_| self.current_redis_db().to_string())
                .unwrap_or_default();
            let database = self.database.clone().unwrap_or_default();
            let target = self.target.clone();
            let project_mode = name.starts_with("transfer-project-new:");
            if !project_mode {
                self.create_query_tab();
            }
            self.workbench.transfer.project_mode = project_mode;
            self.tabs[self.active].title = i18n::tr("数据交换与迁移").into_owned();
            self.view = "transfer-workbench".into();
            let provider = profile
                .as_ref()
                .map(|p| {
                    if p.provider == datapier_core::model::ProviderKind::Redis {
                        "redis"
                    } else {
                        "mysql"
                    }
                })
                .unwrap_or("mysql");
            let id = profile.map(|p| p.id.to_string()).unwrap_or_default();
            for (key, value) in [
                ("mode", mode),
                ("provider", provider),
                ("source-profile", id.as_str()),
                ("destination-profile", id.as_str()),
                ("source-database", database.as_str()),
                ("destination-database", database.as_str()),
                ("source-redis-db", redis_db.as_str()),
                ("destination-redis-db", redis_db.as_str()),
                ("format", "csv"),
                ("encoding", "utf-8"),
                ("delimiter", ","),
                ("conflict", "insert"),
                ("conversion", "stop"),
                ("include-schema", "true"),
                ("include-data", "true"),
                ("create-missing", "false"),
                ("sync-schema", "false"),
                ("pattern", "*"),
                ("redis-selection", "pattern"),
            ] {
                self.transfer_set(key, value);
            }
            if project_mode {
                for key in [
                    "source-database",
                    "destination-database",
                    "source-redis-db",
                    "destination-redis-db",
                    "source-alias",
                    "destination-alias",
                    "project-path",
                ] {
                    self.transfer_set(key, "");
                }
            }
            self.transfer_set("report-format", "json");
            self.transfer_set("report-input", "false");
            for key in ["retry-failed", "retry-rolled-back", "retry-not-sent"] {
                self.transfer_set(key, "true");
            }
            self.workbench.transfer.files = 1;
            self.workbench.transfer.objects = 1;
            for key in [
                "file-0",
                "file-target-0",
                "file-mapping-0",
                "file-range-0",
                "file-types-0",
                "object-source-0",
                "object-target-0",
                "object-mapping-0",
                "object-types-0",
                "output",
                "preset-name",
                "redis-keys",
                "source-prefix",
                "target-prefix",
                "report-path",
            ] {
                self.transfer_set(key, "");
            }
            self.transfer_set("file-format-0", "csv");
            self.transfer_set("file-encoding-0", "utf-8");
            self.transfer_set("file-delimiter-0", ",");
            self.transfer_set("file-header-0", "true");
            self.transfer_set("file-backslash-0", "true");
            self.transfer_set("file-null-marker-0", "\\N");
            self.transfer_set("file-columns-0", "[]");
            self.transfer_set("file-binary-columns-0", "[]");
            self.transfer_set("file-sync-0", "false");
            self.transfer_set("definer-mode", "current_user");
            self.transfer_set("definer-map", "");
            self.transfer_set("triggers-confirmed", "false");
            self.transfer_object_defaults(0);
            if let Some(target) = target.filter(|t| t.kind == "table") {
                self.transfer_set("file-target-0", &target.name);
                self.transfer_set("object-source-0", &target.name);
                self.transfer_set("object-target-0", &target.name);
            }
            return true;
        }
        if let Some(rest) = name.strip_prefix("field:transfer-") {
            if let Some((field, value)) = rest.split_once(':') {
                let field = format!("transfer-{field}");
                if matches!(value, "literal" | "expression")
                    && let Some(prefix) = field.strip_suffix("-kind")
                    && prefix.contains("-file-default-")
                {
                    let value_field = format!("{prefix}-value");
                    if !self.fields.contains_key(&value_field) {
                        self.field_set(&value_field, "");
                    }
                }
                self.field_set(&field, value);
                self.changed_field(&field);
                self.workbench.generic_select = None;
            }
            return true;
        }
        if let Some(stage) = name.strip_prefix("transfer-stage:") {
            if matches!(
                stage,
                "source" | "objects" | "policy" | "preview" | "report"
            ) {
                self.workbench.transfer.stage = stage.into();
                self.focus.clear();
            }
            return true;
        }
        if !name.starts_with("transfer-ui-") {
            return false;
        }
        self.dirty = true;
        if (self.workbench.transfer.pending.is_some()
            && name != "transfer-ui-prepare"
            && name != "transfer-ui-retry")
            || (self.workbench.transfer.queued_prepare.is_some()
                && !matches!(
                    name,
                    "transfer-ui-cleanup-poll"
                        | "transfer-ui-prepare"
                        | "transfer-ui-retry"
                        | "transfer-ui-refresh-run"
                        | "transfer-ui-run-items"
                        | "transfer-ui-cancel"
                ))
        {
            self.status = i18n::UiMessage::key("请求处理中，请稍候");
            return true;
        }
        if let Some(i) = name
            .strip_prefix("transfer-ui-filter-add:")
            .and_then(|s| s.parse::<usize>().ok())
        {
            if i < self.workbench.transfer.objects {
                let j = self
                    .workbench
                    .transfer
                    .object_filters
                    .get(i)
                    .copied()
                    .unwrap_or(0);
                if self.workbench.transfer.object_filters.len() <= i {
                    self.workbench.transfer.object_filters.resize(i + 1, 0);
                }
                self.workbench.transfer.object_filters[i] = j + 1;
                self.transfer_filter_defaults(i, j);
                self.workbench.transfer.invalidate();
            }
            return true;
        }
        if let Some((i, j)) = name
            .strip_prefix("transfer-ui-filter-remove:")
            .and_then(|s| s.split_once(':'))
            .and_then(|(i, j)| Some((i.parse::<usize>().ok()?, j.parse::<usize>().ok()?)))
        {
            let count = self
                .workbench
                .transfer
                .object_filters
                .get(i)
                .copied()
                .unwrap_or(0);
            if i < self.workbench.transfer.objects && j < count {
                for index in j..count - 1 {
                    for key in ["column", "op", "type", "value"] {
                        let value = self
                            .transfer_value(&format!("object-filter-{key}-{i}-{}", index + 1))
                            .to_owned();
                        self.transfer_set(&format!("object-filter-{key}-{i}-{index}"), value);
                    }
                }
                self.workbench.transfer.object_filters[i] = count - 1;
                self.workbench.transfer.invalidate();
            }
            return true;
        }
        if let Some(section) = name.strip_prefix("transfer-ui-section:") {
            if matches!(section, "items" | "samples" | "diagnostics" | "spec") {
                self.workbench.transfer.plan_section = section.into();
                self.workbench.transfer.plan_offset = 0;
                self.workbench.transfer.plan_pages.clear();
                self.workbench.transfer.chunk_offset = 0;
                self.workbench.transfer.chunk_text.clear();
                self.workbench.transfer.plan_page = None;
                self.transfer_action("transfer-ui-page-read", backend);
            }
            return true;
        }
        if let Some(index) = name
            .strip_prefix("transfer-ui-page-item:")
            .and_then(|v| v.parse::<usize>().ok())
        {
            if let (Some(plan), Some(item)) = (
                self.workbench.transfer.plan.clone(),
                self.workbench
                    .transfer
                    .plan_page
                    .as_ref()
                    .and_then(|p| p["items"].get(index))
                    .cloned(),
            ) {
                if !transfer_selectable_item(&item) {
                    return true;
                }
                let selected = !item["selected"].as_bool().unwrap_or(false);
                self.workbench.transfer.invalidate();
                self.transfer_send(backend,"transfer.plan.update",json!({"plan_id":plan.get("id").or_else(||plan.get("plan_id")),"expected_revision":plan["revision"],"item_selection":[{"id":item["id"],"selected":selected}]}));
            }
            return true;
        }
        match name {
            "transfer-ui-file-add" => {
                let i = self.workbench.transfer.files;
                self.workbench.transfer.files += 1;
                for key in [
                    format!("file-{i}"),
                    format!("file-target-{i}"),
                    format!("file-mapping-{i}"),
                    format!("file-range-{i}"),
                    format!("file-types-{i}"),
                ] {
                    self.transfer_set(&key, "");
                }
                self.transfer_set(&format!("file-format-{i}"), "csv");
                self.transfer_set(&format!("file-encoding-{i}"), "utf-8");
                self.transfer_set(&format!("file-delimiter-{i}"), ",");
                self.transfer_set(&format!("file-header-{i}"), "true");
                self.transfer_set(&format!("file-backslash-{i}"), "true");
                self.transfer_set(&format!("file-null-marker-{i}"), "\\N");
                self.transfer_set(&format!("file-columns-{i}"), "[]");
                self.transfer_set(&format!("file-binary-columns-{i}"), "[]");
                self.transfer_set(&format!("file-sync-{i}"), "false");
                self.workbench.transfer.invalidate();
            }
            "transfer-ui-object-add" => {
                let i = self.workbench.transfer.objects;
                self.workbench.transfer.objects += 1;
                self.transfer_object_defaults(i);
                self.workbench.transfer.invalidate();
            }
            "transfer-ui-file-remove" => {
                if self.workbench.transfer.files > 1 {
                    self.workbench.transfer.files -= 1;
                    self.workbench.transfer.invalidate();
                }
            }
            "transfer-ui-object-remove" => {
                if self.workbench.transfer.objects > 1 {
                    self.workbench.transfer.objects -= 1;
                    self.workbench.transfer.invalidate();
                }
            }
            "transfer-ui-project-return" => {
                if self.workbench.transfer.busy() || self.workbench.transfer.plan.is_some() {
                    self.workbench.transfer.stage = "preview".into();
                    self.status = i18n::UiMessage::key("请先丢弃迁移计划并等待任务结束");
                } else {
                    self.view = "project-workbench".into();
                }
            }
            "transfer-ui-project-save" => {
                if self.workbench.transfer.busy() || self.workbench.transfer.plan.is_some() {
                    self.workbench.transfer.stage = "preview".into();
                    self.status = i18n::UiMessage::key("请先丢弃迁移计划并等待任务结束");
                    return true;
                }
                let name = self.transfer_value("preset-name").trim().to_owned();
                let path = self.transfer_value("project-path").trim().to_owned();
                if name.is_empty() || path.is_empty() {
                    self.transfer_field_error((
                        if name.is_empty() {
                            "preset-name"
                        } else {
                            "project-path"
                        }
                        .into(),
                        "请输入共享迁移名称和项目内路径".into(),
                    ));
                    return true;
                }
                for i in 0..self.workbench.transfer.objects {
                    for side in ["source", "target"] {
                        let field = format!("object-{side}-db-{i}");
                        if !self.transfer_value(&field).trim().is_empty() {
                            self.transfer_field_error((
                                field,
                                "共享配置的实际数据库请通过本机绑定填写".into(),
                            ));
                            return true;
                        }
                    }
                }
                match self.transfer_spec() {
                    Ok(spec) => {
                        let text = match serde_json::to_string_pretty(&spec) {
                            Ok(text) => text,
                            Err(error) => {
                                self.status = i18n::UiMessage::raw(error.to_string());
                                return true;
                            }
                        };
                        if let Some(project) = &mut self.workbench.project.project
                            && let Some(transfers) = project["manifest"]["transfers"].as_array_mut()
                        {
                            let id = transfers
                                .iter()
                                .find(|t| t["path"].as_str() == Some(path.as_str()))
                                .map(|t| t["id"].clone())
                                .unwrap_or_else(|| json!(Uuid::new_v4()));
                            transfers.retain(|t| t["path"].as_str() != Some(path.as_str()));
                            transfers.push(json!({"id":id,"name":name,"path":path}));
                            self.workbench.project.transfer_files.insert(path, text);
                            self.workbench.project.unsaved = true;
                            self.workbench.project.invalidate();
                            self.view = "project-workbench".into();
                        }
                    }
                    Err(error) => self.transfer_field_error(error),
                }
            }
            "transfer-ui-prepare" => match self.transfer_spec() {
                Ok(spec) => {
                    self.workbench.field_error = None;
                    self.workbench.transfer.stage = "preview".into();
                    self.workbench.transfer.invalidate();
                    self.workbench.transfer.queued_prepare = Some((
                        "transfer.prepare".into(),
                        if self.workbench.transfer.project_mode {
                            json!({"workspace_id":self.workspace_id,"spec":spec,"project_id":self.workbench.project.project.as_ref().map(|p| &p["project_id"]),"binding_group":self.field(&self.project_key("group"))})
                        } else {
                            json!({"workspace_id":self.workspace_id,"spec":spec})
                        },
                    ));
                    if self.workbench.transfer.pending.is_none() {
                        self.transfer_release_or_prepare(backend);
                    }
                }
                Err(error) => self.transfer_field_error(error),
            },
            "transfer-ui-cleanup-poll" => {
                if self.workbench.transfer.cleanup_wait {
                    self.transfer_release_or_prepare(backend);
                }
            }
            "transfer-ui-discard" => {
                self.workbench.transfer.invalidate();
                self.transfer_release_or_prepare(backend);
            }
            "transfer-ui-refresh-plan" => {
                if let Some(plan) = self.workbench.transfer.plan.clone() {
                    self.transfer_send(backend,"transfer.plan.read",json!({"plan_id":plan.get("id").or_else(||plan.get("plan_id")),"revision":plan.get("revision"),"section":"summary","offset":0,"limit":100}));
                }
            }
            "transfer-ui-page-read" => {
                if let Some(plan) = self.workbench.transfer.plan.clone() {
                    self.transfer_send(backend,"transfer.plan.read",json!({"plan_id":plan.get("id").or_else(||plan.get("plan_id")),"revision":plan.get("revision"),"section":self.workbench.transfer.plan_section,"offset":self.workbench.transfer.plan_offset,"limit":100,"chunk_offset":self.workbench.transfer.chunk_offset}));
                }
            }
            "transfer-ui-page-next" => {
                if let Some(next) = self
                    .workbench
                    .transfer
                    .plan_page
                    .as_ref()
                    .and_then(|p| p.get("next_offset"))
                    .and_then(Value::as_u64)
                {
                    self.workbench
                        .transfer
                        .plan_pages
                        .push(self.workbench.transfer.plan_offset);
                    self.workbench.transfer.plan_offset = next;
                    self.workbench.transfer.chunk_offset = 0;
                    self.workbench.transfer.chunk_text.clear();
                    self.transfer_action("transfer-ui-page-read", backend);
                }
            }
            "transfer-ui-page-prev" => {
                if let Some(previous) = self.workbench.transfer.plan_pages.pop() {
                    self.workbench.transfer.plan_offset = previous;
                    self.workbench.transfer.chunk_offset = 0;
                    self.workbench.transfer.chunk_text.clear();
                    self.transfer_action("transfer-ui-page-read", backend);
                }
            }
            "transfer-ui-submit" => {
                if let Some(plan) = self.workbench.transfer.plan.clone() {
                    self.workbench.transfer.pending_spec =
                        plan.get("spec").filter(|v| !v.is_null()).cloned();
                    self.transfer_send(backend,"transfer.submit",json!({"plan_id":plan.get("id").or_else(||plan.get("plan_id")),"revision":plan.get("revision"),"digest":plan.get("digest"),"destructive_confirmations":self.workbench.transfer.confirmations}));
                }
            }
            "transfer-ui-refresh-run" => {
                if let Some(run) = self.workbench.transfer.run.clone() {
                    self.transfer_send(
                        backend,
                        "transfer.run.get",
                        json!({"run_id":run.get("run_id").or_else(||run.get("id"))}),
                    );
                }
            }
            "transfer-ui-run-items" => {
                if let Some(run) = self.workbench.transfer.run.clone() {
                    self.transfer_send(backend,"transfer.run.items",json!({"run_id":run.get("run_id").or_else(||run.get("id")),"offset":self.workbench.transfer.run_offset,"limit":100}));
                }
            }
            "transfer-ui-items-next" => {
                if let Some(next) = self
                    .workbench
                    .transfer
                    .items
                    .as_ref()
                    .and_then(|p| p.get("next"))
                    .and_then(Value::as_u64)
                {
                    self.workbench
                        .transfer
                        .run_pages
                        .push(self.workbench.transfer.run_offset);
                    self.workbench.transfer.run_offset = next.saturating_add(1);
                    self.transfer_action("transfer-ui-run-items", backend);
                }
            }
            "transfer-ui-items-prev" => {
                if let Some(previous) = self.workbench.transfer.run_pages.pop() {
                    self.workbench.transfer.run_offset = previous;
                    self.transfer_action("transfer-ui-run-items", backend);
                }
            }
            "transfer-ui-cancel" => {
                if let Some(task) = self
                    .workbench
                    .transfer
                    .run
                    .as_ref()
                    .and_then(|v| v.get("task_id"))
                    .cloned()
                {
                    self.transfer_send(backend, "execution.cancel", json!({"task_id":task}));
                }
            }
            "transfer-ui-preset-list" | "transfer-ui-preset-next" | "transfer-ui-preset-prev" => {
                match name {
                    "transfer-ui-preset-list" => self.workbench.transfer.preset_offset = 0,
                    "transfer-ui-preset-next" if self.workbench.transfer.preset_next => {
                        self.workbench.transfer.preset_offset += 100
                    }
                    "transfer-ui-preset-prev" => {
                        self.workbench.transfer.preset_offset =
                            self.workbench.transfer.preset_offset.saturating_sub(100)
                    }
                    _ => return true,
                }
                self.transfer_send(backend, "transfer.preset.list", json!({"workspace_id":self.workspace_id,"offset":self.workbench.transfer.preset_offset,"limit":100}));
            }
            "transfer-ui-preset-save" => match self.transfer_spec() {
                Ok(spec) => {
                    if self.transfer_value("preset-name").trim().is_empty() {
                        self.transfer_field_error(("preset-name".into(), "请输入预设名称".into()));
                        return true;
                    }
                    let selected = self.workbench.transfer.selected_preset.clone();
                    self.transfer_send(backend,"transfer.preset.save",json!({"id":selected.as_ref().and_then(|v|v.get("id")),"expected_revision":selected.as_ref().and_then(|v|v.get("revision")),"workspace_id":self.workspace_id,"name":self.transfer_value("preset-name"),"spec":spec}));
                }
                Err(error) => self.transfer_field_error(error),
            },
            "transfer-ui-preset-delete" => {
                if let Some(preset) = self.workbench.transfer.selected_preset.clone() {
                    self.transfer_send(backend,"transfer.preset.delete",json!({"id":preset["id"],"expected_revision":preset["revision"],"workspace_id":self.workspace_id}));
                }
            }
            "transfer-ui-report" => {
                if let Some(run) = self.workbench.transfer.run.clone() {
                    let include = self.transfer_value("report-input") == "true";
                    let spec = if include {
                        match self.workbench.transfer.run_spec.clone().or_else(|| {
                            self.workbench
                                .transfer
                                .selected_preset
                                .as_ref()
                                .and_then(|p| p.get("spec"))
                                .cloned()
                        }) {
                            Some(spec) => Some(spec),
                            None => {
                                self.status =
                                    i18n::UiMessage::key("请先选择原预设，或仅导出报告元数据");
                                return true;
                            }
                        }
                    } else {
                        None
                    };
                    self.transfer_send(backend,"transfer.report.export",json!({"run_id":run.get("run_id").or_else(||run.get("id")),"path":self.transfer_value("report-path"),"format":self.transfer_value("report-format"),"include_failed_input":include,"spec":spec}));
                }
            }
            "transfer-ui-retry" => {
                if let Some(run) = self.workbench.transfer.run.clone() {
                    let Some(spec) = self.workbench.transfer.run_spec.clone().or_else(|| {
                        self.workbench
                            .transfer
                            .selected_preset
                            .as_ref()
                            .and_then(|p| p.get("spec"))
                            .cloned()
                    }) else {
                        self.status = i18n::UiMessage::key("请先选择原预设，再准备重试");
                        return true;
                    };
                    let states: Vec<_> = [
                        ("retry-failed", "failed"),
                        ("retry-rolled-back", "rolled_back"),
                        ("retry-not-sent", "not_sent"),
                    ]
                    .into_iter()
                    .filter_map(|(key, state)| {
                        (self.transfer_value(key) == "true").then_some(state)
                    })
                    .collect();
                    if states.is_empty() {
                        self.status = i18n::UiMessage::key("请至少选择一种续作状态");
                        return true;
                    }
                    self.workbench.transfer.invalidate();
                    self.workbench.transfer.queued_prepare = Some((
                        "transfer.retry.prepare".into(),
                        json!({"run_id":run.get("run_id").or_else(||run.get("id")),"spec":spec,"states":states}),
                    ));
                    if self.workbench.transfer.pending.is_none() {
                        self.transfer_release_or_prepare(backend);
                    }
                }
            }
            _ => {
                if let Some(id) = name.strip_prefix("transfer-ui-confirm:") {
                    if !self.workbench.transfer.confirmations.remove(id) {
                        self.workbench.transfer.confirmations.insert(id.to_owned());
                    }
                } else if let Some(index) = name
                    .strip_prefix("transfer-ui-preset-load:")
                    .and_then(|v| v.parse::<usize>().ok())
                {
                    if let Some(preset) = self.workbench.transfer.presets.get(index).cloned() {
                        self.transfer_load_preset(preset);
                    }
                } else if let Some(index) = name
                    .strip_prefix("transfer-ui-item:")
                    .and_then(|v| v.parse::<usize>().ok())
                    && let Some(plan) = self.workbench.transfer.plan.clone()
                    && let Some(item) = plan["items"].get(index)
                    && transfer_selectable_item(item)
                {
                    let selected = !item["selected"].as_bool().unwrap_or(false);
                    let item_id = item["id"].clone();
                    self.workbench.transfer.invalidate();
                    self.transfer_send(backend,"transfer.plan.update",json!({"plan_id":plan.get("id").or_else(||plan.get("plan_id")),"expected_revision":plan["revision"],"item_selection":[{"id":item_id,"selected":selected}]}));
                }
            }
        }
        true
    }
}

// 预设未暴露的类型属性必须保留；表单可编辑字段始终以当前输入为准。
fn retain_unedited_spec(current: &mut Value, base: &Value) {
    if let (Some(current), Some(base)) = (current.as_object_mut(), base.as_object()) {
        for (key, old) in base {
            if !current.contains_key(key) {
                current.insert(key.clone(), old.clone());
            }
        }
        if let (Some(value), Some(old)) = (current.get_mut("options"), base.get("options")) {
            retain_unedited_spec(value, old);
        }
        for group in ["inputs", "objects"] {
            if let (Some(values), Some(old_values)) = (
                current.get_mut(group).and_then(Value::as_array_mut),
                base.get(group).and_then(Value::as_array),
            ) {
                for (value, old) in values.iter_mut().zip(old_values) {
                    let key = if group == "inputs" {
                        "columns"
                    } else {
                        "target_columns"
                    };
                    if let (Some(columns), Some(old_columns)) = (
                        value.get_mut(key).and_then(Value::as_array_mut),
                        old.get(key).and_then(Value::as_array),
                    ) {
                        for column in columns {
                            if let Some(original) = old_columns.iter().find(|old| {
                                old["name"] == column["name"]
                                    && old["type_name"] == column["type_name"]
                            }) {
                                retain_unedited_spec(column, original);
                            }
                        }
                    }
                    retain_unedited_spec(value, old);
                }
            }
        }
    }
}

fn transfer_selectable_item(item: &Value) -> bool {
    item["phase"] == "schema"
        || (matches!(item["phase"].as_str(), Some("restore" | "after_data"))
            && item["source"].is_object()
            && item["target"].is_object())
}
fn parse_mapping(text: &str) -> std::result::Result<BTreeMap<String, String>, String> {
    if text.trim_start().starts_with('{') {
        let mappings: BTreeMap<String, String> =
            serde_json::from_str(text).map_err(|_| "映射必须是 JSON 字符串对象".to_owned())?;
        if mappings
            .iter()
            .any(|(source, target)| source.trim().is_empty() || target.trim().is_empty())
        {
            return Err("映射字段为空或重复".into());
        }
        return Ok(mappings);
    }
    let mut mappings = BTreeMap::new();
    for entry in mapping_entries(text)?
        .into_iter()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let (source, target) = entry
            .split_once('=')
            .ok_or("映射格式：源字段=目标字段，使用逗号分隔")?;
        if source.trim().is_empty()
            || target.trim().is_empty()
            || mappings
                .insert(source.trim().into(), target.trim().into())
                .is_some()
        {
            return Err("映射字段为空或重复".into());
        }
    }
    Ok(mappings)
}
// SQL 类型参数及引号中的逗号不是映射分隔符；保持原始类型文本不变。
fn mapping_entries(text: &str) -> std::result::Result<Vec<&str>, String> {
    let mut entries = Vec::new();
    let mut start = 0;
    let mut depth = 0usize;
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in text.char_indices() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == delimiter {
                quote = None;
            }
        } else {
            match ch {
                '\'' | '"' | '`' => quote = Some(ch),
                '(' => depth += 1,
                ')' => {
                    if depth == 0 {
                        return Err("映射中的括号或引号未配对".into());
                    }
                    depth -= 1;
                }
                ',' if depth == 0 => {
                    entries.push(&text[start..index]);
                    start = index + 1;
                }
                _ => {}
            }
        }
    }
    if quote.is_some() || depth != 0 {
        return Err("映射中的括号或引号未配对".into());
    }
    entries.push(&text[start..]);
    Ok(entries)
}
pub(super) fn parse_types(text: &str) -> std::result::Result<Vec<Value>, String> {
    if text.trim_start().starts_with('[') {
        let values: Vec<Value> =
            serde_json::from_str(text).map_err(|_| "目标类型必须是 JSON 数组".to_owned())?;
        serde_json::from_value::<Vec<datapier_core::model::Column>>(json!(values))
            .map_err(|error| format!("目标类型无效：{error}"))?;
        return Ok(values);
    }
    Ok(parse_mapping(text)?
        .into_iter()
        .map(|(name, type_name)| json!({"name":name,"type_name":type_name}))
        .collect())
}
pub(super) fn column_default_key(file: usize, column: &str, part: &str) -> String {
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    format!(
        "file-default-{file}-{}-{part}",
        URL_SAFE_NO_PAD.encode(column.as_bytes())
    )
}

fn format_map(value: &Value) -> String {
    value
        .as_object()
        .map(|_| value.to_string())
        .unwrap_or_default()
}
fn format_types(value: &Value) -> String {
    value
        .as_array()
        .map(|_| value.to_string())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "../tests/unit/transfer_lifecycle.rs"]
mod lifecycle_tests;
