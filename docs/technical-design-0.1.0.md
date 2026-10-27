# Datapier 0.1.0 技术方案

## 1. 目标与范围

依据 [产品规划](product-plan.md)，交付“连接 → 浏览／查询 → 编辑 → 提交 → 导入导出”的完整工作流。本文是实施设计，所列验收尚未执行。

| 功能 | 0.1.0 交付 |
|---|---|
| 连接与工作区 | MySQL／Redis 直连、认证、TLS、数据库选择、只读／读写；多 tab、最近连接、草稿与布局恢复；多个 UI 共享 Engine |
| MySQL | database／表／视图／字段／索引／约束／DDL 浏览；过滤、多列排序、分页；可靠键定位的基表增删改 |
| Redis | 单节点 DB 选择、SCAN 与 pattern／type 过滤；String、Hash、List、Set、ZSet 浏览编辑；重命名、删除和 TTL |
| 查询与命令 | 全文／选区／当前 SQL 执行，高亮、关键词／资源补全，多结果展示、错误、取消／停止等待、历史和查询文件 |
| 数据 grid | 列宽设置、隐藏／冻结、单元格／行选择、键盘与 Shift 选择、复制粘贴；文本／JSON／NULL 编辑、撤销、变更集及冲突提示 |
| 数据交换 | 单文件 CSV／JSON；列映射、类型转换、失败记录；从完整结果或数据库导出 |
| 平台 | macOS、Linux、Windows，各自覆盖 x86-64、arm64；六种原生产物及运行验收 |

MCP 属于 0.2.0；结构设计、高级编辑和迁移沿用产品规划的后续版本。首版不包含 Cluster／Sentinel、鼠标拖拽列宽、鼠标框选和连续虚拟滚动。视图及任意 SQL 结果只读，结构和数据写入仍可在读写连接上显式执行 SQL。

## 2. 模块与运行方式

保留产品规划的四个首版 crate，不增加独立插件进程或通用插件框架。

| 路径 | 实现内容 |
|---|---|
| `crates/datapier/src/main.rs` | CLI 分派；提供 `ui`、`engine`、`--version`；不编入 MCP |
| `crates/datapier-core/src/model/` | ID、资源、列、类型化值、分页、能力和错误 |
| `crates/datapier-core/src/connection/` | profile、数据库会话、只读策略、凭据引用 |
| `crates/datapier-core/src/execution/` | 任务、结果集、落盘缓存、取消及导入导出 |
| `crates/datapier-core/src/changes/` | 编辑基线、变更集、并发检查、提交结果 |
| `crates/datapier-core/src/providers/{mysql,redis}/` | 内置实现；registry 显式注册，按操作分派，不做逐单元格动态分派 |
| `crates/datapier-core/src/storage/` | SQLite migrations、工作区、草稿、历史与执行记录；keyring 适配 |
| `crates/datapier-engine/src/{ipc,session,platform}/` | server／client／协议 DTO、客户端租约、实例锁、端点、进程启动及收尾 |
| `crates/datapier-tsp/src/{app,editor,grid,views}/` | TSP 事件循环、界面状态、编辑与展示；只经 Engine client 访问数据 |
| `plugins/datapier/{plugin.toml,window.luau,lib/bootstrap.luau}` | 命令入口、异步安装检查、打开本机 TSP pane |
| `plugins/datapier/install/{unix.sh,windows.ps1}` | 下载、校验、解包及安装原生产物；不编译 Rust |

CLI 组合各模块；TSP 依赖 Engine 的 client／DTO，Engine 调用 Core，Core 不依赖 TSP。所有入口共用同一份 IPC 编解码；平台代码不进入 provider 的业务判断。

使用产品规划的 Tokio、Serde、SQLx 0.9、redis-rs 1.8、keyring 4.2 `v1` 和 sqlparser-rs；TSP 使用已发布的 `tern-sdk 0.1.0`，由 `Cargo.lock` 固定补丁及间接依赖。MySQL 保留 SQLx，采用 prepare 取元数据后单独执行的方式。

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

修改 profile 后，新会话使用新配置，已有会话明确提示重连；不在执行中静默切换账号或数据库。数据库断连使旧会话与事务失效，不自动重放查询、写入或 COMMIT。

## 3. IPC、任务与生命周期

### 3.1 协议 v1

macOS／Linux 使用 Tokio UDS，Windows 使用 Tokio 字节模式 Named Pipe。每帧为 **4 字节大端长度＋UTF-8 JSON**，消息体上限 8 MiB，解码前检查长度。

- 请求：`id`、`method`、`params`；响应：相同 `id` 及 `result` 或 `error`。
- 事件：`seq`、`kind`、`task_id`、`payload`；事件通知状态变化，结果数据由分页接口拉取。
- 握手校验 `protocol_version=1`，返回 `engine_id`、`client_session_id` 和租约参数。业务会话 ID 与数据库会话 ID 分开。
- 错误统一为 `code`、`message`、`details`、可选数据库错误码／SQLSTATE；区分权限、输入、冲突、断连、取消和结果待确认。
- ID 用 UUID 字符串；请求 ID 仅关联响应，不构成写入重试授权。断连后按任务记录查结果，不自动重发写请求。

| 接口组 | 方法与结果 |
|---|---|
| 客户端 | `session.open/heartbeat/close`；会话身份、租约和释放结果 |
| 配置／工作区 | `profile.list/save/test/delete`、`workspace.load/save`；工作区写入带预期 revision，冲突不覆盖 |
| 数据库会话 | `connection.open/close`；DB 会话 ID、当前数据库、capabilities |
| 资源 | `resource.list/describe/browse`；资源树、结构及浏览任务／游标 |
| 执行 | `execution.start/get/cancel`；任务 ID、状态及分语句结果摘要 |
| 结果 | `result.page/value/release`；结果页、大字段分块及释放状态 |
| 编辑 | `change.open/submit/discard`；编辑基线／token、类型化变更集及逐项结果 |
| 文件／交换 | `file.open/save`、`transfer.import/export`；Engine 本机文件、任务和失败报告 |

耗时操作先返回任务 ID。任务在 Engine 内运行，不把单次 RPC 超时当作执行失败。分页、状态查询与取消可并发处理，同一数据库会话上的命令串行化。

### 3.2 执行记录

任务状态为 `queued → running → succeeded/failed/cancelled/outcome_unknown`，取消过程中标记 `cancelling`；只有确认取消生效才进入 `cancelled`。每条语句／变更另记已生效、未执行、已回滚或待确认，失败任务可以包含此前已生效的操作。

写入发送前先持久化任务与发送阶段，接收最终结果后再更新记录。Engine 重启时，未开始操作标记未执行；可能已经发送且不能证明结果的写入标记待确认。数据库与本地 SQLite 不构成分布式事务，不以本地记录推断数据库必然提交或回滚。

### 3.3 实例与收尾

以规范化用户数据目录作为实例范围。启动顺序是“探测端点 → 必要时启动候选 Engine → 候选争取 OS 文件锁 → 持锁者发布端点 → 客户端握手”。锁由进程持有；端点不存在不能替代实例锁，不兼容实例不强杀。

UDS 放入仅当前用户可访问的短运行目录；Windows Named Pipe 按用户 SID 设置 DACL 并拒绝远程客户端。运行描述文件记录实例、端点和协议版本，不存秘密。Windows 保留管道实例持续接入多个客户端。

心跳 **10 秒**、租约 **45 秒**、无人收尾 **30 秒**。任一客户端离开即停止其排队请求、取消读任务并回滚私有事务；已发送写入等待最终结果。最后会话离开或启动后一直无人接入时进入收尾；到期仍未知的写入落盘，关闭连接后退出。新会话可在正式停止接入前取消退出计时，旧任务不恢复。

Engine 不继承 UI 的 PTY；stdout／stderr 写入独立日志或关闭，不持有客户端协议句柄。退出阶段在同一同步边界内检查会话数并停止接入，避免接入与退出竞态。Unix 崩溃残留 socket 取得锁后清理；Windows 句柄关闭后由系统回收管道。

## 4. 数据模型与持久化

### 4.1 类型与结果

`Value` 使用带类型的结构：null、bool、整数、精确小数、浮点、文本、二进制、日期／时间和 JSON。整数、小数及浮点以字符串传输，二进制以 Base64 传输；JSON 内的大数保留数字词法，不先转 `f64`。Redis key／field／member 始终保留原始字节，显示文本只是视图。

列记录原生类型、长度／精度及可取得的来源信息；基表的键、生成列、默认值和可空性来自 catalog。MySQL TIME 保留负值和超 24 小时语义，日期不擅自转换为 UTC。驱动无法无损处理的类型明确报错或禁止对应编辑，不把显示字符串用于写回。

`ResultSet` 保存列、完成状态、已读取行数及截断原因；`Row` 使用结果内序号。编辑定位另用原始完整键与原值基线，禁止拿 UI 行号定位数据库行。Redis 缺失值、空字节和 NULL 分别表达。

默认每页 **200 行／1 MiB**，请求最多 **1,000 行／1 MiB**；字段超过 **64 KiB** 时返回最多 **4 KiB** 预览及引用，单次分块最多 **256 KiB**。组页超限则提前结束；单行过大先外置更多字段并缩减预览，仍超限时明确报大小错误，不返回无法推进的空页或丢行。同一个结果固定行序；过滤或排序改变后生成新结果 ID，UI 丢弃旧请求迟到的响应。

结果增量写入独立 `cache.sqlite3`，按 `(result_id, row_no)` 分页；大字段落入引用文件。批量写盘、有界队列施加背压，UI 仅持有当前页。SQLx 按行读取，单个大字段仍受驱动解码和服务端包大小限制，不宣称字段级网络流式读取。

完整结果缓存跨 Engine 重启保留，用户释放时删除；中断结果明确标记不完整。磁盘不足不能伪装为正常结束。导出已执行的 SQL 结果只读缓存，不自动重执行原 SQL；不完整结果只能明确导出已获取部分，不能标为全量。

### 4.2 本地存储

使用 SQLite WAL 与顺序 migration，配置库和结果缓存分开，避免大结果挤占工作区更新。

| 表／文件 | 主要内容 |
|---|---|
| `profiles` | provider、地址、用户名、TLS／DB 配置、access mode、credential reference；无密码 |
| `workspaces`、`tabs` | revision、布局、连接引用、编辑文本／选区、过滤排序、列配置及最近访问 |
| `change_drafts` | 未提交变更意图和原值；运行期 edit token 不作为持久有效凭据 |
| `executions`、`query_history` | 所属工作区、执行文本、状态、发送阶段、结果引用及结构化错误 |
| `cache.sqlite3`、大字段文件 | 结果元数据、类型化行及字段内容 |
| keyring | macOS Keychain、Windows Credential Manager、Linux Secret Service；不可用时仅 Engine 内存保存 |

文本修改短暂 debounce 后保存，切 tab／关闭时立即 flush；关闭前等待保存确认。崩溃恢复到最后确认保存的版本，不承诺恢复未落盘按键。恢复文本和变更草稿，但不恢复数据库连接、事务或 WATCH；失效变更必须重新读取并由用户确认新基线后才能提交。

## 5. 数据库实现

### 5.1 MySQL

1. **Catalog。** 使用 `information_schema` 和受控的 `SHOW CREATE` 获取表／视图、字段、索引、约束和 DDL；标识符按 catalog 校验并转义，值全部参数绑定。可靠键优先主键，其次完整且各分量非 NULL 的唯一键；不把前缀／表达式索引直接当完整行键。无可靠键的表仍可浏览，行编辑只读。
2. **查询与元数据。** 普通单语句在 tab 专属连接上 `prepare`，从 `Statement::columns()` 保存列定义，再执行并消费 `fetch_many()`，保留空结果列和结果边界。生成的编辑 SQL 复用 prepared statement 并绑定参数，不先执行一遍 SQL 探测结构。
3. **脚本。** 全文／选区逐条执行可可靠识别的顶层语句，当前语句定位依据 tokenizer、引号、注释和当前 `sql_mode`，不按分号盲切。模式变化后重新识别余下文本；遇到存储程序体或不可靠边界时禁用当前语句执行，全文／选区可走原始文本路径。MySQL 客户端 `DELIMITER` 等指令提示不支持，不当作服务端 SQL。
4. **动态结果边界。** `prepare` 不能预先枚举 CALL／动态 SQL 的所有结果。此类执行仍消费每个结果，非空结果按实际行元数据展示；空动态结果若驱动未给出列则标记“列定义未知”，不伪造零列 schema，也不把后续结果套用前一个结果的列。普通 SELECT 空结果必须保留 prepare 得到的完整列。
5. **浏览。** 一次浏览查询按过滤条件与多列排序流式落盘，再在固定结果上分页；无键表不通过重复 OFFSET 查询拼接页面。追加可靠键作排序的稳定后缀；无键结果仅保证本次缓存行序。刷新开启新查询，不承诺跨刷新快照。
6. **提交。** InnoDB 变更集在独立事务中按键锁定原行，比较全部受保护原值后执行参数化增删改。NULL、大小写／尾空格和二进制差异不能被默认 collation 吞掉；以数据库可观察且可无损往返的值定义比较。相同值更新视为无变化，不单凭 affected rows 为零判断冲突；冲突／失败回滚整组，逐项标记回滚或未执行。
7. **非事务表。** 使用完整键及原值条件的单语句 CAS，验证 SQLx 的 matched-row 行为；逐项提交并报告已生效／失败／未执行，不宣称整组回滚。DDL 在查询 tab 中按 MySQL 隐式提交规则执行。
8. **取消。** 记录目标连接 `CONNECTION_ID()`，控制连接发送 `KILL QUERY`；等待原查询终态。取消处理与原结果消费完成前不在该连接启动下一条命令；无法清理时关闭连接，避免迟到的 KILL 影响后续查询。权限不足和写入可能部分生效分别报告。

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
- **选择。** 浏览模式处理方向键、Tab、Home／End、Shift 扩展和 Shift＋点击；显式行选择入口支持整行复制。列配置、过滤和排序持久化，列宽通过设置表单调整。
- **编辑。** Enter／F2／双击进入 input／editor，Escape 放弃本次编辑，确认后进入变更集。导航撤销与文本撤销分开；提交前可查看、撤销或丢弃待提交项。基线仍由 Engine 校验。
- **文本。** Rust 保存文本、UTF-16 cursor／anchor、undo 栈；声明 `edit`／`undo` 并处理事件，同时支持 native editing 关闭时的键盘路径。IME 文本提交后再更新模型，UTF-8 字节偏移与 UTF-16 索引有显式转换。
- **补全与高亮。** tokenizer 输出高亮与语句范围；补全采用关键词及缓存 catalog，显式刷新结构后更新。对未知语法给出基础诊断，服务端执行结果是最终依据。
- **复制粘贴。** 复制选区为 TSV，沿用第 7 节 CSV 的 NULL／二进制／反斜线编码，改用制表符分隔并保留引号规则。普通粘贴先形成类型化编辑预览，验证维度和目标可编辑性，再加入变更集，不直接写库。文本编辑区保留普通文本粘贴行为。
- **文件。** 查询文件以 UTF-8 保存和打开；文件操作由本机 Engine 完成，失败保留编辑文本。跨 UI 的工作区更新使用 revision 检查，不覆盖另一窗口未确认的布局／草稿。

## 7. 导入导出

默认 UTF-8、CSV 逗号分隔与引号转义；导入先预览列映射、NULL 和二进制处理。MySQL 首版导入使用 INSERT，重复键报失败，不隐式覆盖；Redis 默认只创建目标中不存在的 key。

| 格式 | 约定 |
|---|---|
| MySQL JSON | `format=datapier`、`version=1`、列名／原生类型及类型化 rows；大数保持字符串，二进制 Base64 |
| MySQL CSV | 首行列名；整数／小数用完整文本，NULL 为 `\N`，非空值的反斜线一律加倍，二进制为 Base64；先解析 CSV，再识别 NULL，最后还原非空值转义。目标列映射确定类型；外部 CSV 在预览中显式选择 NULL／二进制／转义规则 |
| Redis JSON | 版本化 key 记录：Base64 key、原生类型、完整类型化内容、`expires_at_ms` 为绝对过期毫秒字符串或 null（永久）；List 保留顺序和重复项，ZSet score 用可往返字符串 |
| Redis CSV | 固定列 `version,record,key_b64,type,expires_at_ms,field_b64,index,member_b64,score,value_b64`；`record=key` 存 key 元信息，`record=item` 存逐元素内容；永久期限记为 `permanent`，不适用列留空，等价表达 JSON 内容 |

导出以 writer 流式序列化完整记录，不先构建整个 JSON 文档。Redis 6.2 的绝对过期时间由固定只读脚本结合 TIME／PTTL 获取，较新版本可用 PEXPIRETIME；不把该命令当成 6.2 的通用能力。导入按服务器时间跳过已过期 key，不把 TTL 重置为最初剩余时长。

MySQL 默认每 500 行一个事务批次，转换错误进入失败报告，数据库错误停止后续批次；此前已提交批次保留。非事务表逐项报告。Redis 在临时 key 分批构建，设置目标过期语义后用 RENAMENX 发布，目标存在则冲突；临时 key 写前记账，失败／收尾清理，发布结果未知不自动重放。

MySQL 表导出开启新的只读读取任务；查询结果导出使用原结果缓存。Redis 从数据库遍历目标 key 及完整集合，不只导出当前页，也不宣称并发扫描是一致性快照。取消返回已生效、失败和未执行范围，输出文件用临时路径写入，完成后原子发布；未完成文件明确标记部分内容。

## 8. 跨平台与分发

六个 Rust target 沿用产品规划第 2.7 节：macOS Darwin、Linux GNU／glibc、Windows MSVC，分别构建 x86-64 和 aarch64。固定最低 OS／glibc、TLS 后端和系统依赖，并在发布说明记录测试版本；原生链接库与可执行文件必须同架构。

启动包固定二进制版本及六目标 SHA-256。开发环境优先使用显式配置的本地二进制；发布安装由 `tern.process.run(argv)` 异步调用包内脚本，Unix 使用 curl／tar 与系统校验工具，Windows 使用系统 PowerShell 的下载／解包／校验接口。脚本检测原生架构、加安装锁、写临时目录，验证后发布版本化缓存，只返回简短机器可读结果。

不通过 Luau fetch 搬运整个可执行包：其响应体上限为 16 MiB，且公开 fs 未提供完整的安装权限／原子发布操作。`tern.runtime` 只提供 OS，不假定有 arch 字段；安装脚本负责硬件架构识别和名称归一化。

启动入口固定本机 host。`Launch.command` 是登录 shell 的命令字符串，`Launch.args` 仅用于 Luau block；通过安装目录作为 cwd 和固定相对启动命令打开 pane，分别验证 Unix shell、Windows PowerShell／cmd。用户 SQL、凭据和文件参数不拼进 shell 命令。Engine 由 Rust `Command` 独立传参启动，Windows 避免新建多余控制台，且不继承客户端 stdio。

缓存按版本／OS／架构隔离。Windows 更新写入新版本目录，不覆盖活动 exe；IPC 不兼容时提示先结束旧会话再重启，不强杀 Engine。Windows 验证 Tern 随附 ConPTY 及 OSC 877 输入；Linux 无 Secret Service 时使用会话凭据；macOS 验证 Keychain 授权及安装后的实际启动。

## 9. 实施顺序与验收

| 步骤 | 产出及完成条件 |
|---|---|
| 1. 运行闭环 | 四个 crate、SQLite migration、启动入口、IPC 与租约；在 Tern 中经 Engine 连真实 MySQL／Redis，显示查询结果并验证退出 |
| 2. MySQL 浏览 | catalog、过滤排序、prepare＋执行、多结果缓存与分页；空 SELECT 有列，无键表跨页不重复执行 |
| 3. Grid 与提交 | 公开 TSP 实现冻结／选择／文本编辑／撤销／变更预览；真实 InnoDB 冲突回滚及非事务表逐项反馈 |
| 4. Redis 工作流 | 五类型、二进制 key、游标、WATCH 基线、List 重复值编辑、重命名及 TTL；并发变化被拒绝 |
| 5. 查询工作区 | 两种命令 tab、全文／选区／当前语句、高亮补全、事务隔离、取消、历史、文件与草稿恢复全部接线 |
| 6. 数据交换 | CSV／JSON 流式导入导出、映射、失败报告和取消范围；十万行及五种 Redis 类型真实往返 |
| 7. 六目标发布 | 六种原生产物、干净安装、匹配下载、凭据／TLS／IPC／ConPTY／退出恢复；全部目标完成实际运行后发布 |

至少保留以下行为回归：中文／emoji 的 UTF-16 编辑与选区执行；引号和注释内分号；普通空结果集与连续空／非空结果；BIGINT／DECIMAL／NULL／二进制；同值更新、唯一键 NULL、并发删除、大小写／尾空格冲突；迟到取消不影响后续查询；Redis 重复 List 元素、过期导致 WATCH 失败、TTL 保留、EXEC 单项错误；写入断连不重放；最后客户端退出与新接入竞态。

实际运行还须覆盖：十万行分页与全量导出、百万 key 增量扫描、大字段；重开后的草稿与布局；两 UI 同时操作；只读拒绝写入；磁盘不足和凭据服务不可用。六种目标均需原生运行，交叉编译／模拟器不能替代验收；数据库服务可在独立测试服务器上运行。数据库版本沿用 MySQL 8.0／8.4、Redis 6.2／7.2／8.0，Tern 基线为 0.9.5。

首版发布包含六目标二进制、校验清单、Tern 启动包和对应测试记录。文档、接口原型或编译成功不计为功能已验收。

## 10. 关键依据

- [Rust TSP SDK 与同步 Session／waker](https://github.com/stencil-hq/tern-sdk/blob/main/rust/README.md)、[tern-sdk 0.1.0](https://docs.rs/tern-sdk/0.1.0/tern_sdk/)、[TSP 输入事件](https://docs.stencil.so/tern/protocol/input.md)
- [Tern CSS grid／sticky](https://docs.stencil.so/tern/styles/css.md)、[Launch 与 window API](https://docs.stencil.so/tern/reference/api-window.md)、[process／fetch／fs 限制](https://docs.stencil.so/tern/reference/api-shared.md)
- [SQLx Statement 列元数据](https://docs.rs/sqlx/0.9.0/sqlx/trait.Statement.html)、[SQLx RawSql 多结果](https://docs.rs/sqlx/0.9.0/sqlx/struct.RawSql.html)、[MySQL prepared statements](https://dev.mysql.com/doc/refman/8.4/en/sql-prepared-statements.html)、[MySQL KILL](https://dev.mysql.com/doc/refman/8.4/en/kill.html)
- [Redis WATCH／事务错误](https://redis.io/docs/latest/develop/using-commands/transactions/)、[SCAN](https://redis.io/docs/latest/commands/scan/)、[SET KEEPTTL](https://redis.io/docs/latest/commands/set/)、[redis-rs MultiplexedConnection](https://docs.rs/redis/1.8.0/redis/aio/struct.MultiplexedConnection.html)
