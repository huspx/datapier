use super::*;

impl Store {
    /// 创建未完成的缓存结果，列定义与执行 ID 随结果保存。
    pub async fn start_result(
        &self,
        execution_id: Uuid,
        columns: Vec<Column>,
        kind: &str,
    ) -> Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO results(id,execution_id,columns,kind,row_count,complete,summary) VALUES(?,?,?,?,0,0,'null')")
            .bind(id.to_string()).bind(execution_id.to_string()).bind(serde_json::to_string(&columns)?)
            .bind(kind).execute(&self.cache).await?;
        Ok(id)
    }
    // 文件和 SQLite 行的修改共用结果锁，避免追加、投影和释放彼此交错。
    pub(super) async fn result_guard(&self, id: Uuid) -> ResultGuard {
        let (gate, registration) = {
            let mut gates = self.result_gates.lock().unwrap_or_else(|e| e.into_inner());
            let gate = gates.entry(id).or_insert_with(|| ResultGate {
                lock: Arc::new(Mutex::new(())),
                users: 0,
            });
            gate.users += 1;
            let registration = ResultGateRegistration {
                id,
                gates: self.result_gates.clone(),
            };
            (gate.lock.clone(), registration)
        };
        // 等待被取消时登记也会释放，不能只在成功释放结果时清理。
        let guard = gate.lock_owned().await;
        ResultGuard {
            _guard: guard,
            _registration: registration,
        }
    }
    /// 追加一批行；大值可能先写入外置文件，再将行写入缓存数据库。
    pub async fn append_rows(&self, id: Uuid, rows: Vec<Vec<Value>>) -> Result<()> {
        // 脱离调用方的等待后仍持有结果锁，直到文件写入和行提交结束。
        let store = self.clone();
        tokio::spawn(async move { store.append_rows_inner(id, rows).await })
            .await
            .map_err(|e| AppError::new("storage", e.to_string()))?
    }

    async fn append_rows_inner(&self, id: Uuid, rows: Vec<Vec<Value>>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let _guard = self.result_guard(id).await;
        self.result_meta(id).await?;
        let mut payloads = Vec::with_capacity(rows.len());
        for mut row in rows {
            let mut payload = serde_json::to_vec(&row)?;
            if payload.len() > 64 * 1024 {
                drop(payload);
                for value in &mut row {
                    let raw = serde_json::to_vec(value)?;
                    if raw.len() > 64 * 1024 {
                        *value = self.externalize(id, value, raw, 4096).await?;
                    }
                }
                payload = serde_json::to_vec(&row)?;
            }
            payloads.push(payload);
        }
        let mut connection = self.cache.acquire().await?;
        let result = async {
            // 先取得写锁，避免不同结果的读快照在升级写事务时互相冲突。
            let mut tx = connection.begin_with("BEGIN IMMEDIATE").await?;
            let start: i64 = sqlx::query_scalar("SELECT row_count FROM results WHERE id=?")
                .bind(id.to_string())
                .fetch_one(&mut *tx)
                .await?;
            let count = payloads.len();
            for (i, payload) in payloads.into_iter().enumerate() {
                sqlx::query("INSERT INTO result_rows(result_id,row_no,payload) VALUES(?,?,?)")
                    .bind(id.to_string())
                    .bind(start + i as i64)
                    .bind(payload)
                    .execute(&mut *tx)
                    .await?;
            }
            sqlx::query("UPDATE results SET row_count=row_count+? WHERE id=?")
                .bind(count as i64)
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok(())
        }
        .await;
        if result.is_err() {
            // SQLITE_FULL 等错误可能自动回滚；不复用驱动事务状态已不确定的连接。
            connection.close_on_drop();
        }
        result
    }
    /// 标记结果完整并保存摘要。
    pub async fn finish_result(&self, id: Uuid, summary: Json) -> Result<()> {
        let _guard = self.result_guard(id).await;
        sqlx::query("UPDATE results SET complete=1,summary=? WHERE id=?")
            .bind(summary.to_string())
            .bind(id.to_string())
            .execute(&self.cache)
            .await?;
        Ok(())
    }
    /// 读取结果元数据；结果已释放或不存在时返回 `not_found`。
    pub async fn result_meta(&self, id: Uuid) -> Result<ResultMeta> {
        let row = sqlx::query(
            "SELECT execution_id,columns,kind,row_count,complete,summary FROM results WHERE id=?",
        )
        .bind(id.to_string())
        .fetch_optional(&self.cache)
        .await?
        .ok_or_else(|| AppError::new("not_found", "结果已释放或不存在"))?;
        Ok(ResultMeta {
            id,
            execution_id: parse_id(row.get::<&str, _>(0))?,
            columns: serde_json::from_str(row.get::<&str, _>(1))?,
            kind: row.get(2),
            row_count: row.get::<i64, _>(3) as u64,
            complete: row.get::<i64, _>(4) != 0,
            summary: serde_json::from_str(row.get::<&str, _>(5))?,
        })
    }
    /// 按创建顺序读取执行关联的结果 ID。
    pub async fn result_ids(&self, task: Uuid) -> Result<Vec<Uuid>> {
        let ids: Vec<String> =
            sqlx::query_scalar("SELECT id FROM results WHERE execution_id=? ORDER BY rowid")
                .bind(task.to_string())
                .fetch_all(&self.cache)
                .await?;
        ids.iter().map(|id| parse_id(id)).collect()
    }
    /// 按现有结果页的行数与字节预算计算当前缓存的全部页起点。
    pub async fn result_pagination(
        &self,
        id: Uuid,
        page_size: usize,
        max_bytes: usize,
    ) -> Result<ResultPagination> {
        if !matches!(page_size, 20 | 50 | 100) {
            return Err(AppError::invalid("每页行数只能为 20、50 或 100"));
        }
        let store = self.clone();
        tokio::spawn(async move {
            store
                .result_pagination_inner(id, page_size, max_bytes)
                .await
        })
        .await
        .map_err(|e| AppError::new("storage", e.to_string()))?
    }

    async fn result_pagination_inner(
        &self,
        id: Uuid,
        page_size: usize,
        max_bytes: usize,
    ) -> Result<ResultPagination> {
        let _guard = self.result_guard(id).await;
        let result = self.result_meta(id).await?;
        let max_bytes = max_bytes.clamp(1024, 1024 * 1024);
        let base = Self::result_page_base(&result, max_bytes)?;
        // 先归一化放不进空页的行；外置后缩小的行可能重新放入前一页。
        let mut lengths: Vec<i64> = sqlx::query_scalar(
            "SELECT length(payload) FROM result_rows WHERE result_id=? ORDER BY row_no",
        )
        .bind(id.to_string())
        .fetch_all(&self.cache)
        .await?;
        for (index, length) in lengths.iter_mut().enumerate() {
            if base + *length as usize + 1 > max_bytes {
                let payload: Vec<u8> = sqlx::query_scalar(
                    "SELECT payload FROM result_rows WHERE result_id=? AND row_no=?",
                )
                .bind(id.to_string())
                .bind(index as i64)
                .fetch_one(&self.cache)
                .await?;
                let (_, size) = self
                    .project_result_row(id, index as u64, payload, base, max_bytes)
                    .await?;
                *length = (size - 1) as i64;
            }
        }
        let mut starts = vec![0];
        let mut bytes = base;
        let mut count = 0usize;
        for (index, length) in lengths.into_iter().enumerate() {
            let size = length as usize + 1;
            if count == page_size || (count > 0 && bytes + size > max_bytes) {
                starts.push(index as u64);
                bytes = base;
                count = 0;
            }
            bytes += size;
            count += 1;
        }
        Ok(ResultPagination {
            starts,
            row_count: result.row_count,
            complete: result.complete,
            page_size: page_size as u64,
        })
    }

    fn result_page_base(result: &ResultMeta, max_bytes: usize) -> Result<usize> {
        let bytes = serde_json::to_vec(result)?.len() + 128;
        if bytes > max_bytes {
            return Err(AppError::new(
                "result_too_wide",
                "列定义超过页大小，请增加页预算或减少查询列数",
            ));
        }
        Ok(bytes)
    }

    /// 按起始行取页，行数限制在 1..=1000，字节预算限制在 1 KiB..=1 MiB。
    /// 预算计入元数据；单行过宽时可将字段外置，仍无法容纳则返回错误。
    pub async fn result_page(
        &self,
        id: Uuid,
        start: u64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<ResultPage> {
        let store = self.clone();
        tokio::spawn(async move {
            store
                .result_page_inner(id, start, max_rows, max_bytes)
                .await
        })
        .await
        .map_err(|e| AppError::new("storage", e.to_string()))?
    }

    async fn result_page_inner(
        &self,
        id: Uuid,
        start: u64,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<ResultPage> {
        let _guard = self.result_guard(id).await;
        let result = self.result_meta(id).await?;
        let max_rows = max_rows.clamp(1, 1000);
        let max_bytes = max_bytes.clamp(1024, 1024 * 1024);
        let mut rows = Vec::new();
        let mut bytes = Self::result_page_base(&result, max_bytes)?;
        // SQLite 行号是有符号整数；越界偏移不能转换后回绕到结果开头。
        if start >= result.row_count {
            return Ok(ResultPage {
                result,
                rows,
                start,
                next: None,
            });
        }
        let mut first_over_budget = None;
        let mut stream = sqlx::query("SELECT payload FROM result_rows WHERE result_id=? AND row_no>=? ORDER BY row_no LIMIT ?")
            .bind(id.to_string()).bind(start as i64).bind(max_rows as i64).fetch(&self.cache);
        while let Some(row) = stream.try_next().await? {
            let payload: Vec<u8> = row.get(0);
            if bytes + payload.len() + 1 > max_bytes {
                if rows.is_empty() {
                    first_over_budget = Some(payload);
                }
                break;
            }
            bytes += payload.len() + 1;
            rows.push(serde_json::from_slice(&payload)?);
        }
        // 外置文件登记需要另一条连接，必须先释放读取连接，避免并发分页耗尽连接池。
        drop(stream);
        if let Some(payload) = first_over_budget {
            let (values, size) = self
                .project_result_row(id, start, payload, bytes, max_bytes)
                .await?;
            bytes += size;
            rows.push(values);
            // 投影首行后重新读取其余行，保持原有的行数和字节预算。
            if max_rows > 1 {
                let mut stream = sqlx::query("SELECT payload FROM result_rows WHERE result_id=? AND row_no>=? ORDER BY row_no LIMIT ?")
                    .bind(id.to_string()).bind((start + 1) as i64).bind((max_rows - 1) as i64).fetch(&self.cache);
                while let Some(row) = stream.try_next().await? {
                    let payload: Vec<u8> = row.get(0);
                    if bytes + payload.len() + 1 > max_bytes {
                        break;
                    }
                    bytes += payload.len() + 1;
                    rows.push(serde_json::from_slice(&payload)?);
                }
            }
        }
        let end = start + rows.len() as u64;
        let next = if end < result.row_count {
            Some(end)
        } else {
            None
        };
        Ok(ResultPage {
            result,
            rows,
            start,
            next,
        })
    }
    // 调用方持有结果锁；仅对不能放入空页的行读取和投影完整值。
    async fn project_result_row(
        &self,
        id: Uuid,
        start: u64,
        payload: Vec<u8>,
        bytes: usize,
        max_bytes: usize,
    ) -> Result<(Vec<Value>, usize)> {
        let mut values: Vec<Value> = serde_json::from_slice(&payload)?;
        for value in &mut values {
            if let Value::Large { preview, .. } = value {
                preview.clear();
            }
        }
        let mut size = serde_json::to_vec(&values)?.len() + 1;
        if bytes + size > max_bytes {
            for value in &mut values {
                if matches!(value, Value::Large { .. }) {
                    continue;
                }
                let raw = serde_json::to_vec(value)?;
                let reference = Value::Large {
                    id: Uuid::nil(),
                    original_type: value_type(value).into(),
                    bytes: raw.len() as u64,
                    preview: String::new(),
                };
                let reference_size = serde_json::to_vec(&reference)?.len();
                if reference_size >= raw.len() {
                    continue;
                }
                size = size - raw.len() + reference_size;
                *value = self.externalize(id, value, raw, 0).await?;
                if bytes + size <= max_bytes {
                    break;
                }
            }
        }
        sqlx::query("UPDATE result_rows SET payload=? WHERE result_id=? AND row_no=?")
            .bind(serde_json::to_vec(&values)?)
            .bind(id.to_string())
            .bind(start as i64)
            .execute(&self.cache)
            .await?;
        if bytes + size > max_bytes {
            return Err(AppError::new(
                "result_too_wide",
                "单行引用和列定义超过页大小，请减少查询列数",
            ));
        }
        Ok((values, size))
    }
    /// 逻辑释放结果后删除外置文件；失败时保留文件清理记录，可用同一 ID 重试。
    pub async fn release_result(&self, id: Uuid) -> Result<()> {
        let _guard = self.result_guard(id).await;
        let mut tx = self.cache.begin().await?;
        // schema 1 的文件没有登记；在删除行之前将其纳入同一事务。
        let mut after = -1_i64;
        loop {
            let rows = sqlx::query("SELECT row_no,payload FROM result_rows WHERE result_id=? AND row_no>? ORDER BY row_no LIMIT 128")
                .bind(id.to_string()).bind(after).fetch_all(&mut *tx).await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                after = row.get(0);
                let values: Vec<Value> = serde_json::from_slice(row.get::<&[u8], _>(1))?;
                for value in values {
                    if let Value::Large { id: value_id, .. } = value {
                        sqlx::query("INSERT OR IGNORE INTO value_files(id,result_id) VALUES(?,?)")
                            .bind(value_id.to_string())
                            .bind(id.to_string())
                            .execute(&mut *tx)
                            .await?;
                    }
                }
            }
        }
        sqlx::query("DELETE FROM result_rows WHERE result_id=?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM results WHERE id=?")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        loop {
            let file: Option<String> = sqlx::query_scalar(
                "SELECT id FROM value_files WHERE result_id=? ORDER BY id LIMIT 1",
            )
            .bind(id.to_string())
            .fetch_optional(&self.cache)
            .await?;
            let Some(file) = file else {
                break;
            };
            match tokio::fs::remove_file(self.root.join("values").join(&file)).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            sqlx::query("DELETE FROM value_files WHERE id=?")
                .bind(file)
                .execute(&self.cache)
                .await?;
        }
        Ok(())
    }
}
