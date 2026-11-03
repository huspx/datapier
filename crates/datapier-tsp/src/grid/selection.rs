//! 可见行与单元格/行选择、键盘导航。
use super::*;

impl GridState {
    pub(crate) fn row_in_page(&self, row: u64) -> bool {
        row >= self.page.start && row - self.page.start < self.page.rows.len() as u64
    }
    pub(crate) fn row_visible(&self, row: u64) -> bool {
        self.row_in_page(row)
            && self
                .visible_rows
                .as_ref()
                .is_none_or(|rows| rows.contains(&row))
    }

    pub(super) fn first_cell(&self) -> Option<Cell> {
        let row = if let Some(rows) = &self.visible_rows {
            rows.first().copied()?
        } else if self.page.rows.is_empty() {
            return None;
        } else {
            self.page.start
        };
        self.visible_columns().first().map(|column| Cell {
            row,
            column: *column,
        })
    }

    pub(super) fn previous_visible_row(&self, row: u64) -> Option<u64> {
        if let Some(rows) = &self.visible_rows {
            rows.range(..row).next_back().copied()
        } else {
            row.checked_sub(1).filter(|&row| self.row_in_page(row))
        }
    }

    pub(super) fn next_visible_row(&self, row: u64) -> Option<u64> {
        if let Some(rows) = &self.visible_rows {
            rows.range((std::ops::Bound::Excluded(row), std::ops::Bound::Unbounded))
                .next()
                .copied()
        } else {
            row.checked_add(1).filter(|&row| self.row_in_page(row))
        }
    }
    /// 更新筛选后的行集合，并清除已不可见的行选择和锚点。
    pub(crate) fn set_visible_rows(&mut self, rows: impl IntoIterator<Item = u64>) {
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
        match direction {
            Navigation::Up => cell.row = self.previous_visible_row(cell.row).unwrap_or(cell.row),
            Navigation::Down => cell.row = self.next_visible_row(cell.row).unwrap_or(cell.row),
            Navigation::Left => cell.column = columns[index.saturating_sub(1)],
            Navigation::Right => cell.column = columns[(index + 1).min(columns.len() - 1)],
            Navigation::Home => cell.column = columns[0],
            Navigation::End => cell.column = columns[columns.len() - 1],
            Navigation::Tab => {
                if index + 1 < columns.len() {
                    cell.column = columns[index + 1];
                } else if let Some(row) = self.next_visible_row(cell.row) {
                    cell.row = row;
                    cell.column = columns[0];
                }
            }
            Navigation::BackTab => {
                if index > 0 {
                    cell.column = columns[index - 1];
                } else if let Some(row) = self.previous_visible_row(cell.row) {
                    cell.row = row;
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
}
