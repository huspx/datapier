use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::{AppError, Column, Result, ResultPage, RowChange, Value};
use serde::{Deserialize, Serialize};

/// 仅在结果列与基表列一一对应时合并约束；失败时不修改页面。
pub(super) fn merge_table_columns(page: &mut ResultPage, columns: &[Column]) -> Result<()> {
    if page.result.columns.len() != columns.len() {
        return Err(AppError::invalid("结果列与基表结构不匹配"));
    }
    let mut by_name = BTreeMap::new();
    for column in columns {
        if by_name.insert(column.name.as_str(), column).is_some() {
            return Err(AppError::invalid("基表结构包含重复列名"));
        }
    }
    let mut seen = BTreeSet::new();
    for column in &page.result.columns {
        if !seen.insert(column.name.as_str()) || !by_name.contains_key(column.name.as_str()) {
            return Err(AppError::invalid("结果列与基表结构不匹配"));
        }
    }
    for column in &mut page.result.columns {
        let source = by_name[column.name.as_str()];
        column.nullable = source.nullable;
        column.generated = source.generated;
        column.primary_key = source.primary_key;
        column.auto_increment = source.auto_increment;
        column.default_value = source.default_value.clone();
    }
    Ok(())
}

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
    // 旧工作区没有该标记时保留原列宽；新建布局随 pane 自适应，手动设置后固定。
    #[serde(default)]
    pub auto_width: bool,
}
impl Default for ColumnSetting {
    fn default() -> Self {
        Self {
            width: 160,
            hidden: false,
            frozen: false,
            auto_width: true,
        }
    }
}

fn content_width<'a>(column: &Column, values: impl Iterator<Item = &'a Value>) -> u16 {
    fn units(text: &str) -> usize {
        text.chars()
            .take(66)
            .map(|c| if c.is_ascii() { 1 } else { 2 })
            .sum()
    }
    let mut width = (units(&column.name) * 7 + 24).max(units(&column.type_name) * 5 + 24);
    for value in values {
        let length = match value {
            Value::Integer(text)
            | Value::Unsigned(text)
            | Value::Decimal(text)
            | Value::Float(text)
            | Value::Text(text)
            | Value::Date(text)
            | Value::Time(text)
            | Value::DateTime(text) => units(text),
            Value::Bytes(encoded) => encoded.len().saturating_mul(3) / 4,
            Value::Large { preview, .. } => units(preview),
            Value::Null | Value::Bool(_) => 5,
            Value::Missing => 6,
            Value::Json(_) | Value::Array(_) | Value::Map(_) => 40,
        };
        width = width.max(length.min(66) * 7 + 24);
    }
    width.clamp(64, 480) as u16
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
    selected_rows: BTreeSet<u64>,
    row_anchor: Option<u64>,
    visible_rows: Option<BTreeSet<u64>>,
    pub editable: bool,
    // 跨页保留发生编辑的整行基线，以便之后定位行并检查原值。
    originals: BTreeMap<u64, Vec<Value>>,
    resolved: BTreeMap<Cell, Value>,
    edits: BTreeMap<Cell, Value>,
    undo: Vec<Vec<(Cell, Option<Value>)>>,
    natural_widths: Vec<u16>,
    fitted_width: Option<(u32, bool)>,
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
        let natural_widths = page
            .result
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                content_width(column, page.rows.iter().filter_map(|row| row.get(index)))
            })
            .collect();
        Self {
            page,
            settings,
            focus,
            anchor: focus,
            selected_rows: BTreeSet::new(),
            row_anchor: None,
            visible_rows: None,
            editable,
            originals: BTreeMap::new(),
            resolved: BTreeMap::new(),
            edits: BTreeMap::new(),
            undo: Vec::new(),
            natural_widths,
            fitted_width: None,
        }
    }

    /// 切换同一结果集的页面；结果 ID 或列数不匹配时拒绝替换。
    pub fn set_page(&mut self, page: ResultPage) -> Result<()> {
        if page.result.id != self.page.result.id || page.result.columns.len() != self.settings.len()
        {
            return Err(AppError::invalid("结果页与当前 grid 不匹配"));
        }
        self.page = page;
        self.refresh_natural_widths();
        // 行身份只在当前页有效，翻页不能把相同偏移误认为原来的行。
        self.selected_rows.clear();
        self.row_anchor = None;
        self.visible_rows = None;
        if !self.focus.is_some_and(|cell| self.row_in_page(cell.row)) {
            self.focus = self.first_cell();
            self.anchor = self.focus;
        }
        Ok(())
    }
    fn refresh_natural_widths(&mut self) {
        self.natural_widths = self
            .page
            .result
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                content_width(
                    column,
                    (0..self.page.rows.len()).filter_map(|row| {
                        self.value(Cell {
                            row: self.page.start + row as u64,
                            column: index,
                        })
                    }),
                )
            })
            .collect();
        self.fitted_width = None;
    }

    /// 自动列只分配剩余空间，显式列宽不变；窄窗口保留内容最小宽度并横向滚动。
    pub fn fit_columns(&mut self, available: u32, mysql: bool) {
        if self.fitted_width == Some((available, mysql)) {
            return;
        }
        self.fitted_width = Some((available, mysql));
        let mut fixed = 0u32;
        let mut natural = 0u32;
        let mut last_auto = None;
        for (index, setting) in self.settings.iter().enumerate() {
            if setting.hidden {
                continue;
            }
            if setting.auto_width {
                natural += u32::from(if mysql {
                    self.natural_widths[index]
                } else {
                    self.natural_widths[index].min(320)
                });
                last_auto = Some(index);
            } else {
                fixed += u32::from(setting.width);
            }
        }
        if natural == 0 {
            return;
        }
        if mysql {
            // 按自然宽度分配剩余空间；达到上限的列退出分配，余量继续分给其他列。
            let mut remaining = available.saturating_sub(fixed).max(natural);
            let mut weight = natural;
            let mut pending: Vec<usize> = self
                .settings
                .iter()
                .enumerate()
                .filter_map(|(i, s)| (!s.hidden && s.auto_width).then_some(i))
                .collect();
            loop {
                let capped: Vec<usize> = pending
                    .iter()
                    .copied()
                    .filter(|&i| {
                        u64::from(remaining) * u64::from(self.natural_widths[i])
                            >= u64::from(480u32) * u64::from(weight)
                    })
                    .collect();
                if capped.is_empty() {
                    break;
                }
                for i in capped {
                    self.settings[i].width = 480;
                    remaining = remaining.saturating_sub(480);
                    weight -= u32::from(self.natural_widths[i]);
                    pending.retain(|&item| item != i);
                }
                if pending.is_empty() {
                    return;
                }
            }
            for (position, &i) in pending.iter().enumerate() {
                let width = if position + 1 == pending.len() {
                    remaining
                } else {
                    (u64::from(remaining) * u64::from(self.natural_widths[i]) / u64::from(weight))
                        as u32
                };
                self.settings[i].width = width.clamp(64, 480) as u16;
                remaining = remaining.saturating_sub(u32::from(self.settings[i].width));
                weight -= u32::from(self.natural_widths[i]);
            }
            return;
        }
        let target = available.saturating_sub(fixed).max(natural);
        let mut used = 0u32;
        for (index, setting) in self.settings.iter_mut().enumerate() {
            if setting.hidden || !setting.auto_width {
                continue;
            }
            let width = if Some(index) == last_auto {
                target.saturating_sub(used)
            } else {
                (u64::from(target) * u64::from(self.natural_widths[index].min(320))
                    / u64::from(natural)) as u32
            };
            setting.width = width.clamp(1, u32::from(u16::MAX)) as u16;
            used += u32::from(setting.width);
        }
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
        *target = ColumnSetting {
            auto_width: false,
            ..setting
        };
        self.fitted_width = None;
        if self.focus.is_some_and(|c| self.settings[c.column].hidden) {
            self.focus = self.first_cell();
            self.anchor = self.focus;
        }
        Ok(())
    }
    pub(super) fn row_in_page(&self, row: u64) -> bool {
        row >= self.page.start && row - self.page.start < self.page.rows.len() as u64
    }
    pub(super) fn row_visible(&self, row: u64) -> bool {
        self.row_in_page(row)
            && self
                .visible_rows
                .as_ref()
                .is_none_or(|rows| rows.contains(&row))
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
    /// 更新筛选后的行集合，并清除已不可见的行选择和锚点。
    pub(super) fn set_visible_rows(&mut self, rows: impl IntoIterator<Item = u64>) {
        let rows: BTreeSet<_> = rows
            .into_iter()
            .filter(|&row| self.row_in_page(row))
            .collect();
        self.selected_rows.retain(|row| rows.contains(row));
        if self.row_anchor.is_some_and(|row| !rows.contains(&row)) {
            self.row_anchor = None;
        }
        if self.focus.is_some_and(|cell| !rows.contains(&cell.row)) {
            self.focus = None;
        }
        if self.anchor.is_some_and(|cell| !rows.contains(&cell.row)) {
            self.anchor = self.focus;
        }
        self.visible_rows = Some(rows);
    }

    /// 选择单元格范围，普通单元格操作不构造行选区。
    pub fn select(&mut self, cell: Cell, extend: bool) -> Result<()> {
        if !self.row_in_page(cell.row) || self.settings.get(cell.column).is_none_or(|s| s.hidden) {
            return Err(AppError::invalid("选区不在当前页或列已隐藏"));
        }
        if !extend || self.anchor.is_none() {
            self.anchor = Some(cell);
        }
        self.focus = Some(cell);
        self.selected_rows.clear();
        self.row_anchor = None;
        Ok(())
    }
    /// 按当前页绝对行号选择；Shift 扩展锚点范围，add 切换单行。
    pub fn select_row(&mut self, row: u64, column: usize, extend: bool, add: bool) -> Result<()> {
        if !self.row_in_page(row)
            || self
                .visible_rows
                .as_ref()
                .is_some_and(|rows| !rows.contains(&row))
        {
            return Err(AppError::invalid("选区不在当前页或列已隐藏"));
        }
        let focus_column = self
            .settings
            .get(column)
            .filter(|s| !s.hidden)
            .map(|_| column)
            .or_else(|| self.visible_columns().first().copied());
        let cell = focus_column.map(|column| Cell { row, column });
        if extend {
            let anchor = self.row_anchor.unwrap_or(row);
            if !add {
                self.selected_rows.clear();
            }
            if let Some(visible) = &self.visible_rows {
                self.selected_rows
                    .extend(visible.range(anchor.min(row)..=anchor.max(row)).copied());
            } else {
                self.selected_rows.extend(anchor.min(row)..=anchor.max(row));
            }
            self.row_anchor = Some(anchor);
        } else if add {
            if !self.selected_rows.remove(&row) {
                self.selected_rows.insert(row);
            }
            self.row_anchor = Some(row);
        } else {
            self.selected_rows.clear();
            self.selected_rows.insert(row);
            self.row_anchor = Some(row);
        }
        self.focus = cell;
        self.anchor = cell;
        Ok(())
    }
    pub fn is_row_selected(&self, row: u64) -> bool {
        self.row_in_page(row) && self.selected_rows.contains(&row)
    }
    pub fn selected_row_count(&self) -> usize {
        self.selected_rows.len()
    }
    /// 当前页选中行的绝对行号，按结果自然顺序排列。
    pub fn selected_rows(&self) -> Vec<u64> {
        self.selected_rows.iter().copied().collect()
    }
    pub fn is_selected(&self, cell: Cell) -> bool {
        if !self.row_in_page(cell.row)
            || self
                .settings
                .get(cell.column)
                .is_none_or(|setting| setting.hidden)
        {
            return false;
        }
        if !self.selected_rows.is_empty() {
            return self.selected_rows.contains(&cell.row);
        }
        let Some((anchor, focus)) = self.anchor.zip(self.focus) else {
            return false;
        };
        (anchor.row.min(focus.row)..=anchor.row.max(focus.row)).contains(&cell.row)
            && (anchor.column.min(focus.column)..=anchor.column.max(focus.column))
                .contains(&cell.column)
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
        self.selected_rows.clear();
        self.row_anchor = None;
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
    pub(super) fn selected_cells(&self) -> Result<Vec<Vec<Cell>>> {
        if !self.selected_rows.is_empty() {
            return Ok(self
                .selected_rows
                .iter()
                .map(|&row| {
                    (0..self.page.result.columns.len())
                        .map(|column| Cell { row, column })
                        .collect()
                })
                .collect());
        }
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
                col >= anchor.column.min(focus.column) && col <= anchor.column.max(focus.column)
            })
            .collect();
        Ok((start..=end)
            .filter(|row| self.row_visible(*row))
            .map(|row| cols.iter().map(|&column| Cell { row, column }).collect())
            .collect())
    }
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
        self.refresh_natural_widths();
        true
    }
    pub fn discard(&mut self) {
        self.edits.clear();
        self.originals.clear();
        self.undo.clear();
        self.refresh_natural_widths();
    }
    pub fn edits(&self) -> &BTreeMap<Cell, Value> {
        &self.edits
    }
    /// 全部校验通过后才返回删除变更；调用方负责检查已有待提交行。
    pub(super) fn delete_changes(
        &self,
        rows: &[u64],
        key_columns: &[String],
    ) -> Result<Vec<RowChange>> {
        if !self.editable || rows.is_empty() || key_columns.is_empty() {
            return Err(AppError::invalid("当前结果没有可用的编辑基线或可靠键"));
        }
        let mut changes: Vec<RowChange> = Vec::with_capacity(rows.len());
        for &row in rows {
            if !self.row_in_page(row) || self.edits.keys().any(|cell| cell.row == row) {
                return Err(AppError::invalid("目标行已失效或包含单元格修改"));
            }
            let original = self
                .original_row(row)
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
                .map(|(column, value)| (column.name.clone(), value.clone()))
                .collect();
            if before.len() != original.len() {
                return Err(AppError::invalid("编辑基线包含重复列名"));
            }
            let mut key = BTreeMap::new();
            for name in key_columns {
                let value = before
                    .get(name)
                    .ok_or_else(|| AppError::invalid("没有可靠行标识"))?;
                if matches!(value, Value::Null | Value::Missing | Value::Large { .. })
                    || key.insert(name.clone(), value.clone()).is_some()
                {
                    return Err(AppError::invalid("没有可靠行标识"));
                }
            }
            if changes.iter().any(|change| change.key == key) {
                return Err(AppError::invalid("选中行包含重复的行标识"));
            }
            changes.push(RowChange {
                operation: "delete".into(),
                key,
                before,
                values: BTreeMap::new(),
            });
        }
        Ok(changes)
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

pub(super) fn encode_tsv(rows: &[Vec<Value>]) -> Result<String> {
    let mut writer = csv::WriterBuilder::new()
        .delimiter(b'\t')
        .terminator(csv::Terminator::Any(b'\n'))
        .from_writer(Vec::new());
    for row in rows {
        let fields = row.iter().map(encode_value).collect::<Result<Vec<_>>>()?;
        writer
            .write_record(fields)
            .map_err(|e| AppError::invalid(e.to_string()))?;
    }
    let bytes = writer
        .into_inner()
        .map_err(|e| AppError::invalid(e.to_string()))?;
    String::from_utf8(bytes).map_err(|e| AppError::invalid(e.to_string()))
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

/// 表单文本是原文，NULL 由单独控件表达；复用网格的类型和校验规则。
pub(crate) fn decode_form_value(text: &str, null: bool, column: &Column) -> Result<Value> {
    if null {
        decode_value("\\N", &Value::Null, column)
    } else {
        decode_value(&text.replace('\\', "\\\\"), &Value::Null, column)
    }
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
            let base = kind
                .split(|c: char| c == '(' || c.is_ascii_whitespace())
                .next()
                .unwrap_or("");
            if base == "decimal" || base == "numeric" {
                Value::Decimal(text)
            } else if matches!(base, "float" | "double" | "real") {
                Value::Float(text)
            } else if matches!(
                base,
                "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "bigint"
            ) {
                if kind.contains("unsigned") || kind.contains("zerofill") {
                    Value::Unsigned(text)
                } else {
                    Value::Integer(text)
                }
            } else if matches!(
                base,
                "binary"
                    | "varbinary"
                    | "tinyblob"
                    | "blob"
                    | "mediumblob"
                    | "longblob"
                    | "bytes"
                    | "bit"
            ) {
                Value::Bytes(text)
            } else if base == "json" {
                Value::Json(serde_json::from_str(&text)?)
            } else if matches!(base, "datetime" | "timestamp") {
                Value::DateTime(text)
            } else if base == "date" {
                Value::Date(text)
            } else if base == "time" {
                Value::Time(text)
            } else if matches!(base, "bool" | "boolean") {
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
        Value::Float(s) if !s.parse::<f64>().is_ok_and(f64::is_finite) => {
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

// 反引号标识符使用 MySQL 的双反引号转义，与字符串的 sql_mode 无关。
pub(super) fn mysql_identifier(value: &str) -> String {
    format!("`{}`", value.replace('`', "``"))
}

// 文本使用显式 utf8mb4 十六进制字面量，避免反斜线、引号及 SQL mode 改变原值。
fn mysql_hex(bytes: &[u8], text: bool) -> String {
    use std::fmt::Write;
    let mut output = if text {
        "_utf8mb4 X'".to_owned()
    } else {
        "X'".to_owned()
    };
    for byte in bytes {
        let _ = write!(output, "{byte:02X}");
    }
    output.push('\'');
    output
}

fn mysql_literal(value: &Value) -> Result<String> {
    match value {
        Value::Null => Ok("NULL".into()),
        Value::Bool(value) => Ok(if *value { "1" } else { "0" }.into()),
        Value::Integer(value) | Value::Unsigned(value) | Value::Decimal(value) => {
            if !valid_decimal(value) || value.bytes().filter(u8::is_ascii_digit).count() > 65 {
                return Err(AppError::invalid("数值不能无损编码为 MySQL 字面量"));
            }
            Ok(value.clone())
        }
        Value::Float(value) => {
            // 防止特殊浮点值及非数值 SQL 进入生成文本。
            if !value.parse::<f64>().is_ok_and(f64::is_finite)
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E'))
            {
                return Err(AppError::invalid("浮点值不能无损编码为 MySQL 字面量"));
            }
            Ok(value.clone())
        }
        Value::Text(value) | Value::Date(value) | Value::Time(value) | Value::DateTime(value) => {
            Ok(mysql_hex(value.as_bytes(), true))
        }
        Value::Bytes(value) => STANDARD
            .decode(value)
            .map(|bytes| mysql_hex(&bytes, false))
            .map_err(|_| AppError::invalid("二进制值不是有效 Base64")),
        Value::Json(value) => Ok(format!(
            "CAST({} AS JSON)",
            mysql_hex(serde_json::to_string(value)?.as_bytes(), true)
        )),
        Value::Missing | Value::Large { .. } => {
            Err(AppError::invalid("不存在的值或未加载的大值不能复制"))
        }
        Value::Array(_) | Value::Map(_) => {
            Err(AppError::invalid("该值不能无损编码为 MySQL 字面量"))
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/grid.rs"]
mod selection_tests;
