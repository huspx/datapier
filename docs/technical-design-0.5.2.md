# Datapier 0.5.2 技术方案

## 1. 目标与范围

0.5.2 改善同一个 Datapier pane 内的 SQL 编辑体验，修复从 Tern Files 打开 MCP 项目时左侧列表为空的问题，并为 Tern 之外的 Agent 提供固定的 MCP 启动路径。README 说明 Tern 和普通 omp 如何接入 Datapier MCP。方案以现有 [0.5.1 契约](technical-design-0.5.1.md)为基线，以下行为是 0.5.2 的实施和验收目标。

| 范围 | 用户可见结果 |
|---|---|
| SQL 编辑 | 普通查询和项目分栏查询使用当前 pane 内的 Tern TSP 原生编辑。点击可以定位光标，拖选和常用文本操作按宿主行为工作 |
| Files → Datapier MCP 项目 | 在发起操作的 Tern tab 复用工作台或分屏打开。左侧加载本机项目，右侧定位与 Files 目录匹配的项目 |
| MCP 固定入口与使用说明 | 用户可选择安装当前用户的固定入口。外部 Agent 配置一次 `command`，插件成功更新或回滚后继续使用同一路径。README 说明外部与 Tern 托管的配置入口 |

SQL 编辑采用公开 TSP `editor`。Tern 内置 Database 使用的代码编辑器和 Database provider 接入都依赖上游开放接口。本版用现有 `edit`／`undo` 协商、IPC v6 和配置 schema 6，已有查询草稿及项目配置可直接打开。MCP 沿用 `datapier mcp` 的 stdio 接口和权限模型。 [Tern 编辑协议](https://docs.stencil.so/tern/protocol/input.html#native-editing) · [Tern 插件 API](https://docs.stencil.so/tern/reference/api.html)

## 2. SQL 编辑与执行范围

### 2.1 输入与光标

普通查询和项目分栏查询均移除 `mode("insert")`。该属性原本用于让 Datapier 自己维护键盘选区，Tern 会因此关闭字段的原生编辑。继续使用稳定节点 key、SQL 高亮和已有项目分栏焦点路由。未聚焦字段的首次点击只收到 `focus`，事件只有节点 ID，应用切换焦点并保留该编辑器最后同步的光标。已聚焦字段再次点击并移动光标时，Tern 通过 `edit` 回传位置。Tern 设置中的 **Native composer editing** 决定原生交互是否启用。 [普通查询控件](../crates/datapier-tsp/src/query_ui.rs) · [项目分栏控件](../crates/datapier-tsp/src/views/project_layout.rs) · [Tern 输入事件](https://docs.stencil.so/tern/protocol/input.html#events)

复用现有 UTF-16 编辑模型。应用宿主 `edit` 前核对目标编辑器、基准文本长度、替换范围及替换后光标的 UTF-16 边界。`len` 只能排除长度不同的旧正文。若应用已修改正文，且 TSP 帧确认与事件顺序无法消除同长度旧快照的歧义，拒绝该事件并以当前正文和光标重新同步。单纯移动光标只同步位置及相关命令状态。文本变化继续通过同一编辑路径更新草稿、撤销历史并使旧查询结果失效。Tern 未消费的普通输入及原生编辑关闭时的基本输入沿用现有键盘路径。查询的拖选、全选、复制和剪切交给宿主，移除 SQL 专属的选区复制、剪切和菜单分派。应用快照中的旧选区不参与 SQL 操作目标计算。 [输入分派](../crates/datapier-tsp/src/input.rs) · [运行时事件](../crates/datapier-tsp/src/runtime.rs) · [剪贴板](../crates/datapier-tsp/src/clipboard.rs)

### 2.2 SQL 操作目标

TSP 的 `edit` 事件提供替换范围和编辑后的光标。拖选和键盘原生选区的两个端点留在宿主。Datapier 用最后一次同步到的光标定位当前语句，并使用现有 tokenizer 与连接的 SQL 模式。语句边界判定失败时显示原因。用户画出的高亮范围不影响下表的目标。 [TSP 事件载荷](https://docs.stencil.so/tern/protocol/input.html#events) · [SQL 执行入口](../crates/datapier-tsp/src/action.rs) · [执行计划入口](../crates/datapier-tsp/src/query_ui.rs)

| 操作 | 0.5.2 行为 |
|---|---|
| `⌘/Ctrl+Enter`、工具栏「当前语句」 | 执行 Datapier 最后同步到的光标所在语句。快捷键帮助和按钮提示说明首次点击可能只聚焦，拖选也可能保留上次位置 |
| `⌘/Ctrl+Shift+Enter`、工具栏「执行全部」 | 执行当前查询标签的全文 |
| 行号旁的执行按钮 | 执行该按钮对应的语句 |
| 「查看执行计划」 | 只对最后同步光标所在、可安全识别的单条语句发送普通 `EXPLAIN FORMAT=JSON` |
| Archery「检查全文（不执行）」 | 将编辑区全文交给既有上线 SQL 检查接口，沿用实例、目标数据库和平台权限约束 |
| 执行选区、格式化选区、选区 `EXPLAIN` | 暂停对应入口，等宿主能提供完整选区后再恢复 |

同步工具栏、右键菜单、中英文快捷键帮助和 UI 设计说明。核对实际派发目标，避免旧选区入口通过余留事件触发执行。结果区、任务取消、历史和查询文件沿用原有路径。 [当前快捷键定义](ui-redesign.md#快捷键与系统适配) · [Archery SQL 检查](../crates/datapier-tsp/src/application.rs)

### 2.3 补全与撤销

保留基于光标的 SQL 上下文、候选目录和替换范围校验。候选接受时核对目标标签、正文代次、光标和替换范围。原生拖选可能不产生应用事件，正文代次和最后同步的光标都可能保持原值，因此这些校验无法证明当前没有原生选区。 [补全状态与候选接受](../crates/datapier-tsp/src/completion.rs) · [TSP 原生编辑](https://docs.stencil.so/tern/protocol/input.html#native-editing)

**【待验证】启用补全接受的门槛。** 普通查询和项目分栏查询只有在接受候选时能可靠确认当前编辑器、正文代次、光标与宿主原生选区属于同一份状态，且选区为空，才允许替换文本。发现原生选区或状态变化时，废弃旧候选。若现有公开接口无法提供可靠证据，禁用该编辑器下的补全接受，包括点击候选和 `Tab`／`Enter`，退出补全按键接管。恢复支持依赖 Tern 提供可验证的选区状态，或提供由宿主原子校验并执行替换的接口。

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
| macOS／Linux | `~/.local/bin/datapier` | 软链指向已激活版本的缓存程序。切换时在同一目录创建新软链，再原子替换原软链 |
| Windows | `%LOCALAPPDATA%\Datapier\bin\datapier.exe` | 原生 launcher 读取 `%LOCALAPPDATA%\Datapier\bin\active.json`，按绝对路径启动已激活版本的缓存程序。日常切换只原子替换记录，launcher 与正在运行的缓存程序可继续使用 |

Windows 入口与现有 `%LOCALAPPDATA%\Datapier\cache` 同属当前用户的 Datapier 目录。原生 launcher 可由 MCP 客户端直接启动，普通账户无需开启开发者模式。Windows 的 x64 与 arm64 发布归档各携带 `datapier.exe` 和同架构的 `datapier-launcher.exe`。组包、发布清单和安装器共同校验两个固定成员及各自摘要，只有显式安装入口时才在 `bin` 发布 launcher。launcher 启动子进程时原样传递参数、环境与 stdio，传播退出码，MCP 客户端终止 launcher 时同步结束子进程。安装记录缺失或目标不可用时，通过 stderr 报错并非零退出。 [Windows 用户目录](https://learn.microsoft.com/en-us/windows/win32/shell/knownfolderid) · [Windows 软链条件](https://learn.microsoft.com/en-us/windows/win32/api/winbase/nf-winbase-createsymboliclinkw) · [现有 Windows 安装器](../plugins/datapier/install/windows.ps1)

固定入口只发布已经通过版本及摘要校验的正式缓存程序。固定入口启动的 MCP 默认使用登记的数据目录，显式指定目录时须与登记一致。MCP 在取得该目录的共享准入后重新核对当前目标，共享准入保持至握手完成或启动失败，确保切换前已解析旧路径但尚未准入的进程退出并提示重启。macOS 自动更新沿用现有准入锁和更新门禁。协调器收到新 plugin 加载回执后，在解除门禁前持久化更新 ID 及新旧目标，切换固定入口，再持久化提交阶段。失败按现有回滚条件恢复旧 plugin 和入口目标，确认数据可由旧版打开后才解除门禁。崩溃恢复按更新 ID 和已落盘阶段推进，重复执行得到同一结果。 [现有更新门禁](technical-design-0.5.0.md#55-plugin-版本检查与自动更新边界) · [Engine 准入检查](../crates/datapier-engine/src/platform.rs)

手工更新仍要求先退出 Datapier 客户端和 Engine。已安装固定入口时，从受管手工更新流程发起 plugin 替换，在替换、校验、入口发布及失败恢复期间维持相同的准入保护，阻止外部 Agent 自动重连提前启动 Engine。该流程供各平台在客户端退出后显式执行。手工回滚重新安装并验证旧 plugin，按已有数据兼容及备份恢复要求核对后再恢复旧目标。缓存清理保留固定入口和可回滚版本引用的二进制。开发安装不能接管正式入口。 [插件更新边界](../RELEASING.md#插件更新)

### 4.2 Tern 与 omp 的配置

在 README 的 MCP 配置说明中增加两条启动路径。沿用现有 `datapier mcp` stdio 入口及 23 个工具，配置示例使用占位路径，数据库凭据仍通过 Datapier 配置。

| 用户的启动方式 | 文档内容 |
|---|---|
| 在 shell 中直接运行 `omp` | 用 `/mcp add` 把 Datapier 加到当前 profile 的 User 范围。优先使用 §4.1 的固定入口绝对路径和 `mcp` 参数，数据目录由该入口的安装记录决定。独立安装可使用自行维护的固定程序路径，需要共享配置时设置与工作台一致的 `DATAPIER_DATA_DIR`。用 `/mcp list` 分辨是否发现，用 `/mcp test datapier` 检查连接和工具。手工改文件后先运行 `/mcp reload` |
| 让 Tern 作为 ACP 客户端启动 omp | 说明 `agents.mcp` 按 Host 保存，可按 Agent 过滤。在 Tern 注册自定义 Agent，ID 示例为 `omp-acp`，命令填 omp 可执行文件的绝对路径，参数为 `acp`。在项目目录通过 `tern acp omp-acp` 建立新会话。标明 Datapier 通过这条路径传入 omp 的端到端验证状态 |

普通 `omp` 启动与 Tern 托管 ACP 使用各自的配置来源。Tern 的条目启用后，工具能否访问数据库仍取决于 Datapier 的连接可见性、访问模式和审批。项目关联用于推荐。 [Tern 受管配置](technical-design-0.5.0.md#5-功能2tern-mcp-自动注册) · [omp MCP](https://omp.sh/docs/mcp) · [omp ACP](https://omp.sh/docs/acp#mcp-and-editor-capabilities)

## 5. 实施顺序与验收

1. 切换两个 SQL 输入入口及其事件分派。同步工具栏、帮助、旧操作清理、UI 设计说明、中英文文案及受影响的现有测试。
2. 在 TSP 项目页修复列表与定位续接，覆盖冷启动、未关联、读取失败和过期回包。Engine RPC 和 Tern 路由继续使用既有接口。
3. 接入固定入口的安装、更新和回滚。补齐 Windows launcher 的双架构组包与校验，再更新 README 的 MCP 配置。产品版本派生和发布流程按 [发布指南](../RELEASING.md)执行。

| 验收项 | 通过标准 |
|---|---|
| SQL 基本编辑 | 在真实 Tern 分别打开普通查询和项目分栏查询，通过点击、拖选、全选、复制、剪切、粘贴及撤销编辑包含中文和 emoji 的 SQL。核对编辑落在正确标签，旧快照的同长度编辑不会写入当前正文，原生编辑关闭时仍可使用基本键盘输入 |
| SQL 执行目标 | 加载至少两条返回不同结果的 SQL。分别测试首次点击只聚焦、聚焦后的点击移动光标、拖选后按 `⌘/Ctrl+Enter`、`⌘/Ctrl+Shift+Enter`、行号执行和 `EXPLAIN`，核对实际发送的文本和光标提示。用 Archery 多条 SQL 断言检查请求发送全文且数据库未收到执行请求 |
| 候选与草稿 | 普通查询和项目分栏查询均验证普通输入、IME、分栏切换及查询文件的打开和保存。补全可启用时，打开候选后鼠标拖选或键盘扩选，再分别按 `Tab`／`Enter` 和点击旧候选，确认旧候选不会写入正文，另测空选区接受候选及一次撤销。无法可靠取得原生选区状态时，核对所有接受入口均禁用，`Tab`／`Enter` 沿用普通按键行为 |
| Files 冷热入口 | 有本机项目时，分别从冷启动分屏和已有工作台进入。覆盖匹配、未关联、真实空列表、失败后刷新、连续点击、响应迟到和来源 tab 切换。核对列表加载、匹配项高亮和焦点归属 |
| MCP 固定入口 | 在 macOS、Linux、Windows 用普通账户安装，覆盖同名文件冲突以及带空格、中文的路径。固定路径完成 `initialize` 和 `tools/list`，切换或回滚后重连并核对实际版本。注入校验失败、切换中断及已解析旧路径的启动，核对恢复结果与启动门禁。Windows 的 x64 与 arm64 分别核对归档成员与摘要，并在关闭开发者模式时验证 stdio、退出码及终止后无残留进程 |
| MCP 文档 | 核对两个启动方式、实际配置文件位置、命令及其输出含义。复用已有登记读回和隔离 stdio 握手作为佐证，ACP 转发标注其已知验证状态 |

设计调查时观察到 Tern 0.9.36（`7123a24`）的内置 SQLite Database 可在同一 pane 完成鼠标定位、拖选执行、撤销和重做。公开 TSP 编辑器和内置 Database 的组件不同，该 demo 只用于确认实现边界。已有 Datapier 0.5.1 受管登记的隔离 stdio 握手返回 23 项工具。0.5.2 的界面、固定入口与 MCP 文档按本节验收，各平台分别记录实际运行结果。 [0.5.1 运行基线](technical-design-0.5.1.md#5-验收矩阵) · [仓库验证约定](../CONTRIBUTING.md#测试与验证)
