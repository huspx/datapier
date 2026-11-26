-- 窗口只证明 pane 仍持有 TSP 发出的 nonce；数据库内容不经过这里。
local bootstrap = require("./bootstrap")
local document_preview = require("./document_preview")
local updater = require("./updater")
local M = {}
local busy = false
local seen = {}
local editor_attempts = {}
local candidates = {}
local active = true
local timer = nil
local round = 0
local bridge_child = nil
local bridge_round = nil
local bridge_timeout = nil
local pending = nil
local pending_navigation = {}
local update_poll_timer = nil
local update_status_raw = nil
local update_status_action = nil
local update_status_seen = {}
local english = false
local glimpsed = {}
local update_attempts = {}
local bound_panes = {}
local bound_nonces = {}
local apply_roots = {}
local apply_checks = {}

local request_prefix, request_suffix = '{"requests":[', "]}"
local request_overhead = #request_prefix + #request_suffix
local max_request_bytes, max_request_items = 65536, 128

local function find_marker(value)
  if type(value) == "string" then
    local encoded = value:match("^datapier%-window%-bridge:(.+)$")
    if encoded then
      local ok, marker = pcall(tern.json.decode, encoded)
      if
        ok
        and type(marker) == "table"
        and type(marker.nonce) == "string"
        and #marker.nonce == 36
        and type(marker.data_dir) == "string"
        and marker.data_dir ~= ""
      then
        return marker
      end
    end
  elseif type(value) == "table" then
    for _, child in pairs(value) do
      local nonce = find_marker(child)
      if nonce then
        return nonce
      end
    end
  end
  return nil
end

local function find_node(node)
  if type(node) ~= "table" then
    return nil
  end
  if node.id ~= nil and node.p then
    local marker = find_marker(node.p)
    if marker then
      return {
        id = node.id,
        nonce = marker.nonce,
        data_dir = marker.data_dir,
        editor_handoff = marker.editor_handoff,
        open_file = marker.open_file,
        project_summary = marker.project_summary,
        plugin_request = marker.plugin_request,
        english = marker.english == true,
      }
    end
  end
  if type(node.c) == "table" then
    for _, child in ipairs(node.c) do
      local found = find_node(child)
      if found then
        return found
      end
    end
  end
  return nil
end

local function bridge_node(cx, pane)
  local info = cx.session:pane(pane)
  if not info or info.exited then
    return nil
  end
  local ok, surface = pcall(function()
    -- 窗口证明只读取根部标记，不遍历可能超过快照预算的查询结果。
    return cx.session:surface(pane, { as = "table", depth = 2, nodes = 8, max_chars = 64000 })
  end)
  if not ok or type(surface) ~= "table" then
    return nil
  end
  return find_node(surface.main) or find_node(surface.dock) or find_node(surface.layer)
end

local function launch_editor(cx, pane, node)
  local handoff = node.editor_handoff
  if type(handoff) ~= "string" or #handoff ~= 36 or handoff:find("[^%x%-]") then
    return
  end
  local key = tostring(pane) .. ":" .. node.nonce
  local previous = editor_attempts[key]
  if previous and previous.handoff == handoff then
    if previous.pending then
      return
    end
    pcall(function()
      cx.session:event(pane, {
        ev = "action",
        id = node.id,
        act = (if previous.ok then "datapier-editor-launched:" else "datapier-editor-failed:") .. handoff,
      })
    end)
    return
  end
  local attempt = { handoff = handoff, pending = true, ok = false }
  editor_attempts[key] = attempt
  bootstrap.prepare(cx, function(fresh, directory)
    local current = bridge_node(fresh, pane)
    if
      editor_attempts[key] ~= attempt
      or not current
      or current.nonce ~= node.nonce
      or current.editor_handoff ~= handoff
    then
      return
    end
    local ok, problem = false, if node.english then "Datapier preparation failed" else "Datapier 准备失败"
    if directory then
      ok, problem = pcall(function()
        local program = if tern.runtime.os == "windows" then ".\\datapier.exe" else "./datapier"
        fresh.hosts:terminal("local", {
          cwd = directory,
          command = program .. " value-editor --handoff " .. handoff .. " --data-dir-b64 " .. tern.base64.encode(
            node.data_dir
          ),
        })
      end)
    end
    attempt.pending = false
    attempt.ok = ok
    if not ok then
      bootstrap.invalidate()
      fresh:toast(
        "error",
        if node.english then "Datapier editor launch failed" else "Datapier 编辑器启动失败",
        tostring(problem)
      )
    end
    pcall(function()
      fresh.session:event(pane, {
        ev = "action",
        id = current.id,
        act = (if ok then "datapier-editor-launched:" else "datapier-editor-failed:") .. handoff,
      })
    end)
  end, true)
end

local function notify(cx, pane, node, notifications)
  if type(notifications) ~= "table" then
    return
  end
  local key = tostring(pane) .. ":" .. node.nonce
  local previous = seen[key] or {}
  local current = {}
  for _, item in ipairs(notifications) do
    if type(item) == "table" then
      local id, message
      if item.kind == "approval" and type(item.approval_id) == "string" then
        id, message =
          "approval:" .. item.approval_id,
          if node.english then "A command is awaiting approval" else "有命令等待审批"
      elseif item.kind == "authentication" and type(item.request_id) == "string" then
        id, message =
          "authentication:" .. item.request_id,
          if node.english then "A connection is awaiting authentication" else "有连接等待认证"
      end
      if id then
        current[id] = true
        if not previous[id] then
          cx:toast("info", "Datapier", message)
        end
      end
    end
  end
  seen[key] = current
end

local function refresh_pending()
  local approvals, authentication, unique = 0, 0, {}
  for _, items in pairs(seen) do
    for id in pairs(items) do
      if not unique[id] then
        unique[id] = true
        if id:sub(1, 9) == "approval:" then
          approvals += 1
        else
          authentication += 1
        end
      end
    end
  end
  pending = if next(seen) then { approvals = approvals, authentication = authentication, english = english } else nil
  tern.chrome.refresh()
end

function M.pending_counts()
  return pending
end

local function poll_update_status(cx)
  if not active then
    return
  end
  if next(bound_panes) then
    local ok, raw = pcall(tern.fs.read, tern.plugin.data .. "/update-state.json", 65536)
    if ok and raw ~= update_status_raw then
      local decoded, state = pcall(tern.json.decode, raw)
      if decoded and type(state) == "table" and type(state.progress_revision) == "number" then
        update_status_raw = raw
        update_status_seen = {}
        update_status_action = "datapier-plugin-update-status:"
          .. tern.base64.encode(tern.json.encode({
            state = state.state,
            message = state.message,
            target_version = state.target_version,
            progress = state.progress,
            progress_revision = state.progress_revision,
            download_source = state.download_source,
          }))
      end
    end
    if update_status_action then
      for pane in pairs(bound_panes) do
        local node = bridge_node(cx, pane)
        if node and update_status_seen[pane] ~= node.nonce then
          local sent = pcall(function()
            cx.session:event(pane, { ev = "action", id = node.id, act = update_status_action })
          end)
          if sent then
            update_status_seen[pane] = node.nonce
          end
        end
      end
    end
  end
  update_poll_timer = tern.timer(if next(bound_panes) then 750 else 5000, poll_update_status)
end

local function handle_update(cx, pane, node)
  local request = node.plugin_request
  if
    type(request) ~= "table"
    or request.kind ~= "updates"
    or type(request.id) ~= "string"
    or type(request.action) ~= "string"
  then
    return
  end
  local key = tostring(pane) .. ":" .. node.nonce .. ":" .. request.id
  if update_attempts[key] then
    return
  end
  update_attempts[key] = true
  local function reply(fresh, state, problem)
    local current = bridge_node(fresh, pane)
    if
      not active
      or not current
      or current.nonce ~= node.nonce
      or type(current.plugin_request) ~= "table"
      or current.plugin_request.id ~= request.id
    then
      return
    end
    local value = if state then table.clone(state) else {}
    local read, policy = pcall(updater.policy)
    value.settings = if read then policy else nil
    local failure = problem
      or (if not read then tostring(policy) else nil)
      or (state and state.ok == false and state.message)
    local payload =
      { id = request.id, ok = not failure and (not state or state.ok == true), value = value, error = failure }
    fresh.session:event(pane, {
      ev = "action",
      id = current.id,
      act = "datapier-plugin-reply:" .. tern.base64.encode(tern.json.encode(payload)),
    })
  end
  if request.action == "configure" then
    local settings = request.settings
    if
      type(settings) ~= "table"
      or type(settings.auto_check) ~= "boolean"
      or type(settings.auto_download) ~= "boolean"
      or type(settings.auto_apply) ~= "boolean"
    then
      reply(cx, nil, "更新设置无效")
      return
    end
    if settings.auto_apply and not settings.auto_download then
      reply(cx, nil, "自动应用需要先启用自动下载")
      return
    end
    local saved, problem = pcall(tern.kv.set, "update_policy", {
      auto_check = settings.auto_check,
      auto_download = settings.auto_download,
      auto_apply = settings.auto_apply,
    })
    reply(cx, nil, if saved then nil else tostring(problem))
  elseif
    request.action == "status"
    or request.action == "reload"
    or request.action == "check"
    or request.action == "download"
    or request.action == "apply"
  then
    local action = if request.action == "apply"
      then "schedule"
      elseif request.action == "reload" then "status"
      else request.action
    updater.run(cx, bootstrap.executable(), action, request.action == "check", reply, node.data_dir)
  else
    reply(cx, nil, "未知更新操作")
  end
end

local function glimpse(cx, pane, node)
  local request = node.open_file
  if
    type(request) ~= "table"
    or type(request.id) ~= "string"
    or type(request.path) ~= "string"
    or request.host ~= "local"
  then
    return
  end
  if request.how and cx.session:focused() ~= pane then
    return
  end
  local key = tostring(pane) .. ":" .. node.nonce .. ":" .. request.id
  if glimpsed[key] then
    return
  end
  glimpsed[key] = true
  local ok, problem = pcall(function()
    if request.how == "beside" or request.how == "over" then
      document_preview.open(cx, pane, request.path, request.how, node.english)
    else
      cx:open(request.path, "glimpse")
    end
  end)
  if not ok then
    cx:toast("error", if node.english then "Datapier preview failed" else "Datapier 预览失败", tostring(problem))
  end
  cx.session:event(pane, {
    ev = "action",
    id = node.id,
    act = (if ok then "datapier-glimpse-opened:" else "datapier-glimpse-failed:") .. request.id,
  })
end

local function forget_pane(pane)
  local data_dir = bound_panes[pane]
  bound_panes[pane] = nil
  bound_nonces[pane] = nil
  update_status_seen[pane] = nil
  local prefix = tostring(pane) .. ":"
  for _, entries in ipairs({ seen, editor_attempts, glimpsed, update_attempts }) do
    for key in pairs(entries) do
      if key:sub(1, #prefix) == prefix then
        entries[key] = nil
      end
    end
  end
  return data_dir
end

-- Ctrl+Q 返回 shell 也算业务程序退出；是否可安装仍由原生协调器复核。
local function apply_after_exit(cx, data_dir)
  if not active or next(bound_panes) or not bootstrap.executable() or apply_checks[data_dir] then
    return
  end
  apply_checks[data_dir] = true
  apply_roots[data_dir] = (M.ticks or 0) + 12
  updater.run(cx, bootstrap.executable(), "status", false, function(fresh, state)
    if
      not state
      or not state.ok
      or not state.prepared
      or not (state.apply_after_exit or updater.policy().auto_apply)
    then
      apply_roots[data_dir] = nil
      apply_checks[data_dir] = nil
      return
    end
    if state.gate_blocked then
      apply_checks[data_dir] = nil
      return
    end
    updater.run(fresh, bootstrap.executable(), "apply", false, function(later, result, problem)
      apply_checks[data_dir] = nil
      if result and result.state == "installed" then
        apply_roots[data_dir] = nil
      end
      if not result or not result.ok then
        later:toast("error", "Datapier 更新未应用", tostring(problem or (result and result.message)))
      end
    end, data_dir)
  end, data_dir)
end

local function cancel_bridge_timeout()
  if bridge_timeout then
    bridge_timeout:cancel()
    bridge_timeout = nil
  end
end

local function finish_round()
  cancel_bridge_timeout()
  bridge_round = nil
  busy = false
end

local function stop_bridge()
  local child = bridge_child
  bridge_child = nil
  finish_round()
  if child then
    pcall(function()
      child:kill("kill")
    end)
  end
end

local function consume_batch(cx, response, nodes)
  for _, item in ipairs(response.items) do
    local entry = nodes[item.pane_id]
    local current = entry and bridge_node(cx, entry.pane)
    if current and current.nonce == entry.node.nonce and current.data_dir == entry.node.data_dir then
      if item.bound == true and item.nonce == current.nonce then
        bound_panes[entry.pane] = current.data_dir
        bound_nonces[entry.pane] = current.nonce
        english = current.english
        local navigation = pending_navigation[entry.pane]
        if navigation then
          pending_navigation[entry.pane] = nil
          local info = cx.session:pane(entry.pane)
          if navigation.tab and (not info or info.tab ~= navigation.tab) then
            cx:toast("error", "Datapier", "工作台已离开原标签页 / The workbench has left the original tab")
          elseif tern.now() <= navigation.until_ms then
            cx.session:event(entry.pane, { ev = "action", id = current.id, act = navigation.action })
          end
        end
        notify(cx, entry.pane, current, item.notifications)
        launch_editor(cx, entry.pane, current)
        glimpse(cx, entry.pane, current)
        handle_update(cx, entry.pane, current)
      else
        bound_panes[entry.pane] = nil
        bound_nonces[entry.pane] = nil
        seen[tostring(entry.pane) .. ":" .. current.nonce] = nil
      end
    end
  end
  refresh_pending()
end

local function send_batch(cx)
  local state = bridge_round
  local batch = state.batches[state.index]
  if not batch then
    finish_round()
    return
  end
  local sent, problem = pcall(function()
    bridge_child:write(request_prefix .. table.concat(batch.parts, ",") .. request_suffix .. "\n")
    bridge_timeout = tern.timer(10000, function()
      if active and bridge_round == state and state.batches[state.index] == batch then
        stop_bridge()
        tern.log.warn("Datapier 窗口桥接请求超时")
      end
    end)
  end)
  if not sent then
    stop_bridge()
    cx:toast("error", "Datapier 窗口桥接失败", tostring(problem))
  end
end

local function start_bridge()
  local child
  child = tern.process.spawn({ bootstrap.executable(), "tsp", "--window-bridge", "--bridge-stdin" }, {
    stdout = function(line, cx)
      if not active or bridge_child ~= child then
        return
      end
      local state = bridge_round
      local batch = state and state.batches[state.index]
      local ok, response = pcall(tern.json.decode, line)
      if not batch or not ok or type(response) ~= "table" or type(response.items) ~= "table" then
        stop_bridge()
        tern.log.warn("Datapier 窗口桥接回执无效")
        return
      end
      consume_batch(cx, response, batch.nodes)
      cancel_bridge_timeout()
      state.index += 1
      send_batch(cx)
    end,
    exit = function(result)
      if bridge_child == child then
        bridge_child = nil
        finish_round()
        tern.log.warn("Datapier 窗口桥接已退出: " .. tostring(result.stderr))
      end
    end,
  })
  return child
end

local function sync(cx)
  if busy or not active then
    return
  end
  local batches, eligible = {}, {}
  local batch, size
  local window_id = cx:window_key()
  for pane in pairs(candidates) do
    local node = bridge_node(cx, pane)
    if node then
      local pane_id = tostring(pane)
      local part =
        tern.json.encode({ pane_id = pane_id, nonce = node.nonce, data_dir = node.data_dir, window_id = window_id })
      if #part + request_overhead > max_request_bytes then
        tern.log.warn("Datapier pane " .. pane_id .. " 的窗口桥接请求超过 64 KiB")
      else
        local sent = pcall(function()
          cx.session:event(pane, { ev = "action", id = node.id, act = "datapier-window-bind" })
        end)
        if sent then
          -- 按实际 JSON 字节计数，包含外层对象与条目间逗号，换行不计入 Rust 帧长。
          if not batch or #batch.parts == max_request_items or size + 1 + #part > max_request_bytes then
            batch = { parts = {}, nodes = {} }
            table.insert(batches, batch)
            size = request_overhead
          else
            size += 1
          end
          table.insert(batch.parts, part)
          batch.nodes[pane_id] = { pane = pane, node = node }
          eligible[pane] = true
          size += #part
        end
      end
    end
  end
  local removed = false
  for pane in pairs(bound_panes) do
    if not eligible[pane] then
      local data_dir = forget_pane(pane)
      apply_roots[data_dir] = 0
      removed = true
    end
  end
  if removed then
    refresh_pending()
  end
  if #batches == 0 then
    stop_bridge()
    return
  end
  busy = true
  round += 1
  local generation = round
  bootstrap.prepare(cx, function(fresh, directory)
    if not active or generation ~= round then
      return
    end
    if not directory then
      busy = false
      return
    end
    local started, problem = pcall(function()
      bridge_child = bridge_child or start_bridge()
    end)
    if not started then
      stop_bridge()
      fresh:toast("error", "Datapier 窗口桥接失败", tostring(problem))
      return
    end
    -- 每批收到回执后才推进，整个对账轮次共用同一个子进程。
    bridge_round = { batches = batches, index = 1 }
    send_batch(fresh)
  end, true)
end

local function tick(cx)
  if not active then
    return
  end
  if not M.ticks then
    local managed = tern.kv.get("mcp_managed")
    if type(managed) == "table" and type(managed.data_dir) == "string" then
      apply_roots[managed.data_dir] = 0
    end
  end
  -- 事件维护候选集合；低频全量对账修复 pane 迁移和遗漏事件。
  M.ticks = (M.ticks or 0) + 1
  if M.ticks % 12 == 1 then
    local found = {}
    for _, info in ipairs(cx.session:panes()) do
      if info.pane then
        found[info.pane] = true
      end
    end
    candidates = found
  end
  sync(cx)
  for data_dir, next_tick in pairs(apply_roots) do
    if M.ticks >= next_tick then
      apply_after_exit(cx, data_dir)
    end
  end
  timer = tern.timer(5000, tick)
end

local function clear_tab_navigation(tab)
  if tab then
    for pane, navigation in pairs(pending_navigation) do
      if navigation.tab == tab then
        pending_navigation[pane] = nil
      end
    end
  end
end

function M.navigate_when_ready(pane, action, tab)
  if type(pane) == "number" then
    clear_tab_navigation(tab)
    pending_navigation[pane] = { action = action, tab = tab, until_ms = tern.now() + 30000 }
    candidates[pane] = true
  end
end

function M.navigate(cx, action, tab)
  local eligible = {}
  if tab then
    for _, info in ipairs(cx.session:panes()) do
      if info.tab == tab then
        eligible[info.pane] = true
      end
    end
  end
  local focused = cx.session:focused()
  local pane = focused and (not tab or eligible[focused]) and bridge_node(cx, focused) and focused or nil
  if not pane then
    for candidate in pairs(candidates) do
      if (not tab or eligible[candidate]) and bridge_node(cx, candidate) then
        pane = candidate
        break
      end
    end
  end
  if not pane then
    return false
  end
  local node = bridge_node(cx, pane)
  cx.layout:focus(pane)
  clear_tab_navigation(tab)
  if bound_panes[pane] and bound_nonces[pane] == node.nonce then
    pending_navigation[pane] = nil
    cx.session:event(pane, { ev = "action", id = node.id, act = action })
  else
    M.navigate_when_ready(pane, action, tab)
  end
  return true
end

function M.summary(cx)
  local pane = cx.session:focused()
  local node = pane and bridge_node(cx, pane)
  return if node and type(node.project_summary) == "string" then node.project_summary else "当前没有项目摘要"
end

function M.start()
  timer = tern.timer(0, tick)
  update_poll_timer = tern.timer(0, poll_update_status)
  document_preview.start()
  tern.on("pane_created", function(ev, cx)
    candidates[ev.pane] = true
    sync(cx)
  end)
  tern.on("pane_closed", function(ev)
    candidates[ev.pane] = nil
    pending_navigation[ev.pane] = nil
    local data_dir = forget_pane(ev.pane)
    if data_dir then
      apply_roots[data_dir] = 0
      tern.timer(2000, function(fresh)
        apply_after_exit(fresh, data_dir)
      end)
    end
    refresh_pending()
  end)
  tern.on("unload", function()
    active = false
    round += 1
    stop_bridge()
    if update_poll_timer then
      update_poll_timer:cancel()
    end
    if timer then
      timer:cancel()
    end
  end)
end

return M
