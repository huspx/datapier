//! 单元格详情、大字段加载与诊断上下文。
use super::*;

impl App {
    pub(super) fn open_cell(&mut self, backend: &Backend) {
        let Some(cell) = self.grid.as_ref().and_then(|grid| grid.focus) else {
            self.status = "没有选中单元格".into();
            return;
        };
        self.open_cell_at(cell, backend);
    }
    pub(super) fn open_cell_at(&mut self, cell: Cell, backend: &Backend) {
        let Some(g) = self.grid.as_ref() else {
            self.status = "没有结果".into();
            return;
        };
        if !g.row_in_page(cell.row) || g.value(cell).is_none() {
            self.status = "目标单元格已不可用".into();
            return;
        }
        self.workbench.cell_target = Some((g.page.result.id, cell));
        self.workbench.cell_fields_open = false;
        self.large = None;
        if let Some(datapier_core::model::Value::Large { id, .. }) = g.value(cell) {
            let id = *id;
            // 完整值就绪前不展示旧输入，防止旧值被写入新目标。
            if self.workbench.overlay.as_deref() == Some("cell") {
                self.workbench.overlay = None;
            }
            self.large = Some(LargeLoad {
                cell,
                id,
                offset: 0,
                bytes: Vec::new(),
            });
            self.status = "正在读取完整大字段…".into();
            self.request(
                backend,
                &format!("large.value/{id}/0"),
                "result.value",
                json!({"value_id":id,"offset":0,"max_bytes":262144}),
            );
        } else {
            match g.cell_edit_text(cell) {
                Ok(text) => {
                    self.field_set("cell", text);
                    self.workbench.overlay = Some("cell".into());
                    self.workbench.focus_request = Some("input-cell".into());
                }
                Err(e) => self.status = workbench::grid_cell_error(e, g, cell),
            }
        }
    }
    pub(super) fn large_response(&mut self, tag: &str, value: Value, backend: &Backend) {
        let Some(mut load) = self.large.take() else {
            return;
        };
        if tag != format!("large.value/{}/{}", load.id, load.offset) {
            self.large = Some(load);
            return;
        }
        let full = match load.consume(&value) {
            Ok(Some(full)) => full,
            Ok(None) => {
                let id = load.id;
                let next = load.offset;
                self.large = Some(load);
                self.request(
                    backend,
                    &format!("large.value/{id}/{next}"),
                    "result.value",
                    json!({"value_id":id,"offset":next,"max_bytes":262144}),
                );
                return;
            }
            Err(error) => {
                self.status = i18n::UiMessage::raw(error.to_string());
                return;
            }
        };
        let Some(g) = self.grid.as_mut() else {
            self.status = "结果已关闭".into();
            return;
        };
        if let Err(e) = g.resolve_large(load.cell, full) {
            self.status = workbench::grid_cell_error(e, g, load.cell);
            return;
        }
        if let Some(original) = self.workbench.redis_loading.take() {
            g.focus = Some(original);
            self.workbench_redis_edit(backend);
            return;
        }
        if g.focus != Some(load.cell)
            && self.workbench.cell_target != Some((g.page.result.id, load.cell))
        {
            self.status = "已读取完整字段".into();
            return;
        }
        match g.cell_edit_text(load.cell) {
            Ok(text) => {
                self.field_set("cell", text);
                if self.workbench.inline == Some(load.cell) {
                    self.focus = "cell".into();
                    self.workbench.focus_request = Some("cell-editor".into());
                } else {
                    self.workbench.overlay = Some("cell".into());
                    self.workbench.focus_request = Some("input-cell".into());
                }
                self.status = "已读取完整字段".into();
            }
            Err(e) => self.status = workbench::grid_cell_error(e, g, load.cell),
        }
    }
    pub(super) fn set_detail_context(&mut self, kind: &'static str) {
        let resource = self
            .target
            .as_ref()
            .map(|target| match target.database.as_deref() {
                Some(database) => format!("{database}.{}", target.name),
                None => target.name.clone(),
            });
        self.detail_context = DetailContext {
            kind,
            connection: self.profile.as_ref().map(|profile| profile.name.clone()),
            resource,
        };
    }

    pub(super) fn set_recovery_detail_context(&mut self) {
        self.set_detail_context("恢复详情");
        self.detail_context.resource = self.tabs[self.active]
            .pending
            .get("target")
            .and_then(|value| serde_json::from_value::<ResourceTarget>(value.clone()).ok())
            .map(|target| match target.database {
                Some(database) => format!("{database}.{}", target.name),
                None => target.name,
            });
    }
}
impl LargeLoad {
    pub(super) fn consume(&mut self, value: &Value) -> Result<Option<datapier_core::model::Value>> {
        use base64::Engine as _;
        let encoded = value
            .get("bytes_b64")
            .and_then(Value::as_str)
            .ok_or_else(|| AppError::invalid("缺少字段分块"))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|error| AppError::invalid(error.to_string()))?;
        if value.get("offset").and_then(Value::as_u64) != Some(self.offset) {
            return Err(AppError::invalid("大字段分块偏移不一致"));
        }
        let end = self
            .offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| AppError::invalid("大字段长度溢出"))?;
        self.bytes.extend_from_slice(&bytes);
        if let Some(next) = value.get("next").and_then(Value::as_u64) {
            if next != end || next <= self.offset {
                return Err(AppError::invalid("大字段分块不连续"));
            }
            self.offset = next;
            return Ok(None);
        }
        if value.get("total").and_then(Value::as_u64) != Some(end) {
            return Err(AppError::invalid("大字段长度不一致"));
        }
        Ok(Some(serde_json::from_slice(&self.bytes)?))
    }
}
