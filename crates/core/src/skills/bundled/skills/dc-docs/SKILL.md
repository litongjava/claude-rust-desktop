---
name: dc-docs
description: "DeepSeek Code CLI（dcl）与 Desktop 的使用和排障：构建/运行 dcl、REPL 面板、会话与请求检查、测试分层，以及 MCP 服务器的安装、配置、验证与排障。"
whenToUse: "用于安装或修复 MCP 服务器、使用 dcl 检查会话/请求、驱动 REPL 面板，或验证 CLI/core 改动。"
user-invocable: true
disable-model-invocation: false
---

# DeepSeek Code 使用与 MCP

dcl 是 DeepSeek Code CLI。先选最便宜、能证伪结论的验证层：单元测试不能证明 REPL 渲染，PTY 也不能证明解析逻辑。

本 skill 自带以下资源（路径相对本 skill 目录）：

- `scripts/probe_mcp.py`：stdio MCP 握手、`tools/list` 与可选 `tools/call` 探针。
- `scripts/drive_dcl.py`：通过 ConPTY 驱动 dcl 并渲染屏幕。
- `references/pty-harness.md`：PTY/ConPTY 细节与启动数据目录。

## 使用 dcl

### 构建与运行

```bash
cargo build --locked -p deepseek-code-cli --bin dcl
./target/debug/dcl.exe --version
```

驱动 `target/debug/dcl.exe`（或 `target/release/dcl.exe`），不要用已安装的 `dcl`，除非验证对象就是安装副本。

用 `--data-dir <temp>` 或 `DEEPSEEK_CODE_DATA_DIR` 隔离状态；不要把测试指向真实会话或凭据，使用本地 mock 端点。

### 测试分层

| 要证明的结论 | 运行 |
| --- | --- |
| 核心解析、状态、发现 | `cargo test --locked -p deepseek-code-core --lib <filter>` |
| CLI 格式化、状态文件、斜杠派发 | `cargo test --locked -p deepseek-code-cli --bin dcl <filter>` |
| 真实进程、HTTP 形状、会话 DB | `cargo test --locked -p deepseek-code-cli --test <file> <filter>` |
| REPL 提示、面板渲染、按键处理 | 下面的 PTY harness |

修改 `crates/cli` 或 `crates/core` 前读 `docs/cli-regression-tests.md`；修改 `src/` 或 `src-tauri/` 前读 `docs/desktop-regression-tests.md`。

### REPL 面板

`/skills`、`/plugins`、`/tools` 打开整页面板：

- `←/→` 切标签，`Space` 切换，`Tab` 切换 on-demand（仅 Tools → Built-in），`Enter` 打开详情或状态选择，`Esc` 关闭。
- `/skills` 只列技能；`/plugins` 列插件包；`/tools` 有 Built-in、Skills、Mcp、Plugin 标签。
- 非终端等价命令：`dcl skills list`、`dcl config show`、`dcl plugin list`、`dcl mcp list`。

Piped stdin 不是 REPL：`dcl` 会拒绝（`repl requires interactive stdin`）。用 `scripts/drive_dcl.py` 或 `references/pty-harness.md`。

### 检查一次运行

- `dcl requests <session>` 列出记录的 HTTP 请求；`dcl sessions list` 找会话 ID。
- `dcl config show` 显示生效的工具开关（`tools.<name>.enabled/on_demand`）。
- `dcl skills list` 打印每个技能及其完整内容；用 Python 或 jq 汇总，不要整段粘贴。

### 容易误判的测试失败

- **新数据目录未就绪**：会先显示提供商 onboarding，再显示文件夹信任提示。把可用的 `config.json` 复制进去，并预置 `<data>/startup-prompts.json`；`references/pty-harness.md` 有精确形状。
- **Windows 路径测试可能在 HEAD 就失败**：路径分隔符断言和硬编码哈希可能在你的改动前就失败。先 `git stash push` 后重跑同一 filter，再归因。
- **并行 SQLite 测试会抖动**：单独运行才通过的 `SQLITE_BUSY` 是竞争，不是回归；单独重跑精确 filter。
- **面板输出不只在 stdout**：TUI 走 `stderr`。两边都捕获，用 `pyte` 渲染后读屏幕；手工剥离转义会产生重复片段。

### 汇报

说明精确命令与 filter、观察到的测试数量和结果。保持层次分离：unit、binary、integration、PTY。通过的 unit filter 不能证明面板渲染，也不能证明真实请求携带了工具。

## MCP 服务器

### 先确认

- 配置是全局的：`$DEEPSEEK_CODE_DATA_DIR/mcp-servers.json`，默认 `~/.deepseek-code/mcp-servers.json`；dcl 与 Desktop 共用同一份。
- 优先用 `dcl mcp ...` 修改；只有需要 `cwd`、`env`、`headers` 或批量配置时才手写 JSON。
- `dcl mcp add` 只写配置，不启动进程。运行中的会话不会重新读取配置；添加或修改后新开一个 dcl 会话。
- 工具名是 `mcp__<server>__<tool>`；server 与 tool 中 `[A-Za-z0-9_-]` 以外的字符替换为 `_`。
- `dcl doctor` 不是 CLI 子命令；`/doctor` 只在 REPL 内可用。

### 安装 stdio 服务器

```bash
dcl mcp add <name> <command> [args...]
```

示例：aoci，不写死仓库路径。

```bash
dcl mcp add aoci D:/bin/aoci.exe mcp
```

`dcl mcp add` 默认 `enabled: true`。Windows 路径用正斜杠或转义反斜杠。

### 安装远程服务器

```bash
dcl mcp add --transport http <name> <url>
dcl mcp add --transport sse  <name> <url>
dcl mcp add --transport ws   <name> <url>
dcl mcp add --transport http -H "Authorization: Bearer $TOKEN" <name> <url>
```

`type` 支持 `http`、`streamable-http`、`sse`、`ws`、`websocket`。远程服务器不接收命令参数，`-H` 只用于远程。

### 手动编辑

```json
{
  "mcpServers": {
    "aoci": {
      "command": "D:/bin/aoci.exe",
      "args": ["mcp"],
      "enabled": true
    }
  }
}
```

可选字段：

- `"env": {"KEY": "VALUE"}`：传给子进程的环境变量。
- `"cwd": "D:/path/to/repo"`：stdio 子进程的工作目录；默认继承 dcl 进程的 cwd。
- `"type": "http"`、`"url": "https://..."`、`"headers": {"Authorization": "Bearer ..."}`：远程服务器。
- `enabled` 缺省为 `true`。

### 工具状态

```bash
dcl mcp state                 # 列出所有服务器状态
dcl mcp state aoci            # 查看一个
dcl mcp state aoci on-demand  # 设置
```

| 状态 | 行为 |
| --- | --- |
| `user`（默认） | 工具进入每次模型请求 |
| `on-demand` | 不进入工具 schema；在 “Tools you can load with ToolLoad” 中按名称列出，先用 `ToolLoad` 加载精确名称，未加载就调用会被拒绝 |
| `user-only` | 对模型隐藏，模型调用会被拒绝 |
| `off` | 禁用并停止服务器；不写入状态文件，重新启用时恢复之前的状态 |

`dcl mcp state` 只写 `mcp-states.json`；它不会在运行中的会话里启动服务器。

### 验证

```bash
dcl mcp list    # 只看配置；独立进程里 running 通常为 false
dcl mcp hosts   # 查看存活 host 及其正在运行的服务器 id
```

新开会话后执行 `/mcp`。正常时应显示类似 `aoci  connected · 9 tools`；`/mcp` 里还有 Reconnect、Set state、Disable。

需要直接验证 stdio 握手时，使用本 skill 的探针脚本。默认 bundled 路径是 `~/.deepseek-code/skills/bundled/dc-docs/scripts/probe_mcp.py`；其他安装位置用本 skill 目录下的同名文件：

```bash
python "~/.deepseek-code/skills/bundled/dc-docs/scripts/probe_mcp.py" \
  --command D:/bin/aoci.exe --arg mcp

python "~/.deepseek-code/skills/bundled/dc-docs/scripts/probe_mcp.py" \
  --command D:/bin/aoci.exe --arg mcp --call aoci_header
```

脚本发送 `initialize`、`notifications/initialized`、`tools/list`，可选 `tools/call`；只依赖 Python 标准库。

### aoci 示例

- aoci 是仓库认知 MCP，源码：<https://github.com/aoci-spec/aoci-code>。
- **不要**在全局配置里写死 `--repo`。aoci 默认从自己的 cwd 自动定位仓库（`--repo` 缺省为 `.`）。dcl 用 `-C` 切换项目目录，并在 worktree/工作区切换时更新 cwd；stdio MCP 子进程默认继承该 cwd，所以每个会话会指向自己的仓库。
- 已验证：在 deepseek-code 目录启动得到 deepseek-code 的 Root；在 HuLa 目录启动得到 HuLa 的 Root；在临时目录启动返回 `not_initialized`。
- 确实要固定一个仓库时，才用 `--repo` 或 `cwd`：

  ```bash
  dcl mcp add aoci D:/bin/aoci.exe --repo D:/path/to/repo mcp
  ```

  这会让所有会话都使用该仓库，除非你本来就要这样。
- 重启会话后，aoci 提供 9 个工具：`aoci_rules`、`aoci_overview`、`aoci_maintain`、`aoci_update_entry`、`aoci_get_entries`、`aoci_header`、`aoci_remove_entry`、`aoci_report`、`aoci_search`。
- 工具返回 `not_initialized` 时，在目标仓库根执行 `aoci init`，或检查 cwd 是否指向了正确的仓库。

### 排障

- 运行中的会话看不到新服务器：配置只在 MCP 初始化时读取，重启会话。
- 会话输出 `MCP <name>: ...` 或 `/mcp` 显示 failed：检查 command、args、cwd 和权限，再用探针脚本复现。
- `dcl mcp list` 里所有 `running` 都是 false：这是正常的，list 命令启动自己的 manager 但不启动服务器；用 `/mcp` 或 `dcl mcp hosts` 看存活状态。
- 远程服务器：URL 必须可达；请求头用 `-H` 或 JSON 的 `headers`。
- `env`/`headers` 可能包含凭据；`dcl mcp list` 会脱敏，但不要把 `mcp-servers.json` 提交到仓库。
