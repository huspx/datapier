use super::*;

impl App {
    /// 在创建草稿前按原始可靠键检查整批目标，避免删除与更新同一身份。
    pub(crate) fn check_rows_not_deleted(
        &self,
        rows: impl IntoIterator<Item = u64>,
    ) -> Result<(), UiMessage> {
        if !self
            .pending_rows
            .iter()
            .any(|change| change.operation == "delete")
        {
            return Ok(());
        }
        let invalid = || UiMessage::key("当前结果没有可用的编辑基线或可靠键");
        let grid = self.grid.as_ref().ok_or_else(invalid)?;
        let keys: Vec<String> = self
            .schema
            .as_ref()
            .and_then(|schema| schema.get("keys"))
            .and_then(Value::as_array)
            .and_then(|keys| keys.first())
            .and_then(|keys| serde_json::from_value(keys.clone()).ok())
            .filter(|keys: &Vec<String>| !keys.is_empty())
            .ok_or_else(invalid)?;
        for row in rows {
            if !grid.row_in_page(row) {
                return Err(invalid());
            }
            let original = grid.original_row(row).ok_or_else(invalid)?;
            if original.len() != grid.page.result.columns.len() {
                return Err(invalid());
            }
            let before: std::collections::BTreeMap<_, _> = grid
                .page
                .result
                .columns
                .iter()
                .zip(original)
                .map(|(column, value)| (column.name.clone(), value.clone()))
                .collect();
            if before.len() != original.len() {
                return Err(invalid());
            }
            let mut key = std::collections::BTreeMap::new();
            for name in &keys {
                let value = before.get(name).ok_or_else(invalid)?;
                if matches!(
                    value,
                    DataValue::Null | DataValue::Missing | DataValue::Large { .. }
                ) || key.insert(name.clone(), value.clone()).is_some()
                {
                    return Err(invalid());
                }
            }
            if self
                .pending_rows
                .iter()
                .any(|change| change.operation == "delete" && change.key == key)
            {
                return Err(UiMessage::key("目标行已有待提交变更"));
            }
        }
        Ok(())
    }
    pub(super) fn workbench_can_edit(&self) -> bool {
        self.profile.as_ref().is_some_and(|p| {
            p.access_mode == AccessMode::ReadWrite && p.capabilities().edit.is_supported()
        }) && self.browse_result
            && self.edit_token.is_some()
            && self.target.as_ref().is_some_and(|t| t.kind == "table")
            && self.schema.as_ref().is_some_and(|schema| {
                schema
                    .get("engine")
                    .and_then(Value::as_str)
                    .is_some_and(|engine| !engine.is_empty())
                    && schema
                        .get("keys")
                        .and_then(Value::as_array)
                        .is_some_and(|keys| {
                            keys.first()
                                .and_then(Value::as_array)
                                .is_some_and(|columns| !columns.is_empty())
                        })
            })
            && self.grid.as_ref().is_some_and(|g| g.editable)
    }
    /// 确认弹窗展示的冻结行数。
    pub(crate) fn delete_rows_count(&self) -> usize {
        self.workbench
            .delete_rows
            .as_ref()
            .map_or(0, |pending| pending.rows.len())
    }
    /// 返回确认时冻结的库名和表名，避免弹窗展示后来切换的目标。
    pub(crate) fn delete_rows_target(&self) -> Option<(&str, &str)> {
        let pending = self.workbench.delete_rows.as_ref()?;
        let database = pending
            .target
            .get("database")
            .and_then(Value::as_str)
            .or(pending.database.as_deref())?;
        Some((database, pending.target.get("name")?.as_str()?))
    }

    pub(crate) fn request_delete_rows(&mut self) {
        self.workbench.delete_rows = None;
        if self.workbench.inline.is_some()
            || self.paste_preview.is_some()
            || self.page_token.is_some()
        {
            self.status = UiMessage::key("请先完成或取消单元格编辑及粘贴预览");
            return;
        }
        if !self.tabs[self.active].pending.is_null() && !self.tabs[self.active].pending_live {
            self.status = UiMessage::key("请先恢复或丢弃已保存的编辑意图");
            return;
        }
        if !self.workbench_can_edit() || !self.can_copy_insert() {
            self.status = UiMessage::key("当前结果没有可用的编辑基线或可靠键");
            return;
        }
        let Some(grid) = self.grid.as_ref() else {
            return;
        };
        let rows = grid.selected_rows();
        let (
            Some(profile),
            Some(session),
            Some(target),
            Some(result_id),
            Some(schema),
            Some(edit_token),
        ) = (
            self.profile.as_ref(),
            self.session_id(),
            self.target.as_ref(),
            self.result_id.as_ref(),
            self.schema.as_ref(),
            self.edit_token.as_ref(),
        )
        else {
            self.status = UiMessage::key("当前结果没有可用的编辑基线或可靠键");
            return;
        };
        if rows.is_empty() || result_id != &grid.page.result.id.to_string() {
            self.status = UiMessage::key("目标行已失效");
            return;
        }
        let mut originals = Vec::with_capacity(rows.len());
        for row in rows {
            let Some(original) = grid.original_row(row) else {
                self.status = UiMessage::key("目标行已失效");
                return;
            };
            originals.push((row, original.to_vec()));
        }
        self.workbench.delete_rows = Some(DeleteRowsConfirmation {
            tab: self.tabs[self.active].id,
            generation: self.generation,
            profile: profile.id,
            session,
            target: json!(target),
            database: self.database.clone(),
            result_id: result_id.clone(),
            grid_result_id: grid.page.result.id,
            schema: schema.clone(),
            edit_token: edit_token.clone(),
            columns: json!(grid.page.result.columns),
            rows: originals,
        });
        self.workbench.overlay = Some("delete-rows".into());
    }

    pub(super) fn confirm_delete_rows(&mut self) {
        if self.workbench.overlay.as_deref() != Some("delete-rows") {
            return;
        }
        self.workbench.overlay = None;
        let Some(pending) = self.workbench.delete_rows.take() else {
            return;
        };
        let valid_context = self.workbench_can_edit()
            && self.can_copy_insert()
            && self.workbench.inline.is_none()
            && self.paste_preview.is_none()
            && self.page_token.is_none()
            && (self.tabs[self.active].pending.is_null() || self.tabs[self.active].pending_live)
            && self.tabs[self.active].id == pending.tab
            && self.generation == pending.generation
            && self
                .profile
                .as_ref()
                .is_some_and(|profile| profile.id == pending.profile)
            && self.session_id().as_ref() == Some(&pending.session)
            && self
                .target
                .as_ref()
                .is_some_and(|target| json!(target) == pending.target)
            && self.database == pending.database
            && self.result_id.as_ref() == Some(&pending.result_id)
            && self.schema.as_ref() == Some(&pending.schema)
            && self.edit_token.as_ref() == Some(&pending.edit_token);
        let Some(grid) = self.grid.as_ref().filter(|grid| {
            valid_context
                && grid.page.result.id == pending.grid_result_id
                && json!(grid.page.result.columns) == pending.columns
                && pending.rows.iter().all(|(row, original)| {
                    grid.row_in_page(*row)
                        && grid.original_row(*row) == Some(original.as_slice())
                        && grid.page.rows.get((*row - grid.page.start) as usize) == Some(original)
                })
        }) else {
            self.status = UiMessage::key("删除确认已过期，请重新选择行");
            return;
        };
        let keys: Vec<String> = pending
            .schema
            .get("keys")
            .and_then(Value::as_array)
            .and_then(|keys| keys.first())
            .and_then(|keys| serde_json::from_value(keys.clone()).ok())
            .unwrap_or_default();
        let rows: Vec<_> = pending.rows.iter().map(|(row, _)| *row).collect();
        let changes = match grid.delete_changes(&rows, &keys) {
            Ok(changes) => changes,
            Err(error) => {
                self.status = UiMessage::local(error.message);
                return;
            }
        };
        // 可能已有恢复或暂存的更新／删除，不能对同一身份重复生成变更。
        if changes.iter().any(|change| {
            self.pending_rows
                .iter()
                .any(|existing| existing.operation != "insert" && existing.key == change.key)
        }) {
            self.status = UiMessage::key("目标行已有待提交变更");
            return;
        }
        self.pending_rows.extend(changes);
        self.detail = serde_json::to_string_pretty(&self.pending_rows).unwrap_or_default();
        self.set_detail_context("草稿详情");
        self.dirty = true;
        self.status = UiMessage::key("删除已加入待提交变更");
    }

    pub(crate) fn begin_inline(&mut self, backend: &Backend) {
        if self.workbench.inline.is_some() {
            return;
        }
        if !self.tabs[self.active].pending.is_null() && !self.tabs[self.active].pending_live {
            self.status = UiMessage::key("请先恢复或丢弃已保存的编辑意图");
            return;
        }
        if !self.workbench_can_edit() {
            self.status = UiMessage::key("当前结果没有可用的编辑基线或可靠键");
            return;
        }
        let Some(cell) = self.grid.as_ref().and_then(|g| g.focus) else {
            self.status = UiMessage::key("没有选中单元格");
            return;
        };
        if let Err(error) = self.check_rows_not_deleted([cell.row]) {
            self.status = error;
            return;
        }
        let grid = self.grid.as_ref().unwrap();
        if grid
            .page
            .result
            .columns
            .get(cell.column)
            .is_none_or(|c| c.generated)
        {
            self.status = UiMessage::key("该列不可编辑");
            return;
        }
        if self.workbench.overlay.as_deref() == Some("cell") {
            self.workbench.overlay = None;
            self.workbench.cell_target = None;
            self.workbench.cell_fields_open = false;
        }
        let large = matches!(grid.value(cell), Some(DataValue::Large { .. }));
        if !large {
            match grid.cell_edit_text(cell) {
                Ok(text) => self.field_set("cell", text),
                Err(error) => {
                    self.status = grid_error(error);
                    return;
                }
            }
        }
        self.workbench.inline = Some(cell);
        if large {
            self.open_cell(backend);
            if self.large.is_none() {
                self.workbench.inline = None;
            }
        } else {
            self.focus = "cell".into();
            self.workbench.focus_request = Some("cell-editor".into());
        }
    }

    pub(crate) fn finish_inline(&mut self, commit: bool) -> bool {
        let Some(cell) = self.workbench.inline else {
            return true;
        };
        if !commit {
            self.large = None;
            self.workbench.inline = None;
            self.workbench.focus_request = Some(String::new());
            self.focus = "grid".into();
            return true;
        }
        if self.large.is_some() {
            self.status = UiMessage::key("请等待完整字段读取完成");
            return false;
        }
        if !self.workbench_can_edit() {
            self.status = UiMessage::key("编辑基线已失效，请保留草稿并重新打开资源");
            return false;
        }
        if let Err(error) = self.check_rows_not_deleted([cell.row]) {
            self.status = error;
            return false;
        }
        let text = self.field("cell").to_owned();
        let result = self
            .grid
            .as_mut()
            .ok_or_else(|| UiMessage::key("结果已关闭"))
            .and_then(|grid| {
                if grid.focus != Some(cell) {
                    return Err(UiMessage::key("编辑目标已改变"));
                }
                let value = grid
                    .parse_cell_text(cell, &text)
                    .map_err(|error| grid_cell_error(error, grid, cell))?;
                grid.edit(cell, value)
                    .map_err(|error| grid_cell_error(error, grid, cell))
            });
        match result {
            Ok(()) => {
                self.dirty = true;
                self.workbench.inline = None;
                self.workbench.focus_request = Some(String::new());
                self.focus = "grid".into();
                true
            }
            Err(error) => {
                self.field_error("cell", error);
                false
            }
        }
    }
}
