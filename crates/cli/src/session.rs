use crate::{
    args::{Cli, OutputFormat, PermissionModeArg},
    config::Selection,
};
use anyhow::{bail, Context, Result};
use claude_core::{
    cache::FileCache,
    db::message_repo,
    mcp::{McpServerManager, McpToolRegistry},
    native_engine::{
        engine_core::ChatRequest, provider_manager::ProviderManager, tool_loop::EngineEvent,
        NativeEngine,
    },
    permissions::PermissionMode,
    runtime::{self, RuntimePaths, SessionStore},
};
use serde_json::{json, Value};
use std::{
    io::{self, Write},
    sync::Arc,
};
use tokio::sync::Mutex;
use crate::terminal::input::TerminalInput;

/// One stdin reader for the entire REPL, also used for tool questions. A detached
/// OS thread avoids Tokio's uncancellable stdin read blocking runtime shutdown.
pub fn terminal_input() -> TerminalInput {
    TerminalInput::new()
}

pub struct Session {
    pub(crate) sdk_permissions: Option<Arc<crate::sdk::Permissions>>,
    pub(crate) sdk_content: Option<Value>,
    pub id: String,
    pub store: SessionStore,
    pub engine: NativeEngine,
    pub(crate) paths: RuntimePaths,
    pub(crate) permissions: Arc<claude_core::permissions::PermissionManager>,
    pub(crate) ide: Arc<claude_core::services::ide::IdeClient>,
    mcp: Arc<McpServerManager>,
    mcp_ready: tokio::sync::OnceCell<()>,
    pub(crate) model: String,
    pub(crate) provider_id: String,
    pub(crate) auto_route: bool,
    permission_mode: PermissionModeArg,
    providers: Arc<Mutex<ProviderManager>>,
    pub(crate) workspace: std::path::PathBuf,
    api_format: claude_core::native_engine::provider_manager::ApiFormat,
}

impl Session {
    pub(crate) async fn sdk_set_model(&mut self, name: &str) -> Result<()> {
        let providers=self.providers.lock().await;
        let (provider,model)=name.split_once("::").unwrap_or((&self.provider_id,name));
        let resolved=providers.resolve_selected(provider,model).context("SDK model is not configured")?;
        let selection=Selection {providers:providers.list_providers().to_vec(),provider_id:resolved.provider.id,model:model.to_owned()};
        drop(providers);
        let paths=self.paths.clone();
        self.switch(&paths,selection).await
    }
    pub async fn selected_provider(&self) -> Result<claude_core::native_engine::provider_manager::ResolvedProvider> {
        self.providers.lock().await.resolve_selected(&self.provider_id, &self.model).context("Selected provider/model is unavailable")
    }

    pub async fn set_fast(&self, enabled: bool) -> Result<()> {
        self.providers.lock().await.set_model_fast(&self.provider_id, &self.model, enabled)
    }

    pub async fn reload_plugins(&self) -> Result<()> {
        let context = crate::services::context(&self.paths)?;
        if self.mcp_ready.get().is_some() { claude_core::services::plugins::sync_mcp(&context, &self.mcp).await?; }
        claude_core::tools::configure_lsp_servers(
            &self.workspace.to_string_lossy(),
            claude_core::services::plugins::language_servers(&context)?,
        )?;
        Ok(())
    }

    async fn ensure_mcp(&self) -> Result<()> {
        self.mcp_ready.get_or_try_init(|| async {
            self.mcp.initialize().await?;
            for server in self.mcp.list_servers().await.into_iter().filter(|server|server.enabled && !server.running) {
                if let Err(error) = self.mcp.start_server(&server.id).await { eprintln!("MCP {}: {error}",server.name); }
            }
            Ok::<(), anyhow::Error>(())
        }).await?;
        Ok(())
    }

    pub async fn context_window(&self) -> Option<u32> {
        self.providers.lock().await.resolve_selected(&self.provider_id, &self.model)
            .and_then(|resolved| resolved.model.context_window)
    }
    pub async fn open(
        cli: &Cli,
        paths: &RuntimePaths,
        mut selection: Selection,
        resumed_id: Option<&str>,
    ) -> Result<Self> {
        anyhow::ensure!(
            !cli.fork_session || resumed_id.is_some(),
            "--fork-session requires --resume or --continue"
        );
        cli.system_prompt_text()?;
        claude_core::config::OrchestrationConfig::init_global()?;
        let db = paths.open_database()?;
        let store = SessionStore::new(db.clone());
        let id = if let Some(id) = resumed_id {
            store.resume(id)?;
            id.to_owned()
        } else {
            store.create_with_id(
                &std::env::current_dir()?,
                &selection.model,
                &selection.provider_id,
                &cli.session_id.clone().unwrap_or_else(||uuid::Uuid::new_v4().to_string()),
            ).context("Creating CLI session")?
        };
        let id = if cli.fork_session {
            store.fork(&id)?
        } else {
            id
        };
        let internal_worker = claude_core::services::background_sessions::worker_directory(&paths.data_dir, &id)?.is_some();
        let auto_route = if internal_worker { std::env::var("CLAUDE_RUST_WORKER_AUTO_ROUTE").as_deref() == Ok("true") } else { cli.auto_route };
        if internal_worker {
            if let Ok(value) = std::env::var("CLAUDE_RUST_WORKER_MODEL") {
                let model: claude_core::native_engine::provider_manager::ModelConfig = serde_json::from_str(&value)?;
                let provider = selection.providers.iter_mut().find(|provider|provider.id == selection.provider_id).context("Worker provider unavailable")?;
                let expected=claude_core::native_engine::provider_manager::resolve_model_selection(&provider.models,&selection.model).context("Worker model unavailable")?;
                anyhow::ensure!(model.id == expected.id, "Worker model snapshot does not match the selected model");
                let existing = provider.models.iter_mut().find(|existing|existing.id == model.id).context("Worker model unavailable")?;
                *existing = model;
            }
        }
        let workspace = store.workspace(&id, &paths.data_dir.join("workspaces"))?;
        store.db.with_conn(|conn| {
            conn.execute(
                "UPDATE conversations SET model = ?1, provider = ?2 WHERE id = ?3",
                [&selection.model, &selection.provider_id, &id],
            )
        })??;
        let permissions = runtime::default_permissions();
        if let Some(crate::args::Commands::Agent { agent_type, .. }) = &cli.command {
            let definition = claude_core::services::agents::resolve(&crate::services::context(paths)?, agent_type)?;
            claude_core::services::agents::bind(&store.db, &id, &definition)?;
        }
        permissions.set_mode(match cli.permission_mode {
            PermissionModeArg::Ask => PermissionMode::AskPermissions,
            PermissionModeArg::AcceptEdits => PermissionMode::AcceptEdits,
            PermissionModeArg::Plan => PermissionMode::PlanMode,
            PermissionModeArg::Bypass => PermissionMode::BypassPermissions,
        });
        let api_format = selection
            .providers
            .iter()
            .find(|p| p.id == selection.provider_id)
            .context("Selected provider unavailable")?
            .api_format
            .clone();
        let providers = Arc::new(Mutex::new(ProviderManager::in_memory(
            selection.providers,
            paths.data_dir.join("providers.json"),
        )));
        let mcp = Arc::new(McpServerManager::new(
            paths.data_dir.join("mcp-servers.json"),
        ));
        let ide = Arc::new(claude_core::services::ide::IdeClient::default());
        let engine = runtime::create_engine(
            paths,
            db,
            providers.clone(),
            permissions.clone(),
            Arc::new(FileCache::new()),
        )
        .with_mcp_registry(Arc::new(McpToolRegistry::new(mcp.clone()).with_ide(ide.clone())))
        .with_skills_manager(Arc::new(Mutex::new(
            claude_core::skills::SkillsManager::with_path(paths.data_dir.join("skills")),
        )));
        Ok(Self {
            permissions,
            ide,
            paths: paths.clone(),
            id,
            store,
            engine,
            mcp,
            mcp_ready: tokio::sync::OnceCell::new(),
            model: selection.model,
            provider_id: selection.provider_id,
            providers,
            workspace,
            auto_route,
            permission_mode: cli.permission_mode,
            sdk_permissions: None,
            sdk_content: None,
            api_format,
        })
    }

    pub async fn switch(&mut self, paths: &RuntimePaths, selection: Selection) -> Result<()> {
        let api_format = selection
            .providers
            .iter()
            .find(|p| p.id == selection.provider_id)
            .context("Selected provider unavailable")?
            .api_format
            .clone();
        let next =
            ProviderManager::in_memory(selection.providers, paths.data_dir.join("providers.json"));
        anyhow::ensure!(
            next.resolve_selected(&selection.provider_id, &selection.model)
                .is_some(),
            "Selected model unavailable"
        );
        let mut manager = self.providers.lock().await;
        self.store.db.with_conn(|conn| {
            conn.execute(
                "UPDATE conversations SET model = ?1, provider = ?2 WHERE id = ?3",
                [&selection.model, &selection.provider_id, &self.id],
            )
        })??;
        *manager = next;
        self.model = selection.model;
        self.provider_id = selection.provider_id;
        self.api_format = api_format;
        self.auto_route = false;
        Ok(())
    }

    pub fn clear(&mut self) -> Result<String> {
        let id = self.store.clear(&self.id)?;
        crate::session_commands::copy_settings(&self.store.db, &self.id, &id)?;
        self.id = id.clone();
        Ok(id)
    }

    /// Validate the project and provider before replacing the current session.
    pub async fn resume_saved(&mut self, paths: &RuntimePaths, id: &str) -> Result<()> {
        let saved = self.store.resume(id)?;
        let workspace = self.store.workspace(id, &paths.data_dir.join("workspaces"))?;
        let model = saved.model.context("Saved session has no model")?;
        let provider_id = saved.provider.context("Saved session has no provider")?;
        let resolved = self.providers.lock().await.resolve_selected(&provider_id, &model)
            .context("Saved provider/model is unavailable; current session retained")?;
        self.workspace = workspace;
        self.api_format = resolved.provider.api_format;
        self.model = model;
        self.provider_id = provider_id;
        self.auto_route = false;
        self.id = id.to_owned();
        Ok(())
    }

    pub async fn branch_saved(&mut self, paths: &RuntimePaths, title: &str) -> Result<String> {
        anyhow::ensure!(title.chars().count() <= 200, "Session titles must be at most 200 characters");
        let id = self.store.fork(&self.id)?;
        crate::session_commands::copy_settings(&self.store.db, &self.id, &id)?;
        if !title.is_empty() {
            self.store.db.with_conn(|conn| {
                claude_core::db::conversation_repo::update_conversation_title(conn, &id, title)
            })??;
        }
        self.resume_saved(paths, &id).await?;
        Ok(id)
    }

    pub fn permission_mode(&self) -> &'static str {
        match self.permission_mode {
            PermissionModeArg::Ask => "ask",
            PermissionModeArg::AcceptEdits => "accept-edits",
            PermissionModeArg::Plan => "plan",
            PermissionModeArg::Bypass => "bypass",
        }
    }

    pub async fn set_permission_mode(&mut self, value: &str) -> Result<()> {
        use clap::ValueEnum;
        let mode = PermissionModeArg::from_str(value, false)
            .map_err(|_| anyhow::anyhow!("Unknown permission mode '{value}'; use ask, accept-edits, plan, or bypass"))?;
        self.engine.set_permission_mode(match mode {
            PermissionModeArg::Ask => PermissionMode::AskPermissions,
            PermissionModeArg::AcceptEdits => PermissionMode::AcceptEdits,
            PermissionModeArg::Plan => PermissionMode::PlanMode,
            PermissionModeArg::Bypass => PermissionMode::BypassPermissions,
        }).await;
        self.permission_mode = mode;
        Ok(())
    }

    pub async fn close(&self) -> Result<()> {
        self.engine.cancel_turn(&self.id).await;
        self.ide.disconnect().await;
        claude_core::tools::shutdown_lsp_servers().await;
        self.mcp.shutdown_all().await
    }

    pub async fn turn(
        &self,
        cli: &Cli,
        prompt: &str,
        input: &mut Option<TerminalInput>,
    ) -> Result<()> {
        use std::io::IsTerminal;
        if self.sdk_content.is_some() {return self.run_turn(cli,prompt,input).await;}
        let prepared = tokio::select! {
            prepared = crate::skill_commands::prepare_invocation(cli, self, prompt, input) => prepared?,
            _ = tokio::signal::ctrl_c() => bail!("Skill invocation cancelled"),
        };
        let expanded;
        let prompt = match prepared {
            crate::skill_commands::PreparedInvocation::NotHandled => prompt,
            crate::skill_commands::PreparedInvocation::Inline(value) => { expanded = value; expanded.as_str() },
            crate::skill_commands::PreparedInvocation::Background(job) => {
                return crate::skill_commands::finish_background(cli, self, job, input).await;
            },
        };
        if !crate::terminal::screen_reader() && input.is_some() && cli.output_format == OutputFormat::Text && !cli.print && !cli.print_short
            && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() && std::io::stderr().is_terminal()
            && std::env::var("TERM").as_deref() != Ok("dumb") && !crate::display_commands::load(self)?.focus {
            return crate::worker_frontend::turn(cli, self, prompt, input).await;
        }
        let mut next = prompt.to_owned();
        for attempt in 0..=3 {
            self.run_turn(cli, &next, input).await?;
            // Structured output has one result per submitted query; autonomous
            // continuation is restricted to the interactive text frontend.
            if (input.is_none() || cli.output_format != OutputFormat::Text) && std::env::var_os(claude_core::services::background_sessions::CONTROL_ENV).is_none() {
                break;
            }
            let followup = tokio::select! {
                result = crate::session_commands::goal_check(self) => result?,
                _ = tokio::signal::ctrl_c() => bail!("Goal check cancelled"),
            };
            let Some(followup) = followup else { break };
            if attempt == 3 {
                eprintln!("Goal remains active. Automatic continuation limit reached; use /goal status to review.");
                break;
            }
            eprintln!("Continuing toward the session goal ({}/3). Ctrl+C interrupts.", attempt + 1);
            next = followup;
        }
        Ok(())
    }

    async fn run_turn(
        &self,
        cli: &Cli,
        prompt: &str,
        input: &mut Option<TerminalInput>,
    ) -> Result<()> {
        if let Err(error) = crate::ide_commands::restore(self).await {
            eprintln!("IDE connection unavailable: {error:#}");
            self.ide.disconnect().await;
        }
        self.ensure_mcp().await?;
        self.reload_plugins().await?;
        crate::session_commands::prepare_turn(self, self.context_window().await).await?;
        self.permissions.set_session_rules(&self.id, crate::permission_commands::load(self)?);
        let mut history = self.store.model_history(&self.id, &self.api_format)?;
        history.push(json!({"role":"user","content":self.sdk_content.as_ref().cloned().unwrap_or(json!(prompt))}));
        let mut system = cli.system_prompt_text()?;
        system.push_str(&crate::display_commands::style_prompt(self)?);
        system.push_str(&crate::ide_commands::selection_prompt(self).await?);
        if let Some(goal) = crate::session_commands::goal_prompt(self)? {
            system.push_str(&goal);
        }
        system.push_str("\nThe user is working in the standalone Claude Rust terminal frontend. No Desktop window or Bridge server is running. Use the available native tools directly.\n");
        let cwd = &self.workspace;
        if self.sdk_content.is_some() {anyhow::ensure!(self.selected_provider().await?.model.supports_vision,"Selected model does not accept Image input");}
        let user_message_id = match &self.sdk_content {Some(content)=>self.store.append_user_content(&self.id,prompt,content),None=>self.store.append(&self.id,"user",prompt)}.context("Persisting CLI user message")?;
        let mut checkpoint = match claude_core::services::checkpoints::capture(&self.paths.data_dir, &self.id, &user_message_id, cwd) {
            Ok(capture) => Some(capture),
            Err(error) => { if input.is_some() { eprintln!("Code checkpoint unavailable: {error:#}"); } None },
        };
        let interactive_display = input.is_some() && cli.output_format == OutputFormat::Text && !cli.print && !cli.print_short && !crate::display_commands::load(self)?.focus;
        let request = self
            .engine
            .send_message_with_history(
                ChatRequest {
                    conversation_id: self.id.clone(),
                    messages: history,
                    model: self.model.clone(),
                    provider_id: if self.auto_route { None } else { Some(self.provider_id.clone()) },
                    effort: crate::session_commands::effective_effort(self, cli.effort.as_deref())?,
                    system_prompt: Some(system),
                    max_tokens: cli.max_tokens,
                    max_turns: cli.max_turns,
                    response_schema: cli.response_schema()?,
                    workspace_path: Some(cwd.to_string_lossy().into_owned()),
                    temperature: None,
                    top_p: None,
                    reasoning_mode: None,
                },
                false,
            );
        let worker_cancel = claude_core::services::background_sessions::wait_for_cancellation();
        tokio::pin!(worker_cancel);
        let mut events = tokio::select! {
            _ = &mut worker_cancel => bail!("Background task cancelled before model request"),
            result = request => result?,
            _ = tokio::signal::ctrl_c() => bail!("Interrupted before model request"),
        };
        let mut progress = crate::terminal::live::TurnDisplay::new(interactive_display, self.permission_mode(), &self.paths.data_dir.join("keybindings.json"), self.store.history(&self.id)?.iter().filter(|row|row["role"]=="user").filter_map(|row|row["content"].as_str().map(str::to_owned)).collect())?;
        let mut side_request: Option<std::pin::Pin<Box<dyn std::future::Future<Output = Result<String>> + Send + '_>>> = None;
        let mut text = String::new();
        let mut thinking_text = String::new();
        let mut usage = json!({});
        let mut error = None;
        let mut completed = false;
        let mut snapshots = Vec::<Value>::new();
        let mut tools = Vec::<message_repo::ToolCallRow>::new();
        let mut refresh = tokio::time::interval(std::time::Duration::from_secs(1));
        refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let turn_result: Result<()> = async {
            loop {
                let event = tokio::select! {
                    event = events.recv() => event.context("Engine stream closed before completion")?,
                    _ = &mut worker_cancel => bail!("Background task cancelled"),
                    _ = tokio::signal::ctrl_c() => bail!("Interrupted"),
                    _ = refresh.tick() => {
                        progress.tick();
                        if interactive_display {
                            for notice in crate::background_commands::collect_completed(self)? { progress.notice(&notice); }
                        }
                        continue;
                    },
                    action = progress.action() => {
                        match action.context("Live terminal input closed")? {
                            crate::terminal::live::Action::Cancel => bail!("Interrupted"),
                            crate::terminal::live::Action::Detach => progress.notice("This turn is not owned by a background worker."),
                            crate::terminal::live::Action::Submit(prompt) => if let Some(reader)=input.as_mut() { reader.queue_prompt(prompt); },
                            crate::terminal::live::Action::Draft(draft) => if let Some(reader)=input.as_ref() { reader.restore_draft(draft); },
                            crate::terminal::live::Action::SideQuestion(question) => {
                                if side_request.is_some() { progress.notice("A side question is already running; wait for its answer before sending another."); }
                                else { side_request=Some(Box::pin(async move { crate::background_commands::side_question(self,&question).await })); }
                            },
                        }
                        continue;
                    },
                    answer = async { match side_request.as_mut() {Some(request)=>request.await,None=>std::future::pending().await} } => {
                        side_request=None;
                        match answer { Ok(answer)=>progress.notice(&format!("Side answer:\n{answer}")),Err(error)=>progress.notice(&format!("Side question failed: {error:#}")) }
                        continue;
                    },
                };
                if cli.output_format == OutputFormat::StreamJson {
                    if let Some(value) = event_json(&event) { emit_json(&value)?; }
                }
                match event {
                    EngineEvent::Text(delta) => { text.push_str(&delta); if cli.output_format == OutputFormat::Text { progress.before_text(&delta); if !progress.is_live() { print!("{delta}"); io::stdout().flush()?; } } },
                    EngineEvent::Thinking(delta) => {
                        thinking_text.push_str(&delta);
                        progress.record_thinking(&delta);
                        if cli.verbose { progress.before_text(&delta); if !progress.is_live() { eprint!("{delta}"); } }
                        else { progress.thinking(); }
                    },
                    EngineEvent::MessageStart { .. } => progress.set_activity("Composing"),
                    EngineEvent::AgentProgress { session_id, agent_type, status, tool_name } => progress.agent_progress(&session_id, &agent_type, &status, tool_name.as_deref()),
                    EngineEvent::Usage(value) => { progress.reported_usage(&value); if let Some(fields) = value.as_object() { for (key, value) in fields { usage[key] = value.clone(); } } },
                    EngineEvent::Error(message) => { progress.clear(); error = Some(message); },
                    EngineEvent::MessageStop { full_text, stop_reason } => {
                        if cli.json_schema.is_some() { text = full_text.clone(); }
                        if text.is_empty() && !full_text.is_empty() { if cli.output_format == OutputFormat::Text { progress.before_text(&full_text); if !progress.is_live() { print!("{full_text}"); } } text = full_text; }
                        if stop_reason.as_deref().is_some_and(|s| ["error", "panic", "stream_error", "interrupted", "failed", "cancelled", "max_iterations", "budget_warning", "max_tokens", "length", "incomplete"].contains(&s)) {
                            error.get_or_insert_with(|| format!("Turn stopped: {}", stop_reason.unwrap()));
                        }
                        completed = true; break;
                    },
                    EngineEvent::ToolPermission { request_id, tool_use_id, tool_name, input: tool_input } => {
                        progress.clear();
                        let question = format!("Allow {tool_name} {}? [y/N]", serde_json::to_string(&tool_input)?);
                        let answer = if let Some(sdk)=&self.sdk_permissions { sdk.ask(&request_id,&tool_use_id,&tool_name,&tool_input).await? } else if let Some(answer) = claude_core::services::background_sessions::worker_answer(&self.paths.data_dir, &self.id, &request_id, "permission", &question).await? { answer } else {
                            anyhow::ensure!(input.is_some(), "Tool approval required in non-interactive mode; choose --permission-mode explicitly");
                            turn_answer(&mut progress, input, question).await?
                        };
                        let answer = if ["y", "yes", "是", "允许"].contains(&answer.trim().to_lowercase().as_str()) { "yes" } else { "no" };
                        self.engine.resume_with_answer(&request_id, answer.into()).await?;
                        progress.set_activity("Working");
                    },
                    EngineEvent::AskUser { request_id, tool_use_id, tool_input, question, options } => {
                        progress.clear();
                        if let Some(sdk)=&self.sdk_permissions {
                            let answer=sdk.ask_question(&request_id,&tool_use_id,&question,&tool_input).await?;
                            self.engine.resume_with_answer(&request_id,answer).await?;
                            progress.set_activity("Composing");
                            continue;
                        }
                        let mut prompt = question;
                        for (index, option) in options.iter().enumerate() { prompt.push_str(&format!("\n{}. {option}", index+1)); }
                        let answer = if let Some(answer) = claude_core::services::background_sessions::worker_answer(&self.paths.data_dir, &self.id, &request_id, "question", &prompt).await? { answer } else { turn_answer(&mut progress, input, prompt).await? };
                        let answer = answer.trim().parse::<usize>().ok().and_then(|i| i.checked_sub(1)).and_then(|i| options.get(i)).cloned().unwrap_or(answer);
                        self.engine.resume_with_answer(&request_id, answer).await?;
                        progress.set_activity("Composing");
                    },
                    EngineEvent::ToolUseStart { tool_use_id, tool_name, tool_input, .. } => {
                        progress.clear();
                        if cli.verbose && !progress.is_live() { eprintln!("\n[{tool_name}]"); }
                        progress.tool_started(&tool_use_id, &tool_name, &tool_input);
                    },
                    EngineEvent::ToolUseDone { tool_use_id, tool_name, tool_input, output, is_error }
                    | EngineEvent::PipelineToolResult { tool_use_id, tool_name, tool_input, output, is_error, .. } => {
                        // The baseline is captured before the turn. Attribute changed paths to
                        // the actual post-hook input, including tools that partially fail.
                        if let Some(capture) = checkpoint.as_mut() {
                            if let Err(error) = capture.record_tool(&tool_name, &tool_input) {
                                progress.notice(&format!("Cannot checkpoint this tool's file: {error:#}"));
                            }
                        }
                        progress.tool_finished(&tool_use_id, &tool_name, &output, is_error);
                        tools.push(message_repo::ToolCallRow { id: tool_use_id, message_id: String::new(), name: tool_name, input: Some(tool_input.to_string()), output: Some(output), is_error, sort_order: tools.len() as i64 });
                    },
                    EngineEvent::RequestSnapshot(snapshot) => {
                        if let Some(old) = snapshots.iter_mut().find(|s| s["id"] == snapshot["id"]) { *old = snapshot; } else { snapshots.push(snapshot); }
                    },
                    EngineEvent::BudgetWarning { message, .. } => { progress.notice(&message); },
                    _ => {},
                }
            }
            Ok(())
        }.await;
        progress.clear();
        // Cancel and join before persisting, including Ctrl+C and closed stdin.
        self.engine.cancel_turn(&self.id).await;
        if let Some(capture) = checkpoint {
            if let Err(error) = claude_core::services::checkpoints::finish(capture) {
                progress.notice(&format!("Could not save code checkpoint: {error:#}"));
            }
        }
        if let Err(failure) = turn_result {
            error = Some(failure.to_string());
        }
        if !completed && error.is_none() {
            error = Some("Incomplete response".into());
        }
        if !text.is_empty() || !thinking_text.is_empty() || !tools.is_empty() || !snapshots.is_empty() {
            let message_id = self.store.append(&self.id, "assistant", &text)?;
            self.store.db.with_conn(|conn| -> Result<()> {
                if !thinking_text.is_empty() { conn.execute("UPDATE messages SET thinking = ?1 WHERE id = ?2", [&thinking_text, &message_id])?; }
                for tool in &tools {
                    message_repo::insert_tool_call(
                        conn,
                        &format!("{message_id}:{}", tool.id),
                        &message_id,
                        &tool.name,
                        tool.input.as_deref(),
                        tool.output.as_deref(),
                        tool.is_error,
                        tool.sort_order,
                    )?;
                }
                message_repo::save_request_snapshots(conn, &message_id, &json!(snapshots))?;
                Ok(())
            })??;
        }
        match cli.output_format {
            OutputFormat::Text => { if !progress.is_live() { println!(); io::stdout().flush()?; } },
            _ => emit_json(
                &json!({"type":"result","session_id":self.id,"result":text,"is_error":error.is_some(),"error":error,"usage":usage,"structured_output":if error.is_none() && cli.json_schema.is_some() {serde_json::from_str::<Value>(&text).ok()} else {None}}),
            )?,
        }
        if let Some(request) = side_request.take() {
            if error.is_none() {
                progress.set_activity("Main task complete \u{b7} finishing side question");
                tokio::pin!(request);
                loop {
                    tokio::select! {
                        answer = &mut request => {
                            match answer { Ok(answer)=>progress.notice(&format!("Side answer:\n{answer}")),Err(error)=>progress.notice(&format!("Side question failed: {error:#}")) }
                            break;
                        },
                        action = progress.action() => match action {
                            Some(crate::terminal::live::Action::Cancel)|None => { progress.notice("Side question cancelled."); break; },
                            Some(crate::terminal::live::Action::Submit(prompt)) => if let Some(reader)=input.as_mut() { reader.queue_prompt(prompt); },
                            Some(_) => progress.notice("Finishing the current side question; another can be sent from the next prompt."),
                        },
                        _ = tokio::signal::ctrl_c() => break,
                    }
                }
            }
        }
        for action in progress.finish(error.as_deref()) {
            if let Some(reader)=input.as_mut() {
                match action {
                    crate::terminal::live::Action::Submit(prompt)=>reader.queue_prompt(prompt),
                    crate::terminal::live::Action::SideQuestion(question)=>reader.queue_prompt(format!("/btw {question}")),
                    crate::terminal::live::Action::Draft(draft)=>reader.restore_draft(draft),
                    crate::terminal::live::Action::Cancel|crate::terminal::live::Action::Detach=>{},
                }
            }
        }
        if let Some(error) = error {
            if cli.output_format != OutputFormat::Text {
                return Err(crate::output::ReportedError(format!("{error} (session {})", self.id)).into());
            }
            bail!("{error} (session {})", self.id);
        }
        Ok(())
    }
}

async fn turn_answer(progress: &mut crate::terminal::live::TurnDisplay, input: &mut Option<TerminalInput>, question: String) -> Result<String> {
    if let Some(live)=progress.live.as_ref() {
        let mut answer=live.question(question);
        loop {
            tokio::select! {
                response=&mut answer=>return response.context("Terminal answer cancelled"),
                action=progress.action()=>match action {
                    Some(crate::terminal::live::Action::Cancel)|None=>bail!("Interrupted"),
                    Some(crate::terminal::live::Action::Detach)=>progress.notice("Answer the pending question before detaching."),
                    Some(crate::terminal::live::Action::Submit(prompt))=>if let Some(reader)=input.as_mut(){reader.queue_prompt(prompt);},
                    Some(crate::terminal::live::Action::SideQuestion(question))=>if let Some(reader)=input.as_mut(){reader.queue_prompt(format!("/btw {question}"));},
                    Some(crate::terminal::live::Action::Draft(draft))=>if let Some(reader)=input.as_ref(){reader.restore_draft(draft);},
                },
                _=tokio::signal::ctrl_c()=>bail!("Interrupted"),
            }
        }
    }
    progress.clear();
    let reader=input.as_mut().context("User input required in non-interactive mode; resume this session interactively")?;
    eprintln!("\n{question}");
    read_answer(reader).await
}

async fn read_answer(reader: &mut TerminalInput) -> Result<String> {
    tokio::select! { answer = reader.read_answer("") => answer?.context("Input closed"), _ = tokio::signal::ctrl_c() => bail!("Interrupted") }
}

pub(crate) fn emit_json(value: &Value) -> Result<()> {
    if claude_core::services::session_worker::emit_event(value)? { return Ok(()); }
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    writeln!(stdout)?;
    stdout.flush()?;
    Ok(())
}

fn event_json(event: &EngineEvent) -> Option<Value> {
    Some(match event {
        EngineEvent::Text(delta) => json!({"type":"text","delta":delta}),
        EngineEvent::Thinking(delta) => json!({"type":"thinking","delta":delta}),
        EngineEvent::Usage(usage) => json!({"type":"usage","usage":usage}),
        EngineEvent::MessageStart { model } => json!({"type":"message_start","model":model}),
        EngineEvent::AgentProgress { session_id, agent_type, status, tool_name } => json!({"type":"agent_progress","session_id":session_id,"agent_type":agent_type,"status":status,"tool_name":tool_name}),
        EngineEvent::Error(error) => json!({"type":"error","error":error}),
        EngineEvent::ToolUseStart {
            tool_use_id,
            tool_name,
            tool_input,
            ..
        } => json!({"type":"tool_start","id":tool_use_id,"name":tool_name,"input":tool_input}),
        EngineEvent::ToolUseDone {
            tool_use_id,
            tool_name,
            output,
            is_error,
            ..
        }
        | EngineEvent::PipelineToolResult {
            tool_use_id,
            tool_name,
            output,
            is_error,
            ..
        } => {
            json!({"type":"tool_result","id":tool_use_id,"name":tool_name,"output":output,"is_error":is_error})
        }
        EngineEvent::AskUser {
            request_id,
            question,
            options,
            ..
        } => {
            json!({"type":"ask_user","request_id":request_id,"question":question,"options":options})
        }
        EngineEvent::ToolPermission {
            request_id,
            tool_name,
            input,
            ..
        } => json!({"type":"permission","request_id":request_id,"name":tool_name,"input":input}),
        _ => return None,
    })
}

#[cfg(test)]
mod event_tests {
    use super::*;

    #[tokio::test]
    async fn context_suffix_survives_session_switch_resume_and_worker_snapshot(){
        use clap::Parser;
        let dir=tempfile::tempdir().unwrap();let paths=RuntimePaths::new(dir.path().join("data")).unwrap();
        let provider=serde_json::from_value(json!({"id":"fixture","name":"Fixture","base_url":"http://127.0.0.1:1","api_key":"fixture-only","api_format":"openai","enabled":true,"models":[{"id":"flash","name":"Flash","enabled":true,"supports_vision":false,"supports_web_search":false,"context_window":128000}]})).unwrap();
        let selection=Selection{providers:vec![provider],provider_id:"fixture".into(),model:"flash[1m]".into()};
        let cli=Cli::parse_from(["claude-code","--print"]);
        let mut session=Session::open(&cli,&paths,selection,None).await.unwrap();
        assert_eq!(session.context_window().await,Some(1_000_000));assert_eq!(session.model,"flash[1m]");
        let resolved=session.selected_provider().await.unwrap();
        assert_eq!(resolved.model.id,"flash");
        let env=crate::background_commands::worker_environment(false,&resolved).unwrap();
        let snapshot:Value=serde_json::from_str(&env.iter().find(|(key,_)|key=="CLAUDE_RUST_WORKER_MODEL").unwrap().1).unwrap();
        assert_eq!(snapshot["id"],"flash");assert_eq!(snapshot["context_window"],1_000_000);
        let saved=session.id.clone();session.sdk_set_model("flash").await.unwrap();assert_eq!(session.context_window().await,Some(128000));
        session.sdk_set_model("flash[1m]").await.unwrap();assert_eq!(session.model,"flash[1m]");
        session.resume_saved(&paths,&saved).await.unwrap();assert_eq!(session.context_window().await,Some(1_000_000));
        assert_eq!(session.store.resume(&saved).unwrap().model.as_deref(),Some("flash[1m]"));
    }

    #[tokio::test]
    async fn mcp_starts_on_first_engine_turn_and_is_reused_by_sdk_turns() {
        use clap::Parser;
        let directory = tempfile::tempdir().unwrap();
        let paths = RuntimePaths::new(directory.path().join("data")).unwrap();
        std::fs::create_dir_all(&paths.data_dir).unwrap();
        let marker = directory.path().join("mcp-starts.txt");
        let script = directory.path().join("mcp.cjs");
        std::fs::write(&script, r#"const fs=require('fs');fs.appendFileSync(process.env.START_MARKER,'start\n');require('readline').createInterface({input:process.stdin}).on('line',line=>{const r=JSON.parse(line);if(r.id!==undefined)process.stdout.write(JSON.stringify({jsonrpc:'2.0',id:r.id,result:{protocolVersion:'2024-11-05',capabilities:{},serverInfo:{name:'fixture',version:'1'},tools:[],resources:[]}})+'\n');});"#).unwrap();
        std::fs::write(paths.data_dir.join("mcp-servers.json"), serde_json::to_vec(&json!({"mcpServers":{"fixture":{"command":"node","args":[script],"enabled":true,"env":{"START_MARKER":marker}}}})).unwrap()).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let app = axum::Router::new().route("/v1/chat/completions", axum::routing::post(|| async {
            ([ ("content-type", "text/event-stream") ], "data: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"done\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"fixture\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n")
        }));
        let server = tokio::spawn(async move { axum::serve(listener,app).await.unwrap(); });
        let provider = serde_json::from_value(json!({"id":"fixture","name":"Fixture","base_url":format!("http://{address}/v1"),"api_key":"fixture-only","api_format":"openai","enabled":true,"web_search_strategy":null,"models":[{"id":"fixture-model","name":"Fixture","enabled":true,"max_tokens":64,"supports_vision":false,"supports_web_search":false}]})).unwrap();
        let selection = Selection { providers:vec![provider],provider_id:"fixture".into(),model:"fixture-model".into() };
        let cli = Cli::try_parse_from(["claude-code", "--print", "--output-format", "stream-json"]).unwrap();
        let store = SessionStore::new(paths.open_database().unwrap());
        let id = store.create(directory.path(), "fixture-model", "fixture").unwrap();
        let session = Session::open(&cli, &paths, selection, Some(&id)).await.unwrap();
        assert!(!marker.exists(), "opening the foreground must not start MCP children");
        session.reload_plugins().await.unwrap();
        assert!(!marker.exists(), "metadata refresh must not start unused MCP children");
        for prompt in ["first SDK turn", "second SDK turn"] {
            tokio::time::timeout(std::time::Duration::from_secs(15), session.run_turn(&cli,prompt,&mut None)).await.unwrap().unwrap();
            assert_eq!(std::fs::read_to_string(&marker).unwrap(), "start\n");
        }
        session.close().await.unwrap();
        server.abort();
    }

    #[test]
    fn agent_progress_json_preserves_real_session_status() {
        let value = event_json(&EngineEvent::AgentProgress {
            session_id: "child-42".into(),
            agent_type: "reviewer".into(),
            status: "tool_failed".into(),
            tool_name: Some("Read".into()),
        }).unwrap();
        assert_eq!(value, json!({
            "type": "agent_progress", "session_id": "child-42",
            "agent_type": "reviewer", "status": "tool_failed", "tool_name": "Read"
        }));
    }
}
