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

## v0.4：入口自恢复与运行状态守护

本批次修复“安装文件仍在，但用户级 `CODEX_CLI_PATH` 丢失后，新 Codex 会话绕过图片代理”的真实故障。协议代理必须继续由 Codex 为每个 `app-server` 会话启动并持有该会话的 stdin/stdout 管道；独立守护器不代替协议代理，也不尝试接管已经绕过代理启动的 Codex 进程。

### A. 运行状态记录

- [x] 协议代理在 `%LOCALAPPDATA%\CodexImageDisplayFix\runtime` 写入按 PID 隔离的运行记录。
  - 仅记录 PID、父进程 PID、程序版本、启动时间和最近请求时间。
  - 不记录 thread ID、session 路径、提示词、响应正文、Base64、API Key 或 Header。
  - 使用临时文件、`fsync` 和原子替换；代理退出时删除自身记录。
- [x] 守护器读取运行记录后必须再次验证 PID、进程创建时间和进程映像，失效记录自动清理，不能仅凭残留 JSON 判断代理在线。

### B. 系统托盘守护器

- [x] 单文件 EXE 增加隐藏命令 `guardian`，通过命名互斥锁保证每个 Windows 用户仅一个实例。
- [x] 使用原生 Win32 隐藏消息窗口与 `Shell_NotifyIconW`，不显示命令行黑框。
- [x] 状态机固定为：
  - 绿色：Codex 正在运行且至少一个有效协议代理已连接。
  - 灰色：Codex 未运行，安装入口完整并可供下次启动使用。
  - 黄色：入口已自动修复，当前 Codex 必须重启后才能重新接入代理。
  - 红色：安装损坏、入口被外部占用，或 Codex 正在运行但没有有效代理连接。
- [x] 托盘菜单提供“当前状态”“打开控制面板”“立即修复”“启动 Codex”“退出状态监控”；双击图标打开现有控制面板。
- [x] 状态变化时更新图标和 Tooltip；只在异常或完成自动修复时显示系统通知，避免周期性打扰。

### C. 安装、自启动与受限自恢复

- [x] 安装事务增加 `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 守护器命令的原值备份、写入、故障回滚和卸载恢复。
- [x] 安装完成后启动当前版本守护器；覆盖升级时自启动命令自动切换到新的版本化代理路径。
- [x] 卸载只在 Run 值仍属于本工具时恢复原值；用户或外部软件改过的值保持不变并返回明确警告。
- [x] 守护器只允许以下无歧义自恢复：state、launcher、alias、proxy 及其 SHA256 全部健康，且 `CODEX_CLI_PATH` 当前不存在时，恢复为 state 中记录的 launcher 并广播环境变化。
- [x] `CODEX_CLI_PATH` 非空但指向其他程序时禁止覆盖，只显示红色故障和人工修复入口。
- [x] 自动恢复后写入“需要重启 Codex”运行标记；守护器不能声称已经修复当前未经过代理的会话。
- [x] 旧 v0.3 `state.json` 缺少守护器字段时通过 `#[serde(default)]` 兼容读取并可原位升级。

### D. 控制面板与自动化状态

- [x] `status --json` 只新增 camelCase 字段：`guardianInstalled`、`guardianRunning`、`codexRunning`、`proxyRunning`、`restartRequired` 和 `runtimeState`。
- [x] 控制面板安装页增加“运行状态/托盘守护”信息，区分文件完整、入口完整、代理在线和需要重启，避免把“文件存在”误报为“图片功能正在工作”。
- [x] “立即修复”完成后，如果 Codex 已运行，明确提示先完全退出再重新启动 Codex；不强制结束当前 Codex 进程。

### v0.4 验收

- 模拟 `CODEX_CLI_PATH` 缺失，守护器只在其余安装证据和哈希全部健康时自动恢复；`status --json` 进入黄色 `restartRequired` 状态。
- 模拟 `CODEX_CLI_PATH` 指向外部程序，守护器和安装状态检查都不得自动覆盖。
- Run 注册表写入、升级、故障注入回滚、外部修改保护和卸载恢复均有自动化测试。
- 守护器单实例、运行记录脱敏、过期记录清理和四态转换均有自动化测试。
- 严格 Clippy、全部测试和 Release 构建通过；最终仍只分发一个 `CodexImageFix.exe`。
- 本机覆盖安装后托盘可见，`status --json` 为 healthy/ready；用户手动完全退出并重启 Codex 后代理变绿，新生成图片可直接进入聊天区。
- 关闭控制面板不影响守护器和当前协议代理；退出托盘守护器不终止当前会话代理，下次登录可由 Run 项恢复守护器。

### v0.4 本机验证记录

- [x] 54 项测试、严格 Clippy、格式检查和 Release 构建通过。
- [x] v0.3 原位覆盖安装成功，部署进程 6.4 秒正常退出；守护器使用 `CreateProcessW(bInheritHandles = FALSE)`，不再占用批量部署脚本的 stdout/stderr 管道。
- [x] `status --json` 为 `health=healthy`、`guardianInstalled=true`、`guardianRunning=true`、`restartRequired=true`、`runtimeState=restartRequired`。
- [x] 用户级 Run 项指向当前 SHA256 版本化代理；`config.toml` 与 `auth.json` 的安装前后 SHA256 完全不变。
- [x] 现场删除用户级 `CODEX_CLI_PATH` 后，守护器 12 秒内自动恢复正确 launcher，未调用回退安装。
- [x] 实际窗口验证黄色“请重启 Codex”、环境入口正常、会话代理异常和托盘守护正常，控件无重叠或截断。
- [-] 当前任务不能强制结束自身所在的 Codex；完全退出并重启 Codex 后的绿色连接和新生图验收由用户执行。

## v0.4.1：Codex 重启后守护器自动恢复

现场复测确认用户操作步骤正确：完全退出 Codex 时，旧守护器随 Codex 一并终止；重新打开 Codex 后协议代理已运行，但守护器没有运行，也没有新的 guardian 运行记录。根因是 `CreateProcessW(bInheritHandles = FALSE)` 只阻止继承协议管道，不能让子进程脱离 Codex 所属的 Windows Job；HKCU Run 只在 Windows 用户登录时执行，不能覆盖“关闭并重新打开 Codex”的场景。

### 修复项

- [x] 守护器启动增加 `CREATE_BREAKAWAY_FROM_JOB | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW`，继续保持 `bInheritHandles = FALSE`。
- [x] Job 策略拒绝 breakaway 时，通过 Windows Shell 执行隐藏启动兜底；两种启动方式都不接管协议 stdin/stdout。
- [x] 每次协议代理进入 `app-server` 模式时，使用当前版本化代理 EXE 非阻塞补拉守护器；启动失败不得中断或污染 JSONL 协议。
- [x] guardian 命名互斥锁继续保证最终只有一个常驻实例；并发补拉出的多余进程立即正常退出。
- [x] 自动修复范围扩展到缺失的 `CODEX_CLI_PATH` 和本程序 Run 项，修改前严格校验安装路径、签名和 SHA256。
- [x] 非空外部 `CODEX_CLI_PATH` 或同名 Run 值仍禁止自动覆盖；双入口修复任一步失败时恢复修改前的原始注册表值。
- [x] 版本提升为 `0.4.1`，仍只分发一个 `CodexImageFix.exe`。

### v0.4.1 验收

- [x] 58 项测试、严格 Clippy、格式检查和 Release 构建通过。
- [x] 覆盖安装后 `CODEX_CLI_PATH` 与 Run 项均指向当前 SHA256 版本化代理，guardian `0.4.1` 运行记录有效；当前未重启的 Codex 仍由旧版 proxy 服务，符合版本切换预期。
- [x] 人工结束 guardian 后，短生命周期 `app-server` 已自动补拉新 guardian；父代理退出后 guardian 继续独立存活。
- [x] 同时删除两个本程序入口后，guardian 在 10 秒内自动恢复；`config.toml` 与 `auth.json` 的 SHA256 未变化，原 session 未修改。
- [-] 当前任务不能强制结束自身所在的 Codex；用户再次完全退出并重启 Codex 后的托盘绿色和新生图验收仍需现场确认。

## v0.4.2：状态、历史图片、模型目录与 UI 收口

现场复测新增五项问题：多次重启后仍提示重启；图片模型开关点击后视觉无变化；控制面板可重复多开；代理在线时历史图片仍为空；`gpt-image-2` 模型选项依赖 Codex++。本批次一次性修复，不引入第二个分发文件。

### 根因与修复

- [x] 重启标记原先绑定安装时枚举到的 ChatGPT/Codex 后台 PID，其中部分宿主进程跨窗口重启长期存活。改为以“修复时间之后出现有效 proxy 注册”为重启完成证据。
- [x] 图片模型开关状态实际已修改，但主窗口使用 `WS_CLIPCHILDREN`，仅重绘父窗口不会触发 owner-draw 子控件刷新。切换后直接失效并同步重绘开关控件。
- [x] 控制面板增加当前用户命名互斥锁；重复启动时查找、恢复并前置已有窗口，初始化竞态期间等待窗口就绪，不创建第二实例。
- [x] 控制面板原先只在启动或手动刷新时读取运行报告，Codex 重启后即使新 proxy 已清除重启标记，窗口仍显示旧黄色缓存。增加 3 秒运行状态定时刷新，合并重复 tick，且不重新加载或覆盖模型服务输入和未保存修改。
- [x] 历史回填发现同 ID 的官方空 `imageGeneration` 占位项时，不再直接跳过；先严格保存/校验图片，再原位替换为 `result=""` 与绝对 `savedPath` 的完成项。
- [x] proxy 拦截 `model/list` 成功响应；仅在图片功能启用时，按当前配置的图片模型 ID 克隆兼容协议结构并注入一次，已有模型不重复添加。
- [x] `model/list` 注入不修改 `models_cache.json`、顶层文本 `model` 或原 session，不要求 Codex++ 安装或常驻。
- [x] 版本提升为 `0.4.2`，仍只分发一个 `CodexImageFix.exe`。

### v0.4.2 验收

- [x] 单元测试覆盖重启确认时间、空历史占位替换、模型目录注入去重、控制面板互斥锁和运行刷新合并门控。
- [x] 同一时刻真实官方 app-server `model/list` 只返回 5 个文本模型；经过本 proxy 后返回 6 个模型并新增 `gpt-image-2 / GPT Image 2`，证明新增项来自本程序而非 Codex++。
- [x] 历史空占位回归测试确认同 ID 项被原位替换，`result` 为空且 `savedPath` 指向严格校验后的现有 PNG；现场指定旧 thread 被官方 CLI 以 `no rollout found` 拒绝，需在 Desktop 实际打开该会话后复核界面。
- [x] 连续启动两次控制面板只保留一个无参数 GUI 进程；150% DPI 完整截图确认开关每次点击立即改变轨道、滑块位置和状态文字，第二次点击恢复原状态。
- [x] 63 项测试、严格 Clippy、格式检查和 Release 构建通过；覆盖安装后 guardian 为 `0.4.2`，最终单文件 SHA256 为 `9c49bd7290eae499cf386709507ebfdadcff1c68a34bf4f6080f854df62dac83`。
- [x] 最终控制面板重复启动仍仅保留一个进程；本机配置确认 `gpt-image-2`、图片生成开关和 `comidea` provider 已启用，API Key 文件哈希与受管状态一致，验证过程未输出 API Key、Header 或配置正文。
- [x] 完全退出并重启后当前 Codex 已连接 `0.4.2` proxy，guardian 与 proxy 运行记录均为 `0.4.2`，重启标记自动清除并转绿。

## v0.4.3：官方模型缓存与历史通知重放

`0.4.2` 现场复测确认代理已真实连接，但 `gpt-image-2` 仍未出现在 Desktop 下拉框，旧会话图片也未渲染，新生成图片正常。根因不是安装或图片恢复失败，而是两个 Desktop 宿主层行为未被覆盖。

### 根因与修复

- [x] `model/list` 代理响应实测已包含 `gpt-image-2`，但 Desktop 的宿主与 React 状态仍按官方模型缓存及动态白名单过滤；Codex++ 通过 WebView 脚本同时补丁 `model/list`、宿主结果和 React 状态，因此仅响应注入无法等价替代。
- [x] 隔离 `CODEX_HOME` 实验证明：在官方 `models_cache.json` 插入完整 snake_case 可见模型条目并更新 `fetched_at` 后，官方 app-server 默认 `model/list` 原生返回 `gpt-image-2`；保留过期 `fetched_at` 时官方只返回文本模型。
- [x] 代理在启动官方 app-server 前同步模型缓存，保留 `etag`、`client_version`、所有官方模型和未知字段；条目使用专用 `comp_hash` 标记，写入继续使用临时文件、`fsync` 和原子替换。
- [x] 关闭图片功能或卸载时仅删除带本程序标记的模型条目，将 `fetched_at` 置为过期以触发官方刷新；不修改顶层文本 `model`，不保留伪造的新鲜度。
- [x] 真实旧 thread 的 session 解析出 11 张严格有效图片，`verify-chat` 也确认 `thread/read` 中 11 个 `imageGeneration` 均为 `result=""` 且 `savedPath` 存在，证明图片恢复本身正确。
- [x] Desktop 只即时渲染实时 item reducer 收到的图片通知；历史响应写出后新增去重的 `item/started` 与 `item/completed` 重放，格式与新图链路一致，同时把历史图片放在最后一条 agentMessage 之后。

### v0.4.3 验收

- [x] 单元测试覆盖模型缓存精确增删、官方字段保留、UTC 分钟级新鲜度、`includeHidden` 请求改写、响应对象兼容字段、历史通知重放和重复抑制。
- [x] 隔离缓存真实 app-server 验证默认模型列表返回 9 项并包含 `gpt-image-2`；真实旧 thread 响应后输出 11 条 `item/started`、11 条 `item/completed`，图片 ID 11 个且全部唯一。
- [x] 66 项测试、格式检查、严格 Clippy、Release 脚本和四处发布哈希一致性全部通过；`0.4.3` 单文件 SHA256 为 `b29712063f31f21456e51cbfed0bc4a46a52f2583d418b81b0996189e97bccfa`。
- [x] 本机覆盖安装后 guardian 运行记录为 `0.4.3`；`config.toml` 与 `auth.json` 的安装前后 SHA256 完全一致，模型缓存只有 1 条可见 `gpt-image-2` 且 `comp_hash` 为本程序专用标记。
- [-] 当前任务所在 Codex 仍由安装前代理承载，状态正确显示 `restartRequired`；由用户完全退出并重启后现场确认模型下拉框和旧会话图片。

## v0.4.4：分页历史、模型规范化与入口自修复

`0.4.3` 现场复测确认新生成图片正常，但模型下拉框和旧会话图片仍不可见。对当前 Codex Desktop `26.721.4979` 的只读前端分析与官方 app-server Schema 验证定位到三个缺口。

### 根因与修复

- [x] Desktop 在 `thread/resume` 后使用 `initialTurnsPage`、`thread/turns/list` 和 `thread/items/list` 分页水合历史；旧实现只修改 `thread/read/resume` 中的 `thread.turns`，注入会被分页结果覆盖。代理现同时处理三种分页结构，并继续使用同一套严格图片校验、原子落盘和 SHA256 去重。
- [x] `thread/items/list` 只在首个游标页补入缺失图片，非首页面仅规范化已有图片项，避免重复插入；所有分页注入均保持原 session 不变。
- [x] 当前前端只要 `model/list` 条目满足协议且 `hidden=false` 就应展示。代理不再因已存在同 ID 条目而提前返回，而是把旧缓存或宿主返回的条目移到首位，并补齐有效的 `defaultReasoningEffort`、`supportedReasoningEfforts` 和可见字段。
- [x] 守护器改为每次 tick 先尝试恢复缺失的 `CODEX_CLI_PATH` 与 Run 项，再读取完整状态，避免状态读取瞬时失败时永久跳过入口自修复；非空外部值仍拒绝覆盖。
- [x] 版本提升为 `0.4.4`，继续只分发一个 `CodexImageFix.exe`。

### v0.4.4 验收

- [x] 官方 `0.146.0` app-server Schema 确认 `ThreadTurnsListResponse.data[].items` 和 `ThreadItemsListResponse.data[].item` 接受同一 `imageGeneration` 结构，Windows 绝对路径会被 Desktop 转换为可加载的本地图片源。
- [x] 单元测试覆盖初始分页、后续 turns 分页、items 首页面、非首页面抑制、已有隐藏模型规范化和守护器修复顺序。
- [x] 70 项测试、格式检查、严格 Clippy 和 Release 脚本全部通过；`0.4.4` 单文件 SHA256 为 `cead7175b5d02b70f8ab2e21546a3c0c0e58632ad38744dd2a5d5371fc685d4d`。
- [x] 使用原 session 的工作区隔离副本和独立无密钥 `CODEX_HOME` 完成真实官方 app-server 验证：`thread/resume` 与 `thread/turns/list` 均返回 11 个唯一 `imageGeneration`，11 个 `result` 为空且 11 个 `savedPath` 全部存在；原 session 未修改。
- [x] 本机覆盖安装后安装文件与发布文件 SHA256 一致，guardian 运行记录为 `0.4.4`；真实用户 HKCU 中 `CODEX_CLI_PATH` 和 `ComideaCodexImageBridge` Run 项均指向新版本入口。
- [-] 当前任务所在 Codex 不能被强制关闭；用户完成本任务后完全退出并重启 Codex，再现场确认模型下拉框和旧会话图片。

## v0.4.5：动态白名单与高级模型选择器兼容

`0.4.4` 现场复测确认历史图片已经恢复，但 `gpt-image-2` 仍未出现在模型下拉框。代理、官方模型缓存和 `model/list` 响应都已包含该模型，因此本批次继续定位 Desktop 前端自身的最终过滤条件。

### 根因与修复

- [x] 当前 Desktop 的 Statsig 配置 `107580212` 启用了 `use_hidden_models=true`，其 `available_models` 白名单不包含 `gpt-image-2`；前端会在读取协议模型后再次按该白名单过滤，因此仅设置 `hidden=false` 无法展示图片模型。
- [x] 同一白名单仍保留已退役且官方 app-server 不再返回的 `gpt-5.3-codex`。代理继续注入真实图片模型，并额外提供显示名为 `GPT Image 2` 的隐藏兼容别名，使高级模型选择器能够穿过前端白名单。
- [x] 代理只对 `thread/start` 与 `turn/start` 改写该兼容别名，同时覆盖 `collaborationMode.settings.model`；发送给真实 CLI 和图片服务器的模型始终是 UI 中配置的真实图片模型，其他请求和模型不受影响。
- [x] Desktop 简单模型选择器只支持少量硬编码文本模型。本工具以结构化 JSON 管理 `.codex-global-state.json` 中的 `composer-model-picker-menu-view-v1`，启用图片功能时切换为 `advanced`，保存原偏好，并仅在值仍属于本工具时恢复。
- [x] 全局状态写入继续使用大小限制、临时文件、`fsync` 和原子替换；安装、保存、停用、恢复和卸载失败时均回滚。守护器在 Codex 未运行时补做高级选择器偏好，避免 Desktop 退出时覆盖该设置。
- [x] 版本提升为 `0.4.5`，继续只分发一个 `CodexImageFix.exe`，不修改原 session、顶层文本模型或用户未受管配置。

### v0.4.5 验收

- [x] 单元测试覆盖真实模型与兼容别名注入去重、已有旧别名替换、`includeHidden` 请求改写、thread/turn/协作模式别名回写、模型选择器偏好保存与条件恢复。
- [x] 75 项测试、格式检查和严格 Clippy 通过。
- [x] Release 构建、发布哈希和本机覆盖安装通过；`0.4.5` 单文件为 4,458,496 字节，SHA256 为 `36664a4725f933ab8e7104299caa3d67b7842b055d6a54b1a2dd5f44fdcb4b09`，安装文件与发布文件哈希一致，新 guardian 运行记录为 `0.4.5`。
- [x] 新安装入口启动的真实官方 app-server 返回 10 个模型；其中真实 `gpt-image-2` 为 `hidden=false`，兼容入口 `gpt-5.3-codex` 为 `hidden=true`，两者显示名均为 `GPT Image 2` 且默认推理强度为 `low`。
- [-] 当前任务所在 Codex 不能被强制关闭；覆盖安装后由用户完全退出并重启 Codex，再现场确认 `GPT Image 2` 下拉项。

## v0.4.6：对齐 Codex++ 自定义模型元数据

`0.4.5` 现场复测确认图片显示正常，`GPT Image 2` 也已出现在下拉框，但选择后没有回复。失败会话的结构化记录显示代理最终已把兼容入口改写为 `gpt-image-2`，请求到达 `https://gptapi.comidea.org/v1/responses`，58 秒后返回 `502 Upstream request failed`；问题不在模型名是否送达，而在模型元数据与 Codex++ 路径不一致。

### 根因与修复

- [x] 只读对比本机 Codex++ `1.2.42` 的内嵌前端实现：Codex++ 直接把真实自定义模型加入 Statsig/React 白名单，模型描述只包含 `model/id/slug/name/displayName/description/hidden/isDefault/defaultReasoningEffort/supportedReasoningEfforts`，不会克隆官方文本模型缓存。
- [x] `0.4.3-0.4.5` 为了绕过 Desktop 缓存，曾从官方文本模型克隆完整 `models_cache.json` 条目并写入专用 `comp_hash`。失败 turn 的 `turn_context.comp_hash` 实际为 `comidea-codex-image-bridge:gpt-image-2`，证明这些伪造元数据进入了官方 CLI 的模型解析路径。
- [x] 停止向 `models_cache.json` 插入图片模型；升级后只精确删除带本程序标记的旧条目并把缓存置为待官方刷新，保留所有官方模型、`etag`、`client_version` 和未知字段。
- [x] `model/list` 改为 Codex++ 同款最小描述，不再克隆文本模型的基础指令、输入能力、服务层级、升级信息或 `compHash`；默认推理强度和可选档位与 Codex++ 的自定义模型回退描述保持一致。
- [x] 白名单兼容入口继续只负责 UI 可见性，真实请求仍在协议边界改写为用户配置的图片模型；不修改原 session、顶层文本模型、API Key 或 Header。
- [x] 版本提升为 `0.4.6`，继续只分发一个 `CodexImageFix.exe`。

### v0.4.6 验收

- [x] 单元测试、格式检查、严格 Clippy 和 Release 构建；74 项测试全部通过。
- [x] 本地模拟 Responses 服务确认兼容入口与直接自定义模型产生等价请求结构，且模型上下文不再含 Comidea `comp_hash`。
- [x] 使用真实受管 `CODEX_HOME`、临时 thread 和进程级 provider 覆盖完成无费用探针；`gpt-5.3-codex` 与直接 `gpt-image-2` 最终均发送 `model=gpt-image-2`，顶层字段、输入角色、工具类型和指令长度一致，模型目录 `compHash=null`，两条路径均未产生压缩通知。
- [x] 现场旧缓存确认只有 1 条 `comidea-codex-image-bridge:gpt-image-2` 条目；`0.4.6` 启动后精确移除该条目并把缓存时间置为待官方刷新，保留其余 7 条模型。该旧 `comp_hash` 会在切换图片模型时进入 `turn_context`，是新窗口发送极短消息也触发压缩的直接原因。
- [x] `0.4.6` Release 单文件为 4,447,232 字节，SHA256 为 `1779081a874e1d8158c8c7f2f3ee94351b8ce05b52566420a44bbcbf0252963d`，发布文件与声明哈希一致。
- [x] 本机覆盖安装完成，安装状态为 `health=healthy`，guardian 运行记录版本为 `0.4.6`；旧 Comidea 缓存条目为 0，当前 Codex 仍在运行，因此状态正确保持 `restartRequired=true`，不在本任务中强制关闭宿主进程。
- [-] 真实图片服务器调用可能产生费用；没有明确授权时不主动发起新的生图请求，最终在线出图由用户重启后现场确认。

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
