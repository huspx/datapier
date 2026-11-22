use super::*;

impl App {
    pub(super) fn transfer_field_error(&mut self, (field, message): (String, String)) {
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
    pub(super) fn transfer_send(&mut self, backend: &Backend, method: &str, params: Value) {
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
    pub(super) fn transfer_release_or_prepare(&mut self, backend: &Backend) {
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
    pub(crate) fn transfer_response(
        &mut self,
        tag: &str,
        result: &Result<Value>,
        backend: &Backend,
    ) -> bool {
        if let Some(token) = tag
            .strip_prefix("transfer-input/")
            .and_then(|s| Uuid::parse_str(s).ok())
        {
            let key = self
                .workbench
                .transfer
                .catalog
                .iter()
                .find_map(|(key, entry)| (entry.token == Some(token)).then(|| key.clone()));
            if let Some(key) = key {
                let i = key
                    .strip_prefix("input:")
                    .and_then(|i| i.parse::<usize>().ok());
                let current = i.and_then(|i| self.transfer_input_params(i).ok());
                if current.as_ref()
                    != self
                        .workbench
                        .transfer
                        .catalog
                        .get(&key)
                        .map(|entry| &entry.signature)
                {
                    self.workbench.transfer.catalog.remove(&key);
                    return true;
                }
                if let (Some(i), Ok(value)) = (i, result) {
                    if self.transfer_value("mode") == "import"
                        && self.transfer_value(&format!("file-format-{i}")) == "csv"
                        && self.transfer_value(&format!("file-header-{i}")) == "false"
                        && self.transfer_value(&format!("file-columns-{i}")) == "[]"
                        && let Some(columns) = value["columns"].as_array()
                    {
                        let names: Vec<_> = columns
                            .iter()
                            .filter_map(|column| column["name"].as_str())
                            .collect();
                        self.transfer_set(&format!("file-columns-{i}"), json!(names).to_string());
                    }
                    let signature = self.transfer_input_params(i).unwrap_or_default();
                    if let Some(entry) = self.workbench.transfer.catalog.get_mut(&key) {
                        entry.token = None;
                        entry.signature = signature;
                        entry.value = Some(value.clone());
                    }
                } else if let Some(entry) = self.workbench.transfer.catalog.get_mut(&key) {
                    entry.token = None;
                    if let Err(error) = result {
                        entry.error = Some(error.to_string());
                    }
                }
            }
            return true;
        }
        if let Some(token) = tag
            .strip_prefix("transfer-catalog/")
            .and_then(|s| Uuid::parse_str(s).ok())
        {
            if let Some(entry) = self
                .workbench
                .transfer
                .catalog
                .values_mut()
                .find(|entry| entry.token == Some(token))
            {
                entry.token = None;
                match result {
                    Ok(value) if value["catalog"]["state"] == "unknown" => {
                        entry.error = Some(
                            value["catalog"]["reason"]
                                .as_str()
                                .unwrap_or("数据库发现失败")
                                .to_owned(),
                        );
                    }
                    Ok(value) => {
                        let mut page = value["catalog"].clone();
                        if let (Some(previous), Some(next)) = (
                            entry.value.as_ref().and_then(|v| v["resources"].as_array()),
                            page["resources"].as_array(),
                        ) {
                            let mut resources = previous.clone();
                            resources.extend(next.iter().cloned());
                            page["resources"] = json!(resources);
                        }
                        entry.session = value.get("session_id").filter(|id| !id.is_null()).cloned();
                        entry.value = Some(page);
                    }
                    Err(error) => {
                        entry.session = None;
                        entry.error = Some(error.to_string());
                    }
                }
            } else if let Ok(value) = result
                && let Some(session) = value.get("session_id").filter(|id| !id.is_null())
            {
                let _ = backend.request(
                    "transfer-catalog-close",
                    "connection.close",
                    json!({"session_id":session}),
                );
            }
            return true;
        }
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
                        if value["error"]["code"] == "transfer_target_read_only" {
                            let field = if self.workbench.transfer.project_mode {
                                "destination-alias"
                            } else {
                                "destination-profile"
                            };
                            self.transfer_field_error((
                                field.into(),
                                "目标连接为只读模式，请选择可写连接，或在连接设置中调整访问模式"
                                    .into(),
                            ));
                        }
                        if value["error"]["code"] == "transfer_scope_unknown" {
                            let field = if self.workbench.transfer.project_mode {
                                "destination-alias"
                            } else {
                                "destination-database"
                            };
                            self.transfer_field_error((field.into(), "无法证明 SQL 的访问范围；请检查目标数据库、连接可见范围及输入 SQL 中的数据库".into()));
                        }
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
}
