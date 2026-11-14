-- 窗口只证明 pane 仍持有 TSP 发出的 nonce；数据库内容不经过这里。
local bootstrap = require("./bootstrap")
local M = {}
local busy = false
local seen = {}
local editor_attempts = {}
local preparation_failed = false
local round_verified = false

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
      return { id = node.id, nonce = marker.nonce, data_dir = marker.data_dir, editor_handoff = marker.editor_handoff }
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

-- 路径通过内部 Base64 参数传递，避免依赖用户默认 shell 的引用规则。
local function base64(text)
  local alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
  local parts = {}
  for index = 1, #text, 3 do
    local a, b, c = text:byte(index), text:byte(index + 1), text:byte(index + 2)
    local n = a * 65536 + (b or 0) * 256 + (c or 0)
    local first = math.floor(n / 262144) % 64 + 1
    local second = math.floor(n / 4096) % 64 + 1
    local third = math.floor(n / 64) % 64 + 1
    local fourth = n % 64 + 1
    table.insert(
      parts,
      alphabet:sub(first, first)
        .. alphabet:sub(second, second)
        .. (if b then alphabet:sub(third, third) else "=")
        .. (if c then alphabet:sub(fourth, fourth) else "=")
    )
  end
  return table.concat(parts)
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
    local ok, problem = false, "Datapier 准备失败"
    if directory then
      ok, problem = pcall(function()
        local program = if tern.runtime.os == "windows" then ".\\datapier.exe" else "./datapier"
        fresh.hosts:terminal("local", {
          cwd = directory,
          command = program .. " value-editor --handoff " .. handoff .. " --data-dir-b64 " .. base64(node.data_dir),
        })
      end)
    end
    attempt.pending = false
    attempt.ok = ok
    if not ok then
      bootstrap.invalidate()
      fresh:toast("error", "Datapier 编辑器启动失败", tostring(problem))
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
        id, message = "approval:" .. item.approval_id, "有命令等待审批"
      elseif item.kind == "authentication" and type(item.request_id) == "string" then
        id, message = "authentication:" .. item.request_id, "有连接等待认证"
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

local function sync_panes(cx, panes, index)
  local pane = panes[index]
  if not pane then
    busy = false
    return
  end
  local node = bridge_node(cx, pane)
  if not node then
    sync_panes(cx, panes, index + 1)
    return
  end
  -- 每轮仅在第一个有效 pane 前校验一次，所有桥接进程共用结果。
  if preparation_failed then
    sync_panes(cx, panes, index + 1)
    return
  end
  if not round_verified then
    bootstrap.prepare(cx, function(fresh, directory)
      round_verified = directory ~= nil
      preparation_failed = directory == nil
      sync_panes(fresh, panes, if directory then index else index + 1)
    end, true)
    return
  end
  local binary = bootstrap.executable()
  if not binary then
    preparation_failed = true
    sync_panes(cx, panes, index + 1)
    return
  end
  -- 只有对应 pane 的 action 处理器确认绑定后，Engine 才能续租。
  local sent = pcall(function()
    cx.session:event(pane, { ev = "action", id = node.id, act = "datapier-window-bind" })
  end)
  if not sent then
    sync_panes(cx, panes, index + 1)
    return
  end
  local payload = tern.json.encode({
    nonce = node.nonce,
    window_id = cx:window_key(),
    pane_id = tostring(pane),
  })
  local started = pcall(function()
    tern.process.run(
      { binary, "--data-dir", node.data_dir, "tsp", "--window-bridge", "--bridge-payload", payload },
      { timeout_ms = 4000 },
      function(result, fresh)
        local current = bridge_node(fresh, pane)
        if result.status ~= 0 then
          bootstrap.invalidate()
          preparation_failed = true
        end
        if current and current.nonce == node.nonce and result.status == 0 then
          local ok, response = pcall(tern.json.decode, result.stdout)
          if ok and type(response) == "table" and response.bound == true and response.pane_id == tostring(pane) then
            notify(fresh, pane, current, response.notifications)
            launch_editor(fresh, pane, current)
          end
        end
        sync_panes(fresh, panes, index + 1)
      end
    )
  end)
  if not started then
    bootstrap.invalidate()
    preparation_failed = true
    sync_panes(cx, panes, index + 1)
  end
end

local function tick(cx)
  if not busy then
    preparation_failed = false
    round_verified = false
    busy = true
    local panes = {}
    for _, pane in ipairs(cx.session:panes()) do
      if pane.pane then
        table.insert(panes, pane.pane)
      end
    end
    sync_panes(cx, panes, 1)
  end
  tern.timer(5000, tick)
end

function M.start()
  -- reload 不保证再次收到 window_start；timer 总能取得新上下文。
  tern.timer(0, tick)
  tern.on("pane_closed", function(ev)
    local prefix = tostring(ev.pane) .. ":"
    for key in pairs(seen) do
      if key:sub(1, #prefix) == prefix then
        seen[key] = nil
      end
    end
    for key in pairs(editor_attempts) do
      if key:sub(1, #prefix) == prefix then
        editor_attempts[key] = nil
      end
    end
  end)
end

return M
