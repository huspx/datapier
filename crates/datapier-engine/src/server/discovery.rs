//! 数据库发现及按连接配置版本缓存。

use super::*;

impl Engine {
    pub(super) async fn discover_databases(&self, owner: Uuid, params: &Value) -> Result<Value> {
        let session_id = optional::<Uuid>(params, "session_id")?;
        let db = if session_id.is_some() {
            Some(self.database(owner, params)?)
        } else {
            None
        };
        let supplied = optional::<Profile>(params, "profile")?;
        let cacheable = supplied.is_none();
        let profile = if let Some(profile) = supplied {
            profile
        } else if let Some(db) = &db {
            db.profile.clone()
        } else {
            self.store
                .profile(model::param(params, "profile_id")?)
                .await?
        };
        profile.validate()?;
        let refresh = optional::<bool>(params, "refresh")?.unwrap_or(false);
        if let Some(connection) = profile.archery() {
            if refresh {
                return self.discover_archery_databases(owner, &profile).await;
            }
            return Ok(
                json!({"state":"known","source":"archery_selection","databases":connection.databases,"configured_count":null,"reason":null}),
            );
        }
        if cacheable
            && !refresh
            && let Some((revision, value)) = self.state.lock().unwrap().discovery.get(&profile.id)
            && *revision == profile.revision
        {
            return Ok(value.clone());
        }
        let timeout = self.store.settings().await?.limits.connect_timeout_seconds;
        let cancel = CancellationToken::new();
        let sink = ExecutionSink::new(self.store.clone(), Uuid::new_v4());
        let discovery = if let Some(db) = db {
            let mut session = db.session.lock().await;
            session
                .call("database.list", json!({}), &sink, &cancel)
                .await?
        } else {
            let secret = match optional::<String>(params, "secret")? {
                Some(secret) => Some(secret),
                None => self.store.secret(profile.id).await?,
            };
            let mut session = tokio::time::timeout(
                Duration::from_secs(timeout),
                ProviderSession::connect(profile.clone(), secret),
            )
            .await
            .map_err(|_| AppError::new("timeout", "连接超时"))??;
            let value = session
                .call("database.list", json!({}), &sink, &cancel)
                .await;
            let _ = session.close().await;
            value?
        };
        if cacheable {
            self.state
                .lock()
                .unwrap()
                .discovery
                .insert(profile.id, (profile.revision, discovery.clone()));
        }
        Ok(discovery)
    }
}
