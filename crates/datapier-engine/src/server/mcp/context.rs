//! MCP client 的仓库上下文仅影响发现排序，不影响授权。
use super::*;

#[derive(Clone, Default)]
pub(super) struct ProjectContext {
    roots: Vec<String>,
    source: String,
    environment: Option<String>,
    revision: u64,
}

fn local_uri(uri: &str) -> Option<String> {
    if !uri.contains("://") {
        return Some(uri.to_owned());
    }
    let path = uri.strip_prefix("file://")?;
    let path = if path.starts_with('/') {
        path
    } else {
        let rest = path.strip_prefix("localhost")?;
        if !rest.starts_with('/') {
            return None;
        }
        rest
    };
    let mut bytes = Vec::with_capacity(path.len());
    let raw = path.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' {
            if i + 2 >= raw.len() {
                return None;
            }
            let hex = std::str::from_utf8(&raw[i + 1..i + 3]).ok()?;
            bytes.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    let value = String::from_utf8(bytes).ok()?;
    #[cfg(windows)]
    let value = if value.as_bytes().get(2) == Some(&b':') && value.starts_with('/') {
        value[1..].to_owned()
    } else {
        value
    };
    Some(value)
}

impl Engine {
    pub(super) async fn mcp_context_update(&self, owner: Uuid, params: &Value) -> Result<Value> {
        let roots: Vec<String> = model::param(params, "roots")?;
        let source: String = model::param(params, "source")?;
        let environment: Option<String> = optional(params, "environment")?;
        if roots.len() > 1000 || roots.iter().any(|r| r.len() > 16384) {
            return Err(AppError::invalid("仓库上下文过大"));
        }
        let mut state = self.state.lock().unwrap();
        let old = state.mcp.contexts.get(&owner).map_or(0, |c| c.revision);
        let revision = old
            .checked_add(1)
            .ok_or_else(|| AppError::new("resource_limit", "上下文版本已耗尽"))?;
        state.mcp.contexts.insert(
            owner,
            ProjectContext {
                roots,
                source,
                environment,
                revision,
            },
        );
        Ok(json!({"context_revision":revision}))
    }
    pub(super) async fn mcp_project_context(&self, owner: Uuid) -> Result<Value> {
        let context = self
            .state
            .lock()
            .unwrap()
            .mcp
            .contexts
            .get(&owner)
            .cloned()
            .unwrap_or_default();
        let mut matched = Vec::new();
        for root in &context.roots {
            let Some(path) = local_uri(root) else {
                matched.push(json!({"root":root,"status":"remote"}));
                continue;
            };
            let resolved = self
                .local_project_rpc("local_project.resolve", json!({"path":path}))
                .await?;
            if resolved["status"] != "matched" {
                matched.push(json!({"root":root,"status":resolved["status"]}));
                continue;
            }
            let project = &resolved["project"];
            let environments = project["environments"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let mut selected: Vec<_> = if let Some(name) = &context.environment {
                environments
                    .into_iter()
                    .filter(|e| e["name"] == *name)
                    .collect()
            } else if !project["default_environment_id"].is_null() {
                environments
                    .into_iter()
                    .filter(|e| e["id"] == project["default_environment_id"])
                    .collect()
            } else {
                environments
            };
            for env in &mut selected {
                let resources = env["resources"].as_array().cloned().unwrap_or_default();
                let invalid_count = resources
                    .iter()
                    .filter(|r| r["binding_status"] == "invalid")
                    .count();
                let mut visible = Vec::new();
                for resource in resources {
                    let Some(id) = resource["profile_id"]
                        .as_str()
                        .and_then(|id| Uuid::parse_str(id).ok())
                    else {
                        continue;
                    };
                    let Ok(profile) = self.mcp_profile(owner, id).await else {
                        continue;
                    };
                    let database = resource["database"]
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| resource["redis_db"].as_u64().map(|db| db.to_string()))
                        .or_else(|| profile.default_database().map(str::to_owned));
                    if self
                        .mcp_allow(owner, &profile, database.as_deref(), false)
                        .await
                        .is_ok()
                    {
                        visible.push(resource);
                    }
                }
                env["resources"] = json!(visible);
                env["invalid_resource_count"] = json!(invalid_count);
            }
            let status = if selected.is_empty() {
                "environment_unmatched"
            } else {
                "matched"
            };
            matched.push(json!({"root":root,"status":status,"project_id":project["id"],"project_name":project["name"],"project_revision":project["revision"],"environments":selected}));
        }
        Ok(
            json!({"source":if context.source.is_empty() {"unknown"} else {&context.source},"context_revision":context.revision,"status":if context.roots.is_empty() {"unknown"} else {"available"},"roots":matched}),
        )
    }
    pub(super) fn mcp_recommendations(context: &Value, profile: Uuid) -> Vec<Value> {
        let mut out = Vec::new();
        for root in context["roots"].as_array().into_iter().flatten() {
            for env in root["environments"].as_array().into_iter().flatten() {
                for resource in env["resources"].as_array().into_iter().flatten() {
                    if resource["binding_status"] != "valid" {
                        continue;
                    }
                    if resource["profile_id"].as_str() == Some(profile.to_string().as_str()) {
                        out.push(json!({"project_id":root["project_id"],"project_name":root["project_name"],"environment_id":env["id"],"environment_name":env["name"],"description":resource["description"],"database":resource["database"],"redis_db":resource["redis_db"]}));
                    }
                }
            }
        }
        out
    }
}
