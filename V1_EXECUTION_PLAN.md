# Comidea Codex Image Bridge v1.0 执行计划

## 目标

交付一个面向 Windows x64、可大面积分发的单文件 `CodexImageFix.exe`。程序负责：

- 配置 OpenAI-compatible 图片生成服务器与 `gpt-image-2`。
- 修复 Codex Desktop 已生成图片但聊天区不显示的问题。
- 提供可验证、可回滚、不会覆盖用户新修改的安装和配置管理能力。
- 支持不同用户名、盘符、Codex 安装目录、`CODEX_HOME` 和常见企业网络环境。

## 状态说明

- `[x]` 已完成并通过自动化验证。
- `[-]` 正在执行。
- `[ ]` 尚未开始。
- `[!]` 外部依赖或发布阻断项。

## 已有基线

- [x] 单文件 Windows GUI/CLI 双角色程序，无额外 SQLite 或 VC Runtime DLL。
- [x] 原生三页 UI、`comidea.org` 品牌、多尺寸程序图标和 DPI 自适应。
- [x] 动态发现官方 Codex CLI、用户级安装、版本化代理和 alias 集成。
- [x] 自定义服务器、API Key、`gpt-image-2` 开关、连接测试和安全恢复。
- [x] 支持现有 `custom` provider，并可迁移到受管理的 `comidea` provider。
- [x] 会话路径/thread ID 优先定位，只读 SQLite，目录扫描仅兜底。
- [x] JSONL 增量读取，同时支持 `image_generation_call` 与 `image_generation_end`。
- [x] 不依赖 `completed` 状态，严格验证 PNG/JPEG/WebP。
- [x] 图片临时写入、`fsync`、原子替换、SHA256 去重和 `saved_path` 哈希复用。
- [x] 不修改原 session，不记录 API Key、Base64 或提示词。

## 当前实施批次：v0.3 稳定部署完善

本批次以一次连续开发完成源代码、自动化测试、真实窗口验证和单文件构建。执行顺序固定如下；前一项未通过验收时，不进入依赖它的后一项。

### A. UI 快速切换与绘制稳定性

- [x] 导航消息与状态收敛
  - 按钮命令只处理 `BN_CLICKED`，忽略焦点和其他通知。
  - 重复点击当前页面不执行隐藏、显示或布局。
  - 连续切页请求合并，只应用消息队列中的最后一个目标页面。
- [x] 页面切换重绘
  - 页面切换不再调用全窗口 `layout`；布局只在初始化、DPI 或窗口尺寸变化时执行。
  - 批量隐藏旧页控件、显示新页控件，然后显式刷新父窗口和三个导航子窗口。
  - 父窗口绘制使用内存 DC 双缓冲，避免标题、页面和侧栏撕裂。
- [-] UI 压力验收
  - 自动连续切换三个页面至少 300 次，最终页面与最后一次请求一致。
  - 检查单项高亮、无旧页控件残留、无白屏、无明显闪烁和无句柄增长。
  - 在 100%/125%/150%/200% DPI 下保存真实窗口截图。
  - 当前 DPI 已完成 300 次真实窗口切换：最终仅诊断页可见、单一高亮，GDI 句柄 `27→27`、USER 句柄 `30→30`，截图为 `target/rapid-click-after.png`。
  - v0.3 在 125% DPI 再次完成 300 次真实窗口切换：GDI 句柄 `27→27`、USER 句柄 `35→35`，模型页和最终诊断页截图为 `target/model-page-v0.3-full.png`、`target/rapid-click-v0.3-full.png`。
  - 已实现 `WM_DPICHANGED`、字体和输入边距重建；其他 DPI 的真实截图仍需对应显示环境。

### B. 安装事务最终验收

- [x] 故障注入执行器
  - 覆盖代理复制、启动器复制、state 写入、alias 写入、注册表写入和启动器自检失败。
  - 覆盖全新安装与覆盖升级，验证失败后文件和注册表逐字节恢复。
- [x] 中断与恢复
  - 在 `Applying` 和 `Committed` 阶段模拟强制结束进程。
  - 下次执行 `status`、安装或卸载时自动恢复或完成提交。
  - 外部修改过的文件和注册表值保持不变，并返回可操作错误。

### C. 配置并发与秘密保护

- [x] 乐观并发控制
  - UI 加载时记录 `config.toml`、`auth.json` 和受管理状态的 SHA256。
  - 保存前重新校验，发现外部修改时拒绝覆盖并要求重新加载。
- [x] 多 `CODEX_HOME`
  - 状态按规范化 Codex Home 路径哈希隔离保存。
  - 不同 Home 可独立保存、恢复、卸载和重新管理。
- [x] Windows 秘密保护
  - 原始 `auth.json` 备份使用当前用户 DPAPI 加密，不再以 Base64 明文存入状态。
  - 状态目录 ACL 限制为当前用户、Administrators 和 SYSTEM。
  - API Key 和鉴权 Header 不进入日志、命令行、错误上下文或诊断包。
- [x] 配置变更预览
  - 保存前展示服务器、provider、模型和 feature 变更，不显示 Key 或 Header 值。

### D. 服务器与模型兼容

实现必须遵守 Codex 官方配置结构：自定义 provider 只写入 `[model_providers.comidea]`；`wire_api = "responses"`；Bearer Key 继续通过 `requires_openai_auth = true` 与 `auth.json` 提供；附加 Header 只使用官方支持的 `http_headers` 和 `env_http_headers`。顶层 `model` 是 Codex 文本主模型，不得用于保存图片模型 ID。图片模型 ID 由本工具独立管理，仅用于图片能力检测和生图链路，不得改变用户当前文本模型。

- [x] 地址与模型
  - 规范化根地址和 `/v1`，拒绝重复版本路径。
  - 模型 ID 可编辑，默认 `gpt-image-2`；保存服务器不强制覆盖当前文本模型。
- [x] 分层连接诊断
  - 区分 URL、DNS、系统代理、TLS、鉴权、Models API、Responses API 和模型可用性错误。
  - `/models` 不可用但 Responses API 可用时不误报服务器完全不可用。
- [x] 自定义 Header
  - 支持自定义鉴权 Header 和附加静态 Header。
  - 敏感 Header 与 API Key 使用同等存储、显示和脱敏规则。
  - 环境变量 Header 只保存 Header 名与环境变量名；不把环境变量实际值复制进配置、日志或诊断包。
  - 不写入 Codex 官方配置参考中不存在的字段。

### E. 部署自动化与诊断

自动化接口采用稳定机器协议：成功为退出码 `0`；参数错误为 `2`；未安装为 `10`；安装损坏为 `11`；配置冲突为 `20`；网络或服务诊断失败为 `30`；其他运行错误为 `1`。`status --json` 的字段使用 camelCase，后续版本只新增字段，不改名或改变既有字段语义。

- [x] 自动化命令
  - 提供 `install`、`repair`、`uninstall --silent` 和 `status --json`。
  - 定义稳定退出码，PowerShell/cmd 可等待并读取标准输出和标准错误。
  - 部署秘密仅从标准输入或 DPAPI 包读取，不接受命令行明文。
- [x] GUI 运维体验
  - 增加未保存修改提示、阶段进度、中文错误建议和 Codex 进程检测。
  - 安装、保存或恢复完成后提供启动或重启 Codex 的明确操作。
- [x] 脱敏诊断包
  - 包含程序版本、系统版本、规范化路径、文件哈希、退出码和健康状态。
  - 不包含 API Key、Header 值、Base64、提示词或 session 正文。

### F. Windows 发布与批次验收

发布构建必须由同一个 `build.rs` 嵌入图标、VERSIONINFO 和应用 manifest，确保最终仍只有一个 EXE。每次发布在 EXE 同目录生成 SHA256、CycloneDX SBOM、第三方依赖清单和构建环境记录；这些文件用于发布归档，不是终端电脑运行依赖。

- [x] 发布资源
  - 嵌入 VERSIONINFO 和应用 manifest，声明 `asInvoker`、Per-Monitor DPI、Windows 10/11、UTF-8 和长路径。
  - 输出 SHA256、SBOM、第三方依赖清单和构建环境记录。
- [x] 单文件验收
  - 通过格式、严格 Clippy、全量测试和 Release 构建。
  - 本机完成全新安装、覆盖升级、修复、卸载、恢复和重新安装。
  - 确认最终目录只需分发一个 EXE，不依赖额外 DLL 或运行时。
  - v0.3 已通过 44 项测试、严格 Clippy 和 Release 构建；`dumpbin /DEPENDENTS` 仅列出 Windows 系统 DLL。
  - 本机覆盖修复、运行中锁定文件卸载、`10/notInstalled`、全新安装和 `0/healthy` 闭环通过，模型配置与认证文件 SHA256 前后不变。
  - `dist` 已生成 4,178,944 字节单文件 EXE，SHA256 为 `48acca21267cd2ca3e299341eb49c233e2971e73f62dbd3e577f7d33f6035fec`；同时生成 CycloneDX 1.5 SBOM（104 组件）、依赖清单和构建环境记录。
- [!] 外部发布验收
  - Authenticode 需要组织代码签名证书和可信时间戳服务。
  - Windows 10/11、多语言、非 ASCII 用户名、企业代理和多台电脑矩阵需要外部测试环境。

### 本批次完成定义

- 所有源代码范围项目必须标记为 `[x]` 并附有自动化测试或真实窗口证据。
- 未通过验证的项目保持 `[-]` 或 `[ ]`，不得因已编译而提前标记完成。
- 外部证书和多机矩阵保持 `[!]`，不阻塞生成内部验收版，但阻塞最终公开发布。

## 阶段 1：安装与恢复安全

- [x] 安装事务日志
  - 在修改 state、alias、注册表或安装文件前持久化事务快照。
  - 记录事务版本、阶段、目标路径、旧 state、旧 alias、旧注册表和新建文件。
  - 事务日志使用临时文件、`fsync` 和原子替换。
- [x] 启动时恢复中断事务
  - 安装、卸载、状态检查前检测未完成事务。
  - 断电或进程终止后恢复到操作前状态。
  - 恢复失败时停止后续写入并显示明确错误。
- [x] 升级失败完整回滚
  - 恢复上一个已安装版本，而不是恢复到首次安装前状态。
  - 删除本次新建且未提交的代理和启动器。
  - 故障注入测试覆盖 state、alias、注册表和自检失败。
- [x] 卸载与模型配置解耦
  - 模型配置冲突不能阻止代理卸载。
  - 返回“代理已卸载、模型配置保留”的部分成功结果。
- [x] 跨进程单实例锁
  - GUI、安装、保存、恢复和卸载共享命名互斥锁。
  - 第二个进程只读状态或提示已有操作正在运行。

### 阶段 1 验收

- 升级任意一步失败后，旧代理仍可执行 `codex-cli --version`。
- state、alias 和 `CODEX_CLI_PATH` 与升级前逐字节一致。
- 强制终止安装进程后，下次启动能够自动恢复。
- 配置文件有用户新修改时仍可卸载图片代理。

## 阶段 2：配置并发与秘密保护

- [x] 保存前乐观并发校验
  - 写入前重新校验 `config.toml` 与 `auth.json` 的 SHA256。
  - 读取后发生外部修改时拒绝覆盖并要求重新加载。
- [x] 多 `CODEX_HOME` 状态
  - 备份按规范化路径和路径哈希分别保存。
  - 支持切换、恢复和移除多个 Codex Home 的受管理配置。
- [x] DPAPI 加密备份
  - 原始 `auth.json` 快照使用当前用户 DPAPI 加密。
  - 状态文件 ACL 只允许当前用户和系统访问。
- [x] API Key 生命周期
  - 不进入命令行、日志、诊断包和错误上下文。
  - 操作完成后清理不再需要的内存副本。
- [x] 配置预览
  - 保存前展示 provider、模型和 feature 的字段级变更，不显示 Key。

### 阶段 2 验收

- 并发修改测试证明不会丢失用户更新。
- 状态文件中不存在可直接解码的 API Key 或原始 `auth.json`。
- 两个不同 `CODEX_HOME` 可独立保存和恢复。

## 阶段 3：服务器与模型兼容

- [x] 分层连接测试：DNS/TLS、鉴权、Models API、Responses API、模型存在性。
- [x] `/v1` 路径归一化，避免重复或缺失版本路径。
- [x] 支持可编辑模型 ID，默认 `gpt-image-2`。
- [x] 保存服务器与切换当前模型分离，避免覆盖用户文本模型。
- [x] 支持自定义鉴权 Header 和附加静态 Header，敏感值按 Key 处理。
- [x] 使用 WinHTTP 系统自动代理并区分代理/TLS 错误；真实企业代理矩阵仍列入阶段 6。
- [ ] 可选真实生图验收，执行前提示费用与输出位置。

### 阶段 3 验收

- `/models` 缺失但 Responses API 可用时不会误报服务器完全不可用。
- 远程 HTTP 地址继续被拒绝，本地回环 HTTP 可用。
- 自定义模型 ID 和 Header 不出现在日志或诊断包中。

## 阶段 4：UI、诊断与自动化

- [x] 未保存修改提示、操作阶段状态、忙碌期禁止关闭边界和中文错误建议。
- [x] Codex/Codex++/托盘进程检测，完成后提供启动 Codex 按钮。
- [x] 一键生成脱敏诊断包，包含版本、路径、哈希、退出码和健康状态。
- [ ] 键盘导航、焦点、工具提示、屏幕阅读器和高对比度验证。
- [x] CLI 在 Windows GUI 子系统下可向重定向管道输出文本和错误；PowerShell 使用同步 `.NET Process` 调用。
- [x] `install/repair/uninstall --silent` 与 `status --json` 稳定退出码。
- [x] 自动化命令不接受任何命令行明文 Key；当前模型秘密仅允许 GUI/DPAPI 管理链路。

### 阶段 4 验收

- 诊断包不包含 API Key、Base64、提示词或 session 正文。
- 自动化命令在 PowerShell/cmd 中可等待、可读取输出和退出码。
- 100%/125%/150%/200% DPI 下无重叠、截断和不可操作控件。

## 阶段 5：Windows 发布工程

- [x] 静态 CRT、bundled SQLite、Windows GUI subsystem 和嵌入图标。
- [x] VERSIONINFO：CompanyName、ProductName、FileVersion、ProductVersion。
- [x] 应用 manifest：`asInvoker`、DPI、Windows 10/11、长路径和 UTF-8。
- [!] 组织 Authenticode 证书和可信时间戳服务。
- [x] 发布 SHA256、SBOM、第三方依赖清单和可复现构建记录。
- [!] 签名后验证安装副本签名、资源和依赖未变化；等待组织证书后执行。

## 阶段 6：发布验收矩阵

- [ ] Windows 10/11，普通用户/管理员用户。
- [ ] 中文/英文系统，非 ASCII 用户名，不同盘符和长路径。
- [-] 全新安装、重复安装、覆盖升级、失败回滚、卸载和重装；自动化和本机闭环已通过，待多机矩阵。
- [-] 默认/自定义/多个 `CODEX_HOME`；自动化隔离测试已通过，待多用户真实环境。
- [ ] Codex Desktop、Codex++ 和多个官方 CLI 版本。
- [ ] 断网、超时、错误 Key、TLS 错误、企业代理和服务器异常响应。
- [-] 损坏配置、文件占用、强制结束、断电恢复和并发操作；故障注入与本机锁定文件卸载已通过，待多机矩阵。
- [-] 大型 JSONL、多图、历史任务、PNG/JPEG/WebP 和重复图片；单元/集成测试已覆盖，待真实多任务矩阵。

## 每次提交的质量门槛

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

涉及安装、配置、协议或 UI 的修改还必须完成对应的故障注入、哈希校验或真实窗口截图；未满足验收条件的项目不得标记为完成。

## GitHub 发布准备

本阶段只准备可审查、可持续构建的源码仓库，不自动创建远程仓库、不提交、不推送。`target` 和 `dist` 不进入 Git 历史；可执行文件及其校验、SBOM 和构建记录通过 GitHub Release 分发。

- [x] 公开名称确定为 **Comidea Codex Image Bridge**，GitHub 仓库名为 `codex-image-bridge`。
  - 用户可见的窗口标题、文档、版本资源和 Release 名称统一使用公开名称。
  - `CodexImageFix.exe`、Cargo 包名、安装目录、配置标识和互斥锁保持不变，避免破坏已安装电脑的升级与卸载兼容性。

- [x] 仓库与敏感信息审计
  - 确认仓库尚无提交和远程地址，源码中不存在真实 API Key、用户绝对路径或本机配置副本。
  - 仅扫描项目目录，不读取或输出用户 Codex 配置中的秘密。
- [x] 仓库卫生与项目元数据
  - 完善 `.gitignore` 和 `.gitattributes`，排除构建、发布、日志、诊断与 IDE 本机文件。
  - 在 `Cargo.toml` 中声明用途、主页、最低 Rust 版本和禁止发布到 crates.io；远程地址和许可证未知时不虚构。
- [x] 持续集成
  - Windows CI 使用锁文件依次执行格式检查、严格 Clippy、测试和 Release 构建。
  - CI 不访问用户配置、不需要 API Key、不上传未审查的可执行文件。
- [x] Tag 发布
  - 仅响应 `v*` Tag，并严格校验 Tag 与 `Cargo.toml` 版本一致。
  - 调用统一的 `release.ps1` 生成 EXE、SHA256、CycloneDX SBOM、依赖清单和构建信息。
  - 在组织代码签名和人工验收完成前只创建 GitHub 草稿 Release，避免自动公开未签名程序。
- [x] GitHub 协作与安全说明
  - README 说明源码构建、Release 下载、Tag 规则、未签名风险和批量部署前验收要求。
  - 提供无虚构联系方式的安全报告说明，以及基础 Issue/PR 模板和依赖更新配置。
- [x] 干净源码发布验收
  - 全部门禁命令使用 `--locked` 通过。
  - `git add --dry-run .` 不包含 `target`、`dist`、日志、诊断包、本机路径或秘密。
  - 工作流和 PowerShell 脚本可解析，发布产物名称与 README 一致。
  - 2026-07-26 使用独立目标目录完成严格 Clippy、44 项测试、Release 构建与五项归档产物校验；`git add --dry-run .` 仅列出 26 个源码和仓库文件，索引保持为空。
  - 首次 GitHub Windows CI 发现卸载测试错误依赖 Runner 的系统 Authenticode 类型；测试改为注入签名验证器，生产路径仍强制校验 Microsoft 签名。
  - `release.ps1` 已显式传播所有 Cargo/Rust 原生命令的非零退出码，防止格式、测试或构建失败后继续生成发布产物。
- [!] 发布前人工决策
  - 确认 GitHub owner、仓库名、可见性和远程 URL 后再填写 `repository` 并执行首次提交与推送。
  - 明确开源许可证；未确认前不添加 LICENSE，也不对外宣称开源授权。
