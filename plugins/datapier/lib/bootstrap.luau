-- 薄启动入口：窗口只调度安装和终端启动，数据库状态与界面逻辑均由 Rust 持有。

local updater = require("./updater")
local registration = require("./mcp_registration")
local M = {}
local verified_directory = nil
local waiting = nil
local active = true
local generation = 0
local gate_timer = nil
local registered = nil
local registering = false

local function absolute(path)
  if type(path) ~= "string" or path:find("\0", 1, true) or not utf8.len(path) then
    return false
  end
  if tern.runtime.os == "windows" then
    return path:match("^%a:[/\\]") ~= nil or path:match("^\\\\[^\\]+\\[^\\]+") ~= nil
  end
  return path:sub(1, 1) == "/"
end

local function quote(value)
  return "'" .. value:gsub("'", "'\\''") .. "'"
end

local function powershell_quote(value)
  return "'" .. value:gsub("'", "''") .. "'"
end

local function encoded_powershell(script)
  local units = {}
  for _, code in utf8.codes(script) do
    if code >= 0x10000 then
      code -= 0x10000
      local high = 0xD800 + math.floor(code / 0x400)
      table.insert(units, string.char(high % 256, math.floor(high / 256)))
      code = 0xDC00 + code % 0x400
    end
    table.insert(units, string.char(code % 256, math.floor(code / 256)))
  end
  return tern.base64.encode(table.concat(units))
end

local function powershell()
  local root = tern.getenv("SystemRoot")
  if not absolute(root) or not root:match("^%a:[/\\][%w%._/\\%-]+$") then
    error("无法确认 Windows 系统目录")
  end
  return root .. "\\System32\\WindowsPowerShell\\v1.0\\powershell.exe"
end

function M.valid_directory(path, callback)
  if not absolute(path) then
    callback(nil, false)
    return
  end
  local ok, problem = pcall(function()
    local argv
    if tern.runtime.os == "windows" then
      local script = "if (Test-Path -LiteralPath "
        .. powershell_quote(path)
        .. " -PathType Container) { exit 0 } else { exit 1 }"
      argv = { powershell(), "-NoProfile", "-NonInteractive", "-EncodedCommand", encoded_powershell(script) }
    else
      argv = { "/bin/test", "-d", path }
    end
    tern.process.run(argv, { timeout_ms = 10000 }, function(result, fresh)
      callback(fresh, result.status == 0)
    end)
  end)
  if not ok then
    tern.log.warn("Datapier 目录检查失败: " .. tostring(problem))
    callback(nil, false)
  end
end

function M.source(cx)
  local pane = cx.session:focused()
  if not pane then
    return nil
  end
  local local_pane = false
  for _, host in ipairs(cx.hosts:list()) do
    if host.kind == "local" then
      for _, id in ipairs(host.panes) do
        if id == pane then
          local_pane = true
        end
      end
    end
  end
  if not local_pane then
    return nil
  end
  for _, info in ipairs(cx.session:panes()) do
    if info.pane == pane and info.tab then
      return info
    end
  end
  return nil
end

local function command(binary, data_dir, cwd)
  if not absolute(binary) or not absolute(data_dir) or (cwd and not absolute(cwd)) then
    error("启动路径必须是绝对 UTF-8 路径")
  end
  local args = { binary, "--data-dir-b64", tern.base64.encode(data_dir), "tsp", "--plugin-cache-engine" }
  if cwd then
    table.insert(args, "--query-base-b64")
    table.insert(args, tern.base64.encode(cwd))
  end
  if tern.runtime.os == "windows" then
    local quoted = {}
    for _, arg in ipairs(args) do
      table.insert(quoted, powershell_quote(arg))
    end
    local script = "& " .. table.concat(quoted, " ") .. "; exit $LASTEXITCODE"
    return powershell() .. " -NoProfile -NonInteractive -EncodedCommand " .. encoded_powershell(script)
  end
  local quoted = {}
  for _, arg in ipairs(args) do
    table.insert(quoted, quote(arg))
  end
  return table.concat(quoted, " ")
end

function M.executable()
  if not verified_directory then
    return nil
  end
  return verified_directory .. (if tern.runtime.os == "windows" then "\\datapier.exe" else "/datapier")
end

function M.invalidate()
  verified_directory = nil
  tern.kv.set("binary", "")
end

local function finish(cx, directory)
  verified_directory = directory
  if directory then
    tern.kv.set("binary", M.executable())
  else
    M.invalidate()
  end
  local callbacks = waiting
  waiting = nil
  if callbacks then
    for _, callback in ipairs(callbacks) do
      callback(cx, directory)
    end
  end
end

function M.prepare(cx, ready, quiet)
  if not active then
    return
  end
  if verified_directory then
    ready(cx, verified_directory)
    return
  end
  if waiting then
    table.insert(waiting, ready)
    return
  end
  waiting = { ready }
  generation += 1
  local request_generation = generation
  local os = tern.runtime.os
  local argv
  -- runtime 只保证提供 OS；硬件架构识别、摘要验证和缓存发布交给原生安装脚本。
  if os == "windows" then
    argv = {
      "powershell.exe",
      "-NoProfile",
      "-NonInteractive",
      "-ExecutionPolicy",
      "Bypass",
      "-File",
      tern.plugin.dir .. "/install/windows.ps1",
    }
  elseif os == "macos" or os == "linux" then
    argv = { "/bin/sh", tern.plugin.dir .. "/install/unix.sh" }
  else
    cx:toast("error", "Datapier 无法启动", "此平台不支持本机程序")
    finish(cx, nil)
    return
  end
  if not quiet then
    cx:toast("info", "正在准备 Datapier")
  end
  -- 下载和解包放入异步子进程，既不阻塞窗口，也不受 Luau fetch 响应体大小限制。
  local ok, problem = pcall(function()
    tern.process.run(argv, { timeout_ms = 300000 }, function(result, fresh)
      if not active or request_generation ~= generation then
        return
      end
      local decoded, data = pcall(tern.json.decode, result.stdout)
      if result.status ~= 0 or not decoded or type(data) ~= "table" or not data.ok then
        local message = if decoded and type(data) == "table" then data.error else result.stderr
        fresh:toast("error", "Datapier 安装失败", tostring(message or "安装脚本返回无效结果"))
        finish(fresh, nil)
        return
      end
      if type(data.cwd) ~= "string" or data.cwd == "" then
        fresh:toast("error", "Datapier 安装失败", "安装脚本返回无效缓存目录")
        finish(fresh, nil)
        return
      end
      finish(fresh, data.cwd)
    end)
  end)
  if not ok then
    cx:toast("error", "Datapier 安装失败", tostring(problem))
    finish(cx, nil)
  end
end

local shell_names = {
  sh = true,
  bash = true,
  zsh = true,
  dash = true,
  ksh = true,
  fish = true,
  nu = true,
  elvish = true,
  xonsh = true,
  ["cmd.exe"] = true,
  ["powershell.exe"] = true,
  ["pwsh.exe"] = true,
  pwsh = true,
}

local function shell_directory(cx, source, callback)
  if not source or source.kind ~= "terminal" or source.at_prompt ~= true or not absolute(source.cwd) then
    callback(cx, nil)
    return
  end
  local ok = pcall(function()
    tern.process.run(
      { "tern", "process", tostring(source.pane), "--json" },
      { timeout_ms = 5000 },
      function(result, fresh)
        local decoded, data = pcall(tern.json.decode, result.stdout)
        local child = decoded and type(data) == "table" and data.child or nil
        local foreground = decoded and type(data) == "table" and data.foreground or nil
        local name = type(child) == "table" and type(child.name) == "string" and child.name:lower() or ""
        local valid = result.status == 0
          and decoded
          and type(data) == "table"
          and data.pane == source.pane
          and type(child) == "table"
          and type(foreground) == "table"
          and type(child.pid) == "number"
          and child.pid > 0
          and child.pid == foreground.pid
          and shell_names[name] == true
          and child.cwd == source.cwd
          and foreground.cwd == source.cwd
        callback(fresh, if valid then source.cwd else nil)
      end
    )
  end)
  if not ok then
    callback(cx, nil)
  end
end

local function register_mcp(cx, binary, data_dir, finished)
  local function finish(window)
    if finished then
      finished(window, binary)
    end
  end
  if registering or (registered and registered.binary == binary and registered.data_dir == data_dir) then
    finish(cx)
    return
  end
  if not registration.local_scope(cx) then
    finish(cx)
    return
  end
  registering = true
  local function register(window, command, problem)
    registering = false
    if command then
      local ok, reason = registration.register(window, command, data_dir)
      if ok then
        registered = { binary = binary, data_dir = data_dir }
      else
        problem = reason
      end
    end
    if problem then
      window:toast("error", "Datapier MCP 登记失败", tostring(problem))
    end
    finish(window)
  end
  if tern.getenv("DATAPIER_BINARY") then
    register(cx, binary)
  else
    updater.run(cx, binary, "managed-entry", false, function(done, state, failure)
      if state and state.ok and type(state.command) == "string" then
        register(done, state.command)
      else
        register(done, nil, failure or (state and state.message) or "受管入口准备失败")
      end
    end, data_dir)
  end
end

function M.start(cx, started_pane, options)
  options = options or {}
  local source = M.source(cx)
  local inherited = source and (source.kind == "file" or source.kind == "git") and source.cwd or nil
  local function prepare(initial, cwd)
    M.prepare(initial, function(fresh, directory)
      if not directory then
        if started_pane then
          started_pane(nil)
        end
        return
      end
      updater.allowed(fresh, M.executable(), function(checked, allowed, problem, _, data_dir)
        if not allowed then
          checked:toast("error", "Datapier 暂时无法启动", tostring(problem))
          if started_pane then
            started_pane(nil)
          end
          return
        end
        if options.before_launch and not options.before_launch(checked) then
          if started_pane then
            started_pane(nil)
          end
          return
        end
        local started, pane = pcall(function()
          local launch = { cwd = cwd or directory, command = command(M.executable(), data_dir, cwd) }
          if options.target then
            return checked.layout:split(options.target, "right", launch)
          end
          return checked.hosts:terminal("local", launch)
        end)
        if not started or not pane then
          checked:toast(
            "error",
            "Datapier 启动失败 / Failed to start Datapier",
            tostring(pane or "无法创建分屏 / Cannot create split")
          )
          if started_pane then
            started_pane(nil)
          end
        else
          if started_pane then
            started_pane(pane)
          end
          register_mcp(checked, M.executable(), data_dir)
        end
      end)
    end)
  end
  local function validate(initial, requested)
    if requested then
      M.valid_directory(requested, function(fresh, valid)
        if options.cwd and not valid then
          (fresh or initial):toast("error", "Datapier", "目录无效 / The directory is invalid")
          if started_pane then
            started_pane(nil)
          end
          return
        end
        prepare(fresh or initial, if valid then requested else nil)
      end)
    else
      prepare(initial, nil)
    end
  end
  if options.cwd or inherited then
    validate(cx, options.cwd or inherited)
  else
    shell_directory(cx, source, validate)
  end
end

function M.command_entry(cx, remove)
  M.prepare(cx, function(fresh, directory)
    if not directory then
      return
    end
    updater.run(
      fresh,
      M.executable(),
      if remove then "entry-remove" else "entry-install",
      false,
      function(done, value, problem)
        if not value or not value.ok then
          done:toast(
            "error",
            "Datapier 命令行入口 / Datapier command entry",
            tostring(problem or (value and value.message) or "操作失败 / Action failed")
          )
        elseif remove then
          done:toast("info", "已移除命令行入口 / Command entry removed", "Datapier")
        else
          done:copy(value.command)
          done:toast("info", "已复制 Datapier MCP 路径 / Datapier MCP path copied", value.command)
        end
      end
    )
  end)
end

function M.initialize(cx)
  M.prepare(cx, function(fresh, directory)
    if not directory then
      return
    end
    updater.allowed(fresh, M.executable(), function(checked, allowed, problem, blocked, data_dir)
      if not allowed then
        if blocked then
          -- 新 VM 的 status 可确认加载，但发布必须等原生协调器提交或回滚。
          gate_timer = tern.timer(2000, function(next_cx)
            if active then
              M.initialize(next_cx)
            end
          end)
        else
          checked:toast("error", "Datapier 更新门禁", tostring(problem))
        end
        return
      end
      register_mcp(checked, M.executable(), data_dir, updater.schedule)
    end)
  end, true)
end

tern.on("unload", function()
  active = false
  generation += 1
  waiting = nil
  if gate_timer then
    gate_timer:cancel()
  end
end)

-- 旧 VM 的 callback 不可恢复；安装脚本完成后，新 VM 重新校验缓存。
tern.on("process_exit", function(_, cx)
  if active and not waiting and not verified_directory then
    M.initialize(cx)
  end
end)

return M
