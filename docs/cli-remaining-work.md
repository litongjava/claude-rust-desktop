# claude-code-cli 剩余工作清单（新 session 入口）

更新：2026-10-17。本文汇总独立 Rust CLI 与原版 Claude Code 对齐的未完成项，供新 session 直接接手。详细审计依据见 `D:/code/project/project-harven/claude-code-cli-doc/remaining-parity-acceptance-checklist.md`（以下简称"清单"）。

## 1. 起步

- 开发目录：`D:/code/project/project-harven/claude-rust-desktop-cli-worktree`，分支 `feat/cli-parity-worktree`，本文提交前 HEAD `a01e83a`。
- 未 push：本地 `origin/feat/cli-parity-worktree` 仍为 `9c9028e`。
- 原始 checkout `D:/code/project/project-harven/claude-rust-desktop` 有大量用户未提交改动：不 reset、不 clean、不覆盖，不在那里开发。
- **第一步先问用户**：开发验收记录、终端录屏、测试脚本以后放哪里？是否清理 `claude-code-cli-doc` 中约 8.9 GB 的开发资料（`terminal-*` 录屏、`logs/`）？确认前不删除、不移动。

## 2. 约束

- `claude-code-cli-doc` 是 CLI 的**用户使用手册**，`claude-rust-desktop-docs` 是 Desktop 的用户文档；不要再往里写开发记录。
- Cargo 命令串行执行，统一 `CARGO_TARGET_DIR=D:/code/project/project-harven/claude-rust-desktop/target`。
- 节奏：开发一个、验证一个、commit 一个。英文提交信息，沿用 `feat:`/`fix:`/`docs:` 前缀，结尾 `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`。不 push，不自动关机。
- 只读参考：`D:/code/project/project-harven/claude_code_src`（下文 `reference:`）、`D:/code/project/project-harven/codex`、需求列表 `D:/code/project/project-harven/instructions.txt`。官方程序 `E:/package/claude-code/2.1.295/claude.exe`。
- 源文件多为 CRLF：用 Python 脚本"读入→统一 LF→精确替换→写回 CRLF"编辑（示例 `claude-code-cli-doc/logs/fmt-backup/relay_cli.py`）；`rustfmt` 只用于新文件或测试文件，避免无关格式改动。提交前 `git diff --check`。
- 结束测试进程按进程名（`claude-code.exe`、`remote_control_mock.exe`、`node.exe`）过滤；按 CommandLine 过滤会误杀当前 bash。
- PowerShell 文本显式 UTF-8，避免控制台表格。不提交凭据；Remote Control 只用自身 token（`CLAUDE_BRIDGE_OAUTH_TOKEN`），绝不发送 Anthropic OAuth 凭据。
- 保留 `D:/code/project/project-github/qwerty-learner-cli-pass1`、`-cli-pass2` 的未提交改动。
- 浏览器测试用 `D:/code/project/project-browser-use/deepseek-browser-use`。

## 3. 验证工具

- 测试（最近结果）：
  - `cargo test --locked -p claude-core --lib --tests`：543 通过 / 0 失败 / 4 忽略
  - `cargo test --locked -p claude-code-cli`：251 / 0 / 1
  - `cargo check --locked -p claude-desktop-tauri --all-targets`
  - `cargo build --locked -p claude-code-cli`
- 真实终端（Windows ConPTY），目录 `D:/code/project/project-harven/claude-code-cli-doc/`：
  - `node terminal-harness.mjs <spec.json>`：启动 CLI；运行中向 `<evidence>/input.jsonl` 追加 `{"write": "..."}` 或 `{"snapshot": "name"}` 行进行交互。
  - `node logs/fmt-backup/screen.js <abs path .screen.json> N`：读屏幕快照。
  - `integration-acceptance-fixture.mjs <prefix>`：本地模型 fixture（STEP_EDIT / STEP_WRITE2 / STEP_SLOW / STEP_TOOLS 等脚本化步骤）。
  - `crates/core/examples/remote_control_mock.rs`：Remote Control 模拟服务（`remote_control_mock.exe 5516 rc-mock-token`）；`node logs/fmt-backup/rc_answer.mjs allow|deny|interrupt|state` 扮演远程客户端。
- 测试用临时目录与本地 mock 端点，不用真实凭据。

## 4. 已完成（不要重做）

- 第一阶段集成：MCP 输出替换 `ef22c86`、启动工具规则 `94718dc`、主题修复 `8be287b`。
- 已配置 MCP 工具命名空间 `mcp__<server>__<tool>`：`be8132e`。
- command hook `if` 条件过滤：`7d3c9fe`。
- `/plan <描述>` 进入计划模式并提交任务一次：`08e92cf`。
- `/remote-control`：自定义服务端地址、mock、消息/控制转发、重连、拆除 `f2748fa`；工具审批转发与远程中断 `a01e83a`。
- 清单"Already implemented"一节：SDK hook 回调、SDK MCP 传输、SDK 自定义子 agent、`/statusline setup`、LSP 生命周期、目标提议、owned-child 恢复与后台任务、内置资源工作流等。

## 5. 待开发（按优先级）

每项完成标准：真实触发 + 精确 payload + 事件语义 + 超时/取消 + 会话隔离 + 恢复时不重复；有单元/集成测试，并在需要时做 ConPTY 验收。schema 条目或 `/hooks` 列表不算完成。

### P1 Hook 类型与选项（清单 H2）

入口：`crates/core/src/services/tool_hooks.rs`（`CommandConfig` 约 174 行；loader 只接受 `type: command` 并拒绝 `async: true`；`ActiveHook::condition_matches` 约 77 行，新类型必须复用）。参考：`reference:src/schemas/hooks.ts`、`reference:src/utils/hooks.ts`、`reference:src/utils/hooks/exec{Prompt,Http,Agent}Hook.ts`。

1. `once`：匹配执行一次后注销，重复调用不再运行。
2. `statusMessage`：hook 运行期间显示、结束清除的进度文本。
3. HTTP hook：本地 HTTP fixture；超时/错误策略；header 环境变量插值需显式白名单。
4. Command `async`：生命周期托管的后台执行，后续允许的输出投递，进程清理。
5. `asyncRewake`：完成/唤醒语义（尤其 exit code 2），取消后不重放。
6. Prompt hook：有界的 provider 评估、事件限制、输出校验、失败策略。
7. Agent hook：有界验证 agent，只用原版允许的工具子集，不继承主会话权限。

未支持的选项要明确报错，不能静默接受。验证命令 hook 与 SDK 回调组合时的顺序和 deny/ask 优先级。

### P2 Hook 生命周期事件（清单 H1）

入口：`tool_hooks.rs` 的 `HookEvent`（当前 15 个，约 20 行）；loader 会跳过未知事件名。参考：`reference:src/entrypoints/sdk/coreTypes.ts`（27 个）。

- 无外部依赖，先做：`Notification`、`StopFailure`、`Setup`、`ConfigChange`、`InstructionsLoaded`、`CwdChanged`、`FileChanged`。
- 需本地 MCP elicitation 请求方：`Elicitation`、`ElicitationResult`。
- 依赖 P4 团队：`TaskCreated`、`TaskCompleted`（完成前可拒绝）、`TeammateIdle`。

### P3 Hook 输出与环境（清单 H3）

入口：`crates/core/src/services/hook_callbacks.rs`（拒绝 async 结果、`initialUserMessage`、`watchPaths`）。设计参考 `claude-code-cli-doc/sdk-hook-output-parity-plan.md`。

- `SessionStart.initialUserMessage`：恰好排队一次，带 hook 来源，不进入用户授权账本。
- `watchPaths`：会话级 watcher，忽略初次发现，稳定的 add/change/unlink，路径不存在时监视父目录，cwd 切换与关闭隔离。
- 会话环境文件：原版在 Windows 返回 null，Windows 无需实现；Linux/macOS 需另行验证。

### P4 团队（清单 T1、T2）

入口：`crates/core/src/tools/coordination.rs`（当前 team 是持久任务记录）、`crates/core/src/native_engine/delegation.rs`（顺序执行委派）、`crates/core/src/tools/agent_messages.rs`（UUID 寻址文本消息）。参考：`reference:src/tools/TeamCreateTool/`、`TeamDeleteTool`、`SendMessageTool/SendMessageTool.ts`；原版受 `isAgentSwarmsEnabled()` 开关控制。

- T1：团队身份、leader、命名成员与生命周期持久化；至少两个 teammate 并发；崩溃恢复不留孤儿进程；有活跃成员时拒绝删除。
- T2：按名字投递与 `*` 广播；shutdown_request/response 关联；teammate 计划审批路由到 leader；`/list-agents` 区分子 agent、teammate、独立会话。

### P5 SDK 兼容（清单 S1、S2）

- S1，入口 `crates/core/src/services/permission_updates.rs`（只允许 `session` 目标）：实现 user/project/local 目标（原子写、优先级、跨进程持久化）与 setMode/directory 操作；混合批次中有错误则全部不生效；不得绕过显式 deny 或项目信任。
- S2，入口 `crates/cli/src/sdk_agents.rs`、`crates/cli/src/sdk_initialization.rs`：其余 AgentDefinition 字段（mcp/skills/background/memory/permissionMode/effort/initialPrompt 等）逐项实现；主线程选择 SDK 定义；promptSuggestions 与 agent 进度摘要。

### P6 命令内部差异（清单 C1、C2）

- `/plan`：allowedPrompts 语义化 Bash 授权（入口 `crates/core/src/native_engine/plan_tool.rs`，参考 `reference:src/tools/ExitPlanModeTool/ExitPlanModeV2Tool.ts`）；clear-context 行为验收（保留会话 UUID 是设计差异）。
- `/workflows`（`crates/cli/src/background_commands.rs`）：确定本地 workflow 范围；托管 workflow 需对应账号，不得伪造。
- `/batch`（`crates/cli/src/batch_commands.rs`）：与原版自动编排对比；分别验收失败、取消、重叠检查、恢复与真实 PR。
- `/memory`（`crates/cli/src/display_commands.rs`）：原版 MemoryFileSelector 式文件选择。
- 交互式配置/powerup：按用户操作（选择/编辑/保存/取消）对比原版。

### P7 需要真实环境或账号的验收（清单 D、C3）

代码已存在，缺的是真实环境验证；每项开始前向用户确认所需账号/环境。

- 真实 Remote Control 服务与客户端（授权账号）。
- 真实账号 OAuth、用量、fast 模式权限、反馈投递。
- IDE 扩展（真实扩展版本、选区、diff 接受/拒绝、断线重连）。
- Design 服务（授权、上传、渲染；浏览器测试按 deepseek-browser-use 流程）。
- 真实 LSP 服务器（rust-analyzer/TypeScript/Python，九种操作、写入后诊断、重启）。
- Linux/macOS 编译与运行。
- 与官方 CLI 的终端对比（同宽度/主题/模型，原始 ANSI + 最终屏幕）与响应速度（本地端点同延迟下分离启动/输入/渲染与首 token 延迟）。
- qwerty-learner issue #1095：先读 `issue1095-pass2-final-review.md` 等已有记录，目标仓库不提交。

## 6. 每项完成后

1. 测试通过 + 必要的 ConPTY 验收 + commit。
2. 在用户指定的开发记录位置记录：commit、fixture/终端场景、结果、失败/取消/持久化覆盖、剩余外部要求；同步勾选清单并更新 `cli-parity-current.md` 对应行。
3. 遗留记账：清单 C3 第二项（工具审批转发与中断）已在 `a01e83a` 完成但未勾选；其 ConPTY s4 结果（远程允许 Edit 生效、远程拒绝 Write、本地 `y` 先答时向远程发送 `control_cancel_request`、远程中断 STEP_SLOW、空闲中断不影响下一回合、`/rc disconnect` 正常退出）尚未写入 `remote-control-acceptance-20261017.md`，证据在 `claude-code-cli-doc/terminal-remote-control-20261017/s4/`。确定记录位置后补写。
4. 若用户希望，在 `claude-code-cli-doc` 编写面向用户的使用手册（安装、配置、常用命令、远程控制等）。
