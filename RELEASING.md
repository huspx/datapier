# 发布指南

## 当前发布方式与边界

当前实现版本为 0.1.0，尚未发布。现有工作流 `.github/workflows/native-release.yml` 在 push、pull request 或手动触发时执行独立格式检查，并为六个原生目标构建和生成 Actions artifact；它的权限为 `contents: read`，不会创建 GitHub Release 或上传 Release 资源。正式分发需要在验收完成后人工创建 GitHub Release 并上传组包产物。现有配置没有自动发布到 registry、商店、crates.io 或 npm 的渠道。

[技术方案第 11 节](docs/technical-design-0.1.0.md#11-实现与运行记录)汇总既有 macOS arm64 运行证据和未验收范围；其余五个目标尚未完成原生验收。六目标产物、组包及候选版本运行结果均须按发布前检查核实。

本指南描述发布步骤，不代表已完成这些操作，也不构成提交、推送、合并、创建 tag 或发布的授权。工作区有文件不代表目标提交或分发仓库包含它们；正式发布前必须确保实际源码、Cargo 配置和锁文件、plugin、安装器、组包脚本及工作流都已进入目标提交，不能依赖未跟踪或被忽略的本地文件。

## 版本号与关联文件

发布前逐项核对版本一致；目前以下位置均为 `0.1.0`：

- 根 `Cargo.toml` 的 `workspace.package.version`；四个 crate 的 `Cargo.toml` 通过 `version.workspace = true` 继承，`Cargo.lock` 中四个本地包版本应同步。CLI 的 clap `version` 继承 crate 版本。
- TSP 应用版本、`plugins/datapier/plugin.toml` 的 plugin 版本，以及 `plugins/datapier/install/unix.sh` 和 `plugins/datapier/install/windows.ps1` 的安装器版本。
- `scripts/package-target.ps1` 和 `scripts/assemble-release.ps1` 的默认版本；前者只接受数字三段版本，不接受预发布后缀，后者目前有仅接受 `0.1.0` 的硬编码 guard。发布其他版本须先同步修改相关版本和 guard，不能仅传 `-Version`。
- `.github/workflows/native-release.yml` 的 CLI smoke 期望输出 `datapier 0.1.0` 和汇总 artifact 名 `datapier-0.1.0-release`；新版本也要同步这些硬编码。

产品规划和技术方案中作为历史记录的版本标题不需要机械改写。版本调整需与实际发布内容和验证记录一致。

## 发布说明与 CHANGELOG

根目录 [CHANGELOG.md](CHANGELOG.md) 由贡献者人工维护，功能变更记入适用的未发布条目，发布时整理到对应版本；当前没有自动生成发布说明的机制。维护者结合变更记录、[技术方案运行记录](docs/technical-design-0.1.0.md#11-实现与运行记录)及该版本真实验收记录人工整理发布说明：列出功能与兼容性变化、支持范围、已知限制、各平台的实际 OS／glibc、Tern、数据库版本及验证结果，并说明未执行的检查。

## 发布前检查

按[开发规范中的测试与验证](CONTRIBUTING.md#测试与验证)完成适用检查，记录实际命令和结果。默认测试不包含默认忽略的真实数据库专项，编译、mock、交叉编译或模拟运行均不能替代实际运行证明。发布前六个目标都必须原生运行验收，不能因缺少环境而静默缩减范围。

- 使用隔离的真实 MySQL 5.7／8.0／8.4 和 Redis 6.2／7.2／8.0 单节点，按实际版本验证连接、查询、编辑提交、导入导出、只读、并发冲突与取消；覆盖大整数、精确小数、NULL、二进制、普通空结果、多结果、写入断连不重放、Redis List 重复值、WATCH 与 TTL。数据库测试只创建并清理自身隔离资源，不使用生产数据或真实凭据作为样本。
- 在实际 Tern 中验证完整交互，包括中文／emoji、选区、分页、大字段、草稿恢复、两个 UI、十万行全量导出及百万 key 增量扫描；观察 IPC、客户端退出及 Engine 收尾的进程和资源状态。验证 TLS 成功连接及错误 CA／身份拒绝、系统凭据存取和凭据服务不可用时的行为，确保日志、配置、导出和运行描述不含秘密。
- 在每个原生目标验证干净安装、下载与 SHA-256 校验、缓存复用、启动、IPC 权限和退出恢复。macOS 检查 Keychain；Linux 检查 Secret Service 不可用路径；Windows 检查 Named Pipe 当前用户权限及拒绝远程客户端、Tern 随附 ConPTY／OSC 877、进程退出、文件锁和活动 exe 更新。记录平台、架构、系统和依赖版本及实际结果。
- 核对 README 的安装、功能和平台描述与实际结果一致，保留 [MIT License](LICENSE) 及第三方分发要求。0.1.0 的 MySQL／Redis 单节点范围及不支持项目以 [README](README.md) 和[技术方案](docs/technical-design-0.1.0.md#1-目标与范围)为准，不以配置或单一平台结果扩大支持声明。

各目标验收结果应能关联具体提交、构建产物和环境；任何未完成项保留为发布阻塞。详细验收场景见[技术方案第 8 节](docs/technical-design-0.1.0.md#8-跨平台与分发)和[第 9 节](docs/technical-design-0.1.0.md#9-验收要求)。正式 Release 下载地址需在实际上传后核对；发布前对候选包和隔离下载源的安装验证，不能写成正式下载地址已可用。

## PR 与合并

现有配置和历史未约定固定发布分支、分支命名、PR 审批数量或合并方式，本指南不新增这些要求。如使用 PR，将版本同步、发布说明及所需源码和配置纳入候选变更，按仓库实际目标分支提交并检查对应工作流结果。合并不代表发布完成；最终提交可能与候选提交不同，应在最终提交重跑或核对对应 workflow，并使用该提交的六目标产物完成必要的重新验收与组包。记录产物对应的提交和 workflow run，不把不同提交的文件混作一个版本。

## tag 与目标提交

在六目标产物、完整组包和运行验收均已完成且最终提交确定后，发布操作获授权时，将 `v<Version>` tag 指向实际验证并用于组包的最终提交。`v` 前缀来自组包脚本生成的下载地址 `https://github.com/$Repository/releases/download/v$Version`；务必让 tag、Release、plugin 内下载地址和上传文件一致。现有配置不要求特定 tag 签名或注释形式。

## 构建与组包

现有 workflow 六个原生 runner 与 target 如下：

| runner | target |
|---|---|
| macos-15-intel | x86_64-apple-darwin |
| macos-15 | aarch64-apple-darwin |
| ubuntu-24.04 | x86_64-unknown-linux-gnu |
| ubuntu-24.04-arm | aarch64-unknown-linux-gnu |
| windows-2022 | x86_64-pc-windows-msvc |
| windows-11-arm | aarch64-pc-windows-msvc |

独立 `format` job 的职责与变更文件范围见[贡献指南的代码格式化](CONTRIBUTING.md#代码格式化)。每个 native job 检出代码、设置 stable Rust 和对应 target，Linux 安装构建依赖，核对 runner 原生架构，运行 workspace 默认测试，再构建 release 可执行文件、运行 `--version` smoke、调用单目标组包脚本并上传 `datapier-<target>` Actions artifact。默认测试与版本 smoke 不等于上面的真实运行验收。随后 bundle job 下载并合并六个 artifact，组装启动包和校验清单，上传汇总 Actions artifact；该产物仍需人工发布。

也可在各目标原生机器上，从仓库根目录执行对应 release 构建和组包。以下示例只处理本机对应的单个 target；将 `$Target` 替换为上表中的实际目标，构建后仍须完成相应运行验收。六个归档与 sidecar 必须均来自同一最终提交，为每次发布使用干净的输出目录，避免混入旧 `dist` 或其他提交的文件。

```powershell
$Version = '0.1.0'
$Target = 'aarch64-apple-darwin'
cargo build --release --package datapier --target $Target --locked
pwsh -File ./scripts/package-target.ps1 -Target $Target -Version $Version -Output dist
```

收集六个目标的归档及对应 `.binary-sha256` 文件到同一个干净 `dist` 后，在相同提交的仓库根目录运行下列命令。`OWNER/REPO` 必须替换为实际 GitHub 发布仓库的 owner 和 repository；也可不传 `-Repository`，由实际环境的 `GITHUB_REPOSITORY` 提供。不要猜测仓库地址。

```powershell
$Version = '0.1.0'
$Repository = 'OWNER/REPO'
pwsh -File ./scripts/assemble-release.ps1 -Artifacts dist -Version $Version -Repository $Repository
```

单目标脚本读取 `target/<target>/release/datapier`（Windows 为 `datapier.exe`），生成 `datapier-<version>-<target>.tar.gz`（Windows 为 `.zip`）及该可执行文件的 `datapier-<version>-<target>.binary-sha256` sidecar。汇总脚本要求六个归档和六个有效 sidecar，生成 `SHA256SUMS` 与 `datapier-plugin-<version>.zip`。`SHA256SUMS` 仅包含六个二进制归档的摘要，不包含 plugin zip；sidecar 用于生成 plugin 内的二进制摘要清单。核对 plugin zip 的 `datapier/` 目录内含 plugin 源文件、`LICENSE`、`release-url.txt`、`release-sha256.txt` 和 `release-bin-sha256.txt`，下载地址指向实际发布仓库和版本；并核对归档及解压后二进制分别匹配相应清单。

发布安装无需 Rust：安装器校验归档及程序摘要后复用版本化缓存。`DATAPIER_RELEASE_URL` 仅覆盖已打包的下载地址，`DATAPIER_CACHE_DIR` 指定安装缓存目录；发布核对时应同时确认默认下载地址和缓存行为。

## 实际发布与发布后核对

完成最终提交的上述验收和组包后，在获得相应操作授权时，按 [tag 与目标提交](#tag-与目标提交)确定 tag，并人工创建对应 GitHub Release，上传六个二进制归档、`SHA256SUMS` 和 `datapier-plugin-<version>.zip`，附上人工整理的发布说明。Actions artifact 和本地 `dist` 本身不会成为 Release 下载资源。

发布后从实际 Release 地址核对所有资源的文件名、下载地址、版本与摘要，确认 plugin 内的地址、归档摘要和二进制摘要与已上传资源一致；在相应原生环境验证干净安装、缓存与 Tern 启动，并记录结果。发现不一致或验收缺项时如实记录并停止将其描述为完整六目标发布。
