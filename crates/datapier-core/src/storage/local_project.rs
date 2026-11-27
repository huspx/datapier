//! 本机项目关系存储；资源仍以 project_bindings 为唯一来源。
use super::*;
use crate::storage::transfer::BindingOverrides;
use std::collections::HashSet;

fn conflict() -> AppError {
    AppError::new("conflict", "项目已被修改，请重新读取")
}

fn profile_identity(profile: &Profile) -> Result<String> {
    let identity = match &profile.connection {
        ConnectionConfig::Direct(direct) => {
            json!({"provider":profile.provider,"source":"direct","host":direct.host,"port":direct.port,"username":direct.username,"tls":direct.tls})
        }
        ConnectionConfig::Archery(connection) => {
            json!({"provider":profile.provider,"source":"archery","server_id":connection.server_id,"instance_id":connection.instance_id})
        }
    };
    Ok(format!(
        "{:x}",
        base16ct::HexDisplay(&Sha256::digest(serde_json::to_vec(&identity)?))
    ))
}

impl Store {
    /// 读取本机项目及其环境、资源；不存在时返回 not_found。
    pub async fn local_project_get(&self, id: Uuid) -> Result<LocalProject> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        let project = Self::read_local_project(&mut tx, id).await?;
        tx.commit().await?;
        Ok(project)
    }

    async fn read_local_project(
        tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
        id: Uuid,
    ) -> Result<LocalProject> {
        let row = sqlx::query("SELECT * FROM local_projects WHERE id=?")
            .bind(id.to_string())
            .fetch_optional(&mut **tx)
            .await?
            .ok_or_else(|| AppError::new("not_found", "本机项目不存在"))?;
        let default_id: Option<String> = row.try_get("default_environment_id")?;
        let mut project = LocalProject {
            id,
            name: row.try_get("name")?,
            description: row.try_get("description")?,
            default_environment_id: default_id.as_deref().map(parse_id).transpose()?,
            revision: row.try_get::<i64, _>("revision")? as u64,
            roots: Vec::new(),
            environments: Vec::new(),
        };
        for row in sqlx::query(
            "SELECT path,identity FROM local_project_roots WHERE project_id=? ORDER BY path",
        )
        .bind(id.to_string())
        .fetch_all(&mut **tx)
        .await?
        {
            project.roots.push(LocalProjectRoot {
                path: row.try_get("path")?,
                identity: row.try_get("identity")?,
            });
        }
        for row in sqlx::query(
            "SELECT * FROM local_project_environments WHERE project_id=? ORDER BY name,id",
        )
        .bind(id.to_string())
        .fetch_all(&mut **tx)
        .await?
        {
            let env_id = parse_id(row.try_get("id")?)?;
            let mut env = LocalEnvironment {
                id: env_id,
                name: row.try_get("name")?,
                environment_type: row.try_get("environment_type")?,
                description: row.try_get("description")?,
                resources: Vec::new(),
            };
            for binding in sqlx::query("SELECT profile_id,profile_identity,database_json,description FROM project_bindings WHERE project_id=? AND binding_group=? ORDER BY profile_id,database_json").bind(id.to_string()).bind(env_id.to_string()).fetch_all(&mut **tx).await? {
                let overrides: BindingOverrides = serde_json::from_str(binding.try_get("database_json")?)?;
                let profile_id = parse_id(binding.try_get("profile_id")?)?;
                let baseline: Option<String> = binding.try_get("profile_identity")?;
                let payload: Option<String> = sqlx::query_scalar("SELECT payload FROM profiles WHERE id=?").bind(profile_id.to_string()).fetch_optional(&mut **tx).await?;
                let current = payload.map(|value| serde_json::from_str::<Profile>(&value)).transpose()?.map(|p| profile_identity(&p)).transpose()?;
                let status = if baseline.is_some() && baseline == current { "valid" } else { "invalid" };
                env.resources.push(LocalResource { profile_id, database: overrides.database, redis_db: overrides.redis_db, description: binding.try_get("description")?, binding_status: status.into() });
            }
            project.environments.push(env);
        }
        Ok(project)
    }
    pub async fn local_project_list(&self) -> Result<Vec<LocalProject>> {
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM local_projects ORDER BY name,id")
            .fetch_all(&mut *self.pool.acquire().await?)
            .await?;
        let mut projects = Vec::with_capacity(ids.len());
        for id in ids {
            projects.push(self.local_project_get(parse_id(&id)?).await?);
        }
        Ok(projects)
    }
    /// 整个项目以一个 revision 提交，冲突时不保留任何局部更改。
    pub async fn local_project_save(
        &self,
        project: &LocalProject,
        expected: Option<u64>,
    ) -> Result<LocalProject> {
        if project.name.trim().is_empty()
            || project.name.len() > 1024
            || project.description.len() > 16384
            || project.roots.len() > 1000
            || project.environments.len() > 1000
        {
            return Err(AppError::invalid("本机项目字段无效"));
        }
        let mut env_ids = HashSet::new();
        let mut names = HashSet::new();
        let mut roots = HashSet::new();
        for root in &project.roots {
            if root.path.is_empty() || root.identity.is_empty() || !roots.insert(&root.path) {
                return Err(AppError::invalid("仓库目录无效或重复"));
            }
        }
        for env in &project.environments {
            if env.name.trim().is_empty()
                || env.name.len() > 1024
                || env.environment_type.trim().is_empty()
                || env.description.len() > 16384
                || !env_ids.insert(env.id)
                || !names.insert(&env.name)
                || env.resources.len() > 1000
            {
                return Err(AppError::invalid("环境无效或重复"));
            }
            let mut targets = HashSet::new();
            for resource in &env.resources {
                if resource.description.len() > 16384
                    || resource
                        .database
                        .as_ref()
                        .is_some_and(|name| name.trim().is_empty() || name.len() > 1024)
                    || resource.database.is_some() == resource.redis_db.is_some()
                    || !targets.insert((
                        resource.profile_id,
                        resource.database.as_deref(),
                        resource.redis_db,
                    ))
                {
                    return Err(AppError::invalid(
                        "同一环境的实例和 DB 不能重复，且必须指定数据库或 Redis DB",
                    ));
                }
            }
        }
        if project
            .default_environment_id
            .is_some_and(|id| !env_ids.contains(&id))
        {
            return Err(AppError::invalid("默认环境不属于项目"));
        }
        let next = expected
            .unwrap_or(0)
            .checked_add(1)
            .and_then(|n| i64::try_from(n).ok())
            .ok_or_else(conflict)?;
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        let affected = if let Some(rev) = expected {
            sqlx::query("UPDATE local_projects SET name=?,description=?,default_environment_id=?,revision=? WHERE id=? AND revision=?").bind(&project.name).bind(&project.description).bind(project.default_environment_id.map(|v| v.to_string())).bind(next).bind(project.id.to_string()).bind(i64::try_from(rev).map_err(|_| conflict())?).execute(&mut *tx).await?.rows_affected()
        } else {
            sqlx::query("INSERT INTO local_projects(id,name,description,default_environment_id,revision) VALUES(?,?,?,?,?) ON CONFLICT DO NOTHING").bind(project.id.to_string()).bind(&project.name).bind(&project.description).bind(project.default_environment_id.map(|v| v.to_string())).bind(next).execute(&mut *tx).await?.rows_affected()
        };
        if affected != 1 {
            return Err(conflict());
        }
        let existing: Vec<String> =
            sqlx::query_scalar("SELECT id FROM local_project_environments WHERE project_id=?")
                .bind(project.id.to_string())
                .fetch_all(&mut *tx)
                .await?;
        sqlx::query("DELETE FROM local_project_roots WHERE project_id=?")
            .bind(project.id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM local_project_environments WHERE project_id=?")
            .bind(project.id.to_string())
            .execute(&mut *tx)
            .await?;
        for id in &existing {
            if !env_ids.contains(&parse_id(id)?) {
                sqlx::query("DELETE FROM project_bindings WHERE project_id=? AND binding_group=?")
                    .bind(project.id.to_string())
                    .bind(id)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        for root in &project.roots {
            sqlx::query("INSERT INTO local_project_roots(path,project_id,identity) VALUES(?,?,?)")
                .bind(&root.path)
                .bind(project.id.to_string())
                .bind(&root.identity)
                .execute(&mut *tx)
                .await?;
        }
        for env in &project.environments {
            // 保留已有覆盖中的路径；仅修改本机环境管理负责的 DB 和用途。
            let old = sqlx::query("SELECT profile_id,profile_identity,database_json,revision FROM project_bindings WHERE project_id=? AND binding_group=?").bind(project.id.to_string()).bind(env.id.to_string()).fetch_all(&mut *tx).await?;
            if !existing.contains(&env.id.to_string()) && !old.is_empty() {
                return Err(conflict());
            }
            let mut overrides = std::collections::HashMap::new();
            for row in old {
                let profile = parse_id(row.try_get("profile_id")?)?;
                let value: BindingOverrides = serde_json::from_str(row.try_get("database_json")?)?;
                let key =
                    serde_json::to_string(&(profile, value.database.as_deref(), value.redis_db))?;
                overrides.insert(
                    key,
                    (
                        value,
                        row.try_get::<i64, _>("revision")?,
                        Some(profile),
                        row.try_get::<Option<String>, _>("profile_identity")?,
                    ),
                );
            }
            sqlx::query("DELETE FROM project_bindings WHERE project_id=? AND binding_group=?")
                .bind(project.id.to_string())
                .bind(env.id.to_string())
                .execute(&mut *tx)
                .await?;
            for resource in &env.resources {
                // 复用绑定表；内部键编码实际目标，不引入需要用户维护的名称。
                let key = serde_json::to_string(&(
                    resource.profile_id,
                    resource.database.as_deref(),
                    resource.redis_db,
                ))?;
                let previous = overrides.remove(&key);
                let (mut value, revision, old_profile, old_identity) = previous.unwrap_or_default();
                value.database = resource.database.clone();
                value.redis_db = resource.redis_db;
                let revision = revision.checked_add(1).ok_or_else(conflict)?;
                let identity = if old_profile == Some(resource.profile_id) {
                    old_identity
                } else {
                    let payload: Option<String> =
                        sqlx::query_scalar("SELECT payload FROM profiles WHERE id=?")
                            .bind(resource.profile_id.to_string())
                            .fetch_optional(&mut *tx)
                            .await?;
                    let payload =
                        payload.ok_or_else(|| AppError::new("not_found", "关联连接不存在"))?;
                    Some(profile_identity(&serde_json::from_str::<Profile>(
                        &payload,
                    )?)?)
                };
                sqlx::query("INSERT INTO project_bindings(project_id,binding_group,connection_alias,profile_id,database_json,revision,description,profile_identity) VALUES(?,?,?,?,?,?,?,?)").bind(project.id.to_string()).bind(env.id.to_string()).bind(&key).bind(resource.profile_id.to_string()).bind(serde_json::to_string(&value)?).bind(revision).bind(&resource.description).bind(identity).execute(&mut *tx).await?;
            }
            sqlx::query("INSERT INTO local_project_environments(id,project_id,name,environment_type,description) VALUES(?,?,?,?,?)").bind(env.id.to_string()).bind(project.id.to_string()).bind(&env.name).bind(&env.environment_type).bind(&env.description).execute(&mut *tx).await?;
        }
        let saved = Self::read_local_project(&mut tx, project.id).await?;
        tx.commit().await?;
        Ok(saved)
    }
    pub async fn local_project_delete(&self, id: Uuid, expected: u64) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        let mut tx = conn.begin().await?;
        let groups: Vec<String> =
            sqlx::query_scalar("SELECT id FROM local_project_environments WHERE project_id=?")
                .bind(id.to_string())
                .fetch_all(&mut *tx)
                .await?;
        let result = sqlx::query("DELETE FROM local_projects WHERE id=? AND revision=?")
            .bind(id.to_string())
            .bind(i64::try_from(expected).map_err(|_| conflict())?)
            .execute(&mut *tx)
            .await?;
        if result.rows_affected() != 1 {
            return Err(conflict());
        }
        for group in groups {
            sqlx::query("DELETE FROM project_bindings WHERE project_id=? AND binding_group=?")
                .bind(id.to_string())
                .bind(group)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}
