//! 精确值复制、INSERT 编码与粘贴预览。
use super::*;

impl GridState {
    /// 仅接受与结果完整匹配的基表 schema，使用当前页草稿而不是展示文本。
    pub fn copy_insert(
        &self,
        database: &str,
        table: &str,
        schema: &[Column],
        exclude_auto_primary: bool,
    ) -> Result<String> {
        if database.is_empty()
            || table.is_empty()
            || schema.is_empty()
            || schema.len() != self.page.result.columns.len()
            || schema
                .iter()
                .zip(&self.page.result.columns)
                .any(|(a, b)| a.name != b.name)
        {
            return Err(AppError::invalid("结果与基表 schema 不匹配"));
        }
        if self.selected_rows.is_empty() {
            return Err(AppError::invalid("没有选中行"));
        }
        let columns: Vec<_> = schema
            .iter()
            .enumerate()
            .filter(|(_, col)| {
                !col.generated && !(exclude_auto_primary && col.auto_increment && col.primary_key)
            })
            .collect();
        let mut output = String::new();
        for &row in &self.selected_rows {
            if !self.row_in_page(row) {
                return Err(AppError::invalid("选区跨越未加载的结果页"));
            }
            let mut values = Vec::with_capacity(columns.len());
            for &(index, column) in &columns {
                let value = self
                    .value(Cell { row, column: index })
                    .ok_or_else(|| AppError::invalid("结果行缺少列"))?;
                // 默认 sql_mode 会把自增列的零转换为新值，不能保证复制后保真。
                if column.auto_increment
                    && matches!(value,
                    Value::Integer(text) | Value::Unsigned(text) if text.parse::<i128>() == Ok(0))
                {
                    return Err(AppError::invalid(
                        "自增列的零值依赖目标会话 sql_mode，无法安全生成 INSERT",
                    ));
                }
                values.push(mysql_literal(value)?);
            }
            use std::fmt::Write;
            let names = columns
                .iter()
                .map(|(_, col)| mysql_identifier(&col.name))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(
                output,
                "INSERT INTO {}.{} ({}) VALUES ({});",
                mysql_identifier(database),
                mysql_identifier(table),
                names,
                values.join(", ")
            );
        }
        Ok(output)
    }
    /// 将当前页可见选区编码为 TSV；未加载大值和不存在的值不能复制。
    pub fn copy_tsv(&self) -> Result<String> {
        let rows = self
            .selected_cells()?
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|cell| {
                        self.value(cell)
                            .cloned()
                            .ok_or_else(|| AppError::invalid("结果行缺少列"))
                    })
                    .collect()
            })
            .collect::<Result<Vec<Vec<Value>>>>()?;
        encode_tsv(&rows)
    }
    /// 解析 TSV 并按目标列的原值类型生成预览；越界或任一值无效时不产生草稿。
    pub fn preview_paste(&self, text: &str) -> Result<PastePreview> {
        if !self.editable {
            return Err(AppError::invalid("当前结果不可编辑"));
        }
        let focus = self
            .focus
            .ok_or_else(|| AppError::invalid("没有选中单元格"))?;
        let anchor = self.anchor.unwrap_or(focus);
        let row_selection = !self.selected_rows.is_empty();
        let bounded = row_selection || anchor != focus;
        let start = if row_selection {
            Cell {
                row: *self.selected_rows.first().unwrap(),
                column: focus.column,
            }
        } else {
            Cell {
                row: anchor.row.min(focus.row),
                column: anchor.column.min(focus.column),
            }
        };
        let visible = self.visible_columns();
        let col_start = visible
            .iter()
            .position(|&c| c == start.column)
            .ok_or_else(|| AppError::invalid("目标列不可见"))?;
        let mut reader = csv::ReaderBuilder::new()
            .delimiter(b'\t')
            .has_headers(false)
            .flexible(false)
            .from_reader(text.as_bytes());
        let mut edits = Vec::new();
        let mut selected_rows = self.selected_rows.iter().copied();
        for (offset, record) in reader.records().enumerate() {
            let record = record.map_err(|e| AppError::invalid(format!("粘贴内容格式错误: {e}")))?;
            let row = if row_selection {
                selected_rows
                    .next()
                    .ok_or_else(|| AppError::invalid("粘贴范围超出选中行"))?
            } else {
                start
                    .row
                    .checked_add(offset as u64)
                    .ok_or_else(|| AppError::invalid("行号溢出"))?
            };
            if !self.row_in_page(row) || col_start + record.len() > visible.len() {
                return Err(AppError::invalid("粘贴范围超出当前页或可见列"));
            }
            for (offset, field) in record.iter().enumerate() {
                let cell = Cell {
                    row,
                    column: visible[col_start + offset],
                };
                if bounded && !self.is_selected(cell) {
                    return Err(AppError::invalid("粘贴范围超出选区"));
                }
                let column = &self.page.result.columns[cell.column];
                if column.generated {
                    return Err(AppError::invalid(format!("列 {} 不可编辑", column.name)));
                }
                let before = self
                    .value(cell)
                    .ok_or_else(|| AppError::invalid("结果行缺少列"))?
                    .clone();
                let original = self
                    .original_type_value(cell)
                    .ok_or_else(|| AppError::invalid("缺少编辑基线"))?;
                let after = decode_value(field, original, column)?;
                edits.push(CellEdit {
                    cell,
                    before,
                    after,
                });
            }
        }
        if edits.is_empty() {
            return Err(AppError::invalid("粘贴内容为空"));
        }
        Ok(PastePreview { edits })
    }
    pub fn apply_paste(&mut self, preview: PastePreview) -> Result<()> {
        // 先验证整批及预览时的值，防止过期预览或部分失败留下半批草稿。
        let mut seen = BTreeSet::new();
        for edit in &preview.edits {
            let column = self
                .page
                .result
                .columns
                .get(edit.cell.column)
                .ok_or_else(|| AppError::invalid("列不存在"))?;
            if !self.editable
                || column.generated
                || !self.row_in_page(edit.cell.row)
                || !seen.insert(edit.cell)
                || self.value(edit.cell) != Some(&edit.before)
            {
                return Err(AppError::conflict("粘贴预览已过期或目标不可编辑"));
            }
            validate_value(&edit.after, column)?;
            if !matches!(edit.after, Value::Null) {
                let original = self
                    .original_type_value(edit.cell)
                    .ok_or_else(|| AppError::invalid("缺少编辑基线"))?;
                let typed = decode_value(&encode_value(&edit.after)?, original, column)?;
                if std::mem::discriminant(&typed) != std::mem::discriminant(&edit.after) {
                    return Err(AppError::invalid(format!(
                        "列 {} 的值类型不匹配",
                        column.name
                    )));
                }
            }
        }
        let mut undo = Vec::new();
        for edit in preview.edits {
            if edit.before == edit.after {
                continue;
            }
            let row = edit.cell.row;
            // 只在首次修改时捕获基线，后续翻页或再次编辑不能覆盖原值。
            if !self.originals.contains_key(&row) {
                self.originals.insert(
                    row,
                    self.page.rows[(row - self.page.start) as usize].clone(),
                );
            }
            undo.push((edit.cell, self.edits.get(&edit.cell).cloned()));
            if self.original_type_value(edit.cell) == Some(&edit.after) {
                self.edits.remove(&edit.cell);
            } else {
                self.edits.insert(edit.cell, edit.after);
            }
        }
        if !undo.is_empty() {
            self.undo.push(undo);
            self.refresh_natural_widths();
        }
        Ok(())
    }
}
