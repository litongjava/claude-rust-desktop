//! Child tasks reuse ToolLoopExecutor inline: dropping a parent drops every child future.
use super::{EngineEvent, ToolLoopExecutor};
use crate::{
    runtime::{RuntimePaths, SessionStore},
    services::{agents, EnvironmentCredentials, ServiceContext},
    tools::ToolDefinition,
};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;

pub(super) fn definitions() -> Vec<ToolDefinition> {
    ["Agent", "Delegate", "Worker"].into_iter().map(|name| ToolDefinition {
        name: name.into(), description: "Delegate a bounded task to a persistent child session using the same model and tool permissions. Waits for completion; cancellation stops the child. Use agent_type to select an existing agent definition.".into(),
        input_schema: json!({"type":"object","properties":{"prompt":{"type":"string"},"agent_type":{"type":"string","default":"general"}},"required":["prompt"],"additionalProperties":false})
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

impl ToolLoopExecutor {
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
    pub(super) async fn execute_delegated_typed(&self, input: &Value, schema: Option<Value>, tools: Option<Vec<String>>) -> Result<Value> {
        ensure!(self.delegation_depth < 2, "Delegation depth limit reached");
        let prompt = input["prompt"].as_str().context("prompt is required")?;
        let max_prompt = if schema.is_some() { 1024 * 1024 } else { 64_000 };
        ensure!(!prompt.trim().is_empty() && prompt.len() <= max_prompt,
            "Child prompt must contain 1–{max_prompt} bytes");
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
                Some(allowed) => tools.into_iter().filter(|name| allowed.contains(name)).collect(),
                None => tools,
            });
        }
        let sessions = SessionStore::new(context.db.clone());
        let id = if let Some(parent) = self.conv_id.as_deref() {
            // Preserve the parent's trusted source identity and managed worktree.
            // Creating from actual cwd would incorrectly trust a worktree as a new project.
            let child = sessions.clear(parent)?;
            context.db.with_conn(|conn| conn.execute(
                "UPDATE conversations SET model=?2,provider=?3 WHERE id=?1",
                rusqlite::params![child, self.provider.model.id, self.provider.provider.id],
            ))??;
            child
        } else {
            sessions.create(std::path::Path::new(&self.workspace_cwd), &self.provider.model.id, &self.provider.provider.id)?
        };
        agents::bind(&context.db, &id, &definition)?;
        sessions.append(&id, "user", prompt)?;
        let mut state = ChildState {
            context: context.clone(),
            key: format!("delegation.{id}"),
            event_tx: self.event_tx.clone(),
            value: json!({"session_id":id,"parent_session_id":self.conv_id,"agent_type":definition.id,"status":"running","started_at":chrono::Utc::now().to_rfc3339()}),
        };
        context.save_setting(&state.key, &state.value)?;
        let _ = self.event_tx.send(state.progress("started", None)).await;
        let (events, mut receiver) = tokio::sync::mpsc::channel(500);
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
        .with_bundled_resources(self.bundled_resources.clone())
        .with_search_config_path(self.search_config_path.clone().unwrap());
        let collect_web_evidence = schema.is_some();
        child = child.with_response_schema(schema)?;
        child.allowed_directories = self.allowed_directories.clone();
        child.delegation_depth = self.delegation_depth + 1;
        child.durable_user_input = self.durable_user_input;
        child.permission_manager = self.permission_manager.as_ref().map(|manager|
            Arc::new(manager.fork_session(self.conv_id.as_deref().unwrap_or_default(), &id)));
        child.hooks = self.hooks.clone();
        let hook_session = self.subagent_hook_context.as_ref().map(|(parent, _)| parent.clone())
            .or_else(|| self.conv_id.clone()).unwrap_or_else(|| "unknown".into());
        child.subagent_hook_context = Some((hook_session.clone(), definition.id.clone()));
        if let Some(hooks) = &child.hooks {
            use crate::services::tool_hooks::HookEvent;
            let start = hooks.run(HookEvent::SubagentStart, &hook_session,
                json!({"agent_id":id,"agent_type":definition.id})).await?;
            for context in start.additional_context {
                child.system_prompt.get_or_insert_with(String::new).push_str(&format!("\nSubagentStart context:\n{context}"));
            }
            for warning in start.warnings { tracing::warn!("SubagentStart hook: {warning}"); }
        }
        child.max_tool_iterations = self.max_tool_iterations;
        child.mcp_registry = self.mcp_registry.clone();
        if let Some(memory) = &self.memex_client {
            child = child.with_memex(memory.clone());
        }
        let assistant_id = uuid::Uuid::new_v4().to_string();
        let mut tool_index = 0;
        let mut web_evidence = Vec::new();
        let mut evidence_ids = std::collections::HashSet::new();
        let execution = Box::pin(child.execute());
        tokio::pin!(execution);
        let result = loop {
            tokio::select! {
                biased;
                Some(event) = receiver.recv() => {
                    match event {
                        event @ (EngineEvent::AskUser {..} | EngineEvent::ToolPermission {..} | EngineEvent::Usage(_) | EngineEvent::AgentProgress {..}) => { let _ = self.event_tx.send(event).await; }
                        EngineEvent::ToolUseStart { tool_name, .. } => {
                            let _ = self.event_tx.send(state.progress("running", Some(tool_name))).await;
                        }
                        EngineEvent::ToolUseDone { tool_use_id, tool_name, tool_input, output, is_error }
                        | EngineEvent::PipelineToolResult { tool_use_id, tool_name, tool_input, output, is_error, .. } => {
                            if collect_web_evidence && matches!(tool_name.as_str(), "WebSearch" | "WebFetch") && evidence_ids.insert(tool_use_id.clone()) {
                                web_evidence.push(json!({"tool_use_id":tool_use_id,"name":tool_name,"input":tool_input,"output":serde_json::from_str::<Value>(&output).unwrap_or_else(|_|Value::String(output.clone())),"is_error":is_error}));
                            }
                            let row = crate::db::message_repo::ToolCallRow { id: format!("{assistant_id}:{tool_use_id}"), message_id: assistant_id.clone(), name:tool_name, input:Some(tool_input.to_string()), output:Some(output), is_error, sort_order:tool_index };
                            let persisted = (|| -> Result<()> {
                                context.db.with_conn(|conn| crate::db::message_repo::save_stream_tool_result(conn, &id, &assistant_id, "", &row))??;
                                Ok(())
                            })();
                            if let Err(error) = persisted {
                                state.value["status"] = json!("failed");
                                let _ = self.event_tx.send(state.progress("failed", None)).await;
                                return Err(error);
                            }
                            tool_index += 1;
                            let status = if is_error { "tool_failed" } else { "tool_completed" };
                            let _ = self.event_tx.send(state.progress(status, Some(row.name.clone()))).await;
                        }
                        _ => {}
                    }
                }
                result = &mut execution => break result,
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
                            | "max_tokens" | "length" | "incomplete"
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
                Ok(json!({"session_id":id,"status":"completed","result":text,"web_evidence":web_evidence}))
            }
            Err(error) => {
                state.value["status"] = json!("failed");
                let _ = self.event_tx.send(state.progress("failed", None)).await;
                Err(error)
            }
        }
    }
}
