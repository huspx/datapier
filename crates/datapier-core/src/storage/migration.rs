//! 配置库版本检查及无损迁移。
use super::*;
use sqlx::SqliteConnection;

fn invalid_schema() -> AppError {
    AppError::new(
        "storage_version",
        "配置数据库结构或数据不兼容；请备份原目录并使用新目录，勿覆盖原数据",
    )
}

// 比较 SQLite 保存的建表语句，忽略标识符引号和语句外的排版，保留字符串字面量。
fn canonical_sql(sql: &str) -> String {
    let mut out = String::new();
    let mut literal = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\'' {
            out.push(c);
            if literal && chars.peek() == Some(&'\'') {
                out.push(chars.next().unwrap());
            } else {
                literal = !literal;
            }
        } else if literal {
            out.push(c);
        } else if !c.is_whitespace() && c != '"' && c != '`' && c != '[' && c != ']' && c != ';' {
            out.extend(c.to_lowercase());
        }
    }
    out
}

async fn verify_structure(conn: &mut SqliteConnection, version: i64) -> Result<()> {
    // 使用同一份迁移定义生成参考库，覆盖列、CHECK、外键、唯一约束及索引定义。
    let mut reference = SqliteConnection::connect("sqlite::memory:").await?;
    sqlx::raw_sql(include_str!("../../migrations/0001_config.sql"))
        .execute(&mut reference)
        .await?;
    if version >= 2 {
        sqlx::raw_sql(include_str!("../../migrations/0002_config.sql"))
            .execute(&mut reference)
            .await?;
    }
    if version >= 3 {
        sqlx::raw_sql(include_str!("../../migrations/0003_config.sql"))
            .execute(&mut reference)
            .await?;
    }
    if version >= 4 {
        sqlx::raw_sql(include_str!("../../migrations/0004_config.sql"))
            .execute(&mut reference)
            .await?;
    }
    if version >= 5 {
        sqlx::raw_sql(include_str!("../../migrations/0005_config.sql"))
            .execute(&mut reference)
            .await?;
    }
    if version >= 6 {
        sqlx::raw_sql(include_str!("../../migrations/0006_config.sql"))
            .execute(&mut reference)
            .await?;
    }
    let query = "SELECT type,name,tbl_name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name";
    let expected = sqlx::query(query).fetch_all(&mut reference).await?;
    let actual = sqlx::query(query).fetch_all(&mut *conn).await?;
    if actual.len() != expected.len() {
        return Err(invalid_schema());
    }
    for (a, e) in actual.iter().zip(&expected) {
        if a.get::<&str, _>(0) != e.get::<&str, _>(0)
            || a.get::<&str, _>(1) != e.get::<&str, _>(1)
            || a.get::<&str, _>(2) != e.get::<&str, _>(2)
            || canonical_sql(a.get::<&str, _>(3)) != canonical_sql(e.get::<&str, _>(3))
        {
            return Err(invalid_schema());
        }
    }
    Ok(())
}

fn convert_direct(row: &sqlx::sqlite::SqliteRow) -> Result<String> {
    let id: &str = row.get(0);
    let mut payload: Json =
        serde_json::from_str(row.get::<&str, _>(1)).map_err(|_| invalid_schema())?;
    let obj = payload.as_object_mut().ok_or_else(invalid_schema)?;
    // schema 1 没有来源分支，不能覆盖已有字段或推测其来源。
    if obj.contains_key("connection") {
        return Err(invalid_schema());
    }
    let mut direct = serde_json::Map::new();
    direct.insert("kind".into(), json!("direct"));
    for key in [
        "host",
        "port",
        "username",
        "database",
        "visible_databases",
        "redis_db",
        "tls",
    ] {
        if let Some(value) = obj.remove(key) {
            direct.insert(key.into(), value);
        }
    }
    obj.insert("connection".into(), Json::Object(direct));
    let profile: Profile = serde_json::from_value(payload.clone()).map_err(|_| invalid_schema())?;
    if profile.id.to_string() != id
        || profile.name != row.get::<&str, _>(2)
        || !matches!(row.get::<i64, _>(3), 0 | 1)
    {
        return Err(invalid_schema());
    }
    profile.validate().map_err(|_| invalid_schema())?;
    Ok(serde_json::to_string(&payload)?)
}

// 按读取接口使用的类型解析元数据，不规范化或改写已有记录。
async fn verify_transfer_metadata(conn: &mut SqliteConnection) -> Result<()> {
    fn json_field<T: serde::de::DeserializeOwned>(
        row: &sqlx::sqlite::SqliteRow,
        name: &str,
    ) -> Result<()> {
        let value: &str = row.try_get(name).map_err(|_| invalid_schema())?;
        serde_json::from_str::<T>(value).map_err(|_| invalid_schema())?;
        Ok(())
    }
    fn id_field(row: &sqlx::sqlite::SqliteRow, name: &str, optional: bool) -> Result<()> {
        let value: Option<&str> = row.try_get(name).map_err(|_| invalid_schema())?;
        match value {
            Some(value) => {
                Uuid::parse_str(value).map_err(|_| invalid_schema())?;
            }
            None if optional => (),
            None => return Err(invalid_schema()),
        }
        Ok(())
    }

    for row in sqlx::query("SELECT id,spec_json FROM transfer_presets")
        .fetch_all(&mut *conn)
        .await?
    {
        id_field(&row, "id", false)?;
        json_field::<crate::model::transfer::TransferSpec>(&row, "spec_json")?;
    }
    for row in
        sqlx::query("SELECT id,execution_id,retry_of,context_json,progress_json FROM transfer_runs")
            .fetch_all(&mut *conn)
            .await?
    {
        id_field(&row, "id", false)?;
        id_field(&row, "execution_id", false)?;
        id_field(&row, "retry_of", true)?;
        json_field::<super::transfer::TransferRunContext>(&row, "context_json")?;
        json_field::<crate::model::transfer::TransferProgress>(&row, "progress_json")?;
    }
    for row in sqlx::query("SELECT locator_json,outcome_json FROM transfer_steps")
        .fetch_all(&mut *conn)
        .await?
    {
        json_field::<crate::model::transfer::TransferLocator>(&row, "locator_json")?;
        json_field::<crate::model::transfer::TransferStepOutcome>(&row, "outcome_json")?;
    }
    for row in sqlx::query("SELECT id,run_id,owner_client_id,identity_json FROM transfer_artifacts")
        .fetch_all(&mut *conn)
        .await?
    {
        id_field(&row, "id", false)?;
        id_field(&row, "run_id", true)?;
        id_field(&row, "owner_client_id", true)?;
        json_field::<super::transfer::ArtifactIdentity>(&row, "identity_json")?;
    }
    for row in sqlx::query("SELECT project_id,profile_id,database_json FROM project_bindings")
        .fetch_all(&mut *conn)
        .await?
    {
        id_field(&row, "project_id", false)?;
        id_field(&row, "profile_id", false)?;
        json_field::<super::transfer::BindingOverrides>(&row, "database_json")?;
    }
    Ok(())
}

async fn verify_data(conn: &mut SqliteConnection, version: i64) -> Result<()> {
    let integrity: Vec<String> = sqlx::query_scalar("PRAGMA integrity_check")
        .fetch_all(&mut *conn)
        .await?;
    if integrity.len() != 1 || integrity[0] != "ok" {
        return Err(invalid_schema());
    }
    let invalid: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM profiles WHERE id IS NULL OR id='' OR json_valid(payload)=0 OR credential_saved NOT IN (0,1)) + \
         (SELECT count(*) FROM workspaces WHERE id IS NULL OR id='' OR revision<=0 OR json_valid(payload)=0) + \
         (SELECT count(*) FROM executions WHERE id IS NULL OR id='' OR json_valid(params)=0 OR json_valid(summary)=0) + \
         (SELECT count(*) FROM app_settings WHERE json_valid(payload)=0)"
    ).fetch_one(&mut *conn).await?;
    if invalid != 0 {
        return Err(invalid_schema());
    }
    if version == 1 {
        let collisions: i64 = sqlx::query_scalar("SELECT count(*) FROM executions WHERE json_type(summary)='object' AND EXISTS (SELECT 1 FROM json_each(executions.summary) WHERE key='_migration')")
            .fetch_one(&mut *conn).await?;
        if collisions != 0 {
            return Err(AppError::new(
                "storage_version",
                "历史摘要已包含 _migration，无法无损迁移",
            ));
        }
        let rows = sqlx::query("SELECT id,payload,name,credential_saved FROM profiles")
            .fetch_all(&mut *conn)
            .await?;
        for row in &rows {
            convert_direct(row)?;
        }
    } else {
        let rows = sqlx::query("SELECT id,payload,name,credential_saved,archery_server_id,archery_instance_id FROM profiles")
            .fetch_all(&mut *conn).await?;
        for row in &rows {
            let profile: Profile =
                serde_json::from_str(row.get::<&str, _>(1)).map_err(|_| invalid_schema())?;
            let server: Option<&str> = row.get(4);
            let instance: Option<i64> = row.get(5);
            let saved: i64 = row.get(3);
            if profile.id.to_string() != row.get::<&str, _>(0)
                || profile.name != row.get::<&str, _>(2)
            {
                return Err(invalid_schema());
            }
            match profile.archery() {
                Some(a)
                    if server == Some(a.server_id.to_string().as_str())
                        && instance == Some(a.instance_id)
                        && saved == 0 => {}
                None if server.is_none() && instance.is_none() && matches!(saved, 0 | 1) => (),
                _ => return Err(invalid_schema()),
            }
            profile.validate().map_err(|_| invalid_schema())?;
        }
        let servers = sqlx::query("SELECT id,name,base_url,username,http_tls,revision,totp_mode,credential_saved,totp_credential_saved FROM archery_servers")
            .fetch_all(&mut *conn)
            .await?;
        let mut identities = std::collections::HashSet::new();
        for row in &servers {
            let id: &str = row.try_get(0).map_err(|_| invalid_schema())?;
            let parsed_id = Uuid::parse_str(id).map_err(|_| invalid_schema())?;
            let revision: i64 = row.try_get(5).map_err(|_| invalid_schema())?;
            let mode: &str = row.try_get(6).map_err(|_| invalid_schema())?;
            let password_saved: i64 = row.try_get(7).map_err(|_| invalid_schema())?;
            let totp_saved: i64 = row.try_get(8).map_err(|_| invalid_schema())?;
            let mut server = ArcheryServer {
                id: parsed_id,
                name: row.try_get(1).map_err(|_| invalid_schema())?,
                base_url: row.try_get(2).map_err(|_| invalid_schema())?,
                username: row.try_get(3).map_err(|_| invalid_schema())?,
                http_tls: serde_json::from_str(
                    row.try_get::<&str, _>(4).map_err(|_| invalid_schema())?,
                )
                .map_err(|_| invalid_schema())?,
                revision: u64::try_from(revision).map_err(|_| invalid_schema())?,
                totp_mode: match mode {
                    "manual" => TotpMode::Manual,
                    "automatic" => TotpMode::Automatic,
                    _ => return Err(invalid_schema()),
                },
            };
            let original_url = server.base_url.clone();
            crate::providers::archery::normalize_server(&mut server)
                .map_err(|_| invalid_schema())?;
            // 只检查副本，不能在打开时改写身份或掩盖规范化后的冲突。
            if parsed_id.to_string() != id
                || server.name.trim().is_empty()
                || server
                    .http_tls
                    .ca_path
                    .as_ref()
                    .is_some_and(|path| path.trim().is_empty())
                || revision <= 0
                || !matches!(password_saved, 0 | 1)
                || !matches!(totp_saved, 0 | 1)
                || !identities.insert((server.base_url.clone(), server.username.clone()))
                || original_url != server.base_url
            {
                return Err(invalid_schema());
            }
        }
    }
    if version >= 4 {
        let rows = sqlx::query("SELECT * FROM value_edit_files")
            .fetch_all(&mut *conn)
            .await?;
        for row in &rows {
            super::edit_files::decode_record(row).map_err(|_| invalid_schema())?;
        }
    }
    if version >= 5 {
        verify_transfer_metadata(conn).await?;
    }
    if version >= 5 {
        for row in sqlx::query("SELECT project_id,root,binding_group,binding_digest FROM project_binding_confirmations").fetch_all(&mut *conn).await? {
            let id: &str = row.try_get("project_id").map_err(|_| invalid_schema())?;
            let root: &str = row.try_get("root").map_err(|_| invalid_schema())?;
            let group: &str = row.try_get("binding_group").map_err(|_| invalid_schema())?;
            let digest: &str = row.try_get("binding_digest").map_err(|_| invalid_schema())?;
            if Uuid::parse_str(id).is_err() || root.is_empty() || group.is_empty() || digest.len() != 64 || !digest.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(invalid_schema());
            }
        }
    }
    if version >= 6 {
        let invalid: i64 = sqlx::query_scalar("SELECT count(*) FROM local_projects p WHERE p.default_environment_id IS NOT NULL AND NOT EXISTS (SELECT 1 FROM local_project_environments e WHERE e.id=p.default_environment_id AND e.project_id=p.id)")
            .fetch_one(&mut *conn).await?;
        if invalid != 0 {
            return Err(invalid_schema());
        }
        for row in sqlx::query(
            "SELECT id FROM local_projects UNION ALL SELECT id FROM local_project_environments",
        )
        .fetch_all(&mut *conn)
        .await?
        {
            let id: &str = row.try_get(0).map_err(|_| invalid_schema())?;
            if Uuid::parse_str(id).is_err() {
                return Err(invalid_schema());
            }
        }
    }
    if !sqlx::query("PRAGMA foreign_key_check")
        .fetch_all(&mut *conn)
        .await?
        .is_empty()
    {
        return Err(invalid_schema());
    }
    Ok(())
}

async fn preflight_connection(conn: &mut SqliteConnection) -> Result<i64> {
    let version: i64 = sqlx::query_scalar("PRAGMA user_version")
        .fetch_one(&mut *conn)
        .await?;
    match version {
        0 => {
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM sqlite_master WHERE name NOT LIKE 'sqlite_%'",
            )
            .fetch_one(&mut *conn)
            .await?;
            if count != 0 {
                return Err(invalid_schema());
            }
        }
        1..=6 => {
            verify_structure(conn, version).await?;
            verify_data(conn, version).await?;
        }
        _ => return Err(invalid_schema()),
    }
    Ok(version)
}

pub(super) async fn preflight_config(conn: &mut SqliteConnection) -> Result<()> {
    preflight_connection(conn).await?;
    Ok(())
}

pub(super) async fn migrate_config(conn: &mut SqliteConnection) -> Result<()> {
    // 版本及内容在迁移事务内检查，避免检查后数据库发生变化。
    sqlx::query("PRAGMA foreign_keys=OFF")
        .execute(&mut *conn)
        .await?;
    let off: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
        .fetch_one(&mut *conn)
        .await?;
    if off != 0 {
        return Err(invalid_schema());
    }
    let result: Result<()> = async {
        let mut tx = conn.begin().await?;
        let version = preflight_connection(&mut tx).await?;
        if version == 6 {
            tx.commit().await?;
            return Ok(());
        }
        if version == 0 {
            sqlx::raw_sql(include_str!("../../migrations/0001_config.sql"))
                .execute(&mut *tx)
                .await?;
        }
        if version < 2 {
            let rows = sqlx::query("SELECT id,payload,name,credential_saved FROM profiles")
                .fetch_all(&mut *tx)
                .await?;
            for row in &rows {
                sqlx::query("UPDATE profiles SET payload=? WHERE id=?")
                    .bind(convert_direct(row)?)
                    .bind(row.get::<&str, _>(0))
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::raw_sql(include_str!("../../migrations/0002_config.sql"))
                .execute(&mut *tx)
                .await?;
        }
        if version < 3 {
            sqlx::raw_sql(include_str!("../../migrations/0003_config.sql"))
                .execute(&mut *tx)
                .await?;
        }
        if version < 4 {
            sqlx::raw_sql(include_str!("../../migrations/0004_config.sql"))
                .execute(&mut *tx)
                .await?;
        }
        if version < 5 {
            sqlx::raw_sql(include_str!("../../migrations/0005_config.sql"))
                .execute(&mut *tx)
                .await?;
        }
        sqlx::raw_sql(include_str!("../../migrations/0006_config.sql"))
            .execute(&mut *tx)
            .await?;
        verify_structure(&mut tx, 6).await?;
        verify_data(&mut tx, 6).await?;
        sqlx::query("PRAGMA user_version=6")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }
    .await;
    let restored = async {
        sqlx::query("PRAGMA foreign_keys=ON")
            .execute(&mut *conn)
            .await?;
        let enabled: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut *conn)
            .await?;
        if enabled != 1 {
            return Err(invalid_schema());
        }
        Ok(())
    }
    .await;
    restored?;
    result
}
