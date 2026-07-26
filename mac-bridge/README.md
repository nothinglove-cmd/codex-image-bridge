# Comidea Codex Image Bridge for macOS

这是 macOS 的临时单文件版本。它使用官方 Node.js 24 运行，不需要 Electron、`.app`、DMG、PKG、后台服务或本项目自己的 Apple Developer 签名。

Windows 用户继续使用仓库根目录说明中的 Rust 版 `CodexImageFix.exe`。Mac 版不会安装或修改任何 Windows 文件。

## 分发文件

目标电脑只需要：

```text
codex-image-bridge.mjs
```

`test.mjs`、`package.json` 和本 README 不是运行依赖。

## 前置条件

- 当前 Codex Desktop 支持的 macOS，首批验收以 Apple Silicon 为准。
- 官方 Codex Desktop 已安装在 `/Applications` 或当前用户的 `~/Applications`。
- 官方 Node.js 24 已安装，`node --version` 显示 `v24.x.x` 或更高版本。
- Codex Desktop 已完成 ChatGPT 登录。
- 自定义服务兼容 OpenAI Responses API，并提供 `gpt-image-2` 图片接口。

组织策略如果禁止执行下载的脚本，应由 IT 管理员统一放行。不要关闭 Gatekeeper，也不要修改官方 `Codex.app` 的签名内容。

## 使用

打开终端，进入文件所在目录后运行：

```bash
node codex-image-bridge.mjs
```

浏览器会打开只监听 `127.0.0.1` 随机端口的本地界面：

1. 输入 OpenAI 兼容服务器地址，例如 `https://api.example.com/v1`。
2. 输入本次使用的 API Key。
3. 保持“启用图片生成”打开。
4. 点击“测试连接”。
5. 点击“启动 Codex”。

程序会请求已运行的 Codex 完全退出，然后直接启动官方 App 主程序，并让官方 App 在本次进程环境中使用桥接 CLI。Codex 启动后，本地网页服务和终端命令会自动结束，不需要保持窗口开启。

API Key 不会写入磁盘。每次重新启动 Codex，都需要再次运行本文件并填写或粘贴 Key。卸载只需删除 `codex-image-bridge.mjs`，因为该版本没有安装状态。

## 图片模型

当前 Codex 官方图片工具把后端模型固定为：

```text
gpt-image-2
```

它不是聊天模型，因此不会出现在聊天模型下拉框中。本工具通过临时 Codex 配置开启 `[features].image_generation`，并配置 `comidea` 自定义 provider。文本请求和图片工具请求都会使用本次填写的兼容服务；官方 Codex 的账号与会话逻辑保持不变。

## 文件位置

- Codex Home：优先使用 `CODEX_HOME`，默认 `~/.codex`。
- Session：`~/.codex/sessions` 和 `~/.codex/archived_sessions`。
- 状态库：`~/.codex/state_*.sqlite`，只读打开并启用 `query_only`。
- 恢复图片：`~/.codex/generated-images/<thread-id>/<sha256>.<ext>`。

原始 session 不会被修改。PNG、JPEG 和 WebP 会先进行结构校验，再通过临时文件、文件 `fsync`、原子 `rename` 和目录 `fsync` 落盘。

## 诊断

不启动 UI 的探针命令：

```bash
node codex-image-bridge.mjs --probe --json
```

输出不包含 API Key、Authorization、会话正文、提示词或 Base64。主要检查 Node 版本、CPU 架构、Codex App、内置 CLI、Codex Home 和桥接文件路径。

## 开发验证

```bash
cd mac-bridge
npm test
npm run check
```

项目没有 npm 运行依赖。测试覆盖旧图偏移隔离、增量 JSONL、两种图片事件、非 completed 状态、三种图片格式、Base64 上限、只读 SQLite、`saved_path` 哈希、SHA256 去重、实时/历史协议注入和本地 UI 请求防护。

## 发布边界

Windows 或 CI 可以验证可移植核心逻辑，但正式分发前仍必须在真实 Apple Silicon Mac 上完成：

- `CODEX_CLI_PATH` 被当前 Codex Desktop 版本采用；
- App 完全退出后能继承新的进程环境；
- 自定义 provider 与现有 ChatGPT 登录能同时通过图片工具门控；
- 新生成图片直接出现在聊天区；
- Safari 与 Chrome 中的本地 UI 操作正常。

这些项目未在目标 Mac 上验收前，应把文件视为测试候选版本，而不是大面积部署版本。
