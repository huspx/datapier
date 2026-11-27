//! 数据交换续作范围选择与准备。
use super::*;

fn append_record_range(ranges: &mut Vec<RecordRange>, range: RecordRange) {
    if let Some(previous) = ranges.last_mut()
        && previous.last.checked_add(1) == Some(range.first)
    {
        previous.last = range.last;
    } else {
        ranges.push(range);
    }
}

pub(in crate::server) fn selected_file_records(
    item: &datapier_core::model::transfer::TransferStepOutcome,
    states: &[TransferStepState],
    ranges: &mut Vec<RecordRange>,
) -> Result<bool> {
    let Some(first) = item.locator.first_record else {
        return Ok(false);
    };
    if item.state == TransferStepState::Sent {
        return Ok(false);
    }
    let mut any = false;
    if item.item_states.is_empty() {
        if states.contains(&item.state) {
            append_record_range(
                ranges,
                RecordRange {
                    first,
                    last: item.locator.last_record.unwrap_or(first),
                },
            );
            any = true;
        }
    } else {
        for (offset, state) in item.item_states.iter().enumerate() {
            if states.contains(state) {
                let record = first
                    .checked_add(offset as u64)
                    .ok_or_else(|| AppError::invalid("记录号溢出"))?;
                if item.locator.last_record.is_some_and(|last| record > last) {
                    return Err(AppError::new("storage", "批次状态超出记录范围"));
                }
                append_record_range(
                    ranges,
                    RecordRange {
                        first: record,
                        last: record,
                    },
                );
                any = true;
            }
        }
    }
    Ok(any)
}

impl Engine {
    pub(super) async fn transfer_retry_prepare(
        self: &Arc<Self>,
        owner: Uuid,
        params: &Value,
    ) -> Result<Value> {
        let id: Uuid = model::param(params, "run_id")?;
        let (run, _) = self.transfer_owned_run(owner, id).await?;
        if matches!(run.state.as_str(), "running" | "cancelling") {
            return Err(AppError::new("busy", "原任务仍在执行"));
        }
        let states: Vec<TransferStepState> = optional(params, "states")?.unwrap_or_else(|| {
            vec![
                TransferStepState::Failed,
                TransferStepState::RolledBack,
                TransferStepState::NotSent,
            ]
        });
        if states.is_empty()
            || states.iter().any(|state| {
                !matches!(
                    state,
                    TransferStepState::Failed
                        | TransferStepState::RolledBack
                        | TransferStepState::NotSent
                )
            })
        {
            return Err(AppError::invalid("仅允许选择失败、已回滚或未发送范围"));
        }
        let _binding_guard = self.transfer_binding_guard(params).await?;
        let mut spec: TransferSpec = model::param(params, "spec")?;
        self.resolve_transfer_spec(owner, params, &mut spec).await?;
        let binding = self.transfer_binding_snapshot(params).await?;
        if spec.mode == datapier_core::model::transfer::TransferMode::SqlRestore {
            if migration::digest(&spec)? != run.spec_digest {
                return Err(AppError::new("conflict", "SQL 续作配置必须与原任务一致"));
            }
            if spec.inputs.len() != run.context.inputs.len() {
                return Err(AppError::invalid("SQL 输入数量与原任务不同"));
            }
            for (input, expected) in spec.inputs.iter().zip(&run.context.inputs) {
                let path = input.path.clone();
                let fingerprint = tokio::task::spawn_blocking(move || {
                    datapier_core::execution::data_files::fingerprint(
                        &path,
                        1024 * 1024,
                        16 * 1024 * 1024,
                    )
                })
                .await
                .map_err(|e| AppError::new("io", e.to_string()))??;
                if fingerprint.sha256 != expected.sha256 || fingerprint.bytes != expected.bytes {
                    return Err(AppError::new("conflict", "SQL 续作输入与原任务指纹不同"));
                }
            }
            return self
                .transfer_start_prepare(owner, run.workspace_id, spec, Some((id, states)), binding)
                .await;
        }
        let file_import = spec.provider == datapier_core::model::ProviderKind::Mysql
            && spec.mode == datapier_core::model::transfer::TransferMode::Import;
        if file_import {
            let normalize = |target: &mut datapier_core::model::ResourceTarget,
                             endpoint: Option<
                &datapier_core::model::transfer::TransferEndpoint,
            >| {
                if target.database.is_none() {
                    target.database = endpoint.and_then(|endpoint| endpoint.database.clone());
                }
                if target.kind.is_empty() {
                    target.kind = "table".into();
                }
            };
            for object in &mut spec.objects {
                normalize(&mut object.source, spec.source.as_ref());
                normalize(
                    &mut object.target,
                    spec.destination.as_ref().or(spec.source.as_ref()),
                );
            }
            for input in &mut spec.inputs {
                if let Some(target) = &mut input.target {
                    normalize(target, spec.destination.as_ref());
                }
            }
            if migration::digest(&spec)? != run.spec_digest {
                return Err(AppError::new("conflict", "文件续作配置必须与原任务一致"));
            }
        }
        let mut frontier: Option<(usize, u64)> = None;
        let mut after_data = false;
        let mut ranges: Vec<Vec<RecordRange>> = vec![Vec::new(); spec.inputs.len()];
        let mut range_count = 0usize;
        let mut after = None;
        let mut any = false;
        let mut unknown = false;
        loop {
            let items = self.store.transfer_items(id, after, 500).await?;
            if items.is_empty() {
                break;
            }
            after = items.last().map(|item| item.ordinal);
            for item in items {
                if item.state == TransferStepState::OutcomeUnknown
                    || item.state == TransferStepState::Sent
                    || item.item_states.iter().any(|state| {
                        *state == TransferStepState::OutcomeUnknown
                            || *state == TransferStepState::Sent
                    })
                {
                    unknown = true;
                }
                if let (Some(index), Some(first)) = (item.locator.input, item.locator.first_record)
                {
                    if file_import && matches!(item.phase.as_str(), "data" | "conversion") {
                        let last = item.locator.last_record.unwrap_or(first);
                        frontier =
                            Some(frontier.map_or((index, last), |old| old.max((index, last))));
                    }
                    if index >= ranges.len() {
                        return Err(AppError::invalid("续作配置的输入文件与原任务不匹配"));
                    }
                    let before = ranges[index].len();
                    any |= selected_file_records(&item, &states, &mut ranges[index])?;
                    range_count += ranges[index].len() - before;
                    if range_count > MAX_PLAN_BYTES / std::mem::size_of::<RecordRange>() {
                        return Err(AppError::new("resource_limit", "续作范围超过内存预算"));
                    }
                } else if states.contains(&item.state) {
                    any = true;
                }
                if file_import && frontier.is_some() && item.phase == "schema" {
                    after_data = true;
                }
            }
        }
        if file_import
            && spec.options.include_data
            && run.state != "succeeded"
            && states.contains(&TransferStepState::NotSent)
            && !after_data
        {
            for (index, input) in spec.inputs.iter().enumerate() {
                let start = match frontier {
                    Some((last_input, _)) if index < last_input => continue,
                    Some((last_input, last)) if index == last_input => match last.checked_add(1) {
                        Some(start) => start,
                        None => continue,
                    },
                    _ => 1,
                };
                let before = ranges[index].len();
                if input.record_ranges.is_empty() {
                    append_record_range(
                        &mut ranges[index],
                        RecordRange {
                            first: start,
                            last: u64::MAX,
                        },
                    );
                    any = true;
                } else {
                    for range in &input.record_ranges {
                        let first = range.first.max(start);
                        if first <= range.last {
                            append_record_range(
                                &mut ranges[index],
                                RecordRange {
                                    first,
                                    last: range.last,
                                },
                            );
                            any = true;
                        }
                    }
                }
                range_count += ranges[index].len() - before;
                if range_count > MAX_PLAN_BYTES / std::mem::size_of::<RecordRange>() {
                    return Err(AppError::new("resource_limit", "续作范围超过内存预算"));
                }
            }
        }
        if !any {
            return Err(AppError::invalid("原任务没有选定的可续作范围"));
        }
        if !spec.inputs.is_empty() {
            if spec.inputs.len() != run.context.inputs.len() {
                return Err(AppError::invalid("续作输入数量与原任务不同"));
            }
            let mut retained = Vec::new();
            for (index, mut input) in spec.inputs.into_iter().enumerate() {
                let path = input.path.clone();
                let fingerprint = tokio::task::spawn_blocking(move || {
                    datapier_core::execution::data_files::fingerprint(
                        &path,
                        1024 * 1024,
                        16 * 1024 * 1024,
                    )
                })
                .await
                .map_err(|e| AppError::new("io", e.to_string()))??;
                let expected = &run.context.inputs[index];
                if fingerprint.sha256 != expected.sha256 || fingerprint.bytes != expected.bytes {
                    return Err(AppError::new("conflict", "续作输入与原任务指纹不同"));
                }
                if ranges[index].is_empty() {
                    continue;
                }
                input.record_ranges = std::mem::take(&mut ranges[index]);
                retained.push(input);
            }
            if retained.is_empty() {
                return Err(AppError::invalid("没有可续作的文件记录"));
            }
            spec.inputs = retained;
        } else if unknown {
            // 无可还原 key／行定位时，无法证明新快照不会包含待确认写入。
            return Err(AppError::new(
                "outcome_unknown",
                "原任务含待确认写入，先人工核对并重新配置迁移",
            ));
        }
        self.transfer_start_prepare(owner, run.workspace_id, spec, Some((id, states)), binding)
            .await
    }
}
