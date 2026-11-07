# 贡献指南

## 文档职责与阅读顺序

修改前先读 [README](README.md)，再按任务阅读对应设计与规则。本文是文档职责和开发规范的权威入口；各文档引用本节，不再各自维护一份职责规则。以实际文件和运行证据确认实现状态，规划路径不能当作已存在的模块。

| 文档 | 负责内容 | 不负责内容 |
|---|---|---|
| [README](README.md) | 当前能力、安装使用入口、支持范围和主要限制 | 完整开发规则、发布步骤或验收流水 |
| 本贡献指南 | 开发环境、编码与模块边界、验证要求、文档职责 | 版本发布操作细节 |
| [发布指南](RELEASING.md) | 产品版本派生、组包、发布流程及发布前后核对 | 逐条功能说明和开发排查日志 |
| [CHANGELOG](CHANGELOG.md) | 相对已发布版本的用户可见功能、改进、修复和兼容性变化 | 新功能发布前的内部修正、代码重构、设计稿同步或每轮验证结果 |
| [产品规划](docs/product-plan.md) | 产品目标、交付范围、路线和验收方向 | 控件尺寸、文案、字段级交互及源码实现细节 |
| [0.1.0 技术方案](docs/technical-design-0.1.0.md)、[0.1.1 技术方案](docs/technical-design-0.1.1.md)、[0.2.0 技术方案](docs/technical-design-0.2.0.md)、[0.3.0 技术方案](docs/technical-design-0.3.0.md) | 对应版本的架构、数据与接口契约、决策理由、迁移、验收要求及简洁结果摘要；当前实现与未验收范围以实际代码和对应版本运行证据为准 | 每轮修复和候选构建的流水账 |
| [UI 设计规范](docs/ui-guidelines.md) | 从原设计整理的视觉、组件、文案、响应式和状态约定；新增界面的共同基准 | Engine 接口与版本范围 |
| [UI 设计说明](docs/ui-redesign.md) | 具体功能的操作语义与交互约束 | 重复通用设计规范、平台接口契约或运行日志 |
| [HTML 设计稿](docs/ui-redesign.html) | 与 UI 说明一致的可交互示例 | 真实数据库执行、持久化或原生验收证据 |
| [Archery 调研](docs/archery-integration-research.md) | 上游能力、版本差异和可追溯的源码依据 | 本地实现的实时完成状态 |
| [AGENTS](AGENTS.md) | Agent 阅读入口、执行授权和交付约束 | 重复上述工程与文档规则 |

Changelog 按最后一个已发布版本比较：新功能在首次发布前的修正合并到最终功能描述；只有修复已发布行为的问题才进入“修复”。日常变更直接归入根产品版本对应的章节，标题统一为 `## X.Y.Z`；版本调整时新增对应章节，不维护“待发布／已发布”标记。混合条目应拆分，兼容性变化独立说明。详细排查与开发验证留在 PR、提交或 CI 日志；技术方案只汇总必要的候选、范围、结果、限制和证据索引，不接收从 Changelog 搬来的逐轮修复清单。

用户的新要求优先于历史方案；范围或架构变化时同步相关文档。功能任务按完整可运行路径交付，不留下空壳、假结果或未接线接口。仅涉及文档的任务不顺带创建 workspace、plugin 或数据库。

当前开发版本 0.3.0 承接 0.2.0 的查询／导出／入口实现，增加结构及高级编辑；历史 0.1.x 契约保留在对应方案，不作为当前行为。继续遵守类型、提交、凭据、隔离与验证约束。

## 开发环境

- 使用 stable Rust 和 Cargo；workspace 使用 edition 2024，六个 crate 统一继承 `rust-version = 1.94`，与锁定的 SQLx 0.9 最低要求一致。macOS 可以使用 Homebrew 安装 Rust／Cargo，不强制引入 rustup；Linux 和 Windows 还需本地链接工具，Windows 使用 MSVC 构建工具及目标架构的链接库。
- Linux 目标使用 GNU／glibc；现有 Ubuntu 原生构建配置安装 `pkg-config` 和 `libdbus-1-dev`。不同发行版使用对应的系统开发包。
- 应用程序的 `Cargo.lock` 固定依赖并随源码提交；官方 `tern-sdk` 使用 registry 的 `0.2.0`，版本在 workspace 中精确固定。六种目标使用一致的锁定依赖。
- 产品版本的权威源、源码 plugin 同步命令和发布包派生方式见[发布指南的版本号与关联文件](RELEASING.md#版本号与关联文件)。普通 Cargo 构建不改写 plugin manifest；版本调整需运行同步与只读检查，并由 Cargo 更新锁文件。
- 开发发布脚本及其回归使用 Python 3.15，仅依赖标准库；CI 显式配置该 minor 版本并跟随补丁更新。本地 macOS／Linux 可用 `python3`，Windows 可用已安装的 `python` 或 `py -3.15`。Datapier 安装和运行无需 Python；Windows 用户安装器仍使用系统自带 Windows PowerShell 5.1，日常开发与组包无需安装 `pwsh`。
- 使用 Tern 验证真实 TSP 界面；Luau 由 Tern 内嵌执行，无须单独安装 Luau runtime。修改启动 plugin 时，可用目标版本的 `tern plugin types` 生成声明作参考；真实行为以 Tern 加载和启动为准。LSP 不是强制开发条件；格式化按[代码格式化](#代码格式化)执行。
- 当前开发验收基线为 macOS arm64、Tern 0.9.12 与 tern-sdk 0.2.0，不维护旧 SDK 兼容层。应用以 sans 打开 surface，使用公开 `button`、`field`、`select`、`switch`、`menu` 和 `overlay`。表单与主操作保留明确控件层级；连接／资源行、标签、grid 和状态入口按唯一 HTML 设计采用局部命名 CSS，维护对齐、选中、禁用与键盘焦点，禁止全局覆盖表单控件。右键菜单使用公开结构化条目，外壳和定位由宿主管理。尚无类型化 builder 的公开 prop（如 editor 的 `nowrap`）通过通用 prop 写入并在宿主实测。启动 plugin 和命令面板使用内置 `database` 图标。
- 若仓库已有 `.codegraph/` 且工具可用，优先借助 CodeGraph 定位符号与调用关系；无索引时直接检索代码，不自行建立索引。

构建本地程序，启动普通终端 TUI 或在 Tern 中启动 TSP：

```sh
cargo build --locked -p datapier
./target/debug/datapier tui
tern -e "$PWD/target/debug/datapier" tsp
```

也可链接启动 plugin，从带有本地程序路径的环境启动 Tern：

```sh
tern plugin link "$PWD/plugins/datapier"
DATAPIER_BINARY="$PWD/target/debug/datapier" tern
```

已运行的 Tern 进程不会自动获得新环境变量；命令面板入口为 `New Datapier block`。修改 Rust 后重新构建并运行；修改 Luau 后由 Tern 重载启动入口。

发布安装所需的版本校验、下载覆盖和缓存配置见[发布指南的构建与组包](RELEASING.md#构建与组包)。

`--data-dir` 或 `DATAPIER_DATA_DIR` 可指定独立用户数据目录，`tui --workspace NAME` 选择工作区。Unix 数据目录必须属于当前用户且权限为 `0700`；不接受已有共享目录，也不自动修改其权限。密码不写入 SQLite：连接表单尝试保存到系统凭据库，服务不可用时仅保留在当前 Engine 会话内存。

## 编码约定

以下编码与注释要求适用于本次新增或修改的代码，不为补齐规范扩大修改范围。

- 沟通、文档和提交说明使用简体中文；代码标识符和技术名称保留 English，标准许可证保留原文。
- 修改前阅读相关实现与调用方，遵循项目既有的命名、目录、分层、错误处理和测试约定。
- 优先简单设计和既有实现；围绕当前需求做最小完整修改，避免无关重构、额外功能及为未来版本预建框架。
- 优先复用标准库、已有依赖和公共能力；依赖按方案选型并锁定，新增依赖前确认必要性，评估维护状态、许可证、体积与传递依赖成本。调整依赖需考虑六种目标及许可证。
- 保持模块、类型和函数职责清晰，业务逻辑放在对应职责层；避免入口层堆积业务、万能工具类和职责混杂的长函数。
- 避免复制业务逻辑；抽象应服务于明确的复用或隔离需求，不为假设中的未来需求增加层次。
- 使用类型和明确的数据结构表达业务约束；避免含义模糊的布尔参数、魔法值、字符串状态和隐式约定。
- 在输入边界校验不可信数据，在业务归属层维护权限、状态与一致性约束；不只依赖界面校验。
- 显式处理失败和边界情况，不吞错误、不将失败伪装成成功，不用静默兜底掩盖缺失实现；错误保留原因和必要上下文。
- 明确资源、并发任务与共享状态的所有权及释放时机；共享锁不跨慢查询、阻塞操作或不受控的外部调用持有。
- 避免不必要的分配、复制、重复计算与全量读取；大数据按需、分页或分批处理，不以复杂优化替代清晰设计。
- 日志沿用项目统一机制，提供必要定位信息，不记录凭据、Token 或其他敏感数据；交付前清理临时调试输出。
- 修改接口或行为时更新全部受影响的调用方、相关测试、文档与注释，清理被替代的实现；不保留无需求的兼容分支。

## 代码可读性与注释

- 优先通过准确命名、清晰结构和适度拆分表达意图；不用大段注释解释本可整理清楚的混乱代码。
- 新增或实质修改复杂逻辑时，必须主动补充必要注释，不等待用户另行要求。以下场景必须说明相关背景或约束：
  - 非显然的业务规则、特殊分支及其业务背景。
  - 算法或实现方案的关键取舍，以及不能使用直观方案的原因。
  - 并发时序、锁约束、资源生命周期与状态迁移的不变量。
  - 协议、平台、第三方库或外部系统的特殊行为与限制。
  - 精度、编码、边界值及其他容易被误改的数据处理约束。
- 注释重点解释“为什么”、成立条件与重要后果；不逐行翻译代码，不为简单赋值、直观分支或机械转发添加注释。
- 保留仍然有效的业务背景和历史原因；workaround 与特殊常量应说明依据、适用范围及已知的移除条件，有对应 issue、协议或上游文档时附引用。
- 对外暴露的非平凡模块、类型和接口应提供文档注释，说明职责、必要前置条件、返回与错误语义及重要副作用；按需补充并发、资源和性能约束，不机械罗列显然信息。
- 私有实现中无法仅凭命名和代码理解的约束，也必须就近说明；不以“内部实现”为由省略关键上下文。
- 整体约束写在模块或类型层面，局部原因写在对应接口、分支或操作附近；避免多处重复维护同一说明。
- 注释与代码同步更新；行为变化时修正失效描述，删除代码时清理失去对应对象的注释。
- 注释使用简体中文，标识符和技术术语保留 English；遵循语言生态的文档格式，Rust 接口使用 rustdoc。
- 涉及 unsafe 时，按 Rust 约定在相邻位置通过 `SAFETY` 注释说明安全依据；对外暴露的 unsafe 接口通过 `# Safety` 文档说明调用方必须满足的条件。
- 交付前检查本次变更中依赖背景知识才能理解的逻辑，确认关键原因与约束已记录；不以注释数量、行数或覆盖率代替可读性判断。

## 代码格式化

### 配置与工具职责

[dprint.json](dprint.json) 是 dprint 插件版本、格式参数、包含和排除规则的唯一来源；仅使用固定版本的 Wasm 插件，禁止添加、使用或间接调用 Exec 插件。配置沿用现有空格缩进与 LF；各语言差异由配置表达，不在其他文档或编辑器中复制参数。

| 当前文件范围 | Formatter 与边界 |
|---|---|
| Rust | 原生 `cargo fmt`／rustfmt，沿用 workspace edition 和当前 stable toolchain，不通过 dprint 调用 |
| HTML、JavaScript、CSS（含 HTML 内嵌脚本与样式） | dprint；JavaScript 沿用 TypeScript Wasm 插件，不引入 TypeScript 构建 |
| JSON／JSONC 配置、YAML、TOML（含 Cargo manifest 与 plugin 配置） | dprint |
| 独立 SQL | dprint，dialect 为 SQLite；当前文件是本地持久化 migration，不按 MySQL provider 选择方言 |
| POSIX Shell、Luau | dprint 的 Wasm 插件，无须另装 shfmt 或 StyLua CLI |
| Python 开发发布脚本及回归 | dprint 的 Ruff Wasm 插件，无须另装 Ruff CLI；参数与目标版本以 `dprint.json` 为准 |

当前没有 Java 或 XML 源文件，不预装相应插件；保留的 Windows PowerShell 安装器不接入格式化。Markdown 不属于当前 dprint 范围。新增语言前核对[官方插件目录](https://dprint.dev/plugins/)的实际支持并更新项目配置，不假定 XML 能格式化 MyBatis SQL，或原生语言 Formatter 能处理字符串中的 SQL。

Windows 安装器由 Windows PowerShell 5.1 读取，源码保留 UTF-8 BOM，以正确识别中文；交给启动 plugin 的 stdout JSON 则固定为无 BOM 的 UTF-8。

dprint 默认复用仓库 `.gitignore`，配置另排除依赖、第三方目录及生成文件；不使用忽略注释或排除规则掩盖格式化失败。当前配置 schema 为 4，查询结果不再写入结果缓存；SQL 格式整理必须保持迁移语义，验证新建、升级和拒绝较新基线。Rust、HTML 或 Shell 字符串中的 SQL 不交给独立 SQL 插件处理。

### 本地执行与 AI 修改要求

修改前阅读适用规范并沿用附近实现；修改后显式列出本次任务改动的文件，优先只格式化这些文件，不包含用户已有的无关修改。以下命令均从仓库根目录执行，文件路径替换为本次实际变更：

```sh
dprint fmt --config ./dprint.json --config-discovery=false -- path/to/changed-file
dprint check --config ./dprint.json --config-discovery=false -- path/to/changed-file
```

Rust 单文件使用同一套原生 rustfmt，避免递归改写未变更的子模块；全 workspace 使用现有 `cargo fmt` 做只读检查：

```sh
rustfmt --edition 2024 --config skip_children=true path/to/changed.rs
cargo fmt --all --check
```

不默认运行全仓写入式 `dprint fmt` 或 `cargo fmt --all`；历史格式差异单独报告，批量迁移需有明确任务范围。使用上述仓库命令，不自行更换 Formatter、传入另一套插件或覆盖项目格式参数。编辑器若启用保存格式化，同类文件只能使用本节指定的 Formatter 并读取项目配置，避免与 Prettier、其他 StyLua 配置或个人全局配置重复处理。

### 自动化与检查边界

可执行的 CI 命令和 dprint CLI 固定版本维护在[现有工作流](.github/workflows/native-release.yml)的 `format` job；本地使用与该 job 一致的 CLI 版本。工具缺失时可按该 job 的安装命令或[dprint 官方安装说明](https://dprint.dev/install/)安装；本仓库不引入 npm 项目依赖、通用 Formatter 分发脚本或提交 Hook。

Rust 运行原有全 workspace 格式检查；dprint 检查 PR base／push before 到当前提交之间仍存在的新增、复制、修改或重命名文件。没有有效 before 时检查当前提交相对父提交的变化，根提交检查全部文件；手动触发也遵循该规则。`--allow-no-files` 仅处理变更中没有 dprint 适用文件的情况，不忽略解析错误、插件错误或格式差异。没有全仓格式迁移时，不以全仓 dprint 检查阻塞无关历史差异。

Formatter 仅负责格式，不替代 Clippy、类型／编译检查或行为验证；仍按[测试与验证](#测试与验证)执行适用检查。CLI 参数以[官方 CLI 文档](https://dprint.dev/cli/)为准，项目范围和插件配置以仓库文件为准。

## 模块边界

各模块遵循以下职责边界；新增模块必须随完整可运行路径交付：

| 模块 | 职责 |
|---|---|
| `crates/datapier` | 单一可执行文件和 `tui`／`tsp`／`mcp`／`engine` 子命令分派 |
| `crates/datapier-core` | 类型、资源、数据库会话、provider、执行、变更、持久化与凭据；不依赖 TSP、Luau 或 MCP |
| `crates/datapier-engine` | 唯一运行状态所有者，IPC server／client、租约、实例锁、平台适配和收尾 |
| `crates/datapier-tsp` | 编辑器、grid 和 UI 状态；通过 Engine client／IPC 访问数据，不直接连接数据库 |
| `crates/datapier-tui` | 普通终端中的数据源、认证和设置管理，通过 Engine IPC 访问数据 |
| `crates/datapier-mcp` | MCP stdio tools、协议适配与会话管理，通过 Engine IPC 访问数据 |
| `plugins/datapier` | 薄 Luau 启动包及安装脚本；不复制数据库或 grid 业务 |

MySQL／Redis provider 编译期注册，保留各自语义。后续数据库支持需有类型、交互和真实运行证据，不能仅注册驱动就声明支持。首版不实现动态库加载或外部 provider 进程。

六个 crate 按运行边界划分。单个 crate 内优先按功能维护私有模块；入口只保留分派和状态组合，纯计算逻辑可独立为私有模块。只有出现独立消费者、明确依赖隔离或独立版本需求时再评估新 crate，不按文件行数拆包。

TSP 的 `lib.rs` 组合共享状态与私有模块，`app` 管理初始化和字段，`workspace` 管理快照及标签运行态，`requests` 管理请求路由；`workbench`、`navigation`、`grid` 和视图按功能细分子模块。Engine 的 `server` 将 RPC、任务执行和 Archery 认证分开，仍共用唯一运行状态；Core 的 `storage` 与 `transfer` 分别按存储职责、导入导出路径组织。测试沿相应职责归类，共用 fixture 与构造器留在测试父模块。
HTML 设计稿保留 `docs/ui-redesign.html` 为唯一入口，静态资源维护在 `docs/assets/ui-redesign/`，按既定顺序加载经典脚本和样式，支持直接打开与 HTTP 预览，不引入打包步骤。

## 不可破坏的行为

### 执行与数据

- 大整数、精确小数、JSON 大数和二进制必须保真；NULL、空字符串及 Redis 不存在值分开表达。显示格式不得参与写回转换。
- 结果不持久化，按需读取并保留有界会话内存；TSP 对安全只读源显式重查导出，其他结果只导出保留范围。不静默重放可能有副作用的语句，不把部分结果标成全量。0.1.x 历史行为见对应方案。
- 编辑先形成草稿和变更集，显式提交后才写库；冲突不静默覆盖，过期编辑基线不能直接恢复提交。
- 只读策略在 Engine／Core 统一执行，不仅依赖 UI、字符串前缀或未来的 MCP annotations。
- 写入／COMMIT 发送后失联，无法确认的结果标记待确认；不自动重试或重放。取消等待、关闭连接与服务端撤销是不同状态。

### MySQL

- 保留 SQLx；普通语句在同一专属连接上 prepare 取得列定义，再执行并消费结果，普通空结果集保留列信息。
- 动态结果按实际元数据处理；驱动无法提供的空动态结果列定义明确标为未知，不伪造 schema。
- 当前语句使用 tokenizer、注释／引号和 `sql_mode` 定位，禁止按分号盲切。
- 基表行编辑要求可靠完整键和原值检查；无键表、视图及任意 SQL 结果按首版范围只读浏览。
- 查询 tab 的连接与事务隔离；InnoDB 与非事务表分别处理提交语义。KILL QUERY 的取消竞态处理完成前，目标连接不能执行下一条命令或回池。

### Redis

- key／field／member 保留字节，普通连接按 DB 隔离；multiplexed connection 的 clone 不代表独立物理连接。
- 可编辑 key 先 WATCH 再读取基线；List index 只在有效 edit token 内使用，不能按原值误删重复元素。
- 修改值默认保留 TTL；区分不存在、永久及有限期限，导入导出保留过期语义。
- SCAN 允许重复和空批次，cursor 为零才结束；不承诺全库排序或一致性快照。EXEC 冲突不自动换基线重试，单项执行错误不解释为整组回滚。

### TSP 与 Engine

- TSP Session 由 UI 线程独占，同步输入循环不阻塞 Tokio；IPC、心跳独立运行，队列配合 SDK waker 通知 UI，不把唤醒返回的 `None` 直接当 EOF。
- 使用公开 TSP 元素、稳定 key 和受支持的 CSS；文本状态处理 UTF-16 cursor／anchor，兼容中文、emoji 与 native editing 关闭的路径。
- Grid 范围按首版设计执行，不依赖未公开的拖拽或 scroll offset 事件，也不假定可直接复用内置 Database widget。
- 客户端会话、数据库会话、事务和任务的所有权明确；最后有效会话离开后限时收尾，其他有效会话不受单个客户端退出影响。
- IPC 帧、分页、心跳和收尾参数按技术方案统一定义；实例发现结合真实连接探测和 OS 文件锁，不能只看 PID 或端点文件。

## 平台与安装

- 正式目标是 macOS、Linux、Windows，各自覆盖 x86-64／arm64。Linux 使用 GNU／glibc，Windows 使用 MSVC；平台差异集中在适配层。
- macOS／Linux 使用 UDS，Windows 使用字节模式 Named Pipe；消息协议一致，权限限定当前用户，Named Pipe 拒绝远程客户端。
- Windows 使用 Tern 随附 ConPTY，适配 OSC 877；验证路径、进程退出、文件锁和活动 exe 更新，不能套用 Unix 信号与权限行为。
- 凭据仅存系统凭据库或 Engine 会话内存；日志、配置、导出和运行描述文件不写秘密。
- Luau 由 Tern 内嵌执行；安装脚本经 `tern.process.run(argv)` 异步启动，不用有 16 MiB 响应限制的 fetch 搬运整包，也不假定 `tern.runtime` 有 arch 字段。
- Tern Launch 使用 shell 命令字符串，block 的 args 不能当原生程序 argv；固定启动命令与用户输入分离，Rust 启动子进程独立传参。
- 二进制按版本／OS／架构校验并缓存，Windows 新版本写入新目录；构建、缓存和运行数据放在 plugin 源码目录外。

## 测试与验证

Rust 测试文件统一放在各 crate 的 `tests/`：集成测试放在该目录根部，需访问私有实现的单元测试放在 `tests/unit/`，可按源码职责建立子目录。源码以 `#[cfg(test)]` 和相对路径 `#[path = "..."]` 引入对应测试模块；不在 `src/` 内存放测试文件或内联测试模块，也不为测试扩大公开 API。

Rust 改动按影响范围运行 `cargo check --locked`／`cargo test --locked`；格式检查按[代码格式化](#代码格式化)执行。静态检查与 workspace 默认测试命令为：

```sh
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

Python 开发发布回归放在 `scripts/tests/`，使用标准库 `unittest`，覆盖版本和归档契约、错误及边界；实际组包还须按发布指南使用本机真实二进制验证。执行：

```sh
python3 -m unittest discover -s scripts/tests -p 'test_*.py'
```

真实数据库回归默认忽略，必须显式提供隔离实例。MySQL 使用 `DATAPIER_TEST_MYSQL_PORT`、`DATAPIER_TEST_MYSQL_SECRET`，账户 `datapier`、库 `datapier_fixture`；Redis 使用 `DATAPIER_TEST_REDIS_PORT` 与 DB 15。测试仅创建 UUID 隔离的表／key，并清理自身资源；存储函数用例要求测试库允许创建函数，开启 binlog 时需由测试实例管理员设置 `log_bin_trust_function_creators=1`。

```sh
cargo test -p datapier-core --test databases --test mysql_edges --test redis_integration --test transfers -- --ignored --test-threads=1
STRESS_KEYS=1000000 cargo test -p datapier-core --test redis_scale -- --ignored --nocapture
DATAPIER_TEST_KEYRING=1 cargo test -p datapier-core --test credentials -- --ignored
```

TLS 专属回归使用 `DATAPIER_TLS_MYSQL_PORT`、`DATAPIER_TLS_MYSQL_SECRET`、`DATAPIER_TLS_REDIS_PORT`、`DATAPIER_TLS_CA`、`DATAPIER_TLS_WRONG_CA`，可选 `DATAPIER_TLS_BAD_HOST`（默认 `127.1`）。MySQL 5.7 专项另需 `DATAPIER_TLS_MYSQL57_PORT`、`DATAPIER_TLS_MYSQL57_SECRET`，服务端启用 `require_secure_transport`。需要与测试 CA、SAN 匹配且支持 Rustls 可用 cipher 的隔离 TLS 服务；只有 DHE 的旧 OpenSSL fixture 不能证明 ECDHE 连接路径。

```sh
cargo test -p datapier-core --test tls -- --ignored
```

普通 `cargo test` 通过不等于数据库验收。代码行为变更必须运行对应路径：数据库使用真实测试实例，UI 在实际 Tern 中交互验证，IPC／退出观察进程和资源状态。只通过编译或 mock 测试不能代替运行证明。回归测试针对用户可见行为、边界、冲突和状态迁移；不测试源码文字、机械转发或偶然默认值。

按[0.3.0 技术方案验收要求](docs/technical-design-0.3.0.md#131-必须观察的行为)及既有 [0.2.0 入口与权限边界](docs/technical-design-0.2.0.md#14-验收要求)覆盖真实 MCP、普通终端、Tern、数据库、权限审批、实时分页与导出、升级和生命周期，并记录实际平台与环境。本轮 0.3.0 验收按用户最新确认仅覆盖 macOS arm64、Tern 0.9.12，其他平台保留实现与 CI 配置，不作为本轮门禁；历史证据不替代本候选验证。具体发布检查见[发布前检查](RELEASING.md#发布前检查)。

只报告实际执行的命令、结果、未执行的检查及环境限制，不静默缩减平台范围。[0.1.0 历史验收](docs/technical-design-0.1.0.md#11-实现与运行记录)和[0.1.1 候选运行记录](docs/technical-design-0.1.1.md#12-实现与验证记录)不是当前候选的验证证据；0.2.0 结果记录在[技术方案](docs/technical-design-0.2.0.md#15-风险与当前完成状态)，发布门禁以[发布前检查](RELEASING.md#发布前检查)为准。纯文档变更检查相对链接、章节锚点、标题、表格、代码块及方案一致性，无需安装依赖、启动数据库或运行无关构建、单测和 smoke。

## 分支与 PR

`main` 由 GitHub 的 [protect-main ruleset](https://github.com/huspx/datapier/rules/26316200) 保护，变更必须从工作分支提交 PR 后合并，禁止直接推送、强制推送和删除 `main`；不额外约定工作分支命名。

仓库通过 [.github/CODEOWNERS](.github/CODEOWNERS) 将所有文件（包括 CODEOWNERS 自身）的 code owner 指定为 `@huspx`。GitHub 使用 PR 目标分支上的 CODEOWNERS；本文件必须先合入 `main`，才会对后续以 `main` 为目标的 PR 生效。

常规 PR 至少需要一个批准，并要求 code owner 批准；新提交会撤销过期批准，所有 review 讨论必须解决。无人工协作者的 Copilot PR 需要额外一个批准。仅允许 Merge commit，不要求最后一次推送必须由其他人单独批准。

`huspx` 保留仅在 PR 内绕过规则的权限，仍须通过 PR 合并，不能直接推送 `main`。PR 作者不能批准自己的 PR；维护者本人发起的 PR 若无其他有效审批人，只能显式使用该 PR 内绕过权限合并，不能记作已获得 code owner 批准。完整配置以 GitHub ruleset 为准。

PR 说明变更内容、实际验证及限制。原生构建工作流仅在目标为 `main` 的 pull request、`main` 的 push 和手动触发时执行；普通工作分支和 tag 的 push 不自动触发。PR 的分支过滤按目标分支匹配，保留格式检查、默认测试、release 构建、版本 smoke 与单目标打包，归档随 runner 清理，不上传 Actions artifact。仅 main push 或 main 手动运行把六目标包直接上传到版本草稿 Release，再从同一草稿汇总 plugin 和校验清单；草稿不自动发布，非 main 手动运行也仅做打包验证。main 工作流按分支串行，PR 为只读权限，只有草稿相关任务获得 `contents: write`。当前 ruleset 未配置 required status checks。具体草稿身份、重跑和正式发布边界见[发布指南](RELEASING.md)。

## 文档与交付

按[文档职责与阅读顺序](#文档职责与阅读顺序)同步受影响文档。产品能力、设计或兼容性变化更新对应说明；只有符合 Changelog 口径的变化才写入当前产品版本章节，不因一次开发修正机械追加条目。发布准备时按[发布指南](RELEASING.md#发布说明与-changelog)核对版本内容；正式发布状态以 GitHub Release 和实际资源为准。README 的安装、功能和平台描述必须与实现和发布产物一致，交付报告只说明实际改动、验证与限制。

遵循 [MIT License](LICENSE)，保留第三方分发要求。真实凭据、私有连接、生产数据、生成产物与本地索引不入库；测试样本使用可公开数据。

截图仅用于本地验证，不提交到 Git，也不在设计文档中引用。演示资源仅保留 README 使用的 `docs/assets/datapier-demo.gif`；品牌、数据库图标等非截图资源按现有约定维护。
