use super::*;

impl Store {
    /// 按名称和 ID 返回保存的连接配置，不包含凭据。
    pub async fn profiles(&self) -> Result<Vec<Profile>> {
        let rows = sqlx::query("SELECT payload,archery_server_id,archery_instance_id,credential_saved,id,name,revision FROM profiles ORDER BY name,id").fetch_all(&self.pool).await?;
        rows.iter()
            .map(|r| {
                let mut profile: Profile = serde_json::from_str(r.get::<&str, _>(0))?;
                profile.revision = r.get::<i64, _>(6) as u64;
                let server: Option<String> = r.get(1);
                let instance: Option<i64> = r.get(2);
                let saved: i64 = r.get(3);
                if profile.id.to_string() != r.get::<&str, _>(4)
                    || profile.name != r.get::<&str, _>(5)
                {
                    return Err(AppError::new("storage_version", "连接身份与内容不一致"));
                }
                match profile.archery() {
                    Some(a)
                        if server.as_deref() == Some(a.server_id.to_string().as_str())
                            && instance == Some(a.instance_id)
                            && saved == 0 => {}
                    None if server.is_none() && instance.is_none() => (),
                    _ => return Err(AppError::new("storage_version", "连接关联与内容不一致")),
                }
                profile.validate()?;
                Ok(profile)
            })
            .collect()
    }
    pub async fn profile(&self, id: Uuid) -> Result<Profile> {
        self.profiles()
            .await?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| AppError::new("not_found", "连接不存在"))
    }
    /// 保存连接配置和可选凭据；返回是否已标记为保存到系统凭据库。
    /// 旧凭据无法删除时拒绝降级，保留原有追踪状态。
    pub async fn save_profile(
        &self,
        profile: &Profile,
        secret: Option<String>,
        persist: bool,
    ) -> Result<bool> {
        profile.validate()?;
        if profile.is_archery() {
            return Err(AppError::invalid("Archery 连接必须通过批量管理保存"));
        }
        // 调用方取消等待不能中断 keyring 与 SQLite 间的所有权交接。
        let store = self.clone();
        let profile = profile.clone();
        tokio::spawn(async move { store.save_profile_inner(&profile, secret, persist).await })
            .await
            .map_err(|e| AppError::new("credentials", e.to_string()))?
    }

    async fn save_profile_inner(
        &self,
        profile: &Profile,
        secret: Option<String>,
        persist: bool,
    ) -> Result<bool> {
        let _guard = self.profile_gate.lock().await;
        let existing: Option<(String, i64)> =
            sqlx::query_as("SELECT payload,revision FROM profiles WHERE id=?")
                .bind(profile.id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        if let Some((payload, revision)) = &existing {
            let old: Profile = serde_json::from_str(payload)?;
            if old.is_archery() {
                return Err(AppError::conflict("不能替换连接来源"));
            }
            if *revision as u64 != profile.revision {
                return Err(AppError::conflict("连接已更新，请重新载入"));
            }
        } else if profile.revision != 1 {
            return Err(AppError::conflict("新连接版本无效"));
        }
        let next = if existing.is_some() {
            profile
                .revision
                .checked_add(1)
                .filter(|n| *n <= i64::MAX as u64)
                .ok_or_else(|| AppError::invalid("连接版本溢出"))?
        } else {
            1
        };
        let old: Option<i64> =
            sqlx::query_scalar("SELECT credential_saved FROM profiles WHERE id=?")
                .bind(profile.id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let mut saved = old == Some(1);
        let mut tracked = saved;
        if let Some(secret) = secret {
            if persist {
                // 在写 keyring 之前登记可能存在的条目，避免后续 SQLite 错误丢失清理依据。
                sqlx::query("INSERT INTO profiles(id,name,payload,credential_saved) VALUES(?,?,?,1) ON CONFLICT(id) DO UPDATE SET credential_saved=1")
                    .bind(profile.id.to_string()).bind(&profile.name).bind(serde_json::to_string(profile)?)
                    .execute(&self.pool).await?;
                tracked = true;
                let account = self.credential_account(profile.id);
                let to_save = secret.clone();
                saved = tokio::task::spawn_blocking(move || {
                    keyring::v1::Entry::new("io.datapier.credentials", &account)?
                        .set_secret(to_save.as_bytes())
                })
                .await
                .map_err(|e| AppError::new("credentials", e.to_string()))?
                .is_ok();
            } else {
                saved = false;
            }
            if !saved && old == Some(1) {
                self.delete_credential(profile.id).await?;
                tracked = false;
            }
            self.secrets.write().await.insert(profile.id, secret);
        }
        let mut saved_profile = profile.clone();
        saved_profile.revision = next;
        sqlx::query("INSERT INTO profiles(id,name,payload,credential_saved,revision) VALUES(?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET name=excluded.name,payload=excluded.payload,credential_saved=excluded.credential_saved,revision=excluded.revision")
            .bind(profile.id.to_string()).bind(&profile.name).bind(serde_json::to_string(&saved_profile)?)
            .bind(i64::from(tracked || saved)).bind(next as i64).execute(&self.pool).await?;
        Ok(saved)
    }

    /// 优先读取会话内凭据；已标记持久化时从系统凭据库读取并缓存。
    /// 凭据库不可用返回错误，未保存凭据返回 `None`。
    pub async fn secret(&self, id: Uuid) -> Result<Option<String>> {
        // 与保存、删除共用锁，避免旧凭据读取完成后覆盖新的会话凭据。
        let _guard = self.profile_gate.lock().await;
        if let Some(secret) = self.secrets.read().await.get(&id).cloned() {
            return Ok(Some(secret));
        }
        let saved: Option<i64> =
            sqlx::query_scalar("SELECT credential_saved FROM profiles WHERE id=?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        if saved != Some(1) {
            return Ok(None);
        }
        let account = self.credential_account(id);
        let secret = read_keyring(account).await?;
        let secret =
            String::from_utf8(secret).map_err(|e| AppError::new("credentials", e.to_string()))?;
        self.secrets.write().await.insert(id, secret.clone());
        Ok(Some(secret))
    }

    /// 显式清除系统凭据及会话副本。
    pub async fn clear_profile_secret(&self, id: Uuid) -> Result<()> {
        let _guard = self.profile_gate.lock().await;
        let saved: Option<i64> =
            sqlx::query_scalar("SELECT credential_saved FROM profiles WHERE id=?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        let saved = saved.ok_or_else(|| AppError::new("not_found", "连接不存在"))?;
        if saved == 1 {
            self.delete_credential(id).await?;
        }
        sqlx::query("UPDATE profiles SET credential_saved=0 WHERE id=?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        self.secrets.write().await.remove(&id);
        Ok(())
    }
    /// 删除配置和会话内凭据；标记为持久化的凭据删除失败时保留配置。
    pub async fn delete_profile(&self, id: Uuid) -> Result<()> {
        let store = self.clone();
        tokio::spawn(async move { store.delete_profile_inner(id).await })
            .await
            .map_err(|e| AppError::new("credentials", e.to_string()))?
    }

    async fn delete_profile_inner(&self, id: Uuid) -> Result<()> {
        let _guard = self.profile_gate.lock().await;
        let saved: Option<i64> =
            sqlx::query_scalar("SELECT credential_saved FROM profiles WHERE id=?")
                .bind(id.to_string())
                .fetch_optional(&self.pool)
                .await?;
        if saved == Some(1) {
            self.delete_credential(id).await?;
        }
        sqlx::query("DELETE FROM profiles WHERE id=?")
            .bind(id.to_string())
            .execute(&self.pool)
            .await?;
        self.secrets.write().await.remove(&id);
        Ok(())
    }

    async fn delete_credential(&self, id: Uuid) -> Result<()> {
        let account = self.credential_account(id);
        let result = tokio::task::spawn_blocking(move || {
            keyring::v1::Entry::new("io.datapier.credentials", &account)?.delete_credential()
        })
        .await
        .map_err(|e| AppError::new("credentials", e.to_string()))?;
        match result {
            Ok(()) | Err(keyring::v1::Error::NoEntry) => Ok(()),
            Err(e) => Err(AppError::new(
                "credentials",
                format!("凭据删除失败，连接配置保留：{e}"),
            )),
        }
    }

    fn credential_account(&self, id: Uuid) -> String {
        let digest = Sha256::digest(self.root.to_string_lossy().as_bytes());
        format!("{:x}:{id}", digest)
    }
}
