use super::*;

impl Store {
    /// 创建排队中的任务记录并返回其状态，调用方提供的参数会持久化。
    pub async fn create_task(
        &self,
        owner: Uuid,
        workspace: &str,
        method: &str,
        params: Json,
        may_write: bool,
    ) -> Result<TaskRecord> {
        let params = sanitize_params(params);
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO executions(id,owner,workspace_id,method,params,state,sent,may_write,summary) VALUES(?,?,?,?,?,'queued',0,?,'null')")
            .bind(id.to_string()).bind(owner.to_string()).bind(workspace).bind(method).bind(params.to_string())
            .bind(i64::from(may_write)).execute(&self.pool).await?;
        self.task(id).await
    }

    /// 按全局保留规则清理终态命令，存活结果及尚需核对的执行始终保留。
    pub async fn cleanup_history(&self) -> Result<u64> {
        let settings = self.settings().await?;
        let history = &settings.history;
        if !history.auto_cleanup_enabled
            || (history.retention_days.is_none() && history.max_entries.is_none())
        {
            return Ok(0);
        }
        let retention = history
            .retention_days
            .map(|days| days.saturating_mul(86_400).min(i64::MAX as u64) as i64);
        let maximum = history
            .max_entries
            .map(|count| count.min(i64::MAX as u64) as i64);
        // 与 start_result 共用锁直到事务结束，防止检查后产生依赖已删除 proof 的结果。
        let mut results = self.results.lock().await;
        let live =
            results.live_execution_ids(Duration::from_secs(settings.limits.result_idle_seconds));
        let mut tx = self.pool.begin().await?;
        sqlx::query("CREATE TEMP TABLE IF NOT EXISTS history_cleanup_candidates(seq INTEGER PRIMARY KEY,id TEXT NOT NULL)").execute(&mut *tx).await?;
        sqlx::query("DELETE FROM history_cleanup_candidates")
            .execute(&mut *tx)
            .await?;
        // 先冻结排序，再分批删除；删除不能改变尚未遍历记录的排名。
        sqlx::query("INSERT INTO history_cleanup_candidates(seq,id) SELECT seq,id FROM (SELECT rowid AS seq,id,state,created_at,row_number() OVER (ORDER BY created_at DESC,rowid DESC) AS rank FROM executions WHERE method IN ('execution.start','execution.run')) WHERE state IN ('succeeded','failed','cancelled') AND ((? IS NOT NULL AND created_at < unixepoch()-?) OR (? IS NOT NULL AND rank>?))")
            .bind(retention).bind(retention).bind(maximum).bind(maximum).execute(&mut *tx).await?;
        let mut last = 0i64;
        let mut deleted = 0u64;
        loop {
            let rows = sqlx::query(
                "SELECT seq,id FROM history_cleanup_candidates WHERE seq>? ORDER BY seq LIMIT 200",
            )
            .bind(last)
            .fetch_all(&mut *tx)
            .await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                last = row.get(0);
                let id: &str = row.get(1);
                if live.contains(&parse_id(id)?) {
                    continue;
                }
                let result = sqlx::query("DELETE FROM executions WHERE id=? AND state IN ('succeeded','failed','cancelled') AND NOT EXISTS (SELECT 1 FROM approvals WHERE execution_id=executions.id AND state IN ('pending','dispatching'))")
                    .bind(id).execute(&mut *tx).await?;
                deleted += result.rows_affected();
            }
        }
        sqlx::query("DROP TABLE history_cleanup_candidates")
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(deleted)
    }
    /// 查询开始时记录物理连接的当前数据库，不能以初始连接配置代替 USE 后的状态。
    pub async fn record_task_database(&self, id: Uuid, database: Option<&str>) -> Result<()> {
        sqlx::query("UPDATE executions SET params=json_set(params,'$._task_context.database',?) WHERE id=? AND json_type(params,'$._task_context')='object'")
            .bind(database).bind(id.to_string()).execute(&self.pool).await?;
        Ok(())
    }
    /// 在持有会话锁的执行路径记录当前 Redis DB，保留缺失任务上下文的历史记录。
    pub async fn record_task_redis_db(&self, id: Uuid, db: u32) -> Result<()> {
        sqlx::query("UPDATE executions SET params=json_set(params,'$._task_context.redis_db',?) WHERE id=? AND json_type(params,'$._task_context')='object'")
            .bind(i64::from(db)).bind(id.to_string()).execute(&self.pool).await?;
        Ok(())
    }

    /// 读取任务及关联结果的轻量状态；任务不存在时返回 `not_found`。
    pub async fn task(&self, id: Uuid) -> Result<TaskRecord> {
        let row = sqlx::query("SELECT owner,workspace_id,method,state,sent,may_write,summary,error,CAST(created_at AS REAL),json_extract(params,'$._task_context'),CASE WHEN json_type(params,'$.target')='object' THEN json_extract(params,'$.target') END,CASE WHEN json_type(params,'$.targets')='array' THEN json_extract(params,'$.targets') END,COALESCE(json_extract(params,'$._execution_text_truncated'),0) FROM executions WHERE id=?")
            .bind(id.to_string()).fetch_optional(&self.pool).await?.ok_or_else(|| AppError::new("not_found", "任务不存在"))?;
        let owner = Uuid::parse_str(row.get::<&str, _>(0))
            .map_err(|e| AppError::new("storage", e.to_string()))?;
        let error: Option<String> = row.get(7);
        let result_summaries = self.result_summaries(id).await?;
        let summary: Json = serde_json::from_str(row.get::<&str, _>(6))?;
        let context: Option<String> = row.get(9);
        let mut context: Option<crate::model::TaskContext> = context
            .map(|value| serde_json::from_str(&value))
            .transpose()?;
        if let Some(context) = &mut context {
            let targets: Option<String> = row.get(11);
            let target: Option<String> = row.get(10);
            context.targets = if let Some(targets) = targets {
                serde_json::from_str(&targets).unwrap_or_default()
            } else if let Some(target) = target {
                serde_json::from_str(&target).ok().into_iter().collect()
            } else {
                Vec::new()
            };
        }
        Ok(TaskRecord {
            id,
            owner,
            workspace_id: row.get(1),
            method: row.get(2),
            text: None,
            execution_text_truncated: row.get::<i64, _>(12) != 0,
            started_at: summary.pointer("/_task/started_at").and_then(Json::as_f64),
            elapsed_ms: summary.pointer("/_task/elapsed_ms").and_then(Json::as_u64),
            context,
            created_at: row.get(8),
            state: row.get(3),
            sent: row.get::<i64, _>(4) != 0,
            may_write: row.get::<i64, _>(5) != 0,
            results: result_summaries.iter().map(|result| result.id).collect(),
            result_summaries,
            summary,
            error: error.map(|e| serde_json::from_str(&e)).transpose()?,
        })
    }

    /// 按需读取任务文本；普通任务查询不载入执行文本。
    pub async fn task_details(&self, id: Uuid) -> Result<TaskRecord> {
        let mut task = self.task(id).await?;
        task.text = sqlx::query_scalar("SELECT COALESCE(CASE WHEN json_type(params,'$.text')='text' THEN json_extract(params,'$.text') END, CASE WHEN json_type(params,'$._execution_text')='text' THEN json_extract(params,'$._execution_text') END) FROM executions WHERE id=?")
            .bind(id.to_string())
            .fetch_one(&self.pool)
            .await?;
        Ok(task)
    }

    /// 返回工作区最近的最多 500 条任务。
    pub async fn tasks(&self, workspace: &str) -> Result<Vec<TaskRecord>> {
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM executions WHERE workspace_id=? ORDER BY created_at DESC,rowid DESC LIMIT 500")
            .bind(workspace).fetch_all(&self.pool).await?;
        let mut tasks = Vec::with_capacity(ids.len());
        for id in ids {
            tasks.push(self.task(parse_id(&id)?).await?);
        }
        Ok(tasks)
    }

    /// 在真实执行路径追加有界文本，原始请求 text 始终独立保留。
    pub async fn append_execution_text(&self, id: Uuid, text: &str, truncated: bool) -> Result<()> {
        const LIMIT: i64 = 65536;
        sqlx::query("UPDATE executions SET params=json_set(params,'$._execution_text',substr(COALESCE(json_extract(params,'$._execution_text'),'') || ?,1,?),'$._execution_text_truncated',json(CASE WHEN ? OR COALESCE(json_extract(params,'$._execution_text_truncated'),0) OR length(COALESCE(json_extract(params,'$._execution_text'),'') || ?) > ? THEN 'true' ELSE 'false' END)) WHERE id=?")
            .bind(text).bind(LIMIT).bind(truncated).bind(text).bind(LIMIT).bind(id.to_string()).execute(&self.pool).await?;
        Ok(())
    }

    /// 持久化任务状态、摘要和可选错误。
    pub async fn update_task(
        &self,
        id: Uuid,
        state: &str,
        summary: Json,
        error: Option<AppError>,
    ) -> Result<()> {
        sqlx::query("UPDATE executions SET state=?,summary=?,error=?,updated_at=unixepoch('subsec') WHERE id=?")
            .bind(state).bind(summary.to_string()).bind(error.map(|e| serde_json::to_string(&e)).transpose()?)
            .bind(id.to_string()).execute(&self.pool).await?;
        Ok(())
    }
    /// 持久化发送阶段；`may_write` 只会从假变真。
    pub async fn mark_sent(&self, id: Uuid, may_write: bool) -> Result<()> {
        sqlx::query("UPDATE executions SET sent=1,may_write=MAX(may_write,?),summary=CASE WHEN json_extract(params,'$._task_context.source')='archery' OR json_type(summary,'$.remote_completion') IS NOT NULL THEN json_set(CASE WHEN json_type(summary)='object' THEN summary ELSE '{}' END,'$.remote_completion','unknown') ELSE summary END,updated_at=unixepoch('subsec') WHERE id=?")
            .bind(i64::from(may_write)).bind(id.to_string()).execute(&self.pool).await?;
        Ok(())
    }
    /// 已验证完整平台响应；后续取消或本地落盘失败不能丢失该事实。
    pub async fn mark_remote_completed(&self, id: Uuid) -> Result<()> {
        let updated = sqlx::query("UPDATE executions SET summary=json_set(CASE WHEN json_type(summary)='object' THEN summary ELSE '{}' END,'$.remote_completion','completed'),updated_at=unixepoch('subsec') WHERE id=? AND sent=1")
            .bind(id.to_string()).execute(&self.pool).await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::invalid("任务尚未发送或已不存在"));
        }
        Ok(())
    }
    /// 将中断任务恢复为取消或待确认；已发送且可能写入的任务需人工核对。
    pub async fn recover_tasks(&self) -> Result<()> {
        let unknown = serde_json::to_string(&AppError::unknown(
            "Engine 已中断，数据库写入结果需人工核对",
        ))?;
        let interrupted = serde_json::to_string(&AppError::new(
            "interrupted",
            "Engine 已中断，原会话不可恢复",
        ))?;
        sqlx::query("UPDATE executions SET state=CASE WHEN json_extract(params,'$._task_context.source')='archery' THEN 'failed' WHEN sent=1 AND may_write=1 THEN 'outcome_unknown' ELSE 'cancelled' END,error=CASE WHEN json_extract(params,'$._task_context.source')='archery' THEN ? WHEN sent=1 AND may_write=1 THEN ? ELSE ? END WHERE state IN ('queued','running','cancelling')")
            .bind(&interrupted).bind(unknown).bind(interrupted).execute(&self.pool).await?;
        Ok(())
    }
    /// 返回未分页授权之前的候选，调用方必须先过滤再统计和输出。
    pub async fn history_candidates(
        &self,
        workspace: &str,
        search: &str,
        offset: u64,
        limit: u64,
    ) -> Result<Vec<TaskRecord>> {
        if limit == 0 || limit > 1000 || offset > i64::MAX as u64 {
            return Err(AppError::invalid("历史分页参数无效"));
        }
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM executions WHERE workspace_id=? AND method IN ('execution.start','execution.run') AND (instr(lower(COALESCE(json_extract(params,'$.text'),'')),lower(?))>0 OR instr(lower(COALESCE(json_extract(params,'$._execution_text'),'')),lower(?))>0 OR instr(lower(COALESCE(json_extract(params,'$._task_context'),'')),lower(?))>0) ORDER BY created_at DESC,rowid DESC LIMIT ? OFFSET ?")
            .bind(workspace).bind(search).bind(search).bind(search).bind(limit as i64).bind(offset as i64).fetch_all(&self.pool).await?;
        let mut tasks = Vec::with_capacity(ids.len());
        for id in ids {
            tasks.push(self.task_details(parse_id(&id)?).await?);
        }
        Ok(tasks)
    }
    pub async fn execution_proof(&self, id: Uuid) -> Result<Option<Json>> {
        let value: Option<Option<String>> = sqlx::query_scalar(
            "SELECT json_extract(params,'$._access_proof') FROM executions WHERE id=?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        value
            .ok_or_else(|| AppError::new("not_found", "任务不存在"))?
            .map(|v| serde_json::from_str(&v).map_err(Into::into))
            .transpose()
    }
    /// 在同一读取快照中合并有效状态、筛选、计数并分页。
    pub async fn history(
        &self,
        workspace: &str,
        offset: u64,
        limit: u64,
        search: &str,
        state_overrides: &HashMap<Uuid, &str>,
        search_states: &[String],
    ) -> Result<Json> {
        if limit == 0 || limit > 100 || offset > i64::MAX as u64 {
            return Err(AppError::invalid("历史分页参数无效"));
        }
        let overrides = serde_json::to_string(state_overrides)?;
        let states = serde_json::to_string(search_states)?;
        // 两个查询共用筛选条件，所有用户输入均作为值绑定。
        macro_rules! history_query {
            ($select:literal) => {
                concat!(
                    "WITH input AS (SELECT ? AS workspace, ? AS search, ? AS overrides, ? AS states), ",
                    "effective AS (SELECT executions.rowid AS ordering, executions.id, executions.params, ",
                    "COALESCE(overrides.value, executions.state) AS state, executions.created_at ",
                    "FROM executions CROSS JOIN input LEFT JOIN json_each(input.overrides) AS overrides ON overrides.key=executions.id ",
                    "WHERE executions.workspace_id=input.workspace AND executions.method IN ('execution.start','execution.run')), ",
                    "filtered AS (SELECT effective.* FROM effective CROSS JOIN input WHERE ",
                    "instr(lower(COALESCE(json_extract(params,'$.text'),'')),lower(input.search))>0 OR ",
                    "instr(lower(COALESCE(json_extract(params,'$._execution_text'),'')),lower(input.search))>0 OR ",
                    "instr(lower(COALESCE(json_extract(params,'$._task_context'),'')),lower(input.search))>0 OR ",
                    "instr(lower(state),lower(input.search))>0 OR ",
                    "instr(COALESCE(strftime('%Y-%m-%d %H:%M:%S', CASE WHEN CAST(created_at AS REAL)<CAST(CAST(created_at AS REAL) AS INTEGER) THEN CAST(CAST(created_at AS REAL) AS INTEGER)-1 ELSE CAST(CAST(created_at AS REAL) AS INTEGER) END, 'unixepoch', 'localtime'),''),input.search)>0 OR ",
                    "EXISTS (SELECT 1 FROM json_each(input.states) AS aliases WHERE aliases.value=effective.state)) ",
                    $select
                )
            };
        }
        let mut tx = self.pool.begin().await?;
        let total: i64 = sqlx::query_scalar(history_query!("SELECT count(*) FROM filtered"))
            .bind(workspace)
            .bind(search)
            .bind(&overrides)
            .bind(&states)
            .fetch_one(&mut *tx)
            .await?;
        // 预览在 SQLite 内截取，避免分页时把多条完整查询复制到 Rust 和 IPC。
        let rows = sqlx::query(history_query!("SELECT id,substr(COALESCE(CASE WHEN json_type(params,'$.text')='text' THEN json_extract(params,'$.text') END,CASE WHEN json_type(params,'$._execution_text')='text' THEN json_extract(params,'$._execution_text') END),1,1024),length(CAST(COALESCE(CASE WHEN json_type(params,'$.text')='text' THEN json_extract(params,'$.text') END,CASE WHEN json_type(params,'$._execution_text')='text' THEN json_extract(params,'$._execution_text') END) AS BLOB)),state,CAST(created_at AS REAL),json_extract(params,'$._task_context') FROM filtered ORDER BY created_at DESC,ordering DESC LIMIT ? OFFSET ?"))
            .bind(workspace).bind(search).bind(&overrides).bind(&states).bind(limit as i64).bind(offset as i64).fetch_all(&mut *tx).await?;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let text: Option<String> = row.get(1);
            let text_bytes: Option<i64> = row.get(2);
            let truncated = text_bytes
                .is_some_and(|bytes| bytes as usize > text.as_ref().map_or(0, String::len));
            let context: Option<String> = row.get(5);
            let context: Option<Json> = context
                .map(|value| serde_json::from_str(&value))
                .transpose()?;
            items.push(json!({"id":row.get::<&str,_>(0),"text":text,"text_truncated":truncated,"text_bytes":text_bytes,"state":row.get::<&str,_>(3),"created_at":row.get::<f64,_>(4),"context":context}));
        }
        tx.commit().await?;
        Ok(json!({"items":items,"total":total,"offset":offset,"limit":limit}))
    }
}

fn sanitize_params(mut value: Json) -> Json {
    fn clean(value: &mut Json) {
        match value {
            Json::Object(map) => {
                map.retain(|key, _| {
                    !matches!(
                        key.to_ascii_lowercase().as_str(),
                        "before"
                            | "rows"
                            | "result"
                            | "password"
                            | "secret"
                            | "token"
                            | "cookie"
                            | "otp"
                            | "totp"
                    )
                });
                for child in map.values_mut() {
                    clean(child);
                }
            }
            Json::Array(items) => {
                for item in items {
                    clean(item);
                }
            }
            _ => {}
        }
    }
    clean(&mut value);
    value
}
