# 发布指南

## 当前发布方式与边界

0.1.0 已于 2026-11-01 在 [GitHub Releases](https://github.com/huspx/datapier/releases/tag/v0.1.0) 正式公开，tag `v0.1.0` 对应提交 `fadaec502e4926f32c7d5aa0ef752f0c335a07a9`。正式分发入口为 [GitHub Releases](https://github.com/huspx/datapier/releases)。现有工作流 `.github/workflows/native-release.yml` 仅在目标为 `main` 的 pull request、`main` 的 push 或手动触发时执行独立格式检查和六目标原生验证；只有运行 ref 为 `refs/heads/main` 时组包并生成 Actions artifact，PR 和手动运行其他分支均不生成交付物。普通工作分支和 tag 的 push 不自动触发。工作流权限为 `contents: read`，不会创建 GitHub Release 或上传 Release 资源，正式发布仍由维护者执行。现有配置没有自动发布到 registry、商店、crates.io 或 npm 的渠道。

[技术方案第 11 节](docs/technical-design-0.1.0.md#11-实现与运行记录)汇总既有 macOS arm64 运行证据和未验收范围；其余五个目标尚未完成完整原生验收。0.1.0 经维护者明确接受已披露的验收范围后公开，例外与限制保留在对应 Release 说明中；该次授权不代表其余平台已验收，也不自动放宽后续版本的发布要求。六目标产物、组包及候选版本运行结果均须按发布前检查核实。

本指南描述发布步骤，不代表已完成这些操作，也不构成提交、推送、合并、创建 tag 或发布的授权。工作区有文件不代表目标提交或分发仓库包含它们；正式发布前必须确保实际源码、Cargo 配置和锁文件、plugin、安装器、组包脚本及工作流都已进入目标提交，不能依赖未跟踪或被忽略的本地文件。

调整为 Actions 内发布时，应复用已通过格式检查、六目标构建和汇总组包的固定提交产物，校验 tag、workflow run 的成功状态及提交 SHA 一致，再由发布 job 上传原文件，无需经维护者本机中转或为发布再次编译。仅发布 job 授予 `contents: write`，PR 检查和普通构建保持只读；发布仍须满足验收与操作授权。跨 job／workflow 使用 artifact 传递，同一 job 已有完整 `dist` 时可直接上传 Release；GitHub 不支持用 artifact ID 直接替代 Release asset 上传。当前工作流尚未实现这段自动发布。

## 版本号与关联文件

根 `Cargo.toml` 的 `workspace.package.version` 是唯一人工维护的产品版本。四个 crate 继承此版本，CLI 和 TSP 编译时使用对应 crate 版本；修改后通过 Cargo 更新 `Cargo.lock` 中本地包版本，不手工编辑锁文件。组包当前支持无前导零的三段数字版本，不支持预发布后缀。

修改根版本后，从仓库根目录运行以下命令同步源码 plugin，再用只读检查确认一致；源码 plugin 保持可直接 `tern plugin link`，Cargo 构建不会修改它。CI 同样运行 `--check`，版本缺失、重复、无效或不一致时失败。

```sh
python3 scripts/sync-version.py
python3 scripts/sync-version.py --check
```

安装器从随插件提供的 `plugin.toml` 读取版本并校验程序版本。组包脚本和 CI 从根 manifest 读取版本，显式传入 `--version` 时必须与根版本一致；汇总时只同步临时插件副本，不改源码。Windows 用户安装器使用系统自带的 Windows PowerShell 5.1，须保留其 UTF-8 BOM。

产品规划和技术方案中作为历史记录的版本标题不需要机械改写。版本调整需与实际发布内容和验证记录一致。

### Python 3.15 工具环境

开发发布脚本使用 Python 3.15，仅依赖标准库。CI 通过 `actions/setup-python@v7` 配置 `python-version: '3.15'`，跟随该分支补丁版本，架构随原生 runner；短命令使用 Bash，架构检查和二进制 smoke 直接使用 Python。开发环境、格式化与回归命令见[贡献指南](CONTRIBUTING.md#开发环境)。

版本读取与校验共用 `scripts/version.py`；同步、单目标组包和六目标汇总分别使用 `sync-version.py`、`package-target.py` 和 `assemble-release.py`。Windows 用户安装器继续使用系统自带 Windows PowerShell 5.1，Unix 安装器使用 `/bin/sh`；安装与运行 Datapier 无需 Python，开发与组包无需 `pwsh`。

脚本回归覆盖版本同步、只读检查不写文件、无效或重复版本及显式版本不一致拒绝，以及缺失／无效 sidecar 拒绝和汇总不改源码 plugin。实际组包须使用本机真实二进制，解包核对内容、归档与程序摘要；完整汇总必须收齐同一提交的六目标原生产物。脚本测试和本机组包不能代替六目标 CI 或完整原生运行验收。

## 发布说明与 CHANGELOG

根目录 [CHANGELOG.md](CHANGELOG.md) 由贡献者人工维护，记录变更内容；当前没有自动生成发布说明或回写发布状态的机制。版本章节存在、CI 成功、artifact 上传完成、tag 创建或 Release 草稿创建，都不等于正式发布完成。

- 日常新变更记入顶部的“未发布（Unreleased）”章节，不把新功能追加到已经发布的版本章节。
- 发布准备 PR 将待发布条目整理为对应版本章节，并同步版本号、安装器和组包配置。版本章节是该提交的内容快照，不使用“已发布／未发布”标记推断线上状态，也不预填实际发布日期。
- 对外发布状态以 GitHub Release 为准：`draft=true` 为草稿，`draft=false` 且 `prerelease=true` 为预发布，`draft=false` 且 `prerelease=false` 为正式公开；实际发布时间使用 `published_at`。资源完整性、摘要及安装验证仍须另行核对，不能只看这些字段。
- 发布失败时保留草稿和诊断记录，不声称已发布；重试应优先复用同一提交、同一已验证 workflow 的完整产物。产物过期、校验失败或源码改变时，重新完成对应构建和验证，不混用旧文件，不移动已经公开的版本 tag。
- 发布成功后不再自动提交一次“改为已发布”的 CHANGELOG 更新，不改写已验证 tag；发布时间、正式下载验证和验收范围写入 GitHub Release 说明。已发布的 0.1.0 保留在其版本章节，后续变更归入未发布章节。

维护者结合变更记录、[技术方案运行记录](docs/technical-design-0.1.0.md#11-实现与运行记录)及该版本真实验收记录人工整理发布说明：列出功能与兼容性变化、支持范围、已知限制、各平台的实际 OS／glibc、Tern、数据库版本及验证结果，并说明未执行的检查。

## 发布前检查

按[开发规范中的测试与验证](CONTRIBUTING.md#测试与验证)完成适用检查，记录实际命令和结果。默认测试不包含默认忽略的真实数据库专项，编译、mock、交叉编译或模拟运行均不能替代实际运行证明。发布前六个目标都必须原生运行验收，不能因缺少环境而静默缩减范围。

- 使用隔离的真实 MySQL 5.7／8.0／8.4 和 Redis 6.2／7.2／8.0 单节点，按实际版本验证连接、查询、编辑提交、导入导出、只读、并发冲突与取消；覆盖大整数、精确小数、NULL、二进制、普通空结果、多结果、写入断连不重放、Redis List 重复值、WATCH 与 TTL。数据库测试只创建并清理自身隔离资源，不使用生产数据或真实凭据作为样本。
- 在实际 Tern 中验证完整交互，包括中文／emoji、选区、分页、大字段、草稿恢复、两个 UI、十万行全量导出及百万 key 增量扫描；观察 IPC、客户端退出及 Engine 收尾的进程和资源状态。验证 TLS 成功连接及错误 CA／身份拒绝、系统凭据存取和凭据服务不可用时的行为，确保日志、配置、导出和运行描述不含秘密。
- 在每个原生目标验证干净安装、下载与 SHA-256 校验、缓存复用、启动、IPC 权限和退出恢复。macOS 检查 Keychain；Linux 检查 Secret Service 不可用路径；Windows 检查 Named Pipe 当前用户权限及拒绝远程客户端、Tern 随附 ConPTY／OSC 877、进程退出、文件锁和活动 exe 更新。记录平台、架构、系统和依赖版本及实际结果。
- 核对 README 的安装、功能和平台描述与实际结果一致，保留 [MIT License](LICENSE) 及第三方分发要求。0.1.0 的 MySQL／Redis 单节点范围及不支持项目以 [README](README.md) 和[技术方案](docs/technical-design-0.1.0.md#1-目标与范围)为准，不以配置或单一平台结果扩大支持声明。

各目标验收结果应能关联具体提交、构建产物和环境；任何未完成项保留为发布阻塞。详细验收场景见[技术方案第 8 节](docs/technical-design-0.1.0.md#8-跨平台与分发)和[第 9 节](docs/technical-design-0.1.0.md#9-验收要求)。正式 Release 下载地址需在实际上传后核对；发布前对候选包和隔离下载源的安装验证，不能写成正式下载地址已可用。

## PR 与合并

`main` 必须通过 PR 合并，CODEOWNERS、审批、Merge commit 和仅限 PR 的绕过要求见[贡献指南的分支与 PR](CONTRIBUTING.md#分支与-pr)。将版本同步、CHANGELOG 版本内容、发布说明及所需源码和配置纳入发布准备 PR，检查对应工作流结果；CODEOWNERS 的审批分配以 PR 目标分支已有定义为准。合并不代表发布完成；最终提交可能与候选提交不同，应在最终提交重跑或核对对应 workflow，并使用该提交的六目标产物完成必要的重新验收与组包。记录产物对应的提交和 workflow run，不把不同提交的文件混作一个版本。

## tag 与目标提交

在六目标产物、完整组包和运行验收均已完成且最终提交确定后，发布操作获授权时，将 `v<Version>` tag 指向实际验证并用于组包的最终提交。`v` 前缀来自组包脚本生成的下载地址 `https://github.com/<owner>/<repo>/releases/download/v<version>`；务必让 tag、Release、plugin 内下载地址和上传文件一致。现有配置不要求特定 tag 签名或注释形式。

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

独立 `format` job 的职责与变更文件范围见[贡献指南的代码格式化](CONTRIBUTING.md#代码格式化)，并检查源码 plugin 版本。每个 native job 检出代码、设置 stable Rust 和对应 target，Linux 安装构建依赖，核对 runner 原生架构，运行 workspace 默认测试，再构建 release 可执行文件并运行 `--version` smoke。PR 到此完成验证，不执行单目标组包、不上传 artifact，runner 内临时二进制随 job 清理。只有运行 ref 为 `refs/heads/main` 时，才调用单目标组包脚本并上传 `datapier-<version>-<target>` Actions artifact；bundle job 同样仅在 main 运行，下载并合并六个 artifact，组装启动包和校验清单，上传汇总 Actions artifact。手动选择 main 时生成交付物，选择其他分支时仅验证。默认测试、版本 smoke 和交付物生成不等于完整原生运行验收；正式发布仍按后文人工核对。

官方 Actions 使用 Node.js 24 运行时：`actions/checkout@v7`、`actions/upload-artifact@v7` 和 `actions/download-artifact@v8`。升级时核对官方运行时声明、runner 要求及产物参数兼容性；当前使用 GitHub 托管 runner。

也可在各目标原生机器上，从仓库根目录执行对应 release 构建和组包。以下示例只处理本机对应的单个 target；将 `target` 替换为上表中的实际目标，运行验收范围按[发布前检查](#发布前检查)执行。六个归档与 sidecar 必须均来自同一最终提交，为每次发布使用干净的输出目录，避免混入旧 `dist` 或其他提交的文件。

```sh
target='aarch64-apple-darwin'
cargo build --release --package datapier --target "$target" --locked
python3 scripts/package-target.py --target "$target" --output dist
```

收集六个目标的归档及对应 `.binary-sha256` 文件到同一个干净 `dist` 后，在相同提交的仓库根目录运行下列命令。`OWNER/REPO` 必须替换为实际 GitHub 发布仓库的 owner 和 repository；也可不传 `--repository`，由实际环境的 `GITHUB_REPOSITORY` 提供。不要猜测仓库地址。

```sh
repository='OWNER/REPO'
python3 scripts/assemble-release.py --artifacts dist --repository "$repository"
```

单目标脚本读取 `target/<target>/release/datapier`（Windows 为 `datapier.exe`），运行该程序的 `--version` 并拒绝与根版本不一致的二进制，再生成 `datapier-<version>-<target>.tar.gz`（Windows 为 `.zip`）及该可执行文件的 `datapier-<version>-<target>.binary-sha256` sidecar，因此应在对应目标的原生机器运行。汇总脚本要求六个归档和六个有效 sidecar，生成 `SHA256SUMS` 与 `datapier-plugin-<version>.zip`。`SHA256SUMS` 仅包含六个二进制归档的摘要，不包含 plugin zip；sidecar 用于生成 plugin 内的二进制摘要清单。核对 plugin zip 的 `datapier/` 目录内含 plugin 源文件、`LICENSE`、`release-url.txt`、`release-sha256.txt` 和 `release-bin-sha256.txt`，下载地址指向实际发布仓库和版本；并核对归档及解压后二进制分别匹配相应清单。

发布安装无需 Rust：安装器校验归档及程序摘要后复用版本化缓存。`DATAPIER_RELEASE_URL` 仅覆盖已打包的下载地址，`DATAPIER_CACHE_DIR` 指定安装缓存目录；发布核对时应同时确认默认下载地址和缓存行为。

## 实际发布与发布后核对

完成最终提交的上述验收和组包后，在获得相应操作授权时，按 [tag 与目标提交](#tag-与目标提交)确定 tag，并人工创建对应 GitHub Release，上传六个二进制归档、`SHA256SUMS` 和 `datapier-plugin-<version>.zip`，附上人工整理的发布说明。Actions artifact 和本地 `dist` 本身不会成为 Release 下载资源。

发布后从实际 Release 地址核对所有资源的文件名、下载地址、版本与摘要，确认 plugin 内的地址、归档摘要和二进制摘要与已上传资源一致；在相应原生环境验证干净安装、缓存与 Tern 启动，并记录结果。发现不一致或验收缺项时如实记录并停止将其描述为完整六目标发布。
