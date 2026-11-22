-- 薄启动入口：窗口只调度安装和终端启动，数据库状态与界面逻辑均由 Rust 持有。

local updater = require("./updater")
local registration = require("./mcp_registration")
local M = {}
local verified_directory = nil
local waiting = nil
local active = true
local generation = 0
local gate_timer = nil

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

function M.start(cx, started_pane)
  M.prepare(cx, function(fresh, directory)
    if not directory then
      return
    end
    updater.allowed(fresh, M.executable(), function(checked, allowed, problem, _, data_dir)
      if not allowed then
        checked:toast("error", "Datapier 暂时无法启动", tostring(problem))
        return
      end
      local registered, reason = registration.register(checked, M.executable(), data_dir)
      if not registered then
        checked:toast("error", "Datapier MCP 登记失败", tostring(reason))
      end
      -- 新终端未必继承窗口环境；Base64 保留路径字符且不引入 shell 展开。
      local program = if tern.runtime.os == "windows" then ".\\datapier.exe" else "./datapier"
      local command = program .. " --data-dir-b64 " .. tern.base64.encode(data_dir) .. " tsp"
      local started, pane = pcall(function()
        return checked.hosts:terminal("local", { cwd = directory, command = command })
      end)
      if not started then
        M.invalidate()
        checked:toast("error", "Datapier 启动失败", tostring(pane))
      elseif started_pane then
        started_pane(pane)
      end
    end)
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
      local ok, reason = registration.register(checked, M.executable(), data_dir)
      if not ok then
        checked:toast("error", "Datapier MCP 登记失败", tostring(reason))
      end
      updater.schedule(checked, M.executable())
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
