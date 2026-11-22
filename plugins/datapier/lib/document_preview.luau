-- Canvas 仅预览用户明确选择的项目文档。标题、代码与链接由宿主 Markdown 渲染。
local M = {}
local ui = tern.ui
local split_key = "datapier-document-split"
local sizes_key = "document-preview-sizes"
local heading_action = "datapier-document-heading"

local function outline(markdown)
  local buttons, seen = {}, {}
  local fence, previous
  for line in (markdown .. "\n"):gmatch("([^\n]*)\n") do
    line = line:gsub("\r$", "")
    local ticks = line:match("^ ? ? ?(```+)") or line:match("^ ? ? ?(~~~+)")
    if ticks then
      if not fence then
        fence = ticks
      elseif ticks:sub(1, 1) == fence:sub(1, 1) and #ticks >= #fence and line:match("^[ `~\t]+$") then
        fence = nil
      end
      previous = nil
    elseif not fence then
      local marks, heading = line:match("^ ? ? ?(#+)[ \t]+(.+)$")
      if marks and #marks <= 6 then
        heading = heading:gsub("[ \t]+#+[ \t]*$", "")
      elseif previous and (line:match("^ ? ? ?=+[ \t]*$") or line:match("^ ? ? ?%-+[ \t]*$")) then
        heading = previous
      else
        heading = nil
      end
      if heading then
        heading = heading:match("^%s*(.-)%s*$")
        if #heading > 0 and #heading <= 240 and not seen[heading] then
          seen[heading] = true
          table.insert(
            buttons,
            ui.button(heading, {
              variant = "quiet",
              actions = { click = heading_action .. "=" .. heading },
            })
          )
          if #buttons == 48 then
            break
          end
        end
      end
      previous = if heading or line:match("^%s*$") then nil else line
    end
  end
  return buttons
end

function M.open(cx, pane, path, how, english)
  -- 宿主对单帧 Canvas 有 256 KiB 上限，文件读取也须有界。
  local markdown = tern.fs.read(path, 96 * 1024)
  if not utf8.len(markdown) then
    error(if english then "The Markdown file is not valid UTF-8" else "Markdown 文件不是有效 UTF-8")
  end
  local title = path:match("[^/\\]+$") or path
  local end_at = utf8.offset(title, 77)
  if end_at then
    title = title:sub(1, end_at - 1) .. "…"
  end
  local body = ui.md(markdown, { run = false, icons = true, cwd = path:match("^(.*)[/\\][^/\\]+$") })
  local buttons = outline(markdown)
  local view = body
  if #buttons > 0 then
    table.insert(buttons, 1, ui.text(if english then "Contents" else "目录"))
    view = ui.node("split", { key = split_key, sizes = tern.kv.get(sizes_key) or { 0.25, 0.75 } }, {
      ui.node("scroll", {}, { ui.col(buttons) }),
      ui.node("scroll", {}, { body }),
    })
  end
  local spec = { title = title, view = view, how = how }
  if how == "beside" then
    spec.target = pane
    spec.ratio = 0.45
  end
  local opened, problem = cx.canvas:open(spec)
  if not opened then
    error(problem or (if english then "Could not open the documentation preview" else "无法打开文档预览"))
  end
end

function M.start()
  tern.on("canvas_action", function(ev, cx)
    if ev.act == heading_action and type(ev.value) == "string" then
      local ok, problem = cx.canvas:reveal(ev.pane, { heading = ev.value })
      if not ok then
        cx:toast("error", "Datapier", tostring(problem))
      end
      return true
    end
  end)
  tern.on("canvas_resized", function(ev, cx)
    local canvas = cx.canvas:get(ev.pane)
    if canvas and canvas.view and canvas.view.p and canvas.view.p.key == split_key and #ev.sizes == 2 then
      tern.kv.set(sizes_key, ev.sizes)
    end
  end)
end

return M
