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
        out.write_all(b"{\"format\":\"datapier\",\"version\":2,\"records\":[")?;
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
        let mut cursor: Option<String> = None;
        loop {
            ensure_not_cancelled(cancel)?;
            let response = session.call("resource.list", json!({"cursor":cursor,"pattern":params.get("pattern"),"type":params.get("type"),"limit":500}), sink, cancel).await?;
            if response.get("resource_limited").and_then(Json::as_bool) == Some(true) {
                return Err(AppError::new(
                    "resource_limit",
                    "Redis 导出扫描超过内存预算",
                ));
            }
            let page: ResourcePage = serde_json::from_value(response)?;
            for resource in page.resources {
                let key = resource
                    .key_b64
                    .ok_or_else(|| AppError::invalid("Redis 资源缺少 key_b64"))?;
                let target = ResourceTarget {
                    database: resource.database,
                    name: resource.name,
                    key_b64: Some(key),
                    kind: resource.kind,
                };
                export_key(session, &target, sink, cancel, p, &mut writer).await?;
            }
            if page.complete || page.cursor.is_none() {
                break;
            }
            cursor = page.cursor;
        }
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
    if let Some(c) = csv.as_mut() {
        c.write_record(["2", "key", key, ty, exp, "", "", "", "", "", ""])
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
    if ty == "string" {
        if let Some(c) = csv.as_mut() {
            flush_csv(c, out, true)?;
            let mut prefix = csv::Writer::from_writer(Vec::new());
            prefix
                .write_record(redis_csv_row(key, ty, &[Value::Bytes("AAAA".into())])?)
                .map_err(csv_error)?;
            prefix.flush()?;
            let encoded = prefix.get_ref();
            if !encoded.ends_with(b"AAAA,\n") {
                return Err(AppError::new("csv", "Redis CSV 前缀无效"));
            }
            out.write_all(&encoded[..encoded.len() - 6])?;
        } else {
            out.write_all(b"[{\"type\":\"bytes\",\"value\":\"")?;
        }
        let mut cursor = None;
        loop {
            ensure_not_cancelled(cancel)?;
            let page = session
                .call(
                    "export.read",
                    json!({"target":target,"type":ty,"cursor":cursor}),
                    sink,
                    cancel,
                )
                .await?;
            let rows: Vec<Vec<Value>> = serde_json::from_value(
                page.get("rows")
                    .cloned()
                    .ok_or_else(|| AppError::invalid("Redis 导出缺少数据"))?,
            )?;
            let [Value::Bytes(chunk)] = rows
                .first()
                .map(Vec::as_slice)
                .ok_or_else(|| AppError::invalid("Redis String 导出响应无效"))?
            else {
                return Err(AppError::invalid("Redis String 导出响应无效"));
            };
            valid_b64(chunk)?;
            out.write_all(chunk.as_bytes())?;
            cursor = page.get("next").and_then(Json::as_u64);
            if cursor.is_none() {
                break;
            }
        }
        if csv.is_some() {
            out.write_all(b",\n")?;
        } else {
            out.write_all(b"\"}]]}")?;
        }
        p.rows += 1;
        p.keys += 1;
        return Ok(());
    }
    let mut row_first = true;
    let mut cursor = None;
    let mut row_count = 0u64;
    let mut seen = HashSet::new();
    let mut seen_bytes = 0usize;
    let budget = sink
        .store
        .settings()
        .await?
        .limits
        .result_memory_bytes_per_client as usize;
    let reservation = Uuid::new_v4();
    let scan: Result<()> = async {
        loop {
            ensure_not_cancelled(cancel)?;
            let page = session
                .call(
                    "export.read",
                    json!({"target":target,"type":ty,"cursor":cursor}),
                    sink,
                    cancel,
                )
                .await?;
            let rows: Vec<Vec<Value>> = serde_json::from_value(
                page.get("rows")
                    .cloned()
                    .ok_or_else(|| AppError::invalid("Redis 导出缺少数据"))?,
            )?;
            for row in rows {
                if matches!(ty, "hash" | "set" | "zset") {
                    let member = serde_json::to_vec(&row[0])?;
                    if seen.contains(&member) {
                        continue;
                    }
                    seen_bytes = seen_bytes.saturating_add(member.len()).saturating_add(64);
                    if seen_bytes > budget {
                        return Err(AppError::new(
                            "resource_limit",
                            "Redis 导出扫描超过内存预算",
                        ));
                    }
                    sink.store
                        .reserve_auxiliary(sink.execution_id, reservation, seen_bytes)
                        .await?;
                    seen.insert(member);
                }
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
            cursor = page.get("next").and_then(Json::as_u64);
            if cursor.is_none() {
                break;
            }
        }
        Ok(())
    }
    .await;
    sink.store.release_auxiliary(reservation).await?;
    scan?;
    if row_count == 0 {
        return Err(AppError::conflict("Redis key 在导出期间消失或内容为空"));
    }
    if csv.is_none() {
        out.write_all(b"]}")?;
    }
    p.keys += 1;
    Ok(())
}
