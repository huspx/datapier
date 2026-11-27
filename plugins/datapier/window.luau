local bootstrap = require("./lib/bootstrap")
local window_bridge = require("./lib/window_bridge")

local opening = {}
local request_number = 0
local latest_valid = {}

local function source_in_tab(cx, pane, tab)
  local belongs = false
  for _, host in ipairs(cx.hosts:list()) do
    if host.kind == "local" then
      for _, id in ipairs(host.panes) do
        if id == pane then
          belongs = true
        end
      end
    end
  end
  if not belongs then
    return false
  end
  for _, info in ipairs(cx.session:panes()) do
    if info.pane == pane and info.tab == tab and info.exited == nil then
      return true
    end
  end
  return false
end

local function source_error(cx)
  cx:toast(
    "error",
    "Datapier",
    "来源 block 不可用，请返回本机 block 重试 / Source block is unavailable; return to a local block and try again"
  )
end
window_bridge.start()

tern.timer(0, function(cx)
  bootstrap.initialize(cx)
end)

local function open_todos(cx, section)
  local action = if section then "datapier-open-todos:" .. section else "datapier-open-todos"
  if not window_bridge.navigate(cx, action) then
    bootstrap.start(cx, function(pane)
      window_bridge.navigate_when_ready(pane, action)
    end)
  end
end

tern.command({
  id = "todos",
  title = "Datapier 待办 / Pending items",
  icon = "database",
  run = function(cx)
    open_todos(cx)
  end,
})
for _, section in ipairs({
  { id = "approvals", title = "Datapier 待审批 / Pending approvals" },
  { id = "authentication", title = "Datapier 待认证 / Pending authentication" },
}) do
  tern.command({
    id = section.id,
    title = section.title,
    icon = "database",
    run = function(cx)
      open_todos(cx, section.id)
    end,
  })
end
tern.chrome.widget("todos", {
  name = "Datapier 待办 / Pending items",
  description = "待审批与待认证 / Pending approvals and authentication",
  icon = "database",
  render = function()
    local counts = window_bridge.pending_counts()
    local english = not counts or counts.english
    local unknown = if english then "Unknown" else "未知"
    local open = if english then "View" else "查看"
    return {
      text = if counts then tostring(counts.approvals + counts.authentication) else "—",
      command = "plugin.datapier.todos",
      card = {
        {
          id = "approvals",
          label = if english then "Pending approvals" else "待审批",
          text = if counts then tostring(counts.approvals) else unknown,
          actions = { { id = "view", label = open } },
        },
        {
          id = "authentication",
          label = if english then "Pending authentication" else "待认证",
          text = if counts then tostring(counts.authentication) else unknown,
          actions = { { id = "view", label = open } },
        },
      },
    }
  end,
})
tern.on("widget_action", function(ev, cx)
  if ev.widget == "todos" and ev.act == "view" and (ev.row == "approvals" or ev.row == "authentication") then
    open_todos(cx, ev.row)
  end
end)

tern.files.button({
  id = "projects",
  title = "Datapier MCP 项目 / MCP projects",
  icon = "database",
  run = function(files, cx)
    local counts = window_bridge.pending_counts()
    local english = not counts or counts.english
    local local_host = false
    for _, host in ipairs(cx.hosts:list()) do
      if host.name == files.host and host.kind == "local" then
        local_host = true
        break
      end
    end
    if not local_host or type(files.root) ~= "string" or files.root == "" then
      cx:toast(
        "error",
        "Datapier",
        if english
          then "Cannot map the current Files directory to this computer"
          else "无法将当前 Files 目录映射到本机"
      )
      return
    end
    local source = bootstrap.source(cx)
    if not source then
      source_error(cx)
      return
    end
    local tab, pane, root = source.tab, source.pane, files.root
    request_number += 1
    local request = request_number
    latest_valid[tab] = request
    window_bridge.cancel_navigation(tab)
    bootstrap.valid_directory(root, function(fresh, valid)
      if request ~= latest_valid[tab] then
        return
      end
      if not fresh then
        return
      end
      if not source_in_tab(fresh, pane, tab) then
        source_error(fresh)
        return
      end
      local payload = tern.base64.encode(tern.json.encode({ host = "localhost", path = root }))
      local action = "datapier-project-path:" .. payload
      local pending = opening[tab]
      if
        pending
        and pending.pane
        and (tern.now() > pending.until_ms or not source_in_tab(fresh, pending.pane, tab))
      then
        opening[tab] = nil
        pending = nil
      end
      if pending then
        pending.action = action
        pending.request = request
        pending.target = pane
        pending.options.target = pane
        if pending.pane then
          if not window_bridge.navigate(fresh, action, tab, pane) then
            window_bridge.navigate_when_ready(pending.pane, action, tab, pane)
          end
        end
        return
      end
      if window_bridge.navigate(fresh, action, tab, pane) then
        return
      end
      pending = { action = action, target = pane, request = request }
      pending.options = {
        cwd = if valid then root else nil,
        target = pane,
        before_launch = function(checked)
          if pending.request ~= latest_valid[tab] then
            return false
          end
          if not source_in_tab(checked, pending.target, tab) then
            source_error(checked)
            return false
          end
          if window_bridge.navigate(checked, pending.action, tab, pending.target) then
            return false
          end
          return true
        end,
      }
      opening[tab] = pending
      bootstrap.start(fresh, function(started)
        if opening[tab] ~= pending then
          return
        end
        if not started then
          opening[tab] = nil
          return
        end
        pending.pane = started
        pending.until_ms = tern.now() + 30000
        window_bridge.navigate_when_ready(started, pending.action, tab, pending.target)
      end, pending.options)
    end)
  end,
})

tern.on("pane_closed", function(ev)
  for tab, pending in pairs(opening) do
    if pending.pane == ev.pane then
      opening[tab] = nil
    end
  end
end)

tern.carly.export("open", { sig = "open()", doc = "打开 Datapier 工作台" }, function(cx)
  bootstrap.start(cx)
  return "正在准备 Datapier 工作台"
end)
tern.carly.export("todos", { sig = "todos()", doc = "定位 Datapier 待办" }, function(cx)
  open_todos(cx)
  return "已请求定位 Datapier 待办"
end)
tern.carly.export("project", { sig = "project()", doc = "查看当前 Datapier 项目关联摘要" }, function(cx)
  return window_bridge.summary(cx)
end)

tern.command({
  id = "new-datapier",
  title = "New Datapier block",
  icon = "database",
  run = function(cx)
    bootstrap.start(cx)
  end,
})

for _, entry in ipairs({
  {
    id = "install-datapier-command",
    title = "安装 Datapier 命令行入口 / Install Datapier command entry",
    remove = false,
  },
  {
    id = "remove-datapier-command",
    title = "移除 Datapier 命令行入口 / Remove Datapier command entry",
    remove = true,
  },
}) do
  tern.command({
    id = entry.id,
    title = entry.title,
    icon = "database",
    run = function(cx)
      bootstrap.command_entry(cx, entry.remove)
    end,
  })
end
