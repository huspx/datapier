-- 更新状态由原生进程持久化；窗口 VM 只负责调度，不能自行解除启动门禁。
local text = require("./locale").text
local M = {}
local active = true

function M.run(cx, binary, action, force, callback, data_dir)
  local argv = { binary }
  if data_dir then
    table.insert(argv, "--data-dir")
    table.insert(argv, data_dir)
  end
  for _, arg in ipairs({
    "plugin-update",
    "--action",
    action,
    "--plugin-dir",
    tern.plugin.dir,
    "--state-dir",
    tern.plugin.data,
  }) do
    table.insert(argv, arg)
  end
  if force then
    table.insert(argv, "--force")
  end
  local ok, problem = pcall(function()
    -- 两个归档各有网络时限，随后安装器还需独立完成校验。
    tern.process.run(argv, { timeout_ms = if action == "download" then 930000 else 300000 }, function(result, fresh)
      if not active then
        return
      end
      local decoded, value = pcall(tern.json.decode, result.stdout)
      if not decoded or type(value) ~= "table" then
        callback(
          fresh,
          nil,
          result.stderr ~= "" and result.stderr
            or text("更新程序返回无效状态", "The updater returned an invalid status")
        )
      else
        callback(fresh, value, if result.status ~= 0 then value.message else nil)
      end
    end)
  end)
  if not ok then
    callback(cx, nil, tostring(problem))
  end
end

function M.allowed(cx, binary, callback)
  M.run(cx, binary, "status", false, function(fresh, state, problem)
    if not state or not state.ok or state.gate_blocked ~= false then
      callback(
        fresh,
        false,
        problem or (state and state.message) or text("无法确认更新门禁", "Could not confirm the update gate"),
        state and state.ok and state.state ~= "failed" and state.gate_blocked == true
      )
      return
    end
    if type(state.data_dir) ~= "string" or state.data_dir == "" then
      callback(
        fresh,
        false,
        text("更新程序未确认数据目录", "The updater did not confirm the data directory")
      )
      return
    end
    callback(fresh, true, nil, false, state.data_dir)
  end)
end

function M.policy()
  local value = tern.kv.get("update_policy")
  if type(value) ~= "table" then
    value = {}
  end
  return {
    auto_check = value.auto_check ~= false,
    auto_download = value.auto_download == true,
    auto_apply = value.auto_apply == true and value.auto_download == true,
  }
end

function M.schedule(cx, binary)
  local policy = M.policy()
  if not policy.auto_check then
    return
  end
  M.run(cx, binary, "check", false, function(fresh, state, problem)
    if not state or not state.ok then
      if problem then
        fresh:toast("error", text("Datapier 更新检查失败", "Datapier update check failed"), tostring(problem))
      end
      return
    end
    if state.state == "available" then
      fresh:toast(
        "info",
        text("Datapier 有可用更新", "Datapier update available"),
        tostring(state.message or state.target_version)
      )
      if policy.auto_download then
        M.run(fresh, binary, "download", true, function(later, result, err)
          if not result or not result.ok then
            later:toast(
              "error",
              text("Datapier 更新下载失败", "Datapier update download failed"),
              tostring(err or (result and result.message))
            )
          end
          if result and result.ok and result.prepared and M.policy().auto_apply then
            M.run(later, binary, "apply", false, function(final, applied, failure)
              if not applied or not applied.ok then
                final:toast(
                  "error",
                  text("Datapier 更新未应用", "Datapier update was not applied"),
                  tostring(failure or (applied and applied.message))
                )
              end
            end, result.data_dir)
          end
        end)
      end
    end
  end)
end

tern.on("unload", function()
  active = false
end)
return M
