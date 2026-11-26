# Datapier 0.5.1 技术方案

## 1. 目标与范围

在 Tern 同一个 tab 内，从项目 block 切到 Datapier 时，Files 应继续显示该项目目录。用户在 Datapier 的查询文件对话框输入 `sql/check.sql` 时，打开和保存应指向同一份项目文件，即使同一 Engine 同时服务多个项目。共享 Engine 的工作目录继续由安装缓存决定。[0.5.0 技术方案](technical-design-0.5.0.md)

当前插件把已校验的二进制缓存目录传给 Tern 作为 pane 的 `cwd`，以便使用 `./datapier` 或 `.\datapier.exe` 启动。Files 在跟随模式下使用焦点 block 的目录，因此会跳到缓存。查询文件的路径目前原样交给 Engine 读写，输入相对路径时实际取决于 Engine 的进程目录。[当前启动入口](../plugins/datapier/lib/bootstrap.luau) · [查询文件 RPC](../crates/datapier-engine/src/server/rpc.rs) · [Tern Files 跟随规则](https://docs.stencil.so/tern/scripts/files.md)

本次调整 Tern 插件启动、受管 MCP 的 Engine 冷启动、查询文件路径和 Datapier block 的鼠标反馈。查询、数据交换、项目和证书等常用路径字段提供「浏览…」，在当前 block 内选择文件或目录。进入工作台时，空白区域显示默认箭头，点击目标、文本编辑区和现有拖拽手柄分别显示对应的鼠标形状。

## 2. 用户交互与路径契约

| 场景 | 预期行为 |
|---|---|
| 从本机 `/repo/app` 执行 `New Datapier block` | 新 tab 中的 Datapier 以 `/repo/app` 启动。通过 Files 入口分屏打开，或手动将 Datapier 移到项目 tab 后，同目录的两个 block 切换焦点时 Files 保持 `/repo/app` |
| 点击 Files 的「Datapier MCP 项目」 | 在当前 tab 定位已有的 Datapier。当前 tab 没有工作台时，在本机来源 pane 旁分屏打开，以 `files.root` 启动，就绪后自动定位该目录的 MCP 项目 |
| 在这次打开的 Datapier 中输入 `sql/check.sql` | 「打开」和「保存」均使用 `/repo/app/sql/check.sql`。文件对话框显示相对路径的基准目录 |
| 来源 block 后续 `cd` 到 `/repo/other` 或 Datapier pane 被移到另一个 tab | 已打开的 Datapier 继续使用 `/repo/app`。Tern 仍按焦点 block 显示目录，聚焦它时 Files 可能回到 `/repo/app`。从新目录重新打开 Datapier 才取得新的基准 |
| 查询文件路径已是绝对路径 | 直接使用该路径 |
| 无法确认来源是有效的本机目录 | 仍按现有本机启动路径打开工作台，查询文件沿用原有路径行为。远端路径即使在本机有同名目录也不会继承 |
| 来源目录在 Datapier 打开后失效 | 下次文件操作显示实际读写错误，基准目录在本次 pane 生命周期内保持固定 |

插件保存发起操作时的目录快照。Files 入口同时记录来源 pane 与 tab，异步检查结束后重新核对其身份。切换焦点不会改变本次打开的目录或分屏位置。无法在原 tab 安全分屏时，提示用户回到可用的本机 block 重试。[Tern 窗口与布局 API](https://docs.stencil.so/tern/guides/layout.md)

## 3. 实现设计

### 3.1 Tern 启动

1. 在 `bootstrap.start` 开始时记录来源 pane、tab 和工作目录。命令面板、Carly 打开、待办缺少工作台时走 `hosts:terminal("local", …)`，新开本机 tab。普通入口通过公开宿主 API 检查来源 pane 和目录归属。pane 归属本机且路径在本机存在仍不足以确认 SSH 跳转后的目录归属。缺少归属信息时保守使用第 2 节的本机启动路径。[Window API](https://docs.stencil.so/tern/reference/api-window.md#sessions-and-hosts-cx)
2. Files 项目按钮使用回调的 `files.host`、`files.root` 确认本机目录，其导航候选仅限发起操作的 tab。没有可用工作台时，通过 `cx.layout:split` 在该 tab 的本机来源 pane 旁打开，传入确认后的 `files.root`。异步校验完成后，复核来源 pane 仍在原 tab 且属于本机，并重新检查该 tab 是否已有工作台。同一 tab 的重复点击复用正在打开的 pane，使用最后一次有效的目录请求定位。复用 `navigate_when_ready`，让新 pane 完成桥接绑定后直接定位项目。来源失效或分屏失败时显示原因。[窗口入口](../plugins/datapier/window.luau) · [窗口导航桥接](../plugins/datapier/lib/window_bridge.luau) · [Tern 分屏 API](https://docs.stencil.so/tern/guides/layout.md#arranging-panes)
3. 使用 `bootstrap.executable()` 返回的已校验绝对路径生成 `Launch.command`，将来源目录传给 `Launch.cwd`。继续传递更新门禁确认的 `--data-dir-b64`。有来源目录时，用内部 `tsp --query-base-b64` 传递同一份目录快照，避免登录 shell 的初始化过程影响查询基准。仅允许绝对 UTF-8 目录进入该参数，错误在启动边界报告。
4. `Launch.command` 经登录 shell 解析。Unix 使用 shell 字面量引用，覆盖空格、单引号、反斜杠和 Unicode。Windows 使用可信系统目录中的 Windows PowerShell 5.1 绝对路径和 `-EncodedCommand`，由 UTF-16LE 编码的命令把已校验的绝对路径作为单个程序调用并传递退出码，外层兼容 `cmd` 和 PowerShell。项目 cwd 仅通过 `Launch.cwd` 传递，程序路径和参数均作为字面值处理。实际支持组合以对应候选上的运行验收为准。[Launch 参数](https://docs.stencil.so/tern/reference/api-window.md#launch) · [现有安装约定](../CONTRIBUTING.md#平台与安装)

### 3.2 Engine 冷启动

插件启动 TSP 和登记受管 MCP 时附加同一个内部 `--plugin-cache-engine` 参数。CLI 从当前可执行文件的绝对路径取得安装缓存目录，经两个入口各自的 Engine 客户端选项传递。`Client::connect_with` 仍先按绝对数据目录连接现有实例。需要创建候选 Engine 时，才在子进程 `Command` 上显式设置 `current_dir`。普通 CLI、TUI、手工配置的 MCP 和没有该内部参数的入口沿用现有启动方式。已运行的 Engine 继续服务现有连接，退出后下一次由受管入口冷启动才采用该目录。[CLI 入口](../crates/datapier/src/main.rs) · [Engine 连接](../crates/datapier-engine/src/lib.rs) · [Engine 子进程](../crates/datapier-engine/src/platform.rs)

受管 MCP 复用既有登记所有权与中断恢复逻辑，把原有受管 `args` 更新为带内部参数的版本。用户修改过的同名 MCP 项保持现有保护。数据目录、IPC 端点、实例锁、工作区 ID 及 MCP 仓库上下文沿用现有来源。窗口桥接和本机外部编辑器保持原来的启动路径。[受管 MCP 登记](../plugins/datapier/lib/mcp_registration.luau) · [MCP 入口](../crates/datapier-mcp/src/lib.rs) · [窗口桥接](../plugins/datapier/lib/window_bridge.luau)

### 3.3 查询文件路径

TSP 启动时保存可选的查询基准目录，生命周期与 pane 一致。查询文件对话框共用一条路径解析逻辑。有基准时，普通相对路径用平台 `Path` 语义拼到基准目录，原有绝对路径直接传递。Windows 的驱动器相对路径 `C:check.sql` 和缺少驱动器的根相对路径 `\check.sql` 在有基准时提示用户输入明确的相对路径或绝对路径。文件尚未创建时也能解析保存目标，最终文件系统操作沿用 Engine。没有基准时保留原路径。打开期间编辑变化的保护、现有保存语义和错误处理继续生效。[查询文件操作](../crates/datapier-tsp/src/action.rs) · [文件对话框](../crates/datapier-tsp/src/views/dialogs.rs) · [查询文件回执](../crates/datapier-tsp/src/response.rs)

### 3.4 接口与升级

内部启动参数由同版本插件与二进制配合使用。查询文件 RPC 继续使用现有 `path` 字段，插件场景在发送前完成绝对路径解析。方案沿用 IPC v6 和配置 schema 6，目录快照只保存在运行中的 TSP 实例，已有连接、草稿、查询文件及项目配置无需迁移。

发布安装和 Datapier 更新流程使用成对的插件及二进制。已完成升级后，Datapier 的更新流程拒绝较低版本的目标，并提示「请安装当前版本或更新版本」。受管 MCP 登记遇到较新版本的记录时，保留记录及当前启动字段，并提示「请升级 Datapier」及所需版本。用户通过 Tern 强制安装 0.5.0 时，安装行为由 Tern 管理，0.5.0 插件保留其现有拒绝覆盖和版本提示。更新事务尚未提交时的失败恢复继续走既有一致恢复流程。[受管版本保护](../plugins/datapier/lib/mcp_registration.luau) · [0.5.0 更新边界](technical-design-0.5.0.md#55-plugin-版本检查与自动更新边界) · [发布指南](../RELEASING.md#版本号与关联文件)

### 3.5 工作台鼠标反馈

工作台根布局目前挂有 `result-info-dismiss` 点击动作，用于收起结果说明。Tern 会给挂有点击动作的节点加 `.sf-act` 并显示手形，因此根节点是实现时优先检查的位置。保留点击空白处收起结果说明的行为，在现有工作台样式表里对根布局和相应背景节点设置 `cursor: default`。规则作用于 Datapier 自己的节点，子控件继续按各自的交互语义显示鼠标形状。[根布局与分隔条](../crates/datapier-tsp/src/views.rs) · [工作台样式](../crates/datapier-tsp/src/styles/shell-navigation.css) · [Tern 鼠标样式规则](https://docs.stencil.so/tern/styles/index.md#on-node-elements)

- **默认箭头** `default` 用于工作台空白、静态标题、普通背景和禁用控件。点击一处背景能收起临时提示时，背景仍显示箭头。
- **手形** `pointer` 用于启用的按钮、标签、可点击资源行和菜单项。优先复用 Tern 原生控件和节点动作的鼠标样式。
- **拖拽形状** 由现有拖拽目标和方向决定。SQL 编辑器与结果区、Redis CLI 的上下分隔条显示纵向调整形状，左右分隔条显示横向调整形状。复用 Tern `split` 的原生手柄，鼠标离开手柄或结束拖拽后按当前命中区域恢复形状。
- **文本光标** `text` 用于 SQL 编辑器和可编辑文本输入，保留点击定位及文本选区的现有反馈。

实施时以实际 Tern 的命中结果核对 `cursor` 的优先级，并把必要的覆盖限定在对应节点。本项沿用已有拖拽能力。[Tern `split`](https://docs.stencil.so/tern/elements/layout.md#split) · [Tern 支持的 CSS](https://docs.stencil.so/tern/styles/css.md)

### 3.6 文件与目录选择

路径字段使用 TSP 原生 `picker` 节点，在当前 Datapier surface 的 `layer` 打开。Tern 渲染列表、选中态和操作栏，Datapier 提供本机目录条目、维护筛选和选择状态。输入框保留手动输入、粘贴和修改已选路径的能力。[TSP Picker](https://docs.stencil.so/tern/elements/lists.md#picker)

| 模式 | 入口与回填规则 |
|---|---|
| 选择文件 | 打开查询 SQL、导入单值或项目归档、选择 CA／客户端证书及私钥。进入目录继续浏览，确认文件后回填绝对路径 |
| 选择多个文件 | 数据交换批量导入 CSV／JSON 和 SQL 恢复。单个输入行的「浏览…」替换该行路径，批量入口按选择顺序追加输入行，保留已配置行及各文件的格式、映射和选项。文件上限与校验沿用当前业务规则 |
| 选择目录 | 打开项目和关联本机仓库时回填现有目录。新建项目或导入到尚不存在的目标目录时，先选已有父目录，再在原表单填写新目录名。多对象导出需要目录时，按原有输出目录规则回填 |
| 选择保存位置 | 保存查询 SQL、导出结果或单值、SQL 备份、导出项目归档和遗留资源时，先选目标目录，再在原表单填写或修改文件名。已有文件名时保留文件名，回填所选目录下的完整目标路径。是否覆盖由实际保存或导出流程判断 |

打开时优先使用字段中可浏览的绝对目录，或已填绝对文件路径的父目录。查询文件的手输相对路径按第 3.3 节解析后可用于定位起始目录。其余相对路径沿用各字段已有的操作契约，picker 只在调用方有明确基准时使用它们定位。字段为空或无法定位时，依次使用打开 Datapier 时确认的本机来源目录和本机用户主目录。选择结果作为绝对路径回填，项目清单中要求项目相对路径的字段继续使用既有项目范围与表示。文件扩展名筛选随用途设置，目录始终可进入。列表只展示当前目录，支持返回上级、名称筛选和键盘导航。[查询文件对话框](../crates/datapier-tsp/src/views/dialogs.rs) · [数据交换表单](../crates/datapier-tsp/src/views/transfer_workbench.rs) · [项目路径表单](../crates/datapier-tsp/src/views/project_workbench.rs)

在 `datapier-tsp` 新增共用路径选择状态和视图。调用方提供模式、字段身份和初始值，接收结果后沿用字段更新及失效逻辑。TSP 的 `picker` 搜索行由应用更新 `query` 和选中项，通过 `select`、`activate`、`action` 及键盘事件处理选中、进入目录、确认和取消。选择器打开时接管对应按键，关闭后焦点回到原字段。取消、切换表单或关闭 pane 时释放选择状态并丢弃迟到回执。[TSP 输入事件](https://docs.stencil.so/tern/protocol/input.md)

在 TSP 后台异步读取本机目录并通过既有唤醒机制交回 UI。一次只读取当前目录，限制单次下发条目，超量时分批加载或提示缩小筛选范围。用请求代次和目录身份隔离连续导航回执。空目录、权限不足、目录变化和无法无损表示的路径有明确反馈。枚举、筛选和回填只操作路径。打开、保存、证书读取、导入、导出和覆盖仍由现有业务入口在用户确认后执行。该路径适用于插件和独立 `datapier tsp`，沿用现有 IPC、文件访问和项目目录校验。[TSP 后台](../crates/datapier-tsp/src/backend.rs) · [TSP 视图入口](../crates/datapier-tsp/src/views.rs) · [TLS 字段](../crates/datapier-tsp/src/views/profile_form.rs) · [单值文件表单](../crates/datapier-tsp/src/advanced/files.rs)

## 4. 实施顺序

| 步骤 | 对应文件与交付内容 |
|---|---|
| 1 | `crates/datapier/src/main.rs` 接收内部参数，`datapier-tsp/src/runtime.rs` 和 `backend.rs`、`datapier-mcp/src/lib.rs` 将 Engine 启动目录传到 `datapier-engine/src/lib.rs`，由 `platform.rs` 在创建子进程时应用 |
| 2 | `plugins/datapier/lib/bootstrap.luau` 读取目录快照并生成平台命令，`window.luau` 与 `lib/window_bridge.luau` 接通 Files 的同 tab 分屏和项目定位，`lib/mcp_registration.luau` 更新受管参数与版本提示 |
| 3 | `crates/datapier-tsp/src/action.rs` 解析查询文件路径，`views/dialogs.rs` 显示基准目录，新增文案按现有 i18n 规则提供简体中文和英文 |
| 4 | 在 `crates/datapier-tsp/src/views.rs` 和对应的工作台样式中核对背景、点击动作、编辑输入和分隔条的鼠标样式，保留现有操作 |
| 5 | 在 `datapier-tsp` 实现共用路径选择器、后台目录读取和表单回填，接入查询、数据交换、项目、TLS 及单值文件入口，按现有 i18n 规范提供简体中文和英文 |
| 6 | 用隔离数据目录执行下列验收。实现完成后，按仓库规范同步已实现行为和发布变更记录 |

## 5. 验收矩阵

本轮实机验收基线为 **Tern 0.9.36（`7123a24`），macOS 27.2.1 arm64**。以该环境运行 0.5.1 候选，在隔离数据目录记录实际操作和结果。Linux、Windows 与其他架构的设计继续适用，运行结果待对应环境核验，发布要求见发布指南。

| 范围 | 通过标准 |
|---|---|
| Files 与焦点 | 命令面板默认开新 tab。Files 项目入口在当前本机 tab 复用工作台或分屏打开并定位项目，其他 tab 的工作台保持原位。未固定 Files 根目录时，在同目录的项目 block 与 Datapier 间切换焦点，Files 保持同一根目录 |
| 快照与入口 | 安装或更新检查尚未完成时切换焦点，命令面板、Carly 和待办新开仍使用原目录。Files 项目入口仍在发起操作的 tab 分屏，来源 pane 关闭或移走时显示原因。已有 Datapier 的查询基准保持固定 |
| 查询文件隔离 | 两个项目 pane 共用同一个 Engine，各自保存并打开同名相对路径，逐一读回实际文件。覆盖新建文件、绝对路径及失败路径。查询文件操作不会写入安装缓存里的同名文件 |
| Engine 冷启动与复用 | 分别由插件 TSP 和受管 MCP 先启动，核对实际 Engine cwd 为相应二进制的缓存目录、绝对数据目录及实例身份一致，再接入另一客户端。已有手工启动的 Engine 保持复用，用户自定义 MCP 项保持原值 |
| 版本保护 | 完成升级并发布受管 MCP 后，较低版本目标被 Datapier 更新流程拒绝并提示当前或更新版本。受管登记遇到较新记录时保留 MCP 路径和参数，核对 0.5.1 的升级提示以及 0.5.0 已有的版本提示。升级提交前的失败按既有回滚契约复验 |
| 本机与远端 | 本机 cwd、无 cwd、已失效 cwd、远端 tab 和本机 shell 上的 SSH 跳转分别验证，确认无效或远端路径未用于本机启动和查询文件解析 |
| 启动命令 | 在基线环境用当前登录 shell 启动。安装目录与项目目录覆盖空格、中文和该 shell 的特殊字符，确保 TSP 握手成功且实际启动的是已校验的二进制 |
| 鼠标反馈 | 初次进入及切回 Datapier，在真实 Tern 依次移到背景、启用和禁用控件、SQL 编辑器、可拖拽分隔条及相邻 block，核对箭头、手形、文本和方向调整形状。实际拖动 SQL 和 Redis CLI 分隔条，确认调整生效且松开后恢复当前区域的鼠标形状。点击空白处仍能收起结果说明 |
| 路径选择器 | 在真实 Tern 中通过键盘及鼠标选择文件、批量选择、选择目录和保存位置。分别核对已有路径、来源目录和本机用户主目录。覆盖中文、空格、空目录、权限错误和取消。回填后读回实际 SQL、核对批量输入行和项目路径。浏览和取消期间文件与原表单值保持原状，选中后只按确认的范围回填，保存与覆盖仍走原流程 |
| 回归 | 验证更新门禁、受管 MCP 登记与真实握手、外部编辑器交接，以及相对路径配置的 TLS 证书和数据交换仍按原有路径契约执行 |

本文记录设计及通过标准。功能实现后记录实际运行结果及候选二进制身份。[测试与验证](../CONTRIBUTING.md#测试与验证) · [发布前检查](../RELEASING.md#发布前检查)
