-- 薄启动入口：窗口只调度安装和终端启动，数据库状态与界面逻辑均由 Rust 持有。

local M = {}

function M.start(cx)
    local os = tern.runtime.os
    local argv
    -- runtime 只保证提供 OS；硬件架构识别、摘要验证和缓存发布交给原生安装脚本。
    if os == "windows" then
        argv = { "powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File", tern.plugin.dir .. "/install/windows.ps1" }
    elseif os == "macos" or os == "linux" then
        argv = { "/bin/sh", tern.plugin.dir .. "/install/unix.sh" }
    else
        cx:toast("error", "Datapier 无法启动", "此平台不支持本机程序")
        return
    end
    cx:toast("info", "正在准备 Datapier")
    -- 下载和解包放入异步子进程，既不阻塞窗口，也不受 Luau fetch 响应体大小限制。
    local ok, problem = pcall(function()
        tern.process.run(argv, { timeout_ms = 300000 }, function(result, fresh)
            local decoded, data = pcall(tern.json.decode, result.stdout)
            if result.status ~= 0 or not decoded or type(data) ~= "table" or not data.ok then
                local message = if decoded and type(data) == "table" then data.error else result.stderr
                fresh:toast("error", "Datapier 安装失败", tostring(message or "安装脚本返回无效结果"))
                return
            end
            -- 异步回调使用 fresh 上下文；原生程序通过 cwd + 固定 shell 命令启动，
            -- Launch 的 block args 不能当作进程 argv，用户输入也不能拼入命令。
            local command = if os == "windows" then ".\\datapier.exe ui" else "./datapier ui"
            local started, failure = pcall(function()
                fresh.hosts:terminal("local", { cwd = data.cwd, command = command })
            end)
            if not started then
                fresh:toast("error", "Datapier 启动失败", tostring(failure))
            end
        end)
    end)
    if not ok then
        cx:toast("error", "Datapier 安装失败", tostring(problem))
    end
end

return M
