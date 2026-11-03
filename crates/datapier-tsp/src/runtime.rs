use super::*;
use crate::{input::focus_node_id, workspace::finish_after_input_closed};
use tern_sdk::{Input, Options, Session, SurfaceOptions};
/// 在调用线程运行同步 TSP 界面，Session 始终由该线程独占。
/// Engine IPC 在独立 Tokio 线程运行；终端不可用、保存未确认或 TSP 失败时返回错误。
pub fn run(root: PathBuf, workspace_id: String) -> Result<()> {
    let options = Options::new()
        .app("Datapier")
        .version(env!("CARGO_PKG_VERSION"))
        .feature("edit")
        .feature("undo");
    let mut session = Session::<Msg>::connect(options)
        .map_err(tsp_error)?
        .ok_or_else(|| AppError::new("tsp_unavailable", "需要在 Tern 的 TSP pane 中运行"))?;
    let surface = session
        .open(
            SurfaceOptions::screen()
                .font(tern_sdk::Font::Sans)
                .title("Datapier")
                .role("datapier.database")
                .keep(false),
        )
        .map_err(tsp_error)?;
    let waker = session
        .waker()
        .ok_or_else(|| AppError::new("tsp_unavailable", "TSP 终端没有提供唤醒句柄"))?;
    let mut backend = Backend::start(root, waker)?;
    let mut app = App::new(workspace_id);
    app.icon_blobs = Some(IconBlobs {
        brand: session
            .blob(
                include_bytes!("../../../docs/assets/datapier.svg").to_vec(),
                Some("image/svg+xml"),
            )
            .map_err(tsp_error)?,
        mysql: session
            .blob(
                include_bytes!("../../../docs/assets/providers/mysql.svg").to_vec(),
                Some("image/svg+xml"),
            )
            .map_err(tsp_error)?,
        redis: session
            .blob(
                include_bytes!("../../../docs/assets/providers/redis.svg").to_vec(),
                Some("image/svg+xml"),
            )
            .map_err(tsp_error)?,
        archery: session
            .blob(
                include_bytes!("../../../docs/assets/archery.svg").to_vec(),
                Some("image/svg+xml"),
            )
            .map_err(tsp_error)?,
        archery_badge: session
            .blob(
                include_bytes!("../../../docs/assets/archery-badge.svg").to_vec(),
                Some("image/svg+xml"),
            )
            .map_err(tsp_error)?,
    });
    app.update_viewport(session.caps());
    let mut css = String::new();
    let mut changed = true;
    let mut last_save = Instant::now();
    let mut quitting = false;
    let mut clipboard = clipboard::Clipboard::default();
    let mut revealed_tab = None;
    loop {
        changed |= app.navigation_poll_drop(&backend);
        clipboard.observe_redis_context(&app);
        while let Some(mut reply) = backend.receive() {
            if clipboard.redis_reply(&mut app, &mut session, &mut reply)? {
                changed = true;
                continue;
            }
            app.routed_response(reply, &backend);
            clipboard.observe_redis_context(&app);
            changed = true;
        }
        changed |= clipboard.expire(&mut app);
        if app.last_settings_poll.elapsed() >= Duration::from_secs(5) {
            app.last_settings_poll = Instant::now();
            app.refresh_settings(&backend);
            changed |= i18n::refresh_system_language();
        }
        if quitting {
            if app.save_blocked {
                quitting = false;
                app.status = i18n::UiMessage::template(
                    "保存失败，未退出：{error}",
                    vec![(
                        "error",
                        i18n::UiArg::Message(Box::new(std::mem::take(&mut app.status))),
                    )],
                );
                changed = true;
            } else if app.settings_save_token.is_some() && !backend.is_closed() {
                // 等待本次及排队的语言设置写入确认；失败时仍可退出会话。
            } else if app.dirty {
                app.save_workspace(&backend);
            } else if !app.saving {
                break;
            }
        }
        // Engine 事件只是提示，定期查询每个 tab 的当前任务以补偿遗漏事件。
        if app.last_poll.elapsed() >= Duration::from_millis(700) {
            app.last_poll = Instant::now();
            for (index, tab) in app.tabs.iter().enumerate() {
                let (task, generation) = if index == app.active {
                    (&app.task, app.generation)
                } else {
                    (&tab.runtime.task, tab.runtime.generation)
                };
                if let Some(task) = task
                    && let Err(e) = backend.request(
                        format!("{}:{}:execution.get/{task}", tab.id, generation),
                        "execution.get",
                        json!({"task_id":task}),
                    )
                {
                    app.status = i18n::UiMessage::raw(e.to_string());
                    changed = true;
                }
            }
        }
        if app.dirty && last_save.elapsed() >= Duration::from_millis(800) {
            app.save_workspace(&backend);
            last_save = Instant::now();
        }
        if changed {
            app.sync_grid_visible_rows();
            app.fit_grid();
            let next_css = views::stylesheet(&app);
            if css != next_css {
                session
                    .stylesheet(surface, "datapier", Some(&next_css))
                    .map_err(tsp_error)?;
                css = next_css;
            }
            app.reconcile_modal_focus();
            let focus_request = app.take_focus_request();
            let tab_layout = app.tabs.get(app.active).map(|tab| {
                (
                    tab.id,
                    app.active,
                    app.viewport_width,
                    app.navigation_compact(),
                )
            });
            let reveal =
                tab_layout.is_some() && (revealed_tab != tab_layout || focus_request.is_some());
            let view = views::render(&app);
            let focus = focus_request.map(|key| focus_node_id(&view, &key));
            // 新资源标题可能晚于建 tab 到达；内容焦点请求也须重新保证标签可见。
            // 仅切换或布局变化时滚动，保留用户手动浏览其他标签的位置。
            let tab_id = reveal
                .then(|| focus_node_id(&view, &format!("tab{}", app.active)))
                .flatten();
            let cli_entry = if std::mem::take(&mut app.workbench.cli_reveal) {
                app.workbench
                    .cli_history
                    .last()
                    .and_then(|record| focus_node_id(&view, &format!("cli-entry-{}", record.id)))
            } else {
                None
            };
            session.render(surface, view).map_err(tsp_error)?;
            if let Some(id) = focus {
                session.focus(surface, id.as_deref()).map_err(tsp_error)?;
            }
            if let Some(id) = tab_id {
                session
                    .reveal(surface, &id, tern_sdk::wire::RevealAt::Nearest)
                    .map_err(tsp_error)?;
            }
            if let Some(id) = cli_entry {
                session
                    .reveal(surface, &id, tern_sdk::wire::RevealAt::End)
                    .map_err(tsp_error)?;
            }
            revealed_tab = tab_layout;
            changed = false;
        }
        let redis_sending = clipboard.pump_redis_copy(&mut app, &mut session)?;
        match session.next(Some(if redis_sending {
            Duration::ZERO
        } else {
            Duration::from_millis(250)
        })) {
            Ok(Some(Input::Msg(message, event))) => {
                if app.tab_close.is_some() {
                    continue;
                }
                if !app.workspace_loaded {
                    app.workspace_interacted = true;
                }
                if !matches!(message, Msg::Focus(_)) {
                    app.restore_focus_pending = false;
                }
                let result_info_action = matches!(&message, Msg::Action(name)
                    if name.starts_with("result-info-toggle:")
                        || matches!(name.as_str(), "result-info-close" | "result-info-dismiss" | "result-info-keep"));
                if !result_info_action {
                    app.close_result_info(false);
                }
                app.sync_grid_visible_rows();
                match message {
                    Msg::Action(name) if name == "quit" => {
                        quitting = app.request_quit(&backend);
                    }
                    Msg::Action(name) if name.starts_with("redis-key-click:") => {
                        if let Some(index) =
                            app.redis_key_action_index(&name["redis-key-click:".len()..])
                        {
                            let (shift, primary) = if let tern_sdk::Event::Action(action) = &event {
                                (
                                    action.mods.iter().any(|m| m == "shift"),
                                    i18n::multi_select_modifier(
                                        action.mods.iter().any(|m| m == "meta"),
                                        action.mods.iter().any(|m| m == "ctrl"),
                                    ),
                                )
                            } else {
                                (false, false)
                            };
                            app.redis_key_click(index, shift, primary, &backend);
                        }
                    }
                    Msg::Action(name) if name.starts_with("redis-key-menu:") => {
                        if let Some((identity, action)) =
                            name["redis-key-menu:".len()..].rsplit_once(':')
                            && let Some(index) = app.redis_key_action_index(identity)
                            && let Some(text) = app.redis_key_menu(index, action, &backend)
                        {
                            clipboard.copy(&mut app, &mut session, &text)?;
                        }
                    }
                    Msg::Action(name) if name == "cli-copy" => {
                        clipboard.copy_cli(&mut app, &backend)
                    }
                    Msg::Action(name) => app.action(&name, &backend),
                    Msg::CliResize(share) => app.resize_cli(share),
                    Msg::CopyText(text) => clipboard.copy(&mut app, &mut session, &text)?,
                    Msg::CopyRedisValue => clipboard.copy_redis_value(&mut app, &backend),
                    Msg::CopyGrid => {
                        if app.finish_inline(true) {
                            app.focus = "grid".into();
                            app.workbench.focus_request = Some(String::new());
                            clipboard.copy_grid(&mut app, &backend);
                        }
                    }
                    Msg::QueryMenu(tab, action, start, end, text) => {
                        app.query_menu(tab, &action, start, end, &text, &backend);
                    }
                    Msg::QueryCut(tab, text, start, end) => {
                        clipboard.cut(&mut app, &mut session, tab, start..end, &text)?;
                    }
                    Msg::GridMenu(cell, menus::GridMenuAction::CopyCells) => {
                        if app.prepare_grid_cell_copy(cell) {
                            clipboard.copy_grid(&mut app, &backend);
                        }
                    }
                    Msg::GridMenu(cell, menus::GridMenuAction::CopyRedisRows) => {
                        if app.prepare_redis_grid_menu(cell) {
                            clipboard.copy_redis_selected_rows(&mut app, &backend);
                        }
                    }
                    Msg::GridMenu(cell, menus::GridMenuAction::CopyRows) => {
                        if app.finish_inline(true) {
                            let ready = app.grid.as_mut().is_some_and(|grid| {
                                grid.row_in_page(cell.row)
                                    && (grid.is_row_selected(cell.row)
                                        || grid
                                            .select_row(cell.row, cell.column, false, false)
                                            .is_ok())
                            });
                            if ready {
                                app.focus = "grid".into();
                                app.workbench.focus_request = Some(String::new());
                                clipboard.copy_grid(&mut app, &backend);
                            } else {
                                app.status = i18n::UiMessage::key("目标单元格已不可用");
                            }
                        }
                    }
                    Msg::GridMenu(cell, action) => {
                        if let Some(text) = app.grid_menu(cell, action, &backend) {
                            clipboard.copy(&mut app, &mut session, &text)?;
                        }
                    }
                    Msg::Focus(name) => {
                        if app.modal_allows_field(&name) {
                            app.focus = name;
                            if app.focus == "cli_height" {
                                // split 的键盘由程序处理，先清除宿主输入字段的 native editing。
                                app.workbench.focus_request = Some(String::new());
                            } else if let tern_sdk::Event::Focus(f) = event {
                                session.focus(surface, Some(&f.id)).map_err(tsp_error)?;
                            }
                        }
                    }
                    message @ (Msg::Edit(..) | Msg::Undo(..)) => {
                        app.dispatch_editor_message(message)
                    }
                    Msg::Profile(index) => {
                        if app.finish_inline(true) {
                            app.select_profile(index, &backend);
                        }
                    }
                    Msg::Tab(index) => {
                        if index != app.active && index < app.tabs.len() {
                            app.invalidate_history(&backend);
                        }
                        app.select_tab(index);
                    }
                    Msg::ResultSet(id) => {
                        if !app.finish_inline(true) {
                            changed = true;
                            continue;
                        }
                        if app.has_live_drafts() {
                            app.status = "请先提交或丢弃待提交变更与粘贴预览".into();
                        } else {
                            let start = app.result_saved_start(&id);
                            app.large = None;
                            app.result_id = Some(id);
                            app.page_history.clear();
                            app.grid = None;
                            app.page(&backend, start);
                        }
                    }
                    Msg::Complete(text, start, end) => {
                        if let Some(tab) = app.tabs.get_mut(app.active)
                            && tab.query.edit(start..end, &text)
                        {
                            app.changed_field("query");
                        }
                    }
                    Msg::Cell(row, column, extend) => {
                        if !app.finish_inline(true) {
                            changed = true;
                            continue;
                        }
                        if let Some(grid) = &mut app.grid {
                            if let Err(e) = grid.select(Cell { row, column }, extend) {
                                app.status = workbench::grid_error(e);
                            }
                            app.focus = "grid".into();
                            session.focus(surface, None).map_err(tsp_error)?;
                        }
                    }
                    Msg::CellOpen(row, column) => {
                        if let Some(grid) = &mut app.grid {
                            let _ = grid.select(Cell { row, column }, false);
                        }
                        if app.is_redis() {
                            app.workbench_redis_edit(&backend);
                        } else {
                            app.begin_inline(&backend);
                        }
                    }
                    Msg::Row(row, column) => {
                        if !app.finish_inline(true) {
                            changed = true;
                            continue;
                        }
                        let (extend, add) = if let tern_sdk::Event::Action(action) = &event {
                            (
                                action.mods.iter().any(|m| m == "shift"),
                                i18n::multi_select_modifier(
                                    action.mods.iter().any(|m| m == "meta"),
                                    action.mods.iter().any(|m| m == "ctrl"),
                                ),
                            )
                        } else {
                            (false, false)
                        };
                        if let Some(grid) = &mut app.grid {
                            if let Err(e) = grid.select_row(row, column, extend, add) {
                                app.status = workbench::grid_error(e);
                            }
                            app.focus = "grid".into();
                            session.focus(surface, None).map_err(tsp_error)?;
                        }
                    }
                }
                clipboard.observe_redis_context(&app);
                changed = true;
            }
            Ok(Some(Input::Key(key))) => {
                if app.tab_close.is_some() {
                    continue;
                }
                if key.is("escape") && app.workbench.query_ui.result_info.is_some() {
                    app.close_result_info(true);
                    changed = true;
                    continue;
                }
                if !app.workspace_loaded {
                    app.workspace_interacted = true;
                }
                app.restore_focus_pending = false;
                if key.is("ctrl+q") {
                    quitting = app.request_quit(&backend);
                    changed = true;
                    continue;
                }
                if app.workbench.overlay.as_deref() == Some("history")
                    && app.focus == "history_filter"
                    && key.is("enter")
                {
                    app.action("history-search", &backend);
                    changed = true;
                    continue;
                }
                if !app.tabs.is_empty()
                    && app.focus == "query"
                    && !app.is_redis()
                    && app.workbench.overlay.is_none()
                {
                    if key.name == "enter" && i18n::multi_select_modifier(key.meta, key.ctrl) {
                        let action = if key.shift {
                            "run"
                        } else if app.tabs[app.active].query.selected_text().is_some() {
                            "selection"
                        } else {
                            "current"
                        };
                        app.action(action, &backend);
                        changed = true;
                        continue;
                    }
                    if (key.name == "c" || key.name == "x")
                        && i18n::multi_select_modifier(key.meta, key.ctrl)
                    {
                        let query = &app.tabs[app.active].query;
                        if let Some(text) = query.selected_text().map(str::to_owned) {
                            if key.name == "x" {
                                let tab = app.tabs[app.active].id;
                                let range = query.selection();
                                clipboard.cut(&mut app, &mut session, tab, range, &text)?;
                            } else {
                                clipboard.copy(&mut app, &mut session, &text)?;
                            }
                        }
                        changed = true;
                        continue;
                    }
                }
                if app.workbench_key(&key, &backend) {
                    changed = true;
                    continue;
                }
                if !app.modal_allows_field(&app.focus) {
                    continue;
                }
                if app.focus == "grid" {
                    if key.is("enter") || key.is("f2") {
                        if app.is_redis() {
                            app.workbench_redis_edit(&backend);
                        } else {
                            app.begin_inline(&backend);
                        }
                        changed = true;
                        continue;
                    }
                    if let Some(grid) = &mut app.grid {
                        let nav = match key.name.as_str() {
                            "up" => Some(Navigation::Up),
                            "down" => Some(Navigation::Down),
                            "left" => Some(Navigation::Left),
                            "right" => Some(Navigation::Right),
                            "home" => Some(Navigation::Home),
                            "end" => Some(Navigation::End),
                            "tab" if key.shift => Some(Navigation::BackTab),
                            "tab" => Some(Navigation::Tab),
                            _ => None,
                        };
                        if let Some(nav) = nav {
                            grid.navigate(nav, key.shift);
                        } else if key.is("ctrl+z") || key.is("meta+z") {
                            grid.undo();
                            app.dirty = true;
                        } else if key.name == "paste"
                            && let Some(text) = key.text.as_deref()
                        {
                            match grid.preview_paste(text) {
                                Ok(preview) => {
                                    if let Err(error) = app.check_rows_not_deleted(
                                        preview.edits.iter().map(|edit| edit.cell.row),
                                    ) {
                                        app.status = error;
                                        changed = true;
                                        continue;
                                    }
                                    app.detail = format!(
                                        "粘贴预览：{} 个单元格\n{}",
                                        preview.edits.len(),
                                        preview
                                            .edits
                                            .iter()
                                            .map(|e| format!(
                                                "{}:{}  {} → {}",
                                                e.cell.row + 1,
                                                e.cell.column,
                                                e.before.text(),
                                                e.after.text()
                                            ))
                                            .collect::<Vec<_>>()
                                            .join("\n")
                                    );
                                    app.set_detail_context("粘贴预览");
                                    app.paste_preview = Some(preview);
                                    app.workbench.overlay = Some("preview".into());
                                }
                                Err(e) => app.status = workbench::grid_error(e),
                            }
                        }
                    }
                } else {
                    app.input_key(&key);
                }
                changed = true;
            }
            Ok(Some(Input::Event(event))) => match event {
                tern_sdk::Event::Resize(_) => {
                    changed |= app.update_viewport(session.caps());
                }
                tern_sdk::Event::Error(error) => {
                    app.status = i18n::UiMessage::raw(format!("Tern: {error:?}"));
                    changed = true;
                }
                _ => {}
            },
            // None 也可能来自超时或 waker，只有 InputClosed 才表示输入结束。
            Ok(Some(Input::Clipboard(payload))) => {
                changed |= clipboard.reply(&mut app, &payload);
            }
            Ok(None) => {}
            Err(tern_sdk::Error::InputClosed) => {
                finish_after_input_closed(&mut app, &mut backend)?;
                break;
            }
            Err(e) => return Err(tsp_error(e)),
        }
    }
    session.close().map_err(tsp_error)?;
    Ok(())
}
