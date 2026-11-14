//! Archery 服务配置、独立凭据归属与实例批量事务。
use super::*;
use sqlx::Connection;
use std::collections::{HashMap, HashSet};

fn server_from_row(row: &sqlx::sqlite::SqliteRow) -> Result<ArcheryServer> {
    let mode: &str = row.get(6);
    Ok(ArcheryServer {
        id: parse_id(row.get::<&str, _>(0))?,
        name: row.get(1),
        base_url: row.get(2),
        username: row.get(3),
        http_tls: serde_json::from_str(row.get::<&str, _>(4))?,
        revision: row.get::<i64, _>(5) as u64,
        totp_mode: match mode {
            "manual" => TotpMode::Manual,
            "automatic" => TotpMode::Automatic,
            _ => return Err(AppError::new("storage_version", "验证码模式无效")),
        },
    })
}

impl Store {
    fn archery_account(&self, id: Uuid, totp: bool) -> String {
        let digest = Sha256::digest(self.root.to_string_lossy().as_bytes());
        let suffix = if totp { ":totp" } else { "" };
        format!("{:x}:archery:{id}{suffix}", digest)
    }

    pub async fn archery_servers(&self) -> Result<Vec<ArcheryServerInfo>> {
        let rows = sqlx::query("SELECT s.id,s.name,s.base_url,s.username,s.http_tls,s.revision,s.totp_mode,s.credential_saved,s.totp_credential_saved,(SELECT count(*) FROM profiles p WHERE p.archery_server_id=s.id) FROM archery_servers s ORDER BY s.name,s.id").fetch_all(&mut *self.pool.acquire().await?).await?;
        let passwords = self.archery_secrets.read().await;
        let totps = self.archery_totp_secrets.read().await;
        rows.iter()
            .map(|row| {
                let server = server_from_row(row)?;
                let credential_mode = passwords.get(&server.id).map(|(_, mode)| *mode).unwrap_or(
                    if row.get::<i64, _>(7) != 0 {
                        CredentialMode::Keyring
                    } else {
                        CredentialMode::Unknown
                    },
                );
                let totp_credential_mode = totps.get(&server.id).map(|(_, mode)| *mode).unwrap_or(
                    if row.get::<i64, _>(8) != 0 {
                        CredentialMode::Keyring
                    } else {
                        CredentialMode::Unknown
                    },
                );
                let totp_configured = row.get::<i64, _>(8) != 0 || totps.contains_key(&server.id);
                Ok(ArcheryServerInfo {
                    server,
                    credential_mode,
                    totp_credential_mode,
                    totp_configured,
                    connection_count: row.get::<i64, _>(9) as u64,
                })
            })
            .collect()
    }

    pub async fn archery_server(&self, id: Uuid) -> Result<ArcheryServer> {
        let row = sqlx::query("SELECT id,name,base_url,username,http_tls,revision,totp_mode FROM archery_servers WHERE id=?")
            .bind(id.to_string()).fetch_optional(&mut *self.pool.acquire().await?).await?
            .ok_or_else(|| AppError::new("not_found", "服务不存在"))?;
        server_from_row(&row)
    }

    pub async fn archery_secret(&self, id: Uuid) -> Result<Option<String>> {
        self.read_archery_secret(id, false).await
    }

    pub async fn archery_totp_secret(&self, id: Uuid) -> Result<Option<String>> {
        self.read_archery_secret(id, true).await
    }

    async fn read_archery_secret(&self, id: Uuid, totp: bool) -> Result<Option<String>> {
        let _guard = self.profile_gate.lock().await;
        let cache = if totp {
            &self.archery_totp_secrets
        } else {
            &self.archery_secrets
        };
        if let Some((secret, _)) = cache.read().await.get(&id) {
            return Ok(Some(secret.clone()));
        }
        let query = if totp {
            "SELECT totp_credential_saved FROM archery_servers WHERE id=?"
        } else {
            "SELECT credential_saved FROM archery_servers WHERE id=?"
        };
        let saved: Option<i64> = sqlx::query_scalar(query)
            .bind(id.to_string())
            .fetch_optional(&mut *self.pool.acquire().await?)
            .await?;
        if saved != Some(1) {
            return Ok(None);
        }
        let bytes = read_keyring(self.archery_account(id, totp)).await?;
        let secret =
            String::from_utf8(bytes).map_err(|_| AppError::new("credentials", "凭据编码无效"))?;
        cache
            .write()
            .await
            .insert(id, (secret.clone(), CredentialMode::Keyring));
        Ok(Some(secret))
    }

    /// 分别交接密码和 TOTP；取消等待不会中断外部凭据所有权交接。
    pub async fn save_archery_server(
        &self,
        server: &ArcheryServer,
        expected_revision: u64,
        password: CredentialUpdate,
        totp: CredentialUpdate,
    ) -> Result<ArcheryServerInfo> {
        let store = self.clone();
        let server = server.clone();
        tokio::spawn(async move {
            store
                .save_archery_server_inner(server, expected_revision, password, totp)
                .await
        })
        .await
        .map_err(|e| AppError::new("credentials", e.to_string()))?
    }

    async fn save_archery_server_inner(
        &self,
        server: ArcheryServer,
        expected: u64,
        password: CredentialUpdate,
        totp: CredentialUpdate,
    ) -> Result<ArcheryServerInfo> {
        let mut server = server;
        crate::providers::archery::normalize_server(&mut server)?;
        if server.name.trim().is_empty()
            || server
                .http_tls
                .ca_path
                .as_ref()
                .is_some_and(|path| path.trim().is_empty())
        {
            return Err(AppError::invalid("服务配置无效"));
        }
        for update in [&password, &totp] {
            if let CredentialUpdate::Replace { value, .. } = update
                && value.is_empty()
            {
                return Err(AppError::invalid("凭据不能为空"));
            }
        }
        if let CredentialUpdate::Replace { value, .. } = &totp {
            crate::providers::archery::validate_totp_uri(value)?;
        }
        let _guard = self.profile_gate.lock().await;
        let row = sqlx::query("SELECT id,name,base_url,username,http_tls,revision,totp_mode,credential_saved,totp_credential_saved FROM archery_servers WHERE id=?")
            .bind(server.id.to_string()).fetch_optional(&mut *self.pool.acquire().await?).await?;
        let (password_tracked, totp_tracked) = if let Some(row) = row {
            let old = server_from_row(&row)?;
            if old.base_url != server.base_url || old.username != server.username {
                return Err(AppError::conflict("服务身份不可修改"));
            }
            if old.revision != expected {
                return Err(AppError::conflict("服务修订已变化"));
            }
            (row.get::<i64, _>(7) != 0, row.get::<i64, _>(8) != 0)
        } else {
            if expected != 0 {
                return Err(AppError::conflict("服务不存在或修订已变化"));
            }
            (false, false)
        };
        let totp_present = match &totp {
            CredentialUpdate::Keep => {
                totp_tracked
                    || self
                        .archery_totp_secrets
                        .read()
                        .await
                        .contains_key(&server.id)
            }
            CredentialUpdate::Replace { .. } => true,
            CredentialUpdate::Clear => false,
        };
        if server.totp_mode == TotpMode::Automatic && !totp_present {
            return Err(AppError::invalid("自动验证码需要配置 TOTP"));
        }
        server.revision = expected
            .checked_add(1)
            .filter(|v| *v <= i64::MAX as u64)
            .ok_or_else(|| AppError::invalid("服务修订溢出"))?;
        let mode = match server.totp_mode {
            TotpMode::Manual => "manual",
            TotpMode::Automatic => "automatic",
        };
        if expected == 0 {
            // 在任何 keyring 写入前先持久化配置及可能存在的条目归属。
            sqlx::query("INSERT INTO archery_servers(id,name,base_url,username,http_tls,revision,totp_mode,credential_saved,totp_credential_saved) VALUES(?,?,?,?,?,?,?,?,?)")
                .bind(server.id.to_string()).bind(&server.name).bind(&server.base_url).bind(&server.username)
                .bind(serde_json::to_string(&server.http_tls)?).bind(server.revision as i64).bind(mode)
                .bind(i64::from(matches!(&password, CredentialUpdate::Replace { persist: true, .. })))
                .bind(i64::from(matches!(&totp, CredentialUpdate::Replace { persist: true, .. })))
                .execute(&mut *self.pool.acquire().await?).await?;
        }
        self.update_archery_credential(server.id, false, password, password_tracked)
            .await?;
        self.update_archery_credential(server.id, true, totp, totp_tracked)
            .await?;
        sqlx::query(
            "UPDATE archery_servers SET name=?,http_tls=?,totp_mode=?,revision=? WHERE id=?",
        )
        .bind(&server.name)
        .bind(serde_json::to_string(&server.http_tls)?)
        .bind(mode)
        .bind(server.revision as i64)
        .bind(server.id.to_string())
        .execute(&mut *self.pool.acquire().await?)
        .await?;
        self.archery_servers()
            .await?
            .into_iter()
            .find(|info| info.server.id == server.id)
            .ok_or_else(|| AppError::new("storage", "服务保存后不存在"))
    }

    async fn update_archery_credential(
        &self,
        id: Uuid,
        totp: bool,
        update: CredentialUpdate,
        previously_tracked: bool,
    ) -> Result<()> {
        if matches!(update, CredentialUpdate::Keep) {
            return Ok(());
        }
        let cache = if totp {
            &self.archery_totp_secrets
        } else {
            &self.archery_secrets
        };
        let mark = if totp {
            "UPDATE archery_servers SET totp_credential_saved=? WHERE id=?"
        } else {
            "UPDATE archery_servers SET credential_saved=? WHERE id=?"
        };
        let mut tracked = previously_tracked;
        if let CredentialUpdate::Replace { value, persist } = update {
            if persist {
                // 先登记可能存在的条目，避免 keyring 写入成功而 SQLite 失败时遗失清理依据。
                sqlx::query(mark)
                    .bind(1_i64)
                    .bind(id.to_string())
                    .execute(&mut *self.pool.acquire().await?)
                    .await?;
                tracked = true;
                let account = self.archery_account(id, totp);
                let bytes = value.clone();
                let saved = tokio::task::spawn_blocking(move || {
                    keyring::v1::Entry::new("io.datapier.credentials", &account)?
                        .set_secret(bytes.as_bytes())
                })
                .await
                .map_err(|e| AppError::new("credentials", e.to_string()))?
                .is_ok();
                if saved {
                    cache
                        .write()
                        .await
                        .insert(id, (value, CredentialMode::Keyring));
                    return Ok(());
                }
            }
            if previously_tracked {
                self.delete_archery_credential(id, totp).await?;
                tracked = false;
            }
            sqlx::query(mark)
                .bind(i64::from(tracked))
                .bind(id.to_string())
                .execute(&mut *self.pool.acquire().await?)
                .await?;
            cache
                .write()
                .await
                .insert(id, (value, CredentialMode::Session));
        } else {
            if tracked {
                self.delete_archery_credential(id, totp).await?;
            }
            sqlx::query(mark)
                .bind(0_i64)
                .bind(id.to_string())
                .execute(&mut *self.pool.acquire().await?)
                .await?;
            cache.write().await.remove(&id);
        }
        Ok(())
    }

    async fn delete_archery_credential(&self, id: Uuid, totp: bool) -> Result<()> {
        let account = self.archery_account(id, totp);
        let result = tokio::task::spawn_blocking(move || {
            keyring::v1::Entry::new("io.datapier.credentials", &account)?.delete_credential()
        })
        .await
        .map_err(|e| AppError::new("credentials", e.to_string()))?;
        match result {
            Ok(()) | Err(keyring::v1::Error::NoEntry) => Ok(()),
            Err(e) => Err(AppError::new("credentials", format!("凭据删除失败：{e}"))),
        }
    }

    /// 清理两个独立凭据后，在同一事务中删除冻结的连接集合和服务器。
    pub async fn delete_archery_server(
        &self,
        id: Uuid,
        expected_revision: u64,
        expected_profile_ids: &[Uuid],
    ) -> Result<Vec<Uuid>> {
        let store = self.clone();
        let ids = expected_profile_ids.to_vec();
        tokio::spawn(async move {
            let _guard = store.profile_gate.lock().await;
            let row = sqlx::query("SELECT revision,credential_saved,totp_credential_saved FROM archery_servers WHERE id=?")
                .bind(id.to_string()).fetch_optional(&mut *store.pool.acquire().await?).await?
                .ok_or_else(|| AppError::new("not_found", "服务不存在"))?;
            if row.get::<i64, _>(0) as u64 != expected_revision { return Err(AppError::conflict("服务修订已变化")); }
            let actual: Vec<String> = sqlx::query_scalar("SELECT id FROM profiles WHERE archery_server_id=? ORDER BY id")
                .bind(id.to_string()).fetch_all(&mut *store.pool.acquire().await?).await?;
            let mut expected: Vec<String> = ids.iter().map(Uuid::to_string).collect();
            expected.sort();
            if actual != expected { return Err(AppError::conflict("服务连接集合已变化")); }
            for (totp, column) in [(false, 1), (true, 2)] {
                if row.get::<i64, _>(column) != 0 {
                    store.delete_archery_credential(id, totp).await?;
                    let mark = if totp { "UPDATE archery_servers SET totp_credential_saved=0 WHERE id=?" } else { "UPDATE archery_servers SET credential_saved=0 WHERE id=?" };
                    sqlx::query(mark).bind(id.to_string()).execute(&mut *store.pool.acquire().await?).await?;
                }
                let cache = if totp { &store.archery_totp_secrets } else { &store.archery_secrets };
                cache.write().await.remove(&id);
            }
            let mut connection = store.pool.acquire().await?;
            let mut tx = connection.begin().await?;
            sqlx::query("DELETE FROM profiles WHERE archery_server_id=?").bind(id.to_string()).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM archery_servers WHERE id=?").bind(id.to_string()).execute(&mut *tx).await?;
            tx.commit().await?;
            actual.iter().map(|id| parse_id(id)).collect()
        }).await.map_err(|e| AppError::new("credentials", e.to_string()))?
    }

    /// 按完整冻结基线原子应用实例增改删；历史及结果不随配置删除。
    pub async fn apply_archery_profiles(
        &self,
        server_id: Uuid,
        expected_revision: u64,
        expected_profiles: &[Profile],
        desired_profiles: &[Profile],
    ) -> Result<Json> {
        let store = self.clone();
        let expected = expected_profiles.to_vec();
        let desired = desired_profiles.to_vec();
        tokio::spawn(async move {
            let _guard = store.profile_gate.lock().await;
            let mut connection = store.pool.acquire().await?;
            let mut tx = connection.begin().await?;
            let revision: Option<i64> = sqlx::query_scalar("SELECT revision FROM archery_servers WHERE id=?").bind(server_id.to_string()).fetch_optional(&mut *tx).await?;
            if revision != Some(expected_revision as i64) { return Err(AppError::conflict("服务修订已变化")); }
            let rows = sqlx::query("SELECT payload,revision FROM profiles WHERE archery_server_id=? ORDER BY id").bind(server_id.to_string()).fetch_all(&mut *tx).await?;
            let current: Vec<Profile> = rows.iter().map(|row| { let mut p: Profile = serde_json::from_str(row.get::<&str,_>(0))?; p.revision = row.get::<i64,_>(1) as u64; Ok(p) }).collect::<Result<_>>()?;
            let index = |profiles: &[Profile]| -> Result<HashMap<Uuid, Json>> {
                let mut map = HashMap::new();
                let mut instances = HashSet::new();
                for p in profiles {
                    p.validate()?;
                    let a = p.archery().ok_or_else(|| AppError::invalid("批量连接必须来自 Archery"))?;
                    if a.server_id != server_id || !instances.insert(a.instance_id) || map.insert(p.id, serde_json::to_value(p)?).is_some() { return Err(AppError::conflict("批量连接身份冲突")); }
                }
                Ok(map)
            };
            let old = index(&current)?;
            if old != index(&expected)? { return Err(AppError::conflict("服务连接基线已变化")); }
            let next = index(&desired)?;
            let prior: HashMap<Uuid, &Profile> = current.iter().map(|p| (p.id, p)).collect();
            let prior_instances: HashMap<i64, Uuid> = current.iter().map(|p| (p.archery().unwrap().instance_id, p.id)).collect();
            for p in &desired {
                if prior_instances.get(&p.archery().unwrap().instance_id).is_some_and(|id| *id != p.id) {
                    return Err(AppError::conflict("不能替换实例连接身份"));
                }
                if let Some(previous) = prior.get(&p.id) {
                    if p.revision != previous.revision { return Err(AppError::conflict("连接修订已变化")); }
                    let a = p.archery().unwrap();
                    let b = previous.archery().unwrap();
                    if p.provider != previous.provider || a.server_id != b.server_id || a.instance_id != b.instance_id || a.instance_name != b.instance_name { return Err(AppError::conflict("不能替换实例身份")); }
                }
            }
            let mut added: Vec<Uuid> = next.keys().filter_map(|id| (!old.contains_key(id)).then_some(*id)).collect();
            let mut deleted: Vec<Uuid> = old.keys().filter_map(|id| (!next.contains_key(id)).then_some(*id)).collect();
            let mut modified: Vec<Uuid> = next.iter().filter_map(|(id, profile)| old.get(id).filter(|old| *old != profile).map(|_| *id)).collect();
            added.sort(); modified.sort(); deleted.sort();
            let mut desired = desired;
            for p in &mut desired { if modified.contains(&p.id) { p.revision = p.revision.checked_add(1).filter(|n| *n <= i64::MAX as u64).ok_or_else(|| AppError::invalid("连接版本溢出"))?; } }
            let mut profiles = desired.clone();
            profiles.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
            let result = json!({"added":added,"modified":modified,"deleted":deleted,"profiles":profiles});
            if serde_json::to_vec(&result)?.len() > 4 * 1024 * 1024 - 4096 { return Err(AppError::new("response_too_large", "响应超过 4 MiB，请缩小选择")); }
            for id in &deleted { sqlx::query("DELETE FROM profiles WHERE id=? AND archery_server_id=?").bind(id.to_string()).bind(server_id.to_string()).execute(&mut *tx).await?; }
            for p in &desired {
                if added.contains(&p.id) {
                    let a = p.archery().unwrap();
                    sqlx::query("INSERT INTO profiles(id,name,payload,credential_saved,archery_server_id,archery_instance_id) VALUES(?,?,?,0,?,?)")
                        .bind(p.id.to_string()).bind(&p.name).bind(serde_json::to_string(p)?).bind(server_id.to_string()).bind(a.instance_id).execute(&mut *tx).await?;
                } else if modified.contains(&p.id) {
                    sqlx::query("UPDATE profiles SET name=?,payload=?,revision=? WHERE id=?").bind(&p.name).bind(serde_json::to_string(p)?).bind(p.revision as i64).bind(p.id.to_string()).execute(&mut *tx).await?;
                }
            }
            tx.commit().await?;
            Ok(result)
        }).await.map_err(|e| AppError::new("storage", e.to_string()))?
    }
}
