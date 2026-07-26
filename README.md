# Comidea Codex Image Bridge

本仓库现在包含两个彼此隔离的平台版本：

- **Windows x64 正式版**：Rust 单文件 `CodexImageFix.exe`，提供安装、更新、修复和卸载。
- **macOS 临时版**：Node.js 24 单文件 `codex-image-bridge.mjs`，不安装、不修改 Codex 配置，按次启动 Codex。

macOS 版源码、测试和完整用法位于 [`mac-bridge`](mac-bridge/README.md)。最终分发时，Mac 用户只需要其中的 `codex-image-bridge.mjs`；`test.mjs` 和 `package.json` 仅供开发与 CI 使用。

## macOS 临时版快速使用

1. 安装 Node.js 24，并确认 `node --version` 显示 `v24` 或更高版本。
2. 下载唯一运行文件 `codex-image-bridge.mjs`。
3. 在终端进入文件所在目录，运行 `node codex-image-bridge.mjs`。
4. 在自动打开的本地界面中填写服务器地址和 API Key，保持“启用图片生成”打开。
5. 点击“测试连接”，再点击“启动 Codex”。

工具会先请求退出旧 Codex，再通过桥接环境启动官方 Codex Desktop。启动成功后本工具和终端命令会自动结束，不需要保持后台运行。API Key 只存在于本次进程及其启动的 Codex 进程中，不写入 `config.toml` 或 `auth.json`；下次启动 Codex 时需要再次运行本工具。

macOS 版不会把 `gpt-image-2` 加入聊天模型下拉框。该模型是 Codex 图片工具的固定后端，“启用图片生成”开关负责开启它。用户仍需先在官方 Codex Desktop 中保持有效的 ChatGPT 登录状态。

以下内容为 Windows Rust 正式版说明。

Comidea Codex Image Bridge（`CodexImageFix.exe`）是一个由 `comidea.org` 标识的 Windows x64 单文件工具，用于配置 `gpt-image-2`，并解决 Codex Desktop 已成功生成图片、但聊天区没有显示图片的问题。

同一个 EXE 同时承担两个角色：

- 用户双击且不带参数启动时，显示原生 Windows 安装界面。
- Codex Desktop 启动它并传入 `app-server` 参数时，它作为官方 Codex CLI 的透明 JSONL 代理运行。
- 安装后由同一个 EXE 以隐藏 `guardian` 模式驻留系统托盘，监控入口和实际代理连接状态。

不需要额外的脚本、运行库、SQLite DLL 或第二个可执行文件。

## 普通用户使用

1. 把唯一分发文件 `CodexImageFix.exe` 放到目标电脑任意目录。
2. 完全退出 Codex Desktop；无需安装或启动 Codex++。
3. 双击 `CodexImageFix.exe`。
4. 点击“安装 / 更新”。
5. 打开“模型服务”，填写服务器地址、API Key 和图片模型 ID，点击“测试连接”。
6. 按需填写静态 Header 或环境 Header 的 JSON 对象，打开图片生成开关，点击“保存并启用”。
7. 安装和配置成功后重新打开 Codex Desktop。

安装界面可以直接关闭，也不需要保留命令行窗口。系统托盘状态监控独立于 Codex 运行并在下次登录时自动启动；如果被异常终止，每个新的 Codex `app-server` 会话也会主动补拉它。协议代理仍由每个 Codex 会话自动启动。托盘绿色表示当前 Codex 已连接代理，灰色表示 Codex 未运行但入口就绪，黄色表示需要重启 Codex，红色表示入口或连接异常。

控制面板为单实例程序。重复双击只会唤醒已有窗口，不会继续创建后台进程。窗口每 3 秒自动刷新代理、守护器和重启状态；该轻量刷新不会重新加载或覆盖服务器地址、API Key、模型名、开关及其他未保存输入。

托盘菜单可以打开控制面板、安全修复缺失入口、启动 Codex 或退出状态监控。退出状态监控不会终止当前会话的协议代理；下次登录或下次启动 Codex 时会重新启动状态监控。

需要更新时，用新版 `CodexImageFix.exe` 再点一次“安装 / 更新”。需要移除时，打开任意一份 `CodexImageFix.exe`，点击“卸载”。卸载会恢复安装前记录的用户环境变量、登录自启动项、命令别名和由本工具管理的模型配置，不删除 session 或已经保存的图片。

该程序执行用户级安装，通常不需要管理员权限。企业安全策略如果禁止写入用户级 WindowsApps alias，需要由 IT 管理员放行或统一部署。

## 模型服务配置

- 配置目录优先读取 `CODEX_HOME`；未设置时自动使用当前用户的 `%USERPROFILE%\.codex`，不依赖固定用户名、盘符或 Codex 安装目录。
- “保存并启用”会保留 `config.toml` 的其他 TOML 项和注释，并保留 `auth.json` 的其他 JSON 字段。
- 自定义 provider 保存为 `comidea` 并使用 Responses API；已有 `custom` 等 provider 会先被读取并回显。
- 图片模型 ID 默认是 `gpt-image-2`，可按服务器实际模型修改。本工具不会改写顶层 `model`，也不会把图片模型克隆写入官方 `models_cache.json`；旧版本留下的带 Comidea 标记缓存会被精确清理并触发官方刷新，避免新窗口在第一条短消息时因伪造 `comp_hash` 触发无意义的上下文压缩。图片功能启用后，代理按 Codex++ 的字段集合向 `model/list` 注入最小模型描述，不携带文本模型的基础指令、能力字段或伪造 `comp_hash`。针对 Desktop 动态白名单仍不包含图片模型的版本，代理还会提供一个仅用于下拉框兼容的 `gpt-5.3-codex` 入口，显示名称仍为 `GPT Image 2`；用户选择后，`thread/start`、`turn/start` 和协作模式中的入口名会在发送给官方 CLI 前改写为当前配置的真实图片模型。工具同时把模型选择器切换到高级视图，并在停用或卸载时安全恢复原偏好，因此不依赖 Codex++。
- 历史图片恢复同时覆盖 `thread/read`、`thread/resume`、`initialTurnsPage`、`thread/turns/list` 和 `thread/items/list`。这适配了 Codex Desktop 的分页历史加载，不修改原始 session。
- 静态 Header 输入官方支持的 `http_headers` JSON 对象；环境 Header 输入 `env_http_headers` JSON 对象，其值是环境变量名，不是秘密值本身。静态 Header 默认隐藏，日志和诊断包只记录数量。
- API Key 只写入当前 Codex Home 的 `auth.json`，输入框默认隐藏；诊断信息和程序日志不会显示 API Key。
- 首次保存前的 `config.toml` 与 `auth.json` 会被备份。“恢复配置”只在当前文件仍与本工具最后写入的 SHA256 一致时执行，避免覆盖用户后续修改。
- 服务器地址会统一规范为单一 `/v1`；重复 `/v1/v1` 或把 `/v1` 放在路径中间会被拒绝。远程地址要求 HTTPS；仅 `localhost`、`127.0.0.1` 和 `::1` 允许 HTTP。
- “测试连接”分别检查系统代理/DNS、TLS、鉴权、`/models`、目标模型和 `/responses`。`/models` 不可用但 Responses API 可用时会明确显示“模型存在性未确认”，不会误报整个服务器不可用。

## 批量分发

最终只分发：

```text
CodexImageFix.exe
```

批量安装前建议：

- 使用组织的代码签名证书签名最终 EXE，避免 SmartScreen 和企业应用控制拦截未签名程序。
- 固定发布版本和 SHA256，在所有电脑上校验后再运行。
- 先用少量与生产环境相同的电脑验证安装、实时生图、历史图片和卸载回滚。
- Codex Desktop 或内置 CLI 升级后先验证官方是否已修复；官方修复后应卸载本兼容层。

## 工作方式

安装后，程序位于 Codex Desktop 和官方 Codex CLI 之间：

1. 官方 CLI 继续负责账号、模型、session 和所有正常能力。
2. 代理按 thread 路径或 thread ID 定位只读 session。
3. 历史任务的 `thread/read` 与 `thread/resume` 响应会补入 `imageGeneration` item；已有同 ID 空占位项会被原位补全为有效 `savedPath`，响应写出后再按实时生图的协议重放一次去重后的 `item/started` 与 `item/completed`，兼容 Desktop 的内存渲染状态。
4. 新任务会在 `turn/completed` 之前补发配套的 `item/started` 和 `item/completed`。
5. 注入给 Desktop 的 `result` 始终为空，界面只通过绝对 `savedPath` 读取本地图片，不会再次传输大型 Base64。

安装器会动态定位当前电脑自己的官方 Codex CLI，验证官方 CLI、辅助程序和 Microsoft PowerShell 启动副本的 Authenticode 签名，然后：

- 保存原始用户级 `CODEX_CLI_PATH` 和 WindowsApps alias 的字节、属性及 SHA256。
- 安装运行副本到 `%LOCALAPPDATA%\CodexImageDisplayFix`。
- 将用户级 `CODEX_CLI_PATH` 指向经过签名验证的启动副本。
- 写入当前用户登录自启动项并启动系统托盘守护器。
- 验证完整启动链的 `codex-cli --version` 输出未被污染。

守护器发现 `CODEX_CLI_PATH` 或本程序的用户级 Run 项缺失时，只会在 state、launcher、alias、proxy 和 SHA256 全部匹配的情况下恢复入口。任一非空值如果指向其他程序，守护器不会覆盖，只会显示红色异常。`CODEX_CLI_PATH` 恢复后，已经运行的 Codex 必须完全退出并重启，托盘和控制面板会明确显示黄色状态。

黄色重启标记不再依赖可能长期存活的 ChatGPT 后台 PID。修复时间之后一旦出现当前代理的有效 `app-server` 连接，标记立即完成并转为绿色；已打开的控制面板会在下一个 3 秒刷新周期同步显示新状态。

重复安装不会覆盖第一次保存的原始备份。卸载时只有当前值仍属于本工具，才会恢复旧值；用户后续做出的其他修改不会被覆盖。

## 安全与性能

- 在 `turn/start` 请求转发前记录时间和 session 文件末尾偏移，实时恢复只接受该位置之后新增的图片，避免旧图冒充新图。
- 会话定位顺序为响应路径、内存缓存、只读 SQLite 中的 thread ID 映射，递归目录扫描仅作为兜底。
- SQLite 使用 `SQLITE_OPEN_READ_ONLY` 并启用 `query_only`，不会写入 Codex 状态库。
- JSONL 使用持久偏移量增量扫描；历史 Base64 只解析一次，重试只读取新增完整行。
- 同时合并 `event_msg.image_generation_end` 和 `response_item.image_generation_call`。
- 图片是否可用只取决于有效 `result` 或有效 `saved_path`，不依赖状态必须是 `completed`。
- Base64 编码长度上限为 128 MiB，数据 URI 类型必须与图片内容一致。
- PNG 会校验完整 chunk 边界、IHDR、CRC、IDAT 和 IEND；JPEG 会校验 marker、帧、扫描和 EOI；WebP 会校验 RIFF、chunk、尺寸和图像数据。
- 图片先写同目录临时文件并执行 `fsync`，再使用写穿透原子替换。
- 图片以 SHA256 内容寻址，同一 thread 内相同内容只保存一次，实时注入也按 SHA256 去重。
- session 已有 `saved_path` 时，先严格校验文件并计算 SHA256；有 Base64 结果时必须哈希一致才复用。
- 图片代理不修改原 session、账号或全局 PATH；只有用户在 UI 中明确保存模型服务时，才会按上述备份规则修改 `config.toml` 和 `auth.json`。程序只会从 `models_cache.json` 精确移除旧版本留下的专用 Comidea 条目，并将缓存置为待官方刷新；不会插入、克隆或覆盖任何模型元数据。
- 日志只记录进程、事件和字节数，不记录 Base64 内容或输入前缀。
- 运行状态文件只记录 PID、父 PID、版本、启动时间和心跳时间；读取时会验证实时进程路径与创建时间，过期记录自动删除。

## 构建

```powershell
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
cargo build --locked --release
```

唯一发布文件：

```text
target\release\CodexImageFix.exe
```

项目使用静态 CRT 和 bundled SQLite。构建机需要 Visual Studio Build Tools 与 Windows SDK 的 `rc.exe`，用于把多尺寸程序图标嵌入 EXE。可用 `dumpbin /DEPENDENTS` 验证最终 EXE 不依赖 `sqlite3.dll`、`vcruntime*.dll` 或 `msvcp*.dll`。

正式发布使用：

```powershell
.\release.ps1
```

脚本在 `dist` 中生成单文件 EXE，以及归档用的 SHA256、CycloneDX SBOM、第三方依赖清单和构建环境记录。目标电脑运行时仍只需要 `CodexImageFix.exe`。

## GitHub 源码与发布

- 源码构建仅支持 Windows x64。构建机需要 Rust 1.86 或更高版本、Visual Studio Build Tools 和 Windows SDK。
- Git 仓库不保存 `target` 或 `dist`。最终 EXE、SHA256、SBOM、依赖清单和构建信息由 `v*` Tag 的 GitHub Actions 工作流生成。
- Tag 必须与 `Cargo.toml` 版本完全一致，例如版本 `0.4.6` 只能使用 `v0.4.6`。
- 自动化发布只创建草稿 Release。维护者完成 Authenticode 签名、SHA256 复核和发布矩阵验收后，再手动公开 Release。
- 普通用户只需从正式 Release 下载 `CodexImageFix.exe`；其余文件用于安全校验和发布审计，不是运行依赖。

下载后可在 PowerShell 中核对 SHA256：

```powershell
$expected = (Get-Content .\CodexImageFix.exe.sha256).Split(' ')[0]
$actual = (Get-FileHash .\CodexImageFix.exe -Algorithm SHA256).Hash.ToLowerInvariant()
if ($actual -ne $expected) { throw "CodexImageFix.exe SHA256 mismatch" }
```

当前构建在获得组织代码签名证书前属于未签名程序，Windows SmartScreen 或企业应用控制可能拦截。大面积分发前应完成签名、少量试点和阶段 6 的多机验收，不应要求用户绕过组织安全策略。

## 自动化命令

GUI 是默认操作方式，以下命令用于批量部署、诊断和验收，均为用户级操作：

```powershell
CodexImageFix.exe install --silent
CodexImageFix.exe repair --silent
CodexImageFix.exe uninstall --silent
CodexImageFix.exe status --json
CodexImageFix.exe support-bundle --output <诊断包.json>
CodexImageFix.exe diagnose --session <rollout.jsonl>
CodexImageFix.exe restore --session <rollout.jsonl> --output-dir <目录>
CodexImageFix.exe verify-chat --thread <thread-id>
```

由于同一个 EXE 必须保持 Windows GUI 子系统以确保双击时没有命令行黑框，PowerShell 批处理应通过 `.NET Process` 同步等待并读取输出：

```powershell
$start = [Diagnostics.ProcessStartInfo]::new()
$start.FileName = "C:\Deploy\CodexImageFix.exe"
$start.Arguments = "status --json"
$start.UseShellExecute = $false
$start.CreateNoWindow = $true
$start.RedirectStandardOutput = $true
$start.RedirectStandardError = $true
$process = [Diagnostics.Process]::Start($start)
$stdout = $process.StandardOutput.ReadToEnd()
$stderr = $process.StandardError.ReadToEnd()
$process.WaitForExit()
$exitCode = $process.ExitCode
```

稳定退出码：`0` 成功、`2` 参数错误、`10` 未安装、`11` 安装损坏、`20` 配置并发冲突、`30` 网络/服务诊断失败、`1` 其他运行错误。部署秘密不接受命令行明文参数；模型服务 Key 只通过 GUI 保存。

`verify-chat` 成功时，`imageGeneration items` 应大于 0，所有 `result` 都应为空字符串，所有 `savedPath` 都应为存在的绝对路径。

## 适用范围

该工具只适用于 Windows x64 上“session 中存在有效图片，但 Codex Desktop 未生成图片卡片”的协议转换故障。网络、权限、额度、内容策略失败，或 session 中根本没有图片结果，不属于本工具的修复范围。

## Open source license

The project is available under the [MIT License](LICENSE). The official Codex
application, Codex CLI, model services, and their trademarks are not included
in or licensed by this repository.

## Code signing policy

See the public [Code Signing Policy](CODE_SIGNING_POLICY.md) and
[Privacy Policy](PRIVACY.md). Free code signing is provided by
[SignPath.io](https://about.signpath.io), with the certificate provided by the
[SignPath Foundation](https://signpath.org), after the project is accepted.

Releases up to and including `v0.4.6` are unsigned. `v0.4.7` is reserved for
the first SignPath-signed Windows release and will not be published until the
signature, SHA256, SBOM, and release test matrix have been verified.
