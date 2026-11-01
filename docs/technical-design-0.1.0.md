# Datapier 0.1.0 技术方案

## 1. 目标与范围

依据 [产品规划](product-plan.md)，交付“连接 → 浏览／查询 → 编辑 → 提交 → 导入导出”的完整工作流。本文同时记录实施设计与验收要求；已执行记录及发布阻塞见第 11 节。

| 功能 | 0.1.0 交付 |
|---|---|
| 连接与工作区 | MySQL／Redis 直连、认证、TLS、数据库选择、只读／读写；多 tab、最近连接、草稿与布局恢复；多个 UI 共享 Engine |
| MySQL | 5.7／8.0／8.4；database／表／视图／字段及列注释／索引／约束／DDL 浏览；过滤、多列排序、分页；可靠键定位的基表增删改 |
| Redis | 单节点 DB 选择、SCAN 与 pattern／type 过滤；默认折叠的多级 key 树；String、Hash、List、Set、ZSet 浏览编辑；重命名、删除和 TTL |
| 查询与命令 | 全文／选区／当前 SQL 执行，高亮、关键词／资源补全，多结果展示、错误、取消／停止等待、历史和查询文件 |
| 数据 grid | 列宽设置、隐藏／冻结、单元格／行选择、键盘与 Shift 选择、复制粘贴；文本／JSON／NULL 编辑、撤销、变更集及冲突提示 |
| 数据交换 | 单文件 CSV／JSON；列映射、类型转换、失败记录；从完整结果或数据库导出 |
| 平台 | macOS、Linux、Windows，各自覆盖 x86-64、arm64；六种原生产物及运行验收 |

MCP 属于 0.2.0；结构设计、高级编辑和迁移沿用产品规划的后续版本。首版不包含 Cluster／Sentinel、鼠标拖拽列宽、鼠标框选和连续虚拟滚动。视图及任意 SQL 结果只读，结构和数据写入仍可在读写连接上显式执行 SQL。

## 2. 模块与运行方式

保留产品规划的四个首版 crate，不增加独立插件进程或通用插件框架。

| 路径 | 实现内容 |
|---|---|
| `crates/datapier/src/main.rs` | CLI 分派；提供 `tui`、`engine`、`--version`；不编入 MCP |
| `crates/datapier-core/src/model/` | Profile、资源、列、类型化值、分页、任务和错误 |
| `crates/datapier-core/src/execution/` | 结果落盘入口、发送阶段与取消控制、流式导入导出 |
| `crates/datapier-core/src/providers/{mysql,redis}/` | 内置连接、资源、只读策略、查询、编辑基线、并发检查及提交；按操作分派，不做逐单元格动态分派 |
| `crates/datapier-core/src/storage/` | SQLite migrations、工作区、草稿、历史与执行记录；keyring 适配 |
| `crates/datapier-engine/` | client／server、协议编解码、客户端租约、实例锁、端点、进程启动及收尾 |
| `crates/datapier-tsp/` | 界面状态与 tab 路由、IPC、连接表单、编辑模型和按功能组织的原生视图 |
| `plugins/datapier/{plugin.toml,window.luau,lib/bootstrap.luau}` | 命令入口、异步安装检查、打开本机 TSP pane |
| `plugins/datapier/install/{unix.sh,windows.ps1}` | 下载、校验、解包及安装原生产物；不编译 Rust |

CLI 组合各模块；TSP 依赖 Engine 的 client／DTO，Engine 调用 Core，Core 不依赖 TSP。所有入口共用同一份 IPC 编解码；平台代码不进入 provider 的业务判断。

各 crate 的测试统一放在 `tests/`：集成测试位于根目录，需访问私有实现的单元测试位于 `tests/unit/`，由源码通过 `#[cfg(test)]` 和相对路径的 `#[path = "..."]` 引入，保持原有私有边界。

使用产品规划的 Tokio、Serde、SQLx 0.9、redis-rs 1.8、keyring 4.2 `v1`、sqlparser-rs 与纯 Rust sqlformat 0.5。官方 `tern-sdk` 使用 registry 的 `0.2.0`，workspace 精确固定版本，`Cargo.lock` 锁定全部依赖；不维护旧 SDK 兼容层或本地 fork。当前界面基线为 Tern 0.9.9；MySQL 保留 SQLx，采用 prepare 取元数据后单独执行的方式。

### 2.1 UI 与异步任务

官方 Rust TSP SDK 是同步 API，`Session::next` 会等待输入。TSP Session 由 UI 主线程独占；同一进程的 Tokio runtime 处理 IPC、心跳和异步文件请求，通过有界队列通知 UI，并调用 `Session::waker()` 唤醒等待。唤醒返回的 `None` 不直接视为 EOF，区分主动退出、超时与唤醒。

UI 消息统一进入状态更新函数，只有状态变化才 render；静态节点复用，SDK 负责 diff 与帧流控。IPC 慢请求、结果写盘及凭据库调用不能阻塞 TSP 输入或心跳；阻塞的系统凭据操作放入 blocking worker。

### 2.2 连接所有权

| 资源 | 所有者及隔离方式 |
|---|---|
| profile | Engine 共享配置；密码只存 keyring 或当前 Engine 内存 |
| MySQL 查询 tab | 独占物理连接并串行执行，保存 `USE`、变量、临时表和事务语义；关闭时回滚并断开 |
| MySQL 浏览／提交 | 使用独立连接；提交事务不与查询 tab 的手动事务混用 |
| Redis 普通浏览 | 按 profile＋DB 分组；共享连接不发送 `SELECT`、`WATCH` 或 `MULTI` |
| Redis 命令 tab | 独占物理连接，隔离 DB 与事务状态；重连后重新选择 DB，不恢复未完成事务 |
| Redis 可编辑 key | 专属监视连接和 edit token；与普通浏览及命令 tab 隔离 |

连接设置的所有入口先整体检查该连接当前工作区标签的草稿、恢复意图、任务和连接操作；有阻塞时保持全部标签并提示处理。通过后按冻结标签 ID 关闭所属会话，等待 ACK 后再打开表单；`not_found` 表示会话已关闭，其余失败保留可恢复状态并报告。其他连接的标签保留。保存时再次检查，已有所属标签的重连重新加载 schema、结果可编辑性和 edit token，刷新只读／读写 UI；已关闭标签不自动重建。测试与保存使用文字按钮，TLS 与证书入口左对齐。新建表单每次重置字段、数据库候选与选择、TLS、错误及撤销历史。数据库断连使旧会话与事务失效，不自动重放查询、写入或 COMMIT。

MySQL 元数据的独立连接建立和读取响应取消，取消后丢弃该连接并释放会话锁。未给出 database 且 profile 没有默认库时，先在共享查询连接读取 `SELECT DATABASE()`，保留 `USE` 语义；若这一步被中断，丢弃共享连接，后续明确返回 `connection_closed`，不静默重建并丢失事务或会话状态。

## 3. IPC、任务与生命周期

### 3.1 协议 v1

macOS／Linux 使用 Tokio UDS，Windows 使用 Tokio 字节模式 Named Pipe。每帧为 **4 字节大端长度＋UTF-8 JSON**，消息体上限 8 MiB，解码前检查长度。Client 在写出任何字节前拒绝的超限请求只返回该请求的输入错误，并保留连接、其他并发请求及事件订阅；实际 I/O 或部分写入错误仍关闭连接。

- 请求：`id`、`method`、`params`；响应：相同 `id` 及 `result` 或 `error`。
- 事件：`seq`、`kind`、`task_id`、`payload`，无请求 `id`。`kind=execution.state`，`payload.state` 通知任务状态；序号在同一 Engine 内单调递增，仅向所属客户端发送，不携带 SQL、凭据或结果行。Client 单独订阅事件，UI 收到当前任务事件后查询状态；保留轮询核对遗漏，结果数据由分页接口拉取。
- 全局语言更新使用 `kind=settings.changed` 广播给客户端；UI 按事件重新读取设置，并以周期读取补偿遗漏，不携带 SQL、凭据或结果数据。
- 握手校验 `protocol_version=1`，返回 `engine_id`、`client_session_id` 和租约参数。业务会话 ID 与数据库会话 ID 分开。
- 错误统一为 `code`、`message`、`details`、可选数据库错误码／SQLSTATE；区分权限、输入、冲突、断连、取消和结果待确认。
- ID 用 UUID 字符串；请求 ID 仅关联响应，不构成写入重试授权。断连后按任务记录查结果，不自动重发写请求。

| 接口组 | 方法与结果 |
|---|---|
| 客户端 | `session.open/heartbeat/close`；会话身份、租约和释放结果 |
| 配置／工作区 | `profile.list/save/test/delete/databases`、`workspace.load/save`；工作区写入带预期 revision，冲突不覆盖；目录加载使用正在编辑的认证与 TLS，返回完整库列表 |
| 应用设置 | `settings.get/set`；`settings.language` 为 `system`、`zh-CN` 或 `en`，按数据目录全局保存，更新后广播 `settings.changed` |
| 数据库会话 | `connection.open/close`；DB 会话 ID、当前数据库、capabilities；Redis 可传临时 `redis_db`，不修改 profile，成功后才替换 UI 旧会话 |
| 资源 | `resource.list/describe/browse`；资源树、结构及浏览任务／游标 |
| 执行 | `execution.start/get/cancel`；任务 ID、状态及分语句结果摘要 |
| 结果 | `result.page/value/release`；结果页、大字段分块及释放状态 |
| 编辑 | `change.open/preview/submit/discard`；编辑基线／token、实际 SQL／命令与参数预览、类型化变更集及逐项结果 |
| 文件／交换 | `file.open/save`、`transfer.import/export`；Engine 本机文件、任务和失败报告 |

耗时操作先返回任务 ID。任务在 Engine 内运行，不把单次 RPC 超时当作执行失败。分页、状态查询与取消可并发处理，同一数据库会话上的命令串行化。

`change.preview` 与提交使用相同 token、target 和类型化草稿，读取但不消费 token。Core 共享实际执行构造器，返回 provider、事务标记、草稿总数、offset／next_offset、逐项操作及语句／有序参数；`offset` 从 0 开始，`limit` 默认和上限均为 20。MySQL 区分原值锁校验和 DML，Redis 包含真实 EVAL 脚本与最终页的 TTL 命令。预览不执行写入或锁定语句，不更改 WATCH；提交仍重新复核。参数显示带类型、原始字节数、完整性及动态参数标记，每值最多 1 KiB、每页展示预算 512 KiB；展示省略不影响完整绑定，不能将截断值拼成可执行 SQL。Redis List 删除的内部随机标记在提交时生成，重命名目标届时才 WATCH／检查；EXEC 单项错误不回滚其他命令。UI 预览与提交共用草稿组装，异步响应复核标签、generation、token 及草稿身份。

Redis 批量 key 删除使用 `change.open {targets}`：目标必须属于同一当前 DB，保留 `key_b64` 原始身份，合法空字节 key 可表示为 `""`。Core 在独立连接 WATCH 全部目标后读取必要类型与 TTL 元数据，返回 `token`、原样 `targets`、`operation: "delete_keys"` 和基线；不完整浏览各 key 的 value。预览与提交传入同一 `token`、原顺序完整 `targets` 和 `operation`，只允许删除已绑定集合。预览直接展示实际 MULTI／DEL／EXEC 计划；提交在发送阶段记录保护内执行一次 DEL，返回实际删除数，WATCH 冲突整批拒绝且不自动重试。确认仅建立删除草稿，预览、提交、丢弃及工作区失效恢复沿用编辑生命周期；恢复需重新确认和建立 WATCH，不复用旧 token。复合条目多选删除保持单 key 基线，完整读取身份后一次暂存；List 映射原始索引并按执行索引降序删除，一次批量操作按一组撤销。

### 3.2 执行记录

任务状态为 `queued → running → succeeded/failed/cancelled/outcome_unknown`，取消过程中标记 `cancelling`；只有确认取消生效才进入 `cancelled`。每条语句／变更另记已生效、未执行、已回滚或待确认，失败任务可以包含此前已生效的操作。

`execution.get` 的任务记录提供任务 ID、创建时间、真实运行开始时间、任务耗时，以及结果的类型、已读取行数、已知列数和完成状态。开始时间在任务进入运行时记录；耗时由单调时钟测量任务运行至返回的经过时间，不冒充数据库纯执行耗时。任务元数据复用既有 `executions.summary`，连接／数据库与目标上下文在创建任务时保存到既有参数记录，不新增 schema 版本；终态、取消和写盘失败的内存完成记录保留同一份已观察元数据。历史缺失指标明确显示“未记录”，未知列数不显示为零；结果释放后只展示仍有真实记录支持的指标。

普通任务查询不载入执行文本；打开详情以 `include_text: true` 按需读取，不返回整包参数。查询文本来自原始请求，浏览 SQL／Redis 命令来自 provider 实际构造和发送路径，记录有界并明确标记截断，不从资源名伪造命令。默认详情采用图标操作标题、右侧状态、双列分隔摘要、结果表和唯一执行文本区；逐语句结构与原始字段仅在完整诊断中展开，错误和部分执行状态仍须明确可见。开始时间、任务 ID 与连接上下文按任务归属呈现，迟到响应不能替换其他任务内容。

写入发送前先持久化任务与发送阶段，接收最终结果后再更新记录。Engine 重启时，未开始操作标记未执行；可能已经发送且不能证明结果的写入标记待确认。数据库与本地 SQLite 不构成分布式事务，不以本地记录推断数据库必然提交或回滚。

数据目录满卷等原因使最终记录无法保存时，Engine 在本次进程内保留明确的存储错误及已观察结果：读取任务返回 `failed`，可能已发送的写入保守返回 `outcome_unknown` 并提示人工核对。单条查询、任务列表、历史及取消接口保持相同终态，不把旧的 `running` 当作仍在执行。该内存记录不承诺跨 Engine 重启保留；重启仍按已落盘的发送阶段恢复，不重放操作。

### 3.3 实例与收尾

以规范化用户数据目录作为实例范围。启动顺序是“探测端点 → 必要时启动候选 Engine → 候选争取 OS 文件锁 → 持锁者发布端点 → 客户端握手”。锁由进程持有；端点不存在不能替代实例锁，不兼容实例不强杀。

UDS 放入仅当前用户可访问的短运行目录；Windows Named Pipe 按用户 SID 设置 DACL 并拒绝远程客户端。运行描述文件记录实例、端点和协议版本，不存秘密。Windows 保留管道实例持续接入多个客户端。

心跳 **10 秒**、租约 **45 秒**、无人收尾 **30 秒**。任一客户端离开即停止其排队请求、取消读任务并回滚私有事务；已发送写入等待最终结果。最后会话离开或启动后一直无人接入时进入收尾；到期仍未知的写入落盘，关闭连接后退出。新会话可在正式停止接入前取消退出计时，旧任务不恢复。

`connection.open`、`profile.test` 和 `profile.databases` 的只读等待归所属客户端的生命周期取消信号管理；断线或租约过期会结束等待并释放 profile 锁，已创建的测试任务记录取消终态。`profile.save/delete` 不随断线直接 abort，继续完成凭据库与 SQLite 的所有权交接。

Engine 不继承 UI 的 PTY；stdout／stderr 写入独立日志或关闭，不持有客户端协议句柄。退出阶段在同一同步边界内检查会话数并停止接入，避免接入与退出竞态。Unix 崩溃残留 socket 取得锁后清理；Windows 句柄关闭后由系统回收管道。

## 4. 数据模型与持久化

### 4.1 类型与结果

`Value` 使用带类型的结构：null、bool、整数、精确小数、浮点、文本、二进制、日期／时间和 JSON。整数、小数及浮点以字符串传输，二进制以 Base64 传输；JSON 内的大数保留数字词法，不先转 `f64`。Redis key／field／member 始终保留原始字节，显示文本只是视图。

列记录原生类型、长度／精度及可取得的来源信息；基表的键、生成列、自增标志、默认值和可空性来自 catalog。MySQL 从 `EXTRA` 分别识别生成列与 `auto_increment`，不把 `DEFAULT_GENERATED` 当成生成列。MySQL TIME 保留负值和超 24 小时语义，日期不擅自转换为 UTC；驱动无法无损处理的类型明确报错或禁止对应编辑，不把显示字符串用于写回。

`ResultSet` 保存列、完成状态、已读取行数及截断原因；`Row` 使用结果内序号。编辑定位另用原始完整键与原值基线，禁止拿 UI 行号定位数据库行。Redis 缺失值、空字节和 NULL 分别表达。

默认每页 **200 行／1 MiB**，请求最多 **1,000 行／1 MiB**；字段超过 **64 KiB** 时返回最多 **4 KiB** 预览及引用，单次分块最多 **256 KiB**。组页超限则提前结束；单行过大先外置更多字段并缩减预览，仍超限时明确报大小错误，不返回无法推进的空页或丢行。同一个结果固定行序；过滤或排序改变后生成新结果 ID，UI 丢弃旧请求迟到的响应。

结果增量写入独立 `cache.sqlite3`，按 `(result_id, row_no)` 分页；大字段落入引用文件。批量写盘、有界队列施加背压，UI 仅持有当前页。SQLx 按行读取，单个大字段仍受驱动解码和服务端包大小限制，不宣称字段级网络流式读取。

完整结果缓存跨 Engine 重启保留，用户释放时删除；中断结果明确标记不完整。磁盘不足不能伪装为正常结束。导出已执行的 SQL 结果只读缓存，不自动重执行原 SQL；不完整结果只能明确导出已获取部分，不能标为全量。

### 4.2 本地存储

使用 SQLite WAL，配置库和结果缓存分开，避免大结果挤占工作区更新；0.1.0 各以一份完整初始 migration 管理。

| 表／文件 | 主要内容 |
|---|---|
| `profiles` | provider、地址、用户名、TLS／DB、access mode、`visible_databases` 与系统凭据条目追踪标记；无密码；未设显示范围表示全部可访问库，空数组表示全部隐藏 |
| `app_settings` | 数据目录内全局语言偏好，不属于 workspace 快照 |
| `workspaces` | revision 与 JSON 快照：tab、共享侧栏收展、布局、连接引用、文本／选区、过滤排序、列配置及未提交变更意图；运行期 edit token 不作为有效凭据恢复 |
| `executions` | 所属工作区、执行文本、状态、发送阶段及结构化错误；查询历史直接从执行记录读取 |
| `cache.sqlite3`、`values/` | 结果元数据、类型化行、结果引用及大字段内容 |
| keyring | macOS Keychain、Windows Credential Manager、Linux Secret Service；不可用时仅 Engine 内存保存 |

配置库和结果缓存均以完整 schema 1 在事务中初始化；重开时核对必需表与索引，拒绝较新版本或不完整基线。`value_files` 登记外置字段与所属结果，先登记归属，再异步写文件并等待 flush，最后提交结果行；追加和分页任务在调用方取消等待后仍持有结果锁直到该次操作结束。分页先释放读取连接，再登记与写入外置文件，避免并发耗尽连接池。释放结果先在 SQLite 事务中撤销结果与行，再删除文件；删除失败保留登记，结果读取返回已释放，可对同一结果 ID 再次释放完成清理，不自动启动后台清理。

写入 keyring 前先保守登记条目；写入失败时新密码可仅保留在当前 Engine 内存，但不遗失可能已创建的系统条目。提供新密码且改为不持久化时先删除旧条目，无法删除则明确报错并保留配置。未提供新密码的元数据更新保留既有凭据。配置删除同样先清理条目，`NoEntry` 视为已清理，其他错误保留可重试状态。

文本修改短暂 debounce 后保存，切 tab／关闭时立即 flush；关闭前等待保存确认。崩溃恢复到最后确认保存的版本，不承诺恢复未落盘按键。恢复文本和变更草稿，但不恢复数据库连接、事务或 WATCH；失效变更必须重新读取并由用户确认新基线后才能提交。

## 5. 数据库实现

### 5.1 MySQL

1. **Catalog。** 使用 `information_schema` 和受控的 `SHOW CREATE` 获取表／视图、字段、索引、约束和 DDL；标识符按 catalog 校验并转义，值全部参数绑定。列注释读取 `COLUMNS.COLUMN_COMMENT`，保持中文、多行和空字符串；表浏览按实际列名及顺序附带注释，任意 SQL 结果不猜测注释。5.7 的 `STATISTICS` 没有 `EXPRESSION` 列，按 catalog 能力选择查询；有该列时继续排除表达式索引。可靠键优先主键，其次完整且各分量非 NULL 的唯一键；不把前缀／表达式索引直接当完整行键。无可靠键的表仍可浏览，行编辑只读。
2. **查询与元数据。** 普通单语句在 tab 专属连接上 `prepare`，从 `Statement::columns()` 保存列定义，再执行并消费 `fetch_many()`，保留空结果列和结果边界。生成的编辑 SQL 复用 prepared statement 并绑定参数，不先执行一遍 SQL 探测结构。
3. **脚本。** 全文／选区逐条执行可可靠识别的顶层语句，当前语句定位依据 tokenizer、引号、注释和当前 `sql_mode`，不按分号盲切。模式变化后重新识别余下文本；遇到存储程序体或不可靠边界时禁用当前语句执行，全文／选区可走原始文本路径。MySQL 客户端 `DELIMITER` 等指令提示不支持，不当作服务端 SQL。
4. **动态结果边界。** `prepare` 不能预先枚举 CALL／动态 SQL 的所有结果。此类执行仍消费每个结果，非空结果按实际行元数据展示；空动态结果若驱动未给出列则标记“列定义未知”，不伪造零列 schema，也不把后续结果套用前一个结果的列。普通 SELECT 空结果必须保留 prepare 得到的完整列。
5. **浏览。** 一次浏览查询按过滤条件与多列排序流式落盘，再在固定结果上分页；无键表不通过重复 OFFSET 查询拼接页面。追加可靠键作排序的稳定后缀；无键结果仅保证本次缓存行序。刷新开启新查询，不承诺跨刷新快照。
6. **提交。** InnoDB 变更集在独立事务中按键锁定原行，比较全部受保护原值后执行参数化增删改。NULL、大小写／尾空格和二进制差异不能被默认 collation 吞掉；以数据库可观察且可无损往返的值定义比较。相同值更新视为无变化，不单凭 affected rows 为零判断冲突；冲突／失败回滚整组，逐项标记回滚或未执行。
7. **非事务表。** 使用完整键及原值条件的单语句 CAS，验证 SQLx 的 matched-row 行为；逐项提交并报告已生效／失败／未执行，不宣称整组回滚。DDL 在查询 tab 中按 MySQL 隐式提交规则执行。
8. **取消。** 记录目标连接 `CONNECTION_ID()`，控制连接发送 `KILL QUERY`；等待原查询终态。取消处理与原结果消费完成前不在该连接启动下一条命令；无法清理时关闭连接，避免迟到的 KILL 影响后续查询。权限不足和写入可能部分生效分别报告。
9. **TLS 兼容。** Rustls 要求 TLS 1.2 的 ECDHE 或 TLS 1.3。5.7 可使用支持 ECDHE 的服务端构建；官方 5.7.42 Debian／OpenSSL 1.1.1n 已通过 TLS 1.2 验证。官方 5.7.44 Oracle Linux／OpenSSL 1.0.2k 的默认构建仅能协商 DHE，与当前 Rustls 不兼容；保持验证并报握手错误，不降级到明文或跳过证书检查。普通 5.7 查询、浏览和编辑与此 TLS 构建限制分别验收。

### 5.2 Redis

1. **浏览。** SCAN／HSCAN／SSCAN／ZSCAN 以返回 cursor 为准，cursor 为零才结束；允许空批次及重复项，COUNT 只是提示。扫描去重状态落入缓存，按字节分页；List 使用 LRANGE。并发变更下不承诺快照或全库排序。
2. **编辑基线。** 打开可编辑 key 时先在专属连接 WATCH，再读取类型、值／当前页及 PTTL，返回 edit token。监视从展示基线之前持续到提交；List 索引只在该 token 内有效，避免重复值或前方插入导致误改。刷新、断线、租约失效后旧 token 不可提交，不能重连后假装仍在监视。
3. **提交。** 预校验参数和类型，再 MULTI／EXEC；EXEC 返回 null 是冲突，不能自动换基线重试。按实际 key／field／member／index 生成操作；List 按索引删除使用先确认未出现的临时标记，在受 WATCH 保护的事务内 LSET＋LREM，不能按原值删重复元素。逐条检查 EXEC 回复，运行期错误不等于事务回滚。
4. **TTL。** 区分 PTTL 的不存在、永久和剩余毫秒；String 更新用 `SET ... KEEPTTL`，集合原位修改保留 TTL；删除最后一个元素可能使 key 消失。TTL 修改为显式操作。重命名同时检查目标，使用不覆盖目标的操作，保留源 TTL。
5. **命令 tab。** 按 redis-cli 的引号、空参数和转义规则生成字节参数数组，引用外换行才分命令；不经 shell。SELECT／WATCH／MULTI 仅作用于该 tab 的专属连接。Pub/Sub、MONITOR 等切换为持续推送的连接模式不按普通请求执行，明确提示本版不提供相应订阅界面。
6. **停止等待。** 丢弃 future 不撤销命令；已发送写入继续由 Engine 等待结果。阻塞命令使用专属连接，关闭或放弃等待不能宣称服务端未执行；状态不明时记录待确认，不回池。

### 5.3 只读与权限

Engine 对浏览、编辑、命令和导入统一检查 profile access mode。结构化写操作在只读模式直接拒绝；数据库最小权限账号是第二层约束。

MySQL 自由 SQL 仅放行完整解析且能证明只读的 SELECT／WITH 查询及受控的 SHOW／DESCRIBE／EXPLAIN；检查表达式和函数白名单，拒绝可执行注释、文件输出、动态调用、修改会话安全状态和无法判定的语句。只读数据库会话使用服务端只读事务约束，但不把它当唯一判据。读写模式不以 parser 不支持为由拒绝服务端本可执行的普通 SQL。

Redis 使用命令／子命令／参数的只读白名单，`COMMAND INFO` 只辅助能力识别，不自动扩大白名单。未知命令、用户脚本和无法证明只读的事务拒绝；provider 内部固定且审阅过的只读脚本独立定义，不放开自由 EVAL。缺少数据库权限明确报错，不降级执行。

## 6. TSP 工作台与编辑器

采用 `screen` surface：连接／资源树、主 tab 区、底部任务与待提交区；表单和大值编辑放入 layer。界面状态分为工作区、tab、编辑器、grid 和操作状态，所有数据库结果带 tab generation，防止切换后被旧响应覆盖。

- **Grid。** 使用公开 `el`／文本节点组成 CSS grid，共享列宽；单一滚动容器内的表头与冻结单元格使用 `position: sticky`，左偏移由固定列宽累加，不依赖读取 Tern scroll offset。稳定 key 由结果 ID、行序号和列 ID 构成，分页后不把焦点误映射到其他行。
- **选择与列宽。** 单元格单击选单格、Shift 扩展矩形；行号独立维护普通、Shift 和平台主修饰键行多选。MySQL 表及 SQL 结果自动列宽按当前页内容和可用空间分配，范围 64–480px，超出总上限的空间可留白，窄区域内部横向滚动；手动设置保留，原位编辑期间冻结已有列宽。自然宽度在页面数据改变时更新，不在每次输入或 render 全量重复测量。Redis 内容与 CLI 沿用各自既有列宽策略。列配置、过滤和排序持久化。
- **编辑。** 原位编辑方案在活动单元格外壳内覆盖或替换为 `input`／`editor`，只维护一个活动编辑器；内层定位容器随单元格滚动，冻结外壳保留 `sticky`，不读取或自行计算滚动偏移。Enter／F2／双击进入编辑，节点加入视图后显式请求其稳定 ID 的焦点；Enter 确认进入变更集，Tab 确认后移到下一可编辑格，Escape 放弃当前输入。长值使用侧栏或 layer，不让多行编辑器越过 grid 裁切区域。导航撤销与文本撤销分开；提交前可查看、撤销或丢弃待提交项，基线仍由 Engine 校验。
- **文本。** Rust 保存文本、UTF-16 cursor／anchor、undo 栈，声明并处理 `edit`／`undo`。查询编辑器使用 `mode=insert` 保持应用键盘选区，保证执行、执行计划和格式化取得同一范围；其他字段使用 native editing，并保留关闭 native editing 时的键盘路径。协议不报告原生拖拽选区的 anchor，查询不以宿主选区或剪贴板冒充应用选区。IME 提交后更新模型，UTF-8 字节偏移与 UTF-16 索引有显式转换。
- **补全与高亮。** 查询 editor 通过 `lang=sql` 使用 Tern 原生高亮，应用 tokenizer 负责当前语句范围和基础诊断；补全使用关键词及缓存 catalog，显式刷新结构后更新。服务端执行结果是最终依据。
- **复制粘贴。** 应用右键菜单“复制选中单元格”和 grid 底部 copy 图标复制单格或矩形选区；grid 不绑定 Ctrl+C 或 Command+C，文本编辑器拥有焦点时保留其文本复制。菜单保留 Tern 自动附加的原生 `Copy`：它仅复制右击节点展示文字，不能读取应用选区或完整大值。选区复制沿用第 7 节 CSV 的 NULL／二进制／反斜线保真编码，改用制表符并保留引号规则。大字段在后台完整读取，按标签、generation、结果及选区内容校验后再写入剪贴板，读取失败或上下文改变不复制截断值或部分选区。行复制及 MySQL INSERT 复制继续由行菜单提供。普通粘贴先形成类型化编辑预览，验证维度和目标可编辑性，再加入变更集，不直接写库。
- **文件。** 查询文件以 UTF-8 保存和打开；文件操作由本机 Engine 完成，失败保留编辑文本。跨 UI 的工作区更新使用 revision 检查，不覆盖另一窗口未确认的布局／草稿。

[UI 交互设计稿](ui-redesign.html)保留 SQL／Redis 的布局与交互定义。生产 Rust TSP 共享连接导航，宽屏收展偏好归工作区所有；进入 760px 及以下自动收起到 58px 窄轨，临时展开不覆盖宽屏偏好，返回宽屏恢复偏好，再次进入窄屏重新收起。SQL 树按连接显示配置允许的数据库，并按需加载各库表／视图；隐藏数据库不关闭其已打开 tab。表 tab 的历史 SQL 打开到独立查询 tab，保留原表结果和编辑基线；已有查询打开历史时清除无关旧结果。标签以活动下划线标识当前项；右键按冻结目标位置关闭左侧、右侧或全部，整批预检草稿和任务，会话关闭等待 ACK。全部关闭后 `tabs=[]`，显示居中空态并持久化；宽屏从侧栏选择连接，窄轨空态另提供选择连接入口，迟到响应不能重建已关闭标签。原位编辑使用网格内单一 input，表单、值编辑和变更预览使用高于主布局 sticky 元素的模态层。

连接、数据库和表提供右键菜单。连接删除先保护所属草稿与任务，逐个等待当前工作区相关会话的关闭 ACK，再删除配置；明确的 `not_found` 按会话已关闭处理，其他关闭错误保留配置。MySQL 库／表删除冻结连接身份和资源，确认前及打开专用会话后均复核，配置已改但原会话未重连时拒绝。DROP 只在独立 Engine 会话执行，经取消或终态后等待关闭 ACK；来源标签保留占用，关闭确认弹窗后仍有停止等待入口。成功或结果未知时失效相关资源，未知写入不自动重放。

行菜单以原始可靠行身份处理选中行复制、两种 INSERT 和确认删除；隐藏全部列仍可使用行号菜单。删除先形成草稿，禁止同一行继续生成更新或粘贴变更，提交组装拒绝重复非 INSERT 身份。

Redis 复合项编辑使用独立控件：Hash 为 Field／Value，List 为 Index／Value，ZSet 为 Member／Score；List 的 Index 只读定位当前元素，Value 可多行，Score 单独校验。两个值分别进入结构化草稿和前后值预览，不用换行拼接或拆分字段；String 与 Set 保持单项编辑。

生产视图按连接表单、共享控件、侧栏、grid、Redis 和弹窗组织，输入与渲染、连接管理、编辑生命周期、IPC 和 tab generation 路由各按职责分离。surface 使用 sans；表单和主操作采用 SDK 公开控件，导航行、标签、grid 和状态入口使用局部命名 CSS。新建与编辑连接均用带数据库图标的单选卡片选择类型。MySQL 二级导航右侧显示完整资源标识；数据、查询结果及 Redis 各类型工具区直接提供具名操作，不设通用更多入口。右键菜单以公开 Actions、MenuEntry、MenuItem 描述分组、禁用、图标与危险色，删除项包含 trash 图标，外壳和定位由宿主管理。overlay 正文有稳定 key；下拉展开时临时放开正文裁切，关闭后恢复滚动。变更预览先列真实语句，再以紧凑参数表展示类型、字节及完整性；详情冻结来源、连接和资源，优先显示语义事实，原始诊断按需展开。CLI 在对应结果上方保留执行时命令，后续输入不改变该归属。

工作台不维护自定义明暗主题或主题切换按钮；颜色直接使用 TSP 的公开主题变量，如 `--panel`、`--card`、`--t1`、`--t3`、`--l2`、`--accent` 和 `--tv-sel`，随宿主主题更新。视觉还原以交互 HTML 为依据，从实际交互状态导出截图，在相同内容区域尺寸下与原生 TSP 截图对照布局、字体、间距、控件层次和密度；图片作为视觉参照，交互行为仍以 HTML 和真实数据路径为准。

当前视觉目标见 [UI 交互设计稿](ui-redesign.html)及[还原说明](ui-redesign.md)。设计稿不能作为原生界面已完成还原或验收的证据；生产行为与运行范围以实际源码和验收记录为准。

Redis 列表显示 SCAN 已加载的 key，类型与 TTL 来自真实 provider；TTL 以每页 pipeline 读取，永久、不存在和有限期限分别表达。命名空间只组织已加载项。CLI 的结果和分页与当前 key 的数据、编辑基线隔离；合法 UTF-8 直接展示，其他字节和字面 Base64 前缀保留可逆编码。JSON 与 HEX 仅改变展示，不改变写回值。

Redis 复制原始 value 在后台解析完整缓存及大字段，结构数据按所有权移动编码。UI 每轮最多发送 64 KiB，批次间校验 tab、generation、结果、key 和草稿版本；失效或写入失败不发送提交帧。草稿版本随 tab 交换，在加入、撤销、恢复和清空时递增，避免逐批深比较整个草稿。

查询编辑器使用公开 `lang=sql` 与 `nowrap`，行号和逐句按钮与文本共用滚动容器；横向滚动固定 gutter。语句定位沿用 tokenizer 和真实会话 `sql_mode`，无法可靠识别时保留原文并说明原因。工具栏始终提供执行计划入口，非空选区优先，否则取当前语句；校验为单条可解释 SQL 后只发送普通 `EXPLAIN FORMAT=JSON`，不加入 `ANALYZE`。选区由应用维护；协议未同步鼠标拖选范围，不能把宿主内部选择作为执行目标。格式化按 token 语义核对结果，通过一次 `Editor::edit` 接入撤销。剪切经 OSC 5522 写入，收到匹配请求的 `DONE` 后再次核对 tab 和完整原文再删除；拒绝、超时或上下文变化均保留 SQL。

应用语言由 Core 全局设置经 Engine 同步，UI 进程读取自己的系统首选语言。消息保存静态 key、模板及原始参数，切换后重新渲染；SQL、对象名称、数据和数据库原始错误保留原文。macOS、Windows 与 Linux 使用各自系统语言来源，未知或不支持的语言回退英文；六平台运行证据仍分别验收。

侧栏设置与退出之间提供图标化快捷键帮助，操作和键位均翻译，Command／Ctrl 按 UI 平台展示。执行、普通操作和删除分别使用宿主语义控件的 primary、default／quiet、danger 变体，不自建明暗主题颜色。DDL 全文复制保留图标入口。


## 7. 导入导出

默认 UTF-8、CSV 逗号分隔与引号转义；导入先预览列映射、NULL 和二进制处理。MySQL 首版导入使用 INSERT，重复键报失败，不隐式覆盖；Redis 默认只创建目标中不存在的 key。

| 格式 | 约定 |
|---|---|
| MySQL JSON | `format=datapier`、`version=1`、列名／原生类型及类型化 rows；大数保持字符串，二进制 Base64 |
| MySQL CSV | 首行列名；整数／小数用完整文本，NULL 为 `\N`，非空值的反斜线一律加倍，二进制为 Base64；先解析 CSV，再识别 NULL，最后还原非空值转义。目标列映射确定类型；外部 CSV 在预览中显式选择 NULL／二进制／转义规则 |
| Redis JSON | 版本化 key 记录：Base64 key、原生类型、完整类型化内容、`expires_at_ms` 为绝对过期毫秒字符串或 null（永久）；List 保留顺序和重复项，ZSet score 用可往返字符串 |
| Redis CSV | 固定列 `version,record,key_b64,type,expires_at_ms,field_b64,index,member_b64,score,value_b64`；`record=key` 存 key 元信息，`record=item` 存逐元素内容；永久期限记为 `permanent`，不适用列留空，等价表达 JSON 内容 |

导出以 writer 流式序列化完整记录，不先构建整个 JSON 文档。Redis 6.2 的绝对过期时间由固定只读脚本结合 TIME／PTTL 获取，较新版本可用 PEXPIRETIME；不把该命令当成 6.2 的通用能力。导入按服务器时间跳过已过期 key，不把 TTL 重置为最初剩余时长。

MySQL 默认每 500 行一个事务批次，转换错误进入失败报告，数据库错误停止后续批次；此前已提交批次保留。非事务表逐项报告。Redis 在临时 key 分批构建，设置目标过期语义后用 RENAMENX 发布，目标存在则冲突。临时对象内嵌随机所有权标记，创建、追加、发布、丢弃和会话收尾均在 Lua 脚本内原子验证归属；外部替换的对象不能被修改或删除，用户数据与标记碰撞时轮换标记。创建前记账，创建或标记轮换结果未知时保留可能的标记用于清理；发布结果未知不自动重放。

MySQL 表导出开启新的只读读取任务；查询结果导出使用原结果缓存。Redis 从数据库遍历目标 key 及完整集合，不只导出当前页，也不宣称并发扫描是一致性快照。取消返回已生效、失败和未执行范围，输出文件用临时路径写入，完成后原子发布；未完成文件明确标记部分内容。

## 8. 跨平台与分发

六个 Rust target 沿用产品规划第 2.7 节：macOS Darwin、Linux GNU／glibc、Windows MSVC，分别构建 x86-64 和 aarch64。发布前须明确最低 OS／glibc、TLS 后端和系统依赖，并在发布说明记录测试版本；现有 CI runner 只是构建基线。原生链接库与可执行文件必须同架构。

启动包固定二进制版本及六目标 SHA-256。开发环境优先使用显式配置的本地二进制；发布安装由 `tern.process.run(argv)` 异步调用包内脚本，Unix 使用 curl／tar 与系统校验工具，Windows 使用系统 PowerShell。打包时从实际 `GITHUB_REPOSITORY` 或 `-Repository` 固化下载地址。脚本检测架构、写独立临时目录并验证摘要和版本；Unix 以内容寻址目录和硬链接原子发布，避免陈旧目录锁，Windows 使用进程文件锁。只返回简短机器可读结果，已有有效缓存无需下载地址。

不通过 Luau fetch 搬运整个可执行包：其响应体上限为 16 MiB，且公开 fs 未提供完整的安装权限／原子发布操作。`tern.runtime` 只提供 OS，不假定有 arch 字段；安装脚本负责硬件架构识别和名称归一化。

启动入口固定本机 host。`Launch.command` 是登录 shell 的命令字符串，`Launch.args` 仅用于 Luau block；通过安装目录作为 cwd 和固定相对启动命令打开 pane，分别验证 Unix shell、Windows PowerShell／cmd。用户 SQL、凭据和文件参数不拼进 shell 命令。Engine 由 Rust `Command` 独立传参启动，Windows 避免新建多余控制台，且不继承客户端 stdio。

缓存按版本／OS／架构隔离。Windows 更新写入新版本目录，不覆盖活动 exe；IPC 不兼容时提示先结束旧会话再重启，不强杀 Engine。Windows 验证 Tern 随附 ConPTY 及 OSC 877 输入；Linux 无 Secret Service 时使用会话凭据；macOS 验证 Keychain 授权及安装后的实际启动。

## 9. 验收要求

| 领域 | 验收要求 |
|---|---|
| 运行与生命周期 | 四个 crate、SQLite migration、启动入口、IPC 与租约；在 Tern 中经 Engine 连真实 MySQL／Redis，显示查询结果并验证退出 |
| MySQL 浏览 | catalog、过滤排序、prepare＋执行、多结果缓存与分页；空 SELECT 有列，无键表跨页不重复执行 |
| Grid 与提交 | 公开 TSP 的冻结／选择／文本编辑／撤销／变更预览；真实 InnoDB 冲突回滚及非事务表逐项反馈 |
| Redis 工作流 | 五类型、二进制 key、游标、WATCH 基线、List 重复值编辑、重命名及 TTL；并发变化被拒绝 |
| 查询工作区 | 两种命令 tab、全文／选区／当前语句、高亮补全、事务隔离、取消、历史、文件与草稿恢复 |
| 数据交换 | CSV／JSON 流式导入导出、映射、失败报告和取消范围；十万行及五种 Redis 类型真实往返 |
| 六目标发布 | 六种原生产物、干净安装、匹配下载、凭据／TLS／IPC／ConPTY／退出恢复；全部目标完成实际运行后发布 |

至少保留以下行为回归：中文／emoji 的 UTF-16 编辑与选区执行；引号和注释内分号；普通空结果集与连续空／非空结果；BIGINT／DECIMAL／NULL／二进制；同值更新、唯一键 NULL、并发删除、大小写／尾空格冲突；迟到取消不影响后续查询；Redis 重复 List 元素、过期导致 WATCH 失败、TTL 保留、EXEC 单项错误；写入断连不重放；最后客户端退出与新接入竞态。

实际运行还须覆盖：十万行分页与全量导出、百万 key 增量扫描、大字段；重开后的草稿与布局；两 UI 同时操作；只读拒绝写入；磁盘不足和凭据服务不可用。六种目标均需原生运行，交叉编译／模拟器不能替代验收；数据库版本为 MySQL 5.7／8.0／8.4、Redis 6.2／7.2／8.0，当前 Tern 基线为 0.9.9。

首版发布包含六目标二进制、校验清单、Tern 启动包和对应测试记录。文档、接口原型或编译成功不计为功能已验收。

## 10. 关键依据

- [Rust TSP SDK 0.2.0](https://docs.rs/tern-sdk/0.2.0/tern_sdk/)、[SDK 源码](https://github.com/stencil-hq/tern-sdk)、[默认视图设计](https://docs.stencil.so/tern/guides/design.md)、[TSP 输入事件](https://docs.stencil.so/tern/protocol/input.md)
- [Tern CSS grid／sticky](https://docs.stencil.so/tern/styles/css.md)、[Launch 与 window API](https://docs.stencil.so/tern/reference/api-window.md)、[process／fetch／fs 限制](https://docs.stencil.so/tern/reference/api-shared.md)
- [SQLx Statement 列元数据](https://docs.rs/sqlx/0.9.0/sqlx/trait.Statement.html)、[SQLx RawSql 多结果](https://docs.rs/sqlx/0.9.0/sqlx/struct.RawSql.html)、[MySQL prepared statements](https://dev.mysql.com/doc/refman/8.4/en/sql-prepared-statements.html)、[MySQL KILL](https://dev.mysql.com/doc/refman/8.4/en/kill.html)
- [Redis WATCH／事务错误](https://redis.io/docs/latest/develop/using-commands/transactions/)、[SCAN](https://redis.io/docs/latest/commands/scan/)、[SET KEEPTTL](https://redis.io/docs/latest/commands/set/)、[redis-rs MultiplexedConnection](https://docs.rs/redis/1.8.0/redis/aio/struct.MultiplexedConnection.html)

## 11. 实现与运行记录

0.1.0 尚未发布。当前源码包含四个 crate、共享 Engine、MySQL／Redis provider、TSP 工作台、CSV／JSON 交换、启动 plugin、安装器及六目标 CI 配置。以下为既有 macOS arm64 运行证据，开发基线为 Tern 0.9.9、registry tern-sdk 0.2.0；这些记录不代表本轮重跑，也不能代替候选提交的验收。

| 场景 | 既有运行证据 |
|---|---|
| 自动化 | workspace 默认测试 255 项通过、48 项忽略；另有真实数据库专项 37 项通过，两者分别计数 |
| MySQL | 5.7／8.0／8.4 的相关数据库回归；查询、类型保真、空结果与多结果、编辑冲突、取消竞态、只读、十万行 CSV／JSON 往返 |
| Redis | 6.2／7.2／8.0 的五类型、TTL、WATCH、重命名及双格式往返；百万 key 扫描；复合条目多选、大值复制和批量删除 |
| 安全与资源 | MySQL 8.4／Redis 8 的 TLS 成功及错误 CA／身份拒绝；macOS Keychain 存取；磁盘满后报错与恢复；外置文件清理失败及释放重试 |
| 安装与生命周期 | macOS arm64 单目标包、并发安装、缓存复用和坏缓存拒绝；实际 Tern 查询、客户端事件隔离和最后会话退出收尾 |
| TSP 工作台 | 中文／emoji、键盘选区、查询、多结果、分页、编辑、Redis 五类型、CLI、菜单、恢复、宿主主题及多客户端设置同步；既有大值复制和原生粘贴记录 |

### 2026-11-01：结构整理、测试目录与复制

本轮按职责拆分 MySQL 结果处理、Redis 命令／编辑／交换、Engine 会话及 TSP 动作／响应／视图。所有测试实现移至各 crate 的 `tests/`；私有单元测试放在 `tests/unit/`，通过条件模块加载，不扩大公开 API。代码未写入开发机器的个人绝对路径。

- **静态检查与默认测试**：`cargo fmt --all --check`、workspace 全 targets Clippy（`-D warnings`）通过；workspace 默认测试 256 项通过、48 项忽略。恢复默认 Copy 后额外复跑 TSP 测试，192 项通过、1 项忽略，并重新构建本机程序。
- **真实数据库**：本轮使用隔离 MySQL 8.4.13、Redis 8.10.3，`databases`、`mysql_edges`、`redis_integration`、`transfers` 共 37 项通过，覆盖十万行 CSV／JSON 往返；不将该环境记为重新验证历史版本矩阵。
- **配置一致性**：新增取消等待后配置锁仍覆盖凭据交接的回归；实际 Engine IPC 对接真实 Redis ACL 用户，12 轮并发保存配置／打开连接均通过，临时配置与 ACL 用户已清除。
- **原生复制**：Tern 0.9.9 的自动附加 `Copy` 只复制右击节点的 `ORD-10421`；应用“复制选中单元格”复制完整 2×2 TSV。另将重复一万次的中文／emoji、NULL 和二进制一起复制、粘贴并保存工作区，逐字核对 70,009 字节原文，保留 `\N` 和 `AP8=`。
- **界面与演示**：实际 Tern 验证资源浏览、SQL 结果、任务详情、Redis DB 选择和 Hash 字段；浏览器设计稿验证矩形复制、选区外目标和显示文本复制，无页面脚本错误。README 的 8 帧、12.6 秒 GIF 来自实际原生操作。


其余五个目标尚未完成原生验收，Windows ConPTY／权限及 Linux 凭据服务尚未实际验证；六个真实产物尚未集齐，完整启动包尚未组装成功。正式发布仍须按第 8、9 节及[发布指南](../RELEASING.md)验证候选提交、各平台和实际产物；既有结果不能代替相应运行证据。
