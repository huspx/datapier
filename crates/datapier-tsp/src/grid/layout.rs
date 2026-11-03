//! 列宽计算与布局设置。
use super::*;

impl GridState {
    pub(super) fn refresh_natural_widths(&mut self) {
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
}
pub(super) fn content_width<'a>(column: &Column, values: impl Iterator<Item = &'a Value>) -> u16 {
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
