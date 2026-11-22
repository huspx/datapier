//! 无共享清单的本机项目与环境关系。
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalProject {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub default_environment_id: Option<Uuid>,
    #[serde(default)]
    pub revision: u64,
    #[serde(default)]
    pub roots: Vec<LocalProjectRoot>,
    #[serde(default)]
    pub environments: Vec<LocalEnvironment>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalProjectRoot {
    pub path: String,
    #[serde(default)]
    pub identity: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalEnvironment {
    pub id: Uuid,
    pub name: String,
    pub environment_type: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub resources: Vec<LocalResource>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalResource {
    pub profile_id: Uuid,
    pub database: Option<String>,
    pub redis_db: Option<u32>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub binding_status: String,
}
