# CLI 与 Desktop 共用代码

```text
Cargo workspace
├── crates/core       claude-core：模型、工具、权限、会话、配置、SQLite
├── crates/cli        claude-code-cli：命令解析、REPL、流式输出
├── src-tauri         claude-desktop-tauri：Tauri、React Bridge、系统凭据适配
└── src-tauri/worker  agent-worker：计算机操作辅助进程
```

`claude-code` 直接调用 `claude-core::NativeEngine`，无需启动 Desktop、WebView 或 30085 服务。核心不依赖 Tauri。桌面库重新导出共享模块，保持原有 Rust 调用路径兼容。

## 构建

```powershell
# 原生 CLI，可独立于 npm/Tauri 编译
cargo build --release -p claude-code-cli
./src-tauri/target/release/claude-code.exe --help

# 一次产出 CLI、Desktop 和桌面安装包
npm ci
npm run build:all

# 开发构建 / 单独构建 / 编译检查
npm run build:all -- --debug --no-bundle
npm run build:cli
npm run build:desktop
npm run check:rust
```

统一脚本将产物放在 `artifacts/<版本>-<模式>-<时间>/`。Cargo 缓存和 Tauri 安装包路径仍在 `src-tauri/target`。Windows 安装包由 Tauri 生成；macOS/Linux 需在对应平台构建并安装 Tauri 的系统依赖。也可以继续使用 `npm run tauri -- build`。

自定义 OpenAI/Anthropic 协议由原生 Rust 客户端处理。内置 provider harness 仍需 **Node.js 22+** 和 `runtime/provider-runtime.mjs`；统一构建脚本会打包并复制脚本。只运行 `cargo build` 时，自定义协议不需要该脚本；使用内置 harness 请用统一构建脚本。

## 使用

```powershell
# 使用 Desktop 已保存的 provider，模型 ID 必须匹配
claude-code --provider my-provider --model my-model --path D:\code\my-project

# 环境变量仅注入当前运行，不写入 providers.json
$env:DEEPSEEK_API_KEY = '<your key>'
claude-code --model deepseek-chat query -p '检查项目并解释测试失败原因'

# 单次执行和机器可读输出
claude-code --model deepseek-chat --print '概述项目结构'
'解释这段代码' | claude-code --model deepseek-chat --print --output-format json
claude-code --model deepseek-chat query -p '读取 README' --output-format stream-json

# 任意兼容端点；传入环境变量名而非命令行明文密钥
claude-code --model custom-model --base-url https://example.com --api-format openai --api-key-env MY_API_KEY query -p '检查项目'

claude-code sessions list
claude-code --resume <session-id>
claude-code config show
claude-code config set model deepseek-chat
claude-code config set base_url https://api.deepseek.com
claude-code config set api_format openai
claude-code config import-legacy C:\Users\you\.claude-code\settings.json
claude-code mcp list
claude-code mcp add my-server node -- server.mjs
claude-code skills list
claude-code skills execute <skill-id> '任务说明'
```

默认数据目录与 Desktop 的 `com.claude.desktop` 应用目录一致，可用 `--info` 查看。`--data-dir` 或 `CLAUDE_RUST_DATA_DIR` 可选择隔离目录。两端使用相同的 `config.json`、`claude_desktop.db` 和 MCP 配置。CLI 创建的会话是绑定项目的 Code 会话，沿用项目目录校验和 Code 提示词；恢复会话时使用原项目，传入冲突的 `--path` 会报错。

当前项目的 `CLAUDE.md` 和 `AGENTS.md` 会加入 CLI 请求。模型参数和环境密钥仅在内存生效。`config show` 隐去凭据；`config set api_key` 与 Desktop 的 provider 配置一样会保存密钥到本地配置，优先使用环境变量。Tauri 的登录 token 和系统安全存储仍由 Desktop 管理，CLI 不尝试读取或降级存储这些 token。

REPL 支持 `/help`、`/status`、`/history`、`/tools`、`/exit`；Ctrl+C 取消当前任务，EOF 退出。普通输出只在 stdout 写模型文本，日志和交互提示写 stderr。`json` 返回最终对象；`stream-json` 每行一个事件，以 `type=result` 结束。执行失败返回非零退出码。

## 权限

默认 `--permission-mode ask`。交互会话在终端确认工具操作；单次脚本遇到需要批准的操作立即失败，不自动授权，也不等待五分钟。

- `ask`：按共享权限规则询问。
- `accept-edits`：允许编辑，其他敏感操作仍需确认。
- `plan`：共享核心的只读规划模式。
- `bypass`：显式放开工具权限，适用于你已授权的自动化环境。

## 旧 CLI 合入范围

以下记录当前迁移状态。用户已要求继续补齐外围功能，后续任务、实现顺序和验收标准见 [CLI / Desktop 外围功能迁移交接](cli-desktop-migration-handoff.md)；保留源码快照不代表这些功能已交付。

旧仓库完整的自有 Rust 源码和测试保存在 `legacy/claude-code-cli`，不参与发布构建。活跃 CLI 保留 `query -p`、`repl -p`、`rust`（REPL 别名）、`--model`、`--verbose`、`--no-interactive`、`--info`、配置、MCP、技能和项目初始化入口，改用共享引擎；根级项目路径短参数现在为 `-C`，长参数 `--path` 保留。

旧 `voice`、`services`、`magic-docs`、`team-sync`、`plugin`、`agent`、`stress-test`、旧 JSON memory 管理及原 GUI/Web/WASM 前端保留在源码快照中，**没有宣称这些命令已接入新 CLI**。旧配置导入目前迁移 provider、模型与 token 上限；旧 memory/session、插件和 MCP 数据不会自动转换。这样既保留旧实现，又避免两套模型和工具引擎在正式产物中并存。

## 验证

```powershell
cargo test -p claude-core --lib --tests
cargo test -p claude-code-cli
cargo check -p claude-desktop-tauri --all-targets
cargo test -p claude-desktop-tauri --lib --tests
```

CLI 集成测试使用临时数据目录和本机模拟模型服务，实际启动可执行文件，覆盖流式输出、工具读写、权限中断、会话恢复、失败退出和旧配置只读导入，不使用真实 API 密钥。
