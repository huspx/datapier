use super::*;

impl App {
    pub(super) fn workbench_clear_cli_value(&mut self) {
        self.workbench.cli_large = None;
        self.workbench.cli_value_token = None;
        self.workbench.cli_cell = None;
        if self.workbench.overlay.as_deref() == Some("cli-value") {
            self.workbench.overlay = None;
        }
    }

    pub(super) fn workbench_cli_value(&mut self, backend: &Backend) {
        let Some(load) = self.workbench.cli_large.as_ref() else {
            return;
        };
        let (id, offset) = (load.id, load.offset);
        let token = Uuid::new_v4();
        self.workbench.cli_value_token = Some(token);
        self.request(
            backend,
            &format!("cli.value/{id}/{offset}/{token}"),
            "result.value",
            json!({"value_id":id,"offset":offset,"max_bytes":262144}),
        );
    }

    pub(super) fn workbench_cli_error(&mut self, error: UiMessage) {
        self.workbench.cli_error.push_line(error);
    }

    pub(crate) fn workbench_error(&mut self, tag: &str, error: &AppError) -> bool {
        if let Some(target) = tag.strip_prefix("cli.record/") {
            if let Some((id, index)) = target.split_once('/')
                && let (Ok(id), Ok(index)) = (Uuid::parse_str(id), index.parse::<usize>())
                && let Some(record) = self
                    .workbench
                    .cli_history
                    .iter_mut()
                    .find(|record| record.id == id)
                && record.result_grids.get(index).is_some_and(Option::is_none)
                && !(self.workbench.cli_selected == Some(id)
                    && record.results.get(index).map(String::as_str)
                        == self.workbench.cli_result_id.as_deref()
                    && (self.workbench.cli_grid.is_some()
                        || self.workbench.cli_page_token.is_some()))
            {
                record.error.push_line(UiMessage::raw(error.to_string()));
            }
            return true;
        }
        if tag == "cli.start" || (tag == "execution.get" && self.workbench.cli_running) {
            self.workbench.cli_elapsed = self
                .workbench
                .cli_started_at
                .take()
                .map(|start| start.elapsed());
        }
        let matched = if tag == "cli.start" {
            if !self.workbench.cli_running {
                return true;
            }
            self.workbench.cli_running = false;
            self.task = None;
            self.operation_pending = false;
            true
        } else if tag == "execution.get" && self.workbench.cli_running {
            self.workbench.cli_running = false;
            self.task = None;
            self.operation_pending = false;
            true
        } else if let Some(token) = tag.strip_prefix("cli.page/") {
            if Uuid::parse_str(token).ok() != self.workbench.cli_page_token
                || self.workbench.cli_page_token.is_none()
            {
                return true;
            }
            self.workbench.cli_page_token = None;
            self.workbench.cli_page_transition = None;
            true
        } else if tag.starts_with("cli.value/") {
            let expected = self
                .workbench
                .cli_large
                .as_ref()
                .zip(self.workbench.cli_value_token)
                .map(|(load, token)| format!("cli.value/{}/{}/{token}", load.id, load.offset));
            if expected.as_deref() != Some(tag) {
                return true;
            }
            self.workbench.cli_large = None;
            self.workbench.cli_value_token = None;
            true
        } else if tag.starts_with("cli.") {
            return true;
        } else {
            false
        };
        if matched {
            self.status = UiMessage::raw(error.to_string());
            self.workbench_cli_error(UiMessage::raw(error.to_string()));
            if matches!(tag, "cli.start" | "execution.get")
                && let Some(record) = self.workbench.cli_history.last_mut()
            {
                record.state = UiMessage::key("失败");
                record.elapsed = self.workbench.cli_elapsed;
                record.error = self.workbench.cli_error.clone();
            }
            if tag.starts_with("cli.page/") {
                let selected = self.workbench.cli_selected;
                if let Some(record) = self
                    .workbench
                    .cli_history
                    .iter_mut()
                    .find(|record| Some(record.id) == selected)
                {
                    record.error.push_line(UiMessage::raw(error.to_string()));
                }
            }
        }
        matched
    }

    // 选中的结果只驻留 cli_grid；切换前移回所属记录，避免复制大值。
    pub(super) fn workbench_cli_store_grid(&mut self) {
        let Some(grid) = self.workbench.cli_grid.take() else {
            return;
        };
        let result_id = grid.page.result.id.to_string();
        if let Some(selected) = self.workbench.cli_selected
            && let Some(record) = self
                .workbench
                .cli_history
                .iter_mut()
                .find(|record| record.id == selected)
            && let Some(index) = record.results.iter().position(|id| id == &result_id)
            && let Some(slot) = record.result_grids.get_mut(index)
        {
            *slot = Some(grid);
            if let Some(history) = record.result_page_histories.get_mut(index) {
                *history = std::mem::take(&mut self.workbench.cli_page_history);
            }
        }
    }

    pub(super) fn workbench_cli_preserve_pending_page(&mut self, backend: &Backend) {
        if self.workbench.cli_page_token.is_some()
            && let Some(selected) = self.workbench.cli_selected
            && let Some(result_id) = self.workbench.cli_result_id.clone()
            && let Some(index) = self
                .workbench
                .cli_history
                .iter()
                .find(|record| record.id == selected)
                .and_then(|record| record.results.iter().position(|id| id == &result_id))
        {
            self.request(
                backend,
                &format!("cli.record/{selected}/{index}"),
                "result.page",
                json!({"result_id":result_id,"start":0,"max_rows":if self.profile.as_ref().is_some_and(|p| p.is_archery()) { self.result_page_size(&result_id) } else { 200 },"max_bytes":1048576}),
            );
        }
    }

    pub(crate) fn workbench_cli_page(&mut self, backend: &Backend, start: u64) -> bool {
        let Some(id) = self.workbench.cli_result_id.clone() else {
            return false;
        };
        let archery = self
            .profile
            .as_ref()
            .is_some_and(|profile| profile.is_archery());
        let size = if archery {
            self.result_page_size(&id)
        } else {
            200
        };
        if archery && !self.prepare_result_page(backend, &id, start) {
            return true;
        }
        self.workbench_clear_cli_value();
        self.workbench.cli_page_transition = None;
        let token = Uuid::new_v4();
        let tag = format!(
            "{}:{}:cli.page/{token}",
            self.tabs[self.active].id, self.generation
        );
        match backend.request(
            tag,
            "result.page",
            json!({"result_id":id,"start":start,"max_rows":size,"max_bytes":1048576}),
        ) {
            Ok(()) => {
                self.workbench.cli_page_token = Some(token);
                true
            }
            Err(error) => {
                self.workbench.cli_page_token = None;
                self.status = UiMessage::raw(error.to_string());
                false
            }
        }
    }

    /// CLI 结果有独立分页状态，不能替换当前 key 的结果与编辑基线。
    pub(crate) fn workbench_response(
        &mut self,
        tag: &str,
        value: Value,
        backend: &Backend,
    ) -> bool {
        if tag.starts_with("cli.value/") {
            let Some(mut load) = self.workbench.cli_large.take() else {
                return true;
            };
            let expected = self
                .workbench
                .cli_value_token
                .map(|token| format!("cli.value/{}/{}/{token}", load.id, load.offset));
            if expected.as_deref() != Some(tag) {
                self.workbench.cli_large = Some(load);
                return true;
            }
            self.workbench.cli_value_token = None;
            match load.consume(&value) {
                Ok(None) => {
                    self.workbench.cli_large = Some(load);
                    self.workbench_cli_value(backend);
                }
                Ok(Some(full)) => {
                    let result = self
                        .workbench
                        .cli_grid
                        .as_mut()
                        .ok_or_else(|| UiMessage::key("CLI 结果已关闭"))
                        .and_then(|grid| grid.resolve_large(load.cell, full).map_err(grid_error));
                    if let Err(error) = result {
                        self.workbench_cli_error(error);
                    }
                }
                Err(error) => self.workbench_cli_error(UiMessage::raw(error.message)),
            }
            return true;
        }
        if tag == "cli.start" {
            self.operation_pending = false;
            self.task = value
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            if self.task.is_none() {
                self.workbench.cli_running = false;
                self.workbench.cli_elapsed = self
                    .workbench
                    .cli_started_at
                    .take()
                    .map(|start| start.elapsed());
                self.workbench.cli_error = UiMessage::key("CLI 响应缺少任务 ID");
                if let Some(record) = self.workbench.cli_history.last_mut() {
                    record.state = UiMessage::key("失败");
                    record.elapsed = self.workbench.cli_elapsed;
                    record.error = self.workbench.cli_error.clone();
                }
            } else {
                self.status = UiMessage::key("Redis 命令已提交");
                self.last_poll = Instant::now() - Duration::from_secs(2);
            }
            return true;
        }
        if tag == "execution.get" && self.workbench.cli_running {
            if let Some(db) = value.pointer("/context/redis_db").and_then(Value::as_u64)
                && let Some(record) = self.workbench.cli_history.last_mut()
            {
                record.database = db.to_string();
            }
            let state = value.get("state").and_then(Value::as_str).unwrap_or("");
            if matches!(state, "queued" | "running" | "cancelling") {
                return true;
            }
            // 仅最终确认的命令连接 DB 更新输入目标；缺字段可能是尚未启动的取消。
            if let Some(db) = value.pointer("/summary/command_db") {
                self.workbench.cli_database =
                    Some(db.as_u64().and_then(|db| u32::try_from(db).ok()));
            }
            self.task = None;
            self.workbench.cli_running = false;
            self.workbench.cli_elapsed = self
                .workbench
                .cli_started_at
                .take()
                .map(|start| start.elapsed());
            let state_label = match state {
                "succeeded" => UiArg::Key("已完成"),
                "failed" => UiArg::Key("失败"),
                "cancelled" => UiArg::Key("已取消"),
                "wait_stopped" => UiArg::Key("已结束等待，远端完成状态未知"),
                "outcome_unknown" => UiArg::Key("结果待确认"),
                _ => UiArg::Raw(state.to_owned()),
            };
            self.status = UiMessage::template(
                "Redis 命令任务 {state}",
                vec![("state", state_label.clone())],
            );
            self.workbench.cli_error.clear();
            if state != "succeeded" {
                self.workbench.cli_error = value
                    .get("error")
                    .filter(|v| !v.is_null())
                    .map(|v| {
                        UiMessage::raw(
                            v.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| v.to_string()),
                        )
                    })
                    .unwrap_or_else(|| {
                        UiMessage::template("命令任务 {state}", vec![("state", state_label)])
                    });
            }
            if let Some(record) = self.workbench.cli_history.last_mut() {
                record.state = match state {
                    "succeeded" => UiMessage::key("已完成"),
                    "failed" => UiMessage::key("失败"),
                    "cancelled" => UiMessage::key("已取消"),
                    "wait_stopped" => UiMessage::key("已结束等待，远端完成状态未知"),
                    "outcome_unknown" => UiMessage::key("结果待确认"),
                    _ => UiMessage::raw(state.to_owned()),
                };
                record.elapsed = self.workbench.cli_elapsed;
                record.error = self.workbench.cli_error.clone();
            }
            let results: Vec<String> = value
                .get("results")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            if let Some(items) = value
                .pointer("/summary/redis_results")
                .and_then(Value::as_array)
            {
                for item in items {
                    if let (Some(id), Some(db)) = (
                        item.get("result_id").and_then(Value::as_str),
                        item.get("redis_db")
                            .and_then(Value::as_u64)
                            .and_then(|db| u32::try_from(db).ok()),
                    ) {
                        self.workbench
                            .cli_result_databases
                            .insert(id.to_owned(), db);
                    }
                }
            }
            if let Some(record) = self.workbench.cli_history.last_mut() {
                let record_id = record.id;
                record.results = results.clone();
                record.result_grids = vec![None; results.len()];
                record.result_page_histories = vec![Vec::new(); results.len()];
                let selected = self.workbench.cli_selected == Some(record_id);
                if selected {
                    self.workbench.cli_results = results.clone();
                    self.workbench.cli_result_id = results.first().cloned();
                    self.workbench.cli_page_history.clear();
                    self.workbench_cli_page(backend, 0);
                }
                for (index, id) in results
                    .iter()
                    .enumerate()
                    .skip(if selected { 1 } else { 0 })
                {
                    self.request(
                        backend,
                        &format!("cli.record/{record_id}/{index}"),
                        "result.page",
                        json!({"result_id":id,"start":0,"max_rows":if self.profile.as_ref().is_some_and(|p| p.is_archery()) { 20 } else { 200 },"max_bytes":1048576}),
                    );
                }
            }
            self.archery_auth_required(
                value["error"]["code"].as_str(),
                &value["error"]["details"],
                backend,
            );
            return true;
        }
        if let Some(target) = tag.strip_prefix("cli.record/") {
            if let Some((id, index)) = target.split_once('/')
                && let (Ok(id), Ok(index)) = (Uuid::parse_str(id), index.parse::<usize>())
                && let Some(record) = self
                    .workbench
                    .cli_history
                    .iter_mut()
                    .find(|record| record.id == id)
            {
                match serde_json::from_value::<ResultPage>(value) {
                    Ok(page)
                        if record
                            .results
                            .get(index)
                            .is_some_and(|expected| *expected == page.result.id.to_string()) =>
                    {
                        let selected = self.workbench.cli_selected == Some(id)
                            && self.workbench.cli_result_id.as_deref()
                                == record.results.get(index).map(String::as_str);
                        if selected {
                            if self.workbench.cli_grid.is_none()
                                && self.workbench.cli_page_token.is_none()
                            {
                                self.workbench.cli_grid = Some(GridState::new(page, false));
                            }
                        } else if let Some(grid) = record.result_grids.get_mut(index)
                            && grid.is_none()
                        {
                            *grid = Some(GridState::new(page, false));
                        }
                    }
                    Ok(_) => record
                        .error
                        .push_line(UiMessage::key("CLI 结果页与当前结果不匹配")),
                    Err(error) => record.error.push_line(UiMessage::template(
                        "结果加载失败：{error}",
                        vec![("error", UiArg::Raw(error.to_string()))],
                    )),
                }
            }
            return true;
        }
        if let Some(token) = tag.strip_prefix("cli.page/") {
            if self.workbench.cli_page_token.is_none()
                || Uuid::parse_str(token).ok() != self.workbench.cli_page_token
            {
                return true;
            }
            self.workbench.cli_page_token = None;
            let transition = self.workbench.cli_page_transition.take();
            match serde_json::from_value::<ResultPage>(value) {
                Ok(page)
                    if self.workbench.cli_result_id.as_deref()
                        == Some(page.result.id.to_string().as_str()) =>
                {
                    self.refresh_changed_result_plan(backend, &page);
                    if let Some((forward, start)) = transition {
                        if forward {
                            self.workbench.cli_page_history.push(start);
                        } else {
                            self.workbench.cli_page_history.pop();
                        }
                    }
                    self.workbench.cli_grid = Some(GridState::new(page, false));
                }
                Ok(_) => self.workbench_cli_error(UiMessage::key("CLI 结果页与当前结果不匹配")),
                Err(error) => self.workbench_cli_error(UiMessage::template(
                    "CLI 结果页无效: {error}",
                    vec![("error", UiArg::Raw(error.to_string()))],
                )),
            }
            return true;
        }
        false
    }
}
