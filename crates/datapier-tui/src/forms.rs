use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Clone)]
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
    pub fn display(&self, english: bool) -> String {
        format!(
            "{}: {}",
            if english { &self.en } else { &self.zh },
            if self.secret {
                "•".repeat(self.value.chars().count())
            } else {
                self.value.clone()
            }
        )
    }
}
#[derive(Default)]
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
            field(
                "database",
                "默认 MySQL 数据库",
                "Default MySQL database",
                &c["database"],
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
    Ok(
        json!({"id":f.get("id"),"revision":number("revision")?,"name":f.get("name"),"provider":f.get("provider"),"access_mode":f.get("access"),"approval_policy":f.get("approval"),"connection":{"kind":"direct","host":f.get("host"),"port":number("port")?,"username":f.get("username"),"database":optional("database"),"visible_databases":visible,"redis_db":number("redis_db")?,"redis_databases_override":override_count,"tls":{"enabled":f.get("tls")=="true","ca_path":optional("ca"),"client_cert_path":optional("cert"),"client_key_path":optional("key")}}}),
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
            "查询超时（秒）",
            "Query timeout seconds",
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
    ];
    Form {
        fields: specs
            .into_iter()
            .map(|(path, zh, en)| {
                let v = path.split('.').fold(settings, |v, k| &v[k]);
                let f = field(path, zh, en, v);
                match path {
                    "language" => f.choices(&["system", "zh-CN", "en"]),
                    "approval.default_policy" => f.choices(&["dangerous", "all_writes"]),
                    _ if v.is_boolean() => f.choices(&["false", "true"]),
                    _ => f,
                }
            })
            .collect(),
    }
}
pub fn settings_value(f: &Form, original: &Value) -> Result<Value, String> {
    let mut out = original.clone();
    for field in &f.fields {
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
                json!(
                    field
                        .value
                        .parse::<u64>()
                        .map_err(|_| format!("{}: invalid number", field.key))?
                )
            }
        } else {
            json!(field.value)
        };
        parent[key] = value;
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
