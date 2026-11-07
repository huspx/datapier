-- 窗口只证明 pane 仍持有 TSP 发出的 nonce；数据库内容不经过这里。
local M = {}
local busy = false
local seen = {}

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
      return { id = node.id, nonce = marker.nonce, data_dir = marker.data_dir }
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

local function executable()
  local binary = tern.getenv("DATAPIER_BINARY") or tern.kv.get("binary")
  if type(binary) == "string" and binary ~= "" then
    return binary
  end
  return "datapier"
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
      { executable(), "--data-dir", node.data_dir, "tsp", "--window-bridge", "--bridge-payload", payload },
      { timeout_ms = 4000 },
      function(result, fresh)
        local current = bridge_node(fresh, pane)
        if current and current.nonce == node.nonce and result.status == 0 then
          local ok, response = pcall(tern.json.decode, result.stdout)
          if ok and type(response) == "table" and response.bound == true and response.pane_id == tostring(pane) then
            notify(fresh, pane, current, response.notifications)
          end
        end
        sync_panes(fresh, panes, index + 1)
      end
    )
  end)
  if not started then
    sync_panes(cx, panes, index + 1)
  end
end

local function tick(cx)
  if not busy then
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
  end)
end

return M
