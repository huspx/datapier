use std::collections::{BTreeMap, BTreeSet};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use datapier_core::model::{AppError, Column, Result, ResultPage, RowChange, Value};
use serde::{Deserialize, Serialize};
mod layout;
use layout::content_width;
mod copy;
mod selection;

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
