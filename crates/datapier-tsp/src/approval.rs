use super::*;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum ApprovalTab {
    #[default]
    Approvals,
    Authentication,
    Tasks,
}

#[derive(Default)]
pub(super) struct ApprovalState {
    pub open: bool,
    pub tab: ApprovalTab,
    pub choose_initial_tab: bool,
    pub diagnostics_open: bool,
    pub items: Vec<Value>,
    pub authentication: Vec<Value>,
    pub selected: Option<String>,
    pub detail: Option<Value>,
    pub list_loaded: bool,
    pub list_error: Option<String>,
    pub detail_error: Option<String>,
    pub observe_list_loaded: bool,
    pub observe_list_error: Option<String>,
    pub list_pending: Option<Uuid>,
    pub detail_pending: Option<Uuid>,
    pub decide_pending: Option<Uuid>,
    pub error: Option<String>,
    pub observed_task: Option<Value>,
    pub observed_task_id: Option<String>,
    pub observed_tasks: Vec<Value>,
    pub observe_list_pending: Option<Uuid>,
    pub observed_page: Option<ResultPage>,
    pub observed_result: Option<String>,
    pub observed_size: Option<u64>,
    pub observed_history: Vec<u64>,
    pub observe_task_pending: Option<(Uuid, String)>,
    pub observe_page_pending: Option<(Uuid, String, u64, Option<bool>)>,
    pub observe_error: Option<String>,
}

impl App {
    pub(super) fn approval_refresh(&mut self, backend: &Backend) {
        if self.approvals.observe_list_pending.is_none() {
            let token = Uuid::new_v4();
            match backend.request(
                format!("approval.observe.list/{token}"),
                "execution.observe.list",
                json!({}),
            ) {
                Ok(()) => {
                    self.approvals.observe_list_error = None;
                    self.approvals.observe_list_pending = Some(token);
                }
                Err(error) => self.approvals.observe_list_error = Some(error.to_string()),
            }
        }
        if self.approvals.list_pending.is_some() {
            return;
        }
        let token = Uuid::new_v4();
        match backend.request(format!("approval.list/{token}"), "approval.list", json!({})) {
            Ok(()) => {
                self.approvals.list_error = None;
                self.approvals.list_pending = Some(token);
            }
            Err(error) => self.approvals.list_error = Some(error.to_string()),
        }
    }

    fn approval_load_detail(&mut self, backend: &Backend, id: &str) {
        let token = Uuid::new_v4();
        match backend.request(
            format!("approval.get/{token}/{id}"),
            "approval.get",
            json!({"approval_id":id}),
        ) {
            Ok(()) => {
                self.approvals.detail_error = None;
                self.approvals.detail_pending = Some(token);
            }
            Err(error) => self.approvals.detail_error = Some(error.to_string()),
        }
    }
    fn approval_observe_task(&mut self, backend: &Backend, id: String) {
        if self.approvals.observe_task_pending.is_some() {
            return;
        }
        let token = Uuid::new_v4();
        match backend.request(
            format!("approval.observe.task/{token}"),
            "execution.get",
            json!({"task_id":id}),
        ) {
            Ok(()) => {
                self.approvals.observe_task_pending = Some((token, id));
                self.approvals.observe_error = None;
            }
            Err(error) => self.approvals.observe_error = Some(error.to_string()),
        }
    }

    fn approval_observe_page(&mut self, backend: &Backend, start: u64, transition: Option<bool>) {
        let Some(id) = self.approvals.observed_result.clone() else {
            return;
        };
        if self.approvals.observe_page_pending.is_some() {
            return;
        }
        let size = self
            .approvals
            .observed_size
            .unwrap_or(self.settings.paging.sql_rows as u64);
        let token = Uuid::new_v4();
        match backend.request(format!("approval.observe.page/{token}"), "result.read", json!({"result_id":id,"start":start,"max_rows":size,"max_bytes":self.settings.limits.response_bytes})) {
            Ok(()) => { self.approvals.observe_page_pending = Some((token, id, start, transition)); self.approvals.observe_error = None; }
            Err(error) => self.approvals.observe_error = Some(error.to_string()),
        }
    }

    pub(super) fn approval_action(&mut self, name: &str, backend: &Backend) -> bool {
        match name {
            "approval-toggle" => {
                self.approvals.open = !self.approvals.open;
                if self.approvals.open {
                    self.approvals.choose_initial_tab = true;
                    self.approvals.tab = if self
                        .approvals
                        .items
                        .iter()
                        .any(|item| item["state"].as_str() == Some("pending"))
                    {
                        ApprovalTab::Approvals
                    } else if !self.approvals.authentication.is_empty() {
                        ApprovalTab::Authentication
                    } else {
                        ApprovalTab::Approvals
                    };
                    self.approval_refresh(backend);
                    self.focus = match self.approvals.tab {
                        ApprovalTab::Approvals => "approval-tab-approvals",
                        ApprovalTab::Authentication => "approval-tab-authentication",
                        ApprovalTab::Tasks => "approval-tab-tasks",
                    }
                    .into();
                    self.workbench.focus_request = Some(String::new());
                }
                return true;
            }
            "approval-close" => {
                self.approvals.open = false;
                self.focus = "approval-open".into();
                self.workbench.focus_request = Some(String::new());
                return true;
            }
            "approval-tab-approvals" | "approval-tab-authentication" | "approval-tab-tasks" => {
                self.approvals.choose_initial_tab = false;
                self.approvals.tab = match name {
                    "approval-tab-authentication" => ApprovalTab::Authentication,
                    "approval-tab-tasks" => ApprovalTab::Tasks,
                    _ => ApprovalTab::Approvals,
                };
                self.focus = name.into();
                self.workbench.focus_request = Some(String::new());
                return true;
            }
            "approval-diagnostics-toggle" => {
                self.approvals.diagnostics_open = !self.approvals.diagnostics_open;
                return true;
            }
            "approval-refresh" => {
                self.approval_refresh(backend);
                return true;
            }
            "approval-detail-retry" => {
                if let Some(id) = self.approvals.selected.clone() {
                    self.approval_load_detail(backend, &id);
                }
                return true;
            }
            _ => {}
        }
        if let Some(id) = name.strip_prefix("approval-authenticate:") {
            let server_id = self
                .approvals
                .authentication
                .iter()
                .find(|item| {
                    item["request_id"].as_str() == Some(id)
                        && item["can_authenticate"].as_bool() == Some(true)
                })
                .and_then(|item| item["server_id"].as_str())
                .and_then(|id| Uuid::parse_str(id).ok());
            if let Some(server_id) = server_id {
                if self.workbench.overlay.is_some()
                    || self.archery.open
                    || self.connection_form_busy()
                {
                    self.approvals.error = Some(i18n::tr("请先完成当前操作").into_owned());
                } else if self.finish_inline(true) {
                    self.approvals.open = false;
                    self.archery.open_continuation = None;
                    self.archery.open_capture = None;
                    self.archery_reauthenticate(server_id, backend);
                }
            }
            return true;
        }
        if name == "approval-observe" {
            if let Some(id) = self
                .approvals
                .detail
                .as_ref()
                .and_then(|v| v["execution_id"].as_str())
                .map(str::to_owned)
            {
                self.approvals.tab = ApprovalTab::Tasks;
                self.approvals.choose_initial_tab = false;
                self.approval_action(&format!("approval-observe-task:{id}"), backend);
            }
            return true;
        }
        if let Some(id) = name.strip_prefix("approval-observe-task:") {
            if Uuid::parse_str(id).is_ok() {
                self.approvals.observe_task_pending = None;
                self.approvals.observed_task_id = Some(id.to_owned());
                self.approvals.observed_task = None;
                self.approvals.observed_result = None;
                self.approvals.observed_page = None;
                self.approvals.observed_history.clear();
                self.approvals.observe_page_pending = None;
                self.approval_observe_task(backend, id.to_owned());
            }
            return true;
        }
        if name == "approval-observe-refresh" {
            if let Some(id) = self.approvals.observed_task_id.clone() {
                self.approval_observe_task(backend, id);
            }
            return true;
        }
        if let Some(id) = name.strip_prefix("approval-observe-result:") {
            if self
                .approvals
                .observed_task
                .as_ref()
                .and_then(|v| v["results"].as_array())
                .is_some_and(|results| results.iter().any(|value| value.as_str() == Some(id)))
            {
                self.approvals.observe_page_pending = None;
                self.approvals.observed_result = Some(id.to_owned());
                self.approvals.observed_page = None;
                self.approvals.observed_history.clear();
                self.approval_observe_page(backend, 0, None);
            }
            return true;
        }
        if let Some(size) = name
            .strip_prefix("approval-observe-size:")
            .and_then(|v| v.parse::<u64>().ok())
        {
            if (1..=1000).contains(&size) && self.approvals.observe_page_pending.is_none() {
                self.approvals.observed_size = Some(size);
                self.approvals.observed_history.clear();
                self.workbench.generic_select = None;
                self.approval_observe_page(backend, 0, None);
            }
            return true;
        }
        if name == "approval-observe-next" {
            if let Some(next) = self
                .approvals
                .observed_page
                .as_ref()
                .and_then(|page| page.next.filter(|next| *next < page.result.row_count))
            {
                self.approval_observe_page(backend, next, Some(true));
            }
            return true;
        }
        if name == "approval-observe-prev" {
            if let Some(start) = self.approvals.observed_history.last().copied() {
                self.approval_observe_page(backend, start, Some(false));
            }
            return true;
        }
        if let Some(id) = name.strip_prefix("approval-select:") {
            if Uuid::parse_str(id).is_ok() {
                self.approvals.selected = Some(id.to_owned());
                self.approvals.diagnostics_open = false;
                self.approvals.detail_error = None;
                self.approvals.detail = None;
                self.approvals.observed_task = None;
                self.approvals.observed_task_id = None;
                self.approvals.observed_result = None;
                self.approvals.observed_page = None;
                self.approvals.observed_history.clear();
                self.approvals.observe_task_pending = None;
                self.approvals.observe_page_pending = None;
                self.approvals.observe_error = None;
                self.approvals.error = None;
                self.approval_load_detail(backend, id);
            }
            return true;
        }
        if let Some(args) = name.strip_prefix("approval-decide:") {
            if let Some((id, decision)) = args.split_once(':')
                && matches!(decision, "approve" | "reject")
                && self.approvals.selected.as_deref() == Some(id)
                && self.approvals.decide_pending.is_none()
                && self.approvals.detail_pending.is_none()
                && self.approvals.detail_error.is_none()
                && self.approvals.detail.as_ref().is_some_and(|record| {
                    record["approval_id"].as_str() == Some(id)
                        && record["state"].as_str() == Some("pending")
                        && record["can_decide"].as_bool() == Some(true)
                })
            {
                let token = Uuid::new_v4();
                match backend.request(
                    format!("approval.decide/{token}/{id}"),
                    "approval.decide",
                    json!({"approval_id":id,"decision":decision}),
                ) {
                    Ok(()) => {
                        self.approvals.decide_pending = Some(token);
                        self.approvals.error = None;
                    }
                    Err(error) => self.approvals.error = Some(error.to_string()),
                }
            }
            return true;
        }
        false
    }

    pub(super) fn approval_response(
        &mut self,
        tag: &str,
        result: &datapier_core::Result<Value>,
        backend: &Backend,
    ) -> bool {
        if let Some(token) = tag.strip_prefix("approval.observe.list/") {
            if Uuid::parse_str(token).ok() != self.approvals.observe_list_pending
                || self.approvals.observe_list_pending.is_none()
            {
                return true;
            }
            self.approvals.observe_list_pending = None;
            match result {
                Ok(value) => match value.get("executions").and_then(Value::as_array) {
                    Some(items) => {
                        self.approvals.observed_tasks = items.clone();
                        self.approvals.observe_list_loaded = true;
                        self.approvals.observe_list_error = None;
                    }
                    None => self.approvals.observe_list_error = Some("任务列表响应无效".into()),
                },
                Err(error) => self.approvals.observe_list_error = Some(error.to_string()),
            }
            return true;
        }
        if let Some(token) = tag.strip_prefix("approval.observe.task/") {
            let Some((expected, id)) = self.approvals.observe_task_pending.clone() else {
                return true;
            };
            if Uuid::parse_str(token).ok() != Some(expected) {
                return true;
            }
            self.approvals.observe_task_pending = None;
            if self.approvals.observed_task_id.as_deref() != Some(id.as_str()) {
                return true;
            }
            match result {
                Ok(task) if task["id"].as_str() == Some(id.as_str()) => {
                    let first = task["results"]
                        .as_array()
                        .and_then(|results| results.first())
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    self.approvals.observed_task = Some(task.clone());
                    if self.approvals.observed_result.is_none()
                        && let Some(result_id) = first
                    {
                        self.approvals.observed_result = Some(result_id);
                        self.approval_observe_page(backend, 0, None);
                    }
                }
                Ok(_) => self.approvals.observe_error = Some("任务响应不匹配".into()),
                Err(error) => self.approvals.observe_error = Some(error.to_string()),
            }
            return true;
        }
        if let Some(token) = tag.strip_prefix("approval.observe.page/") {
            let Some((expected, id, start, transition)) =
                self.approvals.observe_page_pending.clone()
            else {
                return true;
            };
            if Uuid::parse_str(token).ok() != Some(expected) {
                return true;
            }
            self.approvals.observe_page_pending = None;
            if self.approvals.observed_result.as_deref() != Some(id.as_str()) {
                return true;
            }
            match result {
                Ok(value) => match serde_json::from_value::<ResultPage>(value.clone()) {
                    Ok(page) if page.result.id.to_string() == id && page.start == start => {
                        if let Some(forward) = transition {
                            if forward {
                                if let Some(current) = &self.approvals.observed_page {
                                    self.approvals.observed_history.push(current.start);
                                }
                            } else {
                                self.approvals.observed_history.pop();
                            }
                        }
                        self.approvals.observed_page = Some(page);
                    }
                    _ => self.approvals.observe_error = Some("结果页响应不匹配".into()),
                },
                Err(error) => self.approvals.observe_error = Some(error.to_string()),
            }
            return true;
        }
        if let Some(token) = tag.strip_prefix("approval.list/") {
            if Uuid::parse_str(token).ok() != self.approvals.list_pending
                || self.approvals.list_pending.is_none()
            {
                return true;
            }
            self.approvals.list_pending = None;
            match result {
                Ok(value) => {
                    if let (Some(items), Some(authentication)) = (
                        value.get("approvals").and_then(Value::as_array),
                        value.get("authentication").and_then(Value::as_array),
                    ) {
                        self.approvals.items = items.clone();
                        self.approvals.authentication = authentication.clone();
                        self.approvals.list_loaded = true;
                        self.approvals.list_error = None;
                        if self.approvals.open && self.approvals.choose_initial_tab {
                            self.approvals.tab = if self
                                .approvals
                                .items
                                .iter()
                                .any(|item| item["state"].as_str() == Some("pending"))
                            {
                                ApprovalTab::Approvals
                            } else if !self.approvals.authentication.is_empty() {
                                ApprovalTab::Authentication
                            } else {
                                ApprovalTab::Approvals
                            };
                            if self.focus.starts_with("approval-tab-") {
                                self.focus = match self.approvals.tab {
                                    ApprovalTab::Approvals => "approval-tab-approvals",
                                    ApprovalTab::Authentication => "approval-tab-authentication",
                                    ApprovalTab::Tasks => "approval-tab-tasks",
                                }
                                .into();
                            }
                            self.approvals.choose_initial_tab = false;
                        }
                        if let Some(id) = self.approvals.selected.clone()
                            && self.approvals.detail_pending.is_none()
                            && self.approvals.decide_pending.is_none()
                        {
                            self.approval_load_detail(backend, &id);
                        }
                    } else {
                        self.approvals.list_error = Some("审批列表响应无效".into());
                    }
                }
                Err(error) => self.approvals.list_error = Some(error.to_string()),
            }
            return true;
        }
        if let Some(rest) = tag.strip_prefix("approval.get/") {
            if let Some((token, id)) = rest.split_once('/')
                && Uuid::parse_str(token).ok() == self.approvals.detail_pending
                && self.approvals.detail_pending.is_some()
            {
                self.approvals.detail_pending = None;
                if self.approvals.selected.as_deref() == Some(id) {
                    match result {
                        Ok(value) if value["approval_id"].as_str() == Some(id) => {
                            self.approvals.detail = Some(value.clone());
                            self.approvals.detail_error = None;
                        }
                        Ok(_) => self.approvals.detail_error = Some("审批详情响应无效".into()),
                        Err(error) => self.approvals.detail_error = Some(error.to_string()),
                    }
                }
            }
            return true;
        }
        if let Some(rest) = tag.strip_prefix("approval.decide/") {
            if let Some((token, id)) = rest.split_once('/')
                && Uuid::parse_str(token).ok() == self.approvals.decide_pending
                && self.approvals.decide_pending.is_some()
            {
                self.approvals.decide_pending = None;
                match result {
                    Ok(_) => {
                        self.approvals.error = None;
                    }
                    Err(error) => self.approvals.error = Some(error.to_string()),
                }
                if self.approvals.selected.as_deref() == Some(id) {
                    self.approval_load_detail(backend, id);
                }
                self.approval_refresh(backend);
            }
            return true;
        }
        false
    }
}
