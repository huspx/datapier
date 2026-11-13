//! 单值会话复用批量编辑的可靠键和完整行基线读取。
use super::{MysqlSession, changes, replace};
use crate::model::{AppError, ResourceTarget, Result, Value, param};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

impl MysqlSession {
    /// 根据完整可靠键读取真实行及结构；调用者必须与原结果核对，不能将重读冒充旧结果。
    pub async fn value_snapshot(
        &mut self,
        params: &Json,
        cancel: &CancellationToken,
    ) -> Result<Json> {
        self.profile.ensure_write()?;
        let target: ResourceTarget = param(params, "target")?;
        let token: Uuid = param(params, "token")?;
        let edit = self
            .edits
            .get(&token)
            .cloned()
            .ok_or_else(|| AppError::conflict("编辑令牌已失效"))?;
        if serde_json::to_value(&target)? != serde_json::to_value(&edit.target)? {
            return Err(AppError::invalid("编辑目标与令牌不匹配"));
        }
        let row: BTreeMap<String, Value> = param(params, "row")?;
        let column: String = param(params, "column")?;
        let max_bytes: usize = param(params, "max_bytes")?;
        let schema = changes::edit_schema(self, &edit, cancel).await?;
        let names = schema
            .keys
            .iter()
            .find(|key| {
                key.iter().all(|name| {
                    row.get(name).is_some_and(|v| {
                        !matches!(v, Value::Null | Value::Missing | Value::Large { .. })
                    })
                })
            })
            .ok_or_else(|| AppError::invalid("结果不包含完整可靠键"))?;
        let key = names
            .iter()
            .map(|name| (name.clone(), row[name].clone()))
            .collect();
        let (schema, before, view_fingerprint) =
            replace::snapshot(self, target, key, max_bytes, cancel).await?;
        if edit.view_fingerprint != view_fingerprint {
            return Err(AppError::conflict("视图映射已变化"));
        }
        if row.len() != before.len() || before.keys().any(|name| !row.contains_key(name)) {
            return Err(AppError::conflict("结果缺少完整可见行，不能作为编辑基线"));
        }
        for (name, value) in &row {
            if before.get(name) != Some(value) {
                return Err(AppError::conflict("结果行与当前数据库不一致"));
            }
        }
        let definition = schema
            .columns
            .iter()
            .find(|item| item.name == column)
            .ok_or_else(|| AppError::invalid("目标列不存在"))?;
        if definition.generated {
            return Err(AppError::invalid("生成列不可编辑"));
        }
        let value = before
            .get(&column)
            .ok_or_else(|| AppError::conflict("完整行缺少目标列"))?;
        let (_, sql_mode, _, _) = self.schema_context(cancel).await?;
        let ddl_fingerprint = super::schema::fingerprint(&schema.ddl, &sql_mode);
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(&(
                &schema.target,
                &schema.columns,
                &schema.keys,
                &schema.engine,
                &schema.details,
                &view_fingerprint,
                &sql_mode,
                ddl_fingerprint
            ))?)
        );
        let key: BTreeMap<_, _> = names
            .iter()
            .map(|name| (name.clone(), before[name].clone()))
            .collect();
        Ok(
            json!({"value":value,"before":before,"key":key,"schema":schema,"fingerprint":fingerprint}),
        )
    }

    /// 使用普通行变更的列类型规则校验值，不发送数据库写入。
    pub fn value_validate(&self, snapshot: &Json, column: &str, value: &Value) -> Result<()> {
        let schema: crate::model::TableSchema = param(snapshot, "schema")?;
        let column = schema
            .columns
            .iter()
            .find(|item| item.name == column)
            .ok_or_else(|| AppError::invalid("目标列不存在"))?;
        changes::validate_import_value(value, column)
    }

    /// 重新证明结构和完整可见行与原基线相同，不会替换旧基线。
    pub async fn value_verify(&mut self, params: &Json, cancel: &CancellationToken) -> Result<()> {
        let expected_before: BTreeMap<String, Value> = param(params, "before")?;
        let expected_fingerprint: String = param(params, "fingerprint")?;
        let actual = self.value_snapshot(params, cancel).await?;
        let before: BTreeMap<String, Value> = param(&actual, "before")?;
        let fingerprint: String = param(&actual, "fingerprint")?;
        if before != expected_before || fingerprint != expected_fingerprint {
            return Err(AppError::conflict("行或结构在编辑期间已变化"));
        }
        Ok(())
    }
}
