-- 只管理自己登记的启动字段；用户对同名项的修改始终优先。
local text = require("./locale").text
local M = {}

function M.local_scope(cx)
  local ok, host = pcall(function()
    return cx.hosts:current()
  end)
  if not ok or type(host) ~= "table" or host.kind ~= "local" then
    return nil, text("当前 host 不是已确认的本机 host", "The current host is not a verified local host")
  end
  return "local"
end

local function startup(entry)
  local env = if type(entry.env) == "table" then entry.env else {}
  return { command = entry.command, transport = entry.transport, args = entry.args, data_dir = env.DATAPIER_DATA_DIR }
end

local function same(a, b)
  if type(a) ~= "table" or type(b) ~= "table" then
    return false
  end
  if a.command ~= b.command or a.transport ~= b.transport or a.data_dir ~= b.data_dir then
    return false
  end
  if type(a.args) ~= "table" or type(b.args) ~= "table" or #a.args ~= #b.args then
    return false
  end
  for i, value in ipairs(a.args) do
    if value ~= b.args[i] then
      return false
    end
  end
  return true
end

local function version()
  local manifest = tern.fs.read(tern.plugin.dir .. "/plugin.toml", 65536)
  local major, minor, patch = manifest:match('version%s*=%s*"(%d+)%.(%d+)%.(%d+)"')
  if not major then
    error(text("无法读取插件版本", "Could not read the plugin version"))
  end
  return { tonumber(major), tonumber(minor), tonumber(patch) }
end

local function newer(a, b)
  if type(a) ~= "table" then
    return false
  end
  for i = 1, 3 do
    if type(a[i]) ~= "number" then
      return false
    end
    if a[i] ~= b[i] then
      return a[i] > b[i]
    end
  end
  return false
end

function M.register(cx, binary, data_dir)
  local host, reason = M.local_scope(cx)
  if not host then
    return false, reason
  end
  if type(data_dir) ~= "string" or data_dir == "" then
    return false, text("程序未确认数据目录", "The program did not confirm the data directory")
  end
  local record = tern.kv.get("mcp_managed")
  if type(record) == "table" and (record.host ~= host or record.data_dir ~= data_dir) then
    return false,
      text(
        "Datapier MCP 已由另一本机数据目录管理",
        "Datapier MCP is already managed by another local data directory"
      )
  end
  local version_ok, current_version = pcall(version)
  if not version_ok then
    return false, tostring(current_version)
  end
  if type(record) == "table" and newer(record.version, current_version) then
    return false, text("请升级 Datapier", "Please upgrade Datapier") .. " " .. table.concat(record.version, ".")
  end
  local ok, result = pcall(function()
    local entries = cx.settings:get("agents.mcp")
    if entries == nil then
      entries = tern.json.array()
    end
    if type(entries) ~= "table" then
      error(text("agents.mcp 不是列表", "agents.mcp is not a list"))
    end
    local found, index = nil, nil
    for i, entry in ipairs(entries) do
      if type(entry) == "table" and entry.name == "datapier" then
        if found then
          error(text("存在多个 Datapier MCP 配置", "Multiple Datapier MCP entries exist"))
        end
        found, index = entry, i
      end
    end
    local current = if found then startup(found) else nil
    if current and (not record or (not same(current, record.startup) and not same(current, record.pending))) then
      error(text("同名 MCP 配置已由用户自定义", "A user-managed MCP entry already uses this name"))
    end
    local desired = if found then table.clone(found) else { name = "datapier" }
    local env = if type(desired.env) == "table" then table.clone(desired.env) else {}
    env.DATAPIER_DATA_DIR = data_dir
    desired.env = env
    desired.transport = "stdio"
    desired.command = binary
    desired.args = { "mcp", "--plugin-cache-engine" }
    local next_startup = startup(desired)
    if same(current, next_startup) then
      local old = record.version
      local same_version = type(old) == "table"
        and old[1] == current_version[1]
        and old[2] == current_version[2]
        and old[3] == current_version[3]
      if not same(record.startup, current) or record.pending ~= nil or not same_version then
        tern.kv.set("mcp_managed", { host = host, data_dir = data_dir, startup = current, version = current_version })
      end
      return found
    end
    if index then
      entries[index] = desired
    else
      table.insert(entries, desired)
    end
    -- 先持久登记待写入的启动字段；中断后可识别两种合法状态。
    tern.kv.set("mcp_managed", {
      host = host,
      data_dir = data_dir,
      startup = current or (record and record.startup),
      pending = next_startup,
      version = current_version,
    })
    -- 同步调用内重新合并最新设置，避免使用安装前的陈旧快照。
    cx.settings:set("agents.mcp", entries)
    local readback = cx.settings:get("agents.mcp")
    local match, count = nil, 0
    for _, entry in ipairs(readback) do
      if type(entry) == "table" and entry.name == "datapier" then
        match = entry
        count += 1
      end
    end
    if count ~= 1 or not same(startup(match), next_startup) then
      error(text("MCP 配置写入后读回不一致", "The saved MCP entry differs from the expected value"))
    end
    tern.kv.set("mcp_managed", { host = host, data_dir = data_dir, startup = next_startup, version = current_version })
    return match
  end)
  if not ok then
    return false, tostring(result)
  end
  return true
end

return M
