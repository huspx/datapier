use crate::i18n;
use crate::{App, backend::Backend, grid::Cell};

#[derive(Debug, Clone, Copy)]
pub(super) enum GridMenuAction {
    CopyCells,
    CopyRedisRows,
    CopyRows,
    CopyInsert,
    CopyInsertWithoutAutoPrimary,
    DeleteRows,
}

impl App {
    pub(super) fn redis_composite_grid(&self) -> bool {
        self.is_redis()
            && self
                .schema
                .as_ref()
                .and_then(|schema| schema.get("type"))
                .and_then(|kind| kind.as_str())
                .is_some_and(|kind| matches!(kind, "hash" | "list" | "set" | "zset"))
    }

    /// 单元格复制保留矩形选区；右击选区外或行选区时只选目标单元格。
    pub(super) fn prepare_grid_cell_copy(&mut self, cell: Cell) -> bool {
        if !self.finish_inline(true) {
            return false;
        }
        let Some(grid) = self.grid.as_mut() else {
            self.status = "没有结果".into();
            return false;
        };
        if !grid.row_in_page(cell.row)
            || grid
                .settings
                .get(cell.column)
                .is_none_or(|setting| setting.hidden)
            || grid.page.result.columns.get(cell.column).is_none()
            || grid.value(cell).is_none()
        {
            self.status = "目标单元格已不可用".into();
            return false;
        }
        if (grid.selected_row_count() != 0 || !grid.is_selected(cell))
            && let Err(error) = grid.select(cell, false)
        {
            self.status = i18n::UiMessage::local(error.message);
            return false;
        }
        self.focus = "grid".into();
        self.workbench.focus_request = Some(String::new());
        true
    }

    /// 右击已选行保留选区，右击其他行先选择目标。
    pub(super) fn prepare_redis_grid_menu(&mut self, cell: Cell) -> bool {
        if !self.finish_inline(true) {
            return false;
        }
        let Some(grid) = self.grid.as_mut().filter(|grid| grid.row_in_page(cell.row)) else {
            self.status = "目标单元格已不可用".into();
            return false;
        };
        if !grid.is_row_selected(cell.row)
            && let Err(error) = grid.select_row(cell.row, cell.column, false, false)
        {
            self.status = i18n::UiMessage::local(error.message);
            return false;
        }
        self.focus = "grid".into();
        self.workbench.focus_request = Some(String::new());
        true
    }

    /// 仅对来源明确、列完整匹配 schema 的 MySQL 基表提供 INSERT。
    pub(super) fn can_copy_insert(&self) -> bool {
        if !self
            .profile
            .as_ref()
            .is_some_and(|profile| profile.provider == datapier_core::model::ProviderKind::Mysql)
            || !self.browse_result
        {
            return false;
        }
        let Some(target) = self.target.as_ref().filter(|target| target.kind == "table") else {
            return false;
        };
        let Some(database) = target.database.as_ref().or(self.database.as_ref()) else {
            return false;
        };
        let Some(columns) = self
            .schema
            .as_ref()
            .and_then(|s| s.get("columns"))
            .and_then(|c| c.as_array())
        else {
            return false;
        };
        let Some(grid) = self.grid.as_ref() else {
            return false;
        };
        !database.is_empty()
            && !target.name.is_empty()
            && !columns.is_empty()
            && columns.len() == grid.page.result.columns.len()
            && columns
                .iter()
                .zip(&grid.page.result.columns)
                .all(|(a, b)| a.get("name").and_then(|name| name.as_str()) == Some(&b.name))
    }

    fn insert_context(&self) -> Option<(String, String, Vec<datapier_core::model::Column>)> {
        if !self.can_copy_insert() {
            return None;
        }
        let target = self.target.as_ref()?;
        let database = target.database.as_ref().or(self.database.as_ref())?.clone();
        let columns = serde_json::from_value(self.schema.as_ref()?.get("columns")?.clone()).ok()?;
        Some((database, target.name.clone(), columns))
    }

    /// 菜单目标已在行选区时保留该选区，否则目标为单行。
    pub(super) fn grid_menu_row_count(&self, cell: Cell) -> usize {
        self.grid
            .as_ref()
            .filter(|grid| grid.row_in_page(cell.row))
            .map_or(0, |grid| {
                if grid.is_row_selected(cell.row) {
                    grid.selected_row_count()
                } else {
                    1
                }
            })
    }

    /// 对当前结果页中的可见单元格执行菜单操作；复制成功时返回完整 TSV。
    pub(super) fn grid_menu(
        &mut self,
        cell: Cell,
        action: GridMenuAction,
        _backend: &Backend,
    ) -> Option<String> {
        if matches!(action, GridMenuAction::DeleteRows) && self.workbench.inline.is_some() {
            self.status = "请先完成或取消单元格编辑及粘贴预览".into();
            return None;
        }
        if !self.finish_inline(true) {
            return None;
        }
        let insert_context = if matches!(
            action,
            GridMenuAction::CopyInsert | GridMenuAction::CopyInsertWithoutAutoPrimary
        ) {
            match self.insert_context() {
                Some(context) => Some(context),
                None => {
                    self.status = "当前结果没有可用的 MySQL 基表 schema".into();
                    return None;
                }
            }
        } else {
            None
        };
        let redis_delete =
            self.redis_composite_grid() && matches!(action, GridMenuAction::DeleteRows);
        let Some(grid) = self.grid.as_mut() else {
            self.status = "没有结果".into();
            return None;
        };
        let row_action = matches!(
            action,
            GridMenuAction::CopyRows
                | GridMenuAction::CopyInsert
                | GridMenuAction::CopyInsertWithoutAutoPrimary
                | GridMenuAction::DeleteRows
        );
        // 行操作只依赖当前页的行身份；单元格操作仍需有效的可见列。
        if !grid.row_in_page(cell.row)
            || (!row_action
                && (grid
                    .settings
                    .get(cell.column)
                    .is_none_or(|setting| setting.hidden)
                    || grid.page.result.columns.get(cell.column).is_none()
                    || grid.page.rows[(cell.row - grid.page.start) as usize]
                        .get(cell.column)
                        .is_none()))
        {
            self.status = "目标单元格已不可用".into();
            return None;
        }
        let selection = if row_action {
            if grid.is_row_selected(cell.row) {
                Ok(())
            } else {
                grid.select_row(cell.row, cell.column, false, false)
            }
        } else if !matches!(action, GridMenuAction::CopyCells)
            || grid.selected_row_count() != 0
            || !grid.is_selected(cell)
        {
            grid.select(cell, false)
        } else {
            Ok(())
        };
        if let Err(error) = selection {
            self.status = i18n::UiMessage::local(error.message);
            return None;
        }
        self.focus = "grid".into();
        self.workbench.focus_request = Some(String::new());
        match action {
            GridMenuAction::CopyRedisRows => None,
            GridMenuAction::CopyCells | GridMenuAction::CopyRows => match grid.copy_tsv() {
                Ok(text) => Some(text),
                Err(error) => {
                    self.status = i18n::UiMessage::local(error.message);
                    None
                }
            },
            GridMenuAction::CopyInsert | GridMenuAction::CopyInsertWithoutAutoPrimary => {
                let (database, table, columns) = insert_context.expect("已校验基表上下文");
                match grid.copy_insert(
                    &database,
                    &table,
                    &columns,
                    matches!(action, GridMenuAction::CopyInsertWithoutAutoPrimary),
                ) {
                    Ok(text) => Some(text),
                    Err(error) => {
                        self.status = i18n::UiMessage::local(error.message);
                        None
                    }
                }
            }
            GridMenuAction::DeleteRows => {
                if redis_delete {
                    self.request_redis_row_delete();
                } else {
                    self.request_delete_rows();
                }
                None
            }
        }
    }
}

#[cfg(test)]
#[path = "../tests/unit/menus.rs"]
mod tests;
