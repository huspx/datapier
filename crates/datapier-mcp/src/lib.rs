//! MCP stdio 适配层。所有业务操作均由 Engine 在 MCP 身份下授权。
use std::{
    borrow::Cow,
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use datapier_core::{
    Result,
    model::{AppError, ClientKind},
};
use datapier_engine::{Client, ClientOptions};
use rmcp::{
    ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ElicitRequestParams,
        ElicitationAction, ElicitationSchema, ErrorData, Implementation, ListToolsResult,
        PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerConfig, Tool,
        ToolAnnotations,
    },
    service::{RequestContext, RoleServer},
    transport::stdio,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

#[derive(Clone, Copy)]
enum Kind {
    Id,
    Text,
    Bool,
    Positive,
    Offset,
    Target,
    Filters,
    Sort,
    Array,
    Decision,
    Source,
}
#[derive(Clone, Copy)]
struct Field {
    name: &'static str,
    kind: Kind,
    required: bool,
}
const fn f(name: &'static str, kind: Kind, required: bool) -> Field {
    Field {
        name,
        kind,
        required,
    }
}
const SESSION: Field = f("session_id", Kind::Id, true);
const CONNECTION: Field = f("connection_id", Kind::Id, true);
const RESULT: Field = f("result_id", Kind::Id, true);
const PAGE: Field = f("page", Kind::Positive, false);
const PAGE_SIZE: Field = f("page_size", Kind::Positive, false);
const CURSOR: Field = f("cursor", Kind::Text, false);

struct Spec {
    name: &'static str,
    description: &'static str,
    fields: &'static [Field],
    read_only: bool,
    destructive: bool,
}
const SPECS: &[Spec] = &[
    Spec {
        name: "get_server_info",
        description: "Get server capabilities, limits and permission status.",
        fields: &[],
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "list_connections",
        description: "List accessible connections.",
        fields: &[
            f("provider", Kind::Text, false),
            f("source", Kind::Text, false),
            CURSOR,
        ],
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "list_databases",
        description: "Discover accessible databases.",
        fields: &[CONNECTION, f("refresh", Kind::Bool, false)],
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "open_session",
        description: "Open a private database session.",
        fields: &[CONNECTION, f("database", Kind::Text, false)],
        read_only: false,
        destructive: false,
    },
    Spec {
        name: "get_session",
        description: "Get session and transaction status.",
        fields: &[SESSION],
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "close_session",
        description: "Close a database session and release its resources.",
        fields: &[SESSION],
        read_only: false,
        destructive: true,
    },
    Spec {
        name: "get_auth_status",
        description: "Get authentication status and user instructions; never submit credentials here.",
        fields: &[CONNECTION],
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "list_resources",
        description: "List database resources or scan Redis keys.",
        fields: &[
            SESSION,
            f("database", Kind::Text, false),
            f("kind", Kind::Text, false),
            f("pattern", Kind::Text, false),
            CURSOR,
        ],
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "describe_resource",
        description: "Describe a database resource.",
        fields: &[SESSION, f("target", Kind::Target, true)],
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "browse_resource",
        description: "Browse a resource and create an execution and result.",
        fields: &[
            SESSION,
            f("target", Kind::Target, true),
            f("filters", Kind::Filters, false),
            f("sort", Kind::Sort, false),
            PAGE,
            PAGE_SIZE,
        ],
        read_only: false,
        destructive: false,
    },
    Spec {
        name: "execute_command",
        description: "Execute a command or create a frozen approval request. Approval never changes the frozen command.",
        fields: &[
            SESSION,
            f("text", Kind::Text, true),
            f("parameters", Kind::Array, false),
            PAGE_SIZE,
        ],
        read_only: false,
        destructive: true,
    },
    Spec {
        name: "approve_command",
        description: "Decide a frozen approval. If native confirmation is unavailable, the host must first ask the user.",
        fields: &[
            f("approval_id", Kind::Id, true),
            f("decision", Kind::Decision, true),
        ],
        read_only: false,
        destructive: true,
    },
    Spec {
        name: "get_execution",
        description: "Get execution status and frozen request summary.",
        fields: &[
            f("execution_id", Kind::Id, true),
            f("include_text", Kind::Bool, false),
        ],
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "cancel_execution",
        description: "Cancel an execution or pending approval.",
        fields: &[f("execution_id", Kind::Id, true)],
        read_only: false,
        destructive: true,
    },
    Spec {
        name: "read_result",
        description: "Read a result page or continue its stream.",
        fields: &[RESULT, PAGE, PAGE_SIZE, CURSOR],
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "count_rows",
        description: "Recount a safely repeatable result.",
        fields: &[RESULT],
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "read_value",
        description: "Read a chunk of a live large value.",
        fields: &[
            f("value_id", Kind::Id, true),
            f("offset", Kind::Offset, true),
            f("max_bytes", Kind::Positive, false),
        ],
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "release_result",
        description: "Release a live result and its memory.",
        fields: &[RESULT],
        read_only: false,
        destructive: false,
    },
    Spec {
        name: "begin_transaction",
        description: "Begin a database transaction.",
        fields: &[SESSION],
        read_only: false,
        destructive: false,
    },
    Spec {
        name: "commit_transaction",
        description: "Commit a transaction subject to authorization and approval.",
        fields: &[SESSION],
        read_only: false,
        destructive: true,
    },
    Spec {
        name: "rollback_transaction",
        description: "Roll back a transaction.",
        fields: &[SESSION],
        read_only: false,
        destructive: true,
    },
    Spec {
        name: "list_history",
        description: "List authorized local or platform history.",
        fields: &[
            CONNECTION,
            f("source", Kind::Source, true),
            f("search", Kind::Text, false),
            PAGE,
            PAGE_SIZE,
        ],
        read_only: true,
        destructive: false,
    },
    Spec {
        name: "check_sql",
        description: "Check SQL with Archery without executing it.",
        fields: &[
            SESSION,
            f("database", Kind::Text, true),
            f("text", Kind::Text, true),
        ],
        read_only: true,
        destructive: false,
    },
];

fn field_schema(kind: Kind) -> Value {
    match kind {
        Kind::Id => json!({"type":"string","format":"uuid"}),
        Kind::Text => json!({"type":"string"}),
        Kind::Bool => json!({"type":"boolean"}),
        Kind::Positive => json!({"type":"integer","minimum":1}),
        Kind::Offset => json!({"type":"integer","minimum":0}),
        Kind::Target => {
            json!({"type":"object","properties":{"name":{"type":"string"},"database":{"type":["string","null"]},"kind":{"type":"string"},"key_b64":{"type":["string","null"]}},"required":["name"]})
        }
        Kind::Filters => {
            json!({"type":"array","items":{"type":"object","properties":{"column":{"type":"string"},"op":{"type":"string"},"value":{}},"required":["column"]}})
        }
        Kind::Sort => {
            json!({"type":"array","items":{"type":"object","properties":{"column":{"type":"string"},"direction":{"type":"string","enum":["asc","desc"]}},"required":["column"]}})
        }
        Kind::Array => json!({"type":"array","items":{}}),
        Kind::Decision => json!({"type":"string","enum":["approve","reject"]}),
        Kind::Source => json!({"type":"string","enum":["local","platform"]}),
    }
}
fn tool(spec: &Spec) -> Tool {
    let properties: Map<String, Value> = spec
        .fields
        .iter()
        .map(|field| (field.name.into(), field_schema(field.kind)))
        .collect();
    let required: Vec<&str> = spec
        .fields
        .iter()
        .filter(|field| field.required)
        .map(|field| field.name)
        .collect();
    let input = json!({"type":"object","properties":properties,"required":required,"additionalProperties":false});
    // Engine 的不同工具返回不同业务对象；公共字段为可选，其他字段由各 provider 补充。
    let output = json!({"type":"object","properties":{
        "execution_id":{"type":["string","null"]},"session_id":{"type":["string","null"]},
        "result_id":{"type":["string","null"]},"approval_id":{"type":["string","null"]},
        "state":{"type":"string"},"rows":{"type":"array"},"columns":{"type":"array"},
        "has_more":{"type":"boolean"},"next_cursor":{"type":["string","null"]},
        "total_rows":{"type":["integer","null"]},"retrieval_mode":{"type":"string"},
        "code":{"type":"string"},"message":{"type":"string"},"details":{},"database_code":{"type":["string","null"]}
    },"additionalProperties":true});
    Tool::new(
        spec.name,
        spec.description,
        Arc::new(input.as_object().unwrap().clone()),
    )
    .with_raw_output_schema(Arc::new(output.as_object().unwrap().clone()))
    .with_annotations(
        ToolAnnotations::new()
            .read_only(spec.read_only)
            .destructive(spec.destructive)
            .open_world(true),
    )
}
fn validate(spec: &Spec, args: &Map<String, Value>) -> std::result::Result<(), ErrorData> {
    for key in args.keys() {
        if !spec.fields.iter().any(|field| field.name == key) {
            return Err(ErrorData::invalid_params(
                format!("Unknown argument: {key}"),
                None,
            ));
        }
    }
    for field in spec.fields {
        let Some(value) = args.get(field.name) else {
            if field.required {
                return Err(ErrorData::invalid_params(
                    format!("Missing argument: {}", field.name),
                    None,
                ));
            }
            continue;
        };
        let valid = match field.kind {
            Kind::Id => value.as_str().is_some_and(|s| Uuid::parse_str(s).is_ok()),
            Kind::Text => value.is_string(),
            Kind::Bool => value.is_boolean(),
            Kind::Positive => value.as_u64().is_some_and(|n| n > 0),
            Kind::Offset => value.as_u64().is_some(),
            Kind::Target => value
                .as_object()
                .is_some_and(|target| target.get("name").is_some_and(Value::is_string)),
            Kind::Filters | Kind::Sort | Kind::Array => value.is_array(),
            Kind::Decision => matches!(value.as_str(), Some("approve" | "reject")),
            Kind::Source => matches!(value.as_str(), Some("local" | "platform")),
        };
        if !valid {
            return Err(ErrorData::invalid_params(
                format!("Invalid argument: {}", field.name),
                None,
            ));
        }
    }
    Ok(())
}

#[derive(Clone)]
struct Adapter {
    engine: Client,
    approvals: Arc<Mutex<HashMap<String, String>>>,
}
fn response(value: Value, error: bool) -> CallToolResponse {
    let value = if value.is_object() {
        value
    } else {
        json!({"data":value})
    };
    let summary = if error {
        value
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Operation failed")
    } else {
        "Operation completed"
    };
    let mut result = if error {
        CallToolResult::error(vec![ContentBlock::text(summary.to_owned())])
    } else {
        CallToolResult::success(vec![ContentBlock::text(summary.to_owned())])
    };
    result.structured_content = Some(value);
    result.into()
}
impl ServerHandler for Adapter {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        // 2026-07-28 禁止 server-to-client 请求；本服务依赖原生 elicitation。
        Cow::Borrowed(ProtocolVersion::known_up_to(&ProtocolVersion::V_2025_11_25))
    }
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("datapier", env!("CARGO_PKG_VERSION")))
            .with_instructions("All database operations are scoped to this MCP client. Ask the user before approving a frozen request when native confirmation is unavailable.")
    }
    async fn list_tools(
        &self,
        _: Option<PaginatedRequestParams>,
        _: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        Ok(ListToolsResult::with_all_items(
            SPECS.iter().map(tool).collect(),
        ))
    }
    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResponse, ErrorData> {
        // 取消只中止本地等待；Engine 已收到的写入仍可能运行并由其负责收尾。
        tokio::select! {
            biased;
            _ = context.ct.cancelled() => Ok(response(json!({"code":"outcome_unknown","message":"The MCP request was cancelled; a dispatched operation may still complete. Check execution status or history before retrying."}), true)),
            result = async {
        let spec = SPECS.iter().find(|spec| spec.name == request.name).ok_or_else(|| ErrorData::invalid_params("Unknown tool", None))?;
        let mut args = request.arguments.unwrap_or_default();
        validate(spec, &args)?;
        if spec.name == "approve_command" {
            let approving = args.get("decision").and_then(Value::as_str) == Some("approve");
            let native = context.client_capabilities().and_then(|c| c.elicitation).is_some_and(|c| c.form.is_some() || c.url.is_none());
            if native && approving {
                let id = args.get("approval_id").and_then(Value::as_str).unwrap();
                let execution = self.approvals.lock().unwrap().get(id).cloned();
                let Some(execution) = execution else {
                    return Ok(response(json!({"code":"approval_not_found","message":"Approval has no execution in this MCP session"}), true));
                };
                let frozen = match self.engine.request("mcp.get_execution", json!({"execution_id":execution,"include_text":true})).await {
                    Ok(value) => value,
                    Err(error) => return Ok(response(serde_json::to_value(error).unwrap(), true)),
                };
                // 在线 TSP 已经是交互控制者，不能再向 MCP 客户端发起第二个确认。
                let approval = frozen.get("approval");
                if approval.and_then(|a| a.get("route")).and_then(Value::as_str) == Some("tsp")
                    || approval.and_then(|a| a.get("tsp_interactive")).and_then(Value::as_bool) == Some(true)
                {
                    return Ok(response(json!({"code":"approval_controlled_by_tsp","message":"This approval is controlled by an interactive TSP window; review and decide there.","execution_id":execution,"approval_id":id,"state":"approval_required"}), true));
                }
                let schema = ElicitationSchema::builder().required_bool("approve").build().map_err(|e| ErrorData::internal_error(e.to_string(), None))?;
                let params = ElicitRequestParams::FormElicitationParams { meta: None, message: format!("Approve this frozen database operation?\n{frozen}"), requested_schema: schema };
                let confirmation = context.peer.create_elicitation_with_timeout(params, Some(Duration::from_secs(120))).await;
                if !matches!(confirmation, Ok(result) if result.action == ElicitationAction::Accept && result.content.as_ref().and_then(|content| content.get("approve")).and_then(Value::as_bool) == Some(true)) {
                    return Ok(response(json!({"code":"approval_not_confirmed","message":"User confirmation was declined, cancelled, or timed out; no approval was sent"}), true));
                }
            }
            args.insert("interaction_source".into(), json!(if native && approving { "native_elicitation" } else { "trusted_host" }));
        }
        let method = format!("mcp.{}", spec.name);
        match self.engine.request(&method, Value::Object(args)).await {
            Ok(value) => {
                if let (Some(approval), Some(execution)) = (value.get("approval_id").and_then(Value::as_str), value.get("execution_id").and_then(Value::as_str)) {
                    self.approvals.lock().unwrap().insert(approval.into(), execution.into());
                }
                Ok(response(value, false))
            }
            Err(error) => Ok(response(serde_json::to_value(error).unwrap(), true)),
        }
            } => result,
        }
    }
}

/// 启动 MCP stdio 服务；上游断开或收到中断时关闭独立 Engine 客户端会话。
pub async fn run(root: PathBuf, allow_connections: Vec<Uuid>) -> Result<()> {
    let engine = Client::connect_with(
        &root,
        ClientOptions {
            kind: ClientKind::Mcp,
            allow_connections,
            ..Default::default()
        },
    )
    .await?;
    let adapter = Adapter {
        engine: engine.clone(),
        approvals: Arc::new(Mutex::new(HashMap::new())),
    };
    let service = match adapter.serve(stdio()).await {
        Ok(service) => service,
        Err(error) => {
            let _ = engine.close().await;
            return Err(AppError::new("mcp", error.to_string()));
        }
    };
    let outcome = tokio::select! {
        result = service.waiting() => result.map(|_| ()).map_err(|error| AppError::new("mcp", error.to_string())),
        _ = shutdown_signal() => Ok(()),
    };
    let closed = engine.close().await;
    outcome.and(closed)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        if let Ok(mut terminate) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
#[path = "../tests/unit/mcp.rs"]
mod tests;
