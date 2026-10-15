//! Child tasks reuse ToolLoopExecutor inline: dropping a parent drops every child future.
use super::{EngineEvent, ToolLoopExecutor};
use crate::{
    runtime::{RuntimePaths, SessionStore},
    services::{agents, EnvironmentCredentials, ServiceContext},
    tools::ToolDefinition,
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};

pub(crate) type OwnedChildren = Arc<Mutex<HashMap<String, OwnedChild>>>;
pub(crate) struct OwnedChild {
    executor: Box<ToolLoopExecutor>,
    pub(super) parent: String,
    policy: Value,
    binding: Value,
    retained: Instant,
}

pub(super) fn definitions() -> Vec<ToolDefinition> {
    ["Agent", "Delegate", "Worker"].into_iter().map(|name| ToolDefinition {
        name: name.into(), description: "Delegate a bounded task to a persistent child session using the same model and tool permissions. Waits for completion; cancellation stops the child. Use agent_type to select an existing agent definition.".into(),
        input_schema: json!({"type":"object","properties":{"prompt":{"type":"string"},"agent_type":{"type":"string","default":"general"},"resume":{"type":"string","description":"Resume a completed child UUID owned by this session; original live configuration must still be retained"}},"required":["prompt"],"additionalProperties":false})
    }).collect()
}

struct ChildState {
    context: ServiceContext,
    key: String,
    value: Value,
    event_tx: tokio::sync::mpsc::Sender<EngineEvent>,
}

struct TeamTaskGuard {
    cwd: String,
    team_id: String,
    task_id: String,
    finished: bool,
}
impl Drop for TeamTaskGuard {
    fn drop(&mut self) {
        if !self.finished {
            let _ = crate::tools::coordination::finish_team_task(
                &self.cwd,
                &self.team_id,
                &self.task_id,
                None,
                Some("Parent task was cancelled or interrupted"),
            );
        }
    }
}
impl ChildState {
    fn progress(&self, status: &str, tool_name: Option<String>) -> EngineEvent {
        EngineEvent::AgentProgress {
            session_id: self.value["session_id"].as_str().unwrap_or_default().into(),
            agent_type: self.value["agent_type"].as_str().unwrap_or_default().into(),
            status: status.into(),
            tool_name,
        }
    }
}
impl Drop for ChildState {
    fn drop(&mut self) {
        if self.value["status"] == "running" {
            self.value["status"] = json!("cancelled");
            let _ = self.event_tx.try_send(self.progress("cancelled", None));
        }
        self.value["finished_at"] = json!(chrono::Utc::now().to_rfc3339());
        if self.context.save_setting(&self.key, &self.value).is_err() {
            tracing::warn!("Could not persist child task state");
        }
    }
}

fn owned_binding(
    context: &ServiceContext,
    parent: &str,
    child: &str,
    expected: &str,
) -> Result<Value> {
    context.db.with_conn(|conn| -> Result<Value> {
        let parent_row=crate::db::conversation_repo::get_conversation(conn,parent)?.context("Owner missing")?;
        let child_row=crate::db::conversation_repo::get_conversation(conn,child)?.context("Child missing")?;
        ensure!(!parent_row.archived && !child_row.archived,"Owner or child archived");
        let root=if child_row.project_id.as_deref().is_none_or(str::is_empty) {std::path::Path::new(expected).parent().context("Private root missing")?.to_path_buf()} else {context.paths.data_dir.join("workspaces")};
        let actual=crate::native_engine::workspace::resolve_workspace(conn,&root,child,None)?.0.canonicalize()?;
        let owner=crate::native_engine::workspace::resolve_workspace(conn,&root,parent,None)?.0.canonicalize()?;
        ensure!(actual==owner && actual==std::path::Path::new(expected).canonicalize()?,"Owner/child workspace no longer matches");
        Ok(json!({"runtime":context.paths.data_dir.canonicalize()?,"parent_project":parent_row.project_id,"parent_mode":parent_row.workspace_mode,"child_project":child_row.project_id,"child_mode":child_row.workspace_mode,"cwd":actual,"provider":child_row.provider,"model":child_row.model}))
    })?
}

impl ToolLoopExecutor {
    fn child_owner_policy(&self) -> Value {
        json!({"permissions":self.permission_manager.as_ref().map(|manager|manager.delegation_policy_snapshot(self.conv_id.as_deref().unwrap_or_default())),"tools":self.allowed_tools,"directories":self.allowed_directories})
    }

    pub(super) async fn resume_pending_owned_children(&self) {
        if self
            .child_resumes
            .load(std::sync::atomic::Ordering::Relaxed)
            >= 8
        {
            return;
        }
        let ids: Vec<String> = {
            let registry = self
                .owned_children
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            registry
                .iter()
                .filter(|(_, child)| Some(child.parent.as_str()) == self.conv_id.as_deref())
                .map(|(id, _)| id.clone())
                .collect()
        };
        let Some(root) = self.search_config_path.as_deref().and_then(|p| p.parent()) else {
            return;
        };
        let Ok(context) = crate::services::session_messages::context(root) else {
            return;
        };
        for id in ids {
            if self
                .child_resumes
                .load(std::sync::atomic::Ordering::Relaxed)
                >= 8
            {
                break;
            }
            if crate::services::session_messages::pending_count(&context, &id).unwrap_or(0) > 0 {
                // The queue remains durable when policy/configuration no longer permits a wake.
                if let Ok(result) = Box::pin(self.resume_owned_child(&id, None, true)).await {
                    if let Some(parent) = self.conv_id.as_deref() {
                        let mut summary = format!(
                            "Owned child {id} resumed at an owner turn boundary and completed: {}",
                            result["result"]
                        );
                        if summary.len() > 60 * 1024 {
                            let mut cut = 60 * 1024;
                            while !summary.is_char_boundary(cut) {
                                cut -= 1;
                            }
                            summary.truncate(cut);
                            summary.push_str(" (truncated; inspect child transcript)");
                        }
                        let _ = crate::services::session_messages::send(
                            &context, &id, parent, &summary,
                        );
                    }
                }
            }
        }
    }

    pub(super) async fn resume_owned_child(
        &self,
        id: &str,
        prompt: Option<&str>,
        automatic: bool,
    ) -> Result<Value> {
        ensure!(
            self.child_resumes
                .load(std::sync::atomic::Ordering::Relaxed)
                < 8,
            "Owned child resume budget exhausted; needs_resume"
        );
        if let Some(prompt) = prompt {
            ensure!(
                !prompt.trim().is_empty() && prompt.len() <= 64000,
                "Resume prompt must contain 1..64000 bytes"
            );
        }
        let parent = self
            .conv_id
            .as_deref()
            .context("Child owner unavailable")?
            .to_owned();
        let root = self
            .search_config_path
            .as_deref()
            .and_then(|path| path.parent())
            .context("Runtime unavailable")?;
        let context = crate::services::session_messages::context(root)?;
        let saved: Value = context
            .setting(&format!("delegation.{id}"))?
            .context("Child ownership record unavailable; needs_resume")?;
        ensure!(
            saved["parent_session_id"] == parent,
            "Child belongs to another owner"
        );
        ensure!(
            saved["status"] == "completed",
            "Child is stopped, cancelled, failed or running; automatic reactivation is prohibited"
        );
        ensure!(
            !crate::services::session_messages::receiver_active(root, id)?,
            "Child already has an active receiver"
        );
        let policy = self.child_owner_policy();
        let mut entry = {
            let mut registry = self
                .owned_children
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let retained = registry
                .get(id)
                .context("Original child configuration is no longer resident; needs_resume")?;
            ensure!(
                retained.parent == parent && retained.policy == policy,
                "Owner policy changed; needs_resume"
            );
            ensure!(
                owned_binding(&context, &parent, id, &retained.executor.workspace_cwd)?
                    == retained.binding,
                "Child workspace/provider binding changed; needs_resume"
            );
            registry.remove(id).unwrap()
        };
        self.child_resumes
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |count| (count < 8).then_some(count + 1),
            )
            .map_err(|_| anyhow::anyhow!("Owned child resume budget exhausted"))?;
        // Original provider/model, tool bounds, permissions and hooks stay with this executor.
        // Only event routing and durable transcript are refreshed for the current owner turn.
        entry.executor.execution_lease =
            crate::services::session_messages::ExecutionLease::acquire(
                &context.paths.data_dir,
                id,
            )?;
        let current: Value = context
            .setting(&format!("delegation.{id}"))?
            .context("Child ownership disappeared")?;
        ensure!(
            current["parent_session_id"] == parent && current["status"] == "completed",
            "Child was stopped or ownership changed before resume"
        );
        // Refresh verified owner constraints on every resume, without promoting
        // the model-generated resume prompt or peer inbox data to user authority.
        crate::services::permission_authority::inherit(&context.db, &parent, id)?;
        let sessions = SessionStore::new(context.db.clone());
        if let Some(prompt) = prompt {
            sessions.append(id, "user", prompt)?;
        }
        entry.executor.messages =
            sessions.model_history(id, &entry.executor.provider.provider.api_format)?;
        let (events, receiver) = tokio::sync::mpsc::channel(500);
        entry.executor.event_tx = events;
        entry.executor.answer_waiters = self.answer_waiters.clone();
        entry.executor.permission_mailbox = self.permission_mailbox.clone();
        entry.executor.message_inbox = None;
        entry.executor.stop_hook_active = false;
        if let Some(manager) = &entry.executor.permission_manager {
            manager.clear_all_confirmations();
        }
        let result = Box::pin(self.run_owned_child(
            context,
            id.into(),
            saved["agent_type"].as_str().unwrap_or("general").into(),
            *entry.executor,
            receiver,
            entry.policy,
        ))
        .await?;
        Ok(
            json!({"session_id":id,"status":"completed","resumed_in_owner_turn":true,"trigger":if automatic {"inbox"} else {"explicit"},"result":result["result"]}),
        )
    }

    pub(super) async fn execute_team(&mut self, input: &Value) -> Result<Value> {
        let team_id = input["team_id"].as_str().context("team_id is required")?;
        let mut results = Vec::new();
        for _ in 0..100 {
            let Some(task) =
                crate::tools::coordination::claim_team_task(&self.workspace_cwd, team_id)?
            else {
                break;
            };
            let task_id = task["task_id"]
                .as_str()
                .context("Claimed task has no ID")?
                .to_string();
            let mut guard = TeamTaskGuard {
                cwd: self.workspace_cwd.clone(),
                team_id: team_id.into(),
                task_id: task_id.clone(),
                finished: false,
            };
            let result = Box::pin(
                self.execute_delegated(&json!({"prompt":task["prompt"],"agent_type":"general"})),
            )
            .await;
            match result {
                Ok(value) => {
                    crate::tools::coordination::finish_team_task(
                        &self.workspace_cwd,
                        team_id,
                        &task_id,
                        Some(&value.to_string()),
                        None,
                    )?;
                    guard.finished = true;
                    results.push(json!({"task_id":task_id,"result":value}));
                }
                Err(error) => {
                    crate::tools::coordination::finish_team_task(
                        &self.workspace_cwd,
                        team_id,
                        &task_id,
                        None,
                        Some(&error.to_string()),
                    )?;
                    guard.finished = true;
                    return Err(error);
                }
            }
        }
        Ok(json!({"team_id":team_id,"completed_tasks":results}))
    }

    pub(super) async fn execute_delegated(&self, input: &Value) -> Result<Value> {
        self.execute_delegated_typed(input, None, None).await
    }

    /// Workflow children inherit authority and may only narrow their tool set.
    pub(super) async fn execute_delegated_typed(
        &self,
        input: &Value,
        schema: Option<Value>,
        tools: Option<Vec<String>>,
    ) -> Result<Value> {
        self.execute_delegated_authorized(input, schema, tools, false)
            .await
    }

    pub(super) async fn execute_delegated_authorized(
        &self,
        input: &Value,
        schema: Option<Value>,
        tools: Option<Vec<String>>,
        preapproved: bool,
    ) -> Result<Value> {
        if !preapproved {
            use crate::permissions::PermissionResult;
            let invocation_id = uuid::Uuid::new_v4().to_string();
            match self.check_permission("Agent", input, &invocation_id).await {
                PermissionResult::Denied(reason) => {
                    anyhow::bail!("Delegation permission denied: {reason}")
                }
                PermissionResult::RequiresConfirmation(_) => {
                    let request_id = uuid::Uuid::new_v4().to_string();
                    let (tx, rx) = tokio::sync::oneshot::channel();
                    self.answer_waiters
                        .lock()
                        .await
                        .insert(request_id.clone(), tx);
                    let _guard = super::PermissionWaiterGuard {
                        waiters: self.answer_waiters.clone(),
                        request_id: request_id.clone(),
                    };
                    self.event_tx
                        .send(EngineEvent::ToolPermission {
                            request_id: request_id.clone(),
                            tool_use_id: invocation_id.clone(),
                            tool_name: "Agent".into(),
                            input: input.clone(),
                        })
                        .await
                        .context("Delegation approval host disconnected")?;
                    let answer = self
                        .wait_for_user_answer(rx)
                        .await
                        .context("Delegation approval timed out")??;
                    ensure!(
                        ["yes", "y", "allow", "approved", "always_allow"]
                            .contains(&answer.trim().to_lowercase().as_str()),
                        "Delegation not approved"
                    );
                    if let Some(manager) = &self.permission_manager {
                        let context = crate::permissions::PermissionContext {
                            tool_name: "Agent".into(),
                            tool_input: input.clone(),
                            conversation_id: self.conv_id.clone().unwrap_or_default(),
                            user_id: None,
                            workspace_path: Some(self.workspace_cwd.clone()),
                        };
                        ensure!(
                            !matches!(
                                manager.check_permission(&context),
                                PermissionResult::Denied(_)
                            ),
                            "Delegation policy denied the action after approval"
                        );
                    }
                }
                PermissionResult::Granted => {}
            }
        }
        if let Some(id) = input["resume"].as_str() {
            return Box::pin(self.resume_owned_child(id, input["prompt"].as_str(), false)).await;
        }
        ensure!(self.delegation_depth < 2, "Delegation depth limit reached");
        let prompt = input["prompt"].as_str().context("prompt is required")?;
        let max_prompt = if schema.is_some() {
            1024 * 1024
        } else {
            64_000
        };
        ensure!(
            !prompt.trim().is_empty() && prompt.len() <= max_prompt,
            "Child prompt must contain 1–{max_prompt} bytes"
        );
        let root = self
            .search_config_path
            .as_deref()
            .and_then(|p| p.parent())
            .context("Runtime directory is unavailable")?;
        let context = ServiceContext::open(
            RuntimePaths::new(root.to_path_buf())?,
            Arc::new(EnvironmentCredentials),
        )?;
        let mut definition =
            agents::resolve(&context, input["agent_type"].as_str().unwrap_or("general"))?;
        if let Some(parent) = &self.allowed_tools {
            definition.tools = Some(match definition.tools {
                Some(child) => child
                    .into_iter()
                    .filter(|name| parent.contains(name))
                    .collect(),
                None => parent.clone(),
            });
        }
        if let Some(tools) = tools {
            definition.tools = Some(match definition.tools {
                Some(allowed) => tools
                    .into_iter()
                    .filter(|name| allowed.contains(name))
                    .collect(),
                None => tools,
            });
        }
        let sessions = SessionStore::new(context.db.clone());
        let id = if let Some(parent) = self.conv_id.as_deref() {
            // Preserve the parent's trusted source identity and managed worktree.
            // Creating from actual cwd would incorrectly trust a worktree as a new project.
            let parent_row = context
                .db
                .with_conn(|conn| crate::db::conversation_repo::get_conversation(conn, parent))??
                .context("Parent session not found")?;
            let workspaces_root = if parent_row.project_id.as_deref().is_none_or(str::is_empty) {
                // The engine may use a private root outside data_dir/workspaces.
                std::path::Path::new(&self.workspace_cwd)
                    .parent()
                    .context("Private workspace has no root")?
                    .to_path_buf()
            } else {
                context.paths.data_dir.join("workspaces")
            };
            let child = sessions.derive_owned_child(
                parent,
                &workspaces_root,
                std::path::Path::new(&self.workspace_cwd),
            )?;
            context.db.with_conn(|conn| {
                conn.execute(
                    "UPDATE conversations SET model=?2,provider=?3 WHERE id=?1",
                    rusqlite::params![child, self.provider.model.id, self.provider.provider.id],
                )
            })??;
            child
        } else {
            sessions.create(
                std::path::Path::new(&self.workspace_cwd),
                &self.provider.model.id,
                &self.provider.provider.id,
            )?
        };
        agents::bind(&context.db, &id, &definition)?;
        sessions.append(&id, "user", prompt)?;
        let (events, receiver) = tokio::sync::mpsc::channel(500);
        let system = format!(
            "{}\nChild task role:\n{}\nChild session ID: {}\nParent session ID: {}",
            self.system_prompt.as_deref().unwrap_or(""),
            definition.system_prompt,
            id,
            self.conv_id.as_deref().unwrap_or("")
        );
        let mut child = ToolLoopExecutor::new(
            self.provider.clone(),
            vec![json!({"role":"user","content":prompt})],
            Some(system),
            self.max_tokens,
            events,
            self.workspace_cwd.clone(),
            self.reasoning_mode.clone(),
        )
        .with_allowed_tools(definition.tools)
        .with_prompt_profile(self.prompt_profile)
        .with_conv_id(id.clone())
        .with_answer_waiters(self.answer_waiters.clone())
        .with_permission_mailbox(self.permission_mailbox.clone())
        .with_bundled_resources(self.bundled_resources.clone())
        .with_search_config_path(self.search_config_path.clone().unwrap());
        child = child.with_response_schema(schema)?;
        child.allowed_directories = self.allowed_directories.clone();
        child.delegation_depth = self.delegation_depth + 1;
        child.durable_user_input = self.durable_user_input;
        child.auto_manual_interactive = self.auto_manual_interactive;
        child.permission_manager = self.permission_manager.as_ref().map(|manager| {
            Arc::new(manager.fork_session(self.conv_id.as_deref().unwrap_or_default(), &id))
        });
        child.hooks = self.hooks.clone();
        let hook_session = self
            .subagent_hook_context
            .as_ref()
            .map(|(parent, _)| parent.clone())
            .or_else(|| self.conv_id.clone())
            .unwrap_or_else(|| "unknown".into());
        child.subagent_hook_context = Some((hook_session.clone(), definition.id.clone()));
        if let Some(hooks) = &child.hooks {
            use crate::services::tool_hooks::HookEvent;
            let start = hooks
                .run(
                    HookEvent::SubagentStart,
                    &hook_session,
                    json!({"agent_id":id,"agent_type":definition.id}),
                )
                .await?;
            for context in start.additional_context {
                child
                    .system_prompt
                    .get_or_insert_with(String::new)
                    .push_str(&format!("\nSubagentStart context:\n{context}"));
            }
            for warning in start.warnings {
                tracing::warn!("SubagentStart hook: {warning}");
            }
        }
        child.max_tool_iterations = self.max_tool_iterations;
        child.mcp_registry = self.mcp_registry.clone();
        if let Some(memory) = &self.memex_client {
            child = child.with_memex(memory.clone());
        }
        let policy = self.child_owner_policy();
        self.run_owned_child(context, id, definition.id, child, receiver, policy)
            .await
    }

    async fn run_owned_child(
        &self,
        context: ServiceContext,
        id: String,
        agent_type: String,
        mut child: ToolLoopExecutor,
        mut receiver: tokio::sync::mpsc::Receiver<EngineEvent>,
        policy: Value,
    ) -> Result<Value> {
        let sessions = SessionStore::new(context.db.clone());
        let binding = owned_binding(
            &context,
            self.conv_id.as_deref().context("Child owner unavailable")?,
            &id,
            &child.workspace_cwd,
        )?;
        if child.execution_lease.is_none() {
            child.execution_lease = crate::services::session_messages::ExecutionLease::acquire(
                &context.paths.data_dir,
                &id,
            )?;
        }
        let mut state = ChildState {
            context: context.clone(),
            key: format!("delegation.{id}"),
            event_tx: self.event_tx.clone(),
            value: json!({"session_id":id,"parent_session_id":self.conv_id,"agent_type":agent_type,"status":"running","started_at":chrono::Utc::now().to_rfc3339()}),
        };
        context.save_setting(&state.key, &state.value)?;
        let _ = self.event_tx.send(state.progress("started", None)).await;
        let mut assistant_id = uuid::Uuid::new_v4().to_string();
        let mut tool_rows: HashMap<String, crate::db::message_repo::ToolCallRow> = HashMap::new();
        let collect_web_evidence = child.response_schema.is_some();
        let mut web_evidence = Vec::new();
        let mut evidence_ids = std::collections::HashSet::new();
        let result = {
            let execution = Box::pin(child.execute());
            tokio::pin!(execution);
            loop {
                tokio::select! {
                    biased;
                    Some(event) = receiver.recv() => {
                        match event {
                            EngineEvent::ContextReset { .. } => {
                                assistant_id = uuid::Uuid::new_v4().to_string();
                                tool_rows.clear();
                            }
                            event @ (EngineEvent::AskUser {..} | EngineEvent::ToolPermission {..} | EngineEvent::Usage(_) | EngineEvent::AgentProgress {..}) => { let _ = self.event_tx.send(event).await; }
                            EngineEvent::ToolUseStart { tool_name, .. } => {
                                let _ = self.event_tx.send(state.progress("running", Some(tool_name))).await;
                            }
                            EngineEvent::ToolUseDone { tool_use_id, tool_name, tool_input, output, is_error }
                            | EngineEvent::PipelineToolResult { tool_use_id, tool_name, tool_input, output, is_error, .. } => {
                                if collect_web_evidence && matches!(tool_name.as_str(), "WebSearch" | "WebFetch") && evidence_ids.insert(tool_use_id.clone()) {
                                    web_evidence.push(json!({"tool_use_id":tool_use_id,"name":tool_name,"input":tool_input,"output":serde_json::from_str::<Value>(&output).unwrap_or_else(|_|Value::String(output.clone())),"is_error":is_error}));
                                }
                                let row_id = format!("{assistant_id}:{tool_use_id}");
                                let previous = tool_rows.get(&row_id);
                                let sort_order = previous.map_or(tool_rows.len() as i64, |row| row.sort_order);
                                let row = crate::db::message_repo::ToolCallRow { id: row_id, message_id: assistant_id.clone(), name:tool_name, input:Some(tool_input.to_string()), output:Some(output), is_error, sort_order };
                                // Parallel execution emits both ordinary and pipeline completion.
                                // Preserve one result position and one progress notice for the pair.
                                if previous.is_some_and(|old| old.name == row.name && old.input == row.input && old.output == row.output && old.is_error == row.is_error) {
                                    continue;
                                }
                                let persisted = (|| -> Result<()> {
                                    context.db.with_conn(|conn| crate::db::message_repo::save_stream_tool_result(conn, &id, &assistant_id, "", &row))??;
                                    Ok(())
                                })();
                                if let Err(error) = persisted {
                                    state.value["status"] = json!("failed");
                                    let _ = self.event_tx.send(state.progress("failed", None)).await;
                                    return Err(error);
                                }
                                tool_rows.insert(row.id.clone(), row.clone());
                                let status = if is_error { "tool_failed" } else { "tool_completed" };
                                let _ = self.event_tx.send(state.progress(status, Some(row.name.clone()))).await;
                            }
                            _ => {}
                        }
                    }
                    result = &mut execution => break result,
                }
            }
        };
        match result {
            Ok((text, reason)) => {
                let failed = matches!(
                    reason.as_deref(),
                    Some(
                        "stream_error"
                            | "error"
                            | "interrupted"
                            | "failed"
                            | "cancelled"
                            | "max_iterations"
                            | "max_tokens"
                            | "length"
                            | "incomplete"
                    )
                );
                if let Err(error) = sessions.append(&id, "assistant", &text) {
                    state.value["status"] = json!("failed");
                    let _ = self.event_tx.send(state.progress("failed", None)).await;
                    return Err(error.into());
                }
                let status = if failed { "failed" } else { "completed" };
                state.value["status"] = json!(status);
                let _ = self.event_tx.send(state.progress(status, None)).await;
                ensure!(
                    !failed,
                    "Child session {id} did not complete: {}",
                    reason.unwrap_or_default()
                );
                if !collect_web_evidence {
                    // Retain configuration/capabilities, not the last turn's transcript or handles.
                    child.messages.clear();
                    child.messages.shrink_to_fit();
                    child.auto_history.clear();
                    child.auto_history.shrink_to_fit();
                    child.streaming_tool_args.clear();
                    child.streaming_tool_args.shrink_to_fit();
                    child.message_inbox = None;
                    child.execution_lease = None;
                    child.context_manager = child
                        .memex_client
                        .clone()
                        .map(|client| crate::memory::ContextManager::new(Arc::new(client)));
                    child.answer_waiters = Arc::new(tokio::sync::Mutex::new(HashMap::new()));
                    child.permission_mailbox = Default::default();
                    let (events, _receiver) = tokio::sync::mpsc::channel(1);
                    child.event_tx = events;
                    let mut registry = self
                        .owned_children
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    if registry.len() >= 32 {
                        if let Some(oldest) = registry
                            .iter()
                            .min_by_key(|(_, entry)| entry.retained)
                            .map(|(id, _)| id.clone())
                        {
                            registry.remove(&oldest);
                        }
                    }
                    registry.insert(
                        id.clone(),
                        OwnedChild {
                            executor: Box::new(child),
                            parent: self.conv_id.clone().unwrap_or_default(),
                            policy,
                            binding,
                            retained: Instant::now(),
                        },
                    );
                }
                Ok(
                    json!({"session_id":id,"status":"completed","result":text,"web_evidence":web_evidence,"resume_available":!collect_web_evidence}),
                )
            }
            Err(error) => {
                state.value["status"] = json!("failed");
                let _ = self.event_tx.send(state.progress("failed", None)).await;
                Err(error)
            }
        }
    }
}

#[cfg(test)]
#[path = "delegation_workspace_tests.rs"]
mod workspace_tests;

#[cfg(test)]
#[path = "delegation_resume_tests.rs"]
mod resume_tests;
