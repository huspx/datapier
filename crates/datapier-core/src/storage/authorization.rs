use super::*;
use sqlx::Connection;

impl Store {
    pub async fn mcp_rules(&self, profile_id: Uuid) -> Result<Vec<McpAccessRule>> {
        let rows = sqlx::query("SELECT scope_kind,scope_key,level FROM mcp_access_rules WHERE profile_id=? ORDER BY scope_kind,scope_key").bind(profile_id.to_string()).fetch_all(&mut *self.pool.acquire().await?).await?;
        rows.iter()
            .map(|row| {
                Ok(McpAccessRule {
                    profile_id,
                    scope_kind: row.get(0),
                    scope_key: row.get(1),
                    level: serde_json::from_value(json!(row.get::<&str, _>(2)))?,
                })
            })
            .collect()
    }
    pub async fn save_mcp_rules(
        &self,
        profile_id: Uuid,
        expected_revision: u64,
        rules: &[McpAccessRule],
    ) -> Result<u64> {
        let next = expected_revision
            .checked_add(1)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or_else(|| AppError::invalid("连接版本溢出"))?;
        let _guard = self.profile_gate.lock().await;
        if rules.iter().any(|r| {
            r.profile_id != profile_id || r.scope_kind.is_empty() || r.scope_key.is_empty()
        }) {
            return Err(AppError::invalid("授权规则无效"));
        }
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin().await?;
        let updated = sqlx::query("UPDATE profiles SET revision=? WHERE id=? AND revision=?")
            .bind(next as i64)
            .bind(profile_id.to_string())
            .bind(expected_revision as i64)
            .execute(&mut *tx)
            .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::conflict("连接已更新，请重新载入"));
        }
        sqlx::query("DELETE FROM mcp_access_rules WHERE profile_id=?")
            .bind(profile_id.to_string())
            .execute(&mut *tx)
            .await?;
        for r in rules {
            let level = match r.level {
                McpAccessLevel::Disabled => "disabled",
                McpAccessLevel::ReadOnly => "read_only",
                McpAccessLevel::ReadWrite => "read_write",
            };
            sqlx::query("INSERT INTO mcp_access_rules(profile_id,scope_kind,scope_key,level) VALUES(?,?,?,?)").bind(profile_id.to_string()).bind(&r.scope_kind).bind(&r.scope_key).bind(level).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(next)
    }
    pub async fn set_profile_approval(
        &self,
        id: Uuid,
        expected_revision: u64,
        policy: ApprovalPolicy,
    ) -> Result<Profile> {
        let _guard = self.profile_gate.lock().await;
        let next = expected_revision
            .checked_add(1)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or_else(|| AppError::invalid("连接版本溢出"))?;
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin().await?;
        let payload: Option<String> =
            sqlx::query_scalar("SELECT payload FROM profiles WHERE id=? AND revision=?")
                .bind(id.to_string())
                .bind(expected_revision as i64)
                .fetch_optional(&mut *tx)
                .await?;
        let mut profile: Profile = serde_json::from_str(
            &payload.ok_or_else(|| AppError::conflict("连接已更新或不存在"))?,
        )?;
        profile.approval_policy = policy;
        profile.revision = next;
        let changed =
            sqlx::query("UPDATE profiles SET payload=?,revision=? WHERE id=? AND revision=?")
                .bind(serde_json::to_string(&profile)?)
                .bind(next as i64)
                .bind(id.to_string())
                .bind(expected_revision as i64)
                .execute(&mut *tx)
                .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::conflict("连接已更新，请重新载入"));
        }
        tx.commit().await?;
        Ok(profile)
    }
    pub async fn create_approval(&self, approval: &ApprovalRecord) -> Result<()> {
        if approval.state != "pending"
            || approval.expires_at <= approval.created_at
            || contains_secret(&approval.request)
        {
            return Err(AppError::invalid("审批请求无效或包含凭据"));
        }
        sqlx::query("INSERT INTO approvals(id,execution_id,owner,session_id,profile_id,request,digest,profile_revision,policy_revision,created_at,expires_at,state,decided_at,decision_source) VALUES(?,?,?,?,?,?,?,?,?,?,?,?,?,?)")
            .bind(approval.id.to_string()).bind(approval.execution_id.to_string()).bind(approval.owner.to_string()).bind(approval.session_id.to_string()).bind(approval.profile_id.to_string()).bind(approval.request.to_string()).bind(&approval.digest).bind(approval.profile_revision as i64).bind(approval.policy_revision as i64).bind(approval.created_at).bind(approval.expires_at).bind(&approval.state).bind(approval.decided_at).bind(&approval.decision_source).execute(&mut *self.pool.acquire().await?).await?;
        Ok(())
    }
    pub async fn approval(&self, id: Uuid) -> Result<ApprovalRecord> {
        let row = sqlx::query("SELECT id,execution_id,owner,session_id,profile_id,request,digest,profile_revision,policy_revision,created_at,expires_at,state,decided_at,decision_source FROM approvals WHERE id=?").bind(id.to_string()).fetch_optional(&mut *self.pool.acquire().await?).await?.ok_or_else(|| AppError::new("not_found", "审批不存在"))?;
        approval_row(&row)
    }
    pub async fn approvals_for_owner(&self, owner: Uuid) -> Result<Vec<ApprovalRecord>> {
        let rows = sqlx::query("SELECT id,execution_id,owner,session_id,profile_id,request,digest,profile_revision,policy_revision,created_at,expires_at,state,decided_at,decision_source FROM approvals WHERE owner=? ORDER BY created_at,id").bind(owner.to_string()).fetch_all(&mut *self.pool.acquire().await?).await?;
        rows.iter().map(approval_row).collect()
    }
    pub async fn decide_approval(
        &self,
        id: Uuid,
        owner: Uuid,
        decision: &str,
        source: &str,
    ) -> Result<ApprovalRecord> {
        let state = match decision {
            "approve" => "dispatching",
            "reject" => "rejected",
            "cancel" => "cancelled",
            "invalidate" => "invalidated",
            "expire" => "expired",
            _ => return Err(AppError::invalid("审批决定无效")),
        };
        let mut connection = self.pool.acquire().await?;
        let mut tx = connection.begin().await?;
        let changed = sqlx::query("UPDATE approvals SET state=CASE WHEN expires_at<=unixepoch() THEN 'expired' ELSE ? END,decided_at=unixepoch(),decision_source=? WHERE id=? AND owner=? AND state='pending' AND (? != 'expire' OR expires_at<=unixepoch())")
            .bind(state).bind(source).bind(id.to_string()).bind(owner.to_string()).bind(decision).execute(&mut *tx).await?;
        let row = sqlx::query("SELECT id,execution_id,owner,session_id,profile_id,request,digest,profile_revision,policy_revision,created_at,expires_at,state,decided_at,decision_source FROM approvals WHERE id=? AND owner=?").bind(id.to_string()).bind(owner.to_string()).fetch_optional(&mut *tx).await?.ok_or_else(|| AppError::new("not_found", "审批不存在"))?;
        let approval = approval_row(&row)?;
        if changed.rows_affected() == 1
            && matches!(
                approval.state.as_str(),
                "rejected" | "cancelled" | "invalidated" | "expired"
            )
        {
            // 到期优先于用户决定；关联未发送执行必须在同一事务内取得真实终态。
            sqlx::query("UPDATE executions SET state=?,summary=json_set(CASE WHEN json_type(summary)='object' THEN summary ELSE '{}' END,'$.approval_id',?),updated_at=unixepoch('subsec') WHERE id=? AND sent=0 AND state IN ('queued','approval_required')")
                .bind(&approval.state).bind(approval.id.to_string()).bind(approval.execution_id.to_string()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(approval)
    }
}
fn approval_row(row: &sqlx::sqlite::SqliteRow) -> Result<ApprovalRecord> {
    Ok(ApprovalRecord {
        id: parse_id(row.get(0))?,
        execution_id: parse_id(row.get(1))?,
        owner: parse_id(row.get(2))?,
        session_id: parse_id(row.get(3))?,
        profile_id: parse_id(row.get(4))?,
        request: serde_json::from_str(row.get(5))?,
        digest: row.get(6),
        profile_revision: row.get::<i64, _>(7) as u64,
        policy_revision: row.get::<i64, _>(8) as u64,
        created_at: row.get(9),
        expires_at: row.get(10),
        state: row.get(11),
        decided_at: row.get(12),
        decision_source: row.get(13),
    })
}
fn contains_secret(value: &Json) -> bool {
    match value {
        Json::Object(map) => map.iter().any(|(key, val)| {
            matches!(
                key.to_ascii_lowercase().as_str(),
                "password" | "secret" | "token" | "cookie" | "otp" | "totp"
            ) || contains_secret(val)
        }),
        Json::Array(a) => a.iter().any(contains_secret),
        _ => false,
    }
}
