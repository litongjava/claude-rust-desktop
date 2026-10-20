# claude-code-cli 剩余工作清单（新 session 入口）

更新：2026-10-18。本文汇总独立 Rust CLI 与原版 Claude Code 对齐的未完成项，供新 session 直接接手。详细审计依据见 `D:/code/project/project-harven/temp/claude-code-cli-dev/remaining-parity-acceptance-checklist.md`（以下简称"清单"）。

## 1. 起步

- 开发目录：`D:/code/project/project-harven/claude-code-desktop`，分支 `feat/claude-code-optimization`。2026-10-17 已合并全部 worktree 和功能分支并删除，直接在此开发，不再使用 worktree。
- 未 push。合并备份与已删除分支的 SHA 在 `D:/code/project/project-harven/temp/merge-backup/`；`git stash@{0}` 为合并前原 checkout 的未提交改动（已被合并内容取代，保留以备核对）。
- 开发记录目录：`D:/code/project/project-harven/temp/claude-code-cli-dev`（状态见其中 `handoff-current.md`、`README.md`）。

## 2. 约束

- `claude-code-cli-doc` 是 CLI 的**用户使用手册**，`claude-rust-desktop-docs` 是 Desktop 的用户文档；开发记录、终端证据、测试脚本一律写到 `temp/claude-code-cli-dev`。
- Cargo 命令串行执行，统一 `CARGO_TARGET_DIR=D:/code/project/project-harven/claude-code-desktop/target`。
- 节奏：开发一个、验证一个、commit 一个。英文提交信息，沿用 `feat:`/`fix:`/`docs:` 前缀，结尾 `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`。不 push，不自动关机。
- 只读参考：`D:/code/project/project-harven/claude_code_src`（下文 `reference:`）、`D:/code/project/project-harven/codex`、需求列表 `D:/code/project/project-harven/instructions.txt`。官方程序 `E:/package/claude-code/2.1.295/claude.exe`。
- 源文件多为 CRLF：用 Python 脚本"读入→统一 LF→精确替换→写回 CRLF"编辑（示例 `temp/claude-code-cli-dev/logs/fmt-backup/relay_cli.py`，其中路径需改成当前目录）；`rustfmt` 只用于新文件或测试文件，避免无关格式改动。提交前 `git diff --check`。
- 结束测试进程按进程名（`ccl.exe`、`remote_control_mock.exe`、`node.exe`）过滤；按 CommandLine 过滤会误杀当前 bash。
- PowerShell 文本显式 UTF-8，避免控制台表格。不提交凭据；Remote Control 只用自身 token（`CLAUDE_BRIDGE_OAUTH_TOKEN`），绝不发送 Anthropic OAuth 凭据。
- 保留 `D:/code/project/project-github/qwerty-learner-cli-pass1`、`-cli-pass2` 的未提交改动。
- 浏览器测试用 `D:/code/project/project-browser-use/deepseek-browser-use`。

## 3. 验证工具

- 测试（最近结果）：
  - `cargo test --locked -p claude-core --lib --tests`：543 通过 / 0 失败 / 4 忽略
  - `cargo test --locked -p claude-code-cli`：251 / 0 / 1
  - `cargo check --locked -p claude-code-desktop --all-targets`
  - `cargo build --locked -p claude-code-cli`
- 真实终端（Windows ConPTY），目录 `D:/code/project/project-harven/temp/claude-code-cli-dev/`（旧 spec 中 `evidenceDir` 仍指向 `claude-code-cli-doc`，复用前修改）：
  - `node terminal-harness.mjs <spec.json>`：启动 CLI；运行中向 `<evidence>/input.jsonl` 追加 `{"write": "..."}` 或 `{"snapshot": "name"}` 行进行交互。
  - `node logs/fmt-backup/screen.js <abs path .screen.json> N`：读屏幕快照。
  - `integration-acceptance-fixture.mjs <prefix>`：本地模型 fixture（STEP_EDIT / STEP_WRITE2 / STEP_SLOW / STEP_TOOLS 等脚本化步骤）。
  - `crates/core/examples/remote_control_mock.rs`：Remote Control 模拟服务（`cargo build --locked -p claude-core --example remote_control_mock` 后运行 `remote_control_mock.exe 5516 rc-mock-token`）；`node logs/fmt-backup/rc_answer.mjs allow|deny|interrupt|state` 扮演远程客户端。
- 测试用临时目录与本地 mock 端点，不用真实凭据。

## 4. 已完成（不要重做）

- 第一阶段集成：MCP 输出替换 `ef22c86`、启动工具规则 `94718dc`、主题修复 `8be287b`。
- 已配置 MCP 工具命名空间 `mcp__<server>__<tool>`：`be8132e`。
- command hook `if` 条件过滤：`7d3c9fe`。
- `/plan <描述>` 进入计划模式并提交任务一次：`08e92cf`。
- `/remote-control`：自定义服务端地址、mock、消息/控制转发、重连、拆除 `f2748fa`；工具审批转发与远程中断 `a01e83a`（ConPTY session 4 已验收并记录）。
- 清单"Already implemented"一节：SDK hook 回调、SDK MCP 传输、SDK 自定义子 agent、`/statusline setup`、LSP 生命周期、目标提议、owned-child 恢复与后台任务、内置资源工作流等。

## 5. 待开发（按优先级）

每项完成标准：真实触发 + 精确 payload + 事件语义 + 超时/取消 + 会话隔离 + 恢复时不重复；有单元/集成测试，并在需要时做 ConPTY 验收。schema 条目或 `/hooks` 列表不算完成。

### P1 Hook 类型与选项（清单 H2）

入口：`crates/core/src/services/tool_hooks.rs`（`CommandConfig` 约 174 行；loader 只接受 `type: command` 并拒绝 `async: true`；`ActiveHook::condition_matches` 约 77 行，新类型必须复用）。参考：`reference:src/schemas/hooks.ts`、`reference:src/utils/hooks.ts`、`reference:src/utils/hooks/exec{Prompt,Http,Agent}Hook.ts`。

1. [x] `once`：首次成功后按会话持久化注销；并发、重载、失败/取消重试已验证（`5b026c8`，详见开发记录 `once-acceptance-20261018.md`）。
2. [x] `statusMessage`：CLI/worker 运行进度、完成/失败/取消清除及 ConPTY 恢复验收完成（`a2a0366`，详见 `status-message-acceptance-20261018.md`）。
3. [x] HTTP hook：本地 HTTP fixture、显式 header 环境变量白名单、超时/错误/取消、权限组合与跨进程 once 恢复已验收（详见开发记录 `http-hook-acceptance-20261018.md`）。
4. [x] Command `async`：会话托管的后台执行、跨进程一次性上下文投递、取消/超时/关闭及 Windows 进程树清理；CLI 集成与 ConPTY 验收完成（详见 `command-async-hook-acceptance-20261018.md`）。
5. [x] Command `asyncRewake`：隐含后台执行、exit 2 阻塞反馈、CLI 空闲/运行中恰好一次投递及取消/关闭/恢复隔离；CLI 集成与 ConPTY 验收完成（详见开发记录 `async-rewake-hook-acceptance-20261018.md`）。
6. [x] Prompt hook：有界单次无工具 provider 评估、事件限制、输出校验、失败/取消、once 恢复与权限组合；OpenAI/Anthropic mock 及 CLI Write 阻止集成验收完成（详见开发记录 `prompt-hook-acceptance-20261018.md`；含 built-in provider，传输重试受 hook timeout 约束）。
7. [x] Agent hook：有界多轮验证、独立只读权限上下文、结构化结果、取消/会话关闭清理及 once/组合优先级；双协议 mock 与独立 CLI 集成验收完成（详见开发记录 `agent-hook-acceptance-20261018.md`）。

未支持的选项要明确报错，不能静默接受。验证命令 hook 与 SDK 回调组合时的顺序和 deny/ask 优先级。

### P2 Hook 生命周期事件（清单 H1）

入口：`tool_hooks.rs` 的 `HookEvent`（当前 24 个）；loader 会跳过未知事件名。参考：`reference:src/entrypoints/sdk/coreTypes.ts`（27 个）。

- [x] 第一批生命周期事件：`Notification`、`StopFailure`、`Setup`、`ConfigChange`、`InstructionsLoaded`、`CwdChanged`。已接入独立 CLI 实际路径、精确 payload/matcher、输出语义、SDK 回调与类型限制；once/if/status/取消/超时复用并测试。core 595/0/7、CLI 261/0/1；transcript 仅在匹配 hook/SDK 实际执行前按 run 生成一次，失败警告不阻断分发；逐事件差异与不可触发子场景见开发记录 `lifecycle-events-acceptance-20261018.md`（ConfigChange 使用轮询/使用前检查，拒绝后阻止后续使用；不声称与原版 watcher 完全等价）。
- [x] `FileChanged`：已接入会话级 watchPaths 监视、basename matcher、精确 payload 与通知型输出；通知通过 tracing，独立 CLI 集成确认 debug-file 收到消息且 stderr 不被直接写入；本次 Windows core 613/0/7、CLI 268/0/1（通过/失败/忽略），详见 `watchpaths-filechanged-acceptance-20261018.md`。task10 已修复跨平台编译及 macOS 系统路径别名；Linux/macOS 完整 core/CLI 与真实模型冒烟已通过，见开发记录 `unix-build-acceptance-20261018.md`。
- [x] `Elicitation`、`ElicitationResult`（form 模式最小可用链路）：stdio / SDK MCP 反向请求、服务器 matcher、hook 直接 accept/decline/cancel、结果修改/阻止、requested_schema 校验、CLI/worker 交互和 SDK 控制应答；无交互默认 cancel。URL 模式及异步 elicitation hook 明确报错，不宣称支持。验收详见开发记录 `elicitation-hooks-acceptance-20261018.md`。
- 依赖 P4 团队：`TaskCreated`、`TaskCompleted`（完成前可拒绝）、`TeammateIdle`。

### P3 Hook 输出与环境（清单 H3）

入口：`crates/core/src/services/hook_callbacks.rs`（仍拒绝 async SDK 结果；watchPaths 已支持）。设计参考 `temp/claude-code-cli-dev/sdk-hook-output-parity-plan.md`。

- [x] `SessionStart.initialUserMessage`：command/HTTP/SDK 最后非空输出按实际观察顺序胜出；打印与 SDK 入口消费一次并先于调用方输入，持久化 hook 来源且不进入用户授权账本；UserPromptSubmit 正常触发一次。REPL/clear/compact 不自动提交，resume 只消费本次新输出，不重放历史。详见开发记录 `initial-user-message-acceptance-20261018.md`（含 ConPTY 与 auto classifier 隔离验收）。
- [x] `watchPaths`：会话级原生 watcher，忽略初次发现、500 ms 去抖、add/change/unlink；缺失目标监视祖先后转为目标；动态集合替换、cwd 重置与会话隔离。审查修订：空闲不递归扫描，通知/pending 驱动 200 ms 采样，缺失路径 2 s metadata 兜底；spawn_blocking 执行快照与正常关闭；扫描上限 10,000 条，超限告警并降级到已知文件；关闭确认最长 2 s，超时告警。core watcher 8/0/0，含空闲计数、超限降级、关闭超时测试；Windows 全量 core 613/0/7、CLI 268/0/1。详见 `watchpaths-filechanged-acceptance-20261018.md`。历史 Linux inotify 5/0/0、macOS FSEvents 6/0/0 仅对应修订前隔离探针，本次按要求不重跑；task10 已完成完整跨平台编译/测试及 macOS FSEvents 系统别名修复，见 `unix-build-acceptance-20261018.md`；非 Windows 会话环境文件已在 task11 验证（见 `unix-session-env-acceptance-20261018.md`）。
- [x] 会话环境文件（`CLAUDE_ENV_FILE`）：Windows 按原版返回 null；Linux/macOS 在 Setup/SessionStart/CwdChanged/FileChanged 的非 PowerShell command hook 中提供私有 0700/0600 文件，经 Bash/sh/zsh 的 `eval` 前注入，cwd 重置清空 cwdchanged/filechanged，会话结束删除目录；符号链接拒绝、会话目录名 SHA256。Windows core 618/0/7、CLI 268/0/1；Linux/macOS `session_environment` 4/4、`protected_paths` 与 `fs::tests` 通过；两平台真实模型 Read+Bash 冒烟（含环境变量与清理）通过。详见 `unix-session-env-acceptance-20261018.md`。

### P4 团队（清单 T1、T2）

入口：`crates/core/src/tools/coordination.rs`（当前 team 是持久任务记录）、`crates/core/src/native_engine/delegation.rs`（顺序执行委派）、`crates/core/src/tools/agent_messages.rs`（UUID 寻址文本消息）。参考：`reference:src/tools/TeamCreateTool/`、`TeamDeleteTool`、`SendMessageTool/SendMessageTool.ts`；原版受 `isAgentSwarmsEnabled()` 开关控制。

- [x] T1：`TeamCreate`/`TeamDelete` 按原版输入输出；团队文件与 `name@team` 成员持久化在运行时目录；`TeamSpawn`（本项目批量入口）并发运行多个命名 teammate，各自独立会话、transcript 与 fork 权限；leader 取消时成员标记 `cancelled`，重启后 active 但无对应进程的成员标记 `stale`；有活跃成员时拒绝删除并清理目录；`--agent-teams`/环境变量开关。core 625/0/7、CLI 269/0/1，含并发屏障、取消与独立 CLI 集成测试；差异（进程内并发替代 tmux、数字后缀命名、无远端 killswitch）见 `team-t1-acceptance-20261020.md`。
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
- IDE 扩展（真实扩展版本、选区、diff 接受/拒绝、断线重连）。
- Design 服务（授权、上传、渲染；浏览器测试按 deepseek-browser-use 流程）。
- 真实 LSP 服务器（rust-analyzer/TypeScript/Python，九种操作、写入后诊断、重启）。
- [x] Linux x86_64 / macOS arm64 core 与独立 CLI 构建、完整测试、真实 Anthropic 兼容接口 print / stream-json / Read 冒烟。task10 验收：Windows core 615/0/7、CLI 268/0/1；Linux core 613/0/7、CLI 267/0/1；macOS core 615/0/7、CLI 267/0/1（通过/失败/忽略）。Windows CLI/Desktop all-target check 通过；不支持的 Unix 本地截图及 macOS 按键/侧键明确报错；平台 cfg 差异、失败修复和未验证项详见开发记录 `unix-build-acceptance-20261018.md`。
- 与官方 CLI 的终端对比（同宽度/主题/模型，原始 ANSI + 最终屏幕）与响应速度（本地端点同延迟下分离启动/输入/渲染与首 token 延迟）。
- qwerty-learner issue #1095：先读 `issue1095-pass2-final-review.md` 等已有记录，目标仓库不提交。

### 暂时不做的任务
- 真实账号 OAuth、用量、fast 模式权限、反馈投递。

## 6. 每项完成后

1. 测试通过 + 必要的 ConPTY 验收 + commit。
2. 在 `temp/claude-code-cli-dev` 记录：commit、fixture/终端场景、结果、失败/取消/持久化覆盖、剩余外部要求；同步勾选清单并更新 `cli-parity-current.md` 对应行。终端逐帧快照（`frame-*`）验收后可删除，保留命名快照与 `raw-ansi.log`。
3. 若用户希望，在 `claude-code-cli-doc`（当前为空）编写面向用户的使用手册（安装、配置、常用命令、远程控制等）。
