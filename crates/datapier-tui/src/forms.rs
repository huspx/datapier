use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone, PartialEq, Eq)]
pub struct Field {
    pub key: String,
    pub zh: String,
    pub en: String,
    pub value: String,
    pub secret: bool,
    pub choices: Vec<String>,
}
impl Field {
    fn new(key: &str, zh: &str, en: &str, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            zh: zh.into(),
            en: en.into(),
            value: value.into(),
            secret: false,
            choices: vec![],
        }
    }
    fn secret(mut self) -> Self {
        self.secret = true;
        self
    }
    fn choices(mut self, choices: &[&str]) -> Self {
        self.choices = choices.iter().map(|v| (*v).into()).collect();
        self
    }
    fn choice_label(&self, english: bool) -> &str {
        if self.choices.is_empty() {
            return &self.value;
        }
        let (zh, en) = match (self.key.as_str(), self.value.as_str()) {
            ("language", "system") => ("跟随系统", "Follow system"),
            ("language", "zh-CN") => ("简体中文", "Simplified Chinese"),
            ("language", "en") => ("English", "English"),
            ("external_editor.mode", "disabled") => ("未配置", "Not configured"),
            (_, "mysql") => ("MySQL", "MySQL"),
            (_, "redis") => ("Redis", "Redis"),
            (_, "false") => ("关闭", "Off"),
            (_, "true") => ("开启", "On"),
            (_, "read_only") => ("只读", "Read only"),
            (_, "read_write") => ("读写", "Read/write"),
            (_, "disabled") => ("禁用", "Disabled"),
            (_, "off") => ("关闭审批", "Approval off"),
            (_, "dangerous") => ("危险操作需审批", "Approve dangerous operations"),
            (_, "all_writes") => ("所有写入需审批", "Approve all writes"),
            (_, "keep") => ("保留", "Keep existing"),
            (_, "keyring") => ("系统凭据库", "System keyring"),
            (_, "session") => ("仅本次会话", "This session only"),
            (_, "clear") => ("清除", "Clear"),
            (_, "manual") => ("手动输入", "Manual"),
            (_, "automatic") => ("自动生成", "Automatic"),
            (_, "gui") => ("图形界面", "GUI"),
            (_, "terminal") => ("终端", "Terminal"),
            _ => return &self.value,
        };
        if english { en } else { zh }
    }
    pub fn display(&self, english: bool) -> String {
        let value = if self.secret {
            std::borrow::Cow::Owned("•".repeat(self.value.chars().count()))
        } else {
            std::borrow::Cow::Borrowed(self.choice_label(english))
        };
        format!("{}: {value}", if english { &self.en } else { &self.zh })
    }
}
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Form {
    pub fields: Vec<Field>,
}
impl Form {
    pub fn get(&self, key: &str) -> &str {
        self.fields
            .iter()
            .find(|f| f.key == key)
            .map(|f| f.value.as_str())
            .unwrap_or("")
    }
    pub fn take(&mut self, key: &str) -> String {
        self.fields
            .iter_mut()
            .find(|f| f.key == key)
            .map(|f| std::mem::take(&mut f.value))
            .unwrap_or_default()
    }
    pub fn edit(&mut self, index: usize, code: crossterm::event::KeyCode) {
        if let Some(f) = self.fields.get_mut(index) {
            match code {
                crossterm::event::KeyCode::Char(' ') | crossterm::event::KeyCode::Enter
                    if !f.choices.is_empty() =>
                {
                    let n = f.choices.iter().position(|c| c == &f.value).unwrap_or(0);
                    f.value = f.choices[(n + 1) % f.choices.len()].clone();
                }
                crossterm::event::KeyCode::Char(c) if f.choices.is_empty() => f.value.push(c),
                crossterm::event::KeyCode::Backspace if f.choices.is_empty() => {
                    f.value.pop();
                }
                crossterm::event::KeyCode::Delete if f.choices.is_empty() => f.value.clear(),
                _ => {}
            }
        }
    }
}
fn val(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        _ => v.to_string(),
    }
}
fn field(key: &str, zh: &str, en: &str, v: &Value) -> Field {
    Field::new(key, zh, en, val(v))
}

pub fn direct(profile: Option<&Value>, default_policy: &Value) -> Form {
    let p = profile.cloned().unwrap_or_else(|| json!({"id":Uuid::new_v4(),"revision":1,"name":"","provider":"mysql","access_mode":"read_only","approval_policy":default_policy.as_str().unwrap_or("dangerous"),"connection":{"kind":"direct","host":"localhost","port":3306,"username":"","database":null,"visible_databases":null,"redis_db":0,"redis_databases_override":null,"tls":{"enabled":false,"ca_path":null,"client_cert_path":null,"client_key_path":null}}}));
    let c = &p["connection"];
    let tls = &c["tls"];
    let mysql = &c["mysql"];
    let timezone = mysql
        .get("timezone")
        .cloned()
        .unwrap_or_else(|| json!("+00:00"));
    let visible = match &c["visible_databases"] {
        Value::Null => "*".into(),
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(","),
        _ => String::new(),
    };
    Form {
        fields: vec![
            field(
                "id",
                "连接 ID（只读）",
                "Connection ID (read only)",
                &p["id"],
            ),
            field(
                "revision",
                "修订（只读）",
                "Revision (read only)",
                &p["revision"],
            ),
            field("name", "名称", "Name", &p["name"]),
            field("provider", "类型", "Provider", &p["provider"]).choices(&["mysql", "redis"]),
            field("host", "主机", "Host", &c["host"]),
            field("port", "端口", "Port", &c["port"]),
            field("username", "用户名", "Username", &c["username"]),
            Field::new("password", "密码（留空保留）", "Password (empty keeps)", "").secret(),
            Field::new("credential", "凭据操作", "Credential action", "keep")
                .choices(&["keep", "keyring", "session", "clear"]),
            field("database", "默认数据库", "Default database", &c["database"]),
            field(
                "mysql_timezone",
                "时区（SYSTEM、±HH:MM 或时区名称；空为服务端默认）",
                "Time zone (SYSTEM, ±HH:MM or name; empty uses server default)",
                &timezone,
            ),
            field(
                "mysql_connect_timeout_seconds",
                "连接超时秒数（1–86400；空继承全局）",
                "Connect timeout seconds (1–86400; empty inherits global)",
                &mysql["connect_timeout_seconds"],
            ),
            field(
                "mysql_query_timeout_seconds",
                "执行超时秒数（1–86400；空继承全局）",
                "Execution timeout seconds (1–86400; empty inherits global)",
                &mysql["query_timeout_seconds"],
            ),
            Field::new(
                "visible",
                "可见 MySQL 库（* 全部，空为无，逗号分隔）",
                "Visible MySQL DBs (* all, empty none, comma separated)",
                visible,
            ),
            field(
                "redis_db",
                "默认 Redis DB",
                "Default Redis DB",
                &c["redis_db"],
            ),
            field(
                "redis_override",
                "Redis DB 数量覆盖（空为自动）",
                "Redis DB count override (empty auto)",
                &c["redis_databases_override"],
            ),
            field("tls", "TLS", "TLS", &tls["enabled"]).choices(&["false", "true"]),
            field("ca", "CA 路径", "CA path", &tls["ca_path"]),
            field(
                "cert",
                "客户端证书路径",
                "Client certificate path",
                &tls["client_cert_path"],
            ),
            field(
                "key",
                "客户端私钥路径",
                "Client key path",
                &tls["client_key_path"],
            ),
            field("access", "访问模式", "Access mode", &p["access_mode"])
                .choices(&["read_only", "read_write"]),
            field(
                "approval",
                "危险操作策略",
                "Approval policy",
                &p["approval_policy"],
            )
            .choices(&["off", "dangerous", "all_writes"]),
        ],
    }
}
pub fn direct_value(f: &Form) -> Result<Value, String> {
    let number = |key: &str| {
        f.get(key)
            .parse::<u64>()
            .map_err(|_| format!("{}: invalid number", key))
    };
    let optional = |key: &str| {
        if f.get(key).is_empty() {
            Value::Null
        } else {
            json!(f.get(key))
        }
    };
    let visible = if f.get("visible") == "*" {
        Value::Null
    } else {
        json!(
            f.get("visible")
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        )
    };
    let override_count = if f.get("redis_override").is_empty() {
        Value::Null
    } else {
        json!(number("redis_override")?)
    };
    let timeout = |key: &str| -> Result<Value, String> {
        if f.get(key).is_empty() {
            return Ok(Value::Null);
        }
        match f.get(key).parse::<u64>() {
            Ok(value) if f.get("provider") != "mysql" || (1..=86400).contains(&value) => {
                Ok(json!(value))
            }
            _ if f.get("provider") != "mysql" => Ok(Value::Null),
            _ => Err(format!("{}: expected 1–86400 seconds", key)),
        }
    };
    if f.get("provider") == "mysql" {
        datapier_core::model::MysqlConnectionOptions {
            timezone: (!f.get("mysql_timezone").is_empty())
                .then(|| f.get("mysql_timezone").to_owned()),
            ..Default::default()
        }
        .validate()
        .map_err(|error| format!("mysql_timezone: {error}"))?;
    }
    Ok(
        json!({"id":f.get("id"),"revision":number("revision")?,"name":f.get("name"),"provider":f.get("provider"),"access_mode":f.get("access"),"approval_policy":f.get("approval"),"connection":{"kind":"direct","host":f.get("host"),"port":number("port")?,"username":f.get("username"),"database":optional("database"),"visible_databases":visible,"redis_db":number("redis_db")?,"redis_databases_override":override_count,"mysql":{"timezone":optional("mysql_timezone"),"connect_timeout_seconds":timeout("mysql_connect_timeout_seconds")?,"query_timeout_seconds":timeout("mysql_query_timeout_seconds")?},"tls":{"enabled":f.get("tls")=="true","ca_path":optional("ca"),"client_cert_path":optional("cert"),"client_key_path":optional("key")}}}),
    )
}

pub fn server(server: Option<&Value>) -> Form {
    let s = server.cloned().unwrap_or_else(|| json!({"id":Uuid::new_v4(),"revision":0,"name":"","base_url":"","username":"","http_tls":{"ca_path":null},"totp_mode":"manual"}));
    Form {
        fields: vec![
            field("id", "服务 ID（只读）", "Server ID (read only)", &s["id"]),
            field(
                "revision",
                "修订（只读）",
                "Revision (read only)",
                &s["revision"],
            ),
            field("name", "服务名称", "Server name", &s["name"]),
            field("base_url", "服务地址", "Server URL", &s["base_url"]),
            field("username", "用户名", "Username", &s["username"]),
            field("ca", "CA 路径", "CA path", &s["http_tls"]["ca_path"]),
            field("totp_mode", "OTP 模式", "OTP mode", &s["totp_mode"])
                .choices(&["manual", "automatic"]),
            Field::new("password", "密码", "Password", "").secret(),
            Field::new("password_action", "密码操作", "Password action", "keep")
                .choices(&["keep", "keyring", "session", "clear"]),
            Field::new("totp", "TOTP URI", "TOTP URI", "").secret(),
            Field::new("totp_action", "TOTP 操作", "TOTP action", "keep")
                .choices(&["keep", "keyring", "session", "clear"]),
            Field::new("otp", "一次性验证码", "One-time code", "").secret(),
        ],
    }
}
pub fn server_value(f: &Form) -> Value {
    json!({"id":f.get("id"),"revision":f.get("revision").parse::<u64>().unwrap_or(0),"name":f.get("name"),"base_url":f.get("base_url"),"username":f.get("username"),"http_tls":{"ca_path":if f.get("ca").is_empty(){Value::Null}else{json!(f.get("ca"))}},"totp_mode":f.get("totp_mode")})
}
pub fn server_credential_updates(form: &Form) -> Result<Value, &'static str> {
    let update = |action: &str, value: &str| match action {
        "clear" => Ok(json!({"kind":"clear"})),
        "keyring" | "session" if value.is_empty() => {
            Err("Enter the password or TOTP URI selected for saving")
        }
        "keyring" | "session" => {
            Ok(json!({"kind":"replace","value":value,"persist":action == "keyring"}))
        }
        _ => Ok(json!({"kind":"keep"})),
    };
    Ok(json!({
        "password_update": update(form.get("password_action"), form.get("password"))?,
        "totp_update": update(form.get("totp_action"), form.get("totp"))?,
    }))
}

pub fn credentials_match_login(updates: &Value, staged: Option<&Value>) -> bool {
    match staged {
        Some(staged) => staged == updates,
        None => {
            updates["password_update"]["kind"] == "keep" && updates["totp_update"]["kind"] == "keep"
        }
    }
}

pub fn visible_tail(text: &str, width: usize) -> String {
    if ratatui::text::Span::raw(text).width() <= width {
        return text.to_owned();
    }
    if width == 0 {
        return String::new();
    }
    let mut start = text.len();
    let mut used = 1;
    for (index, ch) in text.char_indices().rev() {
        let mut buf = [0; 4];
        let char_width = ratatui::text::Span::raw(ch.encode_utf8(&mut buf) as &str).width();
        if used + char_width > width {
            break;
        }
        used += char_width;
        start = index;
    }
    format!("…{}", &text[start..])
}

pub fn settings_group(key: &str, english: bool) -> &'static str {
    let (zh, en) = match key.split('.').next().unwrap_or("") {
        "language" => ("常规", "General"),
        "mcp" => ("MCP 权限", "MCP access"),
        "paging" => ("分页", "Paging"),
        "archery" => ("Archery", "Archery"),
        "limits" => ("资源限制", "Resource limits"),
        "approval" => ("审批", "Approval"),
        "notifications" => ("通知", "Notifications"),
        "history" => ("历史", "History"),
        "editing" => ("编辑限制", "Editing limits"),
        "external_editor" => ("外部编辑器", "External editor"),
        _ => ("设置", "Settings"),
    };
    if english { en } else { zh }
}
pub fn settings_hint(key: &str, english: bool) -> &'static str {
    let (zh, en) = match key {
        "paging.mysql_table_rows"
        | "paging.sql_rows"
        | "paging.redis_value_rows"
        | "paging.history_rows"
        | "paging.archery_grid_rows"
        | "limits.mcp_response_rows" => ("1–1000 行", "1–1000 rows"),
        "limits.response_bytes" => (
            "1024–1048576 字节，≤客户端结果内存",
            "1024–1048576 bytes, ≤ client result memory",
        ),
        "limits.value_preview_bytes" => ("≥1 字节，≤响应字节", "≥1 byte, ≤ response bytes"),
        "limits.result_memory_bytes_per_client" => {
            ("≥1 字节，≥响应字节", "≥1 byte, ≥ response bytes")
        }
        "editing.max_text_editor_bytes" => ("≥1 字节，≤最大单值", "≥1 byte, ≤ maximum value"),
        "editing.max_plan_bytes" | "editing.max_value_bytes" => ("≥1 字节", "≥1 byte"),
        "limits.connect_timeout_seconds"
        | "limits.query_timeout_seconds"
        | "limits.count_timeout_seconds"
        | "limits.result_idle_seconds"
        | "approval.ttl_seconds" => ("≥1 秒", "≥1 second"),
        "history.retention_days" => ("≥1 天，空为不限制", "≥1 day, empty is unlimited"),
        "history.max_entries" => ("≥1 条，空为不限制", "≥1 entry, empty is unlimited"),
        "paging.redis_key_batch"
        | "archery.request_rows"
        | "limits.running_tasks_per_mcp_client"
        | "editing.max_plan_items"
        | "editing.preview_rows" => ("≥1", "≥1"),
        _ => ("", ""),
    };
    if english { en } else { zh }
}
pub fn settings(settings: &Value) -> Form {
    let specs = [
        ("language", "语言", "Language"),
        (
            "mcp.fine_grained_access",
            "MCP 细粒度权限",
            "MCP fine-grained access",
        ),
        (
            "paging.mysql_table_rows",
            "MySQL 表页大小",
            "MySQL table page rows",
        ),
        ("paging.sql_rows", "SQL 页大小", "SQL page rows"),
        (
            "paging.redis_value_rows",
            "Redis 值页大小",
            "Redis value page rows",
        ),
        ("paging.history_rows", "历史页大小", "History page rows"),
        (
            "paging.archery_grid_rows",
            "Archery 页大小",
            "Archery page rows",
        ),
        (
            "paging.redis_key_batch",
            "Redis key 批大小",
            "Redis key batch",
        ),
        (
            "archery.request_rows",
            "Archery 请求行数",
            "Archery request rows",
        ),
        (
            "limits.mcp_response_rows",
            "MCP 响应行数",
            "MCP response rows",
        ),
        ("limits.response_bytes", "响应字节数", "Response bytes"),
        (
            "limits.value_preview_bytes",
            "值预览字节数",
            "Value preview bytes",
        ),
        (
            "limits.connect_timeout_seconds",
            "连接超时（秒）",
            "Connect timeout seconds",
        ),
        (
            "limits.query_timeout_seconds",
            "执行超时（秒）",
            "Execution timeout seconds",
        ),
        (
            "limits.count_timeout_seconds",
            "统计超时（秒）",
            "Count timeout seconds",
        ),
        (
            "limits.result_idle_seconds",
            "结果空闲时间（秒）",
            "Result idle seconds",
        ),
        (
            "limits.result_memory_bytes_per_client",
            "客户端结果内存字节数",
            "Result memory bytes per client",
        ),
        (
            "limits.running_tasks_per_mcp_client",
            "MCP 并行任务数",
            "Running tasks per MCP client",
        ),
        (
            "approval.ttl_seconds",
            "审批有效期（秒）",
            "Approval TTL seconds",
        ),
        (
            "approval.default_policy",
            "默认审批策略",
            "Default approval policy",
        ),
        (
            "notifications.approval",
            "审批通知",
            "Approval notifications",
        ),
        (
            "notifications.authentication",
            "认证通知",
            "Authentication notifications",
        ),
        (
            "history.auto_cleanup_enabled",
            "历史自动清理",
            "Automatic history cleanup",
        ),
        (
            "history.retention_days",
            "历史保留天数（空为不限制）",
            "History retention days (empty unlimited)",
        ),
        (
            "history.max_entries",
            "历史最大条数（空为不限制）",
            "History maximum entries (empty unlimited)",
        ),
        (
            "editing.max_plan_items",
            "最大计划条目",
            "Maximum plan items",
        ),
        (
            "editing.max_plan_bytes",
            "最大计划字节",
            "Maximum plan bytes",
        ),
        (
            "editing.max_value_bytes",
            "最大单值字节",
            "Maximum value bytes",
        ),
        (
            "editing.max_text_editor_bytes",
            "文本编辑器字节上限",
            "Text editor byte limit",
        ),
        ("editing.preview_rows", "预览行数", "Preview rows"),
        (
            "external_editor.mode",
            "外部编辑器模式",
            "External editor mode",
        ),
        (
            "external_editor.program",
            "外部编辑器程序",
            "External editor program",
        ),
        (
            "external_editor.args",
            "参数（JSON 数组，包含 {file}）",
            "Arguments (JSON array containing {file})",
        ),
    ];
    Form {
        fields: specs
            .into_iter()
            .map(|(path, zh, en)| {
                let v = path.split('.').fold(settings, |v, k| &v[k]);
                let f = field(path, zh, en, v);
                match path {
                    "language" => f.choices(&["system", "zh-CN", "en"]),
                    "external_editor.mode" => {
                        let mut f = f;
                        if v.is_null() {
                            f.value = "disabled".into();
                        }
                        f.choices(&["disabled", "gui", "terminal"])
                    }
                    "external_editor.args" if v.is_null() => {
                        Field::new(path, zh, en, "[\"{file}\"]")
                    }
                    "approval.default_policy" => f.choices(&["dangerous", "all_writes"]),
                    _ if v.is_boolean() => f.choices(&["false", "true"]),
                    _ => f,
                }
            })
            .collect(),
    }
}
pub fn settings_value(f: &Form, original: &Value) -> Result<Value, (String, String)> {
    let mut out = original.clone();
    for field in &f.fields {
        if field.key.starts_with("external_editor.") {
            continue;
        }
        let mut parent = &mut out;
        let parts: Vec<_> = field.key.split('.').collect();
        for part in &parts[..parts.len() - 1] {
            parent = &mut parent[*part];
        }
        let key = parts[parts.len() - 1];
        let old = &parent[key];
        let value = if old.is_boolean() {
            json!(field.value == "true")
        } else if old.is_number() || old.is_null() {
            if field.value.is_empty()
                && matches!(
                    field.key.as_str(),
                    "history.retention_days" | "history.max_entries"
                )
            {
                Value::Null
            } else {
                json!(field.value.parse::<u64>().map_err(|_| (
                    field.key.clone(),
                    "请输入有效的非负整数 / Enter a valid non-negative integer".into()
                ))?)
            }
        } else {
            json!(field.value)
        };
        if value.is_number() {
            let n = value.as_u64().unwrap_or(0);
            let max = match field.key.as_str() {
                "paging.mysql_table_rows"
                | "paging.sql_rows"
                | "paging.redis_value_rows"
                | "paging.history_rows"
                | "paging.archery_grid_rows"
                | "limits.mcp_response_rows" => Some(1000),
                "limits.response_bytes" => Some(1_048_576),
                _ => None,
            };
            if n == 0
                || (field.key == "limits.response_bytes" && n < 1024)
                || max.is_some_and(|max| n > max)
            {
                return Err((
                    field.key.clone(),
                    "数值超出允许范围 / Value is out of range".into(),
                ));
            }
        }
        parent[key] = value;
    }
    out["external_editor"] = match f.get("external_editor.mode") {
        "disabled" => Value::Null,
        mode @ ("gui" | "terminal") => {
            let args: Vec<String> = serde_json::from_str(f.get("external_editor.args"))
                .map_err(|error| ("external_editor.args".into(), format!("外部编辑器参数必须是字符串数组 / Arguments must be a string array: {error}")))?;
            if f.get("external_editor.program").trim().is_empty()
                || f.get("external_editor.program").contains('\0')
            {
                return Err((
                    "external_editor.program".into(),
                    "程序不能为空且不能含 NUL / Program must be nonempty and contain no NUL".into(),
                ));
            }
            if args.iter().any(|arg| arg.contains('\0'))
                || args
                    .iter()
                    .map(|arg| arg.matches("{file}").count())
                    .sum::<usize>()
                    != 1
            {
                return Err(("external_editor.args".into(), "参数不能含 NUL，且必须恰含一个 {file} / Arguments must contain exactly one {file} and no NUL".into()));
            }
            json!({"mode":mode,"program":f.get("external_editor.program"),"args":args})
        }
        _ => {
            return Err((
                "external_editor.mode".into(),
                "外部编辑器模式无效 / Invalid editor mode".into(),
            ));
        }
    };
    for (key, other) in [
        ("limits.value_preview_bytes", "limits.response_bytes"),
        (
            "limits.response_bytes",
            "limits.result_memory_bytes_per_client",
        ),
        ("editing.max_text_editor_bytes", "editing.max_value_bytes"),
    ] {
        let get = |path: &str| {
            path.split('.')
                .fold(&out, |value, part| &value[part])
                .as_u64()
                .unwrap_or(0)
        };
        if get(key) > get(other) {
            return Err((key.into(), format!("不能大于 / Must not exceed {other}")));
        }
    }
    Ok(out)
}
pub fn rules(rules: &[Value], profile: &Value) -> Form {
    let redis = profile["provider"] == "redis";
    let mut names: Vec<String> = if redis {
        vec!["*".into()]
    } else {
        profile["connection"]["visible_databases"]
            .as_array()
            .or_else(|| profile["connection"]["databases"].as_array())
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    for r in rules {
        if let Some(key) = r["scope_key"].as_str()
            && !names.iter().any(|s| s == key)
        {
            names.push(key.into());
        }
    }
    if !redis {
        names.push(String::new());
    }
    Form {
        fields: names
            .into_iter()
            .map(|name| {
                if name.is_empty() {
                    return Field::new(
                        "__add",
                        "新增数据库规则（输入名称，F4 添加）",
                        "Add database rule (type name, F4)",
                        "",
                    );
                }
                let level = rules
                    .iter()
                    .find(|r| r["scope_key"] == name)
                    .and_then(|r| r["level"].as_str())
                    .unwrap_or("disabled");
                Field::new(
                    &name,
                    &format!("范围 {name}"),
                    &format!("Scope {name}"),
                    level,
                )
                .choices(&["disabled", "read_only", "read_write"])
            })
            .collect(),
    }
}
pub fn add_rule(form: &mut Form) {
    let name = form.take("__add");
    let name = name.trim();
    if !name.is_empty() && !form.fields.iter().any(|f| f.key == name) {
        form.fields.push(
            Field::new(
                name,
                &format!("范围 {name}"),
                &format!("Scope {name}"),
                "disabled",
            )
            .choices(&["disabled", "read_only", "read_write"]),
        );
    }
}
pub fn profile_name(value: &str) -> Form {
    Form {
        fields: vec![Field::new("name", "连接名称", "Connection name", value)],
    }
}
