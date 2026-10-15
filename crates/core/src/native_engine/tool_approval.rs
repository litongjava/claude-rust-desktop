use super::*;
use crate::native_engine::permission_response::{Pending, PermissionResponse};

impl ToolLoopExecutor {
    pub(crate) fn with_permission_mailbox(
        mut self,
        mailbox: crate::native_engine::permission_response::Mailbox,
    ) -> Self {
        self.permission_mailbox = mailbox;
        self
    }

    pub(super) fn permission_hard_denial(&self, tool: &str, input: &Value) -> Option<String> {
        if self
            .allowed_tools
            .as_ref()
            .is_some_and(|tools| !tools.iter().any(|name| name == tool))
        {
            return Some("Tool is not allowed by this agent definition".into());
        }
        let manager = self.permission_manager.as_ref()?;
        let context = crate::permissions::PermissionContext {
            tool_name: tool.into(),
            tool_input: input.clone(),
            conversation_id: self.conv_id.clone().unwrap_or_default(),
            user_id: None,
            workspace_path: Some(self.workspace_cwd.clone()),
        };
        let result = if self.is_plan_document_tool(tool, input) {
            manager.check_plan_document_permission(&context)
        } else {
            manager.check_permission(&context)
        };
        match result {
            PermissionResult::Denied(reason) => Some(reason),
            _ => None,
        }
    }

    pub(super) fn ensure_permission_continuation(&self) -> Result<()> {
        if let Some(reason) = &self.permission_interrupted {
            anyhow::bail!("Tool permission interrupted the turn: {reason}");
        }
        Ok(())
    }

    async fn validate_approved_input(&self, tool: &str, input: &Value) -> Result<()> {
        anyhow::ensure!(input.is_object(), "Approved tool input must be an object");
        let mut schema = self
            .tool_definitions()
            .into_iter()
            .find(|definition| definition.name == tool)
            .map(|definition| definition.input_schema);
        if schema.is_none() {
            if let Some(registry) = &self.mcp_registry {
                schema = registry
                    .get_all_tools()
                    .await?
                    .into_iter()
                    .find(|definition| definition.name == tool)
                    .map(|definition| definition.input_schema);
            }
        }
        let schema =
            schema.ok_or_else(|| anyhow!("Tool schema unavailable for approved input: {tool}"))?;
        crate::structured_output::validate_result(&schema, &input.to_string())
            .map_err(|error| anyhow!("Invalid approved tool input: {error}"))?;
        let path_keys: &[&str] = match tool {
            "Write" | "Edit" | "Read" | "MultiEdit" | "smart_edit" => &["file_path"],
            "NotebookEdit" => &["path"],
            "ListDir" | "Grep" | "Glob" | "smart_grep" | "smart_project_scan" | "git_status"
            | "git_diff" | "git_log" | "git_commit" | "git_add" => &["path"],
            "Bash" | "Shell" => &["cwd"],
            "LSP" | "lsp" => &["filePath", "path"],
            "worktree" => &["path", "name"],
            _ => &[],
        };
        for key in path_keys {
            if let Some(path) = input.get(*key).and_then(Value::as_str) {
                // The exact session plan document has a separate checked runtime-data scope.
                if self.is_plan_document_tool(tool, input) {
                    continue;
                }
                validate_workspace_path(path, &self.workspace_cwd, &self.allowed_directories)?;
            }
        }
        if let Some(reason) = self.permission_hard_denial(tool, input) {
            anyhow::bail!("Permission denied for approved input: {reason}");
        }
        Ok(())
    }

    pub(super) async fn request_tool_approval(
        &mut self,
        tool: &str,
        input: &Value,
        id: &str,
    ) -> Result<Value> {
        let request_id = uuid::Uuid::new_v4().to_string();
        let (typed_tx, typed_rx) = oneshot::channel();
        self.permission_mailbox
            .lock()
            .await
            .insert(request_id.clone(), typed_tx);
        let _typed = Pending {
            mailbox: self.permission_mailbox.clone(),
            request_id: request_id.clone(),
        };
        // Compatibility for existing terminal/Desktop hosts; questions have only this channel.
        let (text_tx, text_rx) = oneshot::channel();
        self.answer_waiters
            .lock()
            .await
            .insert(request_id.clone(), text_tx);
        let _text = PermissionWaiterGuard {
            waiters: self.answer_waiters.clone(),
            request_id: request_id.clone(),
        };
        self.event_tx
            .send(EngineEvent::ToolPermission {
                request_id,
                tool_use_id: id.into(),
                tool_name: tool.into(),
                input: input.clone(),
            })
            .await
            .map_err(|_| anyhow!("Approval host disconnected"))?;
        let response = async {
            tokio::select! {
                response = typed_rx => response.map_err(|_| anyhow!("Tool approval cancelled")),
                answer = text_rx => {
                    let answer = answer.map_err(|_| anyhow!("Tool approval cancelled"))?;
                    if matches!(answer.trim().to_lowercase().as_str(), "yes"|"y"|"allow"|"ok"|"continue"|"proceed"|"approved"|"always_allow"|"是"|"允许"|"确认"|"继续"|"同意"|"好的"|"可以") {
                        let commit: Option<crate::native_engine::permission_response::Commit> = if answer.trim().eq_ignore_ascii_case("always_allow") {
                            self.permission_manager.clone().map(|manager| {
                                let tool = tool.to_owned();
                                Box::new(move || {
                                    manager.set_tool_permission(&tool, crate::permissions::ToolPermission {
                                        tool_name: tool.clone(), allowed: true, requires_confirmation: false,
                                        scope: crate::permissions::PermissionScope::Global,
                                        level: crate::permissions::PermissionLevel::Execute,
                                    });
                                    Ok(())
                                }) as crate::native_engine::permission_response::Commit
                            })
                        } else { None };
                        Ok(PermissionResponse::Allow { updated_input: None, commit })
                    } else { Ok(PermissionResponse::Deny { message: "User denied the operation".into(), interrupt: false }) }
                }
            }
        };
        let response = if self.durable_user_input {
            response.await?
        } else {
            timeout(Duration::from_secs(300), response)
                .await
                .map_err(|_| anyhow!("Tool approval timed out"))??
        };
        match response {
            PermissionResponse::Deny { message, interrupt } => {
                if interrupt {
                    self.permission_interrupted = Some(message.clone());
                }
                anyhow::bail!("{message}");
            }
            PermissionResponse::Allow {
                updated_input,
                commit,
            } => {
                let actual = match updated_input {
                    Some(Value::Object(fields)) if fields.is_empty() => input.clone(),
                    Some(updated) => updated,
                    None => input.clone(),
                };
                if matches!(tool, "EnterPlanMode" | "ExitPlanMode" | "plan_mode") {
                    anyhow::ensure!(actual == *input, "Plan transitions require their original reviewed input and dedicated plan choice");
                }
                self.validate_approved_input(tool, &actual).await?;
                if let Some(commit) = commit {
                    commit()?;
                }
                // A policy patch cannot override explicit deny or a changed plan boundary.
                if let Some(reason) = self.permission_hard_denial(tool, &actual) {
                    anyhow::bail!("{reason}");
                }
                Ok(actual)
            }
        }
    }

    pub(super) async fn execute_ask_user_confirmation(
        &mut self,
        tool: &str,
        input: &Value,
        id: &str,
        _message: &str,
    ) -> (Value, String, bool) {
        let actual = match self.request_tool_approval(tool, input, id).await {
            Ok(actual) => actual,
            Err(error) => {
                return (
                    input.clone(),
                    format!("Tool approval failed: {error:#}"),
                    true,
                )
            }
        };
        self.approved_call = Some((tool.into(), actual.clone(), id.into()));
        let result = Box::pin(self.execute_tool_call_inner(tool, &actual, id)).await;
        self.approved_call = None;
        result
    }
}

fn validate_workspace_path(path: &str, cwd: &str, additional: &[std::path::PathBuf]) -> Result<()> {
    use std::path::{Path, PathBuf};
    let raw = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        Path::new(cwd).join(path)
    };
    // Resolve existing symlinks before adding missing filename components. Lexically
    // stripping '..' before canonicalization would misinterpret symlink/../ paths.
    let mut current = raw;
    let mut suffix = Vec::new();
    let resolved = loop {
        if let Ok(parent) = current.canonicalize() {
            let mut resolved = parent;
            for component in suffix.iter().rev() {
                resolved.push(component);
            }
            break resolved;
        }
        let Some(std::path::Component::Normal(component)) = current.components().next_back() else {
            anyhow::bail!(
                "Cannot resolve approved path with a missing parent or traversal component"
            );
        };
        suffix.push(component.to_owned());
        anyhow::ensure!(current.pop(), "Cannot resolve approved path");
    };
    let roots = std::iter::once(PathBuf::from(cwd)).chain(additional.iter().cloned());
    anyhow::ensure!(
        roots
            .filter_map(|root| root.canonicalize().ok())
            .any(|root| resolved.starts_with(root)),
        "Approved input path is outside this session's allowed workspace directories"
    );
    Ok(())
}

#[cfg(test)]
#[path = "tool_approval_tests.rs"]
mod tests;
