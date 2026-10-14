# CLI / Desktop 外围功能迁移交接

更新日期：2026-10-14。本文是后续实施清单，未勾选项目尚未完成。

## 1. 用户目标与约束

用户明确要求：**旧 CLI 的语音、团队同步等外围功能必须在 `claude-rust-desktop` 中具备真实可用的实现，不能以保留历史源码代替功能迁移。**

后续按共享业务实现、CLI 命令和 Desktop 图形入口一起交付。旧前端的具体技术栈无需原样搬入，但旧功能需要逐项映射，不能遗漏。当前 session 只整理交接文档，功能实现留给新 session。

- 工作仓库：`D:\code\project\project-harven\claude-rust-desktop`。
- 已创建并正在使用的分支：`feat/shared-core-cli-desktop`。
- 旧仓库：`D:\code\project\project-harven\claude-code-cli`，**只读，不修改、不提交、不清理**。
- 历史源提交：`36a6231ac3134e01c6d9dacafb2094af7afbbe7c`。
- 本文初稿时拆分和整合改动尚未提交，随后用户要求提交；新 session 以实际 Git 状态和提交记录为准。必须保留已完成的工作，不能 reset/clean，也不要重新从旧分支开始。
- `legacy/claude-code-cli` 是 163 个文件的历史源码快照，逐字节核对与来源一致，不属于发布 workspace。后续从这里参考移植，历史快照保持可核对。
- 核心不依赖 Tauri / AppHandle；CLI 独立运行，不要求 Desktop 或 30085 服务启动。需要远程服务的功能可以调用其配置的远程端点。

## 2. 已完成的基础，不要重复拆分

| 位置 | 已有职责 |
|---|---|
| `Cargo.toml`、`Cargo.lock`、`.cargo/config.toml` | Workspace；构建缓存在 `src-tauri/target` |
| `crates/core` (`claude-core`) | 共享模型、工具、权限、配置、SQLite、会话、MCP、技能等 |
| `crates/core/src/runtime.rs` | 两端共用引擎创建、provider 转换、数据路径、会话与工具历史恢复 |
| `crates/cli` (`claude-code-cli`) | 独立 `claude-code` 可执行文件 |
| `src-tauri` | Tauri、React 的后端适配、Bridge、凭据等；库重导出共享核心 |
| `src-tauri/worker` | 计算机操作辅助进程，仍不是 CLI |
| `scripts/build-all.mjs` | 一次构建 CLI、Desktop、安装包，复制 provider runtime |

新 CLI 已接入 query、REPL、stdin、text/json/stream-json、provider/model 选择、权限模式、会话恢复、配置、MCP list/add/remove、skills、init。旧配置导入目前只迁移 provider、模型与 token 上限。使用说明见 [cli.md](cli.md)。

默认使用 Desktop 的 `com.claude.desktop` 数据目录；支持 `--data-dir` 隔离。自定义 OpenAI/Anthropic 协议是原生 Rust；内置 provider harness 仍需要 Node.js 22+ 和 `runtime/provider-runtime.mjs`。

已有取消和终端交互修复：答案等待器先注册再发事件；取消会中止并等待执行任务；Bash 子进程使用 `kill_on_drop(true)`。后续功能要复用该生命周期，不能引入取消后继续执行的后台任务。

## 3. 代码核查发现：不能直接把旧实现当成完成品

以下结论来自源码检查，不代表旧仓库经过完整运行验收。

| 功能 | 当前证据 | 迁移含义 |
|---|---|---|
| 语音 | `legacy/claude-code-cli/src/services/voice.rs` 的 start 只变更状态、清空 buffer；transcribe 向文本聊天发送 `[Audio data: N bytes]`。`src/voice/mod.rs` 还有识别 TODO | 必须补真实采集与音频识别接口，不能复制后宣称可用 |
| 团队同步 | `services/team_memory_sync.rs` 的 auth 本地拼 token；fetch 清空内存，upload 只打印日志，没有远程请求 | 需要真实认证、同步协议与存储；不能沿用伪认证成功 |
| 插件市场 | `services/plugin_marketplace.rs` 的市场条目是硬编码数据，安装需进一步核查包获取与执行链路 | 需要真实插件源、加载执行和生命周期验收 |
| 服务管理 | `services/mod.rs` 的 start_all 主要输出状态，stop_all 主要保存文档状态 | 必须补任务启动、停止、退出和跨进程控制语义 |
| Magic Docs | `services/magic_docs.rs` 已有文档标记识别、模型请求和写文件，但依赖旧 ApiClient | 复用解析逻辑，改走共享引擎、项目权限和受控写入 |
| CLI 更新 | `src/cli/args.rs` 的 run_update 直接打印 Already at latest version | 必须接真实版本检查，错误时不能报告已是最新 |

上述 `services/` 和 `src/` 相对路径均属于 `legacy/claude-code-cli/src/services/` 和 `legacy/claude-code-cli/src/`；语音 TODO 在 `legacy/claude-code-cli/src/voice/mod.rs`。

Desktop 已有可复用入口，也要避免重复建设：

- `src/components/VoiceInput.tsx`、`src/features/voice/useVoiceInput.ts`：浏览器 SpeechRecognition / MediaRecorder 相关实现，尚不能据此认定原生 CLI 语音已支持或 WebView2 下已验收。
- `src/components/PluginsPage.tsx`：目前是静态分类及搜索，不是插件安装、运行管理器。
- `src/components/MemoryPanel.tsx`、`crates/core/src/memory/`：已有记忆能力，需做格式与行为映射。
- `src/components/AgentPanel.tsx`、`AgentExecutionView.tsx`，`crates/core/src/multiagent/`、`orchestration/`、`agent_bus/`：先评估复用，避免增加第二套 agent 执行循环。
- `src-tauri/src/bridge/routes/sync.rs`：已有 provider / conversation / message 同步路由，**不等同于团队记忆同步**。不能直接将含 provider 密钥的同步载荷用于团队分享。

## 4. 待实施功能及验收标准

下面 CLI 子命令以旧 `legacy/claude-code-cli/src/cli/mod.rs` 为基线。参数可在保持兼容的前提下扩充。每一行必须有实际实现、CLI 接入、Desktop 可发现入口及测试证据。

| 状态 | 功能 / CLI 兼容面 | 实现与桌面要求 | 最低验收 |
|---|---|---|---|
| [ ] | `voice`、`--push-to-talk`，连续输入 | 核心定义音频/转写接口；平台适配负责设备；Desktop 接现有语音入口，显示录音、转写、失败和取消状态 | 实际音频字节送达识别后端并返回文字；CLI/桌面均可把文字交给当前会话；无麦克风、拒绝权限、网络失败、取消时正确释放设备 |
| [ ] | `team-sync status/auth/sync/list/create/delete` | 团队设置、真实凭据、团队隔离、记忆 CRUD、增量同步、冲突处理、删除标记、重试；Desktop 提供团队与冲突处理界面 | 两个隔离客户端通过独立服务端同步新增/修改/删除；离线后恢复；认证失败、冲突、重复同步均正确；不上传模型密钥 |
| [ ] | `services status/start/stop/auto-dream/voice/magic-docs/team-sync/plugins/agents` | 共享服务管理器；任务句柄、取消、状态、错误、持久化、重复启动保护；Desktop 服务页控制真实运行实例 | start 后任务确实工作，stop 后确实停止；两个终端命令能控制同一实例；退出无遗留任务；CLI 不依赖桌面进程 |
| [ ] | `memory status/clear/export/import/dream/auto-dream` | 复用现有记忆存储；旧 JSON 导入映射；手动整理与自动调度；Desktop 记忆页增加缺失操作和整理状态 | 导入可重复执行不重复数据；导出再导入保留内容；清理只影响指定范围；整理结果参与后续模型上下文；双端同时运行不会重复整理 |
| [ ] | `magic-docs list/check/update/clear` | 保留标记识别、注册与状态；共享引擎生成更新；Desktop 展示追踪列表、预览差异、确认应用及自动更新开关 | header 保留；重启仍能列出；只写授权项目文件；模型失败/取消不损坏原文；clear 明确只清追踪记录 |
| [ ] | `plugin list/install/remove/update/search/enable/disable` | 插件 manifest、真实来源、本地安装与加载、hook/命令/工具注册、版本与启停；Desktop 将静态分类页接成真实管理器 | 安装测试插件后能在两端产生实际功能；禁用/卸载立即撤销注册；重启状态保留；失败安装不留下半成品；远端不可用有明确错误 |
| [ ] | `agent --agent-type ... --prompt ...`：guide/explore/plan/verify/general 及原别名 | 内置定义映射共享执行器；自定义定义加载、会话、事件、权限、取消；Desktop 支持选择、运行、查看结果 | 各类型正确加载定义，真实走共享模型及工具；执行轨迹、权限与取消有效；不另起旧 ApiClient 工具循环 |
| [ ] | `stress-test --concurrency --iterations` | 参考 `services/stress_tests.rs`，迁入开发诊断模块；Desktop 在诊断页提供入口和结果 | 真实统计请求数、并发、成功/失败、耗时；默认本地可控服务；真实付费端点必须显式选择；不得将结果宣称为官方 Claude CLI/Desktop 性能对比 |
| [ ] | `mcp restart`、更新检查、专题帮助与兼容参数 | 新 CLI 缺少旧 restart 子命令；复用共享 MCP；更新复用 Desktop 可用部分并核查真实性；补帮助与旧命令兼容说明 | restart 操作实际运行实例；不存在的服务明确报错；更新按真实响应判断；`--help` 只列真实实现能力 |
| [ ] | 旧数据、扩展能力全量映射 | 补旧 session/memory/MCP/plugin/team/Magic Docs 数据导入；核查 `learning/`、`performance/`、`i18n/`、`tools/advanced/` 与旧 GUI/Web/WASM 交互功能 | 每项标为“已等价覆盖/已迁入/仍待实现”，附当前实现路径和证据；不能未经核对标为无需迁移 |

最后一项是防遗漏审计：保留现有 React + Tauri 产品形态，不要求重新打包旧 egui/Web/WASM 三套前端；其中独有的用户能力仍需迁入当前产品。若确需增加独立发布目标，应单独记录范围与依据。

## 5. 推荐实现顺序和代码落点

这些是实施建议，不是已经存在的新增文件，也不要求一次引入全部依赖。

1. **P0：盘点和公共服务基础。** 对照上表补完整子命令/功能矩阵；确认现有 Desktop 等价能力。设计服务上下文（RuntimePaths、数据库、共享引擎、权限、事件、凭据接口）、生命周期与数据库迁移。先完成运行/停止/状态的一条完整链路。
2. **P1：语音与团队同步。** 这是用户点名的功能，优先形成 CLI + Desktop 两端可用的闭环。团队同步需要的最小记忆存储和迁移在此阶段完成。
3. **P2：记忆整理、Magic Docs、agents。** 接共享引擎和调度，复用 Desktop 已有页面，补历史数据导入。
4. **P3：插件与剩余兼容面。** 接真实插件执行、MCP restart、诊断压测、更新检查、帮助；完成全量能力映射审计。
5. **P4：集成验收和交付。** 按下面检查项补测试、运行真实 Windows GUI/设备验收，重新产出发布包。

建议业务模块放入 `crates/core/src/services/` 或独立 workspace crate；较重音频/插件平台依赖可做适配 crate，但默认发布产物必须包含承诺功能。CLI 在 `crates/cli/src/args.rs` 注册，按领域拆文件，避免继续膨胀 main。Desktop 的 Tauri commands / Bridge 路由只做适配，业务规则不复制；前端在现有设置、记忆、agent、语音入口上扩展。

实现前须解决的具体选择：

- **语音后端：** 旧项目没有可直接使用的真实 STT 实现。选定音频采集与本地/远程识别适配，记录模型、配置、依赖与安装方式；普通文本聊天 API 不能代替音频 API。
- **团队远端：** 旧项目没有真实团队同步服务。先检查当前仓库是否存在可用协议和服务端；没有则提供可配置远端协议与可本地启动的最小服务端，配两客户端集成测试。生产端点/部署凭据未提供时记录为部署前置项，不能伪造官方认证接口或报告云端已可用。独立团队服务端不等于要求 CLI 启动 Desktop。
- **后台服务：** `services start` 若退出后继续运行，需独立宿主进程、本地 IPC、PID/锁及停止协议；可选择 start 默认前台常驻、显式参数后台化，但必须写清行为。Desktop 自己持有服务时，两端仍应通过锁/租约避免自动同步、记忆整理重复执行。不要把现有 `agent-worker` 当通用服务宿主。
- **凭据：** 共享核心定义接口，宿主实现系统凭据或环境变量获取；核心不能依赖 AppHandle。旧伪造的 `.team_token` 不可直接导入为有效凭据。
- **插件来源：** 先有可验证的本地插件格式、加载闭环，再接可配置远端源；不能把硬编码目录当成真实市场。插件文件路径、权限、卸载范围和失败回滚需覆盖。

外部服务选型未定时先推进不依赖它的接口、存储、UI 和本地测试，记录实际缺项，不把假数据作为交付实现。

## 6. 数据和并发要求

- 统一从 `RuntimePaths` 取路径，新代码不要继续硬编码写入 `~/.claude-code`。旧目录仅在显式导入时读取。
- 导入前备份目标，支持预览和迁移报告；源数据保持不变，失败回滚，重复导入幂等；明确项目/会话 ID、工具历史和团队归属映射。
- CLI 与 Desktop 可同时使用数据库；新增表/字段使用版本化 migration，检查写竞争、唯一约束和任务租约。不要只用进程内布尔值防重。
- 同步需要版本/etag、删除标记和冲突记录；网络调用不要持有会被内部回调再次取得的读写锁。旧 team sync 在持锁遍历中调用可能重新加锁的函数，迁移时重点修复。
- Magic Docs、插件安装等文件写入遵循项目范围和共享权限；修改失败不留下损坏文件。
- 核心输出结构化事件和错误，CLI 将进度发 stderr、结果发 stdout；Desktop 渲染相同状态，防止两套状态各自假成功。

## 7. 已有验证基线与后续验收

以下是上一轮完成记录，不是新增外围功能的验证结果；新 session 如发现代码变化，以重新执行的结果为准。

- 核心单元测试：138 通过，2 忽略；取消集成测试：1 通过。
- CLI 集成测试：5 通过，使用临时目录、本机模拟模型，覆盖工具读写、恢复、权限失败、流式输出和只读导入。
- Desktop Rust 测试：49 通过。合计 193 通过、2 忽略。
- CLI 依赖树无 Tauri/Wry/WebKit；Windows x64 Release CLI、Desktop、MSI、NSIS 构建成功。
- 最后产物：`artifacts/3.1.0-release-2026-10-14T15-08-06-383Z/`；执行文件与安装包哈希核对一致。这些产物**不包含本文待办功能**。
- 尚无新增外围功能的真实麦克风、远程团队服务或 GUI 手工验收，也没有 macOS/Linux 验收结论。

相关命令：

```powershell
git status --short
git branch --show-current
git -C D:\code\project\project-harven\claude-code-cli status --short

cargo test --locked -p claude-core --lib --tests
cargo test --locked -p claude-code-cli
cargo check --locked -p claude-desktop-tauri --all-targets
cargo test --locked -p claude-desktop-tauri --lib --tests
cargo tree -p claude-code-cli
npm run test:run
npm run build:all
git diff --check
```

日常先运行受改动影响的测试；新增依赖时正常更新并保留根 Cargo.lock，再用 `--locked` 验证。最终交付需跑相关完整检查。Windows PowerShell 将原生命令 stderr 重定向时可能产生误导性的外层退出状态；检查实际 cargo 测试结果，必要时用 Python subprocess 记录真实退出码。

最终完成条件：

- [ ] 上表所有功能有实现路径、CLI 用例、Desktop 操作入口和验证结果；源码快照不计为功能完成。
- [ ] CLI 在没有 Desktop 进程/30085 服务的环境中能使用新增功能；两端共享规则及持久化格式。
- [ ] 无真实后端/设备时明确显示不可用原因；不能假登录、假转写、假同步、假安装。
- [ ] 语音有实际设备验收；团队同步有两客户端与独立服务端端到端验收；插件有实际执行结果。
- [ ] 重启、同时运行、取消、离线、失败与数据迁移已覆盖。
- [ ] 更新 `docs/cli.md` 的兼容边界和使用说明，本文逐项勾选并附证据。
- [ ] 新构建包包含运行依赖，记录版本/平台/依赖要求；原 CLI 仓库仍未修改。

## 8. 可直接发给新 session 的任务

```text
继续在 D:\code\project\project-harven\claude-rust-desktop 工作。
先读 docs/cli-desktop-migration-handoff.md 和 docs/cli.md，检查 Git 状态。
现有 feat/shared-core-cli-desktop 分支中已有 workspace 拆分和 CLI 整合，先检查提交记录及未提交改动，保留这些工作，不要重置或重做。
原 D:\code\project\project-harven\claude-code-cli 仓库只读。

要求：将旧 CLI 的语音、团队同步、后台服务、记忆/AutoDream、Magic Docs、插件、agent、压测和其余缺失能力真正接入当前仓库，并提供 CLI 和 Desktop 入口。仅保存 legacy 源码不算完成。
按交接文档的 P0–P4 顺序实施，优先形成语音和团队同步闭环。复用共享核心，CLI 不依赖 Desktop 或 30085，不能恢复第二套旧 ApiClient/工具引擎。
旧语音、团队同步、插件市场有未完成/占位实现，需补真实功能，不能原样复制后报告可用。
每完成一项更新交接文档，记录实现路径、测试和仍缺的外部配置。完成后跑相关测试与统一构建，说明尚未验收的平台或外部环境。
```
