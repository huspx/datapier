//! 数据交换报告编码与产物收尾。
use super::*;

impl Engine {
    pub(super) async fn transfer_report_export(
        &self,
        owner: Uuid,
        params: &Value,
    ) -> Result<Value> {
        use std::io::Write;
        let id: Uuid = model::param(params, "run_id")?;
        let (run, storage_error) = self.transfer_owned_run(owner, id).await?;
        if matches!(run.state.as_str(), "running" | "cancelling") {
            return Err(AppError::new("busy", "运行中的任务不能导出最终报告"));
        }
        let format: String = optional(params, "format")?.unwrap_or_else(|| "json".into());
        if !matches!(format.as_str(), "json" | "csv") {
            return Err(AppError::invalid("报告格式仅支持 JSON 或 CSV"));
        }
        let csv = format == "csv";
        let report_run = transfer_run_value(&run, storage_error.as_ref())?;
        let media = if csv { "text/csv" } else { "application/json" };
        let include: bool = optional(params, "include_failed_input")?.unwrap_or(false);
        let mut spec: Option<TransferSpec> = if include {
            Some(model::param(params, "spec")?)
        } else {
            None
        };
        if let Some(spec) = &spec
            && migration::digest(spec)? != run.spec_digest
        {
            return Err(AppError::new("conflict", "报告配置与原任务不同"));
        }
        if let Some(paths) = optional::<Vec<std::path::PathBuf>>(params, "inputs")? {
            let spec = spec
                .as_mut()
                .ok_or_else(|| AppError::invalid("替代输入路径需要包含失败输入"))?;
            if paths.len() != spec.inputs.len() {
                return Err(AppError::invalid("替代输入路径数量不符"));
            }
            for (input, path) in spec.inputs.iter_mut().zip(paths) {
                input.path = path;
            }
        }
        if let Some(spec) = &spec
            && spec.inputs.len() != run.context.inputs.len()
        {
            return Err(AppError::invalid("输入数量与原任务不同"));
        }
        let path: std::path::PathBuf = model::param(params, "path")?;
        let mut initial = datapier_core::execution::data_files::AtomicOutput::create(&path)?;
        let baseline = initial.snapshot(false)?;
        let partial = initial.partial_path().to_path_buf();
        let identity = json!({"path":baseline.canonical_path,"canonical_path":baseline.canonical_path,"file_id":baseline.file_id,"size":baseline.size,"export_name":path.file_name().and_then(|v|v.to_str()),"media_type":media});
        let artifact = self
            .store
            .transfer_register_artifact(owner, Some(id), "report", &identity)
            .await?;
        let mut output = Some(initial);
        let result: Result<()> = async {
            let writer = output.as_mut().unwrap();
            if csv {
                write!(writer, "section,payload\r\nrun,\"")?;
                serde_json::to_writer(CsvQuoted(&mut *writer), &report_run)?;
                write!(writer, "\"\r\n")?;
            } else {
                write!(writer, "{{\"run\":")?;
                serde_json::to_writer(&mut *writer, &report_run)?;
                write!(writer, ",\"items\":[")?;
            }
            let mut first = true;
            let mut after = None;
            loop {
                let items = self.store.transfer_items(id, after, 250).await?;
                if items.is_empty() {
                    break;
                }
                after = items.last().map(|v| v.ordinal);
                for item in items {
                    let writer = output.as_mut().unwrap();
                    if csv {
                        write!(writer, "outcome,\"")?;
                        serde_json::to_writer(CsvQuoted(&mut *writer), &item)?;
                        write!(writer, "\"\r\n")?;
                    } else {
                        if !first {
                            write!(writer, ",")?;
                        }
                        first = false;
                        write!(writer, "{{\"outcome\":")?;
                        serde_json::to_writer(&mut *writer, &item)?;
                    }
                    if include
                        && item.locator.input.is_some()
                        && (item.state == TransferStepState::Failed
                            || item.item_states.contains(&TransferStepState::Failed)
                            || item.error_record.is_some())
                    {
                        if csv {
                            write!(writer, "input_events,\"")?;
                        } else {
                            write!(writer, ",\"input_events\":[")?;
                        }
                        let spec = spec.as_ref().unwrap().clone();
                        let identities = run.context.inputs.clone();
                        let mut writer = output.take().unwrap();
                        let (returned, written) = tokio::task::spawn_blocking(move || {
                            let result = (|| -> Result<()> {
                                if csv {
                                    let mut quoted = CsvQuoted(&mut writer);
                                    write!(quoted, "[")?;
                                    write_failed_events(&mut quoted, &item, &spec, &identities)?;
                                    write!(quoted, "]")?;
                                } else {
                                    write_failed_events(&mut writer, &item, &spec, &identities)?;
                                }
                                Ok(())
                            })();
                            (writer, result)
                        })
                        .await
                        .map_err(|e| AppError::new("io", e.to_string()))?;
                        output = Some(returned);
                        written?;
                        let writer = output.as_mut().unwrap();
                        if csv {
                            write!(writer, "\"\r\n")?;
                        } else {
                            write!(writer, "]")?;
                        }
                    }
                    if !csv {
                        write!(output.as_mut().unwrap(), "}}")?;
                    }
                }
            }
            if !csv {
                write!(output.as_mut().unwrap(), "]}}")?;
            }
            Ok(())
        }
        .await;
        if let Err(error) = result {
            drop(output.take());
            self.transfer_report_partial(owner, artifact, &partial, &path, media, &baseline)
                .await?;
            return Err(error);
        }
        let output = output.take().unwrap();
        let published = tokio::task::spawn_blocking(move || output.publish())
            .await
            .map_err(|e| AppError::new("io", e.to_string()))
            .and_then(|result| result);
        let path = match published {
            Ok(path) => path,
            Err(mut error) => {
                use datapier_core::execution::data_files::{PublicationState, publication_state};
                match publication_state(&error) {
                    PublicationState::NotPublished => {
                        self.transfer_report_partial(
                            owner, artifact, &partial, &path, media, &baseline,
                        )
                        .await?
                    }
                    PublicationState::Published => {
                        let destination = path.clone();
                        let snapshot = tokio::task::spawn_blocking(move || {
                            datapier_core::execution::data_files::output_snapshot(
                                &destination,
                                true,
                            )
                        })
                        .await
                        .ok()
                        .and_then(|result| result.ok());
                        let identity=snapshot.filter(|snapshot| snapshot.file_id.is_some() && snapshot.file_id==baseline.file_id).map(|snapshot| {
                            json!({"path":snapshot.canonical_path,"canonical_path":snapshot.canonical_path,"file_id":snapshot.file_id,"size":snapshot.size,"sha256":snapshot.sha256,"export_name":path.file_name().and_then(|v|v.to_str()),"media_type":media})
                        });
                        self.store
                            .transfer_update_artifact(
                                artifact,
                                owner,
                                None,
                                "published",
                                identity.as_ref(),
                            )
                            .await?;
                    }
                    PublicationState::Unknown => {
                        error.code = "outcome_unknown".into();
                        self.store
                            .transfer_update_artifact(
                                artifact,
                                owner,
                                None,
                                "outcome_unknown",
                                None,
                            )
                            .await?;
                    }
                }
                return Err(error);
            }
        };
        let identity_path = path.clone();
        let snapshot = tokio::task::spawn_blocking(move || {
            datapier_core::execution::data_files::output_snapshot(&identity_path, true)
        })
        .await
        .map_err(|e| AppError::new("io", e.to_string()))??;
        if snapshot.file_id != baseline.file_id {
            return Err(AppError::new("recovery_conflict", "发布的报告身份已变化"));
        }
        let identity = json!({"path":snapshot.canonical_path,"canonical_path":snapshot.canonical_path,"file_id":snapshot.file_id,"size":snapshot.size,"sha256":snapshot.sha256,"export_name":path.file_name().and_then(|v|v.to_str()),"media_type":media});
        self.store
            .transfer_update_artifact(artifact, owner, Some(1), "published", Some(&identity))
            .await?;
        Ok(json!({"path":path,"artifact_id":artifact,"run_id":id}))
    }
    async fn transfer_report_partial(
        &self,
        owner: Uuid,
        artifact: Uuid,
        partial: &std::path::Path,
        destination: &std::path::Path,
        media: &str,
        baseline: &datapier_core::execution::data_files::OutputSnapshot,
    ) -> Result<()> {
        let snapshot_path = partial.to_owned();
        let snapshot = tokio::task::spawn_blocking(move || {
            datapier_core::execution::data_files::output_snapshot(&snapshot_path, true)
        })
        .await
        .map_err(|e| AppError::new("io", e.to_string()))??;
        if baseline.file_id.is_none()
            || snapshot.canonical_path != baseline.canonical_path
            || snapshot.file_id != baseline.file_id
        {
            return Err(AppError::new(
                "recovery_conflict",
                "报告临时文件身份已变化或无法验证",
            ));
        }
        let identity = json!({"path":snapshot.canonical_path,"canonical_path":snapshot.canonical_path,"file_id":snapshot.file_id,"size":snapshot.size,"sha256":snapshot.sha256,"export_name":destination.file_name().and_then(|v|v.to_str()),"media_type":media});
        self.store
            .transfer_update_artifact(artifact, owner, Some(1), "cleanup_pending", Some(&identity))
            .await?;
        Ok(())
    }
}

/// CSV 单元格内逐块转义，避免将整个报告或 Redis 集合缓存在内存中。
struct CsvQuoted<'a, W: std::io::Write>(&'a mut W);
impl<W: std::io::Write> std::io::Write for CsvQuoted<'_, W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut start = 0;
        for (index, byte) in buf.iter().enumerate() {
            if *byte == b'"' {
                self.0.write_all(&buf[start..index])?;
                self.0.write_all(b"\"\"")?;
                start = index + 1;
            }
        }
        self.0.write_all(&buf[start..])?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// 仅将选中失败记录写入用户显式选择的文件；输入内容不进入执行日志。
fn write_failed_events<W: std::io::Write>(
    output: &mut W,
    item: &datapier_core::model::transfer::TransferStepOutcome,
    spec: &TransferSpec,
    identities: &[datapier_core::model::transfer::TransferInputIdentity],
) -> Result<()> {
    use datapier_core::execution::data_files;
    use datapier_core::model::transfer::TransferFormat;
    let index = item
        .locator
        .input
        .ok_or_else(|| AppError::invalid("失败项缺少输入定位"))?;
    let input = spec
        .inputs
        .get(index)
        .ok_or_else(|| AppError::invalid("失败项输入索引无效"))?;
    let expected = identities
        .get(index)
        .ok_or_else(|| AppError::invalid("缺少冻结输入指纹"))?;
    let fingerprint = data_files::fingerprint(&input.path, 1024 * 1024, 16 * 1024 * 1024)?;
    if fingerprint.sha256 != expected.sha256 || fingerprint.bytes != expected.bytes {
        return Err(AppError::new("conflict", "报告输入与原任务指纹不同"));
    }
    let mut first = true;
    #[derive(serde::Serialize)]
    struct RawRecord<'a> {
        record: u64,
        encoding: &'static str,
        raw_bytes: &'a [u8],
    }
    if input.format == TransferFormat::Sql {
        let mut emit = |value: Value| -> Result<()> {
            if !first {
                write!(output, ",")?;
            }
            first = false;
            serde_json::to_writer(&mut *output, &value)?;
            Ok(())
        };
        let index = datapier_core::execution::sql_restore::inspect(&input.path)?;
        if index.sha256 != expected.sha256 || index.bytes != expected.bytes {
            return Err(AppError::new("conflict", "SQL 输入已变化"));
        }
        let offset = item
            .locator
            .byte_start
            .ok_or_else(|| AppError::invalid("SQL 失败项缺少字节定位"))?;
        let number = index
            .statements
            .iter()
            .position(|statement| statement.start <= offset && offset < statement.end)
            .ok_or_else(|| AppError::invalid("SQL 失败定位不属于已校验语句"))?;
        let statement = &index.statements[number];
        if statement.kind == datapier_core::execution::sql_restore::SqlKind::Insert {
            if item.locator.record_offsets.is_empty() {
                datapier_core::execution::sql_restore::read_insert_statement(
                    &input.path,
                    &index,
                    number,
                    |columns, values, pos| {
                        if pos == offset {
                            emit(json!({"byte_offset":pos,"columns":columns,"values":values}))?;
                        }
                        Ok(())
                    },
                )?;
            } else {
                let mut selected = item
                    .locator
                    .record_offsets
                    .iter()
                    .copied()
                    .enumerate()
                    .filter_map(|(i, offset)| {
                        let is_error = item.error_record == Some(offset);
                        let failed = if item.item_states.is_empty() {
                            item.error_record
                                .map_or(item.state == TransferStepState::Failed, |_| is_error)
                        } else {
                            item.item_states.get(i) == Some(&TransferStepState::Failed) || is_error
                        };
                        failed.then_some(offset)
                    })
                    .peekable();
                while let Some(&offset) = selected.peek() {
                    let number = index
                        .statements
                        .iter()
                        .position(|statement| statement.start <= offset && offset < statement.end)
                        .ok_or_else(|| AppError::invalid("SQL 失败定位不属于已校验语句"))?;
                    if index.statements[number].kind
                        != datapier_core::execution::sql_restore::SqlKind::Insert
                    {
                        return Err(AppError::invalid("SQL 失败记录定位不属于 INSERT 语句"));
                    }
                    datapier_core::execution::sql_restore::read_insert_statement(
                        &input.path,
                        &index,
                        number,
                        |columns, values, pos| {
                            if selected.peek() == Some(&pos) {
                                emit(json!({"byte_offset":pos,"columns":columns,"values":values}))?;
                                selected.next();
                            }
                            Ok(())
                        },
                    )?;
                    if selected.peek() == Some(&offset) {
                        return Err(AppError::invalid("未能在已校验输入中找到失败记录"));
                    }
                }
            }
        } else {
            let sql = datapier_core::execution::sql_restore::read_ddl(&input.path, &index, number)?;
            emit(json!({"byte_offset":statement.start,"sql":sql}))?;
        }
    } else {
        let first_record = item
            .locator
            .first_record
            .or(item.error_record)
            .ok_or_else(|| AppError::invalid("失败项缺少记录定位"))?;
        let last = item.locator.last_record.unwrap_or(first_record);
        let selected = |record: u64| -> bool {
            if record < first_record || record > last {
                return false;
            }
            if !item.item_states.is_empty() {
                return item.item_states.get((record - first_record) as usize)
                    == Some(&TransferStepState::Failed)
                    || item.error_record == Some(record);
            }
            item.error_record
                .map_or(item.state == TransferStepState::Failed, |number| {
                    number == record
                })
        };
        for record in first_record..=last {
            if !selected(record) {
                continue;
            }
            let bytes = data_files::read_raw_record(input, &fingerprint, spec.provider, record)?;
            if !first {
                write!(output, ",")?;
            }
            first = false;
            serde_json::to_writer(
                &mut *output,
                &RawRecord {
                    record,
                    encoding: "byte_array",
                    raw_bytes: &bytes,
                },
            )?;
        }
    }
    if first {
        return Err(AppError::invalid("未能在已校验输入中找到失败记录"));
    }
    Ok(())
}
