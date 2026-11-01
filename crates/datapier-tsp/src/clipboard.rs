//! 剪贴板写入等待宿主确认；SQL 剪切还须确认原编辑上下文未改变。
use super::*;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use std::ops::Range;
use tern_sdk::Session;

struct PendingCut {
    id: Uuid,
    tab: Uuid,
    range: Range<usize>,
    anchor: usize,
    cursor: usize,
    // 等待授权时用户仍可编辑。完整快照避免重复 SQL 导致旧范围误删新文本。
    original: String,
    started: Instant,
}
struct PendingRedisCopy {
    tag: String,
    tab: Uuid,
    generation: u64,
    target: ResourceTarget,
    result_id: Uuid,
    changes_revision: u64,
    profile: Option<Uuid>,
    session: Option<Value>,
    selected: Option<Vec<(u64, Vec<datapier_core::model::Value>)>>,
}

impl PendingRedisCopy {
    fn matches(&self, app: &App) -> bool {
        app.is_redis()
            && app.browse_result
            && app
                .tabs
                .get(app.active)
                .is_some_and(|tab| tab.id == self.tab)
            && app.generation == self.generation
            && app.target.as_ref().is_some_and(|target| {
                target.name == self.target.name
                    && target.kind == self.target.kind
                    && target.database == self.target.database
                    && target.key_b64 == self.target.key_b64
            })
            && app
                .grid
                .as_ref()
                .is_some_and(|grid| grid.page.result.id == self.result_id)
            && app.profile.as_ref().map(|p| p.id) == self.profile
            && app.session == self.session
            && self.selected.as_ref().is_none_or(|rows| {
                app.grid.as_ref().is_some_and(|grid| {
                    rows.iter()
                        .all(|(index, row)| grid.original_row(*index) == Some(row.as_slice()))
                })
            })
            && app.redis_changes_revision == self.changes_revision
    }
}

struct PendingGridCopy {
    tag: String,
    tab: Uuid,
    generation: u64,
    result: Uuid,
    start: u64,
    cells: Vec<Vec<Cell>>,
    values: Vec<Vec<datapier_core::model::Value>>,
}
impl PendingGridCopy {
    fn matches(&self, app: &App) -> bool {
        app.tabs
            .get(app.active)
            .is_some_and(|tab| tab.id == self.tab)
            && app.generation == self.generation
            && app.grid.as_ref().is_some_and(|grid| {
                grid.page.result.id == self.result
                    && grid.page.start == self.start
                    && grid.selected_cells().is_ok_and(|cells| cells == self.cells)
                    && self.cells.iter().zip(&self.values).all(|(cells, values)| {
                        cells
                            .iter()
                            .zip(values)
                            .all(|(&cell, value)| grid.value(cell) == Some(value))
                    })
            })
    }
}
enum CopyContext {
    Redis(PendingRedisCopy),
    Grid(PendingGridCopy),
}
impl CopyContext {
    fn matches(&self, app: &App) -> bool {
        match self {
            Self::Redis(context) => context.matches(app),
            Self::Grid(context) => context.matches(app),
        }
    }
}
const GRID_COPY_TAG: &str = "clipboard.grid/";

const REDIS_COPY_TAG: &str = "clipboard.redis.value/";
// 每轮最多编码 64 KiB，先让事件循环处理输入，再继续下一批。
const REDIS_COPY_BATCH: usize = 64 * 1024;

struct RedisWrite {
    context: CopyContext,
    id: Uuid,
    text: String,
    offset: usize,
    started: bool,
}

impl RedisWrite {
    // 最后的空 wdata 才提交；任何中途失败都不能发送它。
    fn batch(&mut self, mut send: impl FnMut(&[u8]) -> Result<()>) -> Result<bool> {
        if !self.started {
            send(format!("\x1b]5522;type=write:id={}\x1b\\", self.id).as_bytes())?;
            self.started = true;
        }
        let end = self
            .offset
            .saturating_add(REDIS_COPY_BATCH)
            .min(self.text.len());
        for chunk in self.text.as_bytes()[self.offset..end].chunks(4096) {
            let encoded = STANDARD.encode(chunk);
            send(
                format!(
                    "\x1b]5522;type=wdata:id={}:mime=dGV4dC9wbGFpbg==;{encoded}\x1b\\",
                    self.id
                )
                .as_bytes(),
            )?;
            self.offset += chunk.len();
        }
        if self.offset == self.text.len() {
            send(format!("\x1b]5522;type=wdata:id={}\x1b\\", self.id).as_bytes())?;
            return Ok(true);
        }
        Ok(false)
    }
}

#[derive(Default)]
pub(super) struct Clipboard {
    cut: Option<PendingCut>,
    copy: Option<(Uuid, Instant)>,
    redis: Option<PendingRedisCopy>,
    grid: Option<PendingGridCopy>,
    redis_write: Option<RedisWrite>,
}

impl Clipboard {
    pub(super) fn copy_grid(&mut self, app: &mut App, backend: &Backend) {
        self.redis = None;
        self.grid = None;
        self.redis_write = None;
        if self.cut.is_some() || self.copy.is_some() {
            app.status = i18n::UiMessage::key("剪贴板写入尚未完成");
            return;
        }
        let Some(tab) = app.tabs.get(app.active) else {
            return;
        };
        let Some(grid) = app.grid.as_ref() else {
            return;
        };
        let cells = match grid.selected_cells() {
            Ok(cells) if !cells.is_empty() && cells.iter().all(|row| !row.is_empty()) => cells,
            _ => {
                app.status = i18n::UiMessage::key("没有选中单元格");
                return;
            }
        };
        let values: Option<Vec<Vec<_>>> = cells
            .iter()
            .map(|row| row.iter().map(|&cell| grid.value(cell).cloned()).collect())
            .collect();
        let Some(values) = values else {
            app.status = i18n::UiMessage::key("结果行缺少列");
            return;
        };
        let pending = PendingGridCopy {
            tag: format!("{GRID_COPY_TAG}{}", Uuid::new_v4()),
            tab: tab.id,
            generation: app.generation,
            result: grid.page.result.id,
            start: grid.page.start,
            cells,
            values,
        };
        match backend.request(
            pending.tag.clone(),
            "local.grid.copy",
            json!({"rows": pending.values}),
        ) {
            Ok(()) => {
                self.grid = Some(pending);
                app.status = i18n::UiMessage::key("正在读取完整数据…");
            }
            Err(error) => app.status = i18n::UiMessage::raw(error.to_string()),
        }
    }

    /// 读取当前 key 的缓存基线；未提交草稿不参与复制，也不会被更改。
    pub(super) fn copy_redis_value(&mut self, app: &mut App, backend: &Backend) {
        self.grid = None;
        self.redis = None;
        self.redis_write = None;
        if self.cut.is_some() || self.copy.is_some() {
            app.status = i18n::UiMessage::key("剪贴板写入尚未完成");
            return;
        }
        let Some(tab) = app.tabs.get(app.active) else {
            app.status = i18n::UiMessage::key("没有 Redis 结果");
            return;
        };
        let Some(grid) = app
            .grid
            .as_ref()
            .filter(|_| app.is_redis() && app.browse_result)
        else {
            app.status = i18n::UiMessage::key("没有 Redis 结果");
            return;
        };
        let Some(kind) = app
            .schema
            .as_ref()
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
        else {
            app.status = i18n::UiMessage::key("缺少 Redis 类型，请重新打开资源");
            return;
        };
        let pending = PendingRedisCopy {
            tag: format!("{REDIS_COPY_TAG}{}", Uuid::new_v4()),
            tab: tab.id,
            generation: app.generation,
            target: match app.target.as_ref() {
                Some(target) => target.clone(),
                None => {
                    app.status = i18n::UiMessage::key("没有 Redis 结果");
                    return;
                }
            },
            result_id: grid.page.result.id,
            changes_revision: app.redis_changes_revision,
            profile: app.profile.as_ref().map(|p| p.id),
            session: app.session.clone(),
            selected: None,
        };
        match backend.copy_redis_value(pending.tag.clone(), pending.result_id, kind) {
            Ok(()) => {
                self.redis = Some(pending);
                app.status = i18n::UiMessage::key("正在读取完整 Redis 值…");
            }
            Err(error) => app.status = i18n::UiMessage::raw(error.to_string()),
        }
    }

    /// 复制冻结的选中行；后台完整读取大字段后才提交剪贴板。
    pub(super) fn copy_redis_selected_rows(&mut self, app: &mut App, backend: &Backend) {
        self.grid = None;
        self.redis = None;
        self.redis_write = None;
        if self.cut.is_some() || self.copy.is_some() {
            app.status = i18n::UiMessage::key("剪贴板写入尚未完成");
            return;
        }
        let Some(tab) = app.tabs.get(app.active) else {
            return;
        };
        let Some(grid) = app
            .grid
            .as_ref()
            .filter(|_| app.is_redis() && app.browse_result && app.page_token.is_none())
        else {
            app.status = i18n::UiMessage::key("没有 Redis 结果");
            return;
        };
        let Some(kind) = app
            .schema
            .as_ref()
            .and_then(|s| s.get("type"))
            .and_then(Value::as_str)
            .filter(|kind| matches!(*kind, "hash" | "list" | "set" | "zset"))
        else {
            app.status = i18n::UiMessage::key("不支持的 Redis 类型");
            return;
        };
        let rows: Option<Vec<_>> = grid
            .selected_rows()
            .into_iter()
            .map(|i| Some((i, grid.original_row(i)?.to_vec())))
            .collect();
        let Some(rows) = rows.filter(|rows| !rows.is_empty()) else {
            app.status = i18n::UiMessage::key("请选择 Redis 条目");
            return;
        };
        let Some(target) = app.target.clone() else {
            app.status = i18n::UiMessage::key("没有 Redis 结果");
            return;
        };
        let pending = PendingRedisCopy {
            tag: format!("{REDIS_COPY_TAG}{}", Uuid::new_v4()),
            tab: tab.id,
            generation: app.generation,
            target,
            result_id: grid.page.result.id,
            changes_revision: app.redis_changes_revision,
            profile: app.profile.as_ref().map(|p| p.id),
            session: app.session.clone(),
            selected: Some(rows),
        };
        let values = pending
            .selected
            .as_ref()
            .unwrap()
            .iter()
            .map(|(_, row)| row.clone())
            .collect();
        match backend.redis_selected_rows(pending.tag.clone(), values, Some(kind)) {
            Ok(()) => {
                self.redis = Some(pending);
                app.status = i18n::UiMessage::key("正在读取完整 Redis 条目…");
            }
            Err(error) => app.status = i18n::UiMessage::raw(error.to_string()),
        }
    }

    /// 每次用户事件和普通响应后调用，离开上下文即作废，切回也不能恢复。
    pub(super) fn observe_redis_context(&mut self, app: &App) {
        if self
            .grid
            .as_ref()
            .is_some_and(|pending| !pending.matches(app))
        {
            self.grid = None;
        }
        if self
            .redis
            .as_ref()
            .is_some_and(|pending| !pending.matches(app))
        {
            self.redis = None;
        }
        if self
            .redis_write
            .as_ref()
            .is_some_and(|pending| !pending.context.matches(app))
        {
            self.redis_write = None;
        }
    }

    /// 消费本地 Redis 复制响应；返回 true 表示不应再交给普通响应分派。
    pub(super) fn redis_reply(
        &mut self,
        app: &mut App,
        _session: &mut Session<Msg>,
        response: &mut Response,
    ) -> Result<bool> {
        if response.tag.starts_with(GRID_COPY_TAG) {
            if self
                .grid
                .as_ref()
                .is_none_or(|pending| pending.tag != response.tag)
            {
                return Ok(true);
            }
            let pending = self.grid.take().expect("matched grid copy request");
            if !pending.matches(app) {
                return Ok(true);
            }
            match &mut response.result {
                Ok(Value::String(text)) => {
                    self.redis_write = Some(RedisWrite {
                        context: CopyContext::Grid(pending),
                        id: Uuid::new_v4(),
                        text: std::mem::take(text),
                        offset: 0,
                        started: false,
                    });
                    app.status = i18n::UiMessage::key("正在写入剪贴板");
                }
                Ok(_) => app.status = i18n::UiMessage::raw("复制结果无效"),
                Err(error) => app.status = i18n::UiMessage::raw(error.to_string()),
            }
            return Ok(true);
        }
        if !response.tag.starts_with(REDIS_COPY_TAG) {
            return Ok(false);
        }
        if self
            .redis
            .as_ref()
            .is_none_or(|pending| pending.tag != response.tag)
        {
            return Ok(true);
        }
        let pending = self.redis.take().expect("matched Redis copy request");
        if !pending.matches(app) {
            return Ok(true);
        }
        match &mut response.result {
            Ok(Value::String(text)) => {
                self.redis_write = Some(RedisWrite {
                    context: CopyContext::Redis(pending),
                    id: Uuid::new_v4(),
                    text: std::mem::take(text),
                    offset: 0,
                    started: false,
                });
                app.status = i18n::UiMessage::key("正在写入剪贴板");
            }
            Ok(_) => app.status = i18n::UiMessage::raw("Redis 复制结果无效"),
            Err(error) => app.status = i18n::UiMessage::raw(error.to_string()),
        }
        Ok(true)
    }

    /// 最多发送一批；返回 true 表示还有待发送内容，应尽快再次处理事件循环。
    pub(super) fn pump_redis_copy(
        &mut self,
        app: &mut App,
        session: &mut Session<Msg>,
    ) -> Result<bool> {
        self.observe_redis_context(app);
        let Some(mut pending) = self.redis_write.take() else {
            return Ok(false);
        };
        if pending.batch(|bytes| session.write_raw(bytes).map_err(tsp_error))? {
            self.copy = Some((pending.id, Instant::now()));
            Ok(false)
        } else {
            self.redis_write = Some(pending);
            Ok(true)
        }
    }

    pub(super) fn copy(
        &mut self,
        app: &mut App,
        session: &mut Session<Msg>,
        text: &str,
    ) -> Result<()> {
        self.grid = None;
        self.redis = None;
        self.redis_write = None;
        if self.cut.is_some() || self.copy.is_some() {
            app.status = i18n::UiMessage::key("剪贴板写入尚未完成");
            return Ok(());
        }
        let id = Uuid::new_v4();
        Self::write(session, id, text)?;
        self.copy = Some((id, Instant::now()));
        app.status = i18n::UiMessage::key("正在写入剪贴板");
        Ok(())
    }

    pub(super) fn cut(
        &mut self,
        app: &mut App,
        session: &mut Session<Msg>,
        tab: Uuid,
        range: Range<usize>,
        expected: &str,
    ) -> Result<()> {
        self.grid = None;
        self.redis = None;
        self.redis_write = None;
        if self.cut.is_some() || self.copy.is_some() {
            app.status = i18n::UiMessage::key("剪贴板写入尚未完成");
            return Ok(());
        }
        let Some(active) = app.tabs.get(app.active) else {
            app.status = i18n::UiMessage::key("SQL 选区已变化，请重新选择");
            return Ok(());
        };
        let query = &active.query;
        let selected = editor::utf16_to_byte(&query.text, range.start)
            .zip(editor::utf16_to_byte(&query.text, range.end))
            .and_then(|(start, end)| query.text.get(start..end));
        if active.id != tab || range.is_empty() || selected != Some(expected) {
            app.status = i18n::UiMessage::key("SQL 选区已变化，请重新选择");
            return Ok(());
        }
        let id = Uuid::new_v4();
        self.cut = Some(PendingCut {
            id,
            tab,
            range,
            anchor: query.anchor,
            cursor: query.cursor,
            original: query.text.clone(),
            started: Instant::now(),
        });
        Self::write(session, id, expected)?;
        app.status = i18n::UiMessage::key("正在写入剪贴板");
        Ok(())
    }

    // OSC 52 没有写入回执；OSC 5522 的 DONE / EPERM 可区分成功和拒绝授权。
    fn write(session: &mut Session<Msg>, id: Uuid, text: &str) -> Result<()> {
        session
            .write_raw(format!("\x1b]5522;type=write:id={id}\x1b\\").as_bytes())
            .map_err(tsp_error)?;
        for chunk in text.as_bytes().chunks(4096) {
            let encoded = STANDARD.encode(chunk);
            session
                .write_raw(
                    format!("\x1b]5522;type=wdata:id={id}:mime=dGV4dC9wbGFpbg==;{encoded}\x1b\\")
                        .as_bytes(),
                )
                .map_err(tsp_error)?;
        }
        session
            .write_raw(format!("\x1b]5522;type=wdata:id={id}\x1b\\").as_bytes())
            .map_err(tsp_error)?;
        Ok(())
    }

    pub(super) fn reply(&mut self, app: &mut App, payload: &[u8]) -> bool {
        let Ok(payload) = std::str::from_utf8(payload) else {
            return false;
        };
        let metadata = payload.split(';').next().unwrap_or("");
        let field = |name: &str| {
            metadata
                .split(':')
                .find_map(|entry| entry.strip_prefix(name))
        };
        if self.redis_write.as_ref().is_some_and(|pending| {
            field("type=") == Some("write")
                && field("id=").and_then(|s| Uuid::parse_str(s).ok()) == Some(pending.id)
        }) && let Some(status) = field("status=")
        {
            self.redis_write = None;
            app.status = i18n::UiMessage::template(
                "剪贴板写入失败：{error}",
                vec![("error", i18n::UiArg::Raw(status.to_owned()))],
            );
            return true;
        }
        if let Some((id, _)) = self.copy
            && field("type=") == Some("write")
            && field("id=").and_then(|s| Uuid::parse_str(s).ok()) == Some(id)
            && let Some(status) = field("status=")
        {
            self.copy = None;
            app.status = if status == "DONE" {
                i18n::UiMessage::key("已复制")
            } else {
                i18n::UiMessage::template(
                    "剪贴板写入失败：{error}",
                    vec![("error", i18n::UiArg::Raw(status.to_owned()))],
                )
            };
            return true;
        }
        let Some(pending) = self.cut.as_ref() else {
            return false;
        };
        if field("type=") != Some("write")
            || field("id=").and_then(|s| Uuid::parse_str(s).ok()) != Some(pending.id)
        {
            return false;
        }
        let Some(status) = field("status=") else {
            return false;
        };
        let pending = self.cut.take().expect("matched pending clipboard request");
        if status != "DONE" {
            app.status = i18n::UiMessage::template(
                "剪贴板写入失败：{error}",
                vec![("error", i18n::UiArg::Raw(status.to_owned()))],
            );
            return true;
        }
        // 来源已关闭时只消费迟到回执；不能创建标签或把剪切应用到其他查询。
        let Some(active) = app.tabs.get_mut(app.active) else {
            return true;
        };
        let query = &mut active.query;
        if active.id != pending.tab
            || query.text != pending.original
            || query.anchor != pending.anchor
            || query.cursor != pending.cursor
        {
            app.status = i18n::UiMessage::key("SQL 选区已变化，请重新选择");
            return true;
        }
        if query.edit(pending.range, "") {
            app.changed_field("query");
            app.status = i18n::UiMessage::key("已剪切");
        }
        true
    }

    pub(super) fn expire(&mut self, app: &mut App) -> bool {
        if self
            .copy
            .as_ref()
            .is_some_and(|(_, started)| started.elapsed() >= Duration::from_secs(30))
        {
            self.copy = None;
            app.status = i18n::UiMessage::key("剪贴板未确认写入");
            return true;
        }
        if self
            .cut
            .as_ref()
            .is_some_and(|pending| pending.started.elapsed() >= Duration::from_secs(30))
        {
            self.cut = None;
            app.status = i18n::UiMessage::key("剪贴板未确认写入，原文已保留");
            return true;
        }
        false
    }
}

#[cfg(test)]
#[path = "../tests/unit/clipboard.rs"]
mod tests;
