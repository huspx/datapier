//! MCP 结果读取：重新验证执行权限，并将分页与字段句柄绑定到客户端。
use super::*;

fn collect_large(value: &model::Value, ids: &mut Vec<Uuid>) {
    match value {
        model::Value::Large { id, .. } => ids.push(*id),
        model::Value::Array(values) => values.iter().for_each(|value| collect_large(value, ids)),
        model::Value::Map(entries) => entries.iter().for_each(|(key, value)| {
            collect_large(key, ids);
            collect_large(value, ids);
        }),
        _ => {}
    }
}

impl Engine {
    pub(super) async fn mcp_result(
        &self,
        owner: Uuid,
        result_id: Uuid,
    ) -> Result<(model::ResultMeta, model::TaskRecord, AccessProof)> {
        let meta = self.store.result_meta(result_id).await?;
        let task = self.store.task(meta.execution_id).await?;
        if task.owner != owner {
            return Err(denied("结果不属于当前客户端"));
        }
        let raw = self
            .store
            .execution_proof(meta.execution_id)
            .await?
            .ok_or_else(|| denied("结果缺少访问证明"))?;
        let proof: AccessProof =
            serde_json::from_value(raw).map_err(|_| denied("结果访问证明无效"))?;
        self.database(owner, &json!({"session_id":proof.session_id}))?;
        self.mcp_validate_proof(owner, &proof, false).await?;
        Ok((meta, task, proof))
    }

    pub(super) async fn mcp_read(self: &Arc<Self>, owner: Uuid, params: &Value) -> Result<Value> {
        let result_id: Uuid = model::param(params, "result_id")?;
        let (_, task, proof) = self.mcp_result(owner, result_id).await?;
        let limits = self.store.settings().await?.limits;
        let cursor = self.mcp_cursor(owner, "result.read", result_id, params)?;
        let default_size = cursor
            .as_ref()
            .map_or(limits.mcp_response_rows.min(100), |c| c.page_size);
        let (page, size) = paging(params, default_size, limits.mcp_response_rows)?;
        if let Some(cursor) = &cursor
            && (cursor.page_size != size || params.get("page").is_some())
        {
            return Err(AppError::invalid("游标与分页参数不匹配"));
        }
        let start = if let Some(cursor) = &cursor {
            cursor.start
        } else {
            let index = usize::try_from(page - 1).map_err(|_| AppError::invalid("页码过大"))?;
            if index == 0 {
                0
            } else {
                let state = self.state.lock().unwrap();
                *state
                    .mcp
                    .pages
                    .get(&(owner, result_id, size))
                    .and_then(|starts| starts.get(index))
                    .ok_or_else(|| AppError::invalid("请先读取前一页以确定页边界"))?
            }
        };
        let response = self.dispatch_internal(owner, "result.read", json!({"result_id":result_id,"session_id":proof.session_id,"start":start,"max_rows":size,"max_bytes":limits.response_bytes})).await?;
        let mut result: model::ResultPage = serde_json::from_value(response)?;
        if result.result.id != result_id
            || result.result.execution_id != task.id
            || result.start != start
        {
            return Err(AppError::new("internal", "结果页与请求不匹配"));
        }
        // 读取过程中会话、权限或结果可能发生变化。
        self.mcp_result(owner, result_id).await?;
        let mut next = result.next;
        let mut output;
        loop {
            let summary = &result.result.summary;
            output = json!({
                "execution_id":task.id,"session_id":proof.session_id,"result_id":result_id,
                "state":task.state,"retrieval_mode":summary.get("retrieval_mode").and_then(Value::as_str).unwrap_or("bounded_response"),
                "columns":result.result.columns,"rows":result.rows,"row_count":result.rows.len(),
                "has_more":next.is_some(),"next_cursor":next.map(|_| Uuid::nil()),
                "total_rows":summary.get("total_rows"),"counted_at":summary.get("counted_at"),
                "origin":summary.get("origin"),"coverage":summary.get("coverage"),"fidelity":summary.get("fidelity"),
                "masking":summary.get("masking"),"complete":result.result.complete,
                "resource_limited":summary.get("resource_limited").and_then(Value::as_bool).unwrap_or(false),
                "retained_prefix":summary.get("retained_prefix").and_then(Value::as_bool).unwrap_or(false)
            });
            if encoded_len(&output)? <= limits.response_bytes {
                break;
            }
            if result.rows.pop().is_none() {
                return Err(AppError::new(
                    "response_limit",
                    "结果元数据超过响应字节限制",
                ));
            }
            next = Some(start + result.rows.len() as u64);
            if result.rows.is_empty() {
                return Err(AppError::new("response_limit", "结果行超过响应字节限制"));
            }
        }
        let mut ids = Vec::new();
        for row in &result.rows {
            for value in row {
                collect_large(value, &mut ids);
            }
        }
        if let Some(next_start) = next {
            if next_start <= start {
                return Err(AppError::new("internal", "结果页未前进"));
            }
            let token = self.mcp_next_cursor(
                owner,
                "result.read",
                result_id,
                next_start,
                size,
                Value::Null,
            );
            output["next_cursor"] = json!(token);
            let mut state = self.state.lock().unwrap();
            let starts = state
                .mcp
                .pages
                .entry((owner, result_id, size))
                .or_insert_with(|| vec![0]);
            if let Some(index) = starts.iter().position(|value| *value == start)
                && starts.get(index + 1) != Some(&next_start)
            {
                starts.truncate(index + 1);
                starts.push(next_start);
            }
            for id in ids {
                state.mcp.values.insert(id, (owner, result_id));
            }
        } else {
            let mut state = self.state.lock().unwrap();
            for id in ids {
                state.mcp.values.insert(id, (owner, result_id));
            }
        }
        Ok(output)
    }

    pub(super) async fn mcp_count(self: &Arc<Self>, owner: Uuid, params: &Value) -> Result<Value> {
        let result_id: Uuid = model::param(params, "result_id")?;
        let (_, task, proof) = self.mcp_result(owner, result_id).await?;
        let count = self
            .dispatch_internal(
                owner,
                "result.count",
                json!({"result_id":result_id,"session_id":proof.session_id}),
            )
            .await?;
        self.mcp_result(owner, result_id).await?;
        Ok(
            json!({"execution_id":task.id,"session_id":proof.session_id,"result_id":result_id,"state":task.state,
            "total_rows":count.get("total_rows"),"counted_at":count.get("counted_at")}),
        )
    }

    pub(super) async fn mcp_value(self: &Arc<Self>, owner: Uuid, params: &Value) -> Result<Value> {
        let value_id: Uuid = model::param(params, "value_id")?;
        let result_id = {
            let state = self.state.lock().unwrap();
            state
                .mcp
                .values
                .get(&value_id)
                .filter(|(client, _)| *client == owner)
                .map(|(_, result)| *result)
                .ok_or_else(|| denied("字段未向当前客户端公开"))?
        };
        let (_, task, proof) = self.mcp_result(owner, result_id).await?;
        if self.store.value_result_id(value_id).await? != result_id {
            return Err(denied("字段不属于当前结果"));
        }
        let offset = optional::<u64>(params, "offset")?.unwrap_or(0);
        let limit = self.store.settings().await?.limits.response_bytes;
        let maximum = (limit.saturating_sub(512) / 4 * 3).min(262144);
        let requested = optional::<usize>(params, "max_bytes")?.unwrap_or(maximum);
        if requested == 0 || requested > maximum {
            return Err(AppError::invalid("字段块大小超过响应限制"));
        }
        let chunk = self.dispatch_internal(owner, "result.value", json!({"session_id":proof.session_id,"result_id":result_id,"value_id":value_id,"offset":offset,"max_bytes":requested})).await?;
        self.mcp_result(owner, result_id).await?;
        if self.store.value_result_id(value_id).await? != result_id {
            return Err(denied("字段不属于当前结果"));
        }
        Ok(
            json!({"execution_id":task.id,"session_id":proof.session_id,"result_id":result_id,"state":task.state,"value_id":value_id,"encoding":"base64",
            "content_type":chunk.get("content_type"),"offset":chunk.get("offset"),"data_b64":chunk.get("data_b64"),"next":chunk.get("next"),"total_bytes":chunk.get("total_bytes")}),
        )
    }

    pub(super) async fn mcp_release(
        self: &Arc<Self>,
        owner: Uuid,
        params: &Value,
    ) -> Result<Value> {
        let result_id: Uuid = model::param(params, "result_id")?;
        let (_, task, proof) = self.mcp_result(owner, result_id).await?;
        self.dispatch_internal(
            owner,
            "result.release",
            json!({"result_id":result_id,"session_id":proof.session_id}),
        )
        .await?;
        let mut state = self.state.lock().unwrap();
        state.mcp.cursors.retain(|_, cursor| {
            !(cursor.owner == owner
                && cursor.resource == result_id
                && cursor.operation == "result.read")
        });
        state
            .mcp
            .pages
            .retain(|(client, result, _), _| *client != owner || *result != result_id);
        state
            .mcp
            .values
            .retain(|_, (client, result)| *client != owner || *result != result_id);
        Ok(
            json!({"execution_id":task.id,"session_id":proof.session_id,"result_id":result_id,"state":task.state,"released":true}),
        )
    }
}
