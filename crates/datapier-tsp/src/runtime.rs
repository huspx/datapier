use super::*;
use crate::{input::focus_node_id, workspace::finish_after_input_closed};
use tern_sdk::{Input, Options, Session, SurfaceOptions};
/// 每批处理一条请求帧；每个 pane 独立校验目录、nonce 和窗口租约。
pub async fn window_bridge(mut payload: Value) -> Result<Value> {
    let requests = payload
        .get_mut("requests")
        .and_then(Value::as_array_mut)
        .map(std::mem::take)
        .ok_or_else(|| AppError::invalid("缺少窗口桥接请求列表"))?;
    if requests.len() > 128 {
        return Err(AppError::invalid("窗口桥接请求超过 128 项"));
    }
    let mut jobs = tokio::task::JoinSet::new();
    for request in requests {
        jobs.spawn(async move {
            let pane = request.get("pane_id").cloned().unwrap_or(Value::Null);
            let nonce = request["nonce"].clone();
            let root = request["data_dir"].as_str().map(PathBuf::from);
            let result = match root {
                Some(root) if root.is_absolute() => window_bridge_item(root, request).await,
                _ => Err(AppError::invalid("窗口桥接数据目录必须为绝对路径")),
            };
            let mut item = match result {
                Ok(value) => value,
                Err(error) => json!({"bound":false,"error":error}),
            };
            item["pane_id"] = pane;
            item["nonce"] = nonce;
            item
        });
    }
    let mut items = Vec::with_capacity(jobs.len());
    while let Some(result) = jobs.join_next().await {
        items.push(result.map_err(|error| AppError::new("internal", error.to_string()))?);
    }
    Ok(json!({"items":items}))
}

async fn window_bridge_item(root: PathBuf, payload: Value) -> Result<Value> {
    let client = datapier_engine::Client::connect_with(
        &root,
        datapier_engine::ClientOptions {
            kind: datapier_core::model::ClientKind::WindowBridge,
            start_engine: false,
            ..datapier_engine::ClientOptions::default()
        },
    )
    .await?;
    let result = client.request("ui.window.sync", payload).await;
    let closed = client.close().await;
    match result {
        Ok(value) => {
            closed?;
            Ok(value)
        }
        Err(error) => Err(error),
    }
}

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
    let window_data_dir = if root.is_absolute() {
        root.clone()
    } else {
        std::env::current_dir()?.join(&root)
    };
    let mut backend = Backend::start(root, workspace_id.clone(), waker)?;
    let mut app = App::new(workspace_id);
    app.window_data_dir = Some(window_data_dir);
    let window_nonce = Uuid::new_v4();
    let mut window_node = None;
    let mut window_bound = false;
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
    let mut pane_visible = true;
    let mut native_state = "";
    let mut retained_value_sources = std::collections::BTreeSet::new();
    loop {
        // SDK 在 pump/render 中也可能接收 resize；始终同步最新能力。
        changed |= app.update_viewport(session.caps());
        let reduce_motion = session.caps().reduce_motion;
        if app.native_ui.reduce_motion != reduce_motion {
            app.native_ui.reduce_motion = reduce_motion;
            changed = true;
        }
        if app.workspace_loaded
            && !app.tabs.is_empty()
            && app.advanced_schema.is_none()
            && let Some(draft) = app.tabs[app.active].schema_draft.take()
        {
            app.advanced_schema_restore(&backend, draft);
        }
        changed |= app.navigation_poll_drop(&backend);
        changed |= app.completion_poll(&backend);
        changed |= app.native_ui_tick();
        let activity = crate::native_ui::program_state(&app);
        if native_state != activity {
            use std::io::Write as _;
            write!(std::io::stdout(), "\x1b]7501;state={activity}\x1b\\")?;
            std::io::stdout().flush()?;
            native_state = activity;
        }
        changed |= app.navigation_poll_create_database(&backend);
        clipboard.observe_redis_context(&app);
        while let Some(mut reply) = backend.receive() {
            if reply.tag == "window.bind" {
                window_bound = reply
                    .result
                    .as_ref()
                    .is_ok_and(|value| value["bound"] == true);
            }
            if clipboard.redis_reply(&mut app, &mut session, &mut reply)? {
                changed = true;
                continue;
            }
            app.routed_response(reply, &backend);
            clipboard.observe_redis_context(&app);
            changed = true;
        }
        app.release_retired_value_sources(&backend, &mut retained_value_sources);
        app.workspace_restore_poll(&backend);
        if let Some(png) = app.advanced_value_take_png() {
            match session.blob(png, Some("image/png")) {
                Ok(blob) => app.advanced_value.image_blob = Some(blob),
                Err(error) => app.status = i18n::UiMessage::raw(error.to_string()),
            }
            changed = true;
        }
        app.advanced_value_release_previews(&backend);
        app.sync_draft_scope(&backend);
        app.replace_poll(&backend);
        changed |= clipboard.expire(&mut app);
        if app.last_settings_poll.elapsed()
            >= Duration::from_secs(if pane_visible { 5 } else { 30 })
        {
            app.last_settings_poll = Instant::now();
            app.refresh_settings(&backend);
            app.approval_refresh(&backend);
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
            } else if (app.settings_save_token.is_some() || app.native_ui.settings_busy())
                && !backend.is_closed()
            {
                // 全局设置需要确认 Engine 和本机 host 各自负责的写入。
            } else if app.dirty {
                app.save_workspace(&backend);
            } else if !app.saving {
                break;
            }
        }
        // Engine 事件只是提示，定期查询每个 tab 的当前任务以补偿遗漏事件。
        if app.last_poll.elapsed() >= Duration::from_millis(if pane_visible { 700 } else { 7000 }) {
            app.last_poll = Instant::now();
            let active = app.active;
            for index in 0..app.tabs.len() {
                let status = (index != active).then(|| app.status.clone());
                app.switch_tab(index);
                if app.workbench.transfer.cleanup_wait && app.workbench.transfer.pending.is_none() {
                    app.transfer_action("transfer-ui-cleanup-poll", &backend);
                }
                if app.workbench.transfer.pending.is_none()
                    && app
                        .workbench
                        .transfer
                        .plan
                        .as_ref()
                        .and_then(|p| p.get("state"))
                        .and_then(Value::as_str)
                        == Some("preparing")
                {
                    app.transfer_action("transfer-ui-refresh-plan", &backend);
                }
                if app.workbench.transfer.pending.is_none()
                    && app
                        .workbench
                        .transfer
                        .plan
                        .as_ref()
                        .is_some_and(|plan| plan["state"] == "ready" && plan.get("spec").is_none())
                {
                    if app.workbench.transfer.plan_section != "spec" {
                        app.transfer_action("transfer-ui-section:spec", &backend);
                    } else {
                        app.transfer_action("transfer-ui-page-read", &backend);
                    }
                } else if app.workbench.transfer.pending.is_none()
                    && app.workbench.transfer.chunk_offset > 0
                {
                    app.transfer_action("transfer-ui-page-read", &backend);
                }
                if app.workbench.transfer.pending.is_none()
                    && app.workbench.transfer.run.as_ref().is_some_and(|run| {
                        run.get("run_id").or_else(|| run.get("id")).is_some()
                            && matches!(
                                run.get("state").and_then(Value::as_str),
                                None | Some("queued") | Some("running") | Some("cancelling")
                            )
                    })
                {
                    app.transfer_action("transfer-ui-refresh-run", &backend);
                }
                if let Some(status) = status {
                    app.status = status;
                }
            }
            app.switch_tab(active);
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
        if changed && pane_visible {
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
            let view = views::render_window(&app, Some(window_nonce));
            window_node = focus_node_id(&view, "layout");
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
            Duration::from_millis(if pane_visible { 50 } else { 500 })
        })) {
            Ok(Some(Input::Msg(_, tern_sdk::Event::Action(action))))
            | Ok(Some(Input::Event(tern_sdk::Event::Action(action))))
                if action.act == "datapier-window-bind"
                    && window_node.as_deref() == Some(action.id.as_str()) =>
            {
                // SDK 会把有 click handler 的节点 action 包装成 Msg；两种输入都须先核验。
                if let Err(error) = backend.request(
                    "window.bind",
                    "ui.window.bind",
                    json!({"nonce":window_nonce}),
                ) {
                    app.status = i18n::UiMessage::raw(error.to_string());
                }
            }
            Ok(Some(Input::Msg(_, tern_sdk::Event::Action(action))))
            | Ok(Some(Input::Event(tern_sdk::Event::Action(action))))
                if window_node.as_deref() == Some(action.id.as_str())
                    && (action.act.starts_with("datapier-plugin-reply:")
                        || action.act.starts_with("datapier-plugin-update-status:")
                        || action.act.starts_with("datapier-project-path:")
                        || action.act.starts_with("datapier-glimpse-opened:")
                        || action.act.starts_with("datapier-glimpse-failed:")
                        || action.act == "datapier-open-todos"
                        || action.act.starts_with("datapier-open-todos:")) =>
            {
                changed |= app.native_ui_action(&action.act, &backend);
            }
            Ok(Some(Input::Msg(_, _) | Input::Key(_))) if app.workspace_switch_pending() => {}
            Ok(Some(Input::Msg(message, event))) => {
                if window_bound
                    && !matches!(message, Msg::Focus(_))
                    && let Err(error) = backend.request(
                        "window.active",
                        "ui.window.bind",
                        json!({"nonce":window_nonce,"active":true}),
                    )
                {
                    app.status = i18n::UiMessage::raw(error.to_string());
                }
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
                    Msg::Action(name) if name == "query-completion" => {
                        app.completion_update(true);
                    }
                    Msg::Action(name) => app.action(&name, &backend),
                    Msg::CliResize(share) => app.resize_cli(share),
                    Msg::QueryResize(share) => {
                        if share.is_finite() {
                            app.query_share = share.clamp(0.15, 0.85);
                        }
                    }
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
                        let Some(name) = app.resolve_project_editor(name) else {
                            continue;
                        };
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
                        app.dispatch_guarded_editor_message(message, &backend);
                    }
                    Msg::Profile(index) => {
                        if app.finish_inline(true) {
                            app.select_profile(index, &backend);
                        }
                    }
                    Msg::Tab(index) => {
                        if index != app.active && index < app.tabs.len() {
                            app.completion_close();
                            app.completion_unsubscribe(&backend);
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
                    Msg::CompletionAccept(id, index) => app.completion_accept(id, index),
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
                if window_bound
                    && let Err(error) = backend.request(
                        "window.active",
                        "ui.window.bind",
                        json!({"nonce":window_nonce,"active":true}),
                    )
                {
                    app.status = i18n::UiMessage::raw(error.to_string());
                }
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
                    if app.completion_key(&key) {
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
                    if app.restore_rows.is_some() {
                        app.status = i18n::UiMessage::key("正在恢复行草稿，请等待完成");
                        changed = true;
                        continue;
                    }
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
                            app.draft_revision = app.draft_revision.wrapping_add(1);
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
                                    app.guard_draft_scope(
                                        &backend,
                                        workbench::DraftScopeAction::Protect,
                                    );
                                }
                                Err(e) => app.status = workbench::grid_error(e),
                            }
                        }
                    }
                } else {
                    let scan = if matches!(app.focus.as_str(), "pattern" | "type") {
                        app.session_id().zip(app.cursor.clone())
                    } else {
                        None
                    };
                    let editing = app.draft_input_field(&app.focus)
                        && (key.typed().is_some()
                            || matches!(
                                key.name.as_str(),
                                "backspace" | "delete" | "paste" | "enter"
                            ));
                    if !editing || app.draft_scope_ready() {
                        app.input_key(&key);
                    } else {
                        app.guard_draft_scope(
                            &backend,
                            workbench::DraftScopeAction::Key(app.focus.clone(), key.clone()),
                        );
                    }
                    if app.workbench.scan_dirty
                        && app.cursor.is_none()
                        && let Some((session, cursor)) = scan
                    {
                        app.discard_resource_scan(&backend, session, cursor);
                    }
                }
                changed = true;
            }
            Ok(Some(Input::Event(event))) => match event {
                tern_sdk::Event::Visible(event) => {
                    pane_visible = event.visible;
                    if pane_visible {
                        app.last_poll = Instant::now() - Duration::from_secs(8);
                        app.last_settings_poll = Instant::now() - Duration::from_secs(31);
                        app.refresh_settings(&backend);
                        app.approval_refresh(&backend);
                    }
                    changed = true;
                }
                tern_sdk::Event::Action(action)
                    if action.act == "datapier-window-bind"
                        && window_node.as_deref() == Some(action.id.as_str()) =>
                {
                    // 仅处理发给本 surface 节点的 action；进程存活本身不能证明窗口存在。
                    if let Err(error) = backend.request(
                        "window.bind",
                        "ui.window.bind",
                        json!({"nonce":window_nonce}),
                    ) {
                        app.status = i18n::UiMessage::raw(error.to_string());
                    }
                }
                tern_sdk::Event::Action(action)
                    if window_node.as_deref() == Some(action.id.as_str())
                        && (action.act.starts_with("datapier-editor-launched:")
                            || action.act.starts_with("datapier-editor-failed:")) =>
                {
                    let launched = action.act.starts_with("datapier-editor-launched:");
                    let id = action
                        .act
                        .split_once(':')
                        .and_then(|(_, id)| Uuid::parse_str(id).ok());
                    let mut matched = false;
                    if id.is_some() && id == app.advanced_files.terminal_handoff {
                        app.advanced_files.terminal_handoff = None;
                        matched = true;
                    } else if let Some(id) = id
                        && let Some(tab) = app
                            .tabs
                            .iter_mut()
                            .find(|tab| tab.runtime.advanced_files.terminal_handoff == Some(id))
                    {
                        tab.runtime.advanced_files.terminal_handoff = None;
                        matched = true;
                    }
                    if matched {
                        app.status = if launched {
                            i18n::UiMessage::key("终端编辑器已启动，请明确读取修改")
                        } else {
                            i18n::UiMessage::key("终端编辑器启动失败，文件已保留，可重试")
                        };
                        changed = true;
                    }
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
