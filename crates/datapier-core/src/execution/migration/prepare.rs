//! 数据交换准备只读取元数据和有界样本；样本不代表已经逐条校验全部输入。

use super::*;
use crate::{
    execution::data_files::{self, RowEvent, RowReader},
    model::{Column, ResourceTarget},
    providers::mysql::MysqlSession,
};

fn diagnostic(error: AppError, object: Option<String>, input: Option<usize>) -> TransferDiagnostic {
    TransferDiagnostic {
        code: error.code,
        message: error.message,
        object,
        input,
        record: None,
        byte_offset: None,
        blocking: true,
    }
}
fn check_size(plan: &PreparedTransfer) -> Result<()> {
    if plan.items.len() > MAX_PLAN_ITEMS || encoded_size(plan)? > MAX_PLAN_BYTES {
        return Err(AppError::new(
            "resource_limit",
            "迁移计划超过对象或内存预算，请缩小本次对象范围",
        ));
    }
    Ok(())
}
fn target(
    mut target: ResourceTarget,
    endpoint: Option<&TransferEndpoint>,
) -> Result<ResourceTarget> {
    if target.database.is_none() {
        target.database = endpoint.and_then(|endpoint| endpoint.database.clone());
    }
    if target.name.is_empty() || target.database.as_deref().is_none_or(str::is_empty) {
        return Err(AppError::invalid("请明确库、源表和目标表"));
    }
    if target.kind.is_empty() {
        target.kind = "table".into();
    }
    Ok(target)
}
// ALTER 由结构设计器生成单条原子步骤；仅选中该步骤时使用其完整类型化结果。
fn validate_copy_mapping(
    source: &crate::providers::mysql::migration::MigrationTable,
    current: &crate::providers::mysql::migration::MigrationTable,
    object: &TransferObject,
    items: &[TransferPlanItem],
    desired: Option<&crate::model::schema::TableDefinition>,
) -> Result<()> {
    use crate::model::schema::SchemaDefinition;

    if !items
        .iter()
        .any(|item| item.selected && item.phase == "schema" && item.action == "alter_table")
    {
        return MysqlSession::validate_migration_mapping(source, current, object);
    }
    let desired =
        desired.ok_or_else(|| AppError::new("schema_incomplete", "选中结构的类型化定义不可用"))?;
    let Some(SchemaDefinition::Table(source_definition)) = source
        .snapshot
        .as_ref()
        .map(|snapshot| &snapshot.definition)
    else {
        return Err(AppError::new(
            "schema_incomplete",
            "源结构的类型化定义不可用",
        ));
    };
    let Some(SchemaDefinition::Table(current_definition)) = current
        .snapshot
        .as_ref()
        .map(|snapshot| &snapshot.definition)
    else {
        return Err(AppError::new(
            "schema_incomplete",
            "目标结构的类型化定义不可用",
        ));
    };
    let mut projected = current.clone();
    projected.schema.columns.clear();
    projected.collations.clear();
    for (position, definition) in desired.columns.iter().enumerate() {
        let source_name = object
            .columns
            .iter()
            .find(|mapping| mapping.target == definition.name)
            .map(|mapping| mapping.source.as_str())
            .unwrap_or(&definition.name);
        let original = source_definition
            .columns
            .iter()
            .find(|column| column.name == source_name);
        let mut column = Column::new(&definition.name, &definition.native_type);
        column.nullable = definition.nullable;
        column.generated = definition.generated.is_some();
        column.auto_increment = definition.auto_increment;
        projected.schema.columns.push(column);
        let previous = current_definition
            .columns
            .iter()
            .position(|column| column.id == definition.id);
        let unchanged = previous.is_some_and(|old_position| {
            current_definition.columns[old_position] == *definition
                && old_position == position
                && (position == 0
                    || current_definition.columns[old_position - 1].id
                        == desired.columns[position - 1].id)
        });
        if unchanged {
            if let Some(collation) = current.collations.get(&definition.name) {
                projected
                    .collations
                    .insert(definition.name.clone(), collation.clone());
            }
        } else if original.is_some_and(|column| {
            let old_type = column.native_type.split(['(', ' ']).next().unwrap_or("");
            let new_type = definition
                .native_type
                .split(['(', ' '])
                .next()
                .unwrap_or("");
            let numeric = |name: &str| {
                [
                    "tinyint",
                    "smallint",
                    "mediumint",
                    "int",
                    "integer",
                    "bigint",
                    "decimal",
                    "numeric",
                    "float",
                    "double",
                    "real",
                    "bit",
                ]
                .iter()
                .any(|kind| name.eq_ignore_ascii_case(kind))
            };
            (old_type.eq_ignore_ascii_case(new_type) || (numeric(old_type) && numeric(new_type)))
                && column.charset == definition.charset
                && column.collation == definition.collation
        }) && let Some(collation) = source.collations.get(source_name)
            && (collation.1.is_none()
                || definition.collation.as_ref().or(desired.collation.as_ref())
                    == collation.1.as_ref())
        {
            projected
                .collations
                .insert(definition.name.clone(), collation.clone());
        }
    }
    MysqlSession::validate_migration_mapping(source, &projected, object)
}

pub(super) async fn prepare(
    sink: &ExecutionSink,
    mut spec: TransferSpec,
    retry_of: Option<Uuid>,
    cancel: &CancellationToken,
    progress: Option<&tokio::sync::watch::Sender<PreparationProgress>>,
) -> Result<PreparedTransfer> {
    let store = &sink.store;
    let input_count = spec.inputs.len();
    if let Some(progress) = progress {
        progress.send_replace(PreparationProgress {
            input_count,
            ..Default::default()
        });
    }
    spec.validate()?;
    if spec.provider == ProviderKind::Mysql {
        for object in &mut spec.objects {
            object.source = target(object.source.clone(), spec.source.as_ref())?;
            object.target = target(
                object.target.clone(),
                spec.destination.as_ref().or(spec.source.as_ref()),
            )?;
        }
        for input in &mut spec.inputs {
            if let Some(t) = &mut input.target {
                *t = target(t.clone(), spec.destination.as_ref())?;
            }
        }
    }
    validate_databases(store, &spec).await?;
    let prior = if let Some(id) = retry_of.filter(|_| spec.mode == TransferMode::SqlRestore) {
        let run = store.transfer_run(id).await?;
        if matches!(run.state.as_str(), "running" | "cancelling") {
            return Err(AppError::new("busy", "原任务仍在执行"));
        }
        if run.spec_digest != digest(&spec)? {
            return Err(AppError::conflict("SQL 续作配置与原任务不同"));
        }
        Some(run)
    } else {
        None
    };
    let mut plan = PreparedTransfer {
        id: Uuid::new_v4(),
        revision: 1,
        state: "preparing".into(),
        digest: String::new(),
        retry_of,
        retry_states: Vec::new(),
        spec,
        items: Vec::new(),
        samples: Vec::new(),
        diagnostics: Vec::new(),
        inputs: Vec::new(),
        source_schema_fingerprints: BTreeMap::new(),
        source_identity: None,
        destination_identity: None,
        profile_digests: profile_digests(),
    };
    let mut source = None;
    let mut destination = None;
    let result = async {
        if let Some(endpoint) = &plan.spec.source {
            let (session, fingerprint) = connect(store, endpoint, plan.spec.provider, false, cancel).await?;
            plan.source_identity = Some(super::run::session_identity(&session).await?);
            plan.profile_digests.insert(session.profile().id, fingerprint);
            source = Some(session);
        }
        if let Some(endpoint) = &plan.spec.destination {
            let (session, fingerprint) = connect(store, endpoint, plan.spec.provider, true, cancel).await?;
            plan.destination_identity = Some(super::run::session_identity(&session).await?);
            plan.profile_digests.insert(session.profile().id, fingerprint);
            destination = Some(session);
        }
        for (index, input) in plan.spec.inputs.iter().enumerate() {
            crate::model::ensure_not_cancelled(cancel)?;
            let path = input.path.clone();
            let scan_cancel = cancel.clone();
            let scan_progress = progress.cloned();
            let fingerprint = tokio::task::spawn_blocking(move || {
                let mut report = |indexed_bytes, input_bytes| {
                    if let Some(progress) = &scan_progress {
                        progress.send_replace(PreparationProgress {
                            phase: PreparationPhase::Indexing,
                            input_index: Some(index + 1),
                            input_count,
                            indexed_bytes,
                            input_bytes: Some(input_bytes),
                        });
                    }
                };
                data_files::fingerprint_with_progress(&path, MAX_BATCH_BYTES, MAX_PLAN_BYTES / 2, &scan_cancel, &mut report)
            })
                .await.map_err(|error| AppError::new("io", error.to_string()))??;
            plan.inputs.push(fingerprint);
            check_size(&plan)?;
        }
        if let Some(prior) = &prior
            && (prior.context.inputs.len() != plan.inputs.len() || prior.context.inputs.iter().zip(&plan.inputs).any(|(old, new)| old.bytes != new.bytes || old.sha256 != new.sha256)) {
                return Err(AppError::conflict("SQL 续作输入与原任务指纹不同"));
            }
        if let Some(progress) = progress {
            progress.send_replace(PreparationProgress {
                phase: if plan.spec.mode == TransferMode::SqlRestore { PreparationPhase::SqlPreflight } else { PreparationPhase::Preflight },
                input_count,
                ..Default::default()
            });
        }
        match plan.spec.mode {
            TransferMode::SqlDump => {
                let Some(ProviderSession::Mysql(mysql)) = source.as_mut() else { return Err(AppError::invalid("缺少 MySQL 源连接")); };
                plan.items = mysql.prepare_dump(&plan.spec, cancel).await?;
            }
            TransferMode::SqlRestore => {
                let Some(ProviderSession::Mysql(mysql)) = destination.as_mut() else { return Err(AppError::invalid("缺少 MySQL 目标连接")); };
                plan.items = mysql.prepare_restore(&plan.spec, sink, retry_of, cancel).await?;
            }
            TransferMode::Copy | TransferMode::Export if plan.spec.provider == ProviderKind::Mysql => {
                let Some(ProviderSession::Mysql(mysql)) = source.as_mut() else { return Err(AppError::invalid("缺少 MySQL 源连接")); };
                if plan.spec.objects.is_empty() { return Err(AppError::invalid("请至少选择一张源表")); }
                if let Some(ProviderSession::Mysql(target)) = destination.as_mut() {
                    for object in &plan.spec.objects {
                        if mysql.same_table_as(target, &object.source, &object.target).await? {
                            return Err(AppError::invalid("源表和目标表实际相同，不能自拷贝"));
                        }
                    }
                }
                for index in 0..plan.spec.objects.len() {
                    let mut object = plan.spec.objects[index].clone();
                    crate::model::ensure_not_cancelled(cancel)?;
                    let schema = mysql.describe_migration(object.source.clone(), cancel).await?;
                    if object.columns.is_empty() {
                        object.columns = schema.schema.columns.iter().filter(|column| !column.generated).map(|column| ColumnMapping { source: column.name.clone(), target: column.name.clone() }).collect();
                        plan.spec.objects[index].columns = object.columns.clone();
                    }
                    if plan.spec.options.consistency == ReadConsistency::Transaction && !schema.schema.engine.eq_ignore_ascii_case("innodb") {
                        plan.diagnostics.push(diagnostic(AppError::new("inconsistent_engine", "非 InnoDB 表需要明确选择尽力读取"), Some(object.source.name.clone()), None));
                    }
                    if let Some(ProviderSession::Mysql(target)) = destination.as_mut() {
                        let current = match target.describe_migration(object.target.clone(), cancel).await {
                            Ok(table) => Some(table),
                            Err(error) if error.code == "not_found" => None,
                            Err(error) => return Err(error),
                        };
                        if current.is_none() && (!plan.spec.options.include_schema || (!object.create_if_missing && !plan.spec.options.create_missing)) {
                            plan.diagnostics.push(diagnostic(AppError::new("target_missing", "目标表不存在，请明确选择创建缺失表"), Some(object.target.name.clone()), None));
                        }
                        let source_snapshot = schema.snapshot.as_ref().ok_or_else(|| AppError::new("schema_incomplete", "源结构快照不可用"))?;
                        plan.source_schema_fingerprints.insert(index, source_snapshot.fingerprint.clone());
                        let target_snapshot = current.as_ref().and_then(|table| table.snapshot.as_ref());
                        let (version, mode, _, _) = target.schema_context(cancel).await?;
                        let (mut items, desired) = crate::providers::mysql::schema::plan_transfer_with_definition(source_snapshot, target_snapshot, &object, &plan.spec.objects, version, mode)?;
                        if plan.spec.options.include_schema && (current.is_none() || object.sync_schema) {
                            for item in items.iter().filter(|item| item.action == "unknown_attribute") {
                                plan.diagnostics.push(diagnostic(AppError::new("schema_incomplete", item.notes.join("; ")), Some(object.target.name.clone()), None));
                            }
                        }
                        for item in &mut items {
                            if current.is_none() && (object.create_if_missing || plan.spec.options.create_missing) { item.selected = true; }
                            if (!object.sync_schema && current.is_some()) || !plan.spec.options.include_schema { item.selected = false; }
                        }
                        if object.sync_schema && object.selected_schema_items.iter().any(|id| !items.iter().any(|item| &item.id == id)) {
                            return Err(AppError::conflict("结构选择已失效，请重新选择"));
                        }
                        if let Some(current) = &current && plan.spec.options.include_data
                            && let Err(error) = validate_copy_mapping(&schema, current, &object, &items, desired.as_ref()) {
                                plan.diagnostics.push(diagnostic(error, Some(object.target.name.clone()), None));
                            }
                        plan.items.extend(items);
                    }
                    let columns = source_columns(&schema.schema, &object)?;
                    let rows = mysql.sample_migration(&object, plan.spec.options.preview_rows, cancel).await?;
                    plan.samples.push(TransferSample { object: object.source.name.clone(), columns, rows, sampled: true, errors: Vec::new() });
                    check_size(&plan)?;
                }
            }
            TransferMode::Import if plan.spec.provider == ProviderKind::Mysql => {
                let Some(ProviderSession::Mysql(mysql)) = destination.as_mut() else { return Err(AppError::invalid("缺少 MySQL 目标连接")); };
                let mut creates = BTreeMap::new();
                for index in 0..plan.spec.inputs.len() {
                    if let Some(progress) = progress {
                        progress.send_replace(PreparationProgress {
                            phase: PreparationPhase::Preflight,
                            input_index: Some(index + 1),
                            input_count,
                            ..Default::default()
                        });
                    }
                    let original = &plan.spec.inputs[index];
                    let target = original.target.clone().ok_or_else(|| AppError::invalid("请为每个输入文件选择目标表"))?;
                    let current = match mysql.describe_migration(target.clone(), cancel).await {
                        Ok(table) => Some(table), Err(error) if error.code == "not_found" => None, Err(error) => return Err(error),
                    };
                    let mut input = original.clone();
                    // CSV 无自带类型；仅在未显式指定时按目标类型解码，保持用户意图不变。
                    if input.format == TransferFormat::Csv && input.columns.is_empty()
                        && let Some(table) = &current { input.columns = table.schema.columns.clone(); }
                    let fingerprint = plan.inputs[index].clone();
                    let max = plan.spec.options.preview_rows;
                    let read_cancel = cancel.clone();
                    let (columns, rows, confirmation) = tokio::task::spawn_blocking(move || -> Result<_> {
                        let mut reader = RowReader::open_with_cancel(&input, &fingerprint, ProviderKind::Mysql, &read_cancel)?;
                        let columns = reader.columns().to_vec();
                        let confirmation = reader.needs_confirmation();
                        let mut rows = Vec::new(); let mut bytes = 0;
                        while rows.len() < max {
                            match reader.next_event()? {
                                Some(RowEvent::MysqlRow { values, .. }) => {
                                    bytes += encoded_size(&values)?;
                                    if bytes > MAX_BATCH_BYTES { break; }
                                    rows.push(values);
                                }
                                None => break,
                                _ => return Err(AppError::invalid("MySQL 输入包含 Redis 内容")),
                            }
                        }
                        Ok((columns, rows, confirmation))
                    }).await.map_err(|error| AppError::new("io", error.to_string()))??;
                    if current.is_none() {
                        if !plan.spec.options.create_missing || !plan.spec.options.include_schema {
                            plan.diagnostics.push(diagnostic(AppError::new("target_missing", "目标表不存在，请明确选择创建缺失表"), Some(target.name.clone()), Some(index)));
                        } else if confirmation {
                            plan.diagnostics.push(diagnostic(AppError::new("type_confirmation_required", "CSV 样本类型尚需逐列确认"), Some(target.name.clone()), Some(index)));
                        } else if columns.iter().any(|column| column.default_value.is_some() && !plan.spec.inputs[index].column_defaults.contains_key(&column.name)) {
                            plan.diagnostics.push(diagnostic(AppError::new("default_confirmation_required", "输入默认值的字面量、表达式或 NULL 语义不明确，请逐列确认"), Some(target.name.clone()), Some(index)));
                        } else {
                            let (version, mode, _, _) = mysql.schema_context(cancel).await?;
                            let definition = create_table(&target, &columns, &plan.spec.inputs[index].column_mapping, &plan.spec.inputs[index].column_defaults, version, mode)?;
                            let key = (target.database.clone(), target.name.clone());
                            if let Some(previous) = creates.get(&key) {
                                if previous != &definition { return Err(AppError::invalid("同一目标表的输入文件创建结构不一致")); }
                                plan.samples.push(TransferSample { object: target.name, columns, rows, sampled: true, errors: Vec::new() });
                                check_size(&plan)?;
                                continue;
                            }
                            creates.insert(key, definition.clone());
                            plan.items.push(TransferPlanItem { id: format!("input:{index}:create"), phase: "schema".into(), action: "create_table".into(), source: None, target: Some(target.clone()), sql: Some(definition), destructive: false, selected: true, precondition: Some("absent".into()), notes: vec!["候选结构依据文件中可用的列信息生成；文件未提供的完整表属性无法还原，请在执行前核对".into()] });
                        }
                    } else if let Some(current) = current.as_ref() {
                        let input = &plan.spec.inputs[index];
                        let schema_columns: Vec<Column> = if input.format == TransferFormat::Csv && input.columns.is_empty() { Vec::new() } else {
                            columns.iter().filter(|column| {
                                let name = input.column_mapping.get(&column.name).unwrap_or(&column.name);
                                !column.generated && !current.schema.columns.iter().any(|target| &target.name == name && target.generated)
                            }).cloned().collect()
                        };
                        let prefix = format!("input:{index}:");
                        let selected: Vec<String> = input.selected_schema_items.iter().filter_map(|id| id.strip_prefix(&prefix).map(str::to_owned)).collect();
                        let mut items = mysql.prepare_input_schema(current, &schema_columns, &input.column_mapping, &selected).await?;
                        for item in &mut items {
                            item.id = format!("{prefix}{}", item.id);
                            if !input.sync_schema || !plan.spec.options.include_schema { item.selected = false; }
                        }
                        if input.sync_schema && input.selected_schema_items.iter().any(|id| !items.iter().any(|item| &item.id == id)) {
                            return Err(AppError::conflict("输入结构选择已失效，请重新选择"));
                        }
                        for column in &columns {
                            let name = input.column_mapping.get(&column.name).unwrap_or(&column.name);
                            let exists = current.schema.columns.iter().any(|target| &target.name == name);
                            let clause = format!(" ADD COLUMN `{}` ", name.replace('`', "``"));
                            let will_add = items.iter().any(|item| item.selected && item.action == "add_column" && item.sql.as_deref().is_some_and(|sql| sql.contains(&clause)));
                            if !exists && !will_add && !column.generated {
                                plan.diagnostics.push(diagnostic(AppError::invalid(format!("目标列不存在：{name}")), Some(target.name.clone()), Some(index)));
                            }
                        }
                        plan.items.extend(items);
                    }
                    plan.samples.push(TransferSample { object: target.name, columns, rows, sampled: true, errors: Vec::new() });
                    check_size(&plan)?;
                }
            }
            _ if plan.spec.provider == ProviderKind::Redis => {
                let source = match source.as_ref() { Some(ProviderSession::Redis(redis)) => Some(redis.as_ref()), _ => None };
                let destination = match destination.as_ref() { Some(ProviderSession::Redis(redis)) => Some(redis.as_ref()), _ => None };
                let (samples, diagnostics) = super::redis::prepare(&plan.spec, source, destination, cancel).await?;
                plan.samples.extend(samples);
                plan.diagnostics.extend(diagnostics);
            }
            _ => return Err(AppError::invalid("不支持的迁移模式和数据库组合")),
        }
        selected_items(&plan.items)?;
        check_size(&plan)?;
        plan.state = if plan.diagnostics.iter().any(|d| d.blocking) { "invalid" } else { "ready" }.into();
        plan.digest = plan_digest(&plan)?;
        Ok(())
    }.await;
    if let Some(session) = source.as_mut() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.close()).await;
    }
    if let Some(session) = destination.as_mut() {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), session.close()).await;
    }
    result?;
    Ok(plan)
}

pub fn plan_digest(plan: &PreparedTransfer) -> Result<String> {
    digest(&(
        &plan.spec,
        &plan.items,
        &plan.inputs,
        &plan.source_schema_fingerprints,
        &plan.source_identity,
        &plan.destination_identity,
        &plan.profile_digests,
        &plan.retry_of,
        &plan.retry_states,
    ))
}
fn create_table(
    target: &ResourceTarget,
    columns: &[Column],
    mapping: &BTreeMap<String, String>,
    defaults: &BTreeMap<String, crate::model::schema::ColumnDefault>,
    server_version: String,
    sql_mode: String,
) -> Result<String> {
    use crate::model::schema::{
        ColumnDefinition, IndexDefinition, IndexKind, IndexPart, SchemaDefinition,
        SchemaEditIntent, TableDefinition,
    };
    let mut names = std::collections::HashSet::new();
    let mut definitions = Vec::new();
    let mut primary = Vec::new();
    for column in columns {
        if column.generated {
            return Err(AppError::new(
                "schema_incomplete",
                format!("输入列 {} 缺少生成表达式，不能创建目标表", column.name),
            ));
        }
        let name = mapping.get(&column.name).unwrap_or(&column.name);
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(AppError::invalid("多个输入列映射到同一目标列"));
        }
        let default = match defaults.get(&column.name) {
            Some(default) => default.clone(),
            None if column.default_value.is_none() => {
                crate::model::schema::ColumnDefault::Unspecified
            }
            None => {
                return Err(AppError::new(
                    "default_confirmation_required",
                    format!("列 {} 缺少明确的默认值意图", column.name),
                ));
            }
        };
        if column.primary_key {
            primary.push(IndexPart {
                column: Some(name.clone()),
                expression: None,
                prefix_length: None,
                descending: false,
            });
        }
        definitions.push(ColumnDefinition {
            id: name.clone(),
            name: name.clone(),
            native_type: column.type_name.clone(),
            nullable: column.nullable,
            default,
            on_update: None,
            auto_increment: column.auto_increment,
            generated: None,
            charset: None,
            collation: None,
            visible: true,
            comment: column.comment.clone().unwrap_or_default(),
            extra: Vec::new(),
        });
    }
    if defaults
        .keys()
        .any(|name| !columns.iter().any(|column| &column.name == name))
    {
        return Err(AppError::invalid("默认值意图包含不存在的输入列"));
    }
    let indexes = if primary.is_empty() {
        Vec::new()
    } else {
        vec![IndexDefinition {
            id: "PRIMARY".into(),
            name: "PRIMARY".into(),
            kind: IndexKind::Primary,
            parts: primary,
            visible: true,
            method: None,
            options: Vec::new(),
        }]
    };
    let definition = TableDefinition {
        columns: definitions,
        indexes,
        constraints: Vec::new(),
        engine: "InnoDB".into(),
        charset: None,
        collation: None,
        comment: String::new(),
        options: Vec::new(),
    };
    let intent = SchemaEditIntent {
        target: target.clone(),
        baseline: None,
        definition: Some(SchemaDefinition::Table(definition)),
        profile_revision: String::new(),
        algorithm: None,
        lock: None,
        server_version,
        sql_mode,
    };
    let plan = crate::providers::mysql::schema::build_plan(&intent)?;
    plan.steps
        .into_iter()
        .next()
        .map(|step| step.sql)
        .ok_or_else(|| AppError::new("schema_incomplete", "未生成建表步骤"))
}

#[cfg(test)]
#[path = "../../../tests/unit/copy_schema_selection.rs"]
mod tests;
