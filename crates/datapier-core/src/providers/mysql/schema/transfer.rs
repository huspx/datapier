//! 将跨连接结构映射交给既有结构设计器生成冻结 DDL。
use crate::model::{
    AppError, Result,
    schema::*,
    transfer::{TransferObject, TransferPlanItem},
};
use sha2::{Digest, Sha256};

/// 校验完整恢复输入中的外键依赖；调用方必须传入所有输入合并后的表结构和映射。
pub(crate) fn validate_restore_dependencies(
    tables: &std::collections::BTreeMap<
        (String, String),
        crate::providers::mysql::migration::MigrationTable,
    >,
    objects: &[TransferObject],
) -> Result<()> {
    let invalid = |message: String| AppError::new("schema_incomplete", message);
    let mapping = |database: &str, table: &str| -> Result<&TransferObject> {
        let mut matches = objects.iter().filter(|object| {
            object.source.kind == "table"
                && object.source.database.as_deref() == Some(database)
                && object.source.name == table
        });
        let object = matches
            .next()
            .ok_or_else(|| invalid(format!("表 {database}.{table} 缺少映射")))?;
        if matches.next().is_some()
            || object.target.database.as_deref().is_none_or(str::is_empty)
            || object.target.name.is_empty()
        {
            return Err(invalid(format!(
                "表 {database}.{table} 的目标映射不唯一或不完整"
            )));
        }
        Ok(object)
    };
    let mut targets = std::collections::BTreeSet::new();
    for ((database, name), table) in tables {
        let object = mapping(database, name)?;
        if !targets.insert((
            object.target.database.as_deref(),
            object.target.name.as_str(),
        )) {
            return Err(invalid(format!(
                "多个源表映射到同一目标表 {}",
                object.target.name
            )));
        }
        let snapshot = table
            .snapshot
            .as_ref()
            .ok_or_else(|| invalid(format!("表 {database}.{name} 缺少结构快照")))?;
        if snapshot.completeness.state != CompletenessState::Complete {
            return Err(invalid(format!(
                "表 {database}.{name} 的结构不完整：{}",
                snapshot.completeness.reasons.join("; ")
            )));
        }
        let SchemaDefinition::Table(definition) = &snapshot.definition else {
            return Err(invalid(format!("{database}.{name} 不是表结构")));
        };
        let mapped_name = |column: &str| {
            object
                .columns
                .iter()
                .find(|item| item.source == column)
                .map_or(column.to_owned(), |item| item.target.clone())
        };
        let mut mapped_columns = std::collections::BTreeSet::new();
        for column in &definition.columns {
            let mapped = mapped_name(&column.name);
            if mapped.is_empty()
                || !mapped_columns.insert(mapped)
                || object
                    .columns
                    .iter()
                    .filter(|item| item.source == column.name)
                    .count()
                    > 1
            {
                return Err(invalid(format!(
                    "表 {database}.{name} 的列映射不唯一或不完整"
                )));
            }
        }
        for constraint in &definition.constraints {
            let ConstraintDefinition::ForeignKey {
                name: fk,
                columns,
                referenced_database,
                referenced_table,
                referenced_columns,
                ..
            } = constraint
            else {
                continue;
            };
            let referenced = tables
                .get(&(referenced_database.clone(), referenced_table.clone()))
                .ok_or_else(|| {
                    invalid(format!(
                        "外键 {fk} 缺少引用表 {referenced_database}.{referenced_table} 的完整输入"
                    ))
                })?;
            let reference_mapping = mapping(referenced_database, referenced_table)?;
            let reference_snapshot = referenced
                .snapshot
                .as_ref()
                .ok_or_else(|| invalid(format!("外键 {fk} 缺少引用表结构")))?;
            if reference_snapshot.completeness.state != CompletenessState::Complete {
                return Err(invalid(format!("外键 {fk} 的引用表结构不完整")));
            }
            let SchemaDefinition::Table(reference) = &reference_snapshot.definition else {
                return Err(invalid(format!("外键 {fk} 的引用对象不是表")));
            };
            if columns.is_empty() || columns.len() != referenced_columns.len() {
                return Err(invalid(format!("外键 {fk} 的列数量不匹配")));
            }
            if !definition.engine.eq_ignore_ascii_case("innodb")
                || !reference.engine.eq_ignore_ascii_case("innodb")
            {
                return Err(invalid(format!("外键 {fk} 的表引擎不支持安全恢复")));
            }
            for (local, remote) in columns.iter().zip(referenced_columns) {
                let local_column = definition
                    .columns
                    .iter()
                    .find(|column| column.name == *local)
                    .ok_or_else(|| invalid(format!("外键 {fk} 的列 {local} 不存在")))?;
                let remote_column = reference
                    .columns
                    .iter()
                    .find(|column| column.name == *remote)
                    .ok_or_else(|| invalid(format!("外键 {fk} 的引用列 {remote} 不存在")))?;
                let local_target = mapped_name(local);
                let remote_target = reference_mapping
                    .columns
                    .iter()
                    .find(|item| item.source == *remote)
                    .map_or(remote.as_str(), |item| item.target.as_str());
                let local_type = object
                    .target_columns
                    .iter()
                    .find(|column| column.name == local_target)
                    .map_or(local_column.native_type.as_str(), |column| {
                        column.type_name.as_str()
                    });
                let remote_type = reference_mapping
                    .target_columns
                    .iter()
                    .find(|column| column.name == remote_target)
                    .map_or(remote_column.native_type.as_str(), |column| {
                        column.type_name.as_str()
                    });
                if super::verify::normalized_type(local_type)
                    != super::verify::normalized_type(remote_type)
                {
                    return Err(invalid(format!(
                        "外键 {fk} 的列 {local} 和 {remote} 类型不兼容"
                    )));
                }
                if local_target.is_empty() || remote_target.is_empty() {
                    return Err(invalid(format!("外键 {fk} 的列映射不完整")));
                }
                let text = local_type.to_ascii_lowercase();
                if ["char", "text", "enum", "set"]
                    .iter()
                    .any(|kind| text.starts_with(kind))
                {
                    let local_charset = local_column
                        .charset
                        .as_ref()
                        .or(definition.charset.as_ref());
                    let remote_charset = remote_column
                        .charset
                        .as_ref()
                        .or(reference.charset.as_ref());
                    let local_collation = local_column
                        .collation
                        .as_ref()
                        .or(definition.collation.as_ref());
                    let remote_collation = remote_column
                        .collation
                        .as_ref()
                        .or(reference.collation.as_ref());
                    if local_charset.is_none()
                        || local_collation.is_none()
                        || local_charset != remote_charset
                        || local_collation != remote_collation
                    {
                        return Err(invalid(format!("外键 {fk} 的字符集或排序规则未知或不兼容")));
                    }
                }
            }
            if !reference.indexes.iter().any(|index| {
                index.visible
                    && matches!(
                        index.kind,
                        IndexKind::Primary | IndexKind::Unique | IndexKind::Normal
                    )
                    && index
                        .method
                        .as_deref()
                        .is_none_or(|method| method.eq_ignore_ascii_case("BTREE"))
                    && index.parts.len() >= referenced_columns.len()
                    && index
                        .parts
                        .iter()
                        .zip(referenced_columns)
                        .all(|(part, column)| {
                            part.column.as_ref() == Some(column)
                                && part.expression.is_none()
                                && part.prefix_length.is_none()
                                && !part.descending
                        })
            }) {
                return Err(invalid(format!("外键 {fk} 的引用列缺少可用索引")));
            }
        }
    }
    Ok(())
}

pub(crate) fn plan_transfer(
    source: &SchemaSnapshot,
    target: Option<&SchemaSnapshot>,
    object: &TransferObject,
    objects: &[TransferObject],
    version: String,
    mode: String,
) -> Result<Vec<TransferPlanItem>> {
    Ok(plan_transfer_with_definition(source, target, object, objects, version, mode)?.0)
}

pub(crate) fn plan_transfer_with_definition(
    source: &SchemaSnapshot,
    target: Option<&SchemaSnapshot>,
    object: &TransferObject,
    objects: &[TransferObject],
    version: String,
    mode: String,
) -> Result<(Vec<TransferPlanItem>, Option<TableDefinition>)> {
    let diagnostic = |reason: String| TransferPlanItem {
        id: format!(
            "schema:unknown:{:x}",
            base16ct::HexDisplay(&Sha256::digest(reason.as_bytes()))
        ),
        phase: "schema".into(),
        action: "unknown_attribute".into(),
        source: Some(object.source.clone()),
        target: Some(object.target.clone()),
        sql: None,
        destructive: false,
        selected: false,
        precondition: target.map(|s| format!("snapshot:{}", s.fingerprint)),
        notes: vec![reason],
    };
    if source.completeness.state != CompletenessState::Complete {
        return Ok((
            vec![diagnostic(format!(
                "源结构不完整：{}",
                source.completeness.reasons.join("; ")
            ))],
            None,
        ));
    }
    if let Some(target) = target
        && target.completeness.state != CompletenessState::Complete
    {
        return Ok((
            vec![diagnostic(format!(
                "目标结构不完整：{}",
                target.completeness.reasons.join("; ")
            ))],
            None,
        ));
    }
    let SchemaDefinition::Table(mut desired) = source.definition.clone() else {
        return Err(AppError::invalid("源对象不是表"));
    };
    let mapped = |name: &str| {
        object
            .columns
            .iter()
            .find(|mapping| mapping.source == name)
            .map(|mapping| mapping.target.clone())
            .unwrap_or_else(|| name.to_owned())
    };
    let renamed = object
        .columns
        .iter()
        .any(|mapping| mapping.source != mapping.target);
    if renamed
        && (desired.columns.iter().any(|column| {
            column.generated.is_some()
                || matches!(&column.default, ColumnDefault::Expression(_))
                || column.on_update.is_some()
        }) || desired
            .indexes
            .iter()
            .any(|index| index.parts.iter().any(|part| part.expression.is_some()))
            || desired
                .constraints
                .iter()
                .any(|constraint| matches!(constraint, ConstraintDefinition::Check { .. })))
    {
        return Ok((
            vec![diagnostic(
                "列重命名涉及表达式，无法安全证明表达式引用的映射".into(),
            )],
            None,
        ));
    }
    for column in &mut desired.columns {
        let original = column.name.clone();
        column.name = mapped(&original);
        column.id = if let Some(SchemaSnapshot {
            definition: SchemaDefinition::Table(existing),
            ..
        }) = target
        {
            if existing.columns.iter().any(|old| old.name == column.name) {
                column.name.clone()
            } else if existing.columns.iter().any(|old| old.name == original) {
                original
            } else {
                column.name.clone()
            }
        } else {
            column.name.clone()
        };
        if let Some(candidate) = object
            .target_columns
            .iter()
            .find(|candidate| candidate.name == column.name)
        {
            let known_default = match &column.default {
                ColumnDefault::Unspecified => None,
                ColumnDefault::Null => Some("NULL"),
                ColumnDefault::Literal(value) | ColumnDefault::Expression(value) => {
                    Some(value.as_str())
                }
            };
            if candidate
                .default_value
                .as_deref()
                .is_some_and(|value| Some(value) != known_default)
                || candidate.generated != column.generated.is_some()
                || candidate.auto_increment != column.auto_increment
            {
                return Ok((
                    vec![diagnostic(format!(
                        "列 {} 的简化候选属性无法安全转换为完整结构",
                        column.name
                    ))],
                    None,
                ));
            }
            column.native_type = candidate.type_name.clone();
            column.nullable = candidate.nullable;
        }
    }
    for index in &mut desired.indexes {
        for part in &mut index.parts {
            if let Some(name) = &mut part.column {
                *name = mapped(name);
            }
        }
    }
    for constraint in &mut desired.constraints {
        if let ConstraintDefinition::ForeignKey {
            id,
            name,
            columns,
            referenced_database,
            referenced_table,
            referenced_columns,
            ..
        } = constraint
        {
            for name in columns {
                *name = mapped(name);
            }
            let reference = {
                let mut candidates = objects.iter().filter(|candidate| {
                    candidate.source.database.as_deref() == Some(referenced_database.as_str())
                        && candidate.source.name == *referenced_table
                });
                let Some(reference) = candidates.next() else {
                    return Ok((
                        vec![diagnostic(format!(
                            "外键依赖 {}.{} 缺少目标映射",
                            referenced_database, referenced_table
                        ))],
                        None,
                    ));
                };
                if candidates.next().is_some() {
                    return Ok((
                        vec![diagnostic(format!(
                            "外键依赖 {}.{} 存在多个目标映射",
                            referenced_database, referenced_table
                        ))],
                        None,
                    ));
                }
                reference
            };
            for name in referenced_columns {
                if let Some(mapping) = reference
                    .columns
                    .iter()
                    .find(|mapping| mapping.source == *name)
                {
                    *name = mapping.target.clone();
                }
            }
            *referenced_database = reference
                .target
                .database
                .clone()
                .ok_or_else(|| AppError::invalid("外键目标库未指定"))?;
            *referenced_table = reference.target.name.clone();
            let existing_name = target.is_some_and(|snapshot| match &snapshot.definition {
                SchemaDefinition::Table(table) => table.constraints.iter().any(|constraint| matches!(constraint, ConstraintDefinition::ForeignKey { name: old, .. } if old.as_str() == name.as_str())),
                _ => false,
            });
            if !existing_name {
                let digest = Sha256::digest(serde_json::to_vec(&(&object.target, &*name))?);
                *name = format!("dp_fk_{:x}", base16ct::HexDisplay(&digest))[..54].to_owned();
            }
            *id = name.clone();
        }
    }
    let mut deferred = Vec::new();
    if target.is_none() || source.server_version.is_empty() {
        desired.constraints.retain(|constraint| {
            let ConstraintDefinition::ForeignKey { name, .. } = constraint else {
                return true;
            };
            let exists = target.is_some_and(|snapshot| match &snapshot.definition {
                SchemaDefinition::Table(table) => table.constraints.iter().any(|old| matches!(old, ConstraintDefinition::ForeignKey { name: old_name, .. } if old_name == name)),
                _ => false,
            });
            if exists {
                true
            } else {
                deferred.push(constraint.clone());
                false
            }
        });
    }
    let intent = SchemaEditIntent {
        target: object.target.clone(),
        baseline: target.cloned(),
        definition: Some(SchemaDefinition::Table(desired.clone())),
        profile_revision: String::new(),
        algorithm: None,
        lock: None,
        server_version: version.clone(),
        sql_mode: mode.clone(),
    };
    let plan = match super::build_plan(&intent) {
        Ok(plan) => plan,
        Err(error) => {
            return Ok((
                vec![diagnostic(format!(
                    "结构计划无法安全生成：{}",
                    error.message
                ))],
                None,
            ));
        }
    };
    let mut steps: Vec<(String, SchemaStep)> = plan
        .steps
        .into_iter()
        .map(|step| ("schema".into(), step))
        .collect();
    for constraint in deferred {
        let baseline = SchemaSnapshot {
            target: object.target.clone(),
            server_version: version.clone(),
            sql_mode: mode.clone(),
            definition: SchemaDefinition::Table(desired.clone()),
            ddl: "pending".into(),
            dependencies: Vec::new(),
            completeness: Completeness {
                state: CompletenessState::Complete,
                reasons: Vec::new(),
            },
            fingerprint: target
                .map_or_else(|| "pending".into(), |snapshot| snapshot.fingerprint.clone()),
            capabilities: Default::default(),
        };
        desired.constraints.push(constraint);
        let intent = SchemaEditIntent {
            target: object.target.clone(),
            baseline: Some(baseline),
            definition: Some(SchemaDefinition::Table(desired.clone())),
            profile_revision: String::new(),
            algorithm: None,
            lock: None,
            server_version: version.clone(),
            sql_mode: mode.clone(),
        };
        let plan = match super::build_plan(&intent) {
            Ok(plan) => plan,
            Err(error) => {
                return Ok((
                    vec![diagnostic(format!(
                        "外键计划无法安全生成：{}",
                        error.message
                    ))],
                    None,
                ));
            }
        };
        steps.extend(
            plan.steps
                .into_iter()
                .map(|step| ("after_data".into(), step)),
        );
    }
    let mut items = Vec::new();
    for (index, (phase, step)) in steps.into_iter().enumerate() {
        let id = format!(
            "schema:{:x}",
            base16ct::HexDisplay(&Sha256::digest(serde_json::to_vec(&(
                &object.target,
                index,
                &step.sql
            ))?))
        );
        let precondition = match step.precondition {
            SchemaPrecondition::Absent => "absent".into(),
            SchemaPrecondition::Fingerprint(value) => format!("snapshot:{value}"),
        };
        let mut notes = step.destructive_reasons.clone();
        notes.extend(step.warnings);
        if source.server_version.is_empty() {
            notes.push("离线 DDL 未提供源服务端版本和目录信息".into());
        }
        let action = if phase == "after_data" {
            "add_foreign_key"
        } else if target.is_some() {
            "alter_table"
        } else {
            "create_table"
        };
        items.push(TransferPlanItem {
            selected: object.selected_schema_items.contains(&id),
            id,
            phase,
            action: action.into(),
            source: Some(object.source.clone()),
            target: Some(object.target.clone()),
            sql: Some(step.sql),
            destructive: target.is_some() || !step.destructive_reasons.is_empty(),
            precondition: Some(precondition),
            notes,
        });
    }
    Ok((items, Some(desired)))
}
