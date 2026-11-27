# Datapier 0.5.2 技术方案

## 1. 目标与范围

0.5.2 改善同一个 Datapier pane 内的 SQL 编辑体验，修复从 Tern Files 打开 MCP 项目时左侧列表为空的问题，并为 Tern 之外的 Agent 提供固定的 MCP 启动路径。README 说明 Tern 和普通 omp 如何接入 Datapier MCP。方案以现有 [0.5.1 契约](technical-design-0.5.1.md)为基线，以下行为是 0.5.2 的实施和验收目标。

| 范围 | 用户可见结果 |
|---|---|
| SQL 编辑 | 普通查询和项目分栏查询使用当前 pane 内的 Tern TSP 原生编辑。点击可以定位光标，拖选和常用文本操作按宿主行为工作 |
| Files → Datapier MCP 项目 | 在发起操作的 Tern tab 复用工作台或分屏打开。左侧加载本机项目，右侧定位与 Files 目录匹配的项目 |
| MCP 固定入口与使用说明 | 用户可选择安装当前用户的固定入口。外部 Agent 配置一次 `command`，插件成功更新或回滚后继续使用同一路径。README 说明外部与 Tern 托管的配置入口 |
| 程序缓存与更新维护 | 升级或回滚后同步外部固定入口与 Tern 受管 MCP；由 Datapier 更新器回收已确认无引用的旧程序缓存和暂存 |
| 本机数据与自动写入说明 | 说明主数据、程序缓存、Tern 状态、系统凭据和运行时文件的位置、触发条件及清理责任 |
| Archery 首次连接 | Engine 冷启动后，打开已配置的 Archery 连接时优先尝试已保存的密码；需要人工补充时再提示 |

SQL 编辑采用公开 TSP `editor`。Tern 内置 Database 使用的代码编辑器和 Database provider 接入都依赖上游开放接口。本版用现有 `edit`／`undo` 协商、IPC v6 和配置 schema 6，已有查询草稿及项目配置可直接打开。MCP 沿用 `datapier mcp` 的 stdio 接口和权限模型。 [Tern 编辑协议](https://docs.stencil.so/tern/protocol/input.html#native-editing) · [Tern 插件 API](https://docs.stencil.so/tern/reference/api.html)

## 2. SQL 编辑与执行范围

### 2.1 输入与光标

普通查询和项目分栏查询均移除 `mode("insert")`，以启用宿主原生编辑。逻辑编辑器和分栏保持稳定身份，内层 editor 节点随正文修订更新并恢复焦点，旧节点和已经排队的旧消息均不能作用于新正文。节点更换前取消可能仍在 SDK 队列里的旧焦点请求，原生选区会随节点重建复位。首次点击未聚焦字段时，若宿主只发送 `focus`，应用保留最后同步的光标。收到 `edit` 时，根据该事件同步位置。 [普通查询控件](../crates/datapier-tsp/src/query_ui.rs) · [项目分栏控件](../crates/datapier-tsp/src/views/project_layout.rs) · [Tern 输入事件](https://docs.stencil.so/tern/protocol/input.html#events)

复用 UTF-16 编辑模型。应用 `edit` 前核对节点身份、正文修订、基准长度、替换范围及替换后光标的 UTF-16 边界。单纯移动光标只同步位置及相关命令状态。文本变化沿用草稿、撤销历史和查询结果失效路径。普通输入保留键盘路径；拖选、全选、复制和剪切交给宿主。应用侧旧选区不参与 SQL 操作目标计算。 [输入分派](../crates/datapier-tsp/src/input.rs) · [运行时事件](../crates/datapier-tsp/src/runtime.rs)

### 2.2 SQL 操作目标

TSP 的 `edit` 事件提供替换范围和编辑后的光标。拖选和键盘原生选区的两个端点留在宿主。Datapier 用最后一次同步到的光标定位当前语句，并使用现有 tokenizer 与连接的 SQL 模式。语句边界判定失败时显示原因。用户画出的高亮范围不影响下表的目标。 [TSP 事件载荷](https://docs.stencil.so/tern/protocol/input.html#events) · [SQL 执行入口](../crates/datapier-tsp/src/action.rs) · [执行计划入口](../crates/datapier-tsp/src/query_ui.rs)

| 操作 | 0.5.2 行为 |
|---|---|
| `⌘/Ctrl+Enter`、工具栏「当前语句」 | 执行 Datapier 最后同步到的光标所在语句。快捷键帮助和按钮提示说明首次点击可能只聚焦，拖选也可能保留上次位置 |
| `F5`、工具栏「执行全部」 | 执行当前查询标签的全文 |
| 行号旁的执行按钮 | 执行该按钮对应的语句 |
| 「查看执行计划」 | 只对最后同步光标所在、可安全识别的单条语句发送普通 `EXPLAIN FORMAT=JSON` |
| Archery「检查全文（不执行）」 | 将编辑区全文交给既有上线 SQL 检查接口，沿用实例、目标数据库和平台权限约束 |
| 执行选区、格式化选区、选区 `EXPLAIN` | 暂停对应入口，等宿主能提供完整选区后再恢复 |

同步工具栏、右键菜单、中英文快捷键帮助和 UI 设计说明。核对实际派发目标，避免旧选区入口通过余留事件触发执行。结果区、任务取消、历史和查询文件沿用原有路径。 [当前快捷键定义](ui-redesign.md#快捷键与系统适配) · [Archery SQL 检查](../crates/datapier-tsp/src/application.rs)

`F5` 避开 Tern 默认保留的组合键。macOS 的 `⌘+Shift+Enter` 缩放窗格，Windows 和 Linux 的 `Ctrl+Shift+Enter` 向右分屏。 [Tern 默认键位](https://docs.stencil.so/tern/reference/actions.html#blocks-and-panes)

### 2.3 补全与撤销

保留基于光标的 SQL 上下文、候选目录和替换范围校验。候选接受时核对目标标签、正文代次、光标和替换范围。原生拖选可能不产生应用事件，正文代次和最后同步的光标都可能保持原值，因此这些校验无法证明当前没有原生选区。 [补全状态与候选接受](../crates/datapier-tsp/src/completion.rs) · [TSP 原生编辑](https://docs.stencil.so/tern/protocol/input.html#native-editing)

Tern 0.9.36 的公开编辑事件无法可靠确认执行替换时的原生选区为空。0.5.2 禁用普通查询和项目分栏的补全接受与按键接管，包含点击候选和 `Tab`／`Enter`。恢复支持依赖 Tern 提供可验证的选区状态，或提供由宿主原子校验并执行替换的接口。

补全可启用时，验证候选仍锚在正确的光标处，并在真实 Tern 观察 `↑/↓`、`Tab`、`Enter`、`Escape` 与 IME 的路由。每次操作最多接受一次候选或提交一次文字。撤销沿用现有应用历史，本版原生编辑切换本身不提供重做。

## 3. Files 的 MCP 项目列表与定位

### 3.1 当前问题

`project_navigate_path` 先走 `project-new` 发出 `local_project.list`，紧接着发出 `local_project.resolve`。两者使用同一个 `ProjectWorkbench.pending`，后者覆盖列表请求 token，列表回包被丢弃。定位成功只更新当前项目，左侧仍没有条目。 [请求与回包实现](../crates/datapier-tsp/src/project_workbench.rs) · [项目列表渲染](../crates/datapier-tsp/src/views/project_workbench.rs)

### 3.2 请求与状态

沿用 Tern 的当前 tab 寻找、分屏与桥接流程。Rust 先初始化项目标签，再发出唯一一条 `local_project.list`。收到匹配的列表回包并交还 `pending` 后才发送 `local_project.resolve`。项目标签保留本次的 `host`、`path` 和续接身份。两次请求分别核对标签 ID、当前 `epoch` 和各自 token。普通项目入口沿用首次选中规则。Files 入口等待目录解析决定右侧选中状态。 [Files 回调](../plugins/datapier/window.luau) · [项目回包路由](../crates/datapier-tsp/src/requests.rs)

同一工作台收到更新的 Files 请求时收掉旧续接。Tern 桥接派发前校验来源 pane 和 Tern tab 仍有效，后台回包可更新仍打开的所属 Datapier 标签，但不会切换可见标签或夺回焦点。标签关闭或工作区切换时清理请求与续接。列表失败后刷新应重用当前续接的目录参数，并生成新 token。 [桥接派发](../plugins/datapier/lib/window_bridge.luau)

| 结果 | 界面与后续动作 |
|---|---|
| 列表读取成功，目录匹配 | 左侧展示本机项目，按项目 ID 高亮匹配项，右侧显示该项目及默认环境 |
| 列表读取成功，目录未关联、身份失效或已不可用 | 左侧仍可浏览已有项目，右侧显示定位失败的具体原因并允许手动选择 |
| 列表成功返回空数组 | 左侧显示空态并保留新建项目入口 |
| 列表读取失败或定位请求失败 | 显示具体失败，保留已加载列表。首次加载失败显示为失败状态。列表失败后可用刷新入口重试该目录的定位流程 |
| Datapier 标签关闭、工作区切换或续接失效 | 丢弃过期回包，清理待发送的定位请求 |

继续使用 Engine 的目录规范化、最长祖先匹配和目录身份检查。`list` 和 `resolve` 是先后两次读取。若定位结果对应的项目因并发修改未出现在刚加载的列表中，提示重新刷新定位，避免呈现无法从清单选中的匹配项目。 [Engine 本机项目 RPC](../crates/datapier-engine/src/server/local_project.rs)

## 4. MCP 固定入口与使用说明

### 4.1 外部 Agent 的固定入口

Tern 插件提供「安装命令行入口」，用户显式安装后可复制展开的绝对路径作为 MCP `command`，参数使用 `mcp`。入口按 OS 用户安装，创建时不修改 `PATH`。持久登记入口归属、正式插件安装目录及数据目录，同一个入口由当前归属的安装维护。不同安装并发时按固定入口加锁。在同一路径已有用户文件、非受管链接或另一安装的入口时报告冲突。删除入口只清理由该安装创建的对象。 [插件安装与缓存](../RELEASING.md#构建与组包)

| 平台 | 固定命令 | 实现 |
|---|---|---|
| macOS／Linux | `~/.local/bin/datapier` | 原生 launcher 读取同目录 `.datapier/active.json` |
| Windows | `%LOCALAPPDATA%\Datapier\bin\datapier.exe` | 原生 launcher 读取同目录 `active.json` |

六个正式归档各携带同架构 launcher，组包和安装器校验两个固定成员及各自摘要。launcher 继承 stdio，转发参数和退出码。Windows 使用 Job Object，在 launcher 终止后清理 MCP 子进程，保留共享 Engine。Unix 转发常用终止信号并等待；客户端关闭 stdin 时由目标处理 EOF。安装记录或目标不可用时，launcher 通过 stderr 报错。 [Windows 用户目录](https://learn.microsoft.com/en-us/windows/win32/shell/knownfolderid)

launcher 在读取登记的数据目录后取得共享准入锁，在锁内重新读取记录并校验程序摘要，持锁至子进程退出。这也覆盖不会检查固定入口的旧版本。macOS 更新协调器在收到新 plugin 加载回执后，在解除准入前持久化更新 ID 及新旧目标，切换记录，再持久化提交。回滚或中断恢复根据落盘阶段和更新 ID 保留或恢复对应目标。 [现有更新门禁](technical-design-0.5.0.md#55-plugin-版本检查与自动更新边界)

手工切换使用 `plugin-switch` 和目标正式归档。退出 Datapier 客户端和 Engine 后执行，替换、入口发布、备份与恢复期间使用同一准入保护。手工回到具备 MCP 入口的旧版本时保持固定 `command`。配置 schema 不兼容时通过 `--restore-data` 指定升级前的一致备份。后续版本继续使用同一 launcher，缓存保留固定入口及回滚目标。 [手工更新与回滚](../RELEASING.md#已安装固定入口时手工更新或回滚)

### 4.2 Tern 与 omp 的配置

在 README 的 MCP 配置说明中增加两条启动路径。沿用现有 `datapier mcp` stdio 入口及 23 个工具，配置示例使用占位路径，数据库凭据仍通过 Datapier 配置。

| 用户的启动方式 | 文档内容 |
|---|---|
| 在 shell 中直接运行 `omp` | 用 `/mcp add` 把 Datapier 加到当前 profile 的 User 范围。优先使用 §4.1 的固定入口绝对路径和 `mcp` 参数，数据目录由该入口的安装记录决定。独立安装可使用自行维护的固定程序路径，需要共享配置时设置与工作台一致的 `DATAPIER_DATA_DIR`。用 `/mcp list` 分辨是否发现，用 `/mcp test datapier` 检查连接和工具。手工改文件后先运行 `/mcp reload` |
| 让 Tern 作为 ACP 客户端启动 omp | 说明 `agents.mcp` 按 Host 保存，可按 Agent 过滤。在 Tern 注册自定义 Agent，ID 示例为 `omp-acp`，命令填 omp 可执行文件的绝对路径，参数为 `acp`。在项目目录通过 `tern acp omp-acp` 建立新会话。标明 Datapier 通过这条路径传入 omp 的端到端验证状态 |

普通 `omp` 启动与 Tern 托管 ACP 使用各自的配置来源。Tern 的条目启用后，工具能否访问数据库仍取决于 Datapier 的连接可见性、访问模式和审批。项目关联用于推荐。 [Tern 受管配置](technical-design-0.5.0.md#5-功能2tern-mcp-自动注册) · [omp MCP](https://omp.sh/docs/mcp) · [omp ACP](https://omp.sh/docs/acp#mcp-and-editor-capabilities)

### 4.3 更新后的入口同步和旧程序回收

外部固定命令和 Tern 受管 MCP 各自使用稳定 launcher。插件通过公开设置接口将确属 Datapier 管理的旧路径迁移到 `plugin-data/datapier/mcp-entry/`，并读回所登记的启动字段。更新事务切换两个 `active.json`，在恢复时按门禁记录恢复两份入口。原版 0.5.1 的版本保护保留已经登记的固定路径；切换后更新器只读核对落盘的 `agents.mcp`、入口及已安装程序摘要。保留用户自己的同名条目、启用状态、Agent 过滤及额外环境字段。 [固定入口切换](../crates/datapier/src/plugin_update/fixed_entry.rs) · [Tern 受管登记](../plugins/datapier/lib/mcp_registration.luau)

macOS 更新器在提交并读回受管配置后，回收 `managed-stage-*` 中由成功切换登记的旧程序副本。保留当前与最近一次切换的暂存程序、未完成更新、运行进程、固定入口和 Tern 配置仍引用的路径。共享版本缓存、早期 `stage-*`、开发 `dev-*`、用户数据及备份继续保留。读回或清理失败时保留缓存，已提交的切换仍有效，可用 `plugin-update --action cache-clean` 独立重试。 [更新事务](../crates/datapier/src/plugin_update/apply.rs) · [回收命令](../RELEASING.md#已安装固定入口时手工更新或回滚)

### 4.4 主数据与自动写入位置

README 说明下列位置及覆盖方式，发布指南说明更新器回收范围和回滚时必须保留的文件。文档以现有目录为基线。

| 写入位置 | 触发条件和内容 |
|---|---|
| 主数据目录 | macOS 默认 `~/Library/Application Support/datapier/`，Linux 为 `${XDG_DATA_HOME:-$HOME/.local/share}/datapier/`，Windows 为 `%LOCALAPPDATA%\datapier\data\`。直接启动时可用 `--data-dir` 或 `DATAPIER_DATA_DIR` 覆盖，固定入口使用登记目录。Engine 创建 `datapier.sqlite3`、SQLite WAL 相关文件、锁和 `engine.json`，自动保存工作区、草稿和执行记录；相关操作还会产生 `edit-files/`、`project-stages/` 和 `transfer-index/` |
| 程序缓存 | macOS／Linux 默认 `${XDG_CACHE_HOME:-$HOME/.cache}/datapier/`，Windows 为 `%LOCALAPPDATA%\Datapier\cache\`。`DATAPIER_CACHE_DIR` 可覆盖。Tern 插件准备和校验已安装版本时写入程序副本；开发模式写入 `dev-*` |
| Tern 配置根和插件状态 | macOS 默认 `~/Library/Application Support/Tern/`，可由 `TERN_CONFIG_DIR` 覆盖。插件初始化会写入 `plugin-data/datapier/` 的状态及受管入口，并更新 `settings.json` 的受管 MCP 配置。升级另写暂存和备份，并由 Tern 替换 `plugins/datapier/` |
| 系统凭据与运行时 | 已保存的密码和 TOTP 存在系统凭据库。会话密码、Cookie 和普通查询结果行留在内存。Unix IPC socket 位于 `/tmp/datapier-<uid>/`，Windows 使用 Named Pipe |
| 固定命令目录 | 显式安装命令入口时创建 launcher 和登记文件，受管更新自动切换登记 |

主数据目录中的 `cache.sqlite3` 和 `values/` 属于早期版本的结果缓存，通过 TUI 独立预览和清理。用户显式指定的查询文件、项目保存和导出路径按各自操作处理。 [主数据目录](../crates/datapier-engine/src/lib.rs) · [Engine 运行文件](../crates/datapier-engine/src/server.rs) · [旧结果缓存](../crates/datapier-core/src/storage/retired_cache.rs)

## 5. 实施顺序与验收

1. 切换两个 SQL 输入入口及其事件分派。同步工具栏、帮助、旧操作清理、UI 设计说明、中英文文案及受影响的现有测试。
2. 在 TSP 项目页修复列表与定位续接，覆盖冷启动、未关联、读取失败和过期回包。Engine RPC 和 Tern 路由继续使用既有接口。
3. 接入固定入口的安装、更新和回滚。六目标组包校验主程序及 launcher，同步 README 的 MCP 配置和发布指南的手工切换命令。
4. 补齐 Tern 受管 MCP 回滚、无引用程序缓存回收和写盘位置说明。
5. 在工作台打开 Archery 连接时先尝试已保存的凭据，复用现有登录和连接续接。

本轮原生验收范围为 macOS arm64。其他发布目标的构建和运行要求见[发布指南](../RELEASING.md#构建与组包)。

| 验收项 | 通过标准 |
|---|---|
| SQL 基本编辑 | 在真实 Tern 分别打开普通查询和项目分栏查询，通过点击、拖选、全选、复制、剪切、粘贴及撤销编辑包含中文和 emoji 的 SQL。核对编辑落在正确标签，旧快照的同长度编辑不会写入当前正文，原生编辑关闭时仍可使用基本键盘输入 |
| SQL 执行目标 | 加载至少两条返回不同结果的 SQL。验证首次点击的焦点与光标事件、聚焦后的点击定位、拖选后按 `⌘/Ctrl+Enter`、`F5`、行号执行和 `EXPLAIN`，核对实际发送的文本和光标提示。用 Archery 多条 SQL 断言检查请求发送全文且数据库未收到执行请求 |
| 候选与草稿 | 普通查询和项目分栏查询均验证普通输入、IME、分栏切换及查询文件的打开和保存。补全可启用时，打开候选后鼠标拖选或键盘扩选，再分别按 `Tab`／`Enter` 和点击旧候选，确认旧候选不会写入正文，另测空选区接受候选及一次撤销。无法可靠取得原生选区状态时，核对所有接受入口均禁用，`Tab`／`Enter` 沿用普通按键行为 |
| Files 冷热入口 | 有本机项目时，分别从冷启动分屏和已有工作台进入。覆盖匹配、未关联、真实空列表、失败后刷新、连续点击、响应迟到和来源 tab 切换。核对列表加载、匹配项高亮和焦点归属 |
| MCP 固定入口 | 在 macOS arm64 用普通账户安装，覆盖同名文件或链接冲突，以及带空格、中文的路径。核对归档成员及摘要，验证固定路径完成 `initialize` 和 `tools/list`，切换或回滚后重连并核对实际版本。注入校验失败、切换中断及已解析旧路径的启动，核对恢复结果与启动门禁 |
| MCP 文档 | 核对两个启动方式、实际配置文件位置、命令及其输出含义。复用已有登记读回和隔离 stdio 握手作为佐证，ACP 转发标注其已知验证状态 |
| 缓存和受管 MCP | 在 macOS arm64 用混合版本缓存执行升级、回滚与中断恢复。分别核对 Tern `agents.mcp` 和外部固定入口实际启动的版本。确认无引用的受管旧程序可回收，活动程序、恢复材料、开发缓存和未知引用的目录得到保留 |
| 自动写入说明 | 使用隔离数据目录、缓存和 Tern 配置根，核对启动、工作区自动保存和更新前后的写入位置；区分程序缓存和旧结果缓存 |
| Archery 冷启动 | 确认旧 Engine 已退出，从新 Engine 第一次打开已有 MySQL／Redis 连接。可读的正确密码直接登录并续接一次；自动 TOTP、手动验证码、仅会话密码、Keychain 不可读、密码失效、取消和切换标签分别核对提示及请求次数 |

设计调查时观察到 Tern 0.9.36（`7123a24`）的内置 SQLite Database 可在同一 pane 完成鼠标定位、拖选执行、撤销和重做。公开 TSP 编辑器和内置 Database 的组件不同，该 demo 用于确认实现边界。 [0.5.1 运行基线](technical-design-0.5.1.md#5-验收矩阵) · [仓库验证约定](../CONTRIBUTING.md#测试与验证)

### 实际运行记录

- macOS arm64 的 Tern 0.9.36 实窗验证普通查询和项目分栏的鼠标定位、拖选、Unicode 编辑、IME 组合提交、撤销、分栏隔离和查询文件读写。关闭宿主原生编辑后基本键盘输入可用。普通查询的 `Tab` 保留正文，`Enter` 插入换行。连续输入及切换分栏的录制未收到 TSP 协议错误。
- 使用隔离 MySQL 核对实际执行记录。快捷键按同步光标执行当前语句，`F5` 发送两条语句的全文，行号按钮发送对应语句，`EXPLAIN` 发送当前语句。Archery 全文路由通过定向测试。
- Files 实窗覆盖冷启动分屏、热入口复用、匹配、未关联和全新数据目录的空列表。连续点击只打开一个工作台，切换到其他 Tern 标签后焦点保持在该标签。列表失败刷新和迟到回包通过定向测试。
- 用 release 构建及仓库组包脚本生成 macOS arm64 归档，核对主程序、launcher 和摘要。隔离安装覆盖含中文和空格的路径，保留了已有同名文件及链接。注入替换中断时，固定入口及已解析的旧程序路径均被门禁拦截，`--recover` 恢复原入口。通过相同固定路径回滚至正式 0.5.1 后切回候选 0.5.2，两个版本均完成握手并返回 23 个 MCP 工具，关闭 stdin 后进程正常退出。损坏归档被拒绝，原入口保留。

- 在隔离 Tern 窗口内从正式 0.5.1 升到候选 0.5.2，确认受管命令迁移到固定路径，保留禁用状态和 Agent 过滤。往返切换后，直接使用 `agents.mcp` 读回的命令及参数、外部固定命令分别完成 0.5.1 与 0.5.2 的握手，各返回 23 项工具。安装中断时两份入口均被阻断，`--recover` 后恢复原目标。超过保留范围的旧暂存程序被回收，前一版本、旧共享缓存、开发缓存、早期暂存和数据备份保留。
- 旧暂存程序仍在另一隔离数据目录运行时，`cache-clean` 保留程序。进程退出后再次执行，确认删除该程序缓存。
- 使用隔离 Archery 1.14.0、MySQL 和 Redis，在 macOS arm64 的 Tern 实窗从新 Engine 打开已有连接。系统凭据中的正确密码和自动 TOTP 直接完成连接。仅会话保存的密码和已失效的密码分别显示具体原因与密码输入框。手动验证码只显示验证码输入框，验证后续接，取消后连接未打开。凭据库读取失败通过定向测试注入，切换目标及迟到回包也由定向测试覆盖。
- 审查修复后在 macOS arm64 的 Tern 实窗确认，Files 窄分屏按清单、详情顺序纵向显示，并自动定位匹配项目。已有默认查询标签随全局语言切换。SQL 页仅保留查询操作，数据库节点菜单提供建表和数据交换入口。可写 MySQL 连接请求取消时提示等待数据库确认，最终显示实际任务终态。
- 使用隔离 MySQL 实窗验证原位编辑切换后暂存，退出并重开后重新连接原表、恢复与预览草稿。恢复后的草稿再次保存，数据库原值保留。受管固定入口在离线切到 0.5.2 后完成 MCP 握手，返回 23 项工具。模拟 MCP 客户端延迟返回 roots 时，全局发现仍可完成，回复到达后可使用项目上下文。
- 使用同版本的两份 macOS arm64 候选包验证自动安装阶段。为原本为空的目标缓存准备校验归档后，将下载地址指向不可用端点。自动安装 worker 完成替换并收到真实 Tern 窗口回执，已安装清单与目标归档摘要一致，受管固定命令完成握手并返回 23 项工具。测试从已准备状态开始，覆盖退出后安排、离线安装和回执。
- 在隔离 Archery 1.14.0 删除服务端会话后，Tern 实窗使用已保存密码重新认证。过期查询只登记一次，用户显式重试后成功。隔离 Redis 7.2 在续页拒绝 `PTTL` 后，用原游标重试读回了全部 key。

## 6. Archery 冷启动登录

`connection.open` 要求共享认证已就绪。冷启动时，工作台使用既有 `archery.auth.begin` 读取系统凭据。密码条目与数据目录和 Archery 服务 ID 绑定，仅存在于旧 Engine 会话的密码会随进程退出丢失。 [连接打开](../crates/datapier-engine/src/server/archery/connections.rs) · [工作台弹窗](../crates/datapier-tsp/src/connections/archery.rs) · [凭据读取](../crates/datapier-core/src/storage/archery.rs)

用户打开已有 Archery 连接且共享登录未就绪时，工作台先发起一次不带明文密码的 `archery.auth.begin`，并显示认证进行中。同一服务复用共享认证；认证成功后，只续接仍有效的原连接请求一次。已配置且可用的自动 TOTP 沿用现有流程。若平台要求手动验证码，只显示验证码步骤；未保存、凭据库不可用或平台拒绝密码时，说明原因并提示输入密码。网络和服务错误沿用原诊断。取消或切换目标后，迟到结果不得打开旧连接；已失败的 SQL 或 Redis 命令须由用户重新执行。 [认证流程](../crates/datapier-engine/src/server/archery/auth.rs) · [连接续接](../crates/datapier-tsp/src/connections/archery/response.rs)
