use super::*;

struct RedisExportWriter {
    out: BufWriter<File>,
    csv: Option<csv::Writer<Vec<u8>>>,
    first: bool,
}

pub(super) async fn export_redis(
    session: &mut ProviderSession,
    params: &Json,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    fmt: &str,
    file: File,
) -> Result<()> {
    let mut out = BufWriter::new(file);
    let csv = if fmt == "csv" {
        let mut c = csv::Writer::from_writer(Vec::new());
        c.write_record(REDIS_HEADER).map_err(csv_error)?;
        Some(c)
    } else {
        out.write_all(b"{\"format\":\"datapier\",\"version\":1,\"records\":[")?;
        None
    };
    let mut writer = RedisExportWriter {
        out,
        csv,
        first: true,
    };
    let requested_target: Option<ResourceTarget> = params
        .get("target")
        .filter(|v| !v.is_null())
        .map(|v| serde_json::from_value(v.clone()))
        .transpose()?;
    if let Some(db) = requested_target
        .as_ref()
        .and_then(|t| t.database.as_deref())
        && db.parse::<u32>().ok() != Some(session.profile().redis_db())
    {
        return Err(AppError::invalid("目标数据库与连接不一致"));
    }
    if let Some(target) = requested_target.as_ref().filter(|t| t.key_b64.is_some()) {
        export_key(session, target, sink, cancel, p, &mut writer).await?;
    } else {
        // SCAN 可能重复返回 key；以磁盘索引去重，避免将全库 key 保存在内存。
        let seen_path = temp_path(&sink.store.root.join("transfer-scan"), "scan");
        let opts = SqliteConnectOptions::new()
            .filename(&seen_path)
            .create_if_missing(true);
        let mut seen = SqliteConnection::connect_with(&opts).await?;
        let scan_result: Result<()> = async {
            sqlx::query("CREATE TABLE seen (key TEXT PRIMARY KEY)")
                .execute(&mut seen)
                .await?;
            let mut cursor: Option<String> = None;
            loop { ensure_not_cancelled(cancel)?;
                let page: ResourcePage = serde_json::from_value(session.call("resource.list", json!({"cursor":cursor,"pattern":params.get("pattern"),"type":params.get("type"),"limit":500}), sink, cancel).await?)?;
                for resource in page.resources { let key = resource.key_b64.ok_or_else(|| AppError::invalid("Redis 资源缺少 key_b64"))?;
                    let inserted = sqlx::query("INSERT OR IGNORE INTO seen(key) VALUES (?)").bind(&key).execute(&mut seen).await?.rows_affected();
                    if inserted != 0 { let target = ResourceTarget { database: resource.database, name: resource.name, key_b64: Some(key), kind: resource.kind }; export_key(session, &target, sink, cancel, p, &mut writer).await?; }
                }
                if page.complete || page.cursor.is_none() { break; } cursor = page.cursor;
            } Ok(())
        }.await;
        let close_result = seen.close().await;
        let _ = fs::remove_file(seen_path);
        scan_result?;
        close_result?;
    }
    if let Some(c) = writer.csv.as_mut() {
        flush_csv(c, &mut writer.out, true)?;
    } else {
        writer.out.write_all(b"]}")?;
    }
    writer.out.flush()?;
    writer.out.get_ref().sync_all()?;
    Ok(())
}
async fn export_key(
    session: &mut ProviderSession,
    target: &ResourceTarget,
    sink: &ExecutionSink,
    cancel: &CancellationToken,
    p: &mut Progress,
    writer: &mut RedisExportWriter,
) -> Result<()> {
    let RedisExportWriter { out, csv, first } = writer;
    ensure_not_cancelled(cancel)?;
    let key = target
        .key_b64
        .as_deref()
        .ok_or_else(|| AppError::invalid("Redis target 缺少 key_b64"))?;
    let meta = session
        .call("export.meta", json!({"target":target}), sink, cancel)
        .await?;
    let ty = meta
        .get("type")
        .and_then(Json::as_str)
        .ok_or_else(|| AppError::invalid("Redis key 缺少类型"))?;
    if !matches!(ty, "string" | "hash" | "list" | "set" | "zset") {
        return Err(AppError::invalid(format!("不支持 Redis 类型 {ty}")));
    }
    // 导出绝对过期时间，导入时不会因传输耗时延长 key 生命周期。
    let expiry = meta.get("expires_at_ms").cloned().unwrap_or(Json::Null);
    let exp = expiry.as_str().unwrap_or("permanent");
    let browsed = session
        .call("resource.browse", json!({"target":target}), sink, cancel)
        .await?;
    validate_redis_browse_type(ty, &browsed)?;
    let id = first_result(&browsed)?;
    let result_meta = sink.store.result_meta(id).await?;
    if !result_meta.complete {
        return Err(AppError::new("incomplete_result", "Redis 浏览结果不完整"));
    }
    if let Some(c) = csv.as_mut() {
        c.write_record(["1", "key", key, ty, exp, "", "", "", "", ""])
            .map_err(csv_error)?;
        flush_csv(c, out, false)?;
    } else {
        if !*first {
            out.write_all(b",")?;
        }
        *first = false;
        write!(
            out,
            "{{\"key_b64\":{},\"type\":{},\"expires_at_ms\":{},\"rows\":[",
            serde_json::to_string(key)?,
            serde_json::to_string(ty)?,
            expiry
        )?;
    }
    let mut row_first = true;
    let mut start = 0;
    let mut row_count = 0u64;
    loop {
        ensure_not_cancelled(cancel)?;
        let page = sink.store.result_page(id, start, PAGE, BYTES).await?;
        for row in page.rows {
            let row = full_row(sink, row).await?;
            let fields = redis_csv_row(key, ty, &row)?;
            if ty == "list" && fields[6].parse::<u64>().ok() != Some(row_count) {
                return Err(AppError::conflict("Redis List 索引不连续"));
            }
            for value in &row {
                if let Value::Bytes(s) = value {
                    valid_b64(s)?;
                }
            }
            if ty == "string" && row_count != 0 {
                return Err(AppError::conflict("Redis String 包含多行"));
            }
            if let Some(c) = csv.as_mut() {
                c.write_record(fields).map_err(csv_error)?;
                flush_csv(c, out, false)?;
            } else {
                if !row_first {
                    out.write_all(b",")?;
                }
                serde_json::to_writer(&mut *out, &row)?;
                row_first = false;
            }
            p.rows += 1;
            row_count += 1;
        }
        match page.next {
            Some(n) if n > start => start = n,
            Some(_) => return Err(AppError::new("result_page", "结果分页没有推进")),
            None => break,
        }
    }
    if row_count == 0 {
        return Err(AppError::conflict("Redis key 在导出期间消失或内容为空"));
    }
    if csv.is_none() {
        out.write_all(b"]}")?;
    }
    p.keys += 1;
    Ok(())
}
