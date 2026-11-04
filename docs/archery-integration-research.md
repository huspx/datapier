# Archery 接入 Datapier 调研报告

调研日期：2026-11-01。本文仅使用公开上游源码、公开参考项目和 Datapier 仓库材料，不包含任何组织的部署地址、账号、权限配置、连接信息或业务数据。

## 1. 结论与证据边界

Archery 可以作为 Datapier 的数据库访问来源接入，首期限定 MySQL 和 Redis，并支持在添加连接时一次选择多个实例及各自的 DB。建议在 Rust Core 内增加 Archery HTTP 访问通道，复用 Engine、编辑器、结果 grid、任务和本地结果缓存，不依赖额外的 Python MCP 服务。

关键结论：

1. **在线查询依赖 Web 会话。** 已检查的官方相关版本没有 REST 在线查询路由；真实入口为普通 Django Web 接口 `/query/`，需要 Session、CSRF 和相应查询权限。REST Bearer JWT 不能直接替代 Web 登录态。
2. **建议将 1.8.4 作为统一最低适配候选。** 该版本已有实例资源、SQL 工单 REST API 和 Web 查询，可覆盖查询及未来工单需求。1.12.0、1.13.0、1.14.0 的相关源码已核对，但没有完成这些版本的端到端兼容验收。
3. **直连功能不能全部等价复用。** 查询、元数据和文本结果可以适配；专属数据库会话、事务、并发安全编辑、Redis 原始字节和完整扫描没有等价协议保证。
4. **SQL 工单具备核心接口，但存在接入前置问题。** 列表读取、操作者绑定和部分授权逻辑存在源码层面的缺陷，不能仅根据接口名称或第三方工具声明开放审批、执行能力。
5. **Datapier 必须保留本地只读策略。** 官方 v1.12.0 Redis 查询使用前缀正则白名单，会误放行部分写命令；本次已在本地运行该检查函数复现。

本文使用以下证据等级：

| 类型 | 含义 |
|---|---|
| 源码事实 | 固定 tag 或 commit 中可直接观察的路由、参数、状态和实现 |
| 本地复现 | 执行公开源码中的纯逻辑或对应解码行为，没有连接数据库 |
| 方案建议 | 基于已知代码边界推导的设计，尚未实现或验收 |
| 待验收 | 登录、权限、响应和远端状态等需要真实服务才能确认的行为 |

本文是一份调研结果，不代表已经实现 Archery 接入，也不改变 Datapier 当前功能和版本承诺。

## 2. 调研范围与来源

调研范围包括：

- Archery 的 MySQL、Redis 功能及其对外访问方式。
- 多实例、多 DB 的连接选择与保存。
- Datapier 最小接入路径、能力复用和语义差异。
- 兼容 1.12.0 及后续已检查版本时的向下适配候选。
- SQL 上线检查、提交、审批、执行、日志、定时及回滚语句，为未来 MCP 接入提供依据。

公开上游采用 `hhyo/Archery`。参考项目固定为：

| 项目 | 核对版本 | 用途 |
|---|---|---|
| [Archery](https://github.com/hhyo/Archery) | 以 v1.12.0 为主要源码基线，对照历史 tag 及 v1.13.0、v1.14.0 | 官方路由、引擎、认证和工单实现 |
| [ckall/archery-mcp-server](https://github.com/ckall/archery-mcp-server) | `5846378be1b68116563b426b0138ac404a473405` | JWT 和 REST 工单封装参考 |
| [itzhouq/archery-mcp](https://github.com/itzhouq/archery-mcp) | `1836fde230f017e0ca1fa90b14745f6960bcbc08` | Web 会话、CSRF、TOTP 和在线查询参考 |

Datapier 当前状态以 [README](../README.md)、[贡献指南](../CONTRIBUTING.md)、[产品规划](product-plan.md)、[0.1.0 技术方案](technical-design-0.1.0.md)及实际源码为准。MCP 当前属于后续版本规划。

## 3. Archery 能力清单

### 3.1 平台功能

以下为官方 v1.12.0 的能力范围。平台支持某功能，不代表存在对应的完整 REST API，也不代表所有数据库具有相同语义。

| 能力 | MySQL | Redis | 与 Datapier 接入的关系 |
|---|---|---|---|
| 实例与 DB 枚举 | 支持 | 支持，受 Redis 命令权限影响 | 支持批量选择实例及 DB |
| 在线查询 | SQL 查询 | 受限命令查询 | 可复用编辑器、任务及结果展示 |
| 资源与结构浏览 | 库、表、列名、DDL | key 预览和部分读取命令 | 需要专用适配 |
| 查询授权、行数控制和查询历史 | 支持 | 支持相关查询控制 | 查询经过 Archery 的执行路径 |
| 数据脱敏 | 支持，取决于平台配置 | Redis 引擎不做脱敏 | 不能统一标记为已脱敏 |
| 上线工单、人工审批、执行 | 支持 | 支持命令工单 | 可供未来 MCP 使用 |
| 上线语句自动检查 | goInception 等 | 仅逐行生成检查结果 | 两种数据库的检查保证不同 |
| 执行备份和回滚 SQL | 支持，取决于配置和语句 | 不支持 | 不提供统一回滚语义 |
| 数据字典、慢日志、会话、账号、参数、归档 | 官方功能清单支持 | 官方功能清单不支持 | 不属于首期最小接入范围 |

来源：[官方 v1.12.0 功能清单](https://github.com/hhyo/Archery/blob/v1.12.0/README.md)、[Redis 引擎](https://github.com/hhyo/Archery/blob/v1.12.0/sql/engines/redis.py)、[MySQL 引擎](https://github.com/hhyo/Archery/blob/v1.12.0/sql/engines/mysql.py)。

### 3.2 实际查询与资源接口

| 用途 | 方法和路径 | 主要参数或结果 |
|---|---|---|
| 获取登录页 | `GET /login/` | 获取 CSRF cookie |
| 密码认证 | `POST /authenticate/` | `username`、`password`；2FA 启用时返回临时会话信息 |
| 当前用户实例 | `GET /group/user_all_instances/` | 支持 `db_type[]`、`tag_codes[]` 等过滤 |
| DB／表／列名 | `GET /instance/instance_resource/` | `instance_id` 或 `instance_name`、`resource_type`、`db_name`、`schema_name`、`tb_name` |
| MySQL DDL | `POST /instance/describetable/` | `instance_name`、`db_name`、`tb_name` 等；实际返回 `SHOW CREATE TABLE` 结果 |
| SQL／Redis 查询 | `POST /query/` | `instance_name`、`db_name`、`sql_content`、`limit_num`，以及可选 `tb_name/schema_name` |
| 查询历史 | `GET /query/querylog/` | `limit`、`offset`、`search` 等；普通用户默认查看自己的记录 |
| SQL 上线检查 | `POST /api/v1/workflow/sqlcheck/` | `instance_id`、`db_name`、`full_sql` |

普通 Web 查询会检查 `sql.query_submit`、实例关联和查询权限，再进行查询限制、执行、脱敏及日志处理。脱敏异常是否阻断由平台配置决定；不能把经过 Archery 等同于结果必然脱敏。

REST 实例清单使用全部实例 queryset，而 Web 查询根据用户实例关联判断能否查询。批量选择应优先使用“当前用户实例”接口；资源枚举结果不能单独作为查询授权依据。

来源：[Web 路由](https://github.com/hhyo/Archery/blob/v1.12.0/sql/urls.py)、[查询实现](https://github.com/hhyo/Archery/blob/v1.12.0/sql/query.py#L25-L155)、[用户实例](https://github.com/hhyo/Archery/blob/v1.12.0/sql/resource_group.py)、[资源接口](https://github.com/hhyo/Archery/blob/v1.12.0/sql/instance.py#L309-L402)、[REST 实例接口](https://github.com/hhyo/Archery/blob/v1.12.0/sql_api/api_instance.py#L20-L40)。

### 3.3 认证方案

官方 v1.12.0 的 REST 同时支持 JWTAuthentication 和 SessionAuthentication。普通 Django Web 查询只依赖 Web 登录态，不能直接用 Bearer JWT 代替。

**方案建议：首期共用一套 Web Session 和 CSRF 处理查询、资源与必要的 REST 检查请求，JWT 按后续需求增加。** 这比同时维护两套登录状态更少改动。SessionAuthentication 支持在 v1.8.4 中也已存在。

启用 2FA 时，密码验证成功不代表完成登录。需要处理临时 Session、验证页面及 OTP 验证接口；应沿用平台验证流程，不能把 TOTP 密钥写入普通连接配置。

来源：[v1.12.0 认证配置](https://github.com/hhyo/Archery/blob/v1.12.0/archery/settings.py#L270-L307)、[v1.8.4 认证配置](https://github.com/hhyo/Archery/blob/v1.8.4/archery/settings.py#L180-L188)、[密码与临时会话](https://github.com/hhyo/Archery/blob/v1.12.0/common/auth.py)、[2FA 验证](https://github.com/hhyo/Archery/blob/v1.12.0/sql_api/api_user.py#L380-L429)。

## 4. 与 Datapier 现有能力的差异

### 4.1 当前接入边界

Datapier 当前采用直连 MySQL／Redis 的编译期 provider。`Profile` 保存直连字段，`ProviderSession` 建立原生数据库会话，Engine 持有会话和任务，TSP UI 通过 IPC 访问。

关键文件：

| 文件 | 当前职责 | 接入影响 |
|---|---|---|
| [Core model](../crates/datapier-core/src/model/mod.rs) | `ProviderKind`、`Profile`、类型化值、资源和结果 | 增加连接来源及服务引用，保留数据库类型 |
| [Provider 分派](../crates/datapier-core/src/providers/mod.rs) | MySQL／Redis 会话建立和调用 | 增加 Archery HTTP 会话分支 |
| [存储](../crates/datapier-core/src/storage/mod.rs) | Profile、凭据、任务和结果 | 增加共享服务记录和批量保存 |
| [Engine server](../crates/datapier-engine/src/server.rs) | 连接、资源、任务及 IPC 分派 | 服务发现、批量添加、真实能力返回 |
| [连接表单](../crates/datapier-tsp/src/connections.rs) | 单连接配置与保存 | 增加来源和批量选择流程 |
| [导航](../crates/datapier-tsp/src/navigation.rs) | 连接、DB 显示范围和资源导航 | 复用 MySQL DB 选择，增加 Redis 所选 DB 范围 |

当前 `connection.open` 根据读写模式返回 `edit/import` 等能力，不能直接用于 Archery。接入后应由真实 provider 能力决定 UI 入口和 Core 校验。

### 4.2 功能复用范围

| 现有功能 | Archery 原生接口能否等价提供 | 建议处理 |
|---|---|---|
| SQL 编辑器、格式化和执行任务 | 可复用客户端部分 | 执行改走 HTTP provider |
| MySQL 表名、字段、索引和 DDL | 可适配，响应和查询路径不同 | 使用资源接口及受支持的 SHOW 查询 |
| 查询结果 grid、复制、本地分页 | 可以复用 | 保留列序、类型、行数组和平台元数据 |
| 导出 | 仅能导出已取得的数据 | 明确范围，不宣称取得原查询全量数据 |
| MySQL 行编辑、导入、删库删表 | 无现有直连提交语义 | 首期禁用；写入使用独立工单路径 |
| 显式事务、临时表、会话变量 | 无跨请求专属数据库会话保证 | 不开放等价能力 |
| Redis 命令和文本结果 | 部分可用 | 精确校验命令及参数 |
| Redis 完整 key 扫描 | 存在预览和截断问题 | 不声明为完整资源树 |
| Redis 原始字节、WATCH 编辑和批量变更 | 不能等价提供 | 不复用现有编辑提交路径 |
| 取消 | 没有通用的服务端撤销保证 | 区分停止等待与数据库端撤销 |

这些限制是能力差异，不是已完成的实现范围缩减。若目标要求与直连完全一致，需要增加 Archery 服务端协议能力。

### 4.3 MySQL 结果与元数据

查询返回有序 `column_list`、`column_type` 和行数组，客户端应保留这些结构。转成列名为 key 的对象会覆盖重复列名，也会丢失原始列序和部分元数据。

大整数采用字符串化保护，Decimal 和时间由 encoder 转换，但二进制、BIT 等存在转换边界。返回值可能已脱敏，不应直接作为编辑原值或无损备份数据。

元数据接口主要返回名称；`describetable` 返回的是 `SHOW CREATE TABLE`，不是统一字段结构。`SHOW FULL COLUMNS`、`SHOW INDEX`、`SHOW CREATE` 和 `EXPLAIN` 可进入 v1.12.0 的查询检查路径，`DESC/DESCRIBE` 不在同一放行范围；最终可用性仍取决于查询权限及数据库。现有 Datapier catalog 中的 `information_schema` 查询不能原封不动替换传输通道后就认为兼容。

Archery 查询的行数限制和 `fetchmany` 不是可恢复的服务端分页 cursor。本地缓存完成、平台查询返回完成、原查询结果完整是三个不同概念，应分别记录。

来源：[MySQL 元数据及查询](https://github.com/hhyo/Archery/blob/v1.12.0/sql/engines/mysql.py#L456-L597)、[查询权限](https://github.com/hhyo/Archery/blob/v1.12.0/sql/query_privileges.py#L44-L91)、[JSON 编码](https://github.com/hhyo/Archery/blob/v1.12.0/common/utils/extend_json_encoder.py)、[查询序列化](https://github.com/hhyo/Archery/blob/v1.12.0/sql/query.py#L191-L207)。

### 4.4 Redis 的必要限制

**原始字节丢失。** v1.12.0 连接使用 `decode_responses=True, encoding_errors="ignore"`。无效 UTF-8 字节被忽略后，多个原始 key 可能变为相同文本，客户端不能恢复其真实身份。命令文本再经过 `shlex.split`，也不等于原始字节参数协议。

**扫描不完整。** 资源接口只预览最多 100 个 key，没有返回完整扫描 cursor。手工 SCAN 查询把 cursor 放到第一行，再对整组结果按 `limit_num` 截断；已扫描的部分 key 可能被截掉，而返回 cursor 已推进，续扫会漏 key。调小 COUNT 也不能证明安全，因为 COUNT 不是返回数量的硬上限。

**缺少会话编辑语义。** 每次查询新建 Redis 客户端，接口不提供跨请求的专属物理连接或 WATCH 状态，不能承接 Datapier 当前的乐观并发编辑。

**上游只读检查不充分。** `query_check` 使用命令前缀正则，本地复现结果如下：

| 命令 | v1.12.0 检查结果 | 含义 |
|---|---|---|
| `GET sample`、`SCAN 0 COUNT 100` | 允许 | 基础读取可用 |
| `HSCAN sample 0`、`SSCAN sample 0`、`ZSCAN sample 0`、`HLEN sample` | 拒绝 | 部分浏览命令不可用 |
| `GETDEL sample`、`GETSET sample changed` | 允许 | 前缀检查误放行写命令 |
| `SUNIONSTORE dst src`、`SDIFFSTORE dst src`、`ZRANGESTORE dst src 0 -1` | 允许 | 前缀检查误放行写命令 |
| `WATCH sample`、`MULTI`、`SELECT 1` | 拒绝 | 不能模拟现有持久编辑会话 |

方案建议：继续在 Core 执行精确命令 token 和参数检查，不能只依赖 Archery 的只读判定。DB 通过请求目标传递，不通过自由 `SELECT` 命令切换。

来源：[连接及枚举](https://github.com/hhyo/Archery/blob/v1.12.0/sql/engines/redis.py#L25-L106)、[命令检查及结果转换](https://github.com/hhyo/Archery/blob/v1.12.0/sql/engines/redis.py#L108-L215)。

## 5. 最小改动接入方案

本节为方案建议，尚未实现。

### 5.1 连接与会话模型

保留 MySQL／Redis 作为数据库类型，新增 Direct／Archery 连接来源。不要把 Archery 当成一种数据库，也不要将 HTTP URL 塞入现有数据库 host 字段而保留无效端口、TLS 等组合。

建议的数据归属：

| 对象 | 保存内容 |
|---|---|
| Archery 服务 | 服务地址、登录身份、凭据引用 |
| 实例连接 | 服务引用、远端实例 ID／名称、MySQL 或 Redis 类型 |
| DB 选择 | 该实例被选中的 DB、默认 DB及导航显示范围 |
| 运行会话 | Cookie、CSRF、认证状态和当前请求目标 |

密码和必要认证秘密继续使用系统凭据库或 Engine 会话内存。多个实例引用同一服务凭据，不为每个实例或 DB 复制密码。远端实例名称和 ID 的一致性需在连接时校验，失效时明确报告，不自动切换到同名实例。

Core 内新增 Archery 会话分支，统一实现登录、资源请求、查询和结果转换；TSP 和未来 MCP 都调用 Engine。四个 crate 的既有分层保持成立，无需为本次接入新建独立 crate 或外部 provider 进程。

### 5.2 多实例、多 DB 添加

建议交互顺序：

1. 选择 Archery 来源，输入服务地址及认证信息。
2. 完成登录及必要的第二步认证。
3. 加载当前用户关联的实例，仅保留 MySQL、Redis。
4. 多选实例，分别加载和选择 DB。
5. 预览将保存的实例及 DB，确认后批量保存。

示例：选择 MySQL A 的 `app/order`、MySQL B 的 `billing`、Redis C 的 `0/3`，生成三个实例连接，各自保存选定 DB。MySQL 可复用现有 `visible_databases` 逻辑；Redis 需要支持所选 DB 范围，不能继续假设所有连接固定拥有 0–15。

DB 导航范围只控制展示，不替代授权。执行时仍绑定明确的服务、实例和 DB，并通过 Core 与 Archery 校验。

批量保存需要返回明确的成功和失败结果。SQLite 配置与系统凭据库没有跨系统事务，应沿用既有凭据所有权交接和失败追踪规则，不能宣称整个批次天然原子。

### 5.3 能力和结果契约

Archery 来源首期应提供查询、资源描述及受限结果导出，关闭没有等价保证的编辑、导入、事务和结构删除入口。Core 也必须拒绝这些操作，不能只隐藏 UI。

结果转换需要保留有序列、行数组、平台类型、脱敏信息、实际执行文本以及取得结果的范围。无法确认原始类型或字节身份时，按有损或文本结果表达，不伪造可逆的 `Bytes` 或可编辑原值。

若要完整支持 Redis 浏览和编辑，需要上游提供字节安全的参数与结果、不会截断 SCAN 批次的 cursor 协议，以及明确的会话和并发控制机制。这属于服务端扩展，不纳入客户端最小方案。

## 6. 版本兼容性

### 6.1 推荐下限

**统一接入的建议最低适配候选为 v1.8.4。** 此处“统一”包含 Web 查询与资源枚举，以及未来 SQL 工单 REST 能力，并不表示已经实测或所有接口都没有缺陷。

| 官方版本 | 核对结果 | 兼容性建议 |
|---|---|---|
| v1.12.0 | 相关查询、资源、认证和工单接口具备 | 作为主要验收基线 |
| v1.13.0、v1.14.0 | 相关路由延续，仍无 REST 在线查询 | 纳入端到端兼容验收 |
| v1.11.3 | 相关接口具备 | 向下适配候选；不由此推定所有 v1.11 小版本均已核对 |
| v1.10.0 | 相关接口具备，Redis INFO 回退枚举有边界问题 | 单独验证枚举和响应 |
| v1.9.0、v1.9.1 | 相关接口具备，Redis CONFIG 失败后的回退存在实现问题 | 要求准确枚举条件满足，或明确报错 |
| v1.8.4、v1.8.5 | 已有 JWT、实例资源、工单 REST 和 Web 查询 | 统一最低适配候选 |
| v1.8.0–v1.8.3 | 缺少统一方案所需的上述 REST 路由 | 不纳入统一适配承诺 |
| v1.7.0 | Web 查询和枚举已有源码 | 仅为更早查询能力的证据，不是绝对历史下限 |

分界证据：[v1.8.3 REST 路由](https://github.com/hhyo/Archery/blob/v1.8.3/sql_api/urls.py)、[v1.8.4 REST 路由](https://github.com/hhyo/Archery/blob/v1.8.4/sql_api/urls.py)、[v1.12.0 REST 路由](https://github.com/hhyo/Archery/blob/v1.12.0/sql_api/urls.py)、[v1.13.0 REST 路由](https://github.com/hhyo/Archery/blob/v1.13.0/sql_api/urls.py)、[v1.14.0 REST 路由](https://github.com/hhyo/Archery/blob/v1.14.0/sql_api/urls.py)。

纯 Web 查询可以早于 1.8.4，但本次没有追查 v1.6、v1.5 等更早版本，不能把 1.7.0 称为绝对最低版本。对于尚未检查的未来版本，应依靠能力探测和回归验收，不作无条件兼容承诺。

### 6.2 Redis DB 枚举的版本差异

准确枚举优先依赖 `CONFIG GET databases`。回退到 INFO 并不能提供相同保证。

| 版本 | CONFIG 不可用时的源码行为 |
|---|---|
| v1.8.4 | 虽构造 16 个 DB，仍设置错误，资源接口可能直接报错 |
| v1.9.0 | `max(dbs, [16])` 得到列表后转整数，回退存在类型问题 |
| v1.10.0 | `range(max(dbs + [16]))` 会漏掉编号大于等于 16 的最大非空 DB |
| v1.11.3、v1.12.0、v1.13.0、v1.14.0 | `max(dbs + [15]) + 1` 至少给出 0–15，但无法发现最大非空 DB 之后的空 DB；INFO 本身也可能无权调用 |

来源：[v1.8.4 Redis](https://github.com/hhyo/Archery/blob/v1.8.4/sql/engines/redis.py#L44-L60)、[v1.9.0 Redis](https://github.com/hhyo/Archery/blob/v1.9.0/sql/engines/redis.py)、[v1.10.0 Redis](https://github.com/hhyo/Archery/blob/v1.10.0/sql/engines/redis.py#L57-L76)、[v1.11.3 Redis](https://github.com/hhyo/Archery/blob/v1.11.3/sql/engines/redis.py#L60-L89)。

枚举错误和推定范围应明确展示，不能将其静默当作完整 DB 清单。Redis Cluster 的连接分支不使用普通多 DB 参数，不能据普通 Redis 的行为承诺 Cluster 多 DB。

## 7. SQL 上线工单与未来 MCP

### 7.1 REST 能力和参数

以下为官方 v1.12.0 的源码行为，接口存在不等于已完成运行验收。

| 操作 | 方法和路径 | 必要参数及关键语义 |
|---|---|---|
| 检查 | `POST /api/v1/workflow/sqlcheck/` | `instance_id`、`db_name`、`full_sql`；要求 `sql.sql_submit` |
| 提交 | `POST /api/v1/workflow/` | `sql_content`；嵌套 `workflow` 至少包含 `workflow_name/group_id/instance/db_name`，提交时重新检查 |
| 列表 | `GET /api/v1/workflow/` | 可按过滤器查询，但存在 queryset 使用缺陷，不能直接依赖 |
| 待审核 | `POST /api/v1/workflow/auditlist/` | `engineer`；返回字段需适配，不能直接视为完整工单引用 |
| 审核／撤回 | `POST /api/v1/workflow/audit/` | `engineer/workflow_id/workflow_type=2/audit_type/audit_remark`；`audit_type` 为 `pass` 或 `cancel` |
| 执行 | `POST /api/v1/workflow/execute/` | `engineer/workflow_id/workflow_type=2/mode`；`auto` 入队，`manual` 标记线下完成 |
| 日志 | `POST /api/v1/workflow/log/` | `workflow_id/workflow_type=2`；操作日志不等于逐条 SQL 执行结果 |

提交可带 `demand_url`、`is_backup`、`run_date_start/end` 等字段。允许执行时间窗口不等于已经创建定时任务。提交成功后，工单可能因自动审核规则直接变为审核通过或自动审核不通过。

来源：[REST 工单实现](https://github.com/hhyo/Archery/blob/v1.12.0/sql_api/api_workflow.py)、[serializer](https://github.com/hhyo/Archery/blob/v1.12.0/sql_api/serializers.py#L269-L548)。

### 7.2 Web 补充能力

| 能力 | 接口 | 限制 |
|---|---|---|
| 审核／执行结果 | `GET /sqlworkflow/detail_content/` | 根据状态返回审核或执行结果，检查查看权限 |
| 获取回滚 SQL | `GET /sqlworkflow/backup_sql/` | 要求有备份及符合状态；不执行回滚 |
| 定时执行 | `POST /timingtask/` | `workflow_id/run_date`，检查执行权限和时间 |
| 终止／驳回 | `POST /cancel/` | `workflow_id/cancel_remark`，按权限和当前状态处理 |
| 工单状态 | `POST /getWorkflowStatus/` | 所示实现没有逐工单 `can_view` 检查，不能单独作为授权依据 |

这些接口需要 Web 会话；部分写操作返回重定向或 HTML，不是统一 REST JSON。Web cancel 的源码先修改状态，再判断原定时状态，存在未清理定时任务的分支问题；worker 仍有状态防线，但不能将取消响应解释为定时任务已可靠清理。

来源：[Web 工单实现](https://github.com/hhyo/Archery/blob/v1.12.0/sql/sql_workflow.py#L127-L499)。

### 7.3 接入前需要处理的问题

| 问题 | 源码证据与影响 | 建议 |
|---|---|---|
| REST 列表 queryset 使用错误 | `get()` 使用 `self.queryset`，没有调用已定义的 `get_queryset()`；v1.14.0 对应实现仍如此 | 修复服务端或明确使用其他已验证读取路径 |
| 操作者未绑定登录用户 | 审核、执行根据请求体 `engineer` 查用户，相关视图未应用 `IsOwner` 绑定 | 服务端修正，客户端仅从可信登录上下文填写身份 |
| 检查、提交权限不同 | sqlcheck 和提交显式使用 `IsAuthenticated`，覆盖默认 API 白名单；其他接口多继承默认白名单 | 逐接口验证，不用统一白名单假设代替授权 |
| 资源组关联校验不完整 | 提交验证实例可访问，但所选 `group_id` 未同时绑定用户和实例的共同所属关系 | 补齐服务端关联检查 |
| 审批节点 actor 校验缺口 | 默认 AuditV2 查询当前节点是否有活跃用户，未将 actor 本人纳入该查询 | 开放审批前核实并修正 |
| 无端到端幂等键 | 提交或执行超时后可能已经创建、入队或执行 | 记录远端 ID和未知结果，不自动重试写请求 |

这些是公开源码中的实现观察，不代表任何特定运行环境已被验证存在同样行为。

来源：[列表实现](https://github.com/hhyo/Archery/blob/v1.12.0/sql_api/api_workflow.py#L77-L125)、[v1.14.0 对照](https://github.com/hhyo/Archery/blob/v1.14.0/sql_api/api_workflow.py#L77-L125)、[审核身份](https://github.com/hhyo/Archery/blob/v1.12.0/sql_api/api_workflow.py#L218-L245)、[权限类](https://github.com/hhyo/Archery/blob/v1.12.0/sql_api/permissions.py)、[提交关联校验](https://github.com/hhyo/Archery/blob/v1.12.0/sql_api/serializers.py#L300-L406)、[审批节点](https://github.com/hhyo/Archery/blob/v1.12.0/sql/utils/workflow_audit.py#L377-L425)。

### 7.4 状态和数据库语义

SQL 工单状态包括 `workflow_manreviewing`、`workflow_autoreviewwrong`、`workflow_review_pass`、`workflow_timingtask`、`workflow_queuing`、`workflow_executing`、`workflow_finish`、`workflow_exception`、`workflow_abort`。审批记录另有整数状态，不能与 SQL 工单状态混用。

`execute mode=auto` 成功响应只表示执行已入队，不表示 SQL 已成功；`mode=manual` 只确认线下执行完成，不发送 SQL。MySQL 经 goInception 等完成检查、执行及备份；Redis 按非空行拆命令并逐条执行，后续失败不会撤销前面的成功操作。Redis 检查只是逐行构造成功结果及零影响行数，不能视为语法、安全或影响范围验证。

异步 worker 的锁和状态检查不等于请求端到端幂等，也不能消除执行后回调失败造成的未知结果。工单由远端系统持有；Engine 退出不代表远端工单取消，应把本地请求状态和远端执行状态分别记录。

来源：[工单状态](https://github.com/hhyo/Archery/blob/v1.12.0/sql/models.py#L267-L277)、[审批枚举](https://github.com/hhyo/Archery/blob/v1.12.0/common/utils/const.py#L12-L35)、[执行模式](https://github.com/hhyo/Archery/blob/v1.12.0/sql_api/api_workflow.py#L325-L400)、[Redis 工单](https://github.com/hhyo/Archery/blob/v1.12.0/sql/engines/redis.py#L217-L282)、[异步执行](https://github.com/hhyo/Archery/blob/v1.12.0/sql/utils/execute_sql.py#L16-L100)。

### 7.5 未来 MCP 的建议边界

建议按权限拆分查询／检查／读取、提交、审批／执行／定时三组能力，独立授权。`sql_check` 必须区分平台检查成功、检查失败和平台检查未完成，不能把本地规则通过包装成平台审核通过。

MCP 的目标使用服务、实例、DB 的明确引用；操作者从可信认证上下文取得，不能允许模型自由指定 `engineer`。最少持久化服务引用、操作者、实例与 DB、资源组、SQL 摘要、客户端请求 ID、远端工单 ID、请求确认状态及最近远端状态。客户端请求 ID 或 SQL 摘要只用于关联，不能替代服务端幂等键。

## 8. 参考项目评估

| 项目 | 可复用思路 | 已发现的差异或问题 |
|---|---|---|
| ckall/archery-mcp-server | JWT 客户端、REST 工单工具和参数组织 | 查询工具使用 `/api/v1/sqlquery/*`，官方 v1.12.0、v1.14.0 没有这些路由；按表找实例也使用缺失路由；不能用 README 的“1.8+”证明整体可用 |
| itzhouq/archery-mcp | Web Session、CSRF、TOTP、查询与 REST 检查组合 | 固定单目标；作者仅声明 v1.10.0 实测；结果转字典会覆盖重复列名；`describe_table` 对字段结果的假设与官方 v1.12.0 的 SHOW CREATE 返回不一致 |

itzhouq 的本地检查和平台检查合并结果还需要关注平台检查状态，不能只读取一个总 `passed`。其完全委托平台判定查询安全的做法也不适合直接替代 Datapier 的 Core 只读策略。

这些项目适合作为交互和客户端实现参考，不能作为官方接口契约或端到端兼容证据。若复制代码，应单独核实许可证及保留要求。

来源：[ckall client](https://github.com/ckall/archery-mcp-server/blob/5846378be1b68116563b426b0138ac404a473405/src/archery_mcp/client.py)、[ckall 查询工具](https://github.com/ckall/archery-mcp-server/blob/5846378be1b68116563b426b0138ac404a473405/src/archery_mcp/server.py#L174-L315)、[itzhouq client](https://github.com/itzhouq/archery-mcp/blob/1836fde230f017e0ca1fa90b14745f6960bcbc08/src/archery_mcp/client.py)、[itzhouq 查询工具](https://github.com/itzhouq/archery-mcp/blob/1836fde230f017e0ca1fa90b14745f6960bcbc08/src/archery_mcp/tools/query.py#L15-L101)、[检查合并逻辑](https://github.com/itzhouq/archery-mcp/blob/1836fde230f017e0ca1fa90b14745f6960bcbc08/src/archery_mcp/tools/sql_check.py#L67-L105)。

## 9. 已执行验证与待验收项

### 9.1 已执行

- 核对 Datapier 的连接模型、provider 分派、凭据存储、Engine 能力返回和 DB 选择实现。
- 核对公开 Archery 固定 tag 的路由、认证、查询、MySQL／Redis 引擎及工单源码。
- 核对两个参考项目的固定 commit 实现。
- 从官方 v1.12.0 Redis 源码中通过 AST 提取 `query_check` 原函数，本地运行命令分类，得到第 4.4 节结果。
- 本地验证忽略无效 UTF-8 的行为：十六进制 `61`、`ff61`、`8061` 均解码为 `a`，证明该解码方式不能保留字节身份。

没有进行服务登录、数据库查询或工单创建、审批、执行；没有形成任何版本的端到端实测兼容承诺。

### 9.2 原函数本地复现方法

以下脚本只下载公开源码并执行命令分类函数，不连接 Redis，不发送数据库命令。下载失败时应直接报告错误，不改用其他版本源码。

```python
import ast
import re
import urllib.request

url = "https://raw.githubusercontent.com/hhyo/Archery/v1.12.0/sql/engines/redis.py"
source = urllib.request.urlopen(url, timeout=30).read().decode()
tree = ast.parse(source)
engine = next(
    node for node in tree.body
    if isinstance(node, ast.ClassDef) and node.name == "RedisEngine"
)
check = next(
    node for node in engine.body
    if isinstance(node, ast.FunctionDef) and node.name == "query_check"
)
namespace = {"re": re}
exec(
    compile(ast.Module(body=[check], type_ignores=[]), url, "exec"),
    namespace,
)
for command in [
    "GET sample",
    "HSCAN sample 0",
    "GETDEL sample",
    "GETSET sample changed",
    "SUNIONSTORE dst src",
]:
    result = namespace["query_check"](None, db_name="0", sql=command)
    print(command, "拒绝" if result["bad_query"] else "允许")
```

### 9.3 实现前后的验收范围

| 验收面 | 需要确认的行为 |
|---|---|
| 认证 | 普通登录、CSRF、2FA、过期会话、重定向与错误识别 |
| 批量添加 | 多实例多 DB、重复选择、权限变更、枚举失败和保存失败 |
| MySQL | 元数据、空结果、重复列名、精度、二进制、脱敏和返回范围 |
| Redis | 精确只读检查、DB 枚举条件、文本身份限制和扫描完整性 |
| 工单 | 检查与提交权限、列表读取、可信操作者、审批链、入队与最终结果 |
| 恢复 | 请求超时、远端已接受但响应丢失、Engine 退出后远端继续执行 |
| 版本 | v1.8.4 最低候选及 v1.12.0、v1.13.0、v1.14.0 的真实交互路径 |

验收应使用可控的隔离环境。源码具备接口、客户端编译成功、mock 测试通过均不能替代上述运行证据。
