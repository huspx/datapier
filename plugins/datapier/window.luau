local bootstrap = require("./lib/bootstrap")
local window_bridge = require("./lib/window_bridge")

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
    local payload = tern.base64.encode(tern.json.encode({ host = "localhost", path = files.root }))
    if not window_bridge.navigate(cx, "datapier-project-path:" .. payload) then
      cx:toast(
        "info",
        "Datapier",
        if english
          then "Open the workbench first, then select the directory again"
          else "请先打开工作台，然后再次选择目录"
      )
      bootstrap.start(cx)
    end
  end,
})

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
