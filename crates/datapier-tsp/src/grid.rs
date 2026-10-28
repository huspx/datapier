use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::{AppError, Column, Result, ResultPage, RowChange, Value};
use serde::{Deserialize, Serialize};

/// 结果集中的绝对行号和原始列下标，不随分页或隐藏列改变。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Cell {
    pub row: u64,
    pub column: usize,
}

/// 当前页内的键盘导航方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Navigation {
    Up,
    Down,
    Left,
    Right,
    Tab,
    BackTab,
    Home,
    End,
}

/// 按原始列顺序保存的列宽、隐藏与冻结设置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ColumnSetting {
    pub width: u16,
    pub hidden: bool,
    pub frozen: bool,
}
impl Default for ColumnSetting {
    fn default() -> Self {
        Self {
            width: 160,
            hidden: false,
            frozen: false,
        }
    }
}

/// 单个单元格的粘贴前后值，用于预览及失效检测。
#[derive(Debug, Clone)]
pub struct CellEdit {
    pub cell: Cell,
    pub before: Value,
    pub after: Value,
}
/// 尚未应用的整批粘贴变更；应用时会再次校验目标与原值。
#[derive(Debug, Clone)]
pub struct PastePreview {
    pub edits: Vec<CellEdit>,
}

/// 分页 grid、列布局和本地编辑草稿；坐标使用绝对结果行号和原始列下标。
/// 草稿不直接写库，生成变更时仍依赖完整键和首次编辑时保存的基线。
#[derive(Debug, Clone)]
pub struct GridState {
    pub page: ResultPage,
    pub settings: Vec<ColumnSetting>,
    pub focus: Option<Cell>,
    pub anchor: Option<Cell>,
    pub select_rows: bool,
    pub editable: bool,
    // 跨页保留发生编辑的整行基线，以便之后定位行并检查原值。
    originals: BTreeMap<u64, Vec<Value>>,
    resolved: BTreeMap<Cell, Value>,
    edits: BTreeMap<Cell, Value>,
    undo: Vec<Vec<(Cell, Option<Value>)>>,
}

impl GridState {
    pub fn new(page: ResultPage, editable: bool) -> Self {
        let focus = if !page.rows.is_empty() && !page.result.columns.is_empty() {
            Some(Cell {
                row: page.start,
                column: 0,
            })
        } else {
            None
        };
        let settings = vec![ColumnSetting::default(); page.result.columns.len()];
        Self {
            page,
            settings,
            focus,
            anchor: focus,
            select_rows: false,
            editable,
            originals: BTreeMap::new(),
            resolved: BTreeMap::new(),
            edits: BTreeMap::new(),
            undo: Vec::new(),
        }
    }

    /// 切换同一结果集的页面；结果 ID 或列数不匹配时拒绝替换。
    pub fn set_page(&mut self, page: ResultPage) -> Result<()> {
        if page.result.id != self.page.result.id || page.result.columns.len() != self.settings.len()
        {
            return Err(AppError::invalid("结果页与当前 grid 不匹配"));
        }
        self.page = page;
        // 选区采用绝对行号；离开当前页后重置，避免误选新页的同一偏移。
        if !self.focus.is_some_and(|cell| self.row_in_page(cell.row)) {
            self.focus = self.first_cell();
            self.anchor = self.focus;
            self.select_rows = false;
        }
        Ok(())
    }

    pub fn visible_columns(&self) -> Vec<usize> {
        self.settings
            .iter()
            .enumerate()
            .filter_map(|(i, s)| (!s.hidden).then_some(i))
            .collect()
    }
    pub fn frozen_offset(&self, column: usize) -> Option<u32> {
        let setting = self.settings.get(column)?;
        if setting.hidden || !setting.frozen {
            return None;
        }
        Some(
            self.settings[..column]
                .iter()
                .filter(|s| s.frozen && !s.hidden)
                .map(|s| u32::from(s.width))
                .sum(),
        )
    }
    pub fn set_column(&mut self, column: usize, setting: ColumnSetting) -> Result<()> {
        if setting.width == 0 {
            return Err(AppError::invalid("列宽必须大于零"));
        }
        let target = self
            .settings
            .get_mut(column)
            .ok_or_else(|| AppError::invalid("列不存在"))?;
        *target = setting;
        if self.focus.is_some_and(|c| self.settings[c.column].hidden) {
            self.focus = self.first_cell();
            self.anchor = self.focus;
        }
        Ok(())
    }
    fn row_in_page(&self, row: u64) -> bool {
        row >= self.page.start && row - self.page.start < self.page.rows.len() as u64
    }
    fn first_cell(&self) -> Option<Cell> {
        if self.page.rows.is_empty() {
            return None;
        }
        self.visible_columns().first().map(|column| Cell {
            row: self.page.start,
            column: *column,
        })
    }
    pub fn select(&mut self, cell: Cell, extend: bool, rows: bool) -> Result<()> {
        if !self.row_in_page(cell.row) || self.settings.get(cell.column).is_none_or(|s| s.hidden) {
            return Err(AppError::invalid("选区不在当前页或列已隐藏"));
        }
        if !extend || self.anchor.is_none() {
            self.anchor = Some(cell);
        }
        self.focus = Some(cell);
        self.select_rows = rows;
        Ok(())
    }
    pub fn is_selected(&self, cell: Cell) -> bool {
        let Some((anchor, focus)) = self.anchor.zip(self.focus) else {
            return false;
        };
        self.row_in_page(cell.row)
            && self
                .settings
                .get(cell.column)
                .is_some_and(|setting| !setting.hidden)
            && (anchor.row.min(focus.row)..=anchor.row.max(focus.row)).contains(&cell.row)
            && (self.select_rows
                || (anchor.column.min(focus.column)..=anchor.column.max(focus.column))
                    .contains(&cell.column))
    }
    pub fn navigate(&mut self, direction: Navigation, extend: bool) {
        let Some(mut cell) = self.focus.or_else(|| self.first_cell()) else {
            return;
        };
        let columns = self.visible_columns();
        let Some(index) = columns.iter().position(|&c| c == cell.column) else {
            return;
        };
        let last_row = self.page.start + self.page.rows.len().saturating_sub(1) as u64;
        match direction {
            Navigation::Up => cell.row = cell.row.saturating_sub(1).max(self.page.start),
            Navigation::Down => cell.row = cell.row.saturating_add(1).min(last_row),
            Navigation::Left => cell.column = columns[index.saturating_sub(1)],
            Navigation::Right => cell.column = columns[(index + 1).min(columns.len() - 1)],
            Navigation::Home => cell.column = columns[0],
            Navigation::End => cell.column = columns[columns.len() - 1],
            Navigation::Tab => {
                if index + 1 < columns.len() {
                    cell.column = columns[index + 1];
                } else if cell.row < last_row {
                    cell.row += 1;
                    cell.column = columns[0];
                }
            }
            Navigation::BackTab => {
                if index > 0 {
                    cell.column = columns[index - 1];
                } else if cell.row > self.page.start {
                    cell.row -= 1;
                    cell.column = columns[columns.len() - 1];
                }
            }
        }
        if !extend || self.anchor.is_none() {
            self.anchor = Some(cell);
        }
        self.focus = Some(cell);
    }
    pub fn value(&self, cell: Cell) -> Option<&Value> {
        self.edits
            .get(&cell)
            .or_else(|| self.resolved.get(&cell))
            .or_else(|| {
                if !self.row_in_page(cell.row) {
                    return None;
                }
                self.page
                    .rows
                    .get((cell.row - self.page.start) as usize)?
                    .get(cell.column)
            })
    }
    pub fn original(&self, cell: Cell) -> Option<&Value> {
        self.originals
            .get(&cell.row)
            .and_then(|row| row.get(cell.column))
            .or_else(|| {
                if !self.row_in_page(cell.row) {
                    return None;
                }
                self.page
                    .rows
                    .get((cell.row - self.page.start) as usize)?
                    .get(cell.column)
            })
    }
    pub fn original_row(&self, row: u64) -> Option<&[Value]> {
        self.originals.get(&row).map(Vec::as_slice).or_else(|| {
            if !self.row_in_page(row) {
                return None;
            }
            self.page
                .rows
                .get((row - self.page.start) as usize)
                .map(Vec::as_slice)
        })
    }
    /// 缓存完整大字段以供编辑；类型与基线不一致或仍未完整读取时返回错误。
    pub fn resolve_large(&mut self, cell: Cell, full: Value) -> Result<()> {
        let original = self
            .original(cell)
            .ok_or_else(|| AppError::invalid("缺少编辑基线"))?;
        let Value::Large { original_type, .. } = original else {
            return Err(AppError::invalid("目标单元格不是大字段"));
        };
        let kind = match &full {
            Value::Null => "null",
            Value::Missing => "missing",
            Value::Bool(_) => "bool",
            Value::Integer(_) => "integer",
            Value::Unsigned(_) => "unsigned",
            Value::Decimal(_) => "decimal",
            Value::Float(_) => "float",
            Value::Text(_) => "text",
            Value::Bytes(_) => "bytes",
            Value::Date(_) => "date",
            Value::Time(_) => "time",
            Value::DateTime(_) => "date_time",
            Value::Json(_) => "json",
            Value::Array(_) => "array",
            Value::Map(_) => "map",
            Value::Large { .. } => return Err(AppError::invalid("大字段尚未完整读取")),
        };
        if kind != original_type {
            return Err(AppError::invalid("大字段类型与原值不匹配"));
        }
        self.resolved.insert(cell, full);
        Ok(())
    }
    fn original_type_value(&self, cell: Cell) -> Option<&Value> {
        self.resolved.get(&cell).or_else(|| self.original(cell))
    }
    pub fn cell_edit_text(&self, cell: Cell) -> Result<String> {
        if !self.row_in_page(cell.row) {
            return Err(AppError::invalid("目标单元格不在当前页"));
        }
        encode_value(
            self.value(cell)
                .ok_or_else(|| AppError::invalid("目标单元格不存在"))?,
        )
    }
    pub fn parse_cell_text(&self, cell: Cell, text: &str) -> Result<Value> {
        if !self.editable || !self.row_in_page(cell.row) {
            return Err(AppError::invalid("目标单元格不可编辑"));
        }
        let column = self
            .page
            .result
            .columns
            .get(cell.column)
            .ok_or_else(|| AppError::invalid("列不存在"))?;
        if column.generated {
            return Err(AppError::invalid(format!("列 {} 不可编辑", column.name)));
        }
        let original = self
            .original_type_value(cell)
            .ok_or_else(|| AppError::invalid("缺少编辑基线"))?;
        decode_value(text, original, column)
    }
    fn selected_cells(&self) -> Result<Vec<Vec<Cell>>> {
        let (anchor, focus) = self
            .anchor
            .zip(self.focus)
            .ok_or_else(|| AppError::invalid("没有选中单元格"))?;
        let start = anchor.row.min(focus.row);
        let end = anchor.row.max(focus.row);
        if !self.row_in_page(start) || !self.row_in_page(end) {
            return Err(AppError::invalid("选区跨越未加载的结果页"));
        }
        let cols: Vec<_> = self
            .visible_columns()
            .into_iter()
            .filter(|&col| {
                self.select_rows
                    || (col >= anchor.column.min(focus.column)
                        && col <= anchor.column.max(focus.column))
            })
            .collect();
        Ok((start..=end)
            .map(|row| cols.iter().map(|&column| Cell { row, column }).collect())
            .collect())
    }
    /// 将当前页可见选区编码为 TSV；未加载大值和不存在的值不能复制。
    pub fn copy_tsv(&self) -> Result<String> {
        let mut writer = csv::WriterBuilder::new()
            .delimiter(b'\t')
            .terminator(csv::Terminator::Any(b'\n'))
            .from_writer(Vec::new());
        for row in self.selected_cells()? {
            let fields = row
                .into_iter()
                .map(|cell| {
                    self.value(cell)
                        .ok_or_else(|| AppError::invalid("结果行缺少列"))
                        .and_then(encode_value)
                })
                .collect::<Result<Vec<_>>>()?;
            writer
                .write_record(fields)
                .map_err(|e| AppError::invalid(e.to_string()))?;
        }
        let bytes = writer
            .into_inner()
            .map_err(|e| AppError::invalid(e.to_string()))?;
        String::from_utf8(bytes).map_err(|e| AppError::invalid(e.to_string()))
    }
    /// 解析 TSV 并按目标列的原值类型生成预览；越界或任一值无效时不产生草稿。
    pub fn preview_paste(&self, text: &str) -> Result<PastePreview> {
        if !self.editable {
            return Err(AppError::invalid("当前结果不可编辑"));
        }
        let start = self
            .focus
            .ok_or_else(|| AppError::invalid("没有选中单元格"))?;
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
        for (offset, record) in reader.records().enumerate() {
            let record = record.map_err(|e| AppError::invalid(format!("粘贴内容格式错误: {e}")))?;
            let row = start
                .row
                .checked_add(offset as u64)
                .ok_or_else(|| AppError::invalid("行号溢出"))?;
            if !self.row_in_page(row) || col_start + record.len() > visible.len() {
                return Err(AppError::invalid("粘贴范围超出当前页或可见列"));
            }
            for (offset, field) in record.iter().enumerate() {
                let cell = Cell {
                    row,
                    column: visible[col_start + offset],
                };
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
        }
        Ok(())
    }
    pub fn edit(&mut self, cell: Cell, value: Value) -> Result<()> {
        let before = self
            .value(cell)
            .ok_or_else(|| AppError::invalid("目标单元格不在当前页"))?
            .clone();
        self.apply_paste(PastePreview {
            edits: vec![CellEdit {
                cell,
                before,
                after: value,
            }],
        })
    }
    pub fn undo(&mut self) -> bool {
        let Some(batch) = self.undo.pop() else {
            return false;
        };
        for (cell, previous) in batch.into_iter().rev() {
            if let Some(value) = previous {
                self.edits.insert(cell, value);
            } else {
                self.edits.remove(&cell);
            }
        }
        true
    }
    pub fn discard(&mut self) {
        self.edits.clear();
        self.originals.clear();
        self.undo.clear();
    }
    pub fn edits(&self) -> &BTreeMap<Cell, Value> {
        &self.edits
    }
    /// 使用调用方从 schema 选出的完整可靠键生成行更新；缺少基线或可定位键时报错。
    pub fn changes(&self, key_columns: &[String]) -> Result<Vec<RowChange>> {
        if key_columns.is_empty() && !self.edits.is_empty() {
            return Err(AppError::invalid("没有可用于定位行的完整键"));
        }
        let mut rows: BTreeMap<u64, RowChange> = BTreeMap::new();
        for (cell, value) in &self.edits {
            if let std::collections::btree_map::Entry::Vacant(e) = rows.entry(cell.row) {
                let original = self
                    .originals
                    .get(&cell.row)
                    .ok_or_else(|| AppError::invalid("缺少编辑基线"))?;
                if original.len() != self.page.result.columns.len() {
                    return Err(AppError::invalid("编辑基线列数不匹配"));
                }
                let before: BTreeMap<_, _> = self
                    .page
                    .result
                    .columns
                    .iter()
                    .zip(original)
                    .map(|(c, v)| (c.name.clone(), v.clone()))
                    .collect();
                let mut key = BTreeMap::new();
                for name in key_columns {
                    let value = before
                        .get(name)
                        .ok_or_else(|| AppError::invalid(format!("缺少键列 {name}")))?;
                    if matches!(value, Value::Null | Value::Missing | Value::Large { .. }) {
                        return Err(AppError::invalid(format!("键列 {name} 无法定位行")));
                    }
                    key.insert(name.clone(), value.clone());
                }
                e.insert(RowChange {
                    operation: "update".into(),
                    key,
                    before,
                    values: BTreeMap::new(),
                });
            }
            rows.get_mut(&cell.row).unwrap().values.insert(
                self.page.result.columns[cell.column].name.clone(),
                value.clone(),
            );
        }
        Ok(rows.into_values().collect())
    }
}

// \N 专用于 NULL；非 NULL 值中的反斜线加倍，避免文本 \N 与 NULL 混淆。
// Bytes 沿用模型中的编码字符串，不能使用展示文本替代原值。
fn encode_value(value: &Value) -> Result<String> {
    let text = match value {
        Value::Null => return Ok("\\N".into()),
        Value::Missing | Value::Large { .. } => {
            return Err(AppError::invalid("不存在的值或未加载的大值不能复制"));
        }
        Value::Bool(v) => v.to_string(),
        Value::Integer(v)
        | Value::Unsigned(v)
        | Value::Decimal(v)
        | Value::Float(v)
        | Value::Text(v)
        | Value::Bytes(v)
        | Value::Date(v)
        | Value::Time(v)
        | Value::DateTime(v) => v.clone(),
        Value::Json(v) => serde_json::to_string(v)?,
        Value::Array(_) | Value::Map(_) => serde_json::to_string(value)?,
    };
    Ok(text.replace('\\', "\\\\"))
}

fn decode_value(field: &str, original: &Value, column: &Column) -> Result<Value> {
    if field == "\\N" {
        let value = Value::Null;
        validate_value(&value, column)?;
        return Ok(value);
    }
    let mut text = String::with_capacity(field.len());
    let mut chars = field.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if chars.next() != Some('\\') {
                return Err(AppError::invalid("非 NULL 的反斜线必须成对出现"));
            }
            text.push('\\');
        } else {
            text.push(ch);
        }
    }
    let kind = column.type_name.to_ascii_lowercase();
    let value = match original {
        Value::Bool(_) => Value::Bool(match text.as_str() {
            "true" | "1" => true,
            "false" | "0" => false,
            _ => return Err(AppError::invalid("布尔值必须为 true、false、1 或 0")),
        }),
        Value::Integer(_) => Value::Integer(text),
        Value::Unsigned(_) => Value::Unsigned(text),
        Value::Decimal(_) => Value::Decimal(text),
        Value::Float(_) => Value::Float(text),
        Value::Text(_) => Value::Text(text),
        Value::Bytes(_) => Value::Bytes(text),
        Value::Date(_) => Value::Date(text),
        Value::Time(_) => Value::Time(text),
        Value::DateTime(_) => Value::DateTime(text),
        Value::Json(_) => Value::Json(serde_json::from_str(&text)?),
        Value::Array(_) | Value::Map(_) => serde_json::from_str(&text)?,
        Value::Missing | Value::Large { .. } => {
            return Err(AppError::invalid("不存在的值或未加载的大值不可编辑"));
        }
        Value::Null => {
            if kind.contains("unsigned") {
                Value::Unsigned(text)
            } else if kind.contains("int") {
                Value::Integer(text)
            } else if kind.contains("decimal") || kind.contains("numeric") {
                Value::Decimal(text)
            } else if kind.contains("float") || kind.contains("double") || kind.contains("real") {
                Value::Float(text)
            } else if kind.contains("binary") || kind.contains("blob") || kind.contains("bytes") {
                Value::Bytes(text)
            } else if kind.contains("json") {
                Value::Json(serde_json::from_str(&text)?)
            } else if kind.contains("datetime") || kind.contains("timestamp") {
                Value::DateTime(text)
            } else if kind == "date" {
                Value::Date(text)
            } else if kind == "time" {
                Value::Time(text)
            } else if kind == "bool" || kind == "boolean" {
                Value::Bool(match text.as_str() {
                    "true" | "1" => true,
                    "false" | "0" => false,
                    _ => return Err(AppError::invalid("无效布尔值")),
                })
            } else {
                Value::Text(text)
            }
        }
    };
    validate_value(&value, column)?;
    Ok(value)
}

fn validate_value(value: &Value, column: &Column) -> Result<()> {
    match value {
        Value::Null if !column.nullable => {
            return Err(AppError::invalid(format!("列 {} 不允许 NULL", column.name)));
        }
        Value::Missing | Value::Large { .. } => {
            return Err(AppError::invalid("不存在的值或未加载的大值不可编辑"));
        }
        Value::Integer(s) if s.parse::<i128>().is_err() => {
            return Err(AppError::invalid("无效整数"));
        }
        Value::Unsigned(s) if s.parse::<u128>().is_err() => {
            return Err(AppError::invalid("无效无符号整数"));
        }
        Value::Decimal(s) if !valid_decimal(s) => return Err(AppError::invalid("无效精确小数")),
        Value::Float(s) if s.parse::<f64>().is_err() => {
            return Err(AppError::invalid("无效浮点数"));
        }
        Value::Bytes(s) if STANDARD.decode(s).is_err() => {
            return Err(AppError::invalid("二进制值必须是 Base64"));
        }
        _ => {}
    }
    Ok(())
}
fn valid_decimal(s: &str) -> bool {
    let s = s
        .strip_prefix('-')
        .or_else(|| s.strip_prefix('+'))
        .unwrap_or(s);
    let mut parts = s.split('.');
    let integer = parts.next().unwrap_or("");
    let fraction = parts.next();
    parts.next().is_none()
        && (!integer.is_empty() || fraction.is_some_and(|s| !s.is_empty()))
        && integer.bytes().all(|c| c.is_ascii_digit())
        && fraction.is_none_or(|s| s.bytes().all(|c| c.is_ascii_digit()))
}
