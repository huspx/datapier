use super::*;
use crate::i18n::{UiArg, UiMessage, tr};
use datapier_core::model::{Column, Value as DataValue};

#[derive(Clone, Debug)]
pub(super) struct FieldError {
    pub field: String,
    pub message: UiMessage,
    pub overlay: Option<String>,
}

impl App {
    pub(super) fn field_error(&mut self, field: &str, message: UiMessage) {
        if matches!(field, "mapping" | "binary_columns" | "backslash_escape") {
            self.workbench.transfer_advanced = true;
        }
        self.workbench.focus_request =
            Some(if field == "cell" && self.workbench.inline.is_some() {
                "cell-editor".into()
            } else {
                format!("input-{field}")
            });
        self.status = message.clone();
        self.workbench.field_error = Some(FieldError {
            field: field.into(),
            message,
            overlay: self.workbench.overlay.clone(),
        });
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RowMode {
    Default,
    Value,
    Null,
}

#[derive(Clone)]
struct RowField {
    column: Column,
    editor: Editor,
    mode: RowMode,
}

#[derive(Clone, Default)]
pub(super) struct SqlForms {
    row: Vec<RowField>,
    row_advanced: bool,
    pub(super) row_json: Editor,
    columns: Vec<Column>,
    filter_column: Option<usize>,
    filter_op: usize,
    filter_value: Editor,
    sort_column: Option<usize>,
    sort_desc: bool,
    filter_advanced: bool,
    pub(super) open_select: Option<&'static str>,
}

const OPERATORS: [(&str, &str); 10] = [
    ("eq", "等于"),
    ("ne", "不等于"),
    ("lt", "小于"),
    ("lte", "小于等于"),
    ("gt", "大于"),
    ("gte", "大于等于"),
    ("like", "LIKE"),
    ("not_like", "NOT LIKE"),
    ("is_null", "是 NULL"),
    ("is_not_null", "不是 NULL"),
];

// 只处理本地写入预检标记；数据库原始诊断必须原样保留。
fn mysql_validation_reason(column: &Column, error: &AppError) -> Option<UiMessage> {
    if error.code != "invalid_input" || error.database_code.is_some() {
        return None;
    }
    let kind = error.details.get("validation")?.as_str()?;
    let template = match kind {
        "character_length" => "列 {name} 超过字符长度 {limit}",
        "numeric_range" => "列 {name} ({type})：数值无效、超出范围或无法按目标精度和标度无损表示",
        "type_mismatch" => "列 {name} ({type}): 值类型不受支持或不匹配",
        "bit_width" => "列 {name} ({type}) 的二进制值超出位宽或类型无效",
        "generated_column" => "生成列不可写: {name}",
        "null_not_allowed" => "列 {name} 不允许 NULL",
        "invalid_base64" => return Some(UiMessage::key("无效的 Base64 二进制值")),
        _ => return None,
    };
    let mut args = vec![
        ("name", UiArg::Raw(column.name.clone())),
        ("type", UiArg::Raw(column.type_name.clone())),
    ];
    if kind == "character_length" {
        args.push((
            "limit",
            UiArg::Raw(error.details.get("limit")?.as_u64()?.to_string()),
        ));
    }
    Some(UiMessage::template(template, args))
}

fn form_validation_error(template: &'static str, column: &Column, error: AppError) -> UiMessage {
    let reason = if error.database_code.is_some() {
        UiMessage::raw(error.message)
    } else if let Some(reason) = mysql_validation_reason(column, &error) {
        reason
    } else if error.message == format!("列 {} 不允许 NULL", column.name) {
        UiMessage::template(
            "列 {name} 不允许 NULL",
            vec![("name", UiArg::Raw(column.name.clone()))],
        )
    } else if matches!(
        error.message.as_str(),
        "非 NULL 的反斜线必须成对出现"
            | "布尔值必须为 true、false、1 或 0"
            | "不存在的值或未加载的大值不可编辑"
            | "无效布尔值"
            | "无效整数"
            | "无效无符号整数"
            | "无效精确小数"
            | "无效浮点数"
            | "二进制值必须是 Base64"
    ) {
        UiMessage::local(error.message)
    } else {
        UiMessage::raw(error.message)
    };
    let message = UiMessage::template(
        template,
        vec![
            ("name", UiArg::Raw(column.name.clone())),
            ("error", UiArg::Message(Box::new(reason))),
        ],
    );
    UiMessage::template(
        "invalid_input: {error}",
        vec![("error", UiArg::Message(Box::new(message)))],
    )
}

impl SqlForms {
    fn row_values(&self) -> std::result::Result<BTreeMap<String, DataValue>, UiMessage> {
        if self.row.is_empty() {
            return Err(UiMessage::template(
                "invalid_input: {error}",
                vec![("error", UiArg::Key("没有可用的表结构"))],
            ));
        }
        let mut values = BTreeMap::new();
        for field in &self.row {
            if field.column.generated || field.mode == RowMode::Default {
                continue;
            }
            let value = grid::decode_form_value(
                &field.editor.text,
                field.mode == RowMode::Null,
                &field.column,
            )
            .map_err(|error| form_validation_error("列 {name}: {error}", &field.column, error))?;
            values.insert(field.column.name.clone(), value);
        }
        Ok(values)
    }

    fn filter_values(&self) -> std::result::Result<(Value, Value), UiMessage> {
        if self.columns.is_empty() {
            return Err(UiMessage::template(
                "invalid_input: {error}",
                vec![("error", UiArg::Key("没有可用的表结构"))],
            ));
        }
        let mut filters = Vec::new();
        if let Some(index) = self.filter_column {
            let column = &self.columns[index];
            let mut filter = json!({"column":column.name,"op":OPERATORS[self.filter_op].0});
            if self.filter_op < 8 {
                let value = if matches!(OPERATORS[self.filter_op].0, "like" | "not_like") {
                    DataValue::Text(self.filter_value.text.clone())
                } else {
                    grid::decode_form_value(&self.filter_value.text, false, column).map_err(
                        |error| form_validation_error("过滤值 {name}: {error}", column, error),
                    )?
                };
                filter["value"] = json!(value);
            }
            filters.push(filter);
        }
        let sort = self.sort_column.map(|index| json!({"column":self.columns[index].name,"direction":if self.sort_desc {"desc"} else {"asc"}})).into_iter().collect::<Vec<_>>();
        Ok((json!(filters), json!(sort)))
    }
}

impl Workbench {
    pub(super) fn form_editor(&mut self, name: &str) -> Option<&mut Editor> {
        if name == "sql-form-filter-value" {
            return Some(&mut self.sql_forms.filter_value);
        }
        if name == "row_values" {
            return Some(&mut self.sql_forms.row_json);
        }
        let index = name.strip_prefix("sql-form-row-")?.parse::<usize>().ok()?;
        self.sql_forms
            .row
            .get_mut(index)
            .map(|field| &mut field.editor)
    }
    pub(super) fn close_form_select(&mut self) -> bool {
        self.sql_forms.open_select.take().is_some()
    }
    pub(super) fn form_changed(&mut self, name: &str) {
        if let Some(index) = name
            .strip_prefix("sql-form-row-")
            .and_then(|s| s.parse::<usize>().ok())
            && let Some(field) = self.sql_forms.row.get_mut(index)
        {
            field.mode = RowMode::Value;
        }
    }
}

fn columns(app: &App) -> Vec<Column> {
    let mut columns: Vec<Column> = app
        .schema
        .as_ref()
        .and_then(|schema| schema.get("columns"))
        .and_then(|columns| serde_json::from_value(columns.clone()).ok())
        .unwrap_or_default();
    if app
        .target
        .as_ref()
        .is_some_and(|target| target.kind == "view")
    {
        let writable = app
            .schema
            .as_ref()
            .and_then(|schema| schema.pointer("/details/view_mapping/writable"))
            .and_then(Value::as_array);
        for column in &mut columns {
            if !writable.is_some_and(|items| {
                items
                    .iter()
                    .any(|item| item.as_str() == Some(column.name.as_str()))
            }) {
                column.generated = true;
            }
        }
    }
    columns
}

fn value_text(value: &DataValue) -> Option<String> {
    Some(match value {
        DataValue::Bool(v) => v.to_string(),
        DataValue::Integer(v)
        | DataValue::Unsigned(v)
        | DataValue::Decimal(v)
        | DataValue::Float(v)
        | DataValue::Text(v)
        | DataValue::Bytes(v)
        | DataValue::Date(v)
        | DataValue::Time(v)
        | DataValue::DateTime(v) => v.clone(),
        DataValue::Json(v) => v.to_string(),
        _ => return None,
    })
}

fn required_column(column: &Column) -> bool {
    !column.generated
        && !column.nullable
        && !column.auto_increment
        && column.default_value.is_none()
}

fn validate_row(
    values: &BTreeMap<String, DataValue>,
    columns: &[Column],
) -> std::result::Result<(), UiMessage> {
    if columns.is_empty() {
        return Err(UiMessage::key("没有可用的表结构"));
    }
    for name in values.keys() {
        if !columns
            .iter()
            .any(|column| column.name == *name && !column.generated)
        {
            return Err(UiMessage::key("高级 JSON 包含未知字段或生成列，请先修正"));
        }
    }
    for column in columns {
        let Some(value) = values.get(&column.name) else {
            if required_column(column) {
                return Err(UiMessage::template(
                    "列 {name} 必须提供值",
                    vec![("name", UiArg::Raw(column.name.clone()))],
                ));
            }
            continue;
        };
        if matches!(value, DataValue::Null) {
            if !column.nullable {
                return Err(UiMessage::template(
                    "列 {name} 不允许 NULL",
                    vec![("name", UiArg::Raw(column.name.clone()))],
                ));
            }
            continue;
        }
        datapier_core::providers::mysql::validate_import_value(value, column)
            .map_err(|error| form_validation_error("列 {name}: {error}", column, error))?;
    }
    Ok(())
}

impl App {
    pub(super) fn validate_insert_values(
        &mut self,
        values: &BTreeMap<String, DataValue>,
        advanced: bool,
    ) -> bool {
        let columns = columns(self);
        if let Err(error) = validate_row(values, &columns) {
            let field = if advanced {
                "row_values".to_owned()
            } else {
                columns
                    .iter()
                    .enumerate()
                    .find(|(_, column)| {
                        required_column(column) && !values.contains_key(&column.name)
                    })
                    .map(|(index, _)| format!("sql-form-row-{index}"))
                    .unwrap_or_else(|| "row_values".into())
            };
            self.field_error(&field, error);
            return false;
        }
        true
    }
}

impl App {
    pub(super) fn init_sql_form(&mut self, overlay: &str) {
        self.workbench.sql_forms.open_select = None;
        let cols = columns(self);
        if overlay == "row" {
            self.workbench.sql_forms.row = cols
                .into_iter()
                .map(|column| RowField {
                    column,
                    editor: Editor::default(),
                    mode: RowMode::Default,
                })
                .collect();
            self.workbench.sql_forms.row_advanced = false;
            self.field_set("row_values", "{}");
            return;
        }
        let filter: Option<Vec<Value>> = serde_json::from_str(self.field("filter")).ok();
        let sort: Option<Vec<Value>> = serde_json::from_str(self.field("sort")).ok();
        let mut state = SqlForms {
            columns: cols,
            ..SqlForms::default()
        };
        let parsed = (|| -> Option<()> {
            let filters = filter.as_ref()?;
            let sorts = sort.as_ref()?;
            if filters.len() > 1 || sorts.len() > 1 {
                return None;
            }
            if let Some(f) = filters.first() {
                let name = f.get("column")?.as_str()?;
                let index = state.columns.iter().position(|c| c.name == name)?;
                let op = f.get("op").and_then(Value::as_str).unwrap_or("eq");
                state.filter_op = OPERATORS.iter().position(|(key, _)| *key == op)?;
                state.filter_column = Some(index);
                if state.filter_op < 8 {
                    let value: DataValue = serde_json::from_value(f.get("value")?.clone()).ok()?;
                    state.filter_value = Editor::new(value_text(&value)?);
                }
            }
            if let Some(s) = sorts.first() {
                let name = s.get("column")?.as_str()?;
                state.sort_column = Some(state.columns.iter().position(|c| c.name == name)?);
                state.sort_desc = match s.get("direction").and_then(Value::as_str).unwrap_or("asc")
                {
                    "asc" => false,
                    "desc" => true,
                    _ => return None,
                };
            }
            Some(())
        })()
        .is_some();
        state.filter_advanced = !parsed;
        self.workbench.sql_forms.columns = state.columns;
        self.workbench.sql_forms.filter_column = state.filter_column;
        self.workbench.sql_forms.filter_op = state.filter_op;
        self.workbench.sql_forms.filter_value = state.filter_value;
        self.workbench.sql_forms.sort_column = state.sort_column;
        self.workbench.sql_forms.sort_desc = state.sort_desc;
        self.workbench.sql_forms.filter_advanced = state.filter_advanced;
    }

    pub(super) fn sql_form_action(&mut self, name: &str, backend: &Backend) -> bool {
        if let Some(rest) = name.strip_prefix("sql-select-open:") {
            if let Some((kind, open)) = rest.split_once(':') {
                let kind = match kind {
                    "filter-column" => Some("filter-column"),
                    "sort-column" => Some("sort-column"),
                    "filter-op" => Some("filter-op"),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let state = &mut self.workbench.sql_forms;
                    if open == "true" {
                        state.open_select = Some(kind);
                    } else if open == "false" && state.open_select == Some(kind) {
                        state.open_select = None;
                    }
                }
            }
            return true;
        }
        if let Some(index) = name
            .strip_prefix("sql-row-mode:")
            .and_then(|s| s.parse::<usize>().ok())
        {
            if let Some(field) = self.workbench.sql_forms.row.get_mut(index)
                && !field.column.generated
            {
                field.mode = match field.mode {
                    RowMode::Default => RowMode::Value,
                    RowMode::Value if field.column.nullable => RowMode::Null,
                    _ => RowMode::Default,
                };
            }
            if self
                .workbench
                .field_error
                .as_ref()
                .is_some_and(|error| error.field == format!("sql-form-row-{index}"))
            {
                self.workbench.field_error = None;
            }
            return true;
        }
        for (prefix, sort) in [("sql-filter-select:", false), ("sql-sort-select:", true)] {
            if let Some(value) = name.strip_prefix(prefix) {
                let state = &mut self.workbench.sql_forms;
                let index = value
                    .parse::<usize>()
                    .ok()
                    .filter(|&index| index < state.columns.len());
                if sort {
                    state.sort_column = index;
                } else {
                    state.filter_column = index;
                }
                let kind = if sort { "sort-column" } else { "filter-column" };
                if state.open_select == Some(kind) {
                    state.open_select = None;
                }
                return true;
            }
        }
        if let Some(index) = name
            .strip_prefix("sql-op-select:")
            .and_then(|value| value.parse::<usize>().ok())
        {
            if index < OPERATORS.len() {
                self.workbench.sql_forms.filter_op = index;
                if self.workbench.sql_forms.open_select == Some("filter-op") {
                    self.workbench.sql_forms.open_select = None;
                }
            }
            return true;
        }
        match name {
            "sql-row-advanced" => {
                if !self.workbench.sql_forms.row_advanced {
                    match self.workbench.sql_forms.row_values() {
                        Ok(values) => self.field_set("row_values", json!(values).to_string()),
                        Err(error) => {
                            self.status = error;
                            return true;
                        }
                    }
                    self.workbench.sql_forms.row_advanced = true;
                } else {
                    let values = match serde_json::from_str::<BTreeMap<String, DataValue>>(
                        self.field("row_values"),
                    ) {
                        Ok(values) => values,
                        Err(error) => {
                            self.status = UiMessage::template(
                                "行值无效: {error}",
                                vec![("error", UiArg::Raw(error.to_string()))],
                            );
                            return true;
                        }
                    };
                    if values.keys().any(|name| {
                        !self
                            .workbench
                            .sql_forms
                            .row
                            .iter()
                            .any(|field| &field.column.name == name && !field.column.generated)
                    }) {
                        self.status = UiMessage::key("高级 JSON 包含未知字段或生成列，请先修正");
                        return true;
                    }
                    let mut fields = self.workbench.sql_forms.row.clone();
                    for field in &mut fields {
                        match values.get(&field.column.name) {
                            None => {
                                field.mode = RowMode::Default;
                                field.editor = Editor::default();
                            }
                            Some(DataValue::Null) if field.column.nullable => {
                                field.mode = RowMode::Null;
                                field.editor = Editor::default();
                            }
                            Some(value) => match value_text(value) {
                                Some(text) => {
                                    field.mode = RowMode::Value;
                                    field.editor = Editor::new(text);
                                }
                                None => {
                                    self.status =
                                        UiMessage::key("该值无法转为普通表单，请使用高级 JSON");
                                    return true;
                                }
                            },
                        }
                    }
                    self.workbench.sql_forms.row = fields;
                    self.workbench.sql_forms.row_advanced = false;
                }
            }
            "sql-filter-advanced" => {
                if self.workbench.sql_forms.filter_advanced {
                    self.init_sql_form("filters");
                    if self.workbench.sql_forms.filter_advanced {
                        self.status = UiMessage::key("当前多条件或特殊条件需使用高级 JSON");
                    }
                } else {
                    match self.workbench.sql_forms.filter_values() {
                        Ok((filter, sort)) => {
                            self.field_set("filter", filter.to_string());
                            self.field_set("sort", sort.to_string());
                        }
                        Err(error) => {
                            self.status = error;
                            return true;
                        }
                    }
                    self.workbench.sql_forms.filter_advanced = true;
                }
            }
            "sql-sort-direction" => {
                self.workbench.sql_forms.sort_desc = !self.workbench.sql_forms.sort_desc
            }
            "sql-row-add" => {
                let values = match self.workbench.sql_forms.row_values() {
                    Ok(values) => values,
                    Err(error) => {
                        let index = self
                            .workbench
                            .sql_forms
                            .row
                            .iter()
                            .position(|field| {
                                !field.column.generated
                                    && field.mode != RowMode::Default
                                    && grid::decode_form_value(
                                        &field.editor.text,
                                        field.mode == RowMode::Null,
                                        &field.column,
                                    )
                                    .is_err()
                            })
                            .unwrap_or(0);
                        self.field_error(&format!("sql-form-row-{index}"), error);
                        return true;
                    }
                };
                if !self.validate_insert_values(&values, false) {
                    return true;
                }
                self.field_set("row_values", json!(values).to_string());
                let count = self.pending_rows.len();
                self.action("insert-row", backend);
                if self.pending_rows.len() > count {
                    self.workbench.overlay = None;
                    self.status = UiMessage::key("新增行已加入草稿");
                }
            }
            "sql-filter-apply" => {
                if self.has_live_drafts() {
                    self.status = UiMessage::key("请先提交或丢弃草稿，再更改过滤与排序");
                    return true;
                }
                let (filter, sort) = match self.workbench.sql_forms.filter_values() {
                    Ok(values) => values,
                    Err(error) => {
                        self.field_error("sql-form-filter-value", error);
                        return true;
                    }
                };
                self.field_set("filter", filter.to_string());
                self.field_set("sort", sort.to_string());
                self.workbench.overlay = None;
                self.action("browse", backend);
            }
            "sql-filter-apply-advanced" => {
                if self.has_live_drafts() {
                    self.status = UiMessage::key("请先提交或丢弃草稿，再更改过滤与排序");
                    return true;
                }
                let columns = &self.workbench.sql_forms.columns;
                for field in ["filter", "sort"] {
                    let valid = serde_json::from_str::<Vec<Value>>(self.field(field))
                        .ok()
                        .is_some_and(|items| {
                            let mut seen = std::collections::HashSet::new();
                            items.iter().all(|item| {
                                let Some(column) = item.get("column").and_then(Value::as_str)
                                else {
                                    return false;
                                };
                                if !columns.iter().any(|c| c.name == column) {
                                    return false;
                                }
                                if field == "sort" {
                                    return seen.insert(column)
                                        && item.get("direction").is_none_or(|v| {
                                            v.as_str().is_some_and(|s| {
                                                s.eq_ignore_ascii_case("asc")
                                                    || s.eq_ignore_ascii_case("desc")
                                            })
                                        });
                                }
                                let op = item
                                    .get("op")
                                    .map_or(Some("eq"), Value::as_str)
                                    .unwrap_or("")
                                    .to_ascii_lowercase();
                                if !matches!(
                                    op.as_str(),
                                    "eq" | "="
                                        | "ne"
                                        | "!="
                                        | "<>"
                                        | "lt"
                                        | "<"
                                        | "lte"
                                        | "<="
                                        | "gt"
                                        | ">"
                                        | "gte"
                                        | ">="
                                        | "like"
                                        | "not_like"
                                        | "is_null"
                                        | "is_not_null"
                                ) {
                                    return false;
                                }
                                if matches!(op.as_str(), "is_null" | "is_not_null") {
                                    return true;
                                }
                                item.get("value").is_some_and(|value| {
                                    !value.is_null()
                                        && value.get("type").and_then(Value::as_str) != Some("null")
                                        && (value.is_string()
                                            || value.is_number()
                                            || value.is_boolean()
                                            || serde_json::from_value::<DataValue>(value.clone())
                                                .is_ok())
                                })
                            })
                        });
                    if !valid {
                        self.field_error(field, UiMessage::key("过滤或排序条件无效"));
                        return true;
                    }
                }
                self.workbench.overlay = None;
                self.action("browse", backend);
            }
            _ => return false,
        }
        true
    }
}

fn button(label: &str, action: &str) -> Node<Msg> {
    ui::button(label.to_owned())
        .variant(
            if matches!(
                action,
                "sql-row-add" | "sql-filter-apply" | "sql-filter-apply-advanced" | "insert-row"
            ) {
                ui::ButtonVariant::Primary
            } else {
                ui::ButtonVariant::Default
            },
        )
        .key(action.to_owned())
        .on_click(Msg::Action(action.into()))
        .into()
}

fn input(app: &App, name: &str, label: &str, editor: &Editor, required: bool) -> Node<Msg> {
    let focus = name.to_owned();
    let undo = focus.clone();
    let edit = focus.clone();
    let accessible_label = if required {
        crate::i18n::format_message("{label}（必填）", &[("label", label)])
    } else {
        label.to_owned()
    };
    let control: Node<Msg> = ui::field(label.to_owned())
        .required(required)
        .key(name.to_owned())
        .child(
            ui::input()
                .aria(accessible_label)
                .variant(ui::InputVariant::Field)
                .key(format!("input-{name}"))
                .text(editor.text.clone())
                .cursor(editor.cursor)
                .anchor(editor.anchor)
                .on_focus(Msg::Focus(focus))
                .on_undo(Msg::Undo(undo))
                .on_edit(move |e| {
                    Msg::Edit(edit.clone(), e.from, e.to, e.text.clone(), e.cursor, e.len)
                }),
        )
        .into();
    if let Some(error) = app
        .workbench
        .field_error
        .as_ref()
        .filter(|error| error.field == name)
    {
        html::div()
            .child(control)
            .child(html::span(error.message.render().into_owned()).class("field-error"))
            .into()
    } else {
        control
    }
}

fn advanced_input(app: &App, name: &str, label: &str) -> Node<Msg> {
    let editor = if name == "row_values" {
        &app.workbench.sql_forms.row_json
    } else {
        &app.fields[name]
    };
    input(app, name, label, editor, true)
}

/// 正文与主要动作分开返回，长表单滚动时仍能在固定页脚加入草稿。
pub(crate) fn row_form(app: &App) -> (Node<Msg>, Node<Msg>) {
    let state = &app.workbench.sql_forms;
    let mut panel = html::div().class("sql-row-form");
    if state.row_advanced {
        panel = panel.child(advanced_input(
            app,
            "row_values",
            tr("类型化行 JSON").as_ref(),
        ));
    } else {
        if state.row.is_empty() {
            panel = panel.child(html::span(tr("没有可用的表结构").into_owned()).class("muted"));
        }
        let mut fields = html::div().class("sql-row-fields");
        for (index, field) in state.row.iter().enumerate() {
            let mode = match field.mode {
                RowMode::Default => "默认",
                RowMode::Value => "输入值",
                RowMode::Null => "NULL",
            };
            let label = format!("{} · {}", field.column.name, field.column.type_name);
            let mut item = html::div().class("form-field form-row-field");
            if field.column.generated {
                item = item
                    .child(html::span(label).class("label"))
                    .child(html::span(tr("由数据库生成").into_owned()).class("muted"));
            } else {
                item = item
                    .child(input(
                        app,
                        &format!("sql-form-row-{index}"),
                        &label,
                        &field.editor,
                        required_column(&field.column),
                    ))
                    .child(button(tr(mode).as_ref(), &format!("sql-row-mode:{index}")));
            }
            fields = fields.child(item);
        }
        panel = panel.child(fields)
            .child(html::span(tr("默认字段由数据库填充；输入值可为空字符串。点击字段模式可切换默认值、输入值和 NULL。").into_owned()).class("notice"));
    }
    let body = panel
        .child(
            html::div()
                .class("form-actions")
                .child(button(
                    tr(if state.row_advanced {
                        "普通表单"
                    } else {
                        "高级 JSON"
                    })
                    .as_ref(),
                    "sql-row-advanced",
                ))
                .child(button(tr("删除选中行").as_ref(), "delete-rows-request"))
                .child(button(tr("撤销行草稿").as_ref(), "undo-row")),
        )
        .into();
    let action = button(
        tr("新增行草稿").as_ref(),
        if state.row_advanced {
            "insert-row"
        } else {
            "sql-row-add"
        },
    );
    (body, action)
}

/// 将应用动作交给弹窗页脚，避免高级 JSON 把提交入口挤出可见区。
pub(crate) fn filter_form(app: &App) -> (Node<Msg>, Node<Msg>) {
    let state = &app.workbench.sql_forms;
    let mut panel = html::div().class("sql-filter-form");
    if state.filter_advanced {
        panel = panel
            .child(advanced_input(
                app,
                "filter",
                tr("过滤 JSON（支持多条件）").as_ref(),
            ))
            .child(advanced_input(app, "sort", tr("排序 JSON").as_ref()));
    } else {
        let mut row = html::div()
            .class("filter-fields")
            .child(column_select(state, false));
        if state.filter_column.is_some() {
            let mut condition = ui::select()
                .value(state.filter_op.to_string())
                .open(state.open_select == Some("filter-op"))
                .on_toggle(|collapsed| {
                    Msg::Action(format!("sql-select-open:filter-op:{}", !collapsed))
                });
            for (index, (_, label)) in OPERATORS.iter().enumerate() {
                condition = condition.option(ui::SelectOption::new(
                    index.to_string(),
                    tr(label).into_owned(),
                ));
            }
            row = row.child(
                ui::field(tr("条件").into_owned()).child(condition.on_change(|change| {
                    Msg::Action(format!(
                        "sql-op-select:{}",
                        change.value.as_str().unwrap_or_default()
                    ))
                })),
            );
            if state.filter_op < 8 {
                row = row.child(input(
                    app,
                    "sql-form-filter-value",
                    tr("值").as_ref(),
                    &state.filter_value,
                    true,
                ));
            }
        }
        let mut sort = html::div()
            .class("filter-fields")
            .child(column_select(state, true));
        if state.sort_column.is_some() {
            sort = sort.child(button(
                tr(if state.sort_desc {
                    "降序 ↓"
                } else {
                    "升序 ↑"
                })
                .as_ref(),
                "sql-sort-direction",
            ));
        }
        panel = panel.child(row).child(sort);
    }
    let body = panel
        .child(button(
            tr(if state.filter_advanced {
                "普通表单"
            } else {
                "高级 JSON"
            })
            .as_ref(),
            "sql-filter-advanced",
        ))
        .into();
    let action = button(
        tr("应用过滤和排序").as_ref(),
        if state.filter_advanced {
            "sql-filter-apply-advanced"
        } else {
            "sql-filter-apply"
        },
    );
    (body, action)
}

fn column_select(state: &SqlForms, sort: bool) -> Node<Msg> {
    let selected = if sort {
        state.sort_column
    } else {
        state.filter_column
    };
    let empty = tr(if sort { "不排序" } else { "不筛选" });
    let prefix = if sort {
        "sql-sort-select:"
    } else {
        "sql-filter-select:"
    };
    let kind = if sort { "sort-column" } else { "filter-column" };
    let mut select = ui::select()
        .value(
            selected
                .map(|index| index.to_string())
                .unwrap_or_else(|| "none".into()),
        )
        .open(state.open_select == Some(kind))
        .on_toggle(move |collapsed| Msg::Action(format!("sql-select-open:{kind}:{}", !collapsed)))
        .option(ui::SelectOption::new("none", empty.into_owned()));
    for (index, column) in state.columns.iter().enumerate() {
        select = select.option(ui::SelectOption::new(
            index.to_string(),
            column.name.clone(),
        ));
    }
    ui::field(tr(if sort { "排序字段" } else { "筛选字段" }).into_owned())
        .child(select.on_change(move |change| {
            Msg::Action(format!(
                "{}{}",
                prefix,
                change.value.as_str().unwrap_or_default()
            ))
        }))
        .into()
}

#[cfg(test)]
#[path = "../tests/unit/forms.rs"]
mod tests;
