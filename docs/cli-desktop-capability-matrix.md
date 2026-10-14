# CLI / Desktop 能力迁移矩阵

核查日期：2026-10-15。以 `legacy/claude-code-cli/src/cli/mod.rs` 和交接文档为基线。**这是 P0 盘点及 P1 实施记录，未列为完成的能力仍需后续实现/验收。**历史快照不参与功能完成计数。

| 旧 CLI 入口 | 当前 CLI / 共享实现 | Desktop 入口 / 证据 | 状态及缺项 |
|---|---|---|---|
| `repl/query/rust`、stdin、模型、权限、恢复 | `crates/cli/src/{main,session,config}.rs`、`crates/core/src/runtime.rs` | 主聊天 / 项目会话 | 已等价覆盖；既有 `standalone.rs` 5 个测试 |
| `config show/set/reset` | `crates/cli/src/config.rs` | ProviderSettings、GeneralPreferences | 已等价覆盖；旧配置显式只读导入 |
| `mcp list/add/remove` | `crates/core/src/mcp/`、CLI `main.rs` | McpSettingsPage、McpManagementPanel | 已覆盖上述命令；**CLI restart 尚缺跨进程运行实例定位**，不能通过临时启动/停止新实例假实现 |
| `skills list/search/help/execute` | `crates/core/src/skills/`、CLI `main.rs` | SkillsPage | 已接入；完整旧格式映射待审计 |
| `init` | CLI `terminal/init.rs`，不覆盖已有指令文件 | 项目创建与信任对话框 | 基础能力已覆盖；旧 SSH/remote 初始化另行审计 |
| `voice -p/--push-to-talk` | CPAL → PCM WAV → `services/voice.rs` 音频 API；文件/连续输入/共享会话 | 聊天麦克风 → VoiceInput；设置 → 服务与团队 | 实施完成，测试通过（原生硬件采集、multipart、模型会话、前端取消）；**真实 STT 与 WebView2 设备/权限尚未验收** |
| `team-sync status/auth/sync/list/create/delete` | `services/team.rs`、`team_server.rs`；CLI `services.rs` | 设置 → 服务与团队（含更新、冲突与旧数据导入） | 已迁入自部署真实 HTTP 协议；两个 CLI 客户端/独立服务进程验证；生产部署与 GUI 手工验收待完成，不提供伪造官方 OAuth |
| `services status/start/stop/team-sync/voice` | `services/lifecycle.rs`，跨进程 SQLite 租约；CLI 前台宿主，voice 显示后端配置 | 设置 → 服务与团队；宿主句柄由 Tauri 持有，退出中止并等待 | **team-sync 链路完成**；voice 是配置状态，不是后台录音宿主 |
| `services auto-dream/magic-docs/plugins/agents` | 尚未注册这些启动能力 | 尚无对应真实宿主控制 | 仍待实施；不展示不可运行的服务为成功 |
| `memory status/clear/export/import/dream/auto-dream` | 已有 `memory/{mod,local}.rs` 与模型自动检索/摄入；CLI 管理命令待接 | MemoryPanel 支持增添、搜索、清空与配置 | 部分等价覆盖；旧个人 JSON 导入、导出、整理、调度/租约仍待 P2 |
| `magic-docs list/check/update/clear` | 尚未迁入；旧标记与解析可参考 `services/magic_docs.rs` | 追踪/差异/确认应用/自动更新界面尚缺 | 仍待 P2，必须接现有 NativeEngine 与项目权限 |
| `agent --agent-type --prompt` | 已有 `multiagent/`、`orchestration/`、`agent_bus/` | AgentPanel、AgentExecutionView | 类型/别名/自定义定义及 CLI 映射待 P2；不能创建第二套工具循环 |
| `plugin list/install/remove/update/search/enable/disable` | 尚未实现正式插件安装/执行生命周期 | PluginsPage 仍是静态分类搜索 | 仍待 P3；本地 manifest/加载/撤销/回滚先行 |
| `stress-test -c/-i` | 旧 `services/stress_tests.rs` 未迁入 | 诊断入口与结果尚缺 | 仍待 P3；默认可控本地端点，付费端点显式选择 |
| `update`、`help --topic` | Clap 标准帮助已可用，专题帮助/真实版本检查尚缺 | 有 `src-tauri/src/updater.rs` 可继续核查 | 仍待 P3；标准帮助只列实际注册命令 |
| 历史数据 | provider/模型/token 上限；团队 JSON 原子导入、预览、备份、去重与源载荷记录 | 团队导入面板 / 设置 | 旧 session/个人 memory/MCP/plugin/Magic Docs 的全量导入仍待实施；旧伪 token 不导入 |

## 扩展能力审计边界

下面保留逐项审计范围；当前相近实现路径是下一阶段核查起点，**不等于已证明行为等价或无需迁移**。

| 旧目录 / 能力 | 当前可比较位置 | 当前结论 |
|---|---|---|
| `learning/{engine,experience,feedback,pattern}.rs` | `memory/caveman_rtk.rs` 的反馈/迭代 | 仍待详细格式及行为映射；禁止把现有反馈直接视为完整学习系统 |
| `performance/{error_classifier,retry_policy,rate_limiter,prompt_cache,smart_router,tool_persistence}.rs` | `native_engine/`、`cache/`、`metrics/`、`db/` | 仍待逐项审计；现有工具轨迹/模型请求存储已有测试，旧智能路由/限流/重试策略尚未等价验收 |
| `i18n/{loader,locales,translator}.rs` | `src/hooks/useI18n.ts`、`src/locales/` | 桌面语言切换已有覆盖；旧语言范围、CLI 文案、复数/日期/方向行为仍待审计 |
| `tools/advanced/{agent,delegation,worker,team,send_message,escalation}.rs` | `tools/`、`multiagent/`、`agent_bus/`、worker | 仍待逐项参数和权限/生命周期映射 |
| `tools/advanced/{bash,powershell,worktree,web_tools,notebook,lsp,tool_search}.rs` | `tools/`、`worktree/`、`ide/`、`research/` | 仍待审计；已共享 Bash 的取消生命周期，不能据此宣称其余等价 |
| `tools/advanced/{task_tools,todo_write,cron,skill,mcp_tools,mcp_bridge,config,brief,ask_question,permission_tools,plan_mode}.rs` | `task/`、`scheduler/`、`skills/`、`mcp/`、`ask_user/`、`permissions/` | 仍待逐项协议、参数、持久化和交互核查 |
| `advanced/{ssh,remote,project_init}.rs` | 项目与共享工具 | 仍待审计 |
| `gui/`（会话、设置、主题、高亮、侧栏、工具轨迹） | React 当前产品、SQLite 会话、既有前端测试 | 基础产品已有对应形态；独有操作待逐项核查，不重新构建 egui |
| `web/`（路由、模型、模板）、`wasm/`（bridge/client/storage） | Tauri Bridge / React、`runtime.rs` | 基础传输已有对应形态；离线存储/导入导出等独有能力待逐项核查，不据技术栈差异免除迁移 |

P0 已交付共享服务上下文（RuntimePaths/数据库/凭据）、结构化状态、增量数据库迁移、跨进程租约和完整 team-sync 运行/停止链路。模型、工具、权限、会话继续复用原 NativeEngine。P1 团队记忆是明确分享的独立范围，个人 MemEx 数据不会自动上传；团队记忆进入模型上下文的行为尚未实现。

自动化证据：`crates/core/tests/peripheral_services.rs`、`crates/cli/tests/peripheral.rs`、`src/components/{VoiceInput,ServicesSettings}.test.tsx`。Windows 原生设备证据：显式运行 `native_microphone_capture_releases_the_device -- --ignored`；这不是识别准确率或 WebView2 权限验收。
