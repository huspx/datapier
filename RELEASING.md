# 发布指南

## 当前发布方式与边界

0.1.0 已于 2026-11-01 在 [GitHub Releases](https://github.com/huspx/datapier/releases/tag/v0.1.0) 正式公开，tag `v0.1.0` 对应提交 `fadaec502e4926f32c7d5aa0ef752f0c335a07a9`。正式分发入口为 [GitHub Releases](https://github.com/huspx/datapier/releases)。`.github/workflows/native-release.yml` 在目标为 `main` 的 pull request、`main` 的 push 和手动触发时执行格式检查与六目标原生构建、测试、smoke 和单目标打包。PR 与非 main 手动运行仅保留 runner 内临时文件；main push 或 main 手动运行将打包结果直接上传版本草稿 Release，汇总后补齐 plugin 包和校验清单。全部路径均不上传 Actions artifact，不占用其产物保留空间。普通工作分支和 tag 的 push 不自动触发；草稿不会自动发布，也不设为 latest。

[技术方案第 11 节](docs/technical-design-0.1.0.md#11-实现与运行记录)汇总既有 macOS arm64 运行证据和未验收范围；其余五个目标尚未完成完整原生验收。0.1.0 经维护者明确接受已披露的验收范围后公开，例外与限制保留在对应 Release 说明中；该次授权不代表其余平台已验收，也不自动放宽后续版本的发布要求。六目标产物、组包及候选版本运行结果均须按发布前检查核实。

本指南描述发布步骤，不代表已完成这些操作，也不构成提交、推送、合并、创建 tag 或发布的授权。工作区有文件不代表目标提交或分发仓库包含它们；正式发布前必须确保实际源码、Cargo 配置和锁文件、plugin、安装器、组包脚本及工作流都已进入目标提交，不能依赖未跟踪或被忽略的本地文件。

六目标共用 `.github/workflows/native-build.yml`：调用方为 PR 授予 `contents: read`，仅 main 的草稿准备、上传和汇总任务获得 `contents: write`。跨 job 通过同一个草稿 Release 的资源传递文件，不再使用 upload／download-artifact。main 工作流串行运行，草稿绑定完整提交 SHA、run ID 和 attempt；目标上传与汇总均复核身份及未发布状态。只刷新同版本草稿的生成附件，保留人工正文与其他附件；已发布的同版本 Release 或指向其他提交的已有 tag 均拒绝覆盖。正式发布仍由维护者验收并授权。

## 版本号与关联文件

根 `Cargo.toml` 的 `workspace.package.version` 是唯一人工维护的产品版本，当前开发版本为 `0.1.1`，不代表已正式发布。四个 crate 继承此版本，CLI 和 TSP 编译时使用对应 crate 版本；修改后通过 Cargo 更新 `Cargo.lock` 中本地包版本，不手工编辑锁文件。组包当前支持无前导零的三段数字版本，不支持预发布后缀。

修改根版本后，先同步源码 plugin，再更新 CHANGELOG 对应版本章节并运行只读发布检查。源码 plugin 保持可直接 `tern plugin link`，Cargo 构建不会修改它。PR 与 main 的 CI 均执行同一检查，草稿准备、上传和汇总入口也会在访问 GitHub 前检查；plugin 版本不一致、CHANGELOG 首个章节不匹配根版本、版本章节重复或当前章节缺少实际变更条目时失败。检查不会修改文件，也不查询或推断线上发布状态。

```sh
python3 scripts/version.py --sync
python3 scripts/version.py --check-release
```

`python3 scripts/version.py` 默认只输出根产品版本；`--sync` 仅同步源码 plugin，`--check-release` 只读检查 plugin 与 CHANGELOG。同步与检查参数互斥，读取和检查都不会隐式修改文件。

安装器从随插件提供的 `plugin.toml` 读取版本并校验程序版本。组包脚本和 CI 从根 manifest 读取版本，显式传入 `--version` 时必须与根版本一致；汇总时只同步临时插件副本，不改源码。Windows 用户安装器使用系统自带的 Windows PowerShell 5.1，须保留其 UTF-8 BOM。

产品规划和技术方案中作为历史记录的版本标题不需要机械改写。版本调整需与实际发布内容和验证记录一致。

### Python 3.15 工具环境

开发发布脚本使用 Python 3.15，仅依赖标准库。CI 通过 `actions/setup-python@v7` 配置 `python-version: '3.15'`，跟随该分支补丁版本，架构随原生 runner；短命令使用 Bash，架构检查和二进制 smoke 直接使用 Python。开发环境、格式化与回归命令见[贡献指南](CONTRIBUTING.md#开发环境)，工具契约见[0.1.1 技术方案](docs/technical-design-0.1.1.md#24-开发与发布工具)。

版本读取、源码 plugin 同步及只读发布检查统一由 `scripts/version.py` 提供；单目标组包和六目标汇总分别使用 `package-target.py` 和 `assemble-release.py`。`draft_release.py` 负责准备草稿、原生 job 直接上传及从草稿资源汇总，使用 Python 标准库调用 GitHub REST API。Windows 用户安装器继续使用系统自带 Windows PowerShell 5.1，Unix 安装器使用 `/bin/sh`；安装与运行 Datapier 无需 Python，开发与组包无需 `pwsh`。

脚本回归覆盖版本同步、只读检查不写文件、无效或重复版本及显式版本不一致拒绝、CHANGELOG 章节不匹配／重复／缺少实际条目拒绝，以及缺失目标归档、非法归档成员和草稿下载摘要不匹配拒绝、本版本生成附件清理、内嵌归档与程序摘要正确及汇总不改源码 plugin。实际组包须使用本机真实二进制，核对归档和程序摘要，并验证启动前拒绝内容变化的缓存；完整汇总必须收齐同一提交的六目标原生产物。脚本测试和本机组包不能代替六目标 CI 或完整原生运行验收。

## 发布说明与 CHANGELOG

根目录 [CHANGELOG.md](CHANGELOG.md) 由贡献者人工维护，记录变更内容；文档职责与分类口径见[贡献指南的文档职责与阅读顺序](CONTRIBUTING.md#文档职责与阅读顺序)。新功能尚未发布期间的内部修正不逐条记入“修复”。工作流只维护草稿中的构建身份和完成状态，保留人工说明，不自动生成正式发布说明或回写发布状态。版本章节存在、CI 成功、tag 或 Release 草稿存在，都不等于正式发布完成。

- 日常变更直接记入根产品版本对应的章节，例如 `## 0.1.1`；当前版本必须是首个二级章节，至少包含一条实际变更，不再使用独立的 Unreleased 暂存章节。版本号提升时新增下一版本章节，既有历史章节保留。
- 发布准备 PR 核对该版本内容，并同步版本号、安装器和组包配置。版本章节是该提交的内容快照，不加“待发布／已发布”标记，也不预填实际发布日期。
- 维护者在 GitHub 发布草稿时改变实际发布状态：`draft=true` 为草稿，`draft=false` 且 `prerelease=true` 为预发布，`draft=false` 且 `prerelease=false` 为正式公开；实际发布时间使用 `published_at`。CHANGELOG 不回写状态，资源完整性、摘要及安装验证仍须另行核对。
- 构建或上传失败时保留草稿和诊断，不声称组包完成。需要重跑时选择整个工作流，使六目标使用同一个新的 attempt；旧 attempt 或其他提交的任务不能向当前草稿续传。源码改变时重新完成构建和验证，不混用不同提交的资源，不移动已经公开的版本 tag。
- 发布成功后不再自动提交一次“改为已发布”的 CHANGELOG 更新，不改写已验证 tag；发布时间、正式下载验证和验收范围写入 GitHub Release 说明。开始下一轮开发时提升根产品版本并新增对应 CHANGELOG 章节；保留已发布的历史内容。

维护者结合变更记录、[0.1.1 技术方案运行记录](docs/technical-design-0.1.1.md#12-实现与验证记录)及该版本真实验收记录人工整理发布说明：列出功能与兼容性变化、支持范围、已知限制、各平台的实际 OS／glibc、Tern、数据库版本及验证结果，并说明未执行的检查。

## 发布前检查

按[开发规范中的测试与验证](CONTRIBUTING.md#测试与验证)完成适用检查，记录实际命令和结果。0.1.1 原生运行验收仅覆盖 macOS arm64；其他五个目标保留跨平台实现，在 PR 阶段通过现有六目标 CI 验证编译、默认测试和版本 smoke，不要求本版原生运行验收。下列真实数据库、TSP、TLS、凭据和安装运行检查适用于 macOS arm64。默认测试不包含默认忽略的真实数据库专项，构建结果与实际运行记录分别报告。

- 使用隔离的真实 MySQL 5.7／8.0／8.4 和 Redis 6.2／7.2／8.0 单节点，按实际版本验证连接、查询、编辑提交、导入导出、只读、并发冲突与取消；覆盖大整数、精确小数、NULL、二进制、普通空结果、多结果、写入断连不重放、Redis List 重复值、WATCH 与 TTL。数据库测试只创建并清理自身隔离资源，不使用生产数据或真实凭据作为样本。
- 在实际 Tern 中验证完整交互，包括中文／emoji、选区、分页、大字段、草稿恢复、两个 UI、十万行全量导出及百万 key 增量扫描；观察 IPC、客户端退出及 Engine 收尾的进程和资源状态。验证 TLS 成功连接及错误 CA／身份拒绝、系统凭据存取和凭据服务不可用时的行为，确保日志、配置、导出和运行描述不含秘密。
- 在 macOS arm64 验证干净安装、下载与 SHA-256 校验、缓存复用、启动、IPC 权限、Keychain 和退出恢复，记录系统、Tern、数据库及依赖版本。其他平台保留 Secret Service、Windows Named Pipe 当前用户权限及拒绝远程客户端、ConPTY／OSC 877、文件锁和 `.exe` 路径等实现，在 PR 阶段检查对应目标构建；这些平台的安装及交互不列入本版原生运行门禁。
- 核对 README 的安装、功能和平台描述与实际结果一致，保留 [MIT License](LICENSE) 及第三方分发要求。0.1.0 的 MySQL／Redis 单节点范围及不支持项目以 [README](README.md) 和[技术方案](docs/technical-design-0.1.0.md#1-目标与范围)为准，不以配置或单一平台结果扩大支持声明。

本版要求的运行验收和六目标 PR 构建结果应能关联具体提交、构建产物和环境；必需检查未完成时保留对应阻塞。详细运行场景见[技术方案第 8 节](docs/technical-design-0.1.0.md#8-跨平台与分发)和[第 9 节](docs/technical-design-0.1.0.md#9-验收要求)，实际平台范围以本节的 0.1.1 约定为准。正式 Release 下载地址需在实际上传后核对；发布前对候选包和隔离下载源的安装验证，不能写成正式下载地址已可用。

## PR 与合并

`main` 必须通过 PR 合并，CODEOWNERS、审批、Merge commit 和仅限 PR 的绕过要求见[贡献指南的分支与 PR](CONTRIBUTING.md#分支与-pr)。将版本同步、CHANGELOG 版本内容、发布说明及所需源码和配置纳入发布准备 PR，检查对应工作流结果；CODEOWNERS 的审批分配以 PR 目标分支已有定义为准。合并不代表发布完成；最终提交可能与候选提交不同，应在最终提交重跑或核对对应 workflow，并使用该提交的六目标产物完成必要的重新验收与组包。记录产物对应的提交和 workflow run，不把不同提交的文件混作一个版本。

## tag 与目标提交

草稿使用 `v<Version>` 名称并绑定当前完整提交 SHA；准备草稿时不单独推送 Git tag。若同名 tag 已存在，必须解析至本次提交，否则工作流拒绝继续。六目标产物、完整组包及运行验收完成并获得正式发布授权后，维护者核对该 tag 的目标提交再发布草稿。`v` 前缀来自组包脚本生成的下载地址 `https://github.com/<owner>/<repo>/releases/download/v<version>`，tag、Release、plugin 内下载地址和资源必须一致。

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

独立 `format` job 的职责与变更文件范围见[贡献指南的代码格式化](CONTRIBUTING.md#代码格式化)，并执行根版本、源码 plugin 和 CHANGELOG 的只读发布检查。每个 native job 检出代码、设置 stable Rust 和对应 target，Linux 安装构建依赖，核对 runner 原生架构，运行 workspace 默认测试，再构建 release 可执行文件、执行 `--version` smoke 并调用单目标组包脚本。PR 与非 main 手动运行到此结束，归档随 runner 清理，不上传 Actions artifact 或 Release 资源。

main 的 `prepare-release` 在格式检查成功后创建或刷新 `v<version>` 草稿，清理该版本的六个目标归档和 plugin 包，保留人工附加附件；六个 `native-release` job 各上传一个目标归档。所有目标成功后，`bundle` 从相同草稿鉴权下载六个归档，验证资源及 GitHub 提供的可用 SHA-256，生成内嵌归档与程序摘要的 `datapier-plugin-<version>.zip` 并上传，最后标记构建完成，Release 仍保持草稿。流程生成六个归档和一个 plugin 包，共七个附件；人工附加附件不计入此集合，失败时不能把部分资源视为完整交付。草稿资源需要授权访问，正式下载 URL 要到维护者发布后才公开可用。

官方 Actions 使用 Node.js 24 运行时的 `actions/checkout@v7` 与 `actions/setup-python@v7`；当前流程不再调用 `actions/upload-artifact` 或 `actions/download-artifact`。六目标仍使用 GitHub 托管的原生 runner。

也可在各目标原生机器上，从仓库根目录执行对应 release 构建和组包。以下示例只处理本机对应的单个 target；将 `target` 替换为上表中的实际目标，运行验收范围按[发布前检查](#发布前检查)执行。六个归档必须均来自同一最终提交，为每次发布使用干净的输出目录，避免混入旧 `dist` 或其他提交的文件。

```sh
target='aarch64-apple-darwin'
cargo build --release --package datapier --target "$target" --locked
python3 scripts/package-target.py --target "$target" --output dist
```

收集六个目标的归档到同一个干净 `dist` 后，在相同提交的仓库根目录运行下列命令。`OWNER/REPO` 必须替换为实际 GitHub 发布仓库的 owner 和 repository；也可不传 `--repository`，由实际环境的 `GITHUB_REPOSITORY` 提供。不要猜测仓库地址。

```sh
repository='OWNER/REPO'
python3 scripts/assemble-release.py --artifacts dist --repository "$repository"
```

单目标脚本读取 `target/<target>/release/datapier`（Windows 为 `datapier.exe`），运行该程序的 `--version` 并拒绝与根版本不一致的二进制，再仅生成 `datapier-<version>-<target>.tar.gz`（Windows 为 `.zip`），因此应在对应目标的原生机器运行。汇总脚本要求六个仅含固定程序成员的归档，计算归档摘要并流式读取原程序计算摘要，仅生成 `datapier-plugin-<version>.zip`。公开附件摘要使用 GitHub 自动生成的 digest，不生成外部 `SHA256SUMS` 或独立 `.binary-sha256` 附件。核对 plugin zip 的 `datapier/` 目录内含 plugin 源文件、`LICENSE`、`release-url.txt`、`release-sha256.txt` 和 `release-bin-sha256.txt`；下载地址指向实际发布仓库和版本，两个清单分别记录六个归档文件名和六个 target 对应的 SHA-256。

发布安装无需 Rust：安装器验证下载归档的 SHA-256 和唯一固定归档成员，解包后先核对程序 SHA-256，再执行版本检查并发布缓存；发布缓存按归档摘要隔离，每次启动前仍先核对程序 SHA-256，再执行 `--version`，摘要不匹配直接拒绝启动。`DATAPIER_BINARY` 本地开发模式按显式配置的源程序内容哈希缓存并校验副本。`DATAPIER_RELEASE_URL` 仅覆盖已打包的下载地址，`DATAPIER_CACHE_DIR` 指定安装缓存目录；发布核对时应同时确认默认下载地址和缓存行为。

## 实际发布与发布后核对

main 工作流完成后，维护者核对版本草稿绑定的提交、run、六目标归档与 plugin 包，完成所需验收并补充人工发布说明；获得正式发布授权后发布该草稿。无需从 Actions artifact 下载再手动上传，也不因草稿构建完成而自动公开 Release。若采用本地组包，则仍需人工创建草稿并上传相同文件，遵守同一提交和版本约束。

发布后从实际 Release 地址核对七个附件的文件名、下载地址、版本与 GitHub digest，确认 plugin 内的地址、归档摘要及程序摘要与实际资源一致；在 macOS arm64 验证干净安装、缓存完整性校验与 Tern 启动，并记录结果。其他目标的构建结果与未实测运行范围分别披露；发现本版要求的校验或验收缺项时如实记录，不将六目标构建描述为六目标完整原生运行验收。
