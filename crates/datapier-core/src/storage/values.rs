use super::*;

impl Store {
    // 保存类型化值的 JSON 字节，而非展示文本，供后续按需完整还原。
    pub(super) async fn externalize(
        &self,
        result_id: Uuid,
        value: &Value,
        raw: Vec<u8>,
        preview_size: usize,
    ) -> Result<Value> {
        self.externalize_file(result_id, Uuid::new_v4(), value, raw, preview_size)
            .await
    }

    pub(super) async fn externalize_file(
        &self,
        result_id: Uuid,
        id: Uuid,
        value: &Value,
        raw: Vec<u8>,
        preview_size: usize,
    ) -> Result<Value> {
        let original_type = value_type(value);
        // 预览预算按展示文本的 UTF-8 字节计量，外置文件仍保存完整类型化 JSON。
        let prefix = |text: &str, limit: usize| {
            let mut end = text.len().min(limit);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            end
        };
        let preview = match value {
            Value::Integer(text)
            | Value::Unsigned(text)
            | Value::Decimal(text)
            | Value::Float(text)
            | Value::Text(text)
            | Value::Date(text)
            | Value::Time(text)
            | Value::DateTime(text) => text[..prefix(text, preview_size)].to_owned(),
            Value::Bytes(text) => {
                let label = "base64:";
                let label_end = prefix(label, preview_size);
                let text_end = prefix(text, preview_size - label_end);
                format!("{}{}", &label[..label_end], &text[..text_end])
            }
            _ => {
                let text = value.text();
                text[..prefix(&text, preview_size)].to_owned()
            }
        };
        // 先持久化归属：写入失败或 future 被取消时仍可由 release_result 重试清理。
        sqlx::query("INSERT INTO value_files(id,result_id) VALUES(?,?)")
            .bind(id.to_string())
            .bind(result_id.to_string())
            .execute(&self.cache)
            .await?;
        // 文件异步操作由持锁任务等待 flush，取消外层调用不会提前释放结果锁。
        let mut file =
            tokio::fs::File::create(self.root.join("values").join(id.to_string())).await?;
        file.write_all(&raw).await?;
        file.flush().await?;
        Ok(Value::Large {
            id,
            original_type: original_type.into(),
            bytes: raw.len() as u64,
            preview,
        })
    }
    /// 读取外置字段的完整类型化值；引用文件不存在或损坏时返回错误。
    ///
    /// 调用方需保证所属结果尚未释放；字段 ID 不是独立于结果生命周期的持久标识。
    pub async fn read_value(&self, id: Uuid) -> Result<Value> {
        self.ensure_value_available(id).await?;
        let raw = tokio::fs::read(self.root.join("values").join(id.to_string())).await?;
        Ok(serde_json::from_slice(&raw)?)
    }
    /// 按字节读取外置字段的 JSON 编码，返回 Base64 分块和下一偏移。
    /// 单块上限为 256 KiB；偏移超过文件长度时返回输入错误。
    ///
    /// 引用不阻止结果释放，读取与释放并发时可能返回文件已不存在的 I/O 错误。
    pub async fn large_value(&self, id: Uuid, offset: u64, max_bytes: usize) -> Result<Json> {
        self.ensure_value_available(id).await?;
        let mut file = tokio::fs::File::open(self.root.join("values").join(id.to_string())).await?;
        let total = file.metadata().await?.len();
        if offset > total {
            return Err(AppError::invalid("字段分块偏移超出范围"));
        }
        file.seek(std::io::SeekFrom::Start(offset)).await?;
        let length = ((total - offset) as usize).min(max_bytes.clamp(1, 256 * 1024));
        let mut buffer = vec![0; length];
        file.read_exact(&mut buffer).await?;
        let end = offset + length as u64;
        Ok(
            json!({"bytes_b64":STANDARD.encode(buffer),"offset":offset,"next":if end<total {Some(end)} else {None},"total":total}),
        )
    }
    async fn ensure_value_available(&self, id: Uuid) -> Result<()> {
        let released: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM value_files v WHERE v.id=? AND NOT EXISTS (SELECT 1 FROM results r WHERE r.id=v.result_id))")
            .bind(id.to_string()).fetch_one(&self.cache).await?;
        if released {
            return Err(AppError::new("not_found", "结果已释放或不存在"));
        }
        Ok(())
    }
}
